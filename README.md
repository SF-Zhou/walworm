# walworm

*A WAL worm that waits for the tail to grow.*

walworm is a single-file write-ahead log (WAL) for the classic log-tailing
pattern: **one writer** appends records while **any number of readers**
follow along, in the same process or in separate processes.

## Features

- **Single file, one writer, many readers.** The writer creates the file
  exclusively (`O_EXCL`, so reopening an existing WAL is rejected); any
  number of readers may open it at any time, including while it grows.
- **Opaque byte records.** Write any `&[u8]`; records are append-only, with
  no API to delete or replace them.
- **Atomic batches.** `write_batch` stores several records as one physical
  record verified with a single CRC32C, so readers see the whole batch or
  none of it.
- **Optional buffered writes.** Accumulate records in memory for throughput;
  `flush()` is the only durability entry point. `close()` also syncs, since
  it must make the end-of-file marker durable.
- **Optional Direct I/O** (Linux `O_DIRECT`) for writer and reader, with all
  alignment handled internally. It removes page-cache buffering but is
  **not** a durability guarantee: `flush()` and `close()` issue `fdatasync`
  in every mode.
- **CRC32C integrity** on every physical record via the hardware-accelerated
  [`crc-fast`](https://crates.io/crates/crc-fast) crate. Readers stop at
  incomplete or invalid records and never skip past them.
- **Checkpointable reads.** [`Reader::open_at`] resumes at a boundary from
  [`Reader::position`]; [`Reader::next_record_into`] and
  [`Reader::next_record_borrowed`] avoid a heap allocation per record.

## Quick start

Add `walworm = "0.1"` to your `Cargo.toml`, then:

```rust
use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions};

fn main() -> walworm::Result<()> {
    let path = "example.wal";

    // Exactly one writer per file; this fails if the file already exists.
    let mut writer = Writer::create(path, WriterOptions::default())?;
    writer.write(b"hello")?;
    writer.write_batch([b"atom".as_slice(), b"icity".as_slice()])?;
    // Durability barrier: spills buffered data (if any) and fdatasyncs.
    writer.flush()?;

    // Any number of readers.
    let mut reader = Reader::open(path, ReaderOptions::default())?;
    loop {
        match reader.next() {
            Some(Ok(record)) => println!("got {} bytes", record.len()),
            Some(Err(Error::Incomplete)) => break, // not fully visible yet
            Some(Err(e)) => return Err(e),
            None => break, // Writer::close() wrote the EOF marker
        }
    }

    writer.close()?; // appends the EOF marker and makes it durable
    Ok(())
}
```

## Reading a live log and checkpointing

A reader is an iterator; when the next record is not yet fully visible it
yields `Error::Incomplete` without advancing its cursor. `Reader::position()`
returns the offset of the next physical record and is a valid checkpoint for
[`Reader::open_at`] — checkpoint between batches, because `position()`
advances past a whole batch when its first item is yielded.
After reaching EOF, the checkpoint stays at the EOF marker, so reopening
there also finishes iteration.

```rust
use walworm::{Error, Reader, ReaderOptions};

fn resume(path: &str) -> walworm::Result<()> {
    let mut reader = Reader::open(path, ReaderOptions::default())?;

    // ... consume records ...

    let checkpoint = reader.position();

    // Resume exactly there instead of replaying the log.
    let mut resumed = Reader::open_at(path, ReaderOptions::default(), checkpoint)?;

    // Reuse one caller-owned buffer: no allocation per record.
    let mut buf = Vec::new();
    while let Some(result) = resumed.next_record_into(&mut buf) {
        match result {
            Ok(len) => println!("got {len} bytes"), // `buf` holds the record
            Err(Error::Incomplete) => break,        // caught up with the writer
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
```

## Reader semantics

| Situation | Reader sees |
| --- | --- |
| Record fully written and verified | `Some(Ok(record))` |
| Record not yet fully visible / zero tail padding | `Some(Err(Error::Incomplete))`, cursor unchanged |
| CRC32C mismatch or damaged layout | `Some(Err(Error::Corruption { .. }))`, cursor unchanged |
| Length exceeds `ReaderOptions::max_record_size` | `Some(Err(Error::RecordTooLarge { .. }))`, before CRC verification |
| Reader reaches the EOF marker | `None` (the reader is a `FusedIterator`) |

`Reader::open` fails with `Error::NotAWalworm` for non-walworm files or a
damaged header, and `Error::UnsupportedVersion` for an incompatible format
version. While tailing a live Direct I/O log, `Corruption` may be transient.

`Writer::logical_position()` includes records buffered only in memory.
Use [`Writer::durable_position`] for the end confirmed by the last successful
sync when recovering after an I/O error. I/O errors from `write`, `write_batch`
or `flush` poison the writer; a failed `close` remains retryable and rejects
new writes. Its EOF marker may already be visible before the sync succeeds.

## Record size limits

- A single physical record payload is limited to [`MAX_RECORD_SIZE`] (1 GiB)
  by default on both sides (`WriterOptions::max_record_size` and
  `ReaderOptions::max_record_size`). The format allows up to `u32::MAX`, so
  raise the limit on the writer **and** on every reader to exceed it.
- Exceeding either limit is `Error::RecordTooLarge { len, max }`. The reader
  checks the length before verifying CRC32C, so this can indicate either a
  legitimate large record or a damaged length field.
- A reader buffers a whole physical record; allocation growth and read-ahead
  can require more than its payload size. Batch items are parsed lazily with
  constant bookkeeping space in addition to the buffered batch payload.

## Platform support

- Buffered and unbuffered (non-Direct-I/O) operation builds on Unix **and**
  Windows; positioned reads/writes use `seek_read`/`seek_write` on Windows.
  Parent-directory syncing is Unix-only and a no-op elsewhere.
- Direct I/O requires Linux (`O_DIRECT`) and a filesystem that supports it
  (ext4, xfs, btrfs, ...; notably **not** tmpfs). Requesting it on another
  platform returns `Error::Unsupported`.
- Do not mix Direct I/O and page-cache access to the same live file (per
  `open(2)`, coherency is unspecified); reading it back later is fine.
- **Performance trap:** `direct_io(true)` without `buffered(true)` rewrites
  the whole aligned tail block per record — about **57× write amplification**
  for 64-byte records. Prefer `buffered(true)` unless records are ≥ one block.
- The Direct I/O durability model assumes tail-block rewrites preserve
  previously committed bytes in that block.

## Documentation

[The docs.rs API documentation](https://docs.rs/walworm) is authoritative
for per-method, option and error semantics, the on-disk format and crash
recovery. This README is only an overview.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

[`Reader::open_at`]: https://docs.rs/walworm/latest/walworm/struct.Reader.html#method.open_at
[`Reader::position`]: https://docs.rs/walworm/latest/walworm/struct.Reader.html#method.position
[`Reader::next_record_into`]: https://docs.rs/walworm/latest/walworm/struct.Reader.html#method.next_record_into
[`Reader::next_record_borrowed`]: https://docs.rs/walworm/latest/walworm/struct.Reader.html#method.next_record_borrowed
[`Writer::durable_position`]: https://docs.rs/walworm/latest/walworm/struct.Writer.html#method.durable_position
[`MAX_RECORD_SIZE`]: https://docs.rs/walworm/latest/walworm/constant.MAX_RECORD_SIZE.html
