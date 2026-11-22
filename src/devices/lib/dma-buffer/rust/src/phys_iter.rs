// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physical address and size segment iterator for pinned DMA buffers.

use zx::sys::zx_paddr_t;
use zx::system_get_page_size;

/// An iterator yielding `(zx_paddr_t, usize)` physical address and byte length pairs for a pinned
/// DMA buffer.
///
/// The yielded pairs are suitable for hardware DMA transactions that consume a physical address and
/// byte length. The iterator ensures chunk boundaries are not crossed unless the buffer is backed
/// by a single contiguous chunk (`chunk_list.len() == 1`) or contiguous chunk merging (`merge`) is
/// enabled.
#[derive(Debug, Clone)]
pub struct PhysIter<'a> {
    chunk_list: &'a [zx_paddr_t],
    max_length: usize,
    chunk_size: usize,
    merge: bool,
    chunk_index: usize,
    offset_in_chunk: usize,
    remaining: usize,
}

impl<'a> PhysIter<'a> {
    /// Creates a new `PhysIter` with page-sized chunks, unlimited segment length, and chunk merging
    /// disabled.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_list` is empty, `buf_length == 0`,
    /// `chunk_list.len() > 1 && vmo_offset >= system_get_page_size() as usize`, `buf_length`
    /// exceeds the capacity of `chunk_list` when `chunk_list.len() > 1`, or any offset/physical
    /// address addition overflows `usize`.
    pub fn new(chunk_list: &'a [zx_paddr_t], vmo_offset: usize, buf_length: usize) -> Self {
        Self::new_with_options(
            chunk_list,
            system_get_page_size() as usize,
            vmo_offset,
            buf_length,
            usize::MAX,
            false,
        )
    }

    /// Creates a new `PhysIter` with explicit chunk size, maximum segment length, and chunk
    /// merging options.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_list` is empty, `buf_length == 0`, `chunk_size` is not a non-zero power
    /// of two, `chunk_list.len() > 1 && vmo_offset >= chunk_size`, `buf_length` exceeds the
    /// capacity of `chunk_list` when `chunk_list.len() > 1`, or any offset/physical address
    /// addition overflows `usize`.
    pub fn new_with_options(
        chunk_list: &'a [zx_paddr_t],
        chunk_size: usize,
        vmo_offset: usize,
        buf_length: usize,
        max_length: usize,
        merge: bool,
    ) -> Self {
        assert!(!chunk_list.is_empty(), "chunk_list must not be empty");
        assert!(buf_length > 0, "buf_length must be greater than 0");
        assert!(chunk_size.is_power_of_two(), "chunk_size must be a power of 2");
        assert!(
            chunk_list.len() == 1 || vmo_offset < chunk_size,
            "vmo_offset must be less than chunk_size when chunk_list has multiple entries"
        );

        let end_offset =
            vmo_offset.checked_add(buf_length).expect("vmo_offset + buf_length overflow");
        if chunk_list.len() == 1 {
            let _ = chunk_list[0].checked_add(end_offset).expect("physical address range overflow");
        } else {
            let used_chunks = end_offset.div_ceil(chunk_size);
            assert!(used_chunks <= chunk_list.len(), "buf_length exceeds chunk_list capacity");
            for &paddr in &chunk_list[..used_chunks - 1] {
                let _ =
                    paddr.checked_add(chunk_size).expect("chunk physical address range overflow");
            }
            let last_chunk_end = end_offset - (used_chunks - 1) * chunk_size;
            let _ = chunk_list[used_chunks - 1]
                .checked_add(last_chunk_end)
                .expect("chunk physical address range overflow");
        }

        Self {
            chunk_list,
            max_length: if max_length == 0 { usize::MAX } else { max_length },
            chunk_size,
            merge,
            chunk_index: 0,
            offset_in_chunk: vmo_offset,
            remaining: buf_length,
        }
    }
}

impl Iterator for PhysIter<'_> {
    type Item = (zx_paddr_t, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        let addr = self.chunk_list[self.chunk_index] + self.offset_in_chunk;
        let seg_len = if self.chunk_list.len() == 1 {
            self.remaining.min(self.max_length)
        } else {
            let mut avail = self.chunk_size - self.offset_in_chunk;
            let mut idx = self.chunk_index;
            if self.merge {
                while avail < self.remaining && avail < self.max_length {
                    if self.chunk_list[idx] + self.chunk_size != self.chunk_list[idx + 1] {
                        break;
                    }
                    avail = avail.saturating_add(self.chunk_size);
                    idx += 1;
                }
            }
            avail.min(self.remaining).min(self.max_length)
        };

        self.remaining -= seg_len;
        if self.chunk_list.len() == 1 {
            self.offset_in_chunk += seg_len;
        } else if self.remaining > 0 {
            let next_offset = self.offset_in_chunk + seg_len;
            self.chunk_index += next_offset / self.chunk_size;
            self.offset_in_chunk = next_offset % self.chunk_size;
        }
        Some((addr, seg_len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PAGE_SIZE: usize = 4096;
    const HALF_PAGE: usize = 2048;

    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (2 * PAGE_SIZE, PAGE_SIZE)];
        "simple_page_boundary"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        3 * PAGE_SIZE,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "non_contiguous_pages"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(2 * PAGE_SIZE - 5, 5), (2 * PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE)];
        "partial_first_page_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(2 * PAGE_SIZE - 5, 5), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "partial_first_page_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (2 * PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, 5)];
        "partial_last_page_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, 5)];
        "partial_last_page_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (4 * HALF_PAGE, HALF_PAGE),
            (5 * HALF_PAGE, HALF_PAGE),
        ];
        "sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE],
        PAGE_SIZE,
        0,
        1 << 20,
        usize::MAX,
        false,
        &[(PAGE_SIZE, 1 << 20)];
        "zx_bti_contiguous_large_buffer"
    )]
    #[test_case(
        &[HALF_PAGE, 2 * HALF_PAGE, 3 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        false,
        &[(HALF_PAGE, HALF_PAGE), (2 * HALF_PAGE, HALF_PAGE), (3 * HALF_PAGE, HALF_PAGE)];
        "zx_bti_compress_contiguous"
    )]
    #[test_case(
        &[HALF_PAGE, 3 * HALF_PAGE, 5 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        false,
        &[(HALF_PAGE, HALF_PAGE), (3 * HALF_PAGE, HALF_PAGE), (5 * HALF_PAGE, HALF_PAGE)];
        "zx_bti_compress_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (4 * HALF_PAGE, HALF_PAGE),
            (5 * HALF_PAGE, HALF_PAGE),
        ];
        "zx_bti_contiguous_sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (6 * HALF_PAGE, HALF_PAGE),
            (7 * HALF_PAGE, HALF_PAGE),
        ];
        "zx_bti_non_contiguous_sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE)];
        "simple_page_boundary_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        3 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "non_contiguous_pages_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 4 * PAGE_SIZE, 5 * PAGE_SIZE, 6 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        5 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE), (4 * PAGE_SIZE, 3 * PAGE_SIZE)];
        "mixed_contiguous_and_non_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        true,
        &[(2 * PAGE_SIZE - 5, 2 * PAGE_SIZE + 5)];
        "partial_first_page_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE + 5)];
        "partial_last_page_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 4 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        4 * PAGE_SIZE,
        2 * PAGE_SIZE,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE), (3 * PAGE_SIZE, 2 * PAGE_SIZE)];
        "merge_respects_chunk_aligned_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        4 * PAGE_SIZE,
        PAGE_SIZE + HALF_PAGE,
        true,
        &[
            (PAGE_SIZE, PAGE_SIZE + HALF_PAGE),
            (2 * PAGE_SIZE + HALF_PAGE, PAGE_SIZE + HALF_PAGE),
            (5 * PAGE_SIZE, PAGE_SIZE),
        ];
        "merge_respects_unaligned_max_length"
    )]
    #[test_case(
        &[HALF_PAGE, 2 * HALF_PAGE, 3 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        true,
        &[(HALF_PAGE, 3 * HALF_PAGE)];
        "zx_bti_compress_contiguous_merge"
    )]
    #[test_case(
        &[0x22_0000, 0x35_0000],
        0x10_0000,
        0xf_8000,
        0x1_0000,
        0x1_0000,
        true,
        &[(0x31_8000, 0x8000), (0x35_0000, 0x8000)];
        "page_aligned_large_chunks_non_contiguous"
    )]
    #[test_case(
        &[0x22_0000, 0x32_0000],
        0x10_0000,
        0xf_8000,
        0x1_0000,
        0x1_0000,
        true,
        &[(0x31_8000, 0x1_0000)];
        "page_aligned_large_chunks_contiguous_merge"
    )]
    #[test_case(
        &[0x22_0000, 0x35_0000],
        0x10_0000,
        0xd_8000,
        0x3_0000,
        0x1_0000,
        true,
        &[(0x2f_8000, 0x1_0000), (0x30_8000, 0x1_0000), (0x31_8000, 0x8000), (0x35_0000, 0x8000)];
        "page_aligned_large_chunks_multi_step_across_boundary"
    )]
    fn test_phys_iter(
        chunk_list: &[zx_paddr_t],
        chunk_size: usize,
        vmo_offset: usize,
        buf_length: usize,
        max_length: usize,
        merge: bool,
        expected: &[(zx_paddr_t, usize)],
    ) {
        let iter = PhysIter::new_with_options(
            chunk_list, chunk_size, vmo_offset, buf_length, max_length, merge,
        );
        let actual: Vec<_> = iter.collect();
        assert_eq!(actual, expected);
    }

    #[fuchsia::test]
    #[should_panic(expected = "buf_length exceeds chunk_list capacity")]
    fn test_phys_iter_panics_on_out_of_bounds_multi_chunk_buf_length() {
        let _ = PhysIter::new_with_options(
            &[PAGE_SIZE, 2 * PAGE_SIZE],
            PAGE_SIZE,
            0,
            2 * PAGE_SIZE + 1,
            usize::MAX,
            false,
        );
    }

    #[fuchsia::test]
    #[should_panic(expected = "buf_length exceeds chunk_list capacity")]
    fn test_phys_iter_panics_on_out_of_bounds_multi_chunk_with_offset() {
        let _ = PhysIter::new_with_options(
            &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
            PAGE_SIZE,
            PAGE_SIZE - 5,
            2 * PAGE_SIZE + 6,
            usize::MAX,
            true,
        );
    }

    #[fuchsia::test]
    #[should_panic(expected = "overflow")]
    fn test_phys_iter_panics_on_vmo_offset_plus_buf_length_overflow() {
        let _ =
            PhysIter::new_with_options(&[PAGE_SIZE], PAGE_SIZE, 16, usize::MAX, usize::MAX, false);
    }

    #[fuchsia::test]
    #[should_panic(expected = "overflow")]
    fn test_phys_iter_panics_on_first_paddr_overflow() {
        let _ = PhysIter::new_with_options(&[usize::MAX - 3], PAGE_SIZE, 8, 8, usize::MAX, false);
    }

    #[fuchsia::test]
    #[should_panic(expected = "overflow")]
    fn test_phys_iter_panics_on_single_chunk_end_paddr_overflow() {
        let _ = PhysIter::new_with_options(&[usize::MAX - 15], PAGE_SIZE, 0, 32, usize::MAX, false);
    }

    #[fuchsia::test]
    #[should_panic(expected = "overflow")]
    fn test_phys_iter_panics_on_multi_chunk_paddr_overflow() {
        let _ = PhysIter::new_with_options(
            &[usize::MAX - 100, PAGE_SIZE],
            PAGE_SIZE,
            0,
            2 * PAGE_SIZE,
            usize::MAX,
            true,
        );
    }
}
