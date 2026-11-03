//! The WAL writer.
//!
//! This module owns the writer's lifecycle state machine, its durability
//! ordering and the actual positioned I/O (including the test-only fault
//! injection). The pending-byte bookkeeping, including the Direct I/O
//! tail-block alignment rules and the record encoding shared by all record
//! types, lives in [`crate::tail::PendingTail`].

use std::fs::{self, File};
use std::path::Path;

use crate::error::{Error, Result};
use crate::file;
use crate::format::{
    encode_file_header, MAX_PAYLOAD_LEN, MAX_RECORD_SIZE, RECORD_TYPE_BATCH, RECORD_TYPE_DATA,
    RECORD_TYPE_EOF,
};
use crate::tail::PendingTail;

/// Configuration of a [`Writer`].
///
/// # Durability model
///
/// * [`Writer::flush`] and [`Writer::close`] call `fdatasync` for unsynced
///   writes in every mode and are the durability barriers. Spilling a record
///   to the file only makes it visible to the operating system and to readers.
/// * `direct_io` removes page-cache buffering, but it is **not** a durability
///   guarantee: `O_DIRECT` does not flush the device write cache and does not
///   make a file-size extension durable. Direct I/O writers therefore still
///   `fdatasync` in [`Writer::flush`] and [`Writer::close`].
/// * With `buffered == false` every `write`/`write_batch` is sent to the
///   operating system immediately; with `buffered == true` records accumulate
///   in memory and reach the file when the buffer reaches `buffer_capacity`
///   or [`Writer::flush`] is called.
/// * Direct I/O with `buffered == false` rewrites the whole tail block on
///   every record, which is a large write amplification for small records
///   (measured 57x at 64-byte records). Prefer `buffered == true` with Direct
///   I/O unless records are at least one block in size.
#[derive(Debug, Clone, Copy)]
pub struct WriterOptions {
    /// Open the file with `O_DIRECT` (Linux only). The writer performs all
    /// necessary alignment internally, padding the tail block with zeros.
    /// Direct I/O bypasses the page cache but does not make data durable; see
    /// the type-level durability notes. Default: `false`.
    pub direct_io: bool,
    /// Buffer records in memory and write them lazily. Improves throughput
    /// for small records. Requires calling [`Writer::flush`] for durability
    /// and visibility to readers. Default: `false`.
    pub buffered: bool,
    /// Capacity of the in-memory write buffer in bytes. When a buffered
    /// writer's pending data reaches or exceeds this value it is spilled to
    /// the file (without `fdatasync`). Only meaningful with
    /// `buffered == true`. This is not a hard memory bound: a single record
    /// larger than the capacity is buffered in full. Default: 1 MiB.
    pub buffer_capacity: usize,
    /// Block size used for Direct I/O alignment. Must be a power of two in
    /// `[512, 1 MiB]`. Default: 4096.
    pub block_size: usize,
    /// Maximum payload size of a single physical record, in bytes.
    ///
    /// [`Writer::write`] fails with [`Error::RecordTooLarge`] when the payload
    /// is larger; for [`Writer::write_batch`] the limit applies to the whole
    /// encoded batch payload (the sum of the `u32` length prefixes plus the
    /// item bytes). Must be greater than zero and at most `u32::MAX` (the
    /// format's length-field limit); invalid values are rejected by
    /// [`Writer::create`]. Default: [`MAX_RECORD_SIZE`] (1 GiB), which matches
    /// `ReaderOptions::max_record_size`, so a log written with default options
    /// can always be read with default options.
    pub max_record_size: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            direct_io: false,
            buffered: false,
            buffer_capacity: 1 << 20,
            block_size: 4096,
            max_record_size: MAX_RECORD_SIZE,
        }
    }
}

impl WriterOptions {
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

    /// Sets `buffered`.
    #[must_use]
    pub fn with_buffered(mut self, buffered: bool) -> Self {
        self.buffered = buffered;
        self
    }

    /// Sets `buffer_capacity`.
    #[must_use]
    pub fn with_buffer_capacity(mut self, buffer_capacity: usize) -> Self {
        self.buffer_capacity = buffer_capacity;
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

/// Test-only fault injection knobs.
///
/// Kept out of the public API and, outside of `#[cfg(test)]` builds, compiled
/// to a zero-sized struct so production code pays nothing for it.
#[derive(Debug, Clone, Copy, Default)]
struct Faults {
    /// Fail the next positioned write once `write_calls` has reached this
    /// value.
    #[cfg(test)]
    fail_write_after: Option<usize>,
    /// Fail the next `fdatasync` once `sync_calls` has reached this value.
    #[cfg(test)]
    fail_sync_after: Option<usize>,
}

/// Consolidated test-only hooks: fault injection plus the I/O call counters
/// the tests use to observe exactly what the writer did.
///
/// Only compiled in test builds; production builds have no such state at all.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct TestHooks {
    /// Injected I/O failures.
    faults: Faults,
    /// Number of positioned writes attempted so far.
    write_calls: usize,
    /// Number of `fdatasync` calls attempted so far.
    sync_calls: usize,
}

#[cfg(test)]
impl TestHooks {
    /// Applies the write fault hook and counts the attempted write, returning
    /// an injected error when the hook is armed.
    fn before_write(&mut self) -> Option<Error> {
        if let Some(limit) = self.faults.fail_write_after {
            if self.write_calls >= limit {
                return Some(Error::Io(std::io::Error::other("injected write failure")));
            }
        }
        self.write_calls += 1;
        None
    }

    /// Applies the sync fault hook and counts the attempted `fdatasync`,
    /// returning an injected error when the hook is armed.
    fn before_sync(&mut self) -> Option<Error> {
        if let Some(limit) = self.faults.fail_sync_after {
            if self.sync_calls >= limit {
                return Some(Error::Io(std::io::Error::other("injected sync failure")));
            }
        }
        self.sync_calls += 1;
        None
    }
}

/// Lifecycle state of a [`Writer`].
///
/// The states are ordered by how far `close` has progressed: a writer starts
/// [`WriterState::Open`], moves to [`WriterState::EofWritten`] when the
/// end-of-file marker has been appended to the stream, and reaches
/// [`WriterState::Closed`] once that marker is durable. An I/O error from
/// `write`, `write_batch` or `flush` moves the writer to
/// [`WriterState::Poisoned`]; a failed `close` stays retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterState {
    /// Accepting records; no end-of-file marker has been appended.
    Open,
    /// The end-of-file marker has been appended but is not durable yet. Writes
    /// are rejected; `close` retries only the failed I/O.
    EofWritten,
    /// The end-of-file marker is durable; the writer is done.
    Closed,
    /// An I/O error occurred; the writer must not be used any more.
    Poisoned,
}

/// The single writer of a WAL file.
///
/// A writer is created with [`Writer::create`], which fails if the file
/// already exists: walworm writers never reopen an existing WAL. There must
/// be at most one writer per file; readers are unlimited.
///
/// The writer is `Send` but not `Sync`; do not share it between threads.
///
/// # Failure model
///
/// The writer tracks three independent pieces of state:
///
/// * the end-of-file marker has been appended (`close` in progress),
/// * the writer is closed (the marker is durable),
/// * the writer is poisoned by an I/O error.
///
/// After [`Writer::close`] has appended the end-of-file marker, `write` and
/// `write_batch` fail with [`Error::Closed`] even if the following sync
/// failed. An I/O error from `write`, `write_batch` or `flush` poisons the
/// writer: later calls return [`Error::Poisoned`] and no further I/O is
/// attempted. Dropping a writer without a successful `close` makes pending
/// bytes visible to readers best-effort (no `fdatasync`); a poisoned writer
/// performs no I/O on drop.
pub struct Writer {
    file: File,
    buffered: bool,
    buffer_capacity: usize,
    max_record_size: usize,
    /// Pending stream bytes and the spill policy (see [`PendingTail`]).
    tail: PendingTail,
    /// Lifecycle state: open, EOF appended, closed, or poisoned.
    state: WriterState,
    /// Test-only fault injection and call counters.
    #[cfg(test)]
    hooks: TestHooks,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("direct", &self.tail.is_direct())
            .field("buffered", &self.buffered)
            .field("buffer_capacity", &self.buffer_capacity)
            .field("block_size", &self.tail.block_size())
            .field("max_record_size", &self.max_record_size)
            .field("write_pos", &self.tail.write_pos())
            .field("data_end", &self.tail.data_end())
            .field("written_end", &self.tail.written_end())
            .field("durable_end", &self.tail.durable_end())
            .field("state", &self.state)
            .field("dirty", &self.tail.is_dirty())
            .finish_non_exhaustive()
    }
}

impl Writer {
    /// Creates a new WAL file at `path`.
    ///
    /// Fails with [`std::io::ErrorKind::AlreadyExists`] if the file exists:
    /// reopening a WAL with a writer is not supported.
    ///
    /// # Durability
    ///
    /// The file header is durable before this method returns: it is written to
    /// the file and the file is `fdatasync`ed; on Unix the parent directory is
    /// then `fsync`ed as well (platforms without directory fsync treat that
    /// step as a no-op), so a crash cannot leave a directory entry that points
    /// at a file without a valid header. If any step after creating the file
    /// fails, the newly created file is removed again (best effort, with
    /// another parent-directory sync) and the error is returned, so a retry
    /// does not run into a stale empty file.
    ///
    /// # Errors
    ///
    /// Fails with [`std::io::ErrorKind::AlreadyExists`] if the file exists,
    /// with [`Error::InvalidOptions`] for invalid options (including an
    /// unsupported `block_size` or a `max_record_size` out of range), with
    /// [`Error::Unsupported`] when Direct I/O is requested but the platform or
    /// filesystem does not support it, or with the underlying I/O error.
    pub fn create<P: AsRef<Path>>(path: P, options: WriterOptions) -> Result<Writer> {
        Self::create_inner(path.as_ref(), options, Faults::default())
    }

    /// Creates a writer with test-only fault injection enabled.
    #[cfg(test)]
    fn create_with_faults<P: AsRef<Path>>(
        path: P,
        options: WriterOptions,
        faults: Faults,
    ) -> Result<Writer> {
        Self::create_inner(path.as_ref(), options, faults)
    }

    fn create_inner(path: &Path, options: WriterOptions, faults: Faults) -> Result<Writer> {
        validate_options(&options)?;

        let file = file::create_writer(path, options.direct_io)?;

        // In test builds `faults` is stored below; in production builds it is
        // a zero-sized value that exists only to keep this signature uniform.
        #[cfg(not(test))]
        let _ = faults;

        let block_size_field = if options.direct_io {
            options.block_size as u32
        } else {
            0
        };
        let mut writer = Writer {
            file,
            buffered: options.buffered,
            buffer_capacity: options.buffer_capacity,
            max_record_size: options.max_record_size,
            tail: PendingTail::with_header(
                &encode_file_header(block_size_field),
                options.direct_io,
                options.block_size,
            ),
            state: WriterState::Open,
            #[cfg(test)]
            hooks: TestHooks {
                faults,
                ..TestHooks::default()
            },
        };

        if let Err(err) = writer.write_header_durably(path) {
            // The file was created by this call: remove it so a retry does not
            // see a stale empty/partial file. The poisoned state keeps `Drop`
            // from attempting more I/O.
            writer.state = WriterState::Poisoned;
            drop(writer);
            let _ = fs::remove_file(path);
            let _ = file::sync_parent_dir(path);
            return Err(err);
        }
        Ok(writer)
    }

    /// Writes the header and makes it (and its directory entry) durable.
    ///
    /// Order matters: `fdatasync` the file *before* fsyncing the directory, so
    /// the directory entry can never be durable while the inode is not.
    fn write_header_durably(&mut self, path: &Path) -> Result<()> {
        self.sync_pending()?;
        file::sync_parent_dir(path)?;
        Ok(())
    }

    /// Appends a single record to the log.
    ///
    /// The record becomes visible to readers once it has been written to the
    /// file (immediately for unbuffered writers, on spill or [`Writer::flush`]
    /// for buffered writers) and durable after the next successful
    /// [`Writer::flush`] or [`Writer::close`].
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Closed`] after [`Writer::close`] (or after a failed
    /// close that already appended the end-of-file marker), with
    /// [`Error::Poisoned`] after a previous I/O error, with
    /// [`Error::RecordTooLarge`] when `data` exceeds
    /// [`WriterOptions::max_record_size`], or with the underlying I/O error.
    ///
    /// A record whose write returned an error is not counted in
    /// [`Writer::logical_position`].
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        let len = data.len() as u64;
        let max = self.max_record_size as u64;
        if len > max {
            return Err(Error::RecordTooLarge { len, max });
        }

        let prev_data_end = self.tail.data_end();
        let hdr = self.tail.begin_record();
        self.tail.payload_mut().extend_from_slice(data);
        self.tail.finish_record(hdr, RECORD_TYPE_DATA);
        match self.maybe_spill() {
            Ok(()) => Ok(()),
            Err(err) => self.rollback(hdr, prev_data_end, err),
        }
    }

    /// Appends several records atomically.
    ///
    /// All items are stored as one physical record protected by a single
    /// CRC32C, so readers either see every item or none of them. An empty
    /// batch is a no-op. The whole batch is assembled in memory before being
    /// written.
    ///
    /// Size limits are checked incrementally while the batch is assembled, so
    /// an oversized batch fails before allocating most of its payload. On
    /// every error path the writer's buffer and logical position are rolled
    /// back. An I/O failure can still leave partial bytes in the file and
    /// poisons the writer.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Closed`] after [`Writer::close`] (or after a failed
    /// close that already appended the end-of-file marker), with
    /// [`Error::Poisoned`] after a previous I/O error, with
    /// [`Error::RecordTooLarge`] when an item does not fit in a `u32` length
    /// field or the whole batch payload exceeds
    /// [`WriterOptions::max_record_size`], or with the underlying I/O error.
    pub fn write_batch<I, B>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        self.ensure_writable()?;

        let prev_data_end = self.tail.data_end();
        let hdr = self.tail.begin_record();
        let max = self.max_record_size as u64;
        let mut count = 0u64;
        for item in items {
            let bytes = item.as_ref();
            let item_len = bytes.len() as u64;
            // A single item must fit the format's `u32` length field.
            if item_len > MAX_PAYLOAD_LEN {
                return self.rollback(
                    hdr,
                    prev_data_end,
                    Error::RecordTooLarge {
                        len: item_len,
                        max: MAX_PAYLOAD_LEN,
                    },
                );
            }
            // Check the total before appending, so an oversized batch never
            // allocates its whole payload first.
            let payload_len = self.tail.payload(hdr).len() as u64;
            let new_len = payload_len + 4 + item_len;
            if new_len > max {
                return self.rollback(
                    hdr,
                    prev_data_end,
                    Error::RecordTooLarge { len: new_len, max },
                );
            }
            self.tail
                .payload_mut()
                .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            self.tail.payload_mut().extend_from_slice(bytes);
            count += 1;
        }
        if count == 0 {
            // Roll back; an empty batch writes nothing.
            self.tail.rollback(hdr, prev_data_end);
            return Ok(());
        }

        self.tail.finish_record(hdr, RECORD_TYPE_BATCH);
        match self.maybe_spill() {
            Ok(()) => Ok(()),
            Err(err) => self.rollback(hdr, prev_data_end, err),
        }
    }

    /// Makes all previously written records durable.
    ///
    /// Pending buffered records are written to the file first. Unless nothing
    /// new was written since the last successful flush, `fdatasync` is then
    /// called on the file. This holds for every mode: Direct I/O bypasses the
    /// page cache but is **not** a durability guarantee, so a Direct I/O
    /// writer synchronises here as well.
    ///
    /// There is intentionally no separate `sync` method: `flush` is the single
    /// durability entry point.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Poisoned`] if the writer was already poisoned, or
    /// with the underlying I/O error. An I/O error poisons the writer: later
    /// calls return [`Error::Poisoned`]. [`Writer::durable_position`] remains
    /// at the end confirmed by the last successful sync; pending records may
    /// not have reached the file.
    pub fn flush(&mut self) -> Result<()> {
        match self.state {
            WriterState::Poisoned => return Err(Error::Poisoned),
            WriterState::Closed => return Ok(()),
            WriterState::Open | WriterState::EofWritten => {}
        }
        if let Err(err) = self.sync_pending() {
            return Err(self.poison(err));
        }
        if self.state == WriterState::EofWritten {
            // A previous `close` appended the marker but failed before making
            // it durable; a successful flush completes that close.
            self.state = WriterState::Closed;
        }
        Ok(())
    }

    /// Writes the end-of-file marker, flushes, and closes the writer.
    ///
    /// Readers that reach the marker finish iteration (return `None`) instead
    /// of reporting [`Error::Incomplete`]. Closing is idempotent.
    ///
    /// If an I/O error occurs, the end-of-file marker stays logically written:
    /// further `write`/`write_batch` calls fail with [`Error::Closed`], and
    /// calling `close` again retries only the failed I/O — it does **not**
    /// append a second marker. A failed close does not poison the writer.
    /// The marker may already be visible even when its sync failed, so readers
    /// can reach EOF before `close` succeeds. Dropping the writer makes any
    /// pending bytes visible best-effort without syncing; a poisoned writer
    /// performs no I/O on drop.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Poisoned`] if the writer was already poisoned, or
    /// with the underlying I/O error.
    pub fn close(&mut self) -> Result<()> {
        match self.state {
            WriterState::Closed => return Ok(()),
            WriterState::Poisoned => return Err(Error::Poisoned),
            WriterState::Open => {
                self.append_eof();
                self.state = WriterState::EofWritten;
            }
            WriterState::EofWritten => {}
        }
        // Deliberately not poisoning here: the marker has already been
        // appended, so writes are rejected anyway and retrying `close` only
        // repeats the failed I/O.
        self.sync_pending()?;
        self.state = WriterState::Closed;
        Ok(())
    }

    /// Returns whether the writer has been closed and its end-of-file marker
    /// made durable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state == WriterState::Closed
    }

    /// Returns whether the writer has been poisoned by an I/O error.
    ///
    /// A poisoned writer rejects every further operation with
    /// [`Error::Poisoned`]. [`Writer::durable_position`] identifies the last
    /// prefix confirmed by a successful sync for recovery after discarding
    /// the writer.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.state == WriterState::Poisoned
    }

    /// Returns the accepted logical length of the log in bytes.
    ///
    /// This includes the file header and every record whose
    /// `write`/`write_batch` returned `Ok` (buffered records count even before
    /// they are spilled); it never includes a record whose write returned an
    /// error. Once `close` has appended its end-of-file marker, that marker
    /// also counts, even if the following I/O fails.
    ///
    /// This is not a recovery watermark: buffered bytes may never reach the
    /// file if a later operation fails. Use [`Writer::durable_position`] for
    /// the prefix confirmed by the last successful sync.
    #[must_use]
    pub fn logical_position(&self) -> u64 {
        self.tail.data_end()
    }

    /// Returns the logical length confirmed by the last successful sync.
    ///
    /// The position initially includes the durable file header. It advances
    /// after a successful [`Writer::flush`] or [`Writer::close`], and includes
    /// the end-of-file marker after a successful close. Buffered records and
    /// unsynced writes do not advance it; a failed write or sync leaves it
    /// unchanged.
    ///
    /// After an I/O error, discard the writer before truncating the file to
    /// this position to retain only the confirmed prefix. This follows the
    /// crate's crash semantics, including its Direct I/O rewrite assumption.
    /// The file may contain additional readable records beyond this position,
    /// but their durability has not been confirmed.
    #[must_use]
    pub fn durable_position(&self) -> u64 {
        self.tail.durable_end()
    }

    fn ensure_writable(&self) -> Result<()> {
        match self.state {
            WriterState::Open => Ok(()),
            WriterState::Poisoned => Err(Error::Poisoned),
            WriterState::EofWritten | WriterState::Closed => Err(Error::Closed),
        }
    }

    /// Marks the writer as poisoned and returns `err` unchanged.
    fn poison(&mut self, err: Error) -> Error {
        self.state = WriterState::Poisoned;
        err
    }

    /// Rolls the pending-buffer state back to `(buf_len, data_end)` after a
    /// failed write. I/O errors additionally poison the writer.
    fn rollback<T>(&mut self, hdr: usize, data_end: u64, err: Error) -> Result<T> {
        self.tail.rollback(hdr, data_end);
        if matches!(err, Error::Io(_)) {
            self.state = WriterState::Poisoned;
        }
        Err(err)
    }

    /// Appends the end-of-file marker to the pending buffer.
    fn append_eof(&mut self) {
        let hdr = self.tail.begin_record();
        self.tail.finish_record(hdr, RECORD_TYPE_EOF);
    }

    /// Writes pending data out when the buffering policy requires it.
    fn maybe_spill(&mut self) -> Result<()> {
        if !self.buffered || self.tail.pending_len() >= self.buffer_capacity {
            self.write_pending()?;
        }
        Ok(())
    }

    /// Writes all pending bytes in the tail to the file (without syncing).
    ///
    /// This is the only place a spill reaches the file. The pending range is
    /// fetched from the tail (zero-padded to a block boundary in Direct I/O
    /// mode), the positioned write happens here, and only its success commits
    /// the spill; on failure the tail still holds the unwritten records so the
    /// caller can roll them back.
    fn write_pending(&mut self) -> Result<()> {
        let Self {
            file,
            tail,
            #[cfg(test)]
            hooks,
            ..
        } = self;
        let Some(pending) = tail.prepare_spill()? else {
            return Ok(());
        };
        #[cfg(test)]
        if let Some(err) = hooks.before_write() {
            return Err(err);
        }
        file::write_all_at(file, pending.bytes(), pending.offset()).map_err(Error::from)?;
        tail.commit_spill();
        Ok(())
    }

    /// Writes pending bytes and makes them durable: the shared inner path of
    /// [`Writer::flush`], [`Writer::close`] and the creation path.
    ///
    /// It does not touch the lifecycle state, so each caller can apply its own
    /// poison policy.
    fn sync_pending(&mut self) -> Result<()> {
        self.write_pending()?;
        if self.tail.needs_sync() {
            self.sync_file()?;
        }
        Ok(())
    }

    /// One `fdatasync` on the file, then clears the dirty flag.
    fn sync_file(&mut self) -> Result<()> {
        let Self {
            file,
            tail,
            #[cfg(test)]
            hooks,
            ..
        } = self;
        #[cfg(test)]
        if let Some(err) = hooks.before_sync() {
            return Err(err);
        }
        file::sync_data(file)?;
        tail.mark_synced();
        Ok(())
    }
}

/// Validates [`WriterOptions`] before a file is created.
fn validate_options(options: &WriterOptions) -> Result<()> {
    if options.direct_io {
        crate::aligned::check_block_size(options.block_size)?;
    }
    if options.buffered && options.buffer_capacity == 0 {
        return Err(Error::InvalidOptions(
            "buffer_capacity must be greater than zero when buffered writes are enabled"
                .to_string(),
        ));
    }
    crate::format::validate_max_record_size(options.max_record_size)?;
    Ok(())
}

impl Drop for Writer {
    fn drop(&mut self) {
        match self.state {
            // A poisoned writer must not touch the file again; a closed one
            // has nothing left to write.
            WriterState::Closed | WriterState::Poisoned => return,
            WriterState::Open | WriterState::EofWritten => {}
        }
        // Best effort: make buffered bytes visible to readers. Without
        // `close`, readers will see the tail as incomplete, which is the
        // documented signal for a writer that went away.
        let _ = self.write_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{FILE_HEADER_SIZE, RECORD_HEADER_SIZE, RECORD_TRAILER_SIZE};
    use crate::{Reader, ReaderOptions};

    /// Encoded length of a single DATA record with `payload_len` bytes of
    /// payload (header + payload + CRC trailer), excluding the file header.
    fn encoded_data_record(payload_len: usize) -> u64 {
        (RECORD_HEADER_SIZE + RECORD_TRAILER_SIZE + payload_len) as u64
    }

    #[test]
    fn create_fails_on_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"hello").unwrap();
        w.close().unwrap();
        drop(w);

        let err = Writer::create(&path, WriterOptions::default()).unwrap_err();
        match err {
            Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
            other => panic!("unexpected error: {other}"),
        }
        // The pre-existing file was not removed by the failed create.
        assert!(path.exists());
    }

    #[test]
    fn write_after_close_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.close().unwrap();
        assert!(matches!(w.write(b"x"), Err(Error::Closed)));
        assert!(matches!(
            w.write_batch([b"x".as_slice()]),
            Err(Error::Closed)
        ));
        // Idempotent close and harmless flush.
        w.close().unwrap();
        w.flush().unwrap();
    }

    #[test]
    fn empty_batch_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        let before = w.logical_position();
        w.write_batch(std::iter::empty::<&[u8]>()).unwrap();
        assert_eq!(w.logical_position(), before);
        w.close().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, 16 + 9);
    }

    #[test]
    fn buffered_writer_hides_records_until_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::new().with_buffered(true)).unwrap();
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);
        w.write(b"buffered record").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, 16);
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);
        assert!(w.logical_position() > w.durable_position());
        w.flush().unwrap();
        assert!(fs::metadata(&path).unwrap().len() as usize > 16);
        assert_eq!(w.durable_position(), w.logical_position());
        w.close().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
    }

    #[test]
    fn invalid_options_do_not_create_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");

        let err = Writer::create(
            &path,
            WriterOptions::new()
                .with_direct_io(true)
                .with_block_size(1000),
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidOptions(_)));
        assert!(!path.exists(), "invalid block_size must not create a file");

        let err = Writer::create(&path, WriterOptions::new().with_max_record_size(0)).unwrap_err();
        assert!(matches!(err, Error::InvalidOptions(_)));
        assert!(!path.exists());

        #[cfg(target_pointer_width = "64")]
        {
            let err = Writer::create(&path, WriterOptions::new().with_max_record_size(usize::MAX))
                .unwrap_err();
            assert!(matches!(err, Error::InvalidOptions(_)));
            assert!(!path.exists());
        }
    }

    #[test]
    fn write_batch_rolls_back_on_oversized_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        // Small convenient limit so the multi-GiB path is covered cheaply.
        let mut w = Writer::create(&path, WriterOptions::new().with_max_record_size(64)).unwrap();
        w.write(b"good-1").unwrap();
        let pos = w.logical_position();
        let file_len = fs::metadata(&path).unwrap().len();

        let err = w.write_batch([vec![1u8; 40], vec![2u8; 40]]).unwrap_err();
        match err {
            Error::RecordTooLarge { len, max } => {
                assert_eq!(max, 64);
                assert!(len > 64);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            w.logical_position(),
            pos,
            "failed batch must not move position"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            file_len,
            "failed batch must not touch the file"
        );
        assert!(!w.is_poisoned(), "a size error is not an I/O error");

        // The writer remains usable and the log stays readable.
        w.write(b"good-2").unwrap();
        w.close().unwrap();
        drop(w);

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"good-1");
        assert_eq!(r.next().unwrap().unwrap(), b"good-2");
        assert!(r.next().is_none());
    }

    #[test]
    fn batch_limits_are_checked_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::new().with_max_record_size(64)).unwrap();

        // Exactly at the limit: 4-byte item prefix + 60 bytes == 64.
        w.write_batch([vec![0u8; 60]]).unwrap();
        let pos = w.logical_position();
        assert_eq!(pos, FILE_HEADER_SIZE as u64 + encoded_data_record(64));

        // One byte over is rejected and rolled back.
        let err = w.write_batch([vec![0u8; 61]]).unwrap_err();
        assert!(matches!(err, Error::RecordTooLarge { len: 65, max: 64 }));
        assert_eq!(w.logical_position(), pos);
        w.close().unwrap();
    }

    #[test]
    fn failed_write_poisons_writer_and_keeps_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"committed").unwrap();
        let pos = w.logical_position();
        let calls = w.hooks.write_calls;

        w.hooks.faults.fail_write_after = Some(calls);
        let err = w.write(b"not-written").unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert_eq!(
            w.logical_position(),
            pos,
            "failed write must not move position"
        );
        assert!(w.is_poisoned());

        // Every later operation is rejected, including close.
        assert!(matches!(w.write(b"x"), Err(Error::Poisoned)));
        assert!(matches!(
            w.write_batch([b"x".as_slice()]),
            Err(Error::Poisoned)
        ));
        assert!(matches!(w.flush(), Err(Error::Poisoned)));
        assert!(matches!(w.close(), Err(Error::Poisoned)));

        // Dropping a poisoned writer performs no I/O: the file keeps its
        // committed contents.
        let file_len = fs::metadata(&path).unwrap().len();
        drop(w);
        assert_eq!(fs::metadata(&path).unwrap().len(), file_len);
    }

    #[test]
    fn failed_buffered_flush_preserves_durable_recovery_prefix() {
        for fail_sync in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("w.wal");
            let mut w = Writer::create(&path, WriterOptions::new().with_buffered(true)).unwrap();
            w.write(b"durable").unwrap();
            w.flush().unwrap();
            let durable = w.durable_position();

            w.write_batch([b"pending".as_slice(), b"batch".as_slice()])
                .unwrap();
            let logical = w.logical_position();
            assert!(logical > durable);
            assert_eq!(w.durable_position(), durable);
            assert_eq!(fs::metadata(&path).unwrap().len(), durable);

            if fail_sync {
                w.hooks.faults.fail_sync_after = Some(w.hooks.sync_calls);
            } else {
                w.hooks.faults.fail_write_after = Some(w.hooks.write_calls);
            }
            assert!(matches!(w.flush(), Err(Error::Io(_))));
            assert!(w.is_poisoned());
            assert_eq!(w.logical_position(), logical);
            assert_eq!(w.durable_position(), durable);
            drop(w);

            // A write failure leaves the batch only in memory; a sync failure
            // leaves it visible but not confirmed durable. Both recover from
            // the same last successful barrier without extending the file.
            let file_len = fs::metadata(&path).unwrap().len();
            assert_eq!(file_len, if fail_sync { logical } else { durable });
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(durable)
                .unwrap();
            let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
            assert_eq!(r.next().unwrap().unwrap(), b"durable");
            assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
        }
    }

    #[test]
    fn flush_skips_sync_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        let create_syncs = w.hooks.sync_calls;
        assert_eq!(create_syncs, 1, "create must fdatasync the header");

        w.write(b"a").unwrap();
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);
        w.flush().unwrap();
        assert_eq!(w.hooks.sync_calls, create_syncs + 1);
        let durable = w.durable_position();
        assert_eq!(durable, w.logical_position());

        // No new data: no fdatasync (P5).
        w.flush().unwrap();
        assert_eq!(w.hooks.sync_calls, create_syncs + 1);
        assert_eq!(w.durable_position(), durable);

        w.write(b"b").unwrap();
        assert_eq!(w.durable_position(), durable);
        w.flush().unwrap();
        assert_eq!(w.hooks.sync_calls, create_syncs + 2);
        assert_eq!(w.durable_position(), w.logical_position());
    }

    #[test]
    fn close_sync_failure_retries_without_duplicate_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"a").unwrap();
        let syncs = w.hooks.sync_calls;

        w.hooks.faults.fail_sync_after = Some(syncs);
        let err = w.close().unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(!w.is_closed());
        assert!(!w.is_poisoned(), "a close failure must stay retryable");
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);

        // The EOF marker is already logically written: writes are rejected.
        assert!(matches!(w.write(b"b"), Err(Error::Closed)));
        assert!(matches!(
            w.write_batch([b"b".as_slice()]),
            Err(Error::Closed)
        ));

        // Retrying only repeats the sync and appends no second EOF marker.
        w.hooks.faults.fail_sync_after = None;
        w.close().unwrap();
        assert!(w.is_closed());
        assert_eq!(w.durable_position(), w.logical_position());
        w.close().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            FILE_HEADER_SIZE as u64 + encoded_data_record(1) + encoded_data_record(0),
            "exactly one EOF marker"
        );

        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"a");
        assert!(r.next().is_none());
    }

    #[test]
    fn close_write_failure_retries_without_duplicate_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"a").unwrap();
        let calls = w.hooks.write_calls;

        w.hooks.faults.fail_write_after = Some(calls);
        let err = w.close().unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(!w.is_closed());
        assert!(!w.is_poisoned());
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);
        assert!(matches!(w.write(b"b"), Err(Error::Closed)));

        w.hooks.faults.fail_write_after = None;
        w.close().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            FILE_HEADER_SIZE as u64 + encoded_data_record(1) + encoded_data_record(0),
            "exactly one EOF marker"
        );
        let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"a");
        assert!(r.next().is_none());
    }

    #[test]
    fn create_cleans_up_when_header_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        let err = Writer::create_with_faults(
            &path,
            WriterOptions::default(),
            Faults {
                fail_write_after: Some(0),
                ..Faults::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(!path.exists(), "failed create must remove its file");

        // A retry is not blocked by a stale empty file.
        let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
        w.write(b"ok").unwrap();
        w.close().unwrap();
    }

    #[cfg(all(target_os = "linux", not(miri)))]
    #[test]
    fn direct_io_flush_still_fdatasyncs() {
        let dir = tempfile::tempdir().unwrap();
        if !crate::test_support::direct_io_supported(&dir) {
            eprintln!("skipping: direct I/O not supported on this filesystem");
            return;
        }
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap();
        let syncs = w.hooks.sync_calls;
        w.write(b"direct record").unwrap();
        assert_eq!(w.durable_position(), FILE_HEADER_SIZE as u64);
        w.flush().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
        assert_eq!(
            w.hooks.sync_calls,
            syncs + 1,
            "a Direct I/O flush must still fdatasync (C1)"
        );
        // A second flush with nothing new must not rewrite the tail block nor
        // sync again (P5), even though the retained tail block keeps `buf`
        // non-empty in Direct I/O mode.
        let writes = w.hooks.write_calls;
        w.flush().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
        assert_eq!(
            w.hooks.sync_calls,
            syncs + 1,
            "redundant direct flush synced"
        );
        assert_eq!(
            w.hooks.write_calls, writes,
            "redundant direct flush rewrote"
        );
        w.close().unwrap();
        assert_eq!(w.durable_position(), w.logical_position());
        assert_eq!(
            w.hooks.sync_calls,
            syncs + 2,
            "close must make the EOF marker durable (C1)"
        );
    }

    #[cfg(all(target_os = "linux", not(miri)))]
    #[test]
    fn direct_io_staging_allocation_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        if !crate::test_support::direct_io_supported(&dir) {
            eprintln!("skipping: direct I/O not supported on this filesystem");
            return;
        }
        let path = dir.path().join("w.wal");
        let mut w = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap();
        // A 10000-byte record makes the pending buffer 16 + 9 + 10000 = 10025
        // bytes, which rounds up to three 4 KiB blocks. The old
        // `next_power_of_two()` staging allocation would have been 16384.
        w.write(&vec![0x5a; 10_000]).unwrap();
        let staging_len = w.tail.staging_len().expect("staging allocated");
        assert_eq!(
            staging_len,
            3 * w.tail.block_size(),
            "staging must be exact (P6)"
        );
        assert_ne!(staging_len, 16_384);
        w.close().unwrap();
    }

    #[cfg(all(target_os = "linux", not(miri)))]
    #[test]
    fn direct_open_failure_is_unsupported_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wal");
        crate::file::inject_direct_open_failure();
        let err = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
        assert!(
            !path.exists(),
            "a failed Direct I/O open must remove the file it created"
        );
    }
}
