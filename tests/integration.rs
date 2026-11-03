//! Integration tests for walworm, covering the public contract end to end.
//!
//! These tests exercise the crate only through its public API. For files that
//! must contain a specific malformed-but-checksummed record, the bytes are
//! crafted here with hard-coded CRC32C values (the format is documented in the
//! crate root).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions, MAX_RECORD_SIZE};

mod common;

use common::{direct_io_supported, Rng};

fn wal_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("test.wal")
}

/// Drains visible records, asserting they match `model[*idx..]`. Returns
/// true when the log is closed (iterator returned `None`).
fn drain(reader: &mut Reader, model: &[Vec<u8>], idx: &mut usize) -> bool {
    loop {
        match reader.next() {
            Some(Ok(rec)) => {
                assert_eq!(&rec, &model[*idx], "record #{idx} mismatch");
                *idx += 1;
            }
            Some(Err(Error::Incomplete)) => return false,
            Some(Err(e)) => panic!("unexpected read error: {e}"),
            None => return true,
        }
    }
}

fn collect_all(reader: &mut Reader) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for item in reader.by_ref() {
        out.push(item.expect("record should verify"));
    }
    out
}

/// Creates an empty WAL file that contains only the 16-byte file header.
fn header_only_file(path: &Path) {
    let writer = Writer::create(path, WriterOptions::default()).expect("create header file");
    drop(writer); // no close(): leaves just the header
    assert_eq!(fs::metadata(path).unwrap().len(), 16);
}

/// Appends raw bytes to an existing file.
fn append_raw(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

// CRC32C of `len || type` for a zero-length record, per record type. Used to
// craft records that pass verification but are semantically invalid.
const CRC_LEN0_BATCH: [u8; 4] = [0xC2, 0x06, 0x49, 0xA4];
const CRC_LEN0_UNKNOWN7: [u8; 4] = [0xDE, 0x12, 0xB8, 0x91];

/// Builds a zero-length-payload record (`header || trailer`) with a valid CRC.
///
/// Layout (format v2): `len: u32le | type: u8 | crc32c: u32le`; the CRC covers
/// the two header bytes and the (empty) payload.
fn zero_len_record(record_type: u8, crc: [u8; 4]) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0..4].copy_from_slice(&0u32.to_le_bytes());
    out[4] = record_type;
    out[5..9].copy_from_slice(&crc);
    out
}

// ---------------------------------------------------------------------------
// Round-trip and basic contract
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_unbuffered() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let records: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        b"hello world".to_vec(),
        vec![0xab; 100_000], // larger than the reader chunk
        vec![0x00; 4096],    // all-zero payload must not look like padding
        b"tail".to_vec(),
    ];

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    for r in &records {
        w.write(r).unwrap();
    }
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(collect_all(&mut r), records);
    assert!(r.is_at_eof());
    // Once closed, iteration stays finished (FusedIterator).
    assert!(r.next().is_none());
    assert!(r.next().is_none());
}

#[test]
fn incomplete_record_does_not_advance_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"first").unwrap();
    w.write_batch([b"b1".as_slice(), b"b2".as_slice()]).unwrap();
    w.close().unwrap();
    drop(w);

    // Saw the batch record in half: the reader must yield "first", then
    // report Incomplete without yielding any batch item and without moving.
    let full = fs::read(&path).unwrap();
    let batch_start = 16 + (9 + 5); // header + record("first")
    let cut = batch_start + 5 + 3; // batch header + part of its payload
    {
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(cut as u64).unwrap();
    }

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"first");
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
    assert_eq!(r.position(), batch_start as u64);
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
    assert_eq!(r.position(), batch_start as u64);

    // Restore the file; the same reader instance must now make progress.
    fs::write(&path, &full).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"b1");
    assert_eq!(r.next().unwrap().unwrap(), b"b2");
    assert!(r.next().is_none());
}

#[test]
fn corruption_is_reported_and_cursor_stays() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"good").unwrap();
    w.write(b"damaged").unwrap();
    w.close().unwrap();
    drop(w);

    // Flip one bit in the payload of the first record.
    let mut bytes = fs::read(&path).unwrap();
    bytes[16 + 5] ^= 0x01;
    fs::write(&path, &bytes).unwrap();

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    match r.next() {
        Some(Err(Error::Corruption { offset })) => assert_eq!(offset, 16),
        other => panic!("expected corruption, got {other:?}"),
    }
    assert_eq!(r.position(), 16);
    // Stable corruption is reported again on retry.
    assert!(matches!(r.next(), Some(Err(Error::Corruption { .. }))));
    assert_eq!(r.position(), 16);
}

#[test]
fn multiple_readers_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    for i in 0..50u32 {
        w.write(format!("record-{i}").as_bytes()).unwrap();
    }
    w.close().unwrap();
    drop(w);

    let mut r1 = Reader::open(&path, ReaderOptions::default()).unwrap();
    let mut r2 = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r1.next().unwrap().unwrap(), b"record-0");
    let all1 = collect_all(&mut r1);
    assert_eq!(all1.len(), 49);
    // r2 is unaffected by r1's progress.
    let all2 = collect_all(&mut r2);
    assert_eq!(all2.len(), 50);
    assert_eq!(all2[0], b"record-0");
}

#[test]
fn live_tail_with_buffered_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::new().with_buffered(true)).unwrap();
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();

    w.write(b"pending").unwrap();
    // Buffered: not yet written to the file.
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

    w.flush().unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"pending");

    w.write_batch([b"x".as_slice(), b"y".as_slice()]).unwrap();
    w.flush().unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"x");
    assert_eq!(r.next().unwrap().unwrap(), b"y");
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

    w.close().unwrap();
    assert!(r.next().is_none());
}

#[test]
fn dropped_writer_leaves_visible_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::new().with_buffered(true)).unwrap();
    w.write(b"visible after drop").unwrap();
    drop(w); // no close: Drop flushes the buffer best-effort

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"visible after drop");
    // No EOF marker: the reader must report Incomplete, not None.
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
}

#[test]
fn concurrent_live_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::new().with_buffered(true)).unwrap();
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();

    let (tx, rx) = mpsc::channel();
    let writer_thread = thread::spawn(move || {
        for i in 0..200u32 {
            if i % 7 == 0 {
                w.write_batch([
                    format!("batch-{i}-a").into_bytes(),
                    format!("batch-{i}-b").into_bytes(),
                ])
                .unwrap();
            } else {
                w.write(format!("rec-{i}").as_bytes()).unwrap();
            }
            if i % 10 == 0 {
                w.flush().unwrap();
            }
            tx.send(i).unwrap();
        }
        w.flush().unwrap();
        w.close().unwrap();
    });

    let mut received = Vec::new();
    loop {
        match r.next() {
            Some(Ok(rec)) => received.push(rec),
            Some(Err(Error::Incomplete)) => {
                let _ = rx.recv_timeout(Duration::from_millis(1));
                thread::yield_now();
            }
            Some(Err(e)) => panic!("unexpected error: {e}"),
            None => break,
        }
    }
    writer_thread.join().unwrap();

    let mut expected: Vec<Vec<u8>> = Vec::new();
    for i in 0..200u32 {
        if i % 7 == 0 {
            expected.push(format!("batch-{i}-a").into_bytes());
            expected.push(format!("batch-{i}-b").into_bytes());
        } else {
            expected.push(format!("rec-{i}").into_bytes());
        }
    }
    assert_eq!(received, expected);
}

// ---------------------------------------------------------------------------
// Direct I/O
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
#[should_panic(expected = "Direct I/O probe failed:")]
fn unexpected_direct_io_probe_error_fails_the_test() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".direct-io-probe.wal")).unwrap();
    direct_io_supported(&dir);
}

#[test]
fn direct_io_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    if !direct_io_supported(&dir) {
        eprintln!("skipping: direct I/O not supported on this filesystem");
        return;
    }
    let path = wal_path(&dir);
    let opts = WriterOptions::new().with_direct_io(true);

    let mut rng = Rng(0x1234_5678_9abc_def0);
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut w = Writer::create(&path, opts).unwrap();
    for _ in 0..100 {
        let len = rng.below(20_000);
        let rec = rng.bytes(len);
        w.write(&rec).unwrap();
        expected.push(rec);
    }
    w.write_batch([b"direct".as_slice(), b"batch".as_slice()])
        .unwrap();
    expected.push(b"direct".to_vec());
    expected.push(b"batch".to_vec());
    w.close().unwrap();
    drop(w);

    // Buffered reader on a Direct-I/O-written file (mixed mode).
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(collect_all(&mut r), expected);

    // Direct I/O reader as well.
    let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();
    assert_eq!(collect_all(&mut r), expected);

    // Zero-copy APIs must agree with the owned iterator.
    let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();
    let mut borrowed = Vec::new();
    while let Some(item) = r.next_record_borrowed() {
        borrowed.push(item.unwrap().to_vec());
    }
    assert_eq!(borrowed, expected);
}

#[test]
fn direct_io_padding_is_transparent_for_live_tail() {
    let dir = tempfile::tempdir().unwrap();
    if !direct_io_supported(&dir) {
        eprintln!("skipping: direct I/O not supported on this filesystem");
        return;
    }
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap();
    let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();

    w.write(b"one").unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"one");
    // Reader now sits on the zero padding of the tail block.
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

    // The next write rewrites the padded block; the reader must recover.
    w.write(b"two").unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"two");

    w.write_batch([b"three".as_slice(), b"four".as_slice()])
        .unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"three");
    assert_eq!(r.next().unwrap().unwrap(), b"four");
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));

    w.close().unwrap();
    assert!(r.next().is_none());
}

#[test]
fn direct_io_buffered_writer_flush_semantics() {
    let dir = tempfile::tempdir().unwrap();
    if !direct_io_supported(&dir) {
        eprintln!("skipping: direct I/O not supported on this filesystem");
        return;
    }
    let path = wal_path(&dir);

    let mut w = Writer::create(
        &path,
        WriterOptions::new()
            .with_direct_io(true)
            .with_buffered(true),
    )
    .unwrap();
    let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(true)).unwrap();

    w.write(b"buffered-direct").unwrap();
    assert!(matches!(r.next(), Some(Err(Error::Incomplete))));
    w.flush().unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"buffered-direct");
    w.close().unwrap();
    assert!(r.next().is_none());
}

#[test]
fn buffered_writer_spills_at_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(
        &path,
        WriterOptions::new()
            .with_buffered(true)
            .with_buffer_capacity(1024),
    )
    .unwrap();
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();

    let big = vec![7u8; 5000];
    w.write(&big).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), big);
    w.close().unwrap();
    assert!(r.next().is_none());
}

// ---------------------------------------------------------------------------
// C4/C5-style error-path state (the unit tests in writer.rs cover poisoning;
// these cover the observable position()/recovery contract)
// ---------------------------------------------------------------------------

#[test]
fn writer_position_is_logical_end_after_successful_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    assert_eq!(w.logical_position(), 16);
    w.write(&[1u8; 10]).unwrap();
    assert_eq!(w.logical_position(), 16 + 9 + 10);
    w.write_batch([b"ab".as_slice(), b"c".as_slice()]).unwrap();
    assert_eq!(w.logical_position(), 16 + 9 + 10 + 9 + (4 + 2) + (4 + 1));
    w.flush().unwrap();
    w.close().unwrap();
    assert!(w.is_closed());
}

// ---------------------------------------------------------------------------
// C2: a failed write_batch must leave the stream intact
// ---------------------------------------------------------------------------

#[test]
fn failed_write_batch_keeps_log_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    // A tiny configured limit makes the rollback path cheap to exercise.
    let mut w = Writer::create(&path, WriterOptions::new().with_max_record_size(64)).unwrap();
    w.write(b"good-1").unwrap();
    let pos_before = w.logical_position();

    let big = vec![0xAAu8; 100];
    let err = w.write_batch([big.as_slice()]).unwrap_err();
    assert!(
        matches!(err, Error::RecordTooLarge { .. }),
        "unexpected error: {err}"
    );
    // The failed batch must not have advanced the writer.
    assert_eq!(w.logical_position(), pos_before);

    // The writer must still be usable, and the log must contain both records.
    w.write(b"good-2").unwrap();
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"good-1");
    assert_eq!(r.next().unwrap().unwrap(), b"good-2");
    assert!(r.next().is_none());
}

#[test]
fn oversized_single_record_is_rejected_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let mut w = Writer::create(&path, WriterOptions::new().with_max_record_size(16)).unwrap();
    let err = w.write(&[0u8; 17]).unwrap_err();
    assert!(matches!(err, Error::RecordTooLarge { len: 17, max: 16 }));
    // A record at the limit is accepted.
    w.write(&[0u8; 16]).unwrap();
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), vec![0u8; 16]);
    assert!(r.next().is_none());
}

// ---------------------------------------------------------------------------
// C3: zeroed record headers are corruption, except for Direct I/O padding
// ---------------------------------------------------------------------------

#[test]
fn zeroed_header_in_plain_file_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"first").unwrap();
    w.write(b"second").unwrap();
    w.close().unwrap();
    drop(w);

    // Zero out the second record header (offset 16 + 9 + 5).
    let second = 16 + 9 + 5;
    let mut bytes = fs::read(&path).unwrap();
    for b in &mut bytes[second..second + 9] {
        *b = 0;
    }
    fs::write(&path, &bytes).unwrap();

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"first");
    match r.next() {
        Some(Err(Error::Corruption { offset })) => assert_eq!(offset, second as u64),
        other => panic!("expected Corruption for a zeroed header, got {other:?}"),
    }
    // The cursor must not have advanced.
    assert_eq!(r.position(), second as u64);
    // Repeated calls are stable (no infinite "Incomplete" masking).
    assert!(matches!(r.next(), Some(Err(Error::Corruption { .. }))));
}

#[test]
fn zeroed_header_with_data_after_it_is_corruption_in_direct_file() {
    let dir = tempfile::tempdir().unwrap();
    if !direct_io_supported(&dir) {
        eprintln!("skipping: direct I/O not supported on this filesystem");
        return;
    }
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::new().with_direct_io(true)).unwrap();
    w.write(b"first").unwrap();
    w.write(b"second").unwrap();
    w.close().unwrap();
    drop(w);

    let second = 16 + 9 + 5;
    let mut bytes = fs::read(&path).unwrap();
    for b in &mut bytes[second..second + 9] {
        *b = 0;
    }
    fs::write(&path, &bytes).unwrap();

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"first");
    assert!(
        matches!(r.next(), Some(Err(Error::Corruption { .. }))),
        "zeroed header followed by valid data must be corruption, not padding"
    );
}

#[test]
fn zero_item_batch_record_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    header_only_file(&path);
    append_raw(&path, &zero_len_record(2, CRC_LEN0_BATCH));

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    match r.next() {
        Some(Err(Error::Corruption { offset })) => assert_eq!(offset, 16),
        other => panic!("expected Corruption for an empty batch, got {other:?}"),
    }
    assert_eq!(r.position(), 16);
}

#[test]
fn unknown_record_type_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    header_only_file(&path);
    append_raw(&path, &zero_len_record(7, CRC_LEN0_UNKNOWN7));

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(
        r.next().unwrap().unwrap_err(),
        Error::UnknownRecordType { offset: 16, tag: 7 }
    );
}

// ---------------------------------------------------------------------------
// C7: size limits are reported as RecordTooLarge, not Corruption
// ---------------------------------------------------------------------------

#[test]
fn reader_size_limit_is_record_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(&vec![1u8; 2048]).unwrap();
    // A second, smaller record proves the limit error does not imply that the
    // rest of the file is damaged.
    w.write(b"fine").unwrap();
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::new().with_max_record_size(1024)).unwrap();
    assert_eq!(
        r.next().unwrap().unwrap_err(),
        Error::RecordTooLarge {
            len: 2048,
            max: 1024
        }
    );
    assert_eq!(r.position(), 16);

    // With a sufficient limit the same file reads fine.
    let mut r = Reader::open(&path, ReaderOptions::new().with_max_record_size(4096)).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), vec![1u8; 2048]);
    assert_eq!(r.next().unwrap().unwrap(), b"fine");
    assert!(r.next().is_none());
}

#[test]
fn default_limits_are_consistent() {
    // Both sides default to the same documented maximum.
    assert_eq!(MAX_RECORD_SIZE, 1 << 30);
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"x").unwrap();
    w.close().unwrap();
    drop(w);
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"x");
}

// ---------------------------------------------------------------------------
// A1: resume at a position checkpoint
// ---------------------------------------------------------------------------

#[test]
fn open_at_resumes_from_position() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    for i in 0..100u32 {
        w.write(format!("record-{i}").as_bytes()).unwrap();
    }
    w.close().unwrap();
    drop(w);

    // Find the offset of record 50 by scanning.
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    for i in 0..50u32 {
        assert_eq!(r.next().unwrap().unwrap(), format!("record-{i}").as_bytes());
    }
    let checkpoint = r.position();

    // A new reader starting at the checkpoint sees exactly records 50..100.
    let mut resumed = Reader::open_at(&path, ReaderOptions::default(), checkpoint).unwrap();
    assert_eq!(resumed.position(), checkpoint);
    for i in 50..100u32 {
        assert_eq!(
            resumed.next().unwrap().unwrap(),
            format!("record-{i}").as_bytes()
        );
    }
    assert!(resumed.next().is_none());
}

#[test]
fn open_at_rejects_implausible_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"x").unwrap();
    w.close().unwrap();
    drop(w);

    let err = Reader::open_at(&path, ReaderOptions::default(), 0).unwrap_err();
    assert!(matches!(err, Error::InvalidOptions(_)));
}

// ---------------------------------------------------------------------------
// P2: zero-copy read APIs
// ---------------------------------------------------------------------------

#[test]
fn read_into_reuses_the_same_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    for i in 0..1000u32 {
        w.write(format!("record-{i}").as_bytes()).unwrap();
    }
    w.write_batch([b"b1".as_slice(), b"b2".as_slice()]).unwrap();
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    let mut buf = Vec::new();
    let mut seen = 0u32;
    loop {
        match r.next_record_into(&mut buf) {
            Some(Ok(len)) => {
                assert_eq!(len, buf.len());
                let expected = match seen {
                    0..=999 => format!("record-{seen}"),
                    1000 => "b1".to_string(),
                    _ => "b2".to_string(),
                };
                assert_eq!(buf, expected.as_bytes());
                seen += 1;
            }
            Some(Err(e)) => panic!("unexpected error: {e}"),
            None => break,
        }
    }
    assert_eq!(seen, 1002);
    // The caller-owned buffer ends up with a single reusable allocation.
    assert!(buf.capacity() >= 10);
}

#[test]
fn borrowed_records_match_owned_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"a").unwrap();
    w.write_batch([b"b1".as_slice(), b"b2".as_slice()]).unwrap();
    w.write(b"c").unwrap();
    w.close().unwrap();
    drop(w);

    let expected: Vec<Vec<u8>> = vec![b"a".to_vec(), b"b1".to_vec(), b"b2".to_vec(), b"c".to_vec()];

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    let mut got = Vec::new();
    while let Some(item) = r.next_record_borrowed() {
        got.push(item.unwrap().to_vec());
    }
    assert_eq!(got, expected);
}

#[test]
fn borrowed_slices_survive_parse_buffer_growth() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    for i in 0..500u32 {
        w.write(format!("record-{i}").as_bytes()).unwrap();
    }
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    for i in 0..500u32 {
        let got = r.next_record_borrowed().unwrap().unwrap().to_vec();
        assert_eq!(got, format!("record-{i}").as_bytes());
    }
    assert!(r.next().is_none());
}

// ---------------------------------------------------------------------------
// C9/C11: batches are parsed lazily and stay bounded
// ---------------------------------------------------------------------------

#[test]
fn large_batch_is_yielded_lazily() {
    const ITEMS: usize = 200_000;
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let empty: &[u8] = b"";

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write_batch(std::iter::repeat_n(empty, ITEMS)).unwrap();
    w.close().unwrap();
    drop(w);

    // 200k zero-length items are 800 KiB on disk; the reader must not build a
    // per-item Vec (that would be megabytes of overhead and, at larger counts,
    // an OOM abort).
    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    let mut buf = Vec::new();
    let mut n = 0;
    while let Some(item) = r.next_record_into(&mut buf) {
        assert_eq!(item.unwrap(), 0);
        n += 1;
    }
    assert_eq!(n, ITEMS);
}

#[test]
fn batch_position_advances_past_the_whole_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"a").unwrap();
    w.write_batch([b"b1".as_slice(), b"b2".as_slice()]).unwrap();
    w.close().unwrap();
    drop(w);

    let mut r = Reader::open(&path, ReaderOptions::default()).unwrap();
    assert_eq!(r.next().unwrap().unwrap(), b"a");
    let pos_after_a = r.position();
    assert_eq!(r.next().unwrap().unwrap(), b"b1");
    let pos_after_b1 = r.position();
    // The batch is one physical record: position() advances past all of it
    // when its first item is yielded (documented behavior).
    assert!(pos_after_b1 > pos_after_a);
    assert_eq!(r.next().unwrap().unwrap(), b"b2");
    assert_eq!(r.position(), pos_after_b1);
    assert!(r.next().is_none());
}

// ---------------------------------------------------------------------------
// Model-based fuzz across all writer modes
// ---------------------------------------------------------------------------

#[test]
fn fuzz_against_model_all_modes() {
    for &(direct, buffered) in &[(false, false), (false, true), (true, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        if direct && !direct_io_supported(&dir) {
            eprintln!("skipping direct mode: not supported");
            continue;
        }
        let path = wal_path(&dir);
        let opts = WriterOptions::new()
            .with_direct_io(direct)
            .with_buffered(buffered)
            .with_buffer_capacity(4096);

        let mut rng = Rng(0xdead_beef_cafe_f00d ^ (u64::from(direct) << 8) ^ u64::from(buffered));
        let mut model: Vec<Vec<u8>> = Vec::new();
        let mut w = Writer::create(&path, opts).unwrap();
        let mut r = Reader::open(&path, ReaderOptions::new().with_direct_io(direct)).unwrap();
        let mut read_idx = 0usize;

        for _ in 0..300 {
            match rng.below(10) {
                0..=5 => {
                    let len = rng.below(6000);
                    let rec = rng.bytes(len);
                    w.write(&rec).unwrap();
                    model.push(rec);
                }
                6..=7 => {
                    let n = 1 + rng.below(4);
                    let batch: Vec<Vec<u8>> = (0..n)
                        .map(|_| {
                            let len = rng.below(1500);
                            rng.bytes(len)
                        })
                        .collect();
                    w.write_batch(&batch).unwrap();
                    model.extend(batch);
                }
                _ => w.flush().unwrap(),
            }
            // Drain whatever is visible; must match the model prefix.
            assert!(!drain(&mut r, &model, &mut read_idx), "closed early");
        }
        w.close().unwrap();
        assert!(drain(&mut r, &model, &mut read_idx), "must reach EOF");
        assert_eq!(read_idx, model.len(), "mode ({direct}, {buffered})");
    }
}

// ---------------------------------------------------------------------------
// Options and header validation
// ---------------------------------------------------------------------------

#[test]
fn invalid_options_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let err = Writer::create(
        &path,
        WriterOptions::new()
            .with_direct_io(true)
            .with_block_size(1000),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidOptions(_)));
    assert!(!path.exists(), "validation must not leave a file behind");

    let err = Writer::create(
        &path,
        WriterOptions::new()
            .with_buffered(true)
            .with_buffer_capacity(0),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidOptions(_)));

    let err = Writer::create(&path, WriterOptions::new().with_max_record_size(0)).unwrap_err();
    assert!(matches!(err, Error::InvalidOptions(_)));

    let err = Reader::open(&path, ReaderOptions::new().with_max_record_size(0)).unwrap_err();
    assert!(matches!(err, Error::InvalidOptions(_)));
}

/// Golden-bytes test: locks the on-disk record layout.
///
/// Format v2 record: `payload len: u32le | record type: u8 | payload |
/// crc32c: u32le`, where the CRC covers `len || type || payload`. The file
/// header is `"WLWM" | version: u32le (= 2) | block size: u32le | header crc`.
#[test]
fn on_disk_record_layout_is_version_2() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);

    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"hello").unwrap();
    w.close().unwrap();
    drop(w);

    let bytes = fs::read(&path).unwrap();
    // File header.
    assert_eq!(&bytes[0..4], b"WLWM");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 0);

    // DATA record at offset 16: len=5, type=1, payload "hello", then the CRC
    // of `05 00 00 00 01 68 65 6c 6c 6f` (little-endian bytes 0x3b 0xd5 0x58 0x29).
    assert_eq!(&bytes[16..20], &5u32.to_le_bytes());
    assert_eq!(bytes[20], 1, "DATA type tag");
    assert_eq!(&bytes[21..26], b"hello");
    assert_eq!(
        &bytes[26..30],
        &[0x29, 0x58, 0xd5, 0x3b],
        "data CRC trailer"
    );

    // EOF record: len=0, type=3, CRC of `00 00 00 00 03` = 0x562285C1.
    let eof = 16 + 5 + 5 + 4;
    assert_eq!(&bytes[eof..eof + 4], &[0, 0, 0, 0]);
    assert_eq!(bytes[eof + 4], 3, "EOF type tag");
    assert_eq!(&bytes[eof + 5..eof + 9], &[0xc1, 0x85, 0x22, 0x56]);
    assert_eq!(bytes.len(), eof + 9);
}

/// A version-1 file (old CRC-in-header layout) is reported as an unsupported
/// version rather than as corruption, because it uses a different format.
#[test]
fn version_1_files_are_rejected_as_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.wal");
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(b"WLWM");
    header[4..8].copy_from_slice(&1u32.to_le_bytes());
    header[12..16].copy_from_slice(&[0xbc, 0xbb, 0x89, 0x88]);
    fs::write(&path, header).unwrap();
    assert_eq!(
        Reader::open(&path, ReaderOptions::default()).unwrap_err(),
        Error::UnsupportedVersion {
            found: 1,
            supported: 2
        }
    );
}

#[test]
fn bad_header_variants_are_distinguished() {
    let dir = tempfile::tempdir().unwrap();

    // Garbage file.
    let garbage = dir.path().join("garbage.wal");
    fs::write(&garbage, b"definitely not a wal").unwrap();
    assert_eq!(
        Reader::open(&garbage, ReaderOptions::default()).unwrap_err(),
        Error::NotAWalworm
    );

    // Correct magic, unsupported version 3, valid header CRC.
    let version3 = dir.path().join("v3.wal");
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(b"WLWM");
    header[4..8].copy_from_slice(&3u32.to_le_bytes());
    header[12..16].copy_from_slice(&[0xF2, 0x41, 0xF1, 0x1A]); // CRC32C of first 12 bytes
    fs::write(&version3, header).unwrap();
    assert_eq!(
        Reader::open(&version3, ReaderOptions::default()).unwrap_err(),
        Error::UnsupportedVersion {
            found: 3,
            supported: 2
        }
    );

    // Correct magic and version, corrupted header CRC.
    let corrupt = dir.path().join("corrupt.wal");
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(b"WLWM");
    header[4..8].copy_from_slice(&2u32.to_le_bytes());
    fs::write(&corrupt, header).unwrap();
    assert_eq!(
        Reader::open(&corrupt, ReaderOptions::default()).unwrap_err(),
        Error::NotAWalworm
    );
}

#[test]
fn closed_writer_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    let mut w = Writer::create(&path, WriterOptions::default()).unwrap();
    w.write(b"x").unwrap();
    w.close().unwrap();
    assert_eq!(w.write(b"y").unwrap_err(), Error::Closed);
    assert_eq!(w.write_batch([b"y".as_slice()]).unwrap_err(), Error::Closed);
    // Idempotent close and harmless flush.
    w.close().unwrap();
    w.flush().unwrap();
}
