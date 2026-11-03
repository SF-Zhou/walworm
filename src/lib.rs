//! # walworm
//!
//! *A WAL worm that waits for the tail to grow.*
//!
//! walworm implements a write-ahead log (WAL) on top of a single file:
//!
//! * **One writer, many readers.** The writer creates the file exclusively
//!   ([`Writer::create`] fails on an existing file — reopening a WAL is not
//!   supported); any number of readers can open it concurrently, even while
//!   the writer is still appending.
//! * **Arbitrary bytes.** Records are opaque byte strings; there is no
//!   delete operation.
//! * **Atomic batches.** [`Writer::write_batch`] stores several records as
//!   one physical record verified with a single CRC32C, so readers see the
//!   batch completely or not at all.
//! * **Optional buffered writes.** With [`WriterOptions::buffered`] the
//!   writer accumulates records in memory for throughput; call
//!   [`Writer::flush`] to make them durable. [`Writer::close`] also syncs the
//!   end-of-file marker; there is no separate `sync` method.
//! * **Optional Direct I/O.** With [`WriterOptions::direct_io`] /
//!   [`ReaderOptions::direct_io`] the file is accessed with `O_DIRECT`
//!   (Linux only); all alignment is handled internally, padding the tail
//!   with zeros where necessary. Direct I/O removes *page-cache* buffering;
//!   it does **not** by itself make data durable, so [`Writer::flush`] and
//!   [`Writer::close`] use `fdatasync` for unsynced writes in every mode.
//! * **CRC32C integrity.** Every physical record is protected by a CRC32C
//!   checksum computed with [`crc_fast`](https://crates.io/crates/crc-fast);
//!   readers verify each record before yielding it.
//! * **Explicit close.** [`Writer::close`] appends an end-of-file marker so
//!   readers can tell "writer finished" apart from "writer still writing".
//!
//! # Reading a live log
//!
//! The reader is an iterator. When the next record is not yet fully visible
//! it yields [`Error::Incomplete`] *without advancing its cursor*, so you
//! can wait and retry, or stop:
//!
//! ```rust
//! use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions};
//!
//! # fn main() -> walworm::Result<()> {
//! # let dir = tempfile::tempdir()?;
//! let path = dir.path().join("example.wal");
//!
//! let mut writer = Writer::create(&path, WriterOptions::default())?;
//! writer.write(b"hello")?;
//! writer.write_batch([b"atom".as_slice(), b"icity".as_slice()])?;
//! writer.flush()?;
//!
//! let mut reader = Reader::open(&path, ReaderOptions::default())?;
//! loop {
//!     match reader.next() {
//!         Some(Ok(record)) => println!("got {} bytes", record.len()),
//!         Some(Err(Error::Incomplete)) => break, // or sleep and retry
//!         Some(Err(e)) => return Err(e),
//!         None => break, // the writer closed the log
//!     }
//! }
//!
//! writer.close()?;
//! # Ok(())
//! # }
//! ```
//!
//! [`Reader::next_record_into`] reuses one caller-owned buffer instead of
//! allocating per record, and [`Reader::next_record_borrowed`] returns a
//! slice into the reader's internal buffer. [`Reader::open_at`] resumes
//! reading from a record boundary previously reported by
//! [`Reader::position`], which is what a restarting tailer or a recovery
//! scan needs.
//!
//! # On-disk format
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
//! The current format version is `2`. The record CRC32C covers
//! `len || type || payload` and is stored as a 4-byte trailer after the
//! payload, so it is a single contiguous checksum over the header and the
//! payload. Record types: `1` = data, `2` = batch (payload is a sequence of
//! `item len: u32le || item` pairs), `3` = end-of-file marker; `0` is reserved
//! and never written, so a legitimate record header is never all-zero. With
//! Direct I/O the tail may be padded with zeros; the header records the
//! writer's block size so readers can tell such padding apart from damaged
//! (zeroed) record headers: the padding is only accepted when the file was
//! written with Direct I/O and the zero run reaches the physical end of the
//! file.
//!
//! A single record payload may not exceed [`MAX_RECORD_SIZE`] (1 GiB) unless
//! both the writer and the reader raise `max_record_size`; the format itself
//! allows up to `u32::MAX` bytes.
//!
//! # Crash semantics
//!
//! walworm appends logical records. Direct I/O rewrites the partially filled
//! tail block; the durability model assumes these rewrites preserve the
//! already committed bytes in that block. Under this assumption, a torn
//! append affects only the new tail. Readers stop at an incomplete or invalid
//! record without advancing their cursor, so the caller decides whether to
//! retry or stop. A damaged length field can also produce
//! [`Error::RecordTooLarge`] before CRC32C verification is attempted.
//!
//! The writer makes the file header durable before [`Writer::create`]
//! returns: it writes the header, `fdatasync`s the file and, on Unix, fsyncs
//! the parent directory.
//!
//! If an I/O error is returned by [`Writer::write`], [`Writer::write_batch`],
//! or [`Writer::flush`], the writer is poisoned: it stops writing and later
//! calls return [`Error::Poisoned`]. A failed [`Writer::close`] remains
//! retryable; new writes return [`Error::Closed`], and another `close` retries
//! the pending I/O without appending another end-of-file marker. Readers may
//! already see that marker even when its sync failed.
//!
//! [`Writer::logical_position`] includes accepted records still buffered in
//! memory, so it is not a recovery watermark. [`Writer::durable_position`]
//! tracks the logical end confirmed by the last successful file sync. To
//! recover only that confirmed prefix after an error, save the durable
//! position, discard the writer, then truncate the file to that offset and
//! reopen a reader. Bytes beyond that offset have not been confirmed durable.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod aligned;
mod error;
mod file;
mod format;
mod parse_buffer;
mod reader;
mod record;
mod tail;
#[cfg(test)]
mod test_support;
mod writer;

pub use error::{Error, Result};
pub use format::MAX_RECORD_SIZE;
pub use reader::{Reader, ReaderOptions};
pub use writer::{Writer, WriterOptions};
