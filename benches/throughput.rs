//! End-to-end write/read throughput benchmark for walworm.
//!
//! Run with `cargo bench --bench throughput`. The harness has no dependencies beyond
//! `tempfile` and deliberately restricts itself to the long-stable public API
//! (`Writer::{create, write, flush, close}`,
//! `Reader::{open, next, position}`, `WriterOptions`, `ReaderOptions`) so the
//! numbers stay comparable as the incremental-read APIs evolve.
//!
//! Every case writes `count` records, flushes (the timed region includes the
//! `flush`), closes the log, and then re-reads it end to end with the default
//! (buffered) reader. Direct I/O is optional: support is probed by trying to
//! create a writer with `direct_io(true)`; when the file system rejects
//! `O_DIRECT`, those rows are skipped.

#![allow(clippy::cast_precision_loss, clippy::missing_const_for_fn)]

use std::path::Path;
use std::time::{Duration, Instant};

use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions};

#[path = "../tests/common/mod.rs"]
mod common;

use common::direct_io_supported;

/// Record sizes exercised for every write mode.
const RECORD_SIZES: [usize; 2] = [64, 4096];

/// One writer configuration.
#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    buffered: bool,
    direct: bool,
}

/// The measured outcome of a single write+read case.
struct Case {
    records: usize,
    record_size: usize,
    write_elapsed: Duration,
    read_elapsed: Duration,
}

fn main() {
    let dir = tempfile::tempdir().expect("failed to create the benchmark directory");
    let direct_supported = direct_io_supported(&dir);

    let modes = [
        Mode {
            name: "unbuffered",
            buffered: false,
            direct: false,
        },
        Mode {
            name: "buffered",
            buffered: true,
            direct: false,
        },
        Mode {
            name: "direct",
            buffered: false,
            direct: true,
        },
        Mode {
            name: "direct+buffered",
            buffered: true,
            direct: true,
        },
    ];

    println!("walworm write/read throughput");
    println!("write time includes flush(); read uses the default (buffered) reader");
    if !direct_supported {
        println!("note: Direct I/O is not supported on this file system; skipping direct rows");
    }
    println!();
    println!(
        "{:<16} {:>6} {:>8} {:>13} {:>11} {:>13} {:>11}",
        "mode", "rec B", "records", "write ops/s", "write MB/s", "read ops/s", "read MB/s"
    );

    for record_size in RECORD_SIZES {
        for mode in modes {
            if mode.direct && !direct_supported {
                continue;
            }
            let path = dir.path().join(format!(
                "wal-{}-{record_size}.wal",
                mode.name.replace('+', "-")
            ));
            let count = record_count(record_size, mode);
            let case = run_case(&path, record_size, count, mode);
            println!(
                "{:<16} {:>6} {:>8} {:>13.0} {:>11.1} {:>13.0} {:>11.1}",
                mode.name,
                case.record_size,
                case.records,
                ops_per_sec(case.records, case.write_elapsed),
                mb_per_sec(case.records * case.record_size, case.write_elapsed),
                ops_per_sec(case.records, case.read_elapsed),
                mb_per_sec(case.records * case.record_size, case.read_elapsed),
            );
        }
    }
}

/// Record count for one case. Direct I/O without buffering rewrites the whole
/// tail block on every small write (large write amplification), so that mode
/// gets fewer records to keep the whole run short.
fn record_count(record_size: usize, mode: Mode) -> usize {
    let base = if record_size == 64 { 200_000 } else { 20_000 };
    if mode.direct && !mode.buffered {
        base / 10
    } else {
        base
    }
}

/// Writes `count` records of `record_size` bytes, flushes, closes, then reads
/// the log back with the default reader.
fn run_case(path: &Path, record_size: usize, count: usize, mode: Mode) -> Case {
    let payload = vec![0xA5_u8; record_size];
    let options = WriterOptions::new()
        .with_buffered(mode.buffered)
        .with_direct_io(mode.direct);

    let mut writer = Writer::create(path, options).expect("failed to create the WAL file");
    let write_start = Instant::now();
    for _ in 0..count {
        writer.write(&payload).expect("failed to write a record");
    }
    writer.flush().expect("failed to flush the WAL");
    let write_elapsed = write_start.elapsed();
    writer.close().expect("failed to close the WAL");

    let mut reader = Reader::open(path, ReaderOptions::default()).expect("failed to open the WAL");
    let read_start = Instant::now();
    let mut read_records = 0usize;
    let mut read_bytes = 0usize;
    loop {
        match reader.next() {
            Some(Ok(record)) => {
                read_records += 1;
                read_bytes += record.len();
            }
            Some(Err(Error::Incomplete)) | None => break,
            Some(Err(error)) => panic!("failed to read a record: {error}"),
        }
    }
    let read_elapsed = read_start.elapsed();

    assert_eq!(read_records, count, "read back a different record count");
    assert_eq!(
        read_bytes,
        count * record_size,
        "read back a different number of bytes"
    );

    Case {
        records: count,
        record_size,
        write_elapsed,
        read_elapsed,
    }
}

fn ops_per_sec(records: usize, elapsed: Duration) -> f64 {
    records as f64 / elapsed.as_secs_f64().max(1e-9)
}

fn mb_per_sec(bytes: usize, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64().max(1e-9) / (1024.0 * 1024.0)
}
