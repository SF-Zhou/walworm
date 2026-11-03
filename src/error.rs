//! Error types for walworm.

use std::fmt;
use std::io;

/// Result alias used by walworm APIs.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by walworm readers and writers.
///
/// The enum is `#[non_exhaustive]`: new variants may be added in a
/// semver-compatible release, so downstream matches should end with a
/// wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An underlying I/O error.
    Io(io::Error),
    /// The record at the current read position is not yet fully visible.
    ///
    /// This is the *special return code* used by the reader iterator when it
    /// reaches the current end of the written data. The read cursor is **not**
    /// advanced, so callers may wait for the writer to append more data and
    /// then retry, or simply stop iterating.
    Incomplete,
    /// A record failed verification (CRC32C mismatch or malformed layout).
    ///
    /// The cursor is not advanced. When tailing a live file that is written
    /// with Direct I/O, a transient `Corruption` may be observed while the
    /// writer is rewriting its tail block; retrying after the writer's next
    /// `flush` may resolve it. On a file whose writer is gone, `Corruption`
    /// means the tail is torn (e.g. after a crash) or the file is damaged;
    /// readers do not skip past the corrupted record.
    Corruption {
        /// File offset of the corrupted record.
        offset: u64,
    },
    /// A record (or batch payload) is larger than the configured limit.
    ///
    /// The writer returns this when a record exceeds
    /// [`WriterOptions::max_record_size`](crate::WriterOptions::max_record_size);
    /// the reader returns it when a record's length field exceeds
    /// [`ReaderOptions::max_record_size`](crate::ReaderOptions::max_record_size).
    /// The reader checks this limit before verifying CRC32C, so this error
    /// does not establish whether the data is valid: the record may be a
    /// legitimate large record or have a damaged length field. Raising the
    /// reader's limit allows verification to proceed. The writer's limit can
    /// likewise be raised to accept larger payloads.
    RecordTooLarge {
        /// Payload length that was requested or found.
        len: u64,
        /// Configured maximum payload length.
        max: u64,
    },
    /// A record used an unknown record type tag.
    ///
    /// The cursor is not advanced. This is reported for files written by a
    /// newer format version than this crate understands.
    UnknownRecordType {
        /// File offset of the record.
        offset: u64,
        /// The unknown type tag.
        tag: u8,
    },
    /// The file is not a walworm log.
    ///
    /// The magic bytes did not match, or the file header is too short or
    /// failed its CRC32C check (for example because the writer crashed while
    /// creating the file, or because the writer has not written the header
    /// yet; retrying `open` is safe).
    NotAWalworm,
    /// The file uses an unsupported on-disk format version.
    UnsupportedVersion {
        /// Version stored in the file header.
        found: u32,
        /// Version supported by this crate.
        supported: u32,
    },
    /// The writer has already been closed.
    Closed,
    /// The writer is unusable after a previous I/O error.
    ///
    /// Once `write`, `write_batch` or `flush` returns [`Error::Io`], the writer
    /// enters a poisoned state: no further data is written, and later calls
    /// return this error. A failed [`Writer::close`](crate::Writer::close)
    /// remains retryable instead. For recovery of the confirmed durable
    /// prefix, save [`Writer::durable_position`](crate::Writer::durable_position)
    /// before discarding the writer; `logical_position` can include data that
    /// never reached the file.
    Poisoned,
    /// A configuration option is invalid.
    InvalidOptions(String),
    /// The requested feature is not supported on this platform.
    Unsupported(&'static str),
}

impl Error {
    /// Returns `true` for [`Error::Incomplete`].
    #[must_use]
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Error::Incomplete)
    }

    /// Returns `true` for errors that indicate damaged or unreadable data.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Error::Corruption { .. } | Error::UnknownRecordType { .. }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Incomplete => write!(
                f,
                "record is not yet fully visible; the cursor was not advanced, retry later"
            ),
            Error::Corruption { offset } => {
                write!(f, "record at offset {offset} failed CRC32C verification")
            }
            Error::RecordTooLarge { len, max } => {
                write!(f, "record payload length {len} exceeds the maximum {max}")
            }
            Error::UnknownRecordType { offset, tag } => {
                write!(f, "record at offset {offset} has unknown type tag {tag}")
            }
            Error::NotAWalworm => write!(f, "not a walworm file"),
            Error::UnsupportedVersion { found, supported } => write!(
                f,
                "unsupported walworm format version {found} (this crate supports version {supported})"
            ),
            Error::Closed => write!(f, "writer is already closed"),
            Error::Poisoned => write!(f, "writer is poisoned by a previous I/O error"),
            Error::InvalidOptions(msg) => write!(f, "invalid options: {msg}"),
            Error::Unsupported(what) => write!(f, "unsupported on this platform: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Compares errors by variant and payload.
///
/// [`Error::Io`] values compare equal when their [`io::ErrorKind`] matches,
/// which is enough for tests and control flow. The source error message and
/// raw OS code are ignored.
impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::Io(a), Error::Io(b)) => a.kind() == b.kind(),
            (Error::Incomplete, Error::Incomplete)
            | (Error::NotAWalworm, Error::NotAWalworm)
            | (Error::Closed, Error::Closed)
            | (Error::Poisoned, Error::Poisoned) => true,
            (Error::Corruption { offset: a }, Error::Corruption { offset: b }) => a == b,
            (
                Error::RecordTooLarge { len: a, max: b },
                Error::RecordTooLarge { len: c, max: d },
            ) => a == c && b == d,
            (
                Error::UnknownRecordType { offset: a, tag: b },
                Error::UnknownRecordType { offset: c, tag: d },
            ) => a == c && b == d,
            (
                Error::UnsupportedVersion {
                    found: a,
                    supported: b,
                },
                Error::UnsupportedVersion {
                    found: c,
                    supported: d,
                },
            ) => a == c && b == d,
            (Error::InvalidOptions(a), Error::InvalidOptions(b)) => a == b,
            (Error::Unsupported(a), Error::Unsupported(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_compare_by_variant_and_payload() {
        assert_eq!(Error::Incomplete, Error::Incomplete);
        assert_eq!(
            Error::Corruption { offset: 7 },
            Error::Corruption { offset: 7 }
        );
        assert_ne!(
            Error::Corruption { offset: 7 },
            Error::Corruption { offset: 8 }
        );
        assert_eq!(
            Error::RecordTooLarge { len: 3, max: 2 },
            Error::RecordTooLarge { len: 3, max: 2 }
        );
        assert_eq!(
            Error::Io(io::Error::from(io::ErrorKind::NotFound)),
            Error::Io(io::Error::from(io::ErrorKind::NotFound))
        );
        assert_ne!(
            Error::Io(io::Error::from(io::ErrorKind::NotFound)),
            Error::Io(io::Error::from(io::ErrorKind::PermissionDenied))
        );
        assert!(!Error::Incomplete.is_corruption());
        assert!(Error::UnknownRecordType { offset: 1, tag: 9 }.is_corruption());
    }

    #[test]
    fn display_messages_are_non_empty() {
        for e in [
            Error::Incomplete,
            Error::Corruption { offset: 4 },
            Error::RecordTooLarge { len: 9, max: 8 },
            Error::UnknownRecordType { offset: 1, tag: 3 },
            Error::NotAWalworm,
            Error::UnsupportedVersion {
                found: 2,
                supported: 1,
            },
            Error::Closed,
            Error::Poisoned,
            Error::InvalidOptions("x".into()),
            Error::Unsupported("direct io"),
        ] {
            assert_ne!(e.to_string(), "");
        }
    }
}
