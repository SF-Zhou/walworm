//! The WAL reader.
//!
//! The reader parses the on-disk format described in [`crate::format`] from a
//! file that another process may still be appending to. It never advances its
//! cursor on error, so callers can retry after the writer makes progress.
//!
//! The implementation is split into three layers:
//!
//! * [`crate::record`] — pure byte-level record parsing (header decode, CRC and
//!   batch-layout validation, Direct I/O padding classification), no I/O.
//! * [`crate::parse_buffer`] — the buffered byte window and the absolute parse
//!   cursor, including all absolute-to-relative offset conversions and
//!   deferred compaction.
//! * this module — orchestration: the retry loop, filling the window from the
//!   file, the lazy batch cursor and the [`Reader`]'s public API.

use std::fs::File;
use std::path::Path;

use crate::aligned::{align_down, align_up, check_block_size, AlignedBuffer};
use crate::error::{Error, Result};
use crate::file;
use crate::format::{
    decode_file_header, is_all_zero, HeaderError, FILE_HEADER_SIZE, FORMAT_VERSION,
    MAX_RECORD_SIZE, RECORD_HEADER_SIZE, RECORD_TRAILER_SIZE, RECORD_TYPE_BATCH, RECORD_TYPE_DATA,
    RECORD_TYPE_EOF,
};
use crate::parse_buffer::ParseBuffer;
use crate::record::{self, RecordHeader, ZeroRun};

/// Default read chunk size for buffered (non-Direct-I/O) readers.
const READ_CHUNK_BUFFERED: usize = 64 * 1024;

/// Default read chunk size for Direct I/O readers; rounded up to a whole
/// number of blocks.
const READ_CHUNK_DIRECT: usize = 128 * 1024;

/// Compaction threshold for the reader's internal parse buffer.
///
/// Once at least this many consumed bytes are buffered, the parse buffer is
/// compacted at the next safe opportunity. When the buffer is fully consumed
/// its capacity is also released if it is much larger than this threshold, so
/// that a single large record does not pin memory for the reader's lifetime.
const COMPACT_THRESHOLD: usize = 1 << 20;

/// Configuration of a [`Reader`].
#[derive(Debug, Clone, Copy)]
pub struct ReaderOptions {
    /// Open the file with `O_DIRECT` (Linux only). All alignment is handled
    /// internally. Default: `false`.
    pub direct_io: bool,
    /// Block size used for Direct I/O read alignment. Must be a power of two
    /// in `[512, 1 MiB]`. Default: 4096.
    pub block_size: usize,
    /// Maximum accepted payload size of a single physical record.
    ///
    /// A record whose length field exceeds this value is reported as
    /// [`Error::RecordTooLarge`] before its CRC is verified. This can indicate
    /// a valid larger record or a damaged length field. Default:
    /// [`crate::MAX_RECORD_SIZE`] (1 GiB), the same default the writer uses.
    pub max_record_size: usize,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        Self {
            direct_io: false,
            block_size: 4096,
            max_record_size: MAX_RECORD_SIZE,
        }
    }
}

impl ReaderOptions {
    /// Creates the default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `direct_io`.
    #[must_use]
    pub fn with_direct_io(mut self, direct_io: bool) -> Self {
        self.direct_io = direct_io;
        self
    }

    /// Sets `block_size`.
    #[must_use]
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size;
        self
    }

    /// Sets `max_record_size`.
    #[must_use]
    pub fn with_max_record_size(mut self, max_record_size: usize) -> Self {
        self.max_record_size = max_record_size;
        self
    }
}

/// The position of the next unyielded item inside the active batch record.
///
/// Both fields are absolute file offsets, so they stay valid even if the
/// parse buffer is later grown by `fill`. The reader deliberately keeps only
/// this pair (instead of a collection of item ranges), which makes batch
/// parsing O(1) in memory regardless of the item count.
#[derive(Debug, Clone, Copy)]
struct BatchCursor {
    /// Absolute offset of the `item len: u32le` prefix of the next item.
    next_abs: u64,
    /// Absolute offset one past the end of the batch payload.
    end_abs: u64,
}

/// Outcome of attempting to parse one record from the buffered bytes.
///
/// Record payloads are reported as absolute file offsets rather than slices so
/// that the orchestration loop can decide when to advance the parse cursor.
enum Parse {
    /// A fully verified DATA record: absolute payload range and the total
    /// physical record length (header + payload).
    Record {
        /// Absolute offset of the first payload byte.
        payload_start: u64,
        /// Absolute offset one past the last payload byte.
        payload_end: u64,
        /// Total physical record length, header included.
        total_len: u64,
    },
    /// A fully verified BATCH record with a lazy item cursor.
    Batch {
        /// Cursor over the batch payload's items.
        cursor: BatchCursor,
        /// Total physical record length, header included.
        total_len: u64,
    },
    /// The writer's end-of-file marker was decoded.
    Eof,
    /// More bytes are needed before a record can be decoded.
    NeedMore,
    /// A run of zero bytes where a record header is expected: either Direct
    /// I/O tail padding or damage. [`record::inspect_zero_run`] decides which
    /// once the zero run reaches the physical end of the file or a nonzero
    /// byte, whichever comes first.
    ZeroHeader,
    /// A record-level error. The cursor must not be advanced.
    Error(Error),
}

/// Lazily allocated scratch buffers for [`Reader::fill`].
///
/// Both are kept out of [`ParseBuffer`] because they are I/O staging, not parse
/// state, and both are allocated on the first fill that needs them: opening a
/// reader (which reads only the file header) must not allocate or read a full
/// read chunk.
struct ReadScratch {
    /// Aligned staging buffer for Direct I/O reads; allocated lazily on the
    /// first Direct I/O fill.
    staging: Option<AlignedBuffer>,
    /// Scratch buffer for buffered reads; allocated lazily on the first
    /// buffered fill.
    chunk: Vec<u8>,
}

/// A reader iterating over the records of a WAL file.
///
/// Any number of readers may open the same file concurrently, including
/// while the writer is still appending.
///
/// `Reader` implements [`Iterator`]. Iteration yields:
///
/// * `Some(Ok(record))` — the next record. Items of a batch are yielded one
///   by one (and parsed lazily, one item per call), only after the whole batch
///   passed CRC32C verification.
/// * `Some(Err(Error::Incomplete))` — the next record is not yet fully
///   visible. The cursor is **not** advanced: wait for the writer and call
///   `next()` again, or stop. This is the normal state when tailing a live
///   log.
/// * `Some(Err(Error::Corruption { .. }))` — the next record failed
///   verification or the bytes at the cursor are damaged. The cursor is not
///   advanced; the error may be transient while a Direct I/O writer rewrites
///   its tail block, otherwise it marks the end of the valid data.
/// * `Some(Err(Error::RecordTooLarge { .. }))` — the next record is larger
///   than this reader's [`ReaderOptions::max_record_size`]. Its CRC has not
///   been verified, so this alone does not distinguish a valid large record
///   from a damaged length field.
/// * `Some(Err(Error::UnknownRecordType { .. }))` — the next record uses a
///   type tag this crate does not understand (a newer format version).
/// * `None` — the writer has called [`crate::Writer::close`]; the log is
///   complete and will never grow again. `next()` keeps returning `None`
///   (`Reader` is a [`std::iter::FusedIterator`]).
///
/// Records can be copied out ([`Reader::next_record`]), copied into a
/// caller-owned buffer ([`Reader::next_record_into`]) or borrowed from the
/// reader's internal buffer ([`Reader::next_record_borrowed`]).
///
/// # Example
///
/// ```no_run
/// use walworm::{Error, Reader, ReaderOptions};
///
/// # fn main() -> walworm::Result<()> {
/// let mut reader = Reader::open("log.wal", ReaderOptions::default())?;
/// loop {
///     match reader.next() {
///         Some(Ok(record)) => println!("record: {} bytes", record.len()),
///         Some(Err(Error::Incomplete)) => {
///             // Wait for the writer to append more data, then retry.
///             std::thread::sleep(std::time::Duration::from_millis(50));
///         }
///         Some(Err(e)) => return Err(e),
///         None => break, // writer closed the log
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct Reader {
    file: File,
    direct: bool,
    block_size: usize,
    read_chunk: usize,
    max_record_size: usize,
    /// Block size recorded in the file header by the writer; `0` when the
    /// file was written without Direct I/O (so it can never contain padding).
    writer_block_size: u32,
    /// Buffered, not yet parsed bytes plus the absolute parse cursor.
    pbuf: ParseBuffer,
    /// Lazy cursor over the items of the active batch record, if any.
    batch: Option<BatchCursor>,
    closed: bool,
    /// Read staging buffers, allocated lazily on the first fill.
    io: ReadScratch,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("direct", &self.direct)
            .field("block_size", &self.block_size)
            .field("max_record_size", &self.max_record_size)
            .field("writer_block_size", &self.writer_block_size)
            .field("cursor", &self.pbuf.cursor())
            .field("batch_active", &self.batch.is_some())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Opens an existing WAL file for reading from the beginning.
    ///
    /// Fails with [`Error::NotAWalworm`] if the file is not a walworm log (for
    /// example because the writer has not finished creating it yet; retrying
    /// `open` is safe).
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotAWalworm`] for non-walworm files or damaged
    /// headers, [`Error::UnsupportedVersion`] for an unsupported on-disk
    /// format version, [`Error::InvalidOptions`] for invalid options,
    /// [`Error::Unsupported`] when Direct I/O is requested off Linux, or with
    /// the underlying I/O error.
    pub fn open<P: AsRef<Path>>(path: P, options: ReaderOptions) -> Result<Reader> {
        Self::open_with_offset(path.as_ref(), options, FILE_HEADER_SIZE as u64)
    }

    /// Opens an existing WAL file for reading, starting at `offset`.
    ///
    /// This is the recovery/tailing counterpart of [`Reader::open`]: pass a
    /// file offset previously reported by [`Reader::position`] to resume
    /// reading where a previous reader stopped, without replaying the whole
    /// log.
    ///
    /// `offset` must be a **physical record boundary** previously reported by
    /// [`Reader::position`] (and it must be at least the file header size).
    /// Any other offset is a programming error: the reader will typically
    /// report [`Error::Corruption`] (or [`Error::RecordTooLarge`], or
    /// [`Error::Incomplete`] past the end of the file) instead of the records
    /// you expect.
    ///
    /// Batches are replayed item by item, but [`Reader::position`] advances
    /// past the whole batch when its first item is yielded (see
    /// [`Reader::position`] for the exact semantics), so a checkpoint taken in
    /// the middle of a batch resumes **after** that batch when reopened with
    /// `open_at`.
    ///
    /// A checkpoint taken after the log ended ([`Reader::position`] stays on
    /// the end-of-file marker) re-enters the terminal state: the resumed reader
    /// reports [`Reader::is_at_eof`] and its first `next` returns `None`
    /// instead of [`Error::Incomplete`].
    ///
    /// # Errors
    ///
    /// The same errors as [`Reader::open`], plus [`Error::InvalidOptions`]
    /// when `offset < 16` (inside the file header).
    pub fn open_at<P: AsRef<Path>>(path: P, options: ReaderOptions, offset: u64) -> Result<Reader> {
        if offset < FILE_HEADER_SIZE as u64 {
            return Err(Error::InvalidOptions(format!(
                "open_at offset must be at least {FILE_HEADER_SIZE} (the file header size), got {offset}"
            )));
        }
        Self::open_with_offset(path.as_ref(), options, offset)
    }

    fn open_with_offset(path: &Path, options: ReaderOptions, offset: u64) -> Result<Reader> {
        if options.direct_io {
            check_block_size(options.block_size)?;
        }
        crate::format::validate_max_record_size(options.max_record_size)?;

        let file = file::open_reader(path, options.direct_io)?;
        let read_chunk = if options.direct_io {
            align_up(READ_CHUNK_DIRECT as u64, options.block_size as u64) as usize
        } else {
            READ_CHUNK_BUFFERED
        };

        let mut reader = Reader {
            file,
            direct: options.direct_io,
            block_size: options.block_size,
            read_chunk,
            max_record_size: options.max_record_size,
            writer_block_size: 0,
            pbuf: ParseBuffer::new(offset),
            batch: None,
            closed: false,
            io: ReadScratch {
                staging: None,
                chunk: Vec::new(),
            },
        };

        let raw = reader.read_file_header()?;
        let header = decode_file_header(&raw).map_err(|e| match e {
            HeaderError::UnsupportedVersion(found) => Error::UnsupportedVersion {
                found,
                supported: FORMAT_VERSION,
            },
            HeaderError::NotAWalworm | HeaderError::Corrupt => Error::NotAWalworm,
        })?;
        reader.writer_block_size = header.block_size;
        Ok(reader)
    }

    /// Returns the file offset of the next physical record to be parsed.
    ///
    /// This is a *physical record* offset, i.e. a valid checkpoint for
    /// [`Reader::open_at`]. The items of a batch record are not reflected
    /// individually: when the first item of a batch is yielded, `position()`
    /// already points past the whole batch record (the batch is parsed lazily
    /// from its absolute payload range). If you checkpoint while items of a
    /// batch are still pending, resuming from this position skips the
    /// remainder of that batch.
    ///
    /// The returned value only advances after a record was successfully
    /// verified and consumed; it never moves on `Incomplete` or `Corruption`.
    /// After reaching EOF it stays at the end-of-file marker, so reopening
    /// from this checkpoint also finishes iteration when the marker is read.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.pbuf.cursor()
    }

    /// Returns the block size recorded in the file header by the writer.
    ///
    /// This is `0` for files written without Direct I/O (which never contain
    /// zero padding) and the writer's Direct I/O block size otherwise. It is
    /// independent of [`ReaderOptions::block_size`], which only controls the
    /// reader's own I/O alignment.
    #[must_use]
    pub fn file_block_size(&self) -> u32 {
        self.writer_block_size
    }

    /// Returns whether the writer's end-of-file marker has been reached
    /// (`true` once iteration has finished because the log is closed).
    #[must_use]
    pub fn is_at_eof(&self) -> bool {
        self.closed
    }

    /// Reads and validates the fixed-size file header.
    ///
    /// Only the header bytes (plus, for Direct I/O, the alignment padding of
    /// one block) are read here; the read chunk buffers used by `fill` are
    /// allocated lazily so that opening a reader for a 16-byte header never
    /// allocates or reads 64/128 KiB.
    fn read_file_header(&mut self) -> Result<[u8; FILE_HEADER_SIZE]> {
        let mut raw = [0u8; FILE_HEADER_SIZE];
        if self.direct {
            // `O_DIRECT` requires the transfer length to be block aligned, so
            // read one block and take its first 16 bytes.
            let mut block = AlignedBuffer::new_zeroed(self.block_size, self.block_size)?;
            let n = file::read_at(&self.file, block.as_mut_slice(), 0)?;
            if n < FILE_HEADER_SIZE {
                return Err(Error::NotAWalworm);
            }
            raw.copy_from_slice(&block.as_slice()[..FILE_HEADER_SIZE]);
        } else {
            // A single positional read is allowed to be short even when the
            // file is long enough; retry until the header is complete or EOF.
            if !file::read_exact_at(&self.file, &mut raw, 0)? {
                return Err(Error::NotAWalworm);
            }
        }
        Ok(raw)
    }

    /// Allocates the Direct I/O staging buffer on first use.
    fn reserve_staging(&mut self) -> Result<()> {
        if self.io.staging.is_none() {
            self.io.staging = Some(AlignedBuffer::new_zeroed(self.read_chunk, self.block_size)?);
        }
        Ok(())
    }

    /// Appends newly available file bytes to the parse buffer. Returns the
    /// number of bytes appended (0 at end of file).
    fn fill(&mut self) -> Result<usize> {
        let end = self.pbuf.end();
        if self.direct {
            self.reserve_staging()?;
            let block = self.block_size as u64;
            let offset = align_down(end, block);
            let skip = (end - offset) as usize;
            let staging = self.io.staging.as_mut().expect("staging allocated");
            let n = file::read_at(
                &self.file,
                &mut staging.as_mut_slice()[..self.read_chunk],
                offset,
            )?;
            if n > skip {
                let got = n - skip;
                self.pbuf.append(&staging.as_slice()[skip..n])?;
                Ok(got)
            } else {
                Ok(0)
            }
        } else {
            if self.io.chunk.is_empty() {
                self.io.chunk = vec![0u8; self.read_chunk];
            }
            let n = file::read_at(&self.file, &mut self.io.chunk, end)?;
            self.pbuf.append(&self.io.chunk[..n])?;
            Ok(n)
        }
    }

    /// Drops buffered bytes past the cursor so the next `fill` re-reads them
    /// from disk. Used when the visible tail may have been replaced by the
    /// writer (Direct I/O tail-block rewriting).
    fn truncate_tail(&mut self) {
        let cursor = self.pbuf.cursor();
        self.pbuf.truncate_to(cursor);
    }

    /// Advances the cursor past a consumed record.
    ///
    /// Compaction is *not* performed here: while a batch is being yielded the
    /// already-consumed bytes must stay in the parse buffer (batch item
    /// offsets are absolute file offsets into it), and for a data record the
    /// caller still returns a slice into the buffer. Compaction is therefore
    /// deferred to [`Reader::maybe_compact`] via the buffer's pending flag.
    fn advance(&mut self, record_len: u64) {
        self.pbuf.advance(record_len, COMPACT_THRESHOLD);
    }

    /// Applies deferred compaction of the parse buffer.
    ///
    /// Never touches the buffer while a batch is active: its item offsets refer
    /// to the buffered window, and the remaining item bytes must stay where
    /// they are.
    fn maybe_compact(&mut self) {
        if self.batch.is_some() {
            return;
        }
        if self.pbuf.is_drained() {
            // Everything buffered has been consumed: reset the buffer so the
            // next fill starts exactly at the cursor.
            let cursor = self.pbuf.cursor();
            self.pbuf.clear_to(cursor);
            self.pbuf.shrink_if_large(COMPACT_THRESHOLD);
        } else if self.pbuf.compact_pending() {
            let cursor = self.pbuf.cursor();
            self.pbuf.consume_to(cursor);
            self.pbuf.shrink_if_large(COMPACT_THRESHOLD);
        }
    }

    /// Attempts to decode one physical record at the cursor.
    ///
    /// This is the pure half of the parse loop, built from [`crate::record`]
    /// and the [`ParseBuffer`] accessors: it never advances the cursor, so a
    /// parse error leaves the reader exactly where it was. The caller advances
    /// past a record only after this returned a verified record.
    fn parse_once(&self) -> Parse {
        let avail = self.pbuf.available();

        if avail.is_empty() {
            return Parse::NeedMore;
        }
        if avail.len() < RECORD_HEADER_SIZE {
            if !is_all_zero(avail) {
                return Parse::NeedMore;
            }
            // A partial zero run. Without Direct I/O there is no padding, so
            // this is just a record whose header is not fully visible yet (a
            // torn write reports `Incomplete` at EOF instead of pretending to
            // be a zeroed header). With Direct I/O it may be tail padding.
            return if self.writer_block_size == 0 {
                Parse::NeedMore
            } else {
                Parse::ZeroHeader
            };
        }

        let header: [u8; RECORD_HEADER_SIZE] = avail[..RECORD_HEADER_SIZE]
            .try_into()
            .expect("checked length");
        if is_all_zero(&header) {
            // Only Direct I/O files can contain padding; the full zero run is
            // classified by `classify_zero_run`.
            return if self.writer_block_size == 0 {
                Parse::Error(Error::Corruption {
                    offset: self.pbuf.cursor(),
                })
            } else {
                Parse::ZeroHeader
            };
        }

        let RecordHeader { len, record_type } = record::decode_header(header);
        if len > self.max_record_size as u64 {
            return Parse::Error(Error::RecordTooLarge {
                len,
                max: self.max_record_size as u64,
            });
        }
        let total = RECORD_HEADER_SIZE as u64 + len + RECORD_TRAILER_SIZE as u64;
        if (avail.len() as u64) < total {
            return Parse::NeedMore;
        }
        // `avail.len() >= total` and `total` was built from a `u32` length, so
        // the cast is in range on every platform.
        let total = total as usize;
        let covered = &avail[..total - RECORD_TRAILER_SIZE];
        let stored_crc = u32::from_le_bytes(
            avail[total - RECORD_TRAILER_SIZE..total]
                .try_into()
                .expect("4-byte trailer"),
        );
        if !record::verify_crc(covered, stored_crc) {
            return Parse::Error(Error::Corruption {
                offset: self.pbuf.cursor(),
            });
        }
        let payload = &covered[RECORD_HEADER_SIZE..];

        let payload_start = self.pbuf.cursor() + RECORD_HEADER_SIZE as u64;
        match record_type {
            RECORD_TYPE_DATA => Parse::Record {
                payload_start,
                payload_end: payload_start + len,
                total_len: total as u64,
            },
            RECORD_TYPE_BATCH => {
                // An empty BATCH record is malformed: `Writer::write_batch`
                // never emits one (an empty batch is a no-op), and treating
                // it as an empty item list would silently advance the cursor
                // past a record that yielded nothing.
                if len == 0 {
                    return Parse::Error(Error::Corruption {
                        offset: self.pbuf.cursor(),
                    });
                }
                if !record::validate_batch_layout(payload) {
                    return Parse::Error(Error::Corruption {
                        offset: self.pbuf.cursor(),
                    });
                }
                Parse::Batch {
                    cursor: BatchCursor {
                        next_abs: payload_start,
                        end_abs: payload_start + len,
                    },
                    total_len: total as u64,
                }
            }
            RECORD_TYPE_EOF => Parse::Eof,
            tag => Parse::Error(Error::UnknownRecordType {
                offset: self.pbuf.cursor(),
                tag,
            }),
        }
    }

    /// Decides whether the zero bytes at the cursor are Direct I/O padding.
    ///
    /// Must only be called on bytes that were (re-)read from the file during
    /// this call, so that stale buffered zeros cannot be mistaken for damage.
    /// Returns `Ok(true)` when the zero run reaches the physical end of the
    /// file and is shorter than the writer's block size (the only shape valid
    /// padding can have), and `Ok(false)` when the bytes are damaged.
    fn classify_zero_run(&mut self) -> Result<bool> {
        let block = u64::from(self.writer_block_size);
        debug_assert_ne!(block, 0, "only Direct I/O files can have padding");
        loop {
            match record::inspect_zero_run(self.pbuf.available(), block) {
                ZeroRun::Corrupt => return Ok(false),
                ZeroRun::Candidate => {
                    if self.fill()? == 0 {
                        // The zero run reaches the physical end of the file
                        // and is shorter than one block: Direct I/O padding.
                        return Ok(true);
                    }
                }
            }
        }
    }

    /// Returns the absolute byte range of the next item of the active batch,
    /// or `None` when the batch is exhausted.
    ///
    /// The batch payload was CRC-verified and its layout validated when the
    /// batch record was parsed, and its bytes cannot be compacted away while
    /// the batch is active, so the item length prefix is guaranteed to be
    /// available here.
    fn next_batch_item(&mut self) -> Option<Result<(u64, u64)>> {
        let cursor = self.batch?;
        if cursor.next_abs == cursor.end_abs {
            self.batch = None;
            return None;
        }
        let avail = self.pbuf.available_from(cursor.next_abs);
        if avail.len() < 4 {
            // Unreachable for a validated batch, but never read out of bounds.
            return Some(Err(Error::Corruption {
                offset: cursor.next_abs,
            }));
        }
        let item_len = u64::from(u32::from_le_bytes(avail[..4].try_into().expect("4 bytes")));
        let item_start = cursor.next_abs + 4;
        let Some(item_end) = item_start.checked_add(item_len) else {
            return Some(Err(Error::Corruption {
                offset: cursor.next_abs,
            }));
        };
        if item_end > self.pbuf.end() || item_end > cursor.end_abs {
            return Some(Err(Error::Corruption {
                offset: cursor.next_abs,
            }));
        }
        self.batch = Some(BatchCursor {
            next_abs: item_end,
            end_abs: cursor.end_abs,
        });
        Some(Ok((item_start, item_end)))
    }

    /// Returns the next record payload without copying it.
    ///
    /// The returned slice borrows the reader's internal parse buffer and stays
    /// valid until the next call that takes `&mut self`. This is the
    /// allocation-free way to read a log, and the basis of both
    /// [`Reader::next_record_into`] and [`Reader::next_record`].
    ///
    /// Items of a batch are returned one per call. The rest of the contract is
    /// exactly that of the [`Iterator`] implementation; see the [`Reader`]
    /// type documentation for the possible outcomes.
    pub fn next_record_borrowed(&mut self) -> Option<Result<&[u8]>> {
        // Set when the current call already re-read a padding region once;
        // prevents spinning on stable padding.
        let mut padding_refetched = false;
        loop {
            self.maybe_compact();

            if self.batch.is_some() {
                match self.next_batch_item() {
                    Some(Ok((start_abs, end_abs))) => {
                        let start = self.pbuf.index(start_abs);
                        let end = self.pbuf.index(end_abs);
                        return Some(Ok(&self.pbuf.data()[start..end]));
                    }
                    Some(Err(e)) => return Some(Err(e)),
                    None => continue,
                }
            }

            if self.closed {
                return None;
            }

            match self.parse_once() {
                Parse::Record {
                    payload_start,
                    payload_end,
                    total_len,
                } => {
                    self.advance(total_len);
                    let start = self.pbuf.index(payload_start);
                    let end = self.pbuf.index(payload_end);
                    return Some(Ok(&self.pbuf.data()[start..end]));
                }
                Parse::Batch { cursor, total_len } => {
                    self.advance(total_len);
                    self.batch = Some(cursor);
                }
                Parse::Eof => {
                    // Keep the checkpoint on the marker so open_at(position)
                    // can recover the same terminal state.
                    self.closed = true;
                    // Nothing more will ever be read: release the parse buffer
                    // (it may hold a large record or the whole Direct I/O tail
                    // block).
                    let cursor = self.pbuf.cursor();
                    self.pbuf.clear_to(cursor);
                    self.pbuf.shrink_if_large(0);
                    return None;
                }
                Parse::NeedMore => match self.fill() {
                    Ok(0) => return Some(Err(Error::Incomplete)),
                    Ok(_) => {}
                    Err(e) => return Some(Err(e)),
                },
                Parse::ZeroHeader => {
                    if !padding_refetched {
                        // The bytes we hold may be stale (a Direct I/O writer
                        // rewrites its tail block in place). Re-read them from
                        // disk once before deciding whether this is padding or
                        // damage.
                        padding_refetched = true;
                        self.truncate_tail();
                        if let Err(e) = self.fill() {
                            return Some(Err(e));
                        }
                        continue;
                    }
                    match self.classify_zero_run() {
                        Ok(true) => return Some(Err(Error::Incomplete)),
                        Ok(false) => {
                            self.truncate_tail();
                            return Some(Err(Error::Corruption {
                                offset: self.pbuf.cursor(),
                            }));
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
                Parse::Error(e) => {
                    // Drop the bytes that failed to parse so a retry observes
                    // the current file contents rather than stale bytes.
                    self.truncate_tail();
                    return Some(Err(e));
                }
            }
        }
    }

    /// Returns the next record as an owned `Vec<u8>`.
    ///
    /// This copies the payload and allocates one `Vec` per record. For hot
    /// loops prefer [`Reader::next_record_into`] (reused buffer) or
    /// [`Reader::next_record_borrowed`] (no copy at all).
    pub fn next_record(&mut self) -> Option<Result<Vec<u8>>> {
        match self.next_record_borrowed() {
            Some(Ok(slice)) => Some(Ok(slice.to_vec())),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }

    /// Copies the next record into `out`, reusing its allocation, and returns
    /// the record length.
    ///
    /// On success, `out` is replaced with the record while reusing its
    /// capacity; it is left unchanged on errors and EOF. This method does not
    /// allocate per record once `out` has grown to the largest record size.
    /// The rest of the contract is exactly that of the [`Iterator`]
    /// implementation.
    pub fn next_record_into(&mut self, out: &mut Vec<u8>) -> Option<Result<usize>> {
        match self.next_record_borrowed() {
            Some(Ok(slice)) => {
                out.clear();
                out.extend_from_slice(slice);
                Some(Ok(slice.len()))
            }
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }
}

impl Iterator for Reader {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record()
    }
}

impl std::iter::FusedIterator for Reader {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{
        crc32c, encode_file_header, encode_record_header, RECORD_TRAILER_SIZE, RECORD_TYPE_BATCH,
    };
    use crate::{Writer, WriterOptions};
    use std::fs;

    fn collect(reader: &mut Reader) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for item in reader.by_ref() {
            out.push(item?);
        }
        Ok(out)
    }

    fn collect_borrowed(reader: &mut Reader) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        while let Some(item) = reader.next_record_borrowed() {
            out.push(item?.to_vec());
        }
        Ok(out)
    }

    fn collect_into(reader: &mut Reader) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(item) = reader.next_record_into(&mut buf) {
            let n = item?;
            out.push(buf[..n].to_vec());
        }
        Ok(out)
    }

    /// Encodes one physical record exactly like the writer does.
    fn record_bytes(record_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_record_header(payload.len() as u32, record_type).to_vec();
        out.extend_from_slice(payload);
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Builds a WAL file from a file header (with the given writer block size)
    /// and raw record bytes.
    fn wal_bytes(block_size: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = encode_file_header(block_size).to_vec();
        for record in records {
            out.extend_from_slice(record);
        }
        out
    }

    fn write_wal(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn bad_header_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.wal");
        std::fs::write(&path, b"definitely not a wal").unwrap();
        let err = Reader::open(&path, ReaderOptions::default()).unwrap_err();
        assert!(matches!(err, Error::NotAWalworm));
    }

    #[test]
    fn unsupported_version_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut raw = encode_file_header(0);
        raw[4..8].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let crc = crate::format::crc32c(&raw[0..12]);
        raw[12..16].copy_from_slice(&crc.to_le_bytes());
        let path = write_wal(&dir, "version.wal", &raw);
        let err = Reader::open(&path, ReaderOptions::default()).unwrap_err();
        assert_eq!(
            err,
            Error::UnsupportedVersion {
                found: FORMAT_VERSION + 1,
                supported: FORMAT_VERSION,
            }
        );
    }

    #[test]
    fn empty_log_iterates_to_closed_and_is_fused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.close().unwrap();
        drop(w);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.file_block_size(), 0);
        assert!(r.next().is_none());
        assert!(r.is_at_eof());
        assert!(r.next().is_none());
    }

    #[test]
    fn batch_items_are_yielded_individually() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"a").unwrap();
        w.write_batch([b"b1".as_slice(), b"b2".as_slice(), b"".as_slice()])
            .unwrap();
        w.write(b"c").unwrap();
        w.close().unwrap();

        let expected = vec![
            b"a".to_vec(),
            b"b1".to_vec(),
            b"b2".to_vec(),
            Vec::new(),
            b"c".to_vec(),
        ];
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(collect(&mut r).unwrap(), expected);
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(collect_borrowed(&mut r).unwrap(), expected);
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(collect_into(&mut r).unwrap(), expected);
    }

    #[test]
    fn borrowed_slices_survive_pbuf_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        for i in 0..500u32 {
            w.write(format!("record-{i}").as_bytes()).unwrap();
        }
        w.close().unwrap();
        drop(w);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        for i in 0..500u32 {
            let got = r.next_record_borrowed().unwrap().unwrap().to_vec();
            assert_eq!(got, format!("record-{i}").as_bytes());
        }
        assert!(r.next_record_borrowed().is_none());
        assert!(r.is_at_eof());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "yielding 100k items is too slow under the Miri interpreter"
    )]
    fn large_batch_is_parsed_lazily() {
        // 100k empty items: a materialized `Vec<Vec<u8>>` implementation would
        // need ~2.8 MiB of item vectors here (and ~6 GiB for a 1 GiB batch);
        // the lazy cursor needs O(1) beyond the record payload itself.
        const ITEMS: u32 = 100_000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write_batch(std::iter::repeat_n([].as_slice(), ITEMS as usize))
            .unwrap();
        w.close().unwrap();
        drop(w);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        let mut count = 0u32;
        let mut position_after_first = None;
        loop {
            let item_len = match r.next_record_borrowed() {
                Some(Ok(slice)) => slice.len(),
                Some(Err(e)) => panic!("unexpected error: {e}"),
                None => break,
            };
            assert_eq!(item_len, 0);
            count += 1;
            if count == 1 {
                // A5: position() is the next *physical* record offset, so it
                // has already moved past the whole batch when its first item
                // is yielded.
                position_after_first = Some(r.position());
            }
        }
        assert_eq!(count, ITEMS);
        assert!(r.is_at_eof());
        // File header + record header + ITEMS four-byte item lengths + trailer.
        let batch_end = FILE_HEADER_SIZE as u64
            + RECORD_HEADER_SIZE as u64
            + 4 * u64::from(ITEMS)
            + RECORD_TRAILER_SIZE as u64;
        assert_eq!(position_after_first, Some(batch_end));
    }

    #[test]
    fn zeroed_header_in_non_direct_file_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = wal_bytes(0, &[record_bytes(RECORD_TYPE_DATA, b"first")]);
        let offset = bytes.len() as u64;
        bytes.extend_from_slice(&[0u8; RECORD_HEADER_SIZE]);
        bytes.extend_from_slice(&record_bytes(RECORD_TYPE_DATA, b"later"));
        bytes.extend_from_slice(&record_bytes(RECORD_TYPE_EOF, b""));
        let path = write_wal(&dir, "zeroed.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"first");
        assert_eq!(r.next().unwrap().unwrap_err(), Error::Corruption { offset });
        assert_eq!(r.position(), offset);
        // Stable corruption is reported again on retry.
        assert_eq!(r.next().unwrap().unwrap_err(), Error::Corruption { offset });
        assert_eq!(r.position(), offset);
    }

    #[test]
    fn zeroed_header_mid_file_is_corruption_in_direct_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = wal_bytes(4096, &[record_bytes(RECORD_TYPE_DATA, b"first")]);
        let offset = bytes.len() as u64;
        bytes.extend_from_slice(&[0u8; RECORD_HEADER_SIZE]);
        bytes.extend_from_slice(&record_bytes(RECORD_TYPE_DATA, b"later"));
        let path = write_wal(&dir, "zeroed-direct.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"first");
        assert_eq!(r.next().unwrap().unwrap_err(), Error::Corruption { offset });
    }

    #[test]
    fn zero_run_shorter_than_block_is_padding() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = encode_file_header(4096).to_vec();
        bytes.extend_from_slice(&[0u8; 100]);
        let path = write_wal(&dir, "padding.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
        assert_eq!(r.position(), FILE_HEADER_SIZE as u64);
    }

    #[test]
    fn zero_run_at_least_one_block_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = encode_file_header(4096).to_vec();
        bytes.extend_from_slice(&[0u8; 4096]);
        let path = write_wal(&dir, "huge-padding.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(
            r.next().unwrap().unwrap_err(),
            Error::Corruption {
                offset: FILE_HEADER_SIZE as u64
            }
        );
        assert_eq!(r.position(), FILE_HEADER_SIZE as u64);
    }

    #[test]
    fn zero_item_batch_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = wal_bytes(0, &[record_bytes(RECORD_TYPE_BATCH, b"")]);
        let path = write_wal(&dir, "empty-batch.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(
            r.next().unwrap().unwrap_err(),
            Error::Corruption {
                offset: FILE_HEADER_SIZE as u64
            }
        );
        // The cursor must not advance on error.
        assert_eq!(r.position(), FILE_HEADER_SIZE as u64);
    }

    #[test]
    fn unknown_record_type_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = wal_bytes(0, &[record_bytes(9, b"payload")]);
        let path = write_wal(&dir, "unknown.wal", &bytes);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(
            r.next().unwrap().unwrap_err(),
            Error::UnknownRecordType {
                offset: FILE_HEADER_SIZE as u64,
                tag: 9
            }
        );
        assert_eq!(r.position(), FILE_HEADER_SIZE as u64);
    }

    #[test]
    fn oversized_record_is_record_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(&vec![1u8; 2048]).unwrap();
        w.close().unwrap();
        drop(w);

        let mut r = Reader::open(&path, ReaderOptions::new().with_max_record_size(1024)).unwrap();
        assert_eq!(
            r.next().unwrap().unwrap_err(),
            Error::RecordTooLarge {
                len: 2048,
                max: 1024
            }
        );
        assert_eq!(r.position(), FILE_HEADER_SIZE as u64);

        // The same file is readable with a large enough limit.
        let mut r = Reader::open(&path, ReaderOptions::new().with_max_record_size(4096)).unwrap();
        assert_eq!(r.next().unwrap().unwrap().len(), 2048);
    }

    #[test]
    fn open_at_resumes_from_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"a").unwrap();
        w.write_batch([b"b".as_slice(), b"c".as_slice()]).unwrap();
        w.write(b"d").unwrap();
        w.close().unwrap();
        drop(w);

        let expected = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"a");
        let checkpoint = r.position();
        assert_eq!(checkpoint, FILE_HEADER_SIZE as u64 + 9 + 1);

        let mut resumed = Reader::open_at(&path, ReaderOptions::default(), checkpoint).unwrap();
        assert_eq!(resumed.position(), checkpoint);
        assert_eq!(collect(&mut resumed).unwrap(), expected[1..]);
        assert!(resumed.is_at_eof());
    }

    #[test]
    fn eof_checkpoint_resumes_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        for has_record in [false, true] {
            let path = dir.path().join(format!("eof-{has_record}.wal"));
            let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
            if has_record {
                w.write(b"last").unwrap();
            }
            let eof_offset = w.logical_position();
            w.close().unwrap();

            let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
            if has_record {
                assert_eq!(r.next().unwrap().unwrap(), b"last");
            }
            assert!(r.next().is_none());
            assert!(r.is_at_eof());
            assert_eq!(r.position(), eof_offset);

            let mut resumed =
                Reader::open_at(&path, ReaderOptions::default(), r.position()).unwrap();
            assert!(resumed.next().is_none());
            assert!(resumed.is_at_eof());
            assert_eq!(resumed.position(), eof_offset);
            assert!(resumed.next_record_borrowed().is_none());
        }
    }

    #[test]
    fn read_into_preserves_output_on_incomplete_and_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read-into.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        let mut out = b"previous contents".to_vec();
        assert_eq!(r.next_record_into(&mut out), Some(Err(Error::Incomplete)));
        assert_eq!(out, b"previous contents");

        w.write(b"new").unwrap();
        assert_eq!(r.next_record_into(&mut out), Some(Ok(3)));
        assert_eq!(out, b"new");
        w.close().unwrap();
        assert_eq!(r.next_record_into(&mut out), None);
        assert_eq!(out, b"new");
    }

    #[test]
    fn open_at_rejects_offset_inside_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.close().unwrap();
        drop(w);

        let err = Reader::open_at(&path, ReaderOptions::default(), FILE_HEADER_SIZE as u64 - 1)
            .unwrap_err();
        assert!(matches!(err, Error::InvalidOptions(_)));
    }

    #[cfg_attr(miri, ignore = "Miri cannot open files with O_DIRECT")]
    #[test]
    fn direct_padding_is_incomplete_and_recovers_after_append() {
        let dir = tempfile::tempdir().unwrap();
        if !crate::test_support::direct_io_supported(&dir) {
            eprintln!("skipping: direct I/O not supported on this filesystem");
            return;
        }
        let path = dir.path().join("direct.wal");
        let mut w = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap();
        let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();
        assert_eq!(r.file_block_size(), 4096);

        // The writer padded its tail block with zeros; that is "no more data
        // yet", not corruption.
        assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

        w.write(b"one").unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"one");
        assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

        // The writer rewrites the tail block in place: the reader must recover
        // from the stale zeros it buffered.
        w.write(b"two").unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"two");
        assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

        w.close().unwrap();
        assert!(r.next().is_none());
    }

    #[cfg_attr(miri, ignore = "Miri cannot open files with O_DIRECT")]
    #[test]
    fn large_record_releases_excess_capacity_while_waiting_on_direct_padding() {
        let dir = tempfile::tempdir().unwrap();
        if !crate::test_support::direct_io_supported(&dir) {
            eprintln!("skipping: direct I/O not supported on this filesystem");
            return;
        }
        let path = dir.path().join("large-direct.wal");
        let mut w = Writer::create(
            &path,
            WriterOptions::new()
                .with_direct_io(true)
                .with_buffered(true),
        )
        .unwrap();
        let data = vec![0x5a; 8 * COMPACT_THRESHOLD];
        w.write(&data).unwrap();
        w.flush().unwrap();

        let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();
        assert_eq!(r.next_record_borrowed().unwrap().unwrap(), data);
        assert!(r.pbuf.capacity() >= data.len());
        assert_eq!(r.next_record_borrowed(), Some(Err(Error::Incomplete)));
        let capacity = r.pbuf.capacity();
        assert!(capacity <= 2 * COMPACT_THRESHOLD);
        assert!(capacity >= r.read_chunk);
        for _ in 0..3 {
            assert_eq!(r.next_record_borrowed(), Some(Err(Error::Incomplete)));
            assert_eq!(r.pbuf.capacity(), capacity);
        }

        w.write(b"next").unwrap();
        w.flush().unwrap();
        assert_eq!(r.next_record_borrowed().unwrap().unwrap(), b"next");
        w.close().unwrap();
        assert!(r.next_record_borrowed().is_none());
        assert_eq!(r.pbuf.capacity(), 0);
    }
}
