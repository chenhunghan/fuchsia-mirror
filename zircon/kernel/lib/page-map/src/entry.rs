// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{PageMap, PageMapLockClass};
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::ptr::NonNull;
use fbl::{RefPtr, WavlTreeContainable, WavlTreeKeyable, WavlTreeNode};
use ksync::{KCell, LockToken};
use pin_init::{Init, init};
use zx_status::Status;

/// Identifies a page in a VMO using the VMO's address in memory and the page's offset within the
/// VMO.
///
/// Why is it OK to use the object's address in the key?  It seems unlikely we'll implement a
/// garbage collection or object forwarding scheme that would result in multiple entries for a
/// single (logically) VMO.  And even if we do, it would likely manifest as an efficiency issue
/// rather than a correctness issue.
///
/// Required by `WavlTreeKeyable`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub vm_object_paged: usize,
    pub page_offset_in_vmo: usize,
}

/// `Entry` is logically private to `Accessor` and `PageMap`.
///
/// An `Entry` in a `PageMap` that refers to a single mapped page of a VMO.
#[derive(WavlTreeContainable)]
pub struct Entry {
    /// `mapping` is declared before `vmo` so that the mapping is dropped before the VMO it maps.
    mapping: RefPtr<VmMapping>,
    vmo: RefPtr<VmObjectPaged>,
    page_map: NonNull<PageMap>,
    /// Logically guarded by `page_map`'s `lock`.
    ///
    /// TODO(maniscalco): Explore techniques that would allow us to mark this as guarded by the
    /// owning `PageMap`'s lock instance without paying for a reference to the owning `PageMap`.
    accessor_count: KCell<u64, PageMapLockClass>,
    #[wavl_node]
    node: WavlTreeNode<Entry>,
}

// SAFETY: `accessor_count` is only accessed while holding `page_map`'s `lock`, `page_map` points
// to a `PageMap`, which is `Sync`, and `VmObjectPaged` and `VmMapping` synchronize internally, so
// an `Entry` may be created, used, and destroyed on any thread.
unsafe impl Send for Entry {}
// SAFETY: Shared references to `Entry` only read immutable fields, call thread-safe methods on
// `VmObjectPaged` and `VmMapping`, or access `accessor_count` while holding `page_map`'s `lock`.
unsafe impl Sync for Entry {}

impl WavlTreeKeyable<Key> for Entry {
    type Key<'a> = Key;

    /// # Safety
    ///
    /// See [`WavlTreeKeyable::get_key`].  `Entry`'s key fields (`vmo` and `mapping`) are
    /// immutable.
    unsafe fn get_key(&self) -> Key {
        Key {
            vm_object_paged: self.vmo.as_raw().addr(),
            page_offset_in_vmo: self.mapping.object_offset() as usize,
        }
    }
}

impl Entry {
    pub fn new(
        page_map: &PageMap,
        vmo: RefPtr<VmObjectPaged>,
        mapping: RefPtr<VmMapping>,
    ) -> impl Init<Self, Status> {
        let page_map = NonNull::from(page_map);
        init!(Self {
            mapping,
            vmo,
            page_map,
            accessor_count: KCell::new(0),
            node: WavlTreeNode::new(),
        }? Status)
    }

    /// Increment this entry's accessor count.
    ///
    /// # Safety
    ///
    /// May only be called while holding the `lock` of the `PageMap` that owns this entry, and
    /// `token` must be that lock's token.
    pub unsafe fn increment_accessor_count(&self, token: &mut LockToken<'_, PageMapLockClass>) {
        // SAFETY: The caller guarantees that `token` belongs to the owning `PageMap`'s `lock`,
        // which guards `accessor_count`.
        unsafe { *self.accessor_count.get_mut(token) += 1 };
    }

    /// Decrement this entry's accessor count.  Returns true when the ref-count has reached zero.
    ///
    /// # Safety
    ///
    /// May only be called while holding the `lock` of the `PageMap` that owns this entry, and
    /// `token` must be that lock's token.
    pub unsafe fn decrement_accessor_count(
        &self,
        token: &mut LockToken<'_, PageMapLockClass>,
    ) -> bool {
        // SAFETY: The caller guarantees that `token` belongs to the owning `PageMap`'s `lock`,
        // which guards `accessor_count`.
        let accessor_count = unsafe { self.accessor_count.get_mut(token) };
        debug_assert!(*accessor_count > 0);
        *accessor_count -= 1;
        *accessor_count == 0
    }

    /// Return this entry's accessor count.
    ///
    /// # Safety
    ///
    /// May only be called while holding the `lock` of the `PageMap` that owns this entry, and
    /// `token` must be that lock's token.
    pub unsafe fn accessor_count(&self, token: &LockToken<'_, PageMapLockClass>) -> u64 {
        // SAFETY: The caller guarantees that `token` belongs to the owning `PageMap`'s `lock`,
        // which guards `accessor_count`.
        unsafe { *self.accessor_count.get(token) }
    }

    /// Release an `Accessor`'s reference to this instance.  Intended to be called by `Accessor`'s
    /// drop.
    ///
    /// # Safety
    ///
    /// `this` must point to a live `Entry` on which the caller holds an accessor reference (see
    /// `increment_accessor_count`).  The caller gives up that reference and must not use `this`
    /// afterwards, as this call may destroy `*this`.
    pub unsafe fn release(this: NonNull<Self>) {
        // SAFETY: The caller guarantees that `this` points to a live `Entry`.
        let page_map = unsafe { this.as_ref() }.page_map;
        // SAFETY: An `Entry` is owned by the `PageMap` it points to, which is pinned and outlives
        // it.  The caller guarantees that `this` is live and transfers its accessor reference.
        unsafe { page_map.as_ref().release(this) };
    }

    pub fn mapping_base(&self) -> usize {
        self.mapping.base().0
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        debug_assert!(
            *self.accessor_count.get_inner_mut() == 0,
            "{}",
            *self.accessor_count.get_inner_mut()
        );

        let page_offset_in_vmo = self.mapping.object_offset();
        let status = self.mapping.destroy();
        debug_assert!(status.is_ok(), "{:?}", status);

        const MAPPING_SIZE: usize = page::SIZE;
        self.vmo.unpin(page_offset_in_vmo, MAPPING_SIZE as u64);
    }
}
