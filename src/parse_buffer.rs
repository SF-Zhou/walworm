//! A buffered window of WAL file bytes plus the reader's parse cursor.
//!
//! [`ParseBuffer`] owns the three pieces of reader parse state that used to be
//! separate fields of [`crate::Reader`]: the buffered bytes (`buf`), the
//! absolute file offset of the first buffered byte (`from`) and the absolute
//! offset of the next byte to parse (`cursor`).
//!
//! Readers address records by absolute file offset; this type is the single
//! place that converts those offsets into indices into `buf` ([`ParseBuffer::index`]).
//! The cursor is only advanced after a record has been fully verified, and
//! compaction of the already-consumed prefix is deferred so that a zero-copy
//! borrow of `buf` (or a lazily yielded batch) may keep referencing consumed
//! bytes.

use std::io;

/// A contiguous window of buffered file bytes plus the absolute parse cursor.
pub(crate) struct ParseBuffer {
    /// Buffered bytes; `buf[0]` is at absolute file offset `from`.
    buf: Vec<u8>,
    /// Absolute file offset of `buf[0]`.
    from: u64,
    /// Absolute file offset of the next byte to parse. It never moves
    /// backwards and never moves past [`Self::end`].
    cursor: u64,
    /// Set when the consumed prefix has grown large enough to justify
    /// compaction; applied by [`Self::consume_to`] at a safe later point.
    compact_pending: bool,
}

impl ParseBuffer {
    /// Creates an empty buffer whose first appended byte will sit at `from`.
    #[must_use]
    pub(crate) fn new(from: u64) -> Self {
        Self {
            buf: Vec::new(),
            from,
            cursor: from,
            compact_pending: false,
        }
    }

    /// Returns the absolute file offset of the first buffered byte.
    #[must_use]
    pub(crate) fn from(&self) -> u64 {
        self.from
    }

    /// Returns the absolute file offset of the next byte to parse.
    #[must_use]
    pub(crate) fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the absolute file offset one past the last buffered byte.
    #[must_use]
    pub(crate) fn end(&self) -> u64 {
        self.from + self.buf.len() as u64
    }

    /// Returns all buffered bytes, including the already-consumed prefix.
    #[must_use]
    pub(crate) fn data(&self) -> &[u8] {
        &self.buf
    }

    /// Returns the retained allocation size for reader regression tests.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Returns the unconsumed buffered bytes (from [`Self::cursor`] onwards).
    #[must_use]
    pub(crate) fn available(&self) -> &[u8] {
        self.available_from(self.cursor)
    }

    /// Returns the buffered bytes from the absolute offset `abs` onwards.
    ///
    /// `abs` must lie in `self.from()..=self.end()`.
    #[must_use]
    pub(crate) fn available_from(&self, abs: u64) -> &[u8] {
        let start = self.index(abs);
        &self.buf[start..]
    }

    /// Converts an absolute file offset into an index into [`Self::data`].
    ///
    /// This is the one and only place where a buffered absolute offset is
    /// turned into a slice index; every other method is written in terms of
    /// absolute offsets.
    #[must_use]
    pub(crate) fn index(&self, abs: u64) -> usize {
        debug_assert!(
            abs >= self.from(),
            "absolute offset {abs} precedes buffered bytes (from {})",
            self.from()
        );
        debug_assert!(
            abs <= self.end(),
            "absolute offset {abs} is past the buffered end ({})",
            self.end()
        );
        (abs - self.from) as usize
    }

    /// Appends freshly read bytes, which must start at [`Self::end`].
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::OutOfMemory`] when the buffer cannot grow.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buf.try_reserve(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "reader parse buffer allocation failed",
            )
        })?;
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Advances the cursor past a fully verified physical record of `len`
    /// bytes, and records whether the consumed prefix is now worth compacting.
    ///
    /// `compact_threshold` is the number of consumed bytes above which
    /// compaction becomes worthwhile; it is a reader policy knob, so it is
    /// passed in rather than baked into this type.
    pub(crate) fn advance(&mut self, len: u64, compact_threshold: usize) {
        debug_assert!(
            len <= self.end() - self.cursor,
            "record of {len} bytes exceeds the buffered bytes left"
        );
        self.cursor += len;
        let used = self.index(self.cursor);
        if used >= compact_threshold && used * 2 >= self.buf.len() {
            self.compact_pending = true;
        }
    }

    /// Drops buffered bytes past `abs`, keeping the consumed prefix.
    ///
    /// The cursor is left where it is; callers pass [`Self::cursor`] and use
    /// this when the visible tail may have been replaced by the writer, so the
    /// next `fill` re-reads the tail bytes from disk.
    pub(crate) fn truncate_to(&mut self, abs: u64) {
        debug_assert!(
            abs >= self.from,
            "cannot truncate before the buffered start"
        );
        let end = self.index(abs);
        debug_assert!(
            end <= self.buf.len(),
            "cannot extend the buffer by truncating"
        );
        self.buf.truncate(end);
    }

    /// Drops the already-consumed prefix, ending at the absolute offset `abs`
    /// (the deferred compaction drain).
    ///
    /// This rebases the buffer so that the next byte to parse becomes
    /// `buf[0]`; the cursor itself does not move.
    pub(crate) fn consume_to(&mut self, abs: u64) {
        debug_assert!(abs >= self.from, "cannot consume before the buffered start");
        debug_assert!(abs <= self.cursor, "cannot consume past the parse cursor");
        let used = self.index(abs);
        self.buf.drain(..used);
        self.from = abs;
        self.compact_pending = false;
    }

    /// Empties the buffer and rebases it at the absolute offset `abs`.
    ///
    /// The allocation is kept (see [`Self::shrink_if_large`] to release it);
    /// used when everything buffered has been consumed.
    pub(crate) fn clear_to(&mut self, abs: u64) {
        debug_assert!(abs >= self.from, "cannot rebase before the buffered start");
        debug_assert!(abs >= self.cursor, "cannot clear before the parse cursor");
        self.buf.clear();
        self.from = abs;
        self.compact_pending = false;
    }

    /// Shrinks an allocation larger than twice both `threshold` and the
    /// remaining data length.
    ///
    /// Empty buffers are released. Nonempty buffers keep at least `threshold`
    /// bytes of capacity so a small unread tail does not pin a large record's
    /// allocation or cause repeated shrinking and growth during live reads.
    pub(crate) fn shrink_if_large(&mut self, threshold: usize) {
        let retained = threshold.max(self.buf.len());
        if self.buf.capacity() > retained.saturating_mul(2) {
            self.buf
                .shrink_to(if self.buf.is_empty() { 0 } else { retained });
        }
    }

    /// Returns whether every buffered byte has been consumed.
    #[must_use]
    pub(crate) fn is_drained(&self) -> bool {
        self.cursor == self.end()
    }

    /// Returns whether deferred compaction of the consumed prefix is pending.
    #[must_use]
    pub(crate) fn compact_pending(&self) -> bool {
        self.compact_pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: usize = 1 << 20;

    #[test]
    fn new_buffer_is_empty_at_its_base() {
        let pbuf = ParseBuffer::new(100);
        assert_eq!(pbuf.from(), 100);
        assert_eq!(pbuf.cursor(), 100);
        assert_eq!(pbuf.end(), 100);
        assert_eq!(pbuf.data(), []);
        assert_eq!(pbuf.available(), []);
        assert!(pbuf.is_drained());
        assert!(!pbuf.compact_pending());
    }

    #[test]
    fn append_builds_a_window_at_absolute_offsets() {
        let mut pbuf = ParseBuffer::new(100);
        pbuf.append(b"abcdef").unwrap();
        assert_eq!(pbuf.from(), 100);
        assert_eq!(pbuf.cursor(), 100);
        assert_eq!(pbuf.end(), 106);
        assert_eq!(pbuf.data(), b"abcdef");
        assert_eq!(pbuf.available(), b"abcdef");
        assert_eq!(pbuf.index(100), 0);
        assert_eq!(pbuf.index(104), 4);
        assert_eq!(pbuf.available_from(102), b"cdef");
        assert_eq!(pbuf.available_from(106), b"");
        assert!(!pbuf.is_drained());
    }

    #[test]
    fn advance_moves_the_cursor_and_tracks_compaction() {
        let mut pbuf = ParseBuffer::new(0);
        pbuf.append(b"0123456789").unwrap();
        pbuf.advance(4, THRESHOLD);
        assert_eq!(pbuf.cursor(), 4);
        assert_eq!(pbuf.available(), b"456789");
        assert!(!pbuf.compact_pending(), "below threshold");

        // With a tiny threshold the consumed prefix is marked for compaction.
        pbuf.advance(2, 2);
        assert_eq!(pbuf.cursor(), 6);
        assert!(pbuf.compact_pending());
    }

    #[test]
    fn truncate_tail_keeps_only_consumed_bytes() {
        let mut pbuf = ParseBuffer::new(10);
        pbuf.append(b"0123456789").unwrap();
        pbuf.advance(4, THRESHOLD);
        pbuf.truncate_to(pbuf.cursor());
        assert_eq!(pbuf.end(), 14);
        assert_eq!(pbuf.data(), b"0123");
        assert!(pbuf.is_drained());

        // Filling after the truncation appends at the cursor again.
        pbuf.append(b"abcd").unwrap();
        assert_eq!(pbuf.end(), 18);
        assert_eq!(pbuf.available(), b"abcd");
    }

    #[test]
    fn consume_to_rebases_the_consumed_prefix() {
        let mut pbuf = ParseBuffer::new(50);
        pbuf.append(b"01234567").unwrap();
        pbuf.advance(4, 2);
        assert!(pbuf.compact_pending());
        pbuf.consume_to(pbuf.cursor());
        assert_eq!(pbuf.from(), 54);
        assert_eq!(pbuf.cursor(), 54);
        assert_eq!(pbuf.end(), 58);
        assert_eq!(pbuf.data(), b"4567");
        assert_eq!(pbuf.available(), b"4567");
        assert!(!pbuf.compact_pending());
        assert!(!pbuf.is_drained());
    }

    #[test]
    fn clear_then_append_rebases_at_the_right_offset() {
        let mut pbuf = ParseBuffer::new(50);
        pbuf.append(b"0123456789").unwrap();
        pbuf.advance(10, 2);
        assert!(pbuf.is_drained());

        pbuf.clear_to(pbuf.cursor());
        assert_eq!(pbuf.from(), 60);
        assert_eq!(pbuf.cursor(), 60);
        assert_eq!(pbuf.end(), 60);
        assert_eq!(pbuf.data(), []);
        assert!(pbuf.is_drained());
        assert!(!pbuf.compact_pending());

        pbuf.append(b"xy").unwrap();
        assert_eq!(pbuf.from(), 60);
        assert_eq!(pbuf.end(), 62);
        assert_eq!(pbuf.index(61), 1);
        assert_eq!(pbuf.available(), b"xy");
        assert_eq!(pbuf.available_from(60), b"xy");
    }

    #[test]
    fn shrink_if_large_releases_oversized_capacity() {
        let mut pbuf = ParseBuffer::new(0);
        let big = vec![7u8; 64 * 1024];
        pbuf.append(&big).unwrap();
        pbuf.advance(big.len() as u64, THRESHOLD);
        pbuf.clear_to(pbuf.cursor());
        assert!(pbuf.buf.capacity() >= big.len());

        pbuf.shrink_if_large(1024);
        assert!(
            pbuf.buf.capacity() < big.len(),
            "capacity {} was not released",
            pbuf.buf.capacity()
        );

        // A buffer that is already small is left alone.
        let capacity = pbuf.buf.capacity();
        pbuf.shrink_if_large(THRESHOLD);
        assert_eq!(pbuf.buf.capacity(), capacity);
    }
}
