//! Deterministic robustness tests for the reader parser.
//!
//! This is a dependency-free stand-in for a `cargo-fuzz` target: it feeds the
//! reader truncated, mutated and random files and asserts that it never panics
//! or hangs and that every record it yields is a correct prefix of the model.
//! The PRNG is seeded, so failures are reproducible.

use std::fs;
use std::path::Path;

use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions};

mod common;

use common::Rng;

/// Maximum `next()` calls per reader instance. A live/truncated file must
/// either make progress or settle on an error, never spin forever.
const MAX_STEPS: usize = 10_000;

/// Reads a file and returns (number of records yielded, first error), asserting
/// that every yielded record matches `model` in order.
fn scan(path: &Path, model: &[Vec<u8>]) -> (usize, Option<Error>) {
    let mut reader = match Reader::open(path, ReaderOptions::default()) {
        Ok(r) => r,
        Err(e) => return (0, Some(e)),
    };
    let mut idx = 0usize;
    for _ in 0..MAX_STEPS {
        match reader.next() {
            Some(Ok(rec)) => {
                assert!(
                    idx < model.len(),
                    "reader yielded more records than the model has"
                );
                assert_eq!(&rec, &model[idx], "record #{idx} does not match the model");
                idx += 1;
            }
            Some(Err(e)) => return (idx, Some(e)),
            None => return (idx, None),
        }
    }
    panic!("reader did not terminate after {MAX_STEPS} steps");
}

/// Builds a valid WAL file and returns its bytes plus the model records.
fn build_valid_wal(path: &Path) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut model: Vec<Vec<u8>> = Vec::new();
    let mut w = Writer::create(path, WriterOptions::default()).unwrap();
    w.write(b"").unwrap();
    model.push(Vec::new());
    w.write(b"hello").unwrap();
    model.push(b"hello".to_vec());
    w.write(&vec![0xAB; 1000]).unwrap();
    model.push(vec![0xAB; 1000]);
    w.write_batch([b"atom".as_slice(), b"".as_slice(), b"batch-item".as_slice()])
        .unwrap();
    model.push(b"atom".to_vec());
    model.push(Vec::new());
    model.push(b"batch-item".to_vec());
    w.write(&vec![0x00; 4096]).unwrap();
    model.push(vec![0x00; 4096]);
    w.close().unwrap();
    drop(w);
    (fs::read(path).unwrap(), model)
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
}

/// Errors the reader may legitimately return for arbitrary input. Anything
/// else (panic, hang, unexpected variant) is a bug.
fn assert_expected_error(e: &Error) {
    assert!(
        matches!(
            e,
            Error::Incomplete
                | Error::Corruption { .. }
                | Error::RecordTooLarge { .. }
                | Error::UnknownRecordType { .. }
                | Error::NotAWalworm
                | Error::UnsupportedVersion { .. }
                | Error::Io(_)
        ),
        "unexpected error kind from a robust reader: {e:?}"
    );
}

#[test]
fn truncation_every_offset_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let valid_path = dir.path().join("valid.wal");
    let (bytes, model) = build_valid_wal(&valid_path);

    let scratch = dir.path().join("scratch.wal");
    for cut in 0..=bytes.len() {
        write_file(&scratch, &bytes[..cut]);
        let (n, err) = scan(&scratch, &model);
        if let Some(e) = err {
            assert_expected_error(&e);
        }
        // A truncated file can never expose more records than the full one,
        // and every exposed record matched the model (asserted in `scan`).
        assert!(n <= model.len());
    }
}

#[test]
fn single_byte_mutations_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let valid_path = dir.path().join("valid.wal");
    let (bytes, model) = build_valid_wal(&valid_path);

    let scratch = dir.path().join("scratch.wal");
    // Every byte position, a few different bit patterns.
    for pos in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xFF] {
            let mut mutated = bytes.clone();
            mutated[pos] ^= mask;
            write_file(&scratch, &mutated);
            let (n, err) = scan(&scratch, &model);
            if let Some(e) = err {
                assert_expected_error(&e);
            }
            assert!(n <= model.len());
        }
    }
}

#[test]
fn multi_byte_mutations_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let valid_path = dir.path().join("valid.wal");
    let (bytes, model) = build_valid_wal(&valid_path);
    let scratch = dir.path().join("scratch.wal");
    let mut rng = Rng(0x5eed_1234_5678_9abc);

    for _ in 0..2000 {
        let mut mutated = bytes.clone();
        let flips = 1 + rng.below(8);
        for _ in 0..flips {
            let pos = rng.below(mutated.len());
            mutated[pos] ^= 1u8 << rng.below(8);
        }
        write_file(&scratch, &mutated);
        let (n, err) = scan(&scratch, &model);
        if let Some(e) = err {
            assert_expected_error(&e);
        }
        assert!(n <= model.len());
    }
}

#[test]
fn random_files_are_rejected_or_scanned_safely() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.wal");
    let mut rng = Rng(0xdead_c0de_1234_5678);

    for len in [0usize, 1, 4, 15, 16, 17, 64, 4096] {
        for _ in 0..50 {
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            write_file(&scratch, &bytes);
            let (_, err) = scan(&scratch, &[]);
            if let Some(e) = err {
                assert_expected_error(&e);
            }
        }
    }
}

#[test]
fn zeroed_files_and_zero_headers_do_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.wal");

    // All-zero files of various sizes: only the (zeroed) header can match, so
    // these must be rejected as NotAWalworm, never hang.
    for len in [0usize, 16, 4096, 64 * 1024] {
        write_file(&scratch, &vec![0u8; len]);
        let (_, err) = scan(&scratch, &[]);
        assert!(err.is_some());
    }

    // Valid header followed by zeros: for a non-Direct-I/O file a zero header
    // is corruption, not padding, so the reader must stop with an error.
    fs::remove_file(&scratch).unwrap();
    let mut bytes = {
        let mut w = Writer::create(&scratch, WriterOptions::default()).unwrap();
        w.write(b"x").unwrap();
        w.close().unwrap();
        drop(w);
        fs::read(&scratch).unwrap()
    };
    bytes.truncate(16); // header only
    bytes.extend_from_slice(&[0u8; 4096]);
    write_file(&scratch, &bytes);
    let (_, err) = scan(&scratch, &[]);
    assert!(matches!(err, Some(Error::Corruption { .. })));
}

#[test]
fn reader_can_be_reused_after_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.wal");
    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"first").unwrap();
    w.write(b"second").unwrap();
    w.close().unwrap();
    drop(w);

    // Corrupt the second record's payload, observe the error, then repair the
    // file and check the same reader instance recovers (the cursor was not
    // advanced). Record 1 is a 5-byte header + 5-byte payload + 4-byte trailer,
    // so the second record starts at 16 + 14 and its payload at +5.
    let full = fs::read(&path).unwrap();
    let mut broken = full.clone();
    broken[16 + (9 + 5) + 5] ^= 0xFF; // second record's payload
    write_file(&path, &broken);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"first");
    assert!(matches!(r.next(), Some(Err(Error::Corruption { .. }))));
    assert_eq!(r.position(), 16 + 9 + 5);

    write_file(&path, &full);
    assert_eq!(r.next().unwrap().unwrap(), b"second");
    assert!(r.next().is_none());
}
