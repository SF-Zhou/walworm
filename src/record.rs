//! Pure record parsing: byte-level format rules, no I/O and no buffering.
//!
//! This module implements the record layout described in [`crate::format`]:
//!
//! ```text
//! | payload len: u32le | record type: u8 | payload: [u8; len] | crc32c: u32le |
//! ```
//!
//! The CRC32C trailer covers `len || type || payload`. Everything here is a
//! pure function of its arguments so the format rules can be unit-tested
//! without a file, a reader or a parse buffer. The reader's buffering, cursor
//! and I/O policy live in [`crate::reader`]; the byte window itself lives in
//! [`crate::parse_buffer`].

use crate::format::{crc32c, is_all_zero, RECORD_HEADER_SIZE};

/// Decoded record header (`len: u32le | type: u8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordHeader {
    /// Payload length in bytes, decoded from the `u32le` length field.
    pub(crate) len: u64,
    /// Record type tag.
    pub(crate) record_type: u8,
}

/// Decodes a 5-byte record header.
///
/// This never fails: the length limit, payload availability and CRC checks are
/// done by the caller, which owns the reader's limits and buffer.
#[must_use]
pub(crate) fn decode_header(raw: [u8; RECORD_HEADER_SIZE]) -> RecordHeader {
    let len_bytes: [u8; 4] = raw[0..4].try_into().expect("4-byte length field");
    RecordHeader {
        len: u64::from(u32::from_le_bytes(len_bytes)),
        record_type: raw[4],
    }
}

/// Returns whether `stored_crc` is the CRC32C of the record bytes that precede
/// the trailer, i.e. of the contiguous `header || payload` range.
#[must_use]
pub(crate) fn verify_crc(record_without_trailer: &[u8], stored_crc: u32) -> bool {
    crc32c(record_without_trailer) == stored_crc
}

/// Returns whether a CRC-verified batch payload has a valid all-or-nothing
/// layout.
///
/// A valid payload is a whole sequence of `item len: u32le || item bytes`
/// pairs. An empty payload is the empty sequence (the reader separately
/// rejects a zero-length BATCH *record* as malformed). The reader calls this
/// once, before yielding any item, so a malformed batch is reported before any
/// of its items become visible; the per-item walk itself stays lazy in the
/// reader.
#[must_use]
pub(crate) fn validate_batch_layout(payload: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < payload.len() {
        if payload.len() - pos < 4 {
            return false;
        }
        let item_len = u64::from(u32::from_le_bytes(
            payload[pos..pos + 4].try_into().expect("4 bytes"),
        ));
        pos += 4;
        // `item_len` is at most `u32::MAX` and is compared against the
        // remaining payload length (a `usize`), so this check also bounds the
        // `usize` offset arithmetic below.
        if ((payload.len() - pos) as u64) < item_len {
            return false;
        }
        pos += item_len as usize;
    }
    true
}

/// Classification of the zero bytes found where a record header is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZeroRun {
    /// The inspected bytes are all zero and the run is shorter than one
    /// writer block, so it *may* be Direct I/O tail padding. The caller must
    /// fill more bytes (and eventually check for physical EOF) before deciding.
    Candidate,
    /// The bytes cannot be padding: a nonzero byte follows the zero run, or
    /// the zero run has already reached the writer's block size (valid padding
    /// is always shorter than one block). The bytes at the cursor are damaged.
    Corrupt,
}

/// Classifies the bytes currently buffered from the cursor.
///
/// `block_size` is the writer's block size as recorded in the file header. It
/// is `0` for a buffered (non-Direct-I/O) writer, which never leaves padding,
/// so any all-zero run in such a file is corrupt.
#[must_use]
pub(crate) fn inspect_zero_run(bytes_from_cursor: &[u8], block_size: u64) -> ZeroRun {
    if !is_all_zero(bytes_from_cursor) {
        return ZeroRun::Corrupt;
    }
    if bytes_from_cursor.len() as u64 >= block_size {
        return ZeroRun::Corrupt;
    }
    ZeroRun::Candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{
        encode_record_header, RECORD_TRAILER_SIZE, RECORD_TYPE_BATCH, RECORD_TYPE_DATA,
        RECORD_TYPE_EOF,
    };

    /// Builds `header || payload || crc` the way the writer does.
    fn record_bytes(record_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_record_header(payload.len() as u32, record_type).to_vec();
        out.extend_from_slice(payload);
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    #[test]
    fn header_decode_is_little_endian_length_then_type() {
        let header = encode_record_header(0x0102_0304, 7);
        assert_eq!(
            decode_header(header),
            RecordHeader {
                len: 0x0102_0304,
                record_type: 7,
            }
        );
        let header = encode_record_header(0, RECORD_TYPE_DATA);
        assert_eq!(
            decode_header(header),
            RecordHeader {
                len: 0,
                record_type: RECORD_TYPE_DATA,
            }
        );
        // The length field is a `u32`; decoding widens it to `u64`.
        assert_eq!(
            decode_header(encode_record_header(u32::MAX, RECORD_TYPE_EOF)).len,
            u64::from(u32::MAX)
        );
    }

    #[test]
    fn crc_verification_accepts_the_writer_value() {
        for (record_type, payload) in [
            (RECORD_TYPE_DATA, &b""[..]),
            (RECORD_TYPE_DATA, &b"payload"[..]),
            (RECORD_TYPE_BATCH, &b"batch payload"[..]),
            (RECORD_TYPE_EOF, &b""[..]),
        ] {
            let record = record_bytes(record_type, payload);
            let (covered, trailer) = record.split_at(record.len() - RECORD_TRAILER_SIZE);
            let stored = u32::from_le_bytes(trailer.try_into().expect("4-byte trailer"));
            assert!(verify_crc(covered, stored));
            assert!(!verify_crc(covered, stored ^ 1));

            // Any covered byte change is detected: payload, type and length.
            let mut changed = covered.to_vec();
            let last = changed.len() - 1;
            changed[last] ^= 1;
            assert!(!verify_crc(&changed, stored));

            let mut changed = covered.to_vec();
            changed[4] ^= 1; // record type
            assert!(!verify_crc(&changed, stored));

            let mut changed = covered.to_vec();
            changed[0] ^= 1; // payload length
            assert!(!verify_crc(&changed, stored));
        }
    }

    #[test]
    fn batch_layout_accepts_well_formed_payloads() {
        let mut payload = Vec::new();
        assert!(validate_batch_layout(&payload), "empty sequence is valid");

        // One item: len 3 + "abc".
        payload.extend_from_slice(&3u32.to_le_bytes());
        payload.extend_from_slice(b"abc");
        assert!(validate_batch_layout(&payload));

        // A zero-length item followed by a one-byte item.
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"z");
        assert!(validate_batch_layout(&payload));
    }

    #[test]
    fn batch_layout_rejects_malformed_payloads() {
        // A trailing partial length prefix.
        assert!(!validate_batch_layout(b"x"));
        assert!(!validate_batch_layout(&[0u8, 0, 0]));

        // An item length that exceeds the remaining payload.
        assert!(!validate_batch_layout(&[1u8, 0, 0, 0]));
        assert!(!validate_batch_layout(&[5u8, 0, 0, 0, b'a', b'b']));

        // A huge (u32::MAX) item length.
        let mut huge = Vec::new();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(b"some bytes");
        assert!(!validate_batch_layout(&huge));

        // A valid item followed by trailing garbage.
        let mut trailing = Vec::new();
        trailing.extend_from_slice(&1u32.to_le_bytes());
        trailing.extend_from_slice(b"a");
        trailing.push(0xff);
        assert!(!validate_batch_layout(&trailing));
    }

    #[test]
    fn zero_run_short_and_all_zero_is_a_candidate() {
        assert_eq!(inspect_zero_run(&[], 4096), ZeroRun::Candidate);
        assert_eq!(inspect_zero_run(&[0u8; 1], 4096), ZeroRun::Candidate);
        assert_eq!(inspect_zero_run(&[0u8; 4095], 4096), ZeroRun::Candidate);
    }

    #[test]
    fn zero_run_with_nonzero_bytes_is_corrupt() {
        assert_eq!(inspect_zero_run(&[0, 0, 1, 0], 4096), ZeroRun::Corrupt);
        assert_eq!(inspect_zero_run(&[9u8], 4096), ZeroRun::Corrupt);
    }

    #[test]
    fn zero_run_at_least_one_block_is_corrupt() {
        assert_eq!(inspect_zero_run(&[0u8; 4096], 4096), ZeroRun::Corrupt);
        assert_eq!(inspect_zero_run(&[0u8; 5000], 4096), ZeroRun::Corrupt);
        assert_eq!(inspect_zero_run(&[0u8; 512], 512), ZeroRun::Corrupt);
    }

    #[test]
    fn zero_run_in_a_non_direct_file_is_always_corrupt() {
        // `block_size == 0` means the writer did not use Direct I/O, so the
        // file cannot legitimately contain zero padding.
        assert_eq!(inspect_zero_run(&[], 0), ZeroRun::Corrupt);
        assert_eq!(inspect_zero_run(&[0u8; 4], 0), ZeroRun::Corrupt);
        assert_eq!(inspect_zero_run(&[0u8; 1], 0), ZeroRun::Corrupt);
    }
}
