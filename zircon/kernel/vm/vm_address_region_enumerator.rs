// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::marker::PhantomData;
use core::{hint, mem, ptr};
use ksync::LockToken;

use super::vm_address_region::region_list::{self, Cursor};
use super::vm_address_region::{
    LifeCycleState, OwnedVmAddressRegionOrMapping, VmAddressRegion, VmAddressRegionOrMapping,
};
use super::vm_aspace::VmAspaceLockClass;
use super::vm_mapping::VmMapping;
use crate::kernel::types::VAddr;

pub(crate) trait VmAddressRegionEnumeratorType {
    type Item<'b>;
    const YIELDS_VMARS: bool;

    fn from_mapping(mapping: &VmMapping) -> Self::Item<'_>;
    fn from_vmar(vmar: &VmAddressRegion) -> Option<Self::Item<'_>>;
}

/// Both vmars and mappings will be yielded in calls to `next`. Only vmars that are wholly
/// contained within the requested range will be yielded, and it is considered an error to
/// knowingly attempt an enumeration that partially overlaps any sub vmars.
pub(crate) struct VmarsAndMappings;

impl VmAddressRegionEnumeratorType for VmarsAndMappings {
    type Item<'b> = VmAddressRegionOrMapping<'b>;
    const YIELDS_VMARS: bool = true;

    #[inline]
    fn from_mapping(mapping: &VmMapping) -> Self::Item<'_> {
        VmAddressRegionOrMapping::Mapping(mapping)
    }

    #[inline]
    fn from_vmar(vmar: &VmAddressRegion) -> Option<Self::Item<'_>> {
        Some(VmAddressRegionOrMapping::Region(vmar))
    }
}

/// Only mappings will be yielded in calls to `next`, and there are no restrictions on the kind
/// of range that can be specified.
pub(crate) struct MappingsOnly;

impl VmAddressRegionEnumeratorType for MappingsOnly {
    type Item<'b> = &'b VmMapping;
    const YIELDS_VMARS: bool = false;

    #[inline]
    fn from_mapping(mapping: &VmMapping) -> Self::Item<'_> {
        mapping
    }

    #[inline]
    fn from_vmar(_vmar: &VmAddressRegion) -> Option<Self::Item<'_>> {
        None
    }
}

const START_DEPTH: u32 = 1;

enum PauseState {
    Unpaused,
    /// Paused before `region_or_mapping`, which ends at `next_offset`.
    Paused {
        next_offset: usize,
        region_or_mapping: OwnedVmAddressRegionOrMapping,
    },
    /// Paused with nothing left to enumerate.
    PausedAtEnd,
}

/// Helper struct for performing enumeration of a VMAR.
///
/// The purpose of having a stateful enumerator is to have the option to not need to hold the aspace
/// lock over the entire enumeration, whilst guaranteeing forward progress and termination. If the
/// vmar is modified whilst enumeration is paused (due to dropping the lock or otherwise) then it is
/// not well defined whether the enumerator will return any new mappings. However, the enumerator
/// will never return `Dead` mappings, and will not return mappings in ranges it has already
/// enumerated.
///
/// Except between calls to `pause` and `resume`, the vmar should be considered immutable, and
/// sub-vmars and mappings should not be modified.
pub(crate) struct VmAddressRegionEnumerator<'a, T: VmAddressRegionEnumeratorType> {
    /// # Safety
    ///
    /// While `Paused`, `region_or_mapping` is a child of `vmar` or of a VMAR below it for as long
    /// as it is alive.
    state: PauseState,
    min_addr: usize,
    max_addr: usize,
    /// # Safety
    ///
    /// `depth >= START_DEPTH`, and the VMAR whose region list `itr` points into is
    /// `depth - START_DEPTH` levels below `vmar`.
    depth: u32,
    /// Root vmar being enumerated.
    vmar: &'a VmAddressRegion,
    /// This iterator represents the object at which `next` should use to find the next item to
    /// return. An invalid `itr` therefore represents no next object, and means enumeration has
    /// finished.
    ///
    /// # Safety
    ///
    /// `None` while paused, or after enumeration has finished by ascending out of `vmar`'s region
    /// list. Otherwise a cursor into the region list of `vmar` or of a live VMAR below it, which is
    /// not modified while the enumerator is not paused.
    itr: Option<Cursor<'a>>,
    _type: PhantomData<T>,
}

impl<'a, T: VmAddressRegionEnumeratorType> VmAddressRegionEnumerator<'a, T> {
    /// # Safety
    ///
    /// Until the enumerator is dropped, except between a call to `pause` and the next call to
    /// `resume`, the aspace lock is held and the region lists of `vmar` and of the VMARs below it
    /// are not modified.
    pub(crate) unsafe fn new(
        vmar: &'a VmAddressRegion,
        min_addr: usize,
        max_addr: usize,
        token: &LockToken<'_, VmAspaceLockClass>,
    ) -> Self {
        let itr = region_list::include_or_higher(vmar, token, VAddr(min_addr));
        if T::YIELDS_VMARS {
            // If enumerating vmars and mappings validate that, unless the range is empty, that any
            // vmar is wholly contained within the range. This will not catch all errors in users
            // potentially providing a range that only partially overlaps a vmar, but should catch
            // most accidental misuses.
            // This can only be tested here, and not when walking, since due to how pausing and
            // resuming works we may end up with a `min_addr` that is partially in a vmar, either
            // because the vmars were modified while paused or because we need to resume at a
            // mapping part way into a vmar.
            assert!(itr.get().is_none_or(|first| {
                first.is_mapping()
                    || (first.base().0 >= min_addr
                        && first.base().0.wrapping_add(first.size()) <= max_addr)
            }));
        }
        let itr = Some(Self::unbind_cursor(itr));
        Self {
            state: PauseState::Unpaused,
            min_addr,
            max_addr,
            depth: START_DEPTH,
            vmar,
            itr,
            _type: PhantomData,
        }
    }

    #[inline]
    fn unbind_cursor(itr: Cursor<'_>) -> Cursor<'a> {
        // SAFETY: The enumerator uses `itr` only while not paused, with the aspace lock held and
        // the region list not modified, and clears it in `pause`.
        unsafe { itr.unbind() }
    }

    /// Yield the next region or mapping, or `None` if enumeration has completed. Regions are
    /// yielded in depth-first pre-order. The regions are yielded as references, which are
    /// guaranteed to be valid since the lock is held. It is the caller's responsibility to keep
    /// these references alive, by upgrading to `RefPtr`s or otherwise, if they want to `pause` and
    /// drop the lock.
    pub(crate) fn next<'b>(
        &mut self,
        token: &'b LockToken<'_, VmAspaceLockClass>,
    ) -> Option<(T::Item<'b>, u32)>
    where
        'a: 'b,
    {
        // SAFETY: The returned reference borrows `token`, so it is used only while the aspace lock
        // is held and the region list it is in is not modified.
        unsafe { self.next_unbound(token) }
    }

    /// `next`, with the returned reference not bound to `token`.
    ///
    /// # Safety
    ///
    /// The returned reference is used only while the aspace lock is held and the object it refers
    /// to is live.
    pub(super) unsafe fn next_unbound<'b>(
        &mut self,
        token: &LockToken<'_, VmAspaceLockClass>,
    ) -> Option<(T::Item<'b>, u32)>
    where
        'a: 'b,
    {
        assert!(matches!(self.state, PauseState::Unpaused));
        let mut ret = None;
        while ret.is_none() {
            let Some(itr) = &mut self.itr else {
                break;
            };
            let Some(curr) = itr.get() else {
                break;
            };
            if curr.base().0 >= self.max_addr {
                break;
            }

            itr.next();
            debug_assert!(curr.state_locked(token) == LifeCycleState::Alive);

            curr.common_canary().assert();
            match curr {
                VmAddressRegionOrMapping::Mapping(mapping) => {
                    // If the mapping is entirely before `min_addr`, do not run `on_mapping`.
                    // This can happen when a vmar contains `min_addr` but has mappings entirely
                    // below it. (Base addresses above `max_addr` are strictly handled by the
                    // short-circuit earlier in the loop)
                    if mapping.base().0 < self.min_addr
                        && mapping.base().0.wrapping_add(mapping.size()) <= self.min_addr
                    {
                        // Mapping is out of bounds below our range, do not yield it. We do not
                        // `continue` here, as we must fall through to the ascending logic below
                        // in case this was the last child.
                    } else {
                        ret = Some((T::from_mapping(mapping), self.depth));
                    }
                }
                VmAddressRegionOrMapping::Region(vmar) => {
                    // Yield the vmar if its base is greater than or equal to our min. As we only
                    // yield vmars if requested, and a condition of it being requested is that
                    // iteration not be happening part way into a VMAR, we can simply validate
                    // that the vmar base is >= our min.
                    if T::YIELDS_VMARS && vmar.base().0 >= self.min_addr {
                        ret = T::from_vmar(vmar).map(|item| (item, self.depth));
                    }
                    if !region_list::is_empty(vmar, token) {
                        // If the sub-VMAR is not empty, iterate through its children.
                        // Optimization: skip children entirely below `min_addr`.
                        let next_itr =
                            region_list::include_or_higher(vmar, token, VAddr(self.min_addr));
                        if next_itr.is_valid() {
                            self.itr = Some(Self::unbind_cursor(next_itr));
                            self.depth += 1;
                            continue;
                        }
                        // If `next_itr` is invalid, all children in this sub-VMAR are < `min_addr`.
                        // We do not descend. Instead, we fall through to the ascending logic
                        // below to correctly advance to the next sibling of this sub-VMAR.
                    }
                }
            }
            if self.depth > START_DEPTH && !itr.is_valid() {
                // If we are at a depth greater than the minimum, and have reached
                // the end of a sub-VMAR range, we ascend and continue iteration.
                // SAFETY: `curr`'s parent is the VMAR whose region list `itr` points into, which is
                // live while the aspace lock is held and no region list is modified.
                let curr_parent = unsafe { curr.parent_ref_locked(token) };
                // SAFETY: `curr` has a parent, as above.
                let mut up = unsafe { curr_parent.unwrap_unchecked() }.parent_locked(token);
                self.depth -= 1;
                loop {
                    // SAFETY: `up` is the VMAR `self.depth - START_DEPTH` levels below `self.vmar`
                    // on the path to `curr`: on entry it is the parent of the VMAR whose region
                    // list `itr` points into, and each step to `up`'s parent decrements
                    // `self.depth`, which is above `START_DEPTH`. So `up` is `self.vmar` or a VMAR
                    // in its tree, which is live because the aspace lock is held and no region
                    // list is modified.
                    let up_vmar = unsafe { up.unwrap_unchecked().as_ref() };
                    let next_itr = region_list::upper_bound_locked(up_vmar, token, curr.base());
                    if next_itr.is_valid() {
                        self.itr = Some(Self::unbind_cursor(next_itr));
                        break;
                    }
                    if self.depth == START_DEPTH {
                        // If we have reached the end after ascending all the way up,
                        // break out of the loop.
                        self.itr = None;
                        return ret;
                    }
                    up = up_vmar.parent_locked(token);
                    self.depth -= 1;
                }
            }
        }
        ret
    }

    /// Pause enumeration. Until `resume` is called `next` may not be called, but the vmar lock is
    /// permitted to be dropped, and the vmar is permitted to be modified.
    pub(crate) fn pause(&mut self, _token: &LockToken<'_, VmAspaceLockClass>) {
        // Save information of the next iteration we should return.
        // `itr` is cleared, as the region list may change while paused.
        let itr = self.itr.take();
        let new_state = if let Some(next) = itr.as_ref().and_then(Cursor::get) {
            // `itr` represents the next item that needs to be checked / yielded after we
            // `resume`. We know that everything up to `itr` has already been yielded, so if `itr`
            // becomes invalid between now and `resume` we know the following:
            //  1. If `itr` was a mapping then this mapping was deleted and, even if a new mapping
            //     has since replaced it, we are not obligated to return it. As such we can skip it.
            //  2. If `itr` was a mapping then since a mapping cannot cross a vmar boundary,
            //     resuming at the end of the mapping will not result in missing a vmar.
            //  3. If `itr` was a vmar then it being deleted can only happen if the entire subtree
            //     was deleted. Now, similar to a mapping, even if a new subtree was created we are
            //     not obligated to return it and can skip.
            // For these reasons we prepare our `next_offset` to be the end of the current `itr`,
            // meaning that if `itr` is deleted the next object to be yielded is whatever starts
            // after it.
            PauseState::Paused {
                next_offset: next.base().0.wrapping_add(next.size()),
                region_or_mapping: next.to_owned(),
            }
        } else {
            PauseState::PausedAtEnd
        };
        // Assert after building `new_state`, so the compiler knows the old state needs no drop.
        assert!(matches!(self.state, PauseState::Unpaused));
        self.state = new_state;
    }

    /// Resume enumeration allowing `next` to be called again.
    ///
    /// # Safety
    ///
    /// Until the enumerator is dropped or paused again, the aspace lock is held and the region
    /// lists of the enumerated VMAR and of the VMARs below it are not modified.
    pub(crate) unsafe fn resume(&mut self, token: &LockToken<'_, VmAspaceLockClass>) {
        match mem::replace(&mut self.state, PauseState::Unpaused) {
            PauseState::Unpaused => panic!("resume called while not paused"),
            PauseState::Paused { next_offset, region_or_mapping } => {
                let itr = if !region_or_mapping.is_alive_locked(token) {
                    hint::cold_path();
                    // Generate a new iterator that starts at the right offset, but back at the
                    // top. The next call to `next()` will walk back down if necessary to find the
                    // next mapping / VMAR.
                    self.min_addr = next_offset;
                    self.depth = START_DEPTH;
                    region_list::include_or_higher(self.vmar, token, VAddr(self.min_addr))
                } else {
                    let parent = region_or_mapping
                        .parent_locked(token)
                        .expect("live child must have parent");
                    // SAFETY: `parent` is the parent of the live `region_or_mapping`, and the
                    // aspace lock is held.
                    let parent = unsafe { parent.as_ref() };
                    let itr =
                        region_list::include_or_higher(parent, token, region_or_mapping.base());
                    debug_assert!(
                        itr.get().is_some_and(|r| ptr::eq(r.common(), &*region_or_mapping))
                    );
                    itr
                };
                self.itr = Some(Self::unbind_cursor(itr));
                // Free the refptr. Note that the actual destructors of VmAddressRegion and
                // VmMapping objects themselves do very little, so we are safe to potentially
                // invoke the destructor here.
                drop(region_or_mapping);
            }
            PauseState::PausedAtEnd => {
                // There was no refptr, meaning `itr` was already not valid, and should still not be
                // valid.
                assert!(self.itr.is_none());
            }
        }
    }
}
