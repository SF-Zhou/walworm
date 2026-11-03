//! Shared helpers for integration tests and the benchmark.
//!
//! Kept free of `#[test]` functions: each integration test binary includes this
//! module with `mod common;`, and `benches/throughput.rs` includes it by path.

#![allow(dead_code)]

use std::fs;

use tempfile::TempDir;
use walworm::{Error, Writer, WriterOptions};

/// Deterministic xorshift64* PRNG so fuzz runs are reproducible.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Probes whether Direct I/O works on the filesystem holding `dir`.
///
/// Creates a real WAL with `direct_io(true)`, writes and closes it; the probe
/// file is removed afterwards. Only an explicit `Unsupported` error permits
/// skipping tests; other failures must expose regressions in the write path.
/// Setting `WALWORM_REQUIRE_DIRECT_IO` also makes unsupported Direct I/O fail,
/// so CI can require these tests to execute.
pub fn direct_io_supported(dir: &TempDir) -> bool {
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
