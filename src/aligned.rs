//! Aligned memory buffer used for Direct I/O.
//!
//! `O_DIRECT` requires the I/O buffer address, the file offset and the
//! transfer length to all be aligned to the device's logical block size.
//! This module provides a small owned buffer whose address is aligned.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

/// A heap buffer with a guaranteed address alignment.
pub(crate) struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuffer {
    /// Allocates `size` bytes of zeroed memory aligned to `align`.
    ///
    /// `align` must be a non-zero power of two. The requested `size` is
    /// rounded up to a multiple of `align` inside this function. [`Layout`]
    /// itself does **not** require the size to be a multiple of the alignment
    /// (it only requires `size <= isize::MAX - (align - 1)`); the rounding
    /// exists because callers pass slices of this buffer to `O_DIRECT` I/O,
    /// whose transfer length must be a multiple of the block size.
    pub(crate) fn new_zeroed(size: usize, align: usize) -> std::io::Result<Self> {
        if !align.is_power_of_two() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "alignment must be a non-zero power of two",
            ));
        }
        let size = size.max(align);
        // Round up to a whole number of `align` bytes. `checked_add` keeps a
        // pathological `size` from wrapping (the `Layout` below would reject a
        // wrapped value anyway, but an overflow panic in debug builds is worse
        // than a returned error).
        let size = size.checked_add(align - 1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "aligned buffer size overflows usize",
            )
        })? / align
            * align;
        let layout = Layout::from_size_align(size, align)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `layout` has a non-zero size (size >= align > 0). A null
        // return indicates allocation failure, handled below.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            "failed to allocate aligned buffer",
        ))?;
        Ok(Self { ptr, layout })
    }

    /// Total usable size of the buffer in bytes.
    pub(crate) fn len(&self) -> usize {
        self.layout.size()
    }

    /// Views the whole buffer as a mutable slice.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `layout.size()` initialized bytes of owned
        // memory, and `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }

    /// Views the whole buffer as a slice.
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` points to `layout.size()` initialized bytes of owned
        // memory, and the buffer is not mutated while the slice is alive.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` was allocated with `layout` in `new_zeroed` and is
        // only deallocated here, once.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// SAFETY: `AlignedBuffer` owns its memory exclusively; there are no
// thread-local invariants, so moving it to another thread is safe.
unsafe impl Send for AlignedBuffer {}

/// Rounds `value` up to a multiple of `align` (power of two).
pub(crate) fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value.div_ceil(align) * align
}

/// Rounds `value` down to a multiple of `align` (power of two).
pub(crate) fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value / align * align
}

/// Ensures `align` is a power of two within `[512, 1 MiB]`, suitable as a
/// Direct I/O block size.
pub(crate) fn check_block_size(block_size: usize) -> crate::Result<()> {
    let ok = block_size.is_power_of_two() && (512..=(1 << 20)).contains(&block_size);
    if ok {
        Ok(())
    } else {
        Err(crate::Error::InvalidOptions(format!(
            "block_size must be a power of two in [512, 1MiB], got {block_size}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer_is_aligned_and_zeroed() {
        let mut buf = AlignedBuffer::new_zeroed(100, 4096).unwrap();
        assert!(buf.len() >= 100);
        assert_eq!(buf.len() % 4096, 0);
        assert_eq!(buf.as_slice().as_ptr() as usize % 4096, 0);
        assert!(buf.as_slice().iter().all(|b| *b == 0));
        buf.as_mut_slice()[0] = 7;
        assert_eq!(buf.as_slice()[0], 7);
    }

    #[test]
    fn new_zeroed_rounds_size_up_to_alignment() {
        // 100 bytes with 4 KiB alignment: exactly one block (this used to be
        // rounded to the next power of two by the writer's staging buffer).
        assert_eq!(AlignedBuffer::new_zeroed(100, 4096).unwrap().len(), 4096);
        // 4 KiB + 1 rounds up to two blocks.
        assert_eq!(AlignedBuffer::new_zeroed(4097, 4096).unwrap().len(), 8192);
        // Exactly aligned sizes are left alone.
        assert_eq!(AlignedBuffer::new_zeroed(8192, 4096).unwrap().len(), 8192);
        // Small sizes still get one full aligned block.
        assert_eq!(AlignedBuffer::new_zeroed(1, 512).unwrap().len(), 512);
        // Zero size maps to one block: the allocation must never be empty.
        assert_eq!(AlignedBuffer::new_zeroed(0, 512).unwrap().len(), 512);
    }

    #[test]
    fn new_zeroed_rejects_invalid_alignment() {
        assert!(AlignedBuffer::new_zeroed(64, 0).is_err());
        assert!(AlignedBuffer::new_zeroed(64, 1000).is_err());
    }

    #[test]
    fn align_helpers() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
        assert_eq!(align_down(0, 4096), 0);
        assert_eq!(align_down(4095, 4096), 0);
        assert_eq!(align_down(4097, 4096), 4096);
    }

    #[test]
    fn block_size_validation() {
        assert!(check_block_size(4096).is_ok());
        assert!(check_block_size(512).is_ok());
        assert!(check_block_size(4097).is_err());
        assert!(check_block_size(256).is_err());
        assert!(check_block_size(2 << 20).is_err());
    }
}
