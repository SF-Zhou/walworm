//! Helpers shared by the crate's unit tests.
//!
//! This module is only compiled for `cfg(test)`. Integration tests and the
//! benchmark are separate crates and use `tests/common/mod.rs` instead.

use std::fs;

use tempfile::TempDir;

use crate::{Error, Writer, WriterOptions};

/// Probes whether Direct I/O works on the filesystem holding `dir`.
///
/// Creates a real WAL with `direct_io(true)`, writes and closes it, then
/// removes the probe file. Only an explicit `Unsupported` error permits
/// skipping tests; other failures must expose regressions in the write path.
/// Setting `WALWORM_REQUIRE_DIRECT_IO` also makes unsupported Direct I/O fail.
pub(crate) fn direct_io_supported(dir: &TempDir) -> bool {
    let probe = dir.path().join(".direct-io-probe.wal");
    let result =
        Writer::create(&probe, WriterOptions::new().with_direct_io(true)).and_then(|mut writer| {
            writer.write(b"probe")?;
            writer.close()
        });
    let _ = fs::remove_file(&probe);
    match result {
        Ok(()) => true,
        Err(Error::Unsupported(reason)) => {
            assert!(
                std::env::var_os("WALWORM_REQUIRE_DIRECT_IO").is_none(),
                "Direct I/O is required but unsupported: {reason}"
            );
            false
        }
        Err(err) => panic!("Direct I/O probe failed: {err}"),
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
#[test]
#[should_panic(expected = "Direct I/O probe failed:")]
fn unexpected_probe_error_fails_the_test() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".direct-io-probe.wal")).unwrap();
    direct_io_supported(&dir);
}
