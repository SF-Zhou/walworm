# Refactor record (2026-11-03)

Follow-up to the review in [REVIEW.md](REVIEW.md): the fixes landed first, then the
crate was restructured. Behavior, on-disk format and the public API are unchanged
(the reader and writer refactors were verified with a rustdoc-JSON public-API diff
and a byte-identical differential harness, respectively).

## What changed

| Area | Before | After |
|---|---|---|
| Reader | one 832-line production struct mixing parsing, buffer window, batch cursor, padding policy and I/O | `record.rs` (pure decoding/CRC/padding classification), `parse_buffer.rs` (`ParseBuffer` owns data/from/cursor + compaction), `reader.rs` orchestration |
| Writer | one 746-line struct mixing create/durability, buffering policy, Direct-I/O alignment, state machine and test hooks | `tail.rs` (`PendingTail` owns the pending bytes/offsets and the two-phase spill), `writer.rs` orchestration + lifecycle |
| Record encoding | header patch + CRC in three places (`write`, `write_batch`, EOF) | one `PendingTail::begin_record`/`finish_record` path |
| flush/close | two copies of write_pending+dirty+sync | one `Writer::sync_pending` |
| Offset math | 8 scattered `(abs - pbuf_from) as usize` conversions | one `ParseBuffer::index` |
| Platform I/O | duplicated unix/windows `write_all_at`/`read_at` loops + stubs | `raw_write_at`/`raw_read_at` adapters + one platform-independent loop |
| Direct-I/O probe | 5 copies across unit/integration/robustness/bench | `tests/common/mod.rs` (separate test crates) and `src/test_support.rs` (`cfg(test)`) |
| `unsafe` | only in `aligned.rs` by convention | `#![deny(unsafe_code)]` + one `#[allow]` on that module |
| Validation | `max_record_size` checked twice | `format::validate_max_record_size` |
| Naming | `Reader::is_closed`, `Writer::position` | `Reader::is_at_eof`, `Writer::logical_position` |
| Docs | README duplicated rustdoc (~276 lines) | README overview (~169 lines), details live in rustdoc |
| Review report | crate root | `docs/` (excluded from the package) |

## Size / test accounting

- Production + test lines are roughly neutral (`src/` 4440 → 4433 lines): code was
  moved into focused modules, not deleted; the new modules carry their own docs
  and unit tests.
- Unit tests: 47 → 70 (new `record`, `parse_buffer` and `tail` invariant tests).
- Integration (31), robustness (6) and doctests (2) unchanged and green.
- Public API: empty diff (verified via nightly rustdoc JSON).
- On-disk format: unchanged (writer differential harness produced byte-identical
  files for buffered/unbuffered/direct/direct+buffered/error/drop scenarios).

## Consciously not done

- Removing `Reader::next_record` (the owned convenience path over `Iterator` and
  `next_record_into`/`next_record_borrowed`): the duplication is three lines of
  delegation, and removing a public method is a breaking change with no
  simplification payoff.
- A generic I/O trait to make parsing testable: `record.rs` is already pure, so the
  trait would only add indirection.

## Follow-up: on-disk format v2

Since the crate is not released yet, the on-disk format was changed afterwards
without a compatibility path (see the commit that bumps `FORMAT_VERSION` to 2):

- Record layout is now `payload len: u32le | record type: u8 | payload |
  crc32c: u32le`. The checksum moved from the header to a trailer, so it is
  one contiguous `crc32c(header || payload)` call instead of an incremental
  hash (measured earlier: 6.84 ns vs 13.28 ns for a 64-byte record).
- Record type `0` is reserved so a legitimate header can never be all-zero
  (that all-zero shape is what identifies Direct I/O tail padding), hence the
  tags became `1` = data, `2` = batch, `3` = EOF.
- Record totals are unchanged (`5 + len + 4 == 9 + len`), so pagination,
  offsets and Direct I/O block math stay the same.
- Version-1 files are reported as `Error::UnsupportedVersion`, and a
  golden-bytes integration test locks the new layout.
