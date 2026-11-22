// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::{PAddr, VAddr};

/// Helper that tracks a virtual address range for performing MMU operations. This can be used
/// directly, or as part of a [`MappingCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualAddressCursor {
    /// Initial starting virtual address of the cursor.
    start_vaddr: VAddr,
    /// Current virtual address of the cursor.
    vaddr: VAddr,
    /// Offset subtracted when computing [`Self::vaddr_rel`].
    vaddr_rel_offset: VAddr,
    /// Remaining size in bytes to process.
    size: usize,
}

impl VirtualAddressCursor {
    /// Creates a new cursor for the virtual address range `[vaddr, vaddr + size)`.
    #[inline]
    pub const fn new(vaddr: VAddr, size: usize) -> Self {
        Self { start_vaddr: vaddr, vaddr, vaddr_rel_offset: VAddr(0), size }
    }

    /// Sets offset used for `vaddr_rel` and returns true if the cursor lies within that offset and
    /// some specified maximum. Should only be called before the cursor has started to be used.
    #[must_use]
    #[inline]
    pub fn set_vaddr_relative_offset(
        &mut self,
        vaddr_rel_offset: VAddr,
        vaddr_rel_max: usize,
    ) -> bool {
        debug_assert!(self.start_vaddr == self.vaddr);
        let vaddr_rel = VAddr(self.start_vaddr.0.wrapping_sub(vaddr_rel_offset.0));

        if vaddr_rel.0 > vaddr_rel_max.wrapping_sub(self.size) || self.size > vaddr_rel_max {
            return false;
        }
        self.vaddr_rel_offset = vaddr_rel_offset;
        true
    }

    /// Update the cursor to skip over a not-present page table entry.
    #[inline]
    pub fn skip_entry(&mut self, ps: usize) {
        // Cannot just increase by given size as both the current or final vaddr may not be aligned
        // to this given size. These cases only happen as the very first or very last entry we will
        // examine respectively, but still must be handled here.

        // Calculate the amount the cursor should skip to get to the next entry at
        // this page table level.
        let next_entry_offset = ps.wrapping_sub(self.vaddr.0 & ps.wrapping_sub(1));
        // If our endpoint was in the middle of this range, clamp the
        // amount we remove from the cursor
        let consume = if self.size > next_entry_offset { next_entry_offset } else { self.size };

        debug_assert!(self.size >= consume);
        self.size -= consume;
        self.vaddr = VAddr(self.vaddr.0.wrapping_add(consume));
    }

    /// Consumes `ps` bytes from the cursor.
    #[inline]
    pub fn consume(&mut self, ps: usize) {
        debug_assert!(self.size >= ps);
        self.vaddr = VAddr(self.vaddr.0.wrapping_add(ps));
        self.size -= ps;
    }

    /// Returns a new cursor to the, possibly empty, virtual range that has already been processed
    /// by this cursor. The returned cursor will always be a subset of the original cursor's range.
    #[must_use]
    #[inline]
    pub const fn processed_range(&self) -> VirtualAddressCursor {
        let mut ret = VirtualAddressCursor::new(
            self.start_vaddr,
            self.vaddr.0.wrapping_sub(self.start_vaddr.0),
        );
        // As our new cursor is a subrange we know the relative offset will always be valid.
        ret.vaddr_rel_offset = self.vaddr_rel_offset;
        ret
    }

    /// Returns the current virtual address.
    #[inline]
    pub const fn vaddr(&self) -> VAddr {
        self.vaddr
    }

    /// Returns the current virtual address relative to the configured offset.
    #[inline]
    pub const fn vaddr_rel(&self) -> VAddr {
        VAddr(self.vaddr.0.wrapping_sub(self.vaddr_rel_offset.0))
    }

    /// Returns the remaining size in bytes.
    #[inline]
    pub const fn size(&self) -> usize {
        self.size
    }
}

/// Helper class for MMU implementations to track physical address ranges when installing mappings.
/// If just processing a virtual address range, such as for unmapping, then the
/// [`VirtualAddressCursor`] can be used instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingCursor<'a> {
    /// Remaining physical addresses to map.
    paddrs: &'a [PAddr],
    /// Bytes consumed within the current `page_size` physical region.
    paddr_consumed: usize,
    /// Size in bytes of each physical region in `paddrs`.
    page_size: usize,

    /// Underlying virtual address range cursor.
    vaddr_cursor: VirtualAddressCursor,
}

impl<'a> MappingCursor<'a> {
    /// Creates a new mapping cursor over `paddrs`, where each entry covers `page_size` bytes
    /// starting at `vaddr`.
    #[inline]
    pub const fn new(paddrs: &'a [PAddr], page_size: usize, vaddr: VAddr) -> Self {
        Self {
            paddrs,
            paddr_consumed: 0,
            page_size,
            vaddr_cursor: VirtualAddressCursor::new(vaddr, page_size.saturating_mul(paddrs.len())),
        }
    }

    /// See [`VirtualAddressCursor::set_vaddr_relative_offset`].
    #[must_use]
    #[inline]
    pub fn set_vaddr_relative_offset(
        &mut self,
        vaddr_rel_offset: VAddr,
        vaddr_rel_max: usize,
    ) -> bool {
        self.vaddr_cursor.set_vaddr_relative_offset(vaddr_rel_offset, vaddr_rel_max)
    }

    /// Consumes `ps` bytes from both the physical and virtual ranges.
    #[inline]
    pub fn consume(&mut self, ps: usize) {
        self.paddr_consumed = self.paddr_consumed.wrapping_add(ps);
        debug_assert!(self.paddr_consumed <= self.page_size);
        if self.paddr_consumed == self.page_size {
            debug_assert!(!self.paddrs.is_empty());
            self.paddrs = &self.paddrs[1..];
            self.paddr_consumed = 0;
        }
        self.vaddr_cursor.consume(ps);
    }

    /// Returns the current physical address.
    #[inline]
    pub const fn paddr(&self) -> PAddr {
        debug_assert!(self.paddr_consumed < self.page_size);
        debug_assert!(!self.paddrs.is_empty());
        PAddr(self.paddrs[0].0.wrapping_add(self.paddr_consumed))
    }

    /// Returns the remaining unconsumed bytes in the current physical page entry.
    #[inline]
    pub const fn page_remaining(&self) -> usize {
        self.page_size.wrapping_sub(self.paddr_consumed)
    }

    /// Returns a new cursor to the, possibly empty, virtual range that has already been processed
    /// by this cursor. The returned cursor will always be a subset of the original cursor's range
    /// and does not include the `paddrs`.
    #[must_use]
    #[inline]
    pub const fn processed_range(&self) -> VirtualAddressCursor {
        self.vaddr_cursor.processed_range()
    }

    /// Returns the current virtual address.
    #[inline]
    pub const fn vaddr(&self) -> VAddr {
        self.vaddr_cursor.vaddr()
    }

    /// Returns the current virtual address relative to the configured offset.
    #[inline]
    pub const fn vaddr_rel(&self) -> VAddr {
        self.vaddr_cursor.vaddr_rel()
    }

    /// Returns the remaining size in bytes.
    #[inline]
    pub const fn size(&self) -> usize {
        self.vaddr_cursor.size()
    }
}

/// Unit tests for [`VirtualAddressCursor`] and [`MappingCursor`].
#[cfg(ktest)]
#[unittest::suite(name = "mapping_cursor")]
mod tests {
    use super::{MappingCursor, VirtualAddressCursor};
    use crate::kernel::types::{PAddr, VAddr};
    use unittest::{expect_eq, expect_false, expect_true};

    const PAGE_SIZE: usize = page::SIZE;
    const LARGE_PAGE_SIZE: usize = 512 * PAGE_SIZE;

    /// Verifies relative offset bounds checking and top-of-address-space wrapping arithmetic.
    #[test]
    fn virtual_address_cursor_relative_offset() {
        let offset = VAddr(0xffff_0000_0000_0000);
        let max_size: usize = 1usize << 48;

        // Below offset wraps to a large relative address and must be rejected.
        let mut below = VirtualAddressCursor::new(VAddr(0x1000), PAGE_SIZE);
        expect_false!(below.set_vaddr_relative_offset(offset, max_size));
        expect_eq!(below.vaddr_rel().0, 0x1000);

        // Size larger than max_size must be rejected without underflow panic.
        let mut oversized = VirtualAddressCursor::new(offset, max_size + PAGE_SIZE);
        expect_false!(oversized.set_vaddr_relative_offset(offset, max_size));

        // Range extending past offset + max_size must be rejected.
        let mut past_end =
            VirtualAddressCursor::new(VAddr(usize::MAX - PAGE_SIZE + 1), 2 * PAGE_SIZE);
        expect_false!(past_end.set_vaddr_relative_offset(offset, max_size));

        // Range ending at the top of the 64-bit address space (wrapping end to 0) is valid.
        let top_start = VAddr(0usize.wrapping_sub(2 * PAGE_SIZE));
        let mut top = VirtualAddressCursor::new(top_start, 2 * PAGE_SIZE);
        expect_true!(top.set_vaddr_relative_offset(offset, max_size));
        expect_eq!(top.vaddr_rel().0, max_size - 2 * PAGE_SIZE);

        top.consume(PAGE_SIZE);
        expect_eq!(top.vaddr().0, 0usize.wrapping_sub(PAGE_SIZE));
        expect_eq!(top.vaddr_rel().0, max_size - PAGE_SIZE);

        top.consume(PAGE_SIZE);
        expect_eq!(top.vaddr().0, 0);
        expect_eq!(top.size(), 0);

        let processed = top.processed_range();
        expect_eq!(processed.vaddr().0, top_start.0);
        expect_eq!(processed.vaddr_rel().0, max_size - 2 * PAGE_SIZE);
        expect_eq!(processed.size(), 2 * PAGE_SIZE);
    }

    /// Verifies `skip_entry`, `consume`, and `processed_range` across unaligned start/end ranges.
    #[test]
    fn virtual_address_cursor_skip_and_consume() {
        let start = VAddr(0x1000_0000 + PAGE_SIZE);
        let total = 2 * LARGE_PAGE_SIZE + 2 * PAGE_SIZE;
        let mut cursor = VirtualAddressCursor::new(start, total);
        expect_true!(cursor.set_vaddr_relative_offset(VAddr(0x1000_0000), 1usize << 30));
        expect_eq!(cursor.processed_range().size(), 0);

        // Unaligned start: skips only up to the next `LARGE_PAGE_SIZE` boundary.
        cursor.skip_entry(LARGE_PAGE_SIZE);
        expect_eq!(cursor.vaddr().0, 0x1000_0000 + LARGE_PAGE_SIZE);
        expect_eq!(cursor.size(), LARGE_PAGE_SIZE + 3 * PAGE_SIZE);

        // Aligned middle entry: consumes an entire `LARGE_PAGE_SIZE` block.
        cursor.consume(LARGE_PAGE_SIZE);
        expect_eq!(cursor.vaddr().0, 0x1000_0000 + 2 * LARGE_PAGE_SIZE);
        expect_eq!(cursor.size(), 3 * PAGE_SIZE);

        let partial = cursor.processed_range();
        expect_eq!(partial.vaddr().0, start.0);
        expect_eq!(partial.vaddr_rel().0, PAGE_SIZE);
        expect_eq!(partial.size(), total - 3 * PAGE_SIZE);

        // Unaligned end: clamps to the remaining 3 pages rather than overshooting.
        cursor.skip_entry(LARGE_PAGE_SIZE);
        expect_eq!(cursor.vaddr().0, start.0 + total);
        expect_eq!(cursor.size(), 0);
    }

    /// Verifies `MappingCursor` sub-page consumption, physical array advancement, and rollback.
    #[test]
    fn mapping_cursor_consume_and_processed_range() {
        let empty = MappingCursor::new(&[], PAGE_SIZE, VAddr(0x2000));
        expect_eq!(empty.size(), 0);
        expect_eq!(empty.processed_range().size(), 0);

        let paddrs = [PAddr(0x4000_0000), PAddr(0x8000_0000)];
        let base = VAddr(0x1000_0000);

        // Overflowing `page_size * paddrs.len()` saturates to `usize::MAX` and is rejected by
        // `set_vaddr_relative_offset`.
        let mut overflow = MappingCursor::new(&paddrs, 1usize << 63, base);
        expect_eq!(overflow.size(), usize::MAX);
        expect_false!(overflow.set_vaddr_relative_offset(base, 1usize << 48));

        let mut cursor = MappingCursor::new(&paddrs, 2 * PAGE_SIZE, VAddr(base.0 + 0x4000));
        expect_true!(cursor.set_vaddr_relative_offset(base, 0x20000));
        expect_eq!(cursor.size(), 4 * PAGE_SIZE);
        expect_eq!(cursor.paddr().0, 0x4000_0000);
        expect_eq!(cursor.page_remaining(), 2 * PAGE_SIZE);
        expect_eq!(cursor.vaddr_rel().0, 0x4000);

        // Consume half of the first `2 * PAGE_SIZE` entry.
        cursor.consume(PAGE_SIZE);
        expect_eq!(cursor.paddr().0, 0x4000_0000 + PAGE_SIZE);
        expect_eq!(cursor.page_remaining(), PAGE_SIZE);
        expect_eq!(cursor.vaddr().0, base.0 + 0x4000 + PAGE_SIZE);
        expect_eq!(cursor.size(), 3 * PAGE_SIZE);

        // Finish the first entry and advance to the second physical entry.
        cursor.consume(PAGE_SIZE);
        expect_eq!(cursor.paddr().0, 0x8000_0000);
        expect_eq!(cursor.page_remaining(), 2 * PAGE_SIZE);
        expect_eq!(cursor.size(), 2 * PAGE_SIZE);

        let processed = cursor.processed_range();
        expect_eq!(processed.vaddr().0, base.0 + 0x4000);
        expect_eq!(processed.vaddr_rel().0, 0x4000);
        expect_eq!(processed.size(), 2 * PAGE_SIZE);

        // Consume the entire second entry at once.
        cursor.consume(2 * PAGE_SIZE);
        expect_eq!(cursor.size(), 0);
        expect_eq!(cursor.page_remaining(), 2 * PAGE_SIZE);
    }
}
