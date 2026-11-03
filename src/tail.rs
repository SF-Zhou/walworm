//! Pending tail-buffer state of the WAL writer.
//!
//! [`PendingTail`] owns the byte buffer covering the file range
//! `[write_pos, data_end)`, the record-encoding step shared by every record
//! type, and the policy that tracks which of those bytes are already in the
//! file. It also owns the Direct I/O alignment rules: in that mode the on-disk
//! tail is zero-padded to a block boundary, so the still-incomplete last block
//! stays pending and is rewritten by the next spill.
//!
//! The type performs no I/O. The writer assembles a record through
//! [`PendingTail::begin_record`], [`PendingTail::payload_mut`] and
//! [`PendingTail::finish_record`], asks for a spill with
//! [`PendingTail::prepare_spill`], performs the positioned write, and only
//! then commits it with [`PendingTail::commit_spill`]. Keeping the I/O in the
//! writer keeps fault injection and the writer's lifecycle state machine in
//! one place.
//!
//! # Invariants
//!
//! Every method preserves:
//!
//! * `buf` covers `[write_pos, data_end)`: `write_pos + buf.len() == data_end`
//!   outside `begin_record`/`finish_record`/`rollback`.
//! * `written_end` is the `data_end` observed after the last successful spill,
//!   so `written_end <= data_end` and a spill is a no-op when the two are
//!   equal.
//! * `durable_end` is the `written_end` confirmed by the last successful
//!   `fdatasync`, so `durable_end <= written_end`.
//! * In Direct I/O mode `write_pos` is block-aligned and after a spill
//!   `0 <= buf.len() < block_size` (the retained, partially filled tail
//!   block); the physical end of the written range is therefore block-aligned
//!   even though `written_end` is a logical offset.
//! * `dirty` is `true` when bytes were written to the file since the last
//!   successful `fdatasync`.

use crate::aligned::{align_down, align_up, AlignedBuffer};
use crate::format::{crc32c, encode_record_header, RECORD_HEADER_SIZE};
use crate::Result;

/// One pending byte range that must be written at `offset`.
///
/// Returned by [`PendingTail::prepare_spill`]; the bytes borrow the pending
/// buffer directly (or the reusable Direct I/O staging buffer), so the write
/// path does not copy them again.
pub(crate) struct PendingWrite<'a> {
    offset: u64,
    bytes: &'a [u8],
}

impl PendingWrite<'_> {
    /// File offset at which [`bytes`](Self::bytes) must be written.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// Bytes to write. In Direct I/O mode this is the pending data zero-padded
    /// up to a whole number of blocks.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// Pending tail of the WAL stream: encoded-but-not-necessarily-written bytes.
pub(crate) struct PendingTail {
    /// Pending stream bytes covering the file range
    /// `[write_pos, write_pos + buf.len()) == [write_pos, data_end)`.
    buf: Vec<u8>,
    /// File offset of `buf[0]`. Always block-aligned in Direct I/O mode.
    write_pos: u64,
    /// Logical end of the stream (file header + all encoded records).
    data_end: u64,
    /// Logical end of the bytes known to be present in the file: the
    /// `data_end` observed after the last successful spill.
    written_end: u64,
    /// Logical end confirmed by the last successful `fdatasync`.
    durable_end: u64,
    /// Bytes were written to the file since the last successful `fdatasync`.
    dirty: bool,
    /// Whether spills must follow the Direct I/O alignment rules.
    direct: bool,
    /// Direct I/O block size, used only when `direct` is `true`.
    block_size: usize,
    /// Reusable aligned staging buffer for Direct I/O writes.
    staging: Option<AlignedBuffer>,
}

impl PendingTail {
    /// Creates the pending tail with the encoded file header as its initial
    /// (not yet written) content.
    pub(crate) fn with_header(header: &[u8], direct: bool, block_size: usize) -> Self {
        Self {
            buf: header.to_vec(),
            write_pos: 0,
            data_end: header.len() as u64,
            written_end: 0,
            durable_end: 0,
            dirty: false,
            direct,
            block_size,
            staging: None,
        }
    }

    /// Logical end of the stream: the offset just past the last record that
    /// has been encoded into the pending buffer.
    pub(crate) fn data_end(&self) -> u64 {
        self.data_end
    }

    /// File offset of the first pending byte.
    pub(crate) fn write_pos(&self) -> u64 {
        self.write_pos
    }

    /// Logical end of the bytes known to be present in the file.
    pub(crate) fn written_end(&self) -> u64 {
        self.written_end
    }

    /// Logical end confirmed by the last successful `fdatasync`.
    pub(crate) fn durable_end(&self) -> u64 {
        self.durable_end
    }

    /// Number of bytes in the pending buffer, including a retained Direct I/O
    /// tail block that is already in the file.
    pub(crate) fn pending_len(&self) -> usize {
        self.buf.len()
    }

    /// Returns whether the pending buffer is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns whether spills use the Direct I/O alignment rules.
    pub(crate) fn is_direct(&self) -> bool {
        self.direct
    }

    /// Returns whether bytes were written to the file since the last
    /// successful sync.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Direct I/O block size of the writer.
    pub(crate) fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns whether pending bytes were written since the last sync.
    pub(crate) fn needs_sync(&self) -> bool {
        self.dirty
    }

    /// Records that a successful `fdatasync` made the written bytes durable.
    pub(crate) fn mark_synced(&mut self) {
        self.durable_end = self.written_end;
        self.dirty = false;
    }

    /// Starts a new physical record by reserving its 5-byte header, returning
    /// the header's offset inside the pending buffer.
    ///
    /// The caller appends the payload through [`PendingTail::payload_mut`] and
    /// then calls [`PendingTail::finish_record`]. On an error path, call
    /// [`PendingTail::rollback`] with this offset and the logical end observed
    /// before the call.
    pub(crate) fn begin_record(&mut self) -> usize {
        let hdr = self.buf.len();
        self.buf.resize(hdr + RECORD_HEADER_SIZE, 0);
        hdr
    }

    /// Mutable access to the pending buffer for appending the payload of the
    /// record whose header was just reserved by [`PendingTail::begin_record`].
    ///
    /// Only bytes after the reserved header may be appended.
    pub(crate) fn payload_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Payload bytes appended so far for the record whose header starts at
    /// `hdr`.
    ///
    /// Only meaningful before [`PendingTail::finish_record`]: that call
    /// appends the 4-byte CRC trailer, which is not part of the payload.
    pub(crate) fn payload(&self, hdr: usize) -> &[u8] {
        &self.buf[hdr + RECORD_HEADER_SIZE..]
    }

    /// Completes the record whose header starts at `hdr`: encodes the header
    /// (`len` + `type`), computes the CRC32C over the contiguous
    /// `header || payload` range, appends it as the 4-byte trailer and advances
    /// the logical end of the stream past the whole record.
    pub(crate) fn finish_record(&mut self, hdr: usize, record_type: u8) {
        let payload_len = self.buf.len() - hdr - RECORD_HEADER_SIZE;
        self.buf[hdr..hdr + RECORD_HEADER_SIZE]
            .copy_from_slice(&encode_record_header(payload_len as u32, record_type));
        // Header and payload are contiguous, so one checksum call covers the
        // whole verified range.
        let crc = crc32c(&self.buf[hdr..hdr + RECORD_HEADER_SIZE + payload_len]);
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.data_end = self.write_pos + self.buf.len() as u64;
    }

    /// Discards the incomplete record starting at `hdr` and restores the
    /// logical end of the stream to `prev_data_end`.
    ///
    /// `prev_data_end` is the value [`PendingTail::data_end`] had before the
    /// matching [`PendingTail::begin_record`] call.
    pub(crate) fn rollback(&mut self, hdr: usize, prev_data_end: u64) {
        debug_assert_eq!(self.write_pos + hdr as u64, prev_data_end);
        self.buf.truncate(hdr);
        self.data_end = prev_data_end;
    }

    /// Prepares the next positioned write, or returns `None` when there is
    /// nothing to write.
    ///
    /// There is nothing to write when the pending buffer is empty or when
    /// everything in it is already in the file (`data_end == written_end`,
    /// which happens in Direct I/O mode where the partially filled tail block
    /// is retained and rewritten only once new records arrive).
    ///
    /// In Direct I/O mode the returned bytes are zero-padded up to a block
    /// boundary through the reusable staging buffer; otherwise they borrow the
    /// pending buffer directly. The caller must follow a successful write with
    /// [`PendingTail::commit_spill`] and must not use a failed spill's
    /// preparation.
    pub(crate) fn prepare_spill(&mut self) -> Result<Option<PendingWrite<'_>>> {
        if self.is_empty() || self.data_end == self.written_end {
            return Ok(None);
        }
        if !self.direct {
            return Ok(Some(PendingWrite {
                offset: self.write_pos,
                bytes: &self.buf,
            }));
        }

        debug_assert_eq!(self.write_pos % self.block_size as u64, 0);
        let block = self.block_size as u64;
        let padded_len = align_up(self.buf.len() as u64, block) as usize;
        self.reserve_staging(padded_len)?;
        let staging = self.staging.as_mut().expect("staging allocated");
        let data_len = self.buf.len();
        staging.as_mut_slice()[..data_len].copy_from_slice(&self.buf);
        staging.as_mut_slice()[data_len..padded_len].fill(0);
        Ok(Some(PendingWrite {
            offset: self.write_pos,
            bytes: &staging.as_slice()[..padded_len],
        }))
    }

    /// Records that the spill prepared by [`PendingTail::prepare_spill`] was
    /// written successfully: drops the bytes now present in the file and marks
    /// the tail dirty.
    ///
    /// In Direct I/O mode the still-incomplete tail block is retained in `buf`
    /// so that the next spill can rewrite it together with the new records.
    pub(crate) fn commit_spill(&mut self) {
        if self.direct {
            let keep_from = align_down(self.buf.len() as u64, self.block_size as u64) as usize;
            self.buf.drain(..keep_from);
            self.write_pos += keep_from as u64;
            debug_assert!(self.buf.len() < self.block_size);
        } else {
            self.buf.clear();
            self.write_pos = self.data_end;
            debug_assert_eq!(self.buf.len(), 0);
        }
        self.written_end = self.data_end;
        self.dirty = true;
    }

    /// Ensures the Direct I/O staging buffer can hold `needed` bytes.
    ///
    /// `needed` is already a multiple of `block_size` (it comes from
    /// [`align_up`]), so the allocation is exactly that size instead of being
    /// rounded up to a power of two.
    fn reserve_staging(&mut self, needed: usize) -> Result<()> {
        let enough = matches!(&self.staging, Some(s) if s.len() >= needed);
        if !enough {
            self.staging = Some(AlignedBuffer::new_zeroed(needed, self.block_size)?);
        }
        Ok(())
    }

    /// Length of the retained Direct I/O staging buffer, if one was allocated.
    #[cfg(test)]
    pub(crate) fn staging_len(&self) -> Option<usize> {
        self.staging.as_ref().map(AlignedBuffer::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{RECORD_TRAILER_SIZE, RECORD_TYPE_DATA, RECORD_TYPE_EOF};

    /// Direct I/O block size used by the unit tests.
    const BLOCK: usize = 4096;
    /// Size of the encoded file header used by the unit tests.
    const HEADER_LEN: usize = 16;

    fn tail_with(direct: bool) -> PendingTail {
        PendingTail::with_header(&[0u8; HEADER_LEN], direct, BLOCK)
    }

    /// Encodes one DATA record with a `payload_len`-byte payload.
    fn append_data(tail: &mut PendingTail, payload_len: usize) {
        let hdr = tail.begin_record();
        tail.payload_mut()
            .extend(std::iter::repeat_n(0x5a, payload_len));
        tail.finish_record(hdr, RECORD_TYPE_DATA);
    }

    #[test]
    fn header_is_pending_initially() {
        let tail = tail_with(false);
        assert_eq!(tail.write_pos(), 0);
        assert_eq!(tail.data_end(), HEADER_LEN as u64);
        assert_eq!(tail.written_end(), 0);
        assert!(!tail.is_dirty());
        assert!(!tail.is_empty());
    }

    #[test]
    fn non_direct_spill_clears_the_pending_buffer() {
        let mut tail = tail_with(false);
        append_data(&mut tail, 100);
        assert_eq!(
            tail.pending_len(),
            HEADER_LEN + RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE + 100
        );

        {
            let pending = tail.prepare_spill().unwrap().expect("spill");
            assert_eq!(pending.offset(), 0);
            assert_eq!(
                pending.bytes().len(),
                HEADER_LEN + RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE + 100
            );
        }
        tail.commit_spill();

        assert_eq!(tail.write_pos(), tail.data_end());
        assert_eq!(tail.written_end(), tail.data_end());
        assert!(tail.is_empty());
        assert!(tail.is_dirty());
        // A second spill without new records is a no-op.
        assert!(tail.prepare_spill().unwrap().is_none());
    }

    #[test]
    fn direct_spill_retains_the_partial_tail_block() {
        let mut tail = tail_with(true);
        // 16-byte header + 5-byte record header + 10_000 payload + 4-byte trailer = 10_025
        // bytes, which needs three 4 KiB blocks on disk.
        append_data(&mut tail, 10_000);
        assert_eq!(tail.data_end(), 10_025);

        {
            let pending = tail.prepare_spill().unwrap().expect("spill");
            assert_eq!(pending.offset(), 0);
            assert_eq!(pending.bytes().len(), 3 * BLOCK, "zero-padded to blocks");
            assert!(pending.bytes()[10_025..].iter().all(|b| *b == 0));
        }
        tail.commit_spill();

        // The whole blocks were written; the partial last block is retained.
        assert_eq!(
            tail.write_pos(),
            2 * BLOCK as u64,
            "write_pos block-aligned"
        );
        assert_eq!(tail.write_pos() % BLOCK as u64, 0);
        assert_eq!(tail.pending_len(), 10_025 - 8_192);
        assert!(tail.pending_len() < BLOCK);
        assert_eq!(tail.written_end(), tail.data_end());
        assert!(tail.is_dirty());

        // Nothing new: rewriting the retained tail block would be redundant.
        assert!(tail.prepare_spill().unwrap().is_none());
    }

    #[test]
    fn direct_spill_rewrites_the_retained_block_with_new_records() {
        let mut tail = tail_with(true);
        append_data(&mut tail, 10_000);
        {
            let _pending = tail.prepare_spill().unwrap().expect("spill");
        }
        tail.commit_spill();
        assert_eq!(tail.write_pos(), 8_192);

        // A small record keeps the pending buffer inside the same block.
        append_data(&mut tail, 1);
        assert_eq!(
            tail.pending_len(),
            10_025 - 8_192 + RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE + 1
        );
        {
            let pending = tail.prepare_spill().unwrap().expect("spill");
            assert_eq!(pending.offset(), 8_192, "rewrite starts at the block");
            assert_eq!(
                pending.bytes().len(),
                BLOCK,
                "one block in total for the retained tail"
            );
        }
        tail.commit_spill();
        assert_eq!(
            tail.write_pos(),
            8_192,
            "the rewrite covered the retained block only, so write_pos stays"
        );
        assert!(tail.pending_len() < BLOCK);
        assert_eq!(tail.written_end(), tail.data_end());
    }

    #[test]
    fn rollback_restores_buffer_and_logical_end() {
        let mut tail = tail_with(false);
        append_data(&mut tail, 10);
        let prev_data_end = tail.data_end();
        let pending_before = tail.pending_len();

        let hdr = tail.begin_record();
        tail.payload_mut().extend_from_slice(b"torn");
        tail.rollback(hdr, prev_data_end);

        assert_eq!(tail.pending_len(), pending_before);
        assert_eq!(tail.data_end(), prev_data_end);

        // The tail remains usable and appends after the rollback point.
        append_data(&mut tail, 3);
        assert_eq!(
            tail.data_end(),
            prev_data_end + (RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE + 3) as u64
        );
    }

    #[test]
    fn rollback_after_direct_spill_restores_retained_tail() {
        let mut tail = tail_with(true);
        append_data(&mut tail, 10_000);
        {
            let _pending = tail.prepare_spill().unwrap().expect("spill");
        }
        tail.commit_spill();
        let prev_data_end = tail.data_end();
        let pending_before = tail.pending_len();

        let hdr = tail.begin_record();
        tail.payload_mut().extend_from_slice(b"not durable");
        tail.rollback(hdr, prev_data_end);

        assert_eq!(tail.pending_len(), pending_before);
        assert_eq!(tail.data_end(), prev_data_end);
        assert_eq!(tail.written_end(), prev_data_end);
    }

    #[test]
    fn eof_record_is_a_header_only_record() {
        let mut tail = tail_with(false);
        let hdr = tail.begin_record();
        tail.finish_record(hdr, RECORD_TYPE_EOF);
        assert_eq!(
            tail.data_end(),
            HEADER_LEN as u64 + RECORD_HEADER_SIZE as u64 + RECORD_TRAILER_SIZE as u64
        );

        {
            let pending = tail.prepare_spill().unwrap().expect("spill");
            let record = &pending.bytes()[HEADER_LEN..];
            assert_eq!(
                record.len(),
                RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE,
                "an EOF record is header + trailer only"
            );
            assert_eq!(&record[..4], &[0, 0, 0, 0], "length field is zero");
            assert_eq!(record[4], RECORD_TYPE_EOF);
            let expected_crc = crc32c(&record[..RECORD_HEADER_SIZE]);
            assert_eq!(
                &record[RECORD_HEADER_SIZE..],
                &expected_crc.to_le_bytes(),
                "trailer is the CRC of header || payload"
            );
        }
    }

    #[test]
    fn staging_allocation_is_exact() {
        let mut tail = tail_with(true);
        append_data(&mut tail, 10_000);
        assert_eq!(tail.staging_len(), None, "allocated lazily on first spill");
        {
            let pending = tail.prepare_spill().unwrap().expect("spill");
            assert_eq!(pending.bytes().len(), 3 * BLOCK);
        }
        assert_eq!(
            tail.staging_len(),
            Some(3 * BLOCK),
            "staging is exactly the padded length"
        );
        assert_ne!(tail.staging_len(), Some(16_384), "not next_power_of_two");
    }
}
