// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{BlockSize, BlockSizeSpec, GenericBlockSize, MaskShiftSpec};
use std::sync::atomic::{AtomicU64, Ordering};

/// Cached system page size in the MaskShiftSpec representation, initialized on first access.
static PAGE_MASK_SHIFT: AtomicU64 = AtomicU64::new(0);

/// Initializes `PAGE_MASK_SHIFT`.
#[cold]
#[inline(never)]
fn initialize_page_mask_shift() -> u64 {
    let page_size = zx::system_get_page_size();
    let repr = MaskShiftSpec::new(page_size as u64).0;
    PAGE_MASK_SHIFT.store(repr, Ordering::Relaxed);
    repr
}

/// Returns the system page size as a [`PageSize`].
#[inline(always)]
pub fn page_size() -> PageSize {
    let val = PAGE_MASK_SHIFT.load(Ordering::Relaxed);
    let val = if val != 0 { val } else { initialize_page_mask_shift() };
    GenericBlockSize(PageSizeSpec(MaskShiftSpec(val)))
}

/// A [`GenericBlockSize`] configured with the system's page size.
pub type PageSize = GenericBlockSize<PageSizeSpec>;

/// [`BlockSizeSpec`] implementation representing the Fuchsia system memory page size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PageSizeSpec(MaskShiftSpec);

impl BlockSizeSpec for PageSizeSpec {
    #[inline(always)]
    fn size(self) -> u64 {
        self.0.size()
    }

    #[inline(always)]
    fn mask(self) -> u64 {
        self.0.mask()
    }

    #[inline(always)]
    fn shift(self) -> u32 {
        self.0.shift()
    }
}

impl From<PageSize> for BlockSize {
    #[inline(always)]
    fn from(page_size: PageSize) -> Self {
        GenericBlockSize(page_size.0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_page_size() {
        let expected = zx::system_get_page_size() as u64;
        let page_size = page_size();
        assert_eq!(page_size.get(), expected);
        assert_eq!(page_size.mask(), expected - 1);
        assert_eq!(page_size.shift(), (expected - 1).trailing_ones());
        assert!(page_size.is_aligned(expected));
        assert!(!page_size.is_aligned(expected - 1));
    }

    #[fuchsia::test]
    fn test_page_size_to_block_size() {
        let page_size = page_size();
        let block_size: BlockSize = page_size.into();
        assert_eq!(page_size.get(), block_size.get());
        assert_eq!(page_size.mask(), block_size.mask());
        assert_eq!(page_size.shift(), block_size.shift());
    }
}
