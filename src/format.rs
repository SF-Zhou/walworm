//! On-disk format of a walworm log file.
//!
//! Every file starts with a fixed-size header, followed by a contiguous
//! stream of records:
//!
//! ```text
//! +---------------------------- file header (16 bytes) -----------------------------+
//! | magic: 4B "WLWM" | version: u32le | writer block size: u32le | header crc: u32le |
//! +-----------------------------------------------------------------------------------+
//!
//! +----------------- record (5-byte header + payload + 4-byte trailer) ---------------+
//! | payload len: u32le | record type: u8 | payload: [u8; len] | crc32c: u32le          |
//! +-----------------------------------------------------------------------------------+
//! ```
//!
//! The record CRC32C covers `len || type || payload`, i.e. the header and the
//! payload, which are contiguous in the file. Keeping the checksum at the end
//! means it can be computed (and verified) with a single call over a
//! contiguous byte range instead of a streaming/incremental hash.
//!
//! A record is the minimal verification unit: readers only yield a record's
//! payload after the checksum matches.
//!
//! A batch written with [`crate::Writer::write_batch`] is stored as a single
//! physical record of type [`RECORD_TYPE_BATCH`] whose payload is a sequence
//! of `item len: u32le || item bytes` pairs, so the whole batch is verified
//! with one CRC32C and either appears completely or not at all.
//!
//! Record type `0` is reserved and never written, so a legitimate record
//! header (length + type) is never all-zero. Readers rely on that to tell
//! Direct I/O tail padding (a run of zero bytes) apart from a real record.
//!
//! When the writer uses Direct I/O the tail of the file may contain zero
//! padding up to the writer's block size; the header's `block_size` field is
//! non-zero for such files, which lets readers distinguish padding from damage.
//! Readers treat a run of zero bytes where a record header is expected as
//! "no more data yet" only when the file was written with Direct I/O and the
//! zero run reaches the physical end of the file.

/// Magic bytes identifying a walworm file.
pub(crate) const MAGIC: [u8; 4] = *b"WLWM";

/// Current on-disk format version.
///
/// Version 2 moved the record CRC32C from the record header to a trailer after
/// the payload (see the module docs); version 1 is not supported.
pub(crate) const FORMAT_VERSION: u32 = 2;

/// Size of the file header in bytes.
pub(crate) const FILE_HEADER_SIZE: usize = 16;

/// Size of a record header (`len` + `type`) in bytes.
pub(crate) const RECORD_HEADER_SIZE: usize = 5;

/// Size of a record trailer (the CRC32C field) in bytes.
pub(crate) const RECORD_TRAILER_SIZE: usize = 4;

/// Record type: a single opaque payload.
pub(crate) const RECORD_TYPE_DATA: u8 = 1;

/// Record type: a batch of payloads, each prefixed with a `u32le` length.
pub(crate) const RECORD_TYPE_BATCH: u8 = 2;

/// Record type: end-of-file marker written by [`crate::Writer::close`].
pub(crate) const RECORD_TYPE_EOF: u8 = 3;

/// Hard upper bound for a single record payload imposed by the format: the
/// length field is a `u32`.
pub(crate) const MAX_PAYLOAD_LEN: u64 = u32::MAX as u64;

/// Default upper bound for a single physical record payload: 1 GiB.
///
/// This is the default for
/// [`WriterOptions::max_record_size`](crate::WriterOptions::max_record_size)
/// and [`ReaderOptions::max_record_size`](crate::ReaderOptions::max_record_size).
/// Both sides use the same default so a log written with default options can
/// always be read with default options. Larger records are possible by raising
/// the limit on both sides, up to [`MAX_PAYLOAD_LEN`](u32::MAX).
pub const MAX_RECORD_SIZE: usize = 1 << 30;

/// Why a file header could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// The magic bytes do not match (or the file is too short).
    NotAWalworm,
    /// The version field holds an unsupported format version.
    UnsupportedVersion(u32),
    /// The header checksum does not match: the header is damaged.
    Corrupt,
}

/// Validates a configured `max_record_size` limit.
///
/// Shared by [`crate::WriterOptions`] and [`crate::ReaderOptions`] so both
/// sides enforce exactly the same bounds.
pub(crate) fn validate_max_record_size(max: usize) -> crate::Result<()> {
    if max == 0 {
        return Err(crate::Error::InvalidOptions(
            "max_record_size must be greater than zero".to_string(),
        ));
    }
    if max as u64 > MAX_PAYLOAD_LEN {
        return Err(crate::Error::InvalidOptions(format!(
            "max_record_size must not exceed the format limit of {MAX_PAYLOAD_LEN} bytes"
        )));
    }
    Ok(())
}

/// Computes the CRC32C (ISCSI) checksum of a byte slice.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, bytes) as u32
}

/// Encodes a record header: `payload_len: u32le | record_type: u8`.
///
/// The header is exactly the prefix of the record covered by the CRC32C
/// (together with the payload), so appending the payload and then the checksum
/// gives a record whose checksum is one contiguous [`crc32c`] call over
/// `header || payload`.
pub(crate) fn encode_record_header(payload_len: u32, record_type: u8) -> [u8; RECORD_HEADER_SIZE] {
    let mut out = [0u8; RECORD_HEADER_SIZE];
    out[0..4].copy_from_slice(&payload_len.to_le_bytes());
    out[4] = record_type;
    out
}

/// Encodes the file header into a 16-byte array.
pub(crate) fn encode_file_header(block_size: u32) -> [u8; FILE_HEADER_SIZE] {
    let mut out = [0u8; FILE_HEADER_SIZE];
    out[0..4].copy_from_slice(&MAGIC);
    out[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    out[8..12].copy_from_slice(&block_size.to_le_bytes());
    let crc = crc32c(&out[0..12]);
    out[12..16].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Decoded file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileHeader {
    /// Block size the writer used for Direct I/O alignment, or 0 when the
    /// writer did not use Direct I/O (meaning the file contains no padding).
    pub(crate) block_size: u32,
}

/// Validates and decodes a 16-byte file header.
pub(crate) fn decode_file_header(raw: &[u8; FILE_HEADER_SIZE]) -> Result<FileHeader, HeaderError> {
    if raw[0..4] != MAGIC {
        return Err(HeaderError::NotAWalworm);
    }
    let version = u32::from_le_bytes(raw[4..8].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(HeaderError::UnsupportedVersion(version));
    }
    let block_size = u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes"));
    let crc = u32::from_le_bytes(raw[12..16].try_into().expect("4 bytes"));
    if crc32c(&raw[0..12]) != crc {
        return Err(HeaderError::Corrupt);
    }
    Ok(FileHeader { block_size })
}

/// Returns true when the slice consists solely of zero bytes.
pub(crate) fn is_all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_header_roundtrip() {
        for block_size in [0, 512, 4096, 1 << 20] {
            let raw = encode_file_header(block_size);
            let header = decode_file_header(&raw).expect("valid header");
            assert_eq!(header.block_size, block_size);
        }
    }

    #[test]
    fn file_header_rejects_garbage() {
        let mut raw = encode_file_header(4096);
        raw[0] ^= 0xff;
        assert_eq!(decode_file_header(&raw), Err(HeaderError::NotAWalworm));

        let mut raw = encode_file_header(4096);
        raw[15] ^= 0x01;
        assert_eq!(decode_file_header(&raw), Err(HeaderError::Corrupt));
    }

    #[test]
    fn file_header_rejects_other_versions() {
        let mut raw = encode_file_header(4096);
        raw[4..8].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let crc = crc32c(&raw[0..12]);
        raw[12..16].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode_file_header(&raw),
            Err(HeaderError::UnsupportedVersion(FORMAT_VERSION + 1))
        );
    }

    #[test]
    fn crc32c_known_value() {
        // CRC32C("123456789") = 0xe3069283 (CRC catalogue check value).
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn record_header_layout_is_length_then_type() {
        let header = encode_record_header(0x0102_0304, RECORD_TYPE_BATCH);
        assert_eq!(&header[..4], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(header[4], RECORD_TYPE_BATCH);
        assert_eq!(header.len(), RECORD_HEADER_SIZE);
    }

    #[test]
    fn record_crc_covers_header_and_payload_as_one_range() {
        // The trailer checksum is exactly the CRC32C of the contiguous
        // `header || payload` bytes, so it needs no incremental hashing.
        let payload = b"hello";
        let header = encode_record_header(payload.len() as u32, RECORD_TYPE_DATA);
        let mut covered = header.to_vec();
        covered.extend_from_slice(payload);
        // `covered` is exactly the bytes a reader feeds to one checksum call.
        assert_eq!(crc32c(&covered), crc32c(&covered[..]));
        // Any covered byte changes the checksum.
        let mut changed = covered.clone();
        changed[RECORD_HEADER_SIZE] ^= 1;
        assert_ne!(crc32c(&covered), crc32c(&changed));
    }

    #[test]
    fn record_types_are_never_zero() {
        // Readers use an all-zero record header to detect Direct I/O padding,
        // so no legitimate type may be 0 (which would make an empty DATA
        // record's header all zero).
        for tag in [RECORD_TYPE_DATA, RECORD_TYPE_BATCH, RECORD_TYPE_EOF] {
            assert_ne!(tag, 0);
        }
        assert!(!is_all_zero(&encode_record_header(0, RECORD_TYPE_DATA)));
        assert!(!is_all_zero(&encode_record_header(0, RECORD_TYPE_EOF)));
    }
}
