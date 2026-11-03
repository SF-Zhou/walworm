//! Platform-specific file helpers: opening files (optionally with Direct
//! I/O), positioned reads/writes and durability primitives.
//!
//! Buffered positioned I/O is implemented on Unix (`FileExt::write_at` /
//! `read_at`) and on Windows (`FileExt::seek_write` / `seek_read`); platforms
//! with neither get stubs that return [`std::io::ErrorKind::Unsupported`].
//! Direct I/O (`O_DIRECT`) remains Linux-only; other platforms reject
//! `direct_io` with [`crate::Error::Unsupported`].
//!
//! Every I/O helper in this module retries operations that fail with
//! [`std::io::ErrorKind::Interrupted`] (POSIX `EINTR`).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::{Error, Result};

/// Retries an I/O operation while it is interrupted by a signal.
///
/// `read`, `write`, `pread`, `pwrite`, `fsync` and friends may fail with
/// [`io::ErrorKind::Interrupted`] even when they transferred no data; POSIX
/// requires the caller to retry with the same arguments, which is what this
/// helper does. Any other error is returned unchanged.
fn retry_interrupted<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

/// Opens a file for reading, optionally with Direct I/O.
pub(crate) fn open_reader(path: &Path, direct: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    apply_direct_flag(&mut opts, direct)?;
    opts.open(path).map_err(|e| map_open_error(e, direct))
}

/// Creates a new file for writing, optionally with Direct I/O.
///
/// The file is created with `create_new(true)` so that opening an existing
/// WAL fails: a walworm writer never reopens an existing file. If the open
/// fails for any reason other than `AlreadyExists`, a file that this call may
/// have created is removed again (best effort) so that a retry does not see a
/// stale empty file; a pre-existing file is never removed.
pub(crate) fn create_writer(path: &Path, direct: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    apply_direct_flag(&mut opts, direct)?;
    // `create_new(true)` guarantees `AlreadyExists` when the file pre-exists,
    // so an error of any other kind means the path was free (or became free)
    // and any file now present was created by this failed open.
    let existed_before = path.symlink_metadata().is_ok();

    #[cfg(all(test, target_os = "linux"))]
    if direct && take_injected_open_failure() {
        // A real `O_DIRECT` open can create the file and then fail (for
        // example on a filesystem that does not support O_DIRECT); simulate
        // exactly that so the cleanup path is covered by a unit test.
        let _ = OpenOptions::new().write(true).create_new(true).open(path);
        return Err(open_error(
            path,
            existed_before,
            io::Error::from(io::ErrorKind::InvalidInput),
            direct,
        ));
    }

    opts.open(path)
        .map_err(|e| open_error(path, existed_before, e, direct))
}

/// Maps an open error to a walworm error.
///
/// `O_DIRECT` on a filesystem that does not support it (or a block size the
/// filesystem rejects) surfaces as `EINVAL` (`ErrorKind::InvalidInput`) on most
/// filesystems and as `EOPNOTSUPP`/`ENOTSUP` (`ErrorKind::Unsupported`) on
/// some; report both as [`Error::Unsupported`] instead of a bare I/O error, so
/// callers (and the Direct I/O test probe) can tell "this file system cannot do
/// Direct I/O" apart from a real failure.
fn map_open_error(err: io::Error, direct: bool) -> Error {
    let unsupported_kind = matches!(
        err.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    );
    if direct && unsupported_kind {
        Error::Unsupported(
            "Direct I/O (O_DIRECT) is not supported for this path, or the configured block_size \
             is incompatible with the filesystem; try `direct_io = false`",
        )
    } else {
        Error::Io(err)
    }
}

/// Maps an open error and cleans up a file that the failed `create_new` open
/// may have created.
fn open_error(path: &Path, existed_before: bool, err: io::Error, direct: bool) -> Error {
    let pre_existing = existed_before || err.kind() == io::ErrorKind::AlreadyExists;
    let mapped = map_open_error(err, direct);
    // Best effort: only consider removing the file when it cannot have
    // pre-existed. `remove_file` refuses directories, so pointing `path` at
    // a directory cannot delete it.
    if !pre_existing && path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(path);
        let _ = sync_parent_dir(path);
    }
    mapped
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)] // other platforms can fail here
fn apply_direct_flag(opts: &mut OpenOptions, direct: bool) -> Result<()> {
    if direct {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECT);
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_direct_flag(_opts: &mut OpenOptions, direct: bool) -> Result<()> {
    if direct {
        return Err(Error::Unsupported(
            "direct I/O is currently only supported on Linux",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_direct_flag(_opts: &mut OpenOptions, direct: bool) -> Result<()> {
    if direct {
        return Err(Error::Unsupported(
            "direct I/O is currently only supported on Linux",
        ));
    }
    Ok(())
}

/// Performs one positioned write of `buf` at `offset` (Unix `pwrite`).
#[cfg(unix)]
pub(crate) fn raw_write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// Performs one positioned write of `buf` at `offset` (Windows `seek_write`).
#[cfg(windows)]
pub(crate) fn raw_write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

/// Positioned writes are not available on this platform.
#[cfg(not(any(unix, windows)))]
pub(crate) fn raw_write_at(_file: &File, _buf: &[u8], _offset: u64) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned writes are not supported on this platform",
    ))
}

/// Performs one positioned read into `buf` at `offset` (Unix `pread`).
#[cfg(unix)]
pub(crate) fn raw_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

/// Performs one positioned read into `buf` at `offset` (Windows `seek_read`).
#[cfg(windows)]
pub(crate) fn raw_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

/// Positioned reads are not available on this platform.
#[cfg(not(any(unix, windows)))]
pub(crate) fn raw_read_at(_file: &File, _buf: &mut [u8], _offset: u64) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned reads are not supported on this platform",
    ))
}

/// Writes the whole buffer at `offset`, retrying partial writes and `EINTR`.
///
/// A positioned write may transfer fewer bytes than requested; the remaining
/// suffix is written at the advanced offset. A zero-byte write cannot make
/// progress and is reported as [`io::ErrorKind::WriteZero`].
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = retry_interrupted(|| raw_write_at(file, buf, offset))?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write to file",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Reads at `offset`, returning as many bytes as are currently available
/// (short reads happen at end of file; `EINTR` is retried).
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    retry_interrupted(|| raw_read_at(file, buf, offset))
}

/// Reads exactly `buf.len()` bytes at `offset`, retrying short reads.
///
/// A positioned read may legally return fewer bytes than requested (signals,
/// filesystem behaviour, emulation layers); only a zero-length read means the
/// file ended. Returns `Ok(false)` when EOF is reached before `buf` is full.
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<bool> {
    while !buf.is_empty() {
        let n = read_at(file, buf, offset)?;
        if n == 0 {
            return Ok(false);
        }
        let rest = std::mem::take(&mut buf);
        buf = &mut rest[n..];
        offset += n as u64;
    }
    Ok(true)
}

/// Flushes data and metadata to stable storage (`fdatasync`), retrying `EINTR`.
///
/// Retrying a failed `fdatasync` is safe: it is idempotent.
pub(crate) fn sync_data(file: &File) -> io::Result<()> {
    retry_interrupted(|| file.sync_data())
}

/// Resolves the directory whose entry must be durable for `path`.
///
/// The parent of a bare relative filename such as `"log.wal"` is the empty
/// path, which cannot be opened; `"."` is used instead, so plain relative
/// paths are resolved correctly.
#[cfg(unix)]
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Fsyncs the parent directory of `path` so a newly created file's directory
/// entry is durable, returning any error.
///
/// This is what makes the crash-safe create/cleanup sequence complete:
/// `fdatasync(file)` only persists the file's own data and size, while the
/// directory entry (`name -> inode`) lives in the parent directory and needs
/// its own `fsync` on a directory file descriptor.
///
/// Miri cannot emulate `fsync` on a directory file descriptor ("only supported
/// on file-backed file descriptors") and does not model directory durability
/// anyway, so there it is a no-op; real platforms always perform the syscall.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = parent_dir(path);
    #[cfg(miri)]
    {
        let _ = parent;
        Ok(())
    }
    #[cfg(not(miri))]
    {
        let dir = retry_interrupted(|| File::open(parent))?;
        retry_interrupted(|| dir.sync_all())
    }
}

/// Directories cannot be opened for syncing on this platform, so this is a
/// no-op that always succeeds.
#[cfg(not(unix))]
pub(crate) fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Arms a one-shot simulated Direct I/O open failure for the next
/// [`create_writer`] call on this thread. Test-only and Linux-only: it
/// exercises the Linux `O_DIRECT` error mapping/cleanup path.
#[cfg(all(test, target_os = "linux", not(miri)))]
pub(crate) fn inject_direct_open_failure() {
    INJECT_DIRECT_OPEN_FAILURE.with(|flag| flag.set(true));
}

#[cfg(all(test, target_os = "linux"))]
fn take_injected_open_failure() -> bool {
    INJECT_DIRECT_OPEN_FAILURE.with(|flag| flag.replace(false))
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static INJECT_DIRECT_OPEN_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_interrupted_retries_only_interrupted() {
        use std::cell::Cell;

        let calls = Cell::new(0);
        let value = retry_interrupted(|| {
            calls.set(calls.get() + 1);
            if calls.get() <= 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(7u8)
            }
        })
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(calls.get(), 4, "EINTR must be retried until success");

        let calls = Cell::new(0);
        let err = retry_interrupted(|| {
            calls.set(calls.get() + 1);
            Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls.get(), 1, "other errors must not be retried");
    }

    #[test]
    fn map_open_error_classifies_direct_io_failures() {
        // Both EINVAL and EOPNOTSUPP/ENOTSUP mean "this file system cannot do
        // Direct I/O", and must be reported as `Unsupported` so tests can skip
        // and callers can fall back to buffered I/O.
        for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::Unsupported] {
            assert!(matches!(
                map_open_error(io::Error::from(kind), true),
                Error::Unsupported(_)
            ));
        }
        // The same errors without Direct I/O are ordinary I/O failures.
        for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::Unsupported] {
            assert!(matches!(
                map_open_error(io::Error::from(kind), false),
                Error::Io(_)
            ));
        }
        // Unrelated failures are never reclassified.
        assert!(matches!(
            map_open_error(io::Error::from(io::ErrorKind::PermissionDenied), true),
            Error::Io(_)
        ));
    }

    #[test]
    fn read_exact_at_loops_and_reports_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = File::open(&path).unwrap();

        // A full read at the start.
        let mut all = [0u8; 10];
        assert!(read_exact_at(&file, &mut all, 0).unwrap());
        assert_eq!(&all, b"0123456789");

        // A zero-length read succeeds without touching the file.
        assert!(read_exact_at(&file, &mut [], 0).unwrap());

        // Reading past the end returns false, but fills the available prefix.
        let mut tail = [0u8; 4];
        assert!(!read_exact_at(&file, &mut tail, 7).unwrap());
        assert_eq!(&tail[..3], b"789");

        // Reading exactly the tail succeeds.
        let mut exact = [0u8; 3];
        assert!(read_exact_at(&file, &mut exact, 7).unwrap());
        assert_eq!(&exact, b"789");
    }

    #[cfg(unix)]
    #[test]
    fn parent_dir_resolution_handles_bare_relative_paths() {
        // `Path::new("log.wal").parent()` is the empty path, which cannot be
        // opened; it must be mapped to `"."` (this used to be swallowed, so a
        // bare relative filename never fsynced its directory: C6).
        assert_eq!(parent_dir(Path::new("log.wal")), Path::new("."));
        assert_eq!(parent_dir(Path::new("./log.wal")), Path::new("."));
        assert_eq!(parent_dir(Path::new("dir/log.wal")), Path::new("dir"));
        assert_eq!(parent_dir(Path::new("/tmp/x.log")), Path::new("/tmp"));
    }

    // Miri rejects `fsync` on directory file descriptors, so the syscall itself
    // is only exercised on real file systems.
    #[cfg(all(unix, not(miri)))]
    #[test]
    fn sync_parent_dir_handles_bare_relative_paths() {
        sync_parent_dir(Path::new("log.wal")).unwrap();
        sync_parent_dir(Path::new("./log.wal")).unwrap();
    }
}
