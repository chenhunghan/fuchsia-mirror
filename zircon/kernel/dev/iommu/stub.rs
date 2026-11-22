// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Stub IOMMU, Bus Transaction Initiator (`StubBti`), and Pinned Memory Token (`StubPmt`)
//! implementations used when no hardware IOMMU is present or enabled.

use core::pin::Pin;

use fbl::{
    DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    Recyclable, RefPtr, TrackingSize, pin_make_ref_counted,
};
use ksync::{KCell, KMutex, LockToken, PhantomMutex};
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

use crate::iommu::{
    ALL_PERM_FLAGS, BtiPageLeakReason, QueryAddressResult, RequireContiguousMapping,
    print_quarantine_warning,
};
use crate::ksync_kernel::TrackedSpinlock;
use crate::vm::pinned_vm_object::PinnedVmObject;

/// Stub IOMMU controller that performs no hardware address translation.
#[fbl::ref_counted]
#[pin_data]
#[derive(Recyclable)]
#[repr(C)]
pub struct StubIommu {}

impl StubIommu {
    /// Creates a new [`StubIommu`] instance.
    pub(super) fn create() -> Result<RefPtr<Self>, Status> {
        pin_make_ref_counted!(Self {}).map_err(|_| Status::NO_MEMORY)
    }

    /// Creates a new [`StubBti`] bound to this [`StubIommu`].
    pub(super) fn create_bti(&self, bti_id: u64) -> Result<RefPtr<StubBti>, Status> {
        StubBti::create(RefPtr::from_ref(self), bti_id)
    }
}

crate::ksync_kernel::declare_singleton_spinlock!(GlobalOrphanLock);

// `ksync::KOnceCell::get_or_pin_init` returns `&mut T` rather than `Pin<&mut T>`, whereas
// `ORPHAN_LIST` requires `Pin<&mut DoublyLinkedList>` mutation (`push_front` and `erase`) guarded by
// `GlobalOrphanLock`.
struct GlobalOrphanList {
    initialized: core::cell::UnsafeCell<bool>,
    inner: core::cell::UnsafeCell<
        core::mem::MaybeUninit<DoublyLinkedList<RefPtr<StubBti>, DefaultObjectTag, TrackingSize>>,
    >,
}

// SAFETY: Access to `GlobalOrphanList` is synchronized via `GlobalOrphanLock`.
unsafe impl Sync for GlobalOrphanList {}

impl GlobalOrphanList {
    const fn new() -> Self {
        Self {
            initialized: core::cell::UnsafeCell::new(false),
            inner: core::cell::UnsafeCell::new(core::mem::MaybeUninit::uninit()),
        }
    }

    fn get_mut<'a>(
        &'a self,
        _token: &'a mut LockToken<'_, GlobalOrphanLock>,
    ) -> Pin<&'a mut DoublyLinkedList<RefPtr<StubBti>, DefaultObjectTag, TrackingSize>> {
        // SAFETY: Synchronized by the mutable `LockToken` for `GlobalOrphanLock`, which stays
        // borrowed for as long as the returned reference is live, so that two calls cannot hand
        // out two live `&mut` references to the list. The list lives in a static and is
        // initialized in place, so it is never moved.
        unsafe {
            let init = &mut *self.initialized.get();
            let slot = &mut *self.inner.get();
            if !*init {
                let Ok(()) =
                    pin_init::PinInit::__pinned_init(DoublyLinkedList::new(), slot.as_mut_ptr());
                *init = true;
            }
            Pin::new_unchecked(slot.assume_init_mut())
        }
    }
}

// The "global orphan" list holds a list of BTIs which have had their last user-mode handle closed,
// but which still had actively pinned memory (either quarantined or not) when they were closed.
// Even though all user-mode references to the BTI are gone, they maintain their presence in this
// global list for diagnostic purposes for as long as they have active or quarantined PMTs.
//
// If all of the active PMTs held by an orphaned BTI are formally unpinned via a call to
// `zx_pmt_unpin` before being closed, the BTI can be removed from the orphan list and finally
// destroyed.
//
// However, if any of the PMTs managed by the BTI are leaked (closed before calling `zx_pmt_unpin`),
// the BTI will effectively be stuck on the orphan list forever. It is not safe to return the pages
// from the leaked PMT to the PMM, and the user no longer has a way to signal to the kernel that it
// has taken control of its hardware via a call to `zx_bti_release_quarantine` since it no longer
// has access to the BTI object anymore.
static ORPHAN_LIST: GlobalOrphanList = GlobalOrphanList::new();

/// Stub Bus Transaction Initiator representing a pass-through physical DMA context.
///
/// The lifecycle of a [`StubBti`] and its [`StubPmt`] children can be complicated due to the
/// requirement to support the pattern where a user obtains a handle to a PMT via a `zx_bti_pin`
/// syscall, and then destroys the last handle to the PMT without first deliberately unpinning the
/// memory in the PMT via a call to `zx_pmt_unpin`.
///
/// See the comments in [`StubPmt`] for more details about the lifecycle of both objects.
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct StubBti {
    #[pin]
    list_node: DoublyLinkedListNode<StubBti>,
    bti_id: u64,
    iommu: RefPtr<StubIommu>,
    // `collection_lock` is always acquired before `GlobalOrphanLock`.
    #[mutex]
    collection_lock: KMutex<TrackedSpinlock>,
    // A cached copy of our orphaned status protected by the collection lock instead of the global
    // orphan lock. Set to `true` once the BTI Dispatcher has run out of references without having
    // all of its active and quarantined PMTs properly purged. Maintaining this cached copy allows
    // us to test to see if we have become orphaned without needing to acquire the global orphan
    // lock. We only need to hold the orphan lock when we are actively mutating the orphan list.
    #[guarded_by(collection_lock)]
    is_orphaned: bool,
    #[pin]
    #[guarded_by(collection_lock)]
    active_pmts: DoublyLinkedList<RefPtr<StubPmt>, DefaultObjectTag, TrackingSize>,
    #[pin]
    #[guarded_by(collection_lock)]
    quarantined_pmts: DoublyLinkedList<RefPtr<StubPmt>, DefaultObjectTag, TrackingSize>,
    #[guarded_by(collection_lock)]
    quarantined_pages: u64,
    #[mutex]
    name_lock: KMutex<TrackedSpinlock>,
    // The user-friendly BTI name. For debug purposes only.
    #[guarded_by(name_lock)]
    name: [u8; ZX_MAX_NAME_LEN],
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for StubBti {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: `StubBti` state is synchronized via `GlobalOrphanLock`, `collection_lock`, and
// `name_lock`.
unsafe impl Send for StubBti {}
// SAFETY: `StubBti` state is synchronized via `GlobalOrphanLock`, `collection_lock`, and
// `name_lock`.
unsafe impl Sync for StubBti {}

impl StubBti {
    /// Creates a new [`StubBti`] associated with `iommu` and `bti_id`.
    pub(super) fn create(iommu: RefPtr<StubIommu>, bti_id: u64) -> Result<RefPtr<Self>, Status> {
        pin_make_ref_counted!(Self {
            list_node: DoublyLinkedListNode::new(),
            bti_id,
            iommu,
            collection_lock <- KMutex::init(),
            is_orphaned: KCell::new(false),
            active_pmts <- KCell::pin_init(DoublyLinkedList::new()),
            quarantined_pmts <- KCell::pin_init(DoublyLinkedList::new()),
            quarantined_pages: KCell::new(0),
            name_lock <- KMutex::init(),
            name: KCell::new([0u8; ZX_MAX_NAME_LEN]),
        })
        .map_err(|_| Status::NO_MEMORY)
    }

    /// Returns the 64-bit hardware BTI ID of this instance.
    #[inline]
    pub(super) const fn bti_id(&self) -> u64 {
        self.bti_id
    }

    /// Copies the BTI's debug name into `out_name`.
    pub(super) fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        ksync::lock!(let guard = self.lock_name_lock());
        *out_name = *guard.name();
    }

    /// Sets the BTI's debug name from `name`.
    pub(super) fn set_name(&self, name: &[u8]) {
        ksync::lock!(let mut guard = self.lock_name_lock());
        let mut buf = [0u8; ZX_MAX_NAME_LEN];
        let nul_len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let copy_len = nul_len.min(ZX_MAX_NAME_LEN - 1);
        buf[..copy_len].copy_from_slice(&name[..copy_len]);
        *guard.as_mut().name_mut() = buf;
    }

    /// Maps `pinned_vmo` into this [`StubBti`] and returns a new [`StubPmt`].
    pub(super) fn map(
        &self,
        pinned_vmo: PinnedVmObject,
        perms: u32,
        req_contig: RequireContiguousMapping,
    ) -> Result<RefPtr<StubPmt>, Status> {
        debug_assert!(pinned_vmo.vmo().is_some());
        let vmo_offset = pinned_vmo.offset();
        let vmo_size = pinned_vmo.size();

        // Validate the arguments before allocating the `StubPmt`, so that an invalid request
        // reports `ZX_ERR_INVALID_ARGS` even when memory is short.
        if !page::is_aligned(vmo_offset as usize) || vmo_size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        if (perms & !ALL_PERM_FLAGS) != 0 || perms == 0 {
            return Err(Status::INVALID_ARGS);
        }

        let vmo = pinned_vmo.vmo().ok_or(Status::INVALID_ARGS)?;

        // If the user requires a continuous mapping, then our pinned VMO has to be contiguous as we
        // have no MMU to perform HW translation for us.
        if req_contig == RequireContiguousMapping::Yes {
            vmo.lookup_contiguous(vmo_offset, vmo_size)?;
        }

        let pmt = StubPmt::create(RefPtr::from_ref(self), pinned_vmo)?;
        // Success: add the new PMT to the active PMT list, transitioning the state from `Initial`
        // to `Active` in the process.
        {
            ksync::lock!(let mut guard = self.lock_collection_lock());
            // SAFETY: `guard` holds `self.collection_lock`, and `pmt` was just created above with
            // `self` as its owner in `StubPmtState::Initial` and has not been linked into any list.
            unsafe { self.add_to_active_list(&mut guard, &pmt) };
        }

        Ok(pmt)
    }

    /// Releases all quarantined PMTs on this BTI, returning their pinned memory to the system.
    pub(super) fn release_quarantine(&self) {
        pin_init::stack_pin_init!(
            let was_quarantined_list =
                DoublyLinkedList::<RefPtr<StubPmt>, DefaultObjectTag, TrackingSize>::new()
        );

        // Move the quarantine list outside of the scope of the collection lock, then release all of
        // the quarantined memory. Explicitly set the state of all of the PMTs to `Released` as we
        // go so that we don't fail any of our bookkeeping asserts when the PMTs are finally
        // destroyed.
        {
            ksync::lock!(let mut guard = self.lock_collection_lock());

            // `BusTransactionInitiatorDispatcher` holds its state lock across both
            // `release_quarantine` and `on_zero_handles`, checking `!zero_handles` before
            // reaching here, so we cannot have transitioned to the orphaned state.
            debug_assert!(!*guard.is_orphaned());

            let mut released_pages: u64 = 0;
            loop {
                let Some(pmt) = guard.as_mut().quarantined_pmts_mut().pop_front() else {
                    break;
                };
                // SAFETY: `pmt` was on this BTI's quarantine list, so `self` owns it, and `guard`
                // holds `self.collection_lock`.
                let mut pmt_inner =
                    unsafe { pmt.guard_collection_lock_mut(guard.as_mut().token_mut()) };
                debug_assert_eq!(*pmt_inner.state(), StubPmtState::Quarantined);
                released_pages += pmt.pages();
                *pmt_inner.state_mut() = StubPmtState::Released;
                was_quarantined_list.as_mut().push_back(pmt);
            }

            debug_assert_eq!(released_pages, *guard.quarantined_pages());
            *guard.as_mut().quarantined_pages_mut() = 0;
        }

        // Now, outside of the lock, walk the list of PMTs returning their VMOs back to the common
        // pool, then letting the PMT release its final reference to its BTI as it destructs.
        //
        // Note that we should never be destroying the final reference to this BTI during this loop
        // as a consequence of destroying the quarantined PMTs. The only entity which is ever
        // allowed to call `release_quarantine` is our BTI dispatcher, meaning that we cannot be
        // orphaned, and the dispatcher should still be holding at least one reference to us.
        while let Some(release_me) = was_quarantined_list.as_mut().pop_front() {
            release_me.release_quarantined_vmo();
        }
    }

    /// Invoked when the owning `BusTransactionInitiatorDispatcher` reaches zero handles.
    pub(super) fn on_dispatcher_zero_handles(&self) {
        let mut quarantined_pmt_count: usize = 0;
        let mut quarantined_page_count: u64 = 0;

        {
            ksync::lock!(let mut guard = self.lock_collection_lock());

            // The only way to become orphaned is for our handle count to hit zero. If we are
            // already flagged as orphaned, then someone has called on-zero-handles twice, which
            // should be impossible.
            debug_assert!(!*guard.is_orphaned());

            // We've hit zero handles, so user-mode no longer has any access to us. If we are still
            // holding PMTs, then we are now "orphaned". Hold an explicit reference to ourselves on
            // the global orphan list, and take a snapshot of the quarantine stats before dropping
            // our lock. If the last reference to us was dropped while we had quarantined PMTs, we
            // need to report an OOPS as user-mode has managed to both leak PMTs, and close their
            // BTI making it impossible for them to signal that they have taken control of their HW
            // using `zx_bti_release_quarantine`.
            if !guard.active_pmts().is_empty() || !guard.quarantined_pmts().is_empty() {
                ksync::lock!(let mut orphan_guard = GlobalOrphanLock::lock());

                ORPHAN_LIST
                    .get_mut(orphan_guard.as_mut().token_mut())
                    .push_front(RefPtr::from_ref(self));
                *guard.as_mut().is_orphaned_mut() = true;
                quarantined_pmt_count = guard.quarantined_pmts().len();
                quarantined_page_count = *guard.quarantined_pages();
            }
        }

        // Right now, we cannot print quarantine warnings with any spinlocks held. The primary
        // reason is that the print routine attempts to access the current thread's name via the
        // `ThreadDispatcher`, which needs to hold its mutex in order to determine if it is still
        // attached to its kernel thread. Until thread name storage is protected by a spinlock, we
        // need to avoid holding any spinlocks when reporting quarantine warnings.
        if quarantined_pmt_count > 0 {
            let mut bti_name = [0u8; ZX_MAX_NAME_LEN];
            self.get_name(&mut bti_name);
            // Note: `print_quarantine_warning` takes `(total_leaked_pages, total_leaked_vmos)`.
            print_quarantine_warning(
                self.bti_id,
                &bti_name,
                BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts,
                quarantined_page_count,
                quarantined_pmt_count,
            );
        } else {
            debug_assert_eq!(quarantined_page_count, 0);
        }
    }

    /// Returns the minimum contiguity guaranteed by this BTI.
    #[inline]
    pub(super) const fn minimum_contiguity(&self) -> u64 {
        page::SIZE as u64
    }

    /// Returns the virtual address space size managed by this BTI.
    #[inline]
    pub(super) const fn aspace_size(&self) -> u64 {
        u64::MAX
    }

    /// Returns the total number of active and quarantined PMTs on this BTI.
    pub(super) fn pmo_count(&self) -> u64 {
        ksync::lock!(let guard = self.lock_collection_lock());
        (guard.active_pmts().len() + guard.quarantined_pmts().len()) as u64
    }

    /// Returns the number of quarantined PMTs on this BTI.
    pub(super) fn quarantine_count(&self) -> usize {
        ksync::lock!(let guard = self.lock_collection_lock());
        guard.quarantined_pmts().len()
    }

    /// Indicates that this BTI has encountered a "fault" such as a protocol violation like a leaked
    /// PMT, or a HW translation fault. The BTI will refuse new pin operations until the driver
    /// takes control of its hardware, and calls `release_quarantine` on the BTI to indicate that
    /// the fault has been handled.
    pub(super) fn in_fault_state(&self) -> bool {
        ksync::lock!(let guard = self.lock_collection_lock());
        !guard.quarantined_pmts().is_empty()
    }

    /// Drains an orphaned [`StubBti`]'s quarantine list and unlinks it from `ORPHAN_LIST` at the
    /// end of a kernel unit test so test-induced leaks do not persist across the test suite.
    #[cfg(ktest)]
    pub(super) fn reclaim_orphaned_for_test(&self) {
        pin_init::stack_pin_init!(
            let was_quarantined_list =
                DoublyLinkedList::<RefPtr<StubPmt>, DefaultObjectTag, TrackingSize>::new()
        );
        let mut removed_bti: Option<RefPtr<StubBti>> = None;
        {
            ksync::lock!(let mut guard = self.lock_collection_lock());
            debug_assert!(*guard.is_orphaned());
            if !*guard.is_orphaned() {
                return;
            }
            while let Some(pmt) = guard.as_mut().quarantined_pmts_mut().pop_front() {
                // SAFETY: `pmt` was on this BTI's quarantine list, so `self` owns it, and `guard`
                // holds `self.collection_lock`.
                let mut pmt_inner =
                    unsafe { pmt.guard_collection_lock_mut(guard.as_mut().token_mut()) };
                *pmt_inner.state_mut() = StubPmtState::Released;
                was_quarantined_list.as_mut().push_back(pmt);
            }
            *guard.as_mut().quarantined_pages_mut() = 0;
            if guard.active_pmts().is_empty() {
                ksync::lock!(let mut orphan_guard = GlobalOrphanLock::lock());
                debug_assert!(self.list_node.in_container());
                if self.list_node.in_container() {
                    let orphan_list = ORPHAN_LIST.get_mut(orphan_guard.as_mut().token_mut());
                    // SAFETY: `GlobalOrphanLock` is held and `self.list_node.in_container()`
                    // verified that `self` is linked in `ORPHAN_LIST` (the sole list using
                    // `StubBti::list_node`).
                    removed_bti = unsafe { orphan_list.erase(self) };
                }
            }
        }
        while let Some(release_me) = was_quarantined_list.as_mut().pop_front() {
            release_me.release_quarantined_vmo();
        }
        drop(removed_bti);
    }

    /// Links a newly created [`StubPmt`] into `self.active_pmts` and transitions it to
    /// [`StubPmtState::Active`].
    ///
    /// # Safety
    ///
    /// `guard` must hold `self.collection_lock`, and `pmt` must have `self` as its `owner`, be in
    /// [`StubPmtState::Initial`], and not yet be linked in any list.
    unsafe fn add_to_active_list(
        &self,
        guard: &mut Pin<&mut StubBtiCollectionLockGuard<'_>>,
        pmt: &RefPtr<StubPmt>,
    ) {
        // Note that we should not have to check if we have been orphaned by now. Even if we did not
        // hold a reference to our `StubBti`, the only caller of `map` is a `BtiDispatcher` via the
        // `zx_bti_pin` syscall. The dispatcher should still have at least one active handle in
        // flight (meaning that we cannot have transitioned to the orphaned state), and the
        // dispatcher stays alive on the stack during the call.
        debug_assert!(!*guard.is_orphaned());
        debug_assert!(!pmt.in_container());
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        debug_assert_eq!(
            *unsafe { pmt.guard_collection_lock(guard.token()) }.state(),
            StubPmtState::Initial
        );

        guard.as_mut().active_pmts_mut().push_back(pmt.clone());
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        *unsafe { pmt.guard_collection_lock_mut(guard.as_mut().token_mut()) }.state_mut() =
            StubPmtState::Active;
    }

    /// Removes an active [`StubPmt`] from `self.active_pmts` and transitions it to
    /// [`StubPmtState::Released`].
    ///
    /// Note: Something at a higher level (e.g. at the Dispatcher level) holds a reference to `pmt`,
    /// and the caller drops the returned `RefPtr`s outside of `collection_lock` so heap memory is
    /// never freed while holding a spinlock.
    ///
    /// # Safety
    ///
    /// `guard` must hold `self.collection_lock`, and `pmt` must have `self` as its `owner` and be
    /// in [`StubPmtState::Active`] (and therefore linked in `self.active_pmts`).
    unsafe fn remove_from_active_list(
        &self,
        guard: &mut Pin<&mut StubBtiCollectionLockGuard<'_>>,
        pmt: &StubPmt,
    ) -> (Option<RefPtr<StubPmt>>, Option<RefPtr<StubBti>>) {
        debug_assert!(pmt.in_container());
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        debug_assert_eq!(
            *unsafe { pmt.guard_collection_lock(guard.token()) }.state(),
            StubPmtState::Active
        );

        // SAFETY: By caller precondition `pmt` is owned by `self` and in `StubPmtState::Active`,
        // and every `Active` PMT owned by `self` is linked in `self.active_pmts`.
        let removed_pmt = unsafe { guard.as_mut().active_pmts_mut().erase(pmt) };
        debug_assert!(removed_pmt.is_some());
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        *unsafe { pmt.guard_collection_lock_mut(guard.as_mut().token_mut()) }.state_mut() =
            StubPmtState::Released;

        // If we are no longer managing _any_ PMTs (quarantined or otherwise) and we are currently
        // on the orphan list, we can now remove ourselves from the list. We should not need to
        // worry about our ref-count hitting zero here, as our caller must be holding a reference to
        // us and drops `removed_bti` outside the spinlock.
        let mut removed_bti = None;
        if *guard.is_orphaned()
            && guard.active_pmts().is_empty()
            && guard.quarantined_pmts().is_empty()
        {
            ksync::lock!(let mut orphan_guard = GlobalOrphanLock::lock());
            let orphan_list = ORPHAN_LIST.get_mut(orphan_guard.as_mut().token_mut());
            assert!(self.list_node.in_container());
            // SAFETY: `GlobalOrphanLock` is held and `assert!(self.list_node.in_container())`
            // verified that `self` is linked in `ORPHAN_LIST`, the sole list using
            // `StubBti::list_node`.
            removed_bti = unsafe { orphan_list.erase(self) };

            // Note: we deliberately do not clear the `is_orphaned` flag here. A BTI can only become
            // orphaned exactly once, if it is still holding PMTs when it hits zero-handles. After
            // that, the only way off of the list is here, when the last PMT is formally unpinned
            // instead of being leaked. When that happens, we should be on the way to destruction.
            // The orphan list no longer holds a reference to us, nor does any Dispatcher. The final
            // references to us only exist in PMTs which have now been unpinned, and will go away
            // when those PMTs are closed.
            //
            // Keeping the flag set even though we are not in the orphan list container allows us to
            // continue to assert that no one has accidentally called on-zero-handles more than once
            // in the (hopefully) brief period of time between now and destruction.
        }

        (removed_pmt, removed_bti)
    }

    /// Moves an active [`StubPmt`] from `self.active_pmts` to `self.quarantined_pmts` and
    /// transitions it to [`StubPmtState::Quarantined`].
    ///
    /// # Safety
    ///
    /// `guard` must hold `self.collection_lock`, and `pmt` must have `self` as its `owner` and be
    /// in [`StubPmtState::Active`] (and therefore linked in `self.active_pmts`).
    unsafe fn quarantine_from_active_list(
        &self,
        guard: &mut Pin<&mut StubBtiCollectionLockGuard<'_>>,
        pmt: &StubPmt,
    ) {
        debug_assert!(pmt.in_container());
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        debug_assert_eq!(
            *unsafe { pmt.guard_collection_lock(guard.token()) }.state(),
            StubPmtState::Active
        );

        *guard.as_mut().quarantined_pages_mut() += pmt.pages();
        // SAFETY: By caller precondition `pmt` has `self` as its owner, and `guard` holds
        // `self.collection_lock`.
        *unsafe { pmt.guard_collection_lock_mut(guard.as_mut().token_mut()) }.state_mut() =
            StubPmtState::Quarantined;
        // SAFETY: By caller precondition `pmt` is owned by `self` and in `StubPmtState::Active`,
        // and every `Active` PMT owned by `self` is linked in `self.active_pmts`.
        let pmt_ref = unsafe { guard.as_mut().active_pmts_mut().erase(pmt) }
            .expect("active PMT must be in active_pmts");
        guard.as_mut().quarantined_pmts_mut().push_back(pmt_ref);
    }
}

#[pinned_drop]
impl PinnedDrop for StubBti {
    fn drop(self: Pin<&mut Self>) {
        // No locks are taken here: a destructor has exclusive access to its object (`ref_count ==
        // 0`), so the guarded lists are inspected directly without disabling interrupts.
        //
        // SAFETY: We are in `PinnedDrop::drop`, so nothing else can reference `self`, and none of
        // the pinned fields are moved.
        let this = unsafe { self.get_unchecked_mut() };
        assert!(this.active_pmts.get_inner_mut().is_empty());
        assert!(this.quarantined_pmts.get_inner_mut().is_empty());
    }
}

/// Lifecycle state of a [`StubPmt`] instance.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StubPmtState {
    /// PMT has been created, but not successfully mapped and added to its owner BTI.
    Initial = 0,
    /// PMT has been created, mapped, and added to its owner BTI.
    Active = 1,
    /// PMT has been explicitly unpinned. Its pinned VMO is released and it is not on any BTI lists.
    Released = 2,
    /// PMT has been quarantined. The VMO is still pinned and it is a member of its owning BTI's
    /// quarantine list.
    Quarantined = 3,
}

/// Stub Pinned Memory Token representing a pinned VMO range in a [`StubBti`].
///
/// The lifecycle of a [`StubPmt`] can be complicated due to the requirement to support the pattern
/// where a user obtains a handle to a PMT via a `zx_bti_pin` syscall, and then destroys the last
/// handle to the PMT without first deliberately unpinning the memory in the PMT via a call to
/// `zx_pmt_unpin`.
///
/// ```text
///  +------------------------+
///  | User calls zx_bti_pin  |
///  +-----------+------------+
///              |
///              v
///     +--------+---------+
///     | StubBti::map     |
///     | creates StubPmt  |
///     +--------+---------+
///              |
///              v
///   +----------+----------+
///   | StubPmt linked into |
///   | StubBti's           |
///   | active_pmts list    |
///   +----------+----------+
///              |
///              v
///     +--------+---------+
///     | StubBti::map     |
///     | returns StubPmt  |
///     | to Dispatcher    |
///     +--------+---------+
///              |
///              +---------------------------+
///              |                           |
///              v                           v
/// +------------+------------+   +----------+---------------+
/// | Normal Path             |   | Quarantine Path          |
/// | User calls zx_pmt_unpin |   | User closes last         |
/// +------------+------------+   | handle without unpinning |
///              |                +----------+---------------+
///              v                           |
/// +------------+------------+              v
/// | StubPmt unpins VMO and  |   +----------+---------------+
/// | removes self from       |   | Dispatcher calls         |
/// | active_pmts list        |   | StubPmt's                |
/// +------------+------------+   | on_dispatcher_zero_handles|
///              |                +----------+---------------+
///              v                           |
/// +------------+------------+              v
/// | User closes last        |   +----------+---------------+
/// | handle to PMT           |   | StubPmt moves itself     |
/// +------------+------------+   | from active_pmts list    |
///              |                | to quarantined_pmts list |
///              v                +----------+---------------+
/// +------------+------------+              |
/// | Dispatcher calls        |              v
/// | StubPmt's               |   +----------+---------------+
/// | on_dispatcher_zero_handles| | User calls               |
/// | (Now a No-Op)           |   | zx_bti_release_quarantine|
/// +------------+------------+   +----------+---------------+
///              |                           |
///              |                           v
///              |                +----------+---------------+
///              |                | StubBti empties its      |
///              |                | quarantined_pmts list,   |
///              |                | unpinning each PMT's VMO |
///              |                | and releasing its RefPtr |
///              |                +----------+---------------+
///              |                           |
///              +-------------+-------------+
///                            |
///                            v
///               +------------+------------+
///               | StubPmt is finally      |
///               | destroyed when its last |
///               | RefPtr is released.     |
///               +-------------------------+
/// ```
///
/// The other wrinkle in this flow happens when all handles to the BTI are closed while there are
/// still PMTs in flight (either active, or quarantined). The BTI cannot simply destruct at this
/// point in time, because the PMTs still exist. Even if the BTI did not keep `RefPtr`s to the PMTs
/// on its lists, the PMTs would still need to exist and be maintaining their VMO's pinned state.
///
/// So, when the BTI's dispatcher hits zero handles, it calls `StubBti::on_dispatcher_zero_handles`
/// to let the BTI know that it can no longer be referenced from user-mode. If there are no active
/// or quarantined PMTs, the BTI does nothing and is destroyed as soon as the dispatcher drops its
/// `RefPtr` to the BTI.
///
/// If there are any active or quarantined PMTs on the BTI's lists, however, the BTI becomes
/// "orphaned" and adds itself to a global list of orphaned BTIs (`ORPHAN_LIST`), keeping the BTI
/// alive even after its dispatcher drops the reference it was holding.
///
/// If there are quarantined PMTs in the BTI when it becomes orphaned, or if an active PMT becomes
/// quarantined while the BTI is orphaned, a `DRIVER OOPS` warning will be logged as those PMTs
/// have now been permanently leaked, along with their pinned memory. There is no longer any way
/// for user-mode to regain control of the hardware or call `zx_bti_release_quarantine` to release
/// the pages as the BTI handles have all been closed.
///
/// On the other hand, if there are no quarantined PMTs, and the last of the active PMTs is
/// properly unpinned via a call to `zx_pmt_unpin`, the BTI is now finally completely empty, and can
/// be removed from the global orphan list allowing it to finally be destroyed.
// TODO(johngro): Right now we depend on the upper level dispatcher's lock to serialize access to
// this object's user-mode facing API. Consider creating an explicit lock at this level so we can
// make more effective use of static annotations here.
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct StubPmt {
    #[pin]
    list_node: DoublyLinkedListNode<StubPmt>,
    // Notes about `StubPmt` lifecycles and the cyclical reference from a `StubPmt` to its owning
    // `StubBti` held here.
    //
    // `StubPmt`s are always created as a child of a `StubBti`, and during construction, they take a
    // reference to their `StubBti` owner (held here). This reference is deliberately immutable, and
    // is only ever released when the `StubPmt` instance destructs.
    //
    // During operation, there are up to two references in the system which keep the `StubPmt`
    // reference alive. They are:
    //
    // 1) A reference held by the `StubBti` from either the `StubBti`'s active or quarantined PMT
    //    list.
    // 2) A reference held by the PMT dispatcher (`PinnedMemoryTokenDispatcher`).
    //
    // When the PMT transitions to the `Quarantined` state, the `StubBti` holds a reference to the
    // PMT in its quarantine list while the PMT holds a strong reference to its `StubBti` owner,
    // creating a cyclical reference which keeps both objects alive until the quarantine is
    // released.
    owner: RefPtr<StubBti>,
    // Cached from `pinned_vmo.size()` at creation; remains valid after `pinned_vmo` is released.
    size: u64,
    #[mutex(StubBtiCollectionLockClass)]
    collection_lock: KMutex<PhantomMutex>,
    #[guarded_by(collection_lock)]
    pinned_vmo: Option<PinnedVmObject>,
    #[guarded_by(collection_lock)]
    state: StubPmtState,
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for StubPmt {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: `StubPmt` state is synchronized via `StubBti::collection_lock` and atomic `RefCounted`.
unsafe impl Send for StubPmt {}
// SAFETY: `StubPmt` state is synchronized via `StubBti::collection_lock` and atomic `RefCounted`.
unsafe impl Sync for StubPmt {}

impl StubPmt {
    /// Creates a new [`StubPmt`] in [`StubPmtState::Initial`] owned by `owner`.
    fn create(owner: RefPtr<StubBti>, pinned_vmo: PinnedVmObject) -> Result<RefPtr<Self>, Status> {
        let size = pinned_vmo.size();
        pin_make_ref_counted!(Self {
            list_node: DoublyLinkedListNode::new(),
            owner,
            size,
            collection_lock: KMutex::new(PhantomMutex),
            pinned_vmo: KCell::new(Some(pinned_vmo)),
            state: KCell::new(StubPmtState::Initial),
        })
        .map_err(|_| Status::NO_MEMORY)
    }

    /// Returns the size in bytes of the pinned VMO region (cached at creation; remains valid after
    /// release).
    #[inline]
    pub(super) const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the number of pinned pages in this PMT (cached at creation; remains valid after
    /// release).
    #[inline]
    pub(super) const fn pages(&self) -> u64 {
        self.size / (page::SIZE as u64)
    }

    /// Returns `true` if this PMT is currently linked in an owner BTI list.
    #[inline]
    fn in_container(&self) -> bool {
        self.list_node.in_container()
    }

    /// Queries the physical address and contiguous size for `query_offset` and `query_size`.
    pub(super) fn query_address(
        &self,
        query_offset: u64,
        query_size: usize,
    ) -> Result<QueryAddressResult, Status> {
        // Basic parameter validation. The offset into the VMO we are querying must be page
        // aligned, and the size may not be zero.
        if !page::is_aligned(query_offset as usize) || query_size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // If we are no longer active, then we have no pinned+mapped addresses to report. Extract
        // the `VmObject` reference and offset under `collection_lock` so we can call
        // `lookup_contiguous` outside the spinlock (`VmObject::lookup_contiguous` acquires a
        // `VmObject` mutex).
        let (vmo, vmo_offset) = {
            ksync::lock!(let guard = self.owner.lock_collection_lock());
            // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
            // `self`.
            let inner = unsafe { self.guard_collection_lock(guard.token()) };
            if *inner.state() != StubPmtState::Active {
                return Err(Status::BAD_STATE);
            }
            let pinned = inner.pinned_vmo().as_ref().expect("active PMT must have pinned_vmo");
            // The query is relative to what this PMT pinned, which may be a sub-range of the VMO.
            // `lookup_contiguous` checks against the whole VMO, so the bounds of the pin are
            // checked here, before the offset is rebased or the size rounded: otherwise a query
            // could reach pages of the VMO this PMT never pinned, or wrap around.
            let pinned_size = pinned.size();
            if query_offset >= pinned_size || (query_size as u64) > (pinned_size - query_offset) {
                return Err(Status::OUT_OF_RANGE);
            }
            let vmo = pinned.vmo().expect("active PMT must have VMO").clone();
            (vmo, pinned.offset())
        };

        // The user wants to know the device's base virtual address for the continuous range
        // `[query_offset, query_offset + query_size)` in the pinned VMO. Start by attempting to
        // find a continuous region which matches the user's query size. If this fails because the
        // arguments are completely invalid, or because the query goes outside of the possible range
        // of the pinned VMO (even if it was already completely continuous), then fail out
        // immediately.
        //
        // Otherwise, try again, but this time query a region starting at the user's offset, but
        // which is only a page long (the minimum possible continuous region).
        let query_offset = query_offset + vmo_offset;
        let mut query_size = page::round_up(query_size);
        for attempt in 0..2 {
            match vmo.lookup_contiguous(query_offset, query_size as u64) {
                Ok(paddr) => {
                    let device_vaddr = u64::from(paddr);
                    debug_assert_ne!(device_vaddr, crate::iommu::INVALID_PADDR);
                    return Ok(QueryAddressResult { device_vaddr, size: query_size });
                }
                Err(status) => {
                    // Tolerate errors other than INVALID_ARGS or OUT_OF_RANGE only if this is the
                    // second (and final) attempt.
                    if status == Status::INVALID_ARGS
                        || status == Status::OUT_OF_RANGE
                        || attempt != 0
                    {
                        return Err(status);
                    }
                }
            }

            // We shouldn't be here after our first attempt. The second attempt must always either
            // succeed or fail. If this is our first attempt, reduce our query size to just one
            // page.
            debug_assert_eq!(attempt, 0);
            query_size = page::SIZE;
        }

        // We should never get here. If we succeeded on our first or second attempt, we would have
        // already returned, and if we received _any_ errors on our second attempt, we should have
        // already bailed out.
        unreachable!("Second attempt in query_address always returns");
    }

    /// Explicitly unpins and releases the memory held by this PMT.
    pub(super) fn release_pinned_memory(&self) {
        let mut removed_from_active: Option<RefPtr<StubPmt>> = None;
        let mut removed_bti_from_orphans: Option<RefPtr<StubBti>> = None;

        // If we've not already been explicitly unpinned, move our pinned VMO outside of the locked
        // scope so that the underlying VMO is unpinned and our reference to it is deleted as the
        // local `PinnedVmObject` goes out of scope.
        let maybe_destroy = {
            ksync::lock!(let mut guard = self.owner.lock_collection_lock());
            // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
            // `self`.
            let state = *unsafe { self.guard_collection_lock(guard.token()) }.state();
            if state == StubPmtState::Active {
                debug_assert!(
                    // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock
                    // guarding `self`.
                    unsafe { self.guard_collection_lock(guard.token()) }
                        .pinned_vmo()
                        .as_ref()
                        .is_some_and(|p| p.vmo().is_some())
                );
                // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
                // `self`.
                let taken_vmo =
                    unsafe { self.guard_collection_lock_mut(guard.as_mut().token_mut()) }
                        .pinned_vmo_mut()
                        .take();

                // It should be safe to remove ourselves from our BTI's active collection and drop
                // the reference it held. We are currently being kept alive by the reference held by
                // our PMT dispatcher.
                //
                // SAFETY: `guard` holds `self.owner.collection_lock`, `self.owner` owns `self`,
                // and `state == StubPmtState::Active` was verified above.
                let (removed_pmt, removed_bti) =
                    unsafe { self.owner.remove_from_active_list(&mut guard, self) };
                removed_from_active = removed_pmt;
                removed_bti_from_orphans = removed_bti;
                taken_vmo
            } else {
                // If we are no longer active, then we should be able to assert that we are in the
                // `Released` state. It should be impossible for us to be in any other state,
                // including the `Quarantined` state.
                //
                // Even if a `zx_pmt_unpin` operation were racing with a `zx_handle_close`
                // operation, this should be impossible.
                //
                // Unlike most syscalls which operate on handles, `zx_pmt_unpin` does not just
                // validate the handle and obtain a reference to the Dispatcher, it actually removes
                // the handle from the handle table and takes ownership of the `HandleOwner`. It is
                // this `HandleOwner` who holds the handle reference on the Dispatcher, and the
                // Dispatcher cannot reach zero handles while it is still living. The only way for a
                // PMT to become quarantined is for it to reach `on_zero_handles` before becoming
                // unpinned.
                //
                // So, consider the following two cases.
                //
                // Case 1: The two operations are racing and using the same handle value, `H`, which
                //         is the final handle to the object.
                //
                // One of these operations wins the race and removes `H` from the handle table, and
                // the other operation fails to find `H` in the table. If the unpin wins, then it
                // keeps the dispatcher above a zero-handle count until after the unpin is
                // completed, meaning that it cannot hit zero handles and cannot become quarantined.
                // If the handle close wins, then the unpin operation will not find the handle in
                // the handle table, and `release_pinned_memory` will never be called. Meanwhile,
                // the final handle will close and the PMT will be quarantined.
                //
                // Case 2: The two operations are racing and using different handle values,
                //         `H_close` and `H_unpin`.
                //
                // It really does not matter who wins the race in this case. `H_unpin` is going to
                // keep the `HandleOwner` alive until the unpin completes, ensuring that the object
                // cannot hit zero-handles (and therefore cannot become quarantined) before the
                // unpin is finished. The outcome of the race only determines who ends up calling
                // `on_zero_handles` on the dispatcher: the unpin operation if the handle close
                // operation finishes first, or the handle close operation if the unpin finishes
                // first.
                debug_assert_eq!(state, StubPmtState::Released);
                debug_assert!(!self.in_container());
                // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
                // `self`.
                debug_assert!(
                    unsafe { self.guard_collection_lock(guard.token()) }.pinned_vmo().is_none()
                );
                None
            }
        };

        // Looks like we transitioned from Active -> Released. Release our pinned VMO reference and
        // any removed list references outside of the spinlock (`PinnedVmObject::drop` will acquire
        // the VMO mutex).
        drop(maybe_destroy);
        drop(removed_from_active);
        drop(removed_bti_from_orphans);
    }

    /// Invoked when the last user handle to the `PinnedMemoryTokenDispatcher` is closed.
    pub(super) fn on_dispatcher_zero_handles(&self) {
        // Our dispatcher has hit zero handles, which means user-mode is finished with us. If we are
        // still active (and have not been explicitly released), then we need to add ourselves to
        // our owner's quarantine list.
        let mut report_bti_quarantine = false;
        let mut quarantined_pmt_count: usize = 0;
        let mut quarantined_page_count: u64 = 0;

        {
            ksync::lock!(let mut guard = self.owner.lock_collection_lock());
            // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
            // `self`.
            let state = *unsafe { self.guard_collection_lock(guard.token()) }.state();
            debug_assert_ne!(state, StubPmtState::Initial);
            if state == StubPmtState::Active {
                debug_assert!(self.in_container());
                debug_assert!(
                    // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock
                    // guarding `self`.
                    unsafe { self.guard_collection_lock(guard.token()) }
                        .pinned_vmo()
                        .as_ref()
                        .is_some_and(|p| p.vmo().is_some())
                );

                // If our BTI has been orphaned by user-mode, and this is the first PMT being
                // leaked, then we need to report a BTI quarantine event in addition to a PMT
                // quarantine event.
                report_bti_quarantine = *guard.is_orphaned() && guard.quarantined_pmts().is_empty();

                // Move ourselves from our owner's active list to its quarantine list, then record
                // the quarantine statistics for the quarantine warning(s) we are about to generate.
                //
                // SAFETY: `guard` holds `self.owner.collection_lock`, `self.owner` owns `self`,
                // and `state == StubPmtState::Active` was verified above.
                unsafe { self.owner.quarantine_from_active_list(&mut guard, self) };
                quarantined_pmt_count = guard.quarantined_pmts().len();
                quarantined_page_count = *guard.quarantined_pages();
            } else {
                // We are not active right now meaning that we must have been explicitly released
                // with a call to `zx_pmt_unpin`. Assert this.
                debug_assert_eq!(state, StubPmtState::Released);
                debug_assert!(!self.in_container());
                // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
                // `self`.
                debug_assert!(
                    unsafe { self.guard_collection_lock(guard.token()) }.pinned_vmo().is_none()
                );
            }
        }

        // If we entered the quarantine, log a warning outside of the spinlock. Either a `DRIVER
        // OOPS` if our BTI has already been orphaned (as there is no way to recover these pages at
        // this point), or a `DRIVER WARNING` if the BTI is not orphaned yet (as we may recover
        // these pages when user-mode eventually notices its mistake and calls
        // `zx_bti_release_quarantine`).
        if quarantined_pmt_count > 0 {
            let reason = if report_bti_quarantine {
                BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned
            } else {
                BtiPageLeakReason::PmtQuarantined
            };
            let mut bti_name = [0u8; ZX_MAX_NAME_LEN];
            self.owner.get_name(&mut bti_name);
            // Note: `print_quarantine_warning` takes `(total_leaked_pages, total_leaked_vmos)`.
            print_quarantine_warning(
                self.owner.bti_id(),
                &bti_name,
                reason,
                quarantined_page_count,
                quarantined_pmt_count,
            );
        } else {
            debug_assert!(!report_bti_quarantine);
            debug_assert_eq!(quarantined_page_count, 0);
        }
    }

    /// Unconditionally releases the pinned VMO held by a quarantined PMT after
    /// `StubBti::release_quarantine` has removed it from `quarantined_pmts`.
    fn release_quarantined_vmo(&self) {
        // Move the VMO state on the stack while holding the collection lock, then let it destruct
        // outside of the lock so that we don't unpin a VMO (which acquires a mutex) while holding
        // a spinlock. The BTI code has already set our state to `Released` so that we pass our
        // destructor debug checks.
        let to_release = {
            ksync::lock!(let mut guard = self.owner.lock_collection_lock());
            debug_assert!(!self.in_container());
            // SAFETY: `guard` holds `self.owner.collection_lock`, which is the lock guarding
            // `self`.
            let mut inner = unsafe { self.guard_collection_lock_mut(guard.as_mut().token_mut()) };
            debug_assert_eq!(*inner.state(), StubPmtState::Released);
            debug_assert!(inner.pinned_vmo().as_ref().is_some_and(|p| p.vmo().is_some()));

            inner.pinned_vmo_mut().take()
        };
        drop(to_release);
    }
}

#[pinned_drop]
impl PinnedDrop for StubPmt {
    fn drop(self: Pin<&mut Self>) {
        // By the time that we destruct, we must have gone through either `release_pinned_memory`
        // followed by `on_dispatcher_zero_handles`, or `on_dispatcher_zero_handles` followed by
        // `release_quarantine`, so we should be able to assert that we have been released and are
        // no longer on any of our owner's lists.
        #[cfg(debug_assertions)]
        {
            // SAFETY: We are in `PinnedDrop::drop`, so nothing else can reference `self`, and none
            // of the pinned fields are moved.
            let this = unsafe { self.get_unchecked_mut() };
            debug_assert!(!this.list_node.in_container());
            debug_assert_eq!(*this.state.get_inner_mut(), StubPmtState::Released);
            debug_assert!(this.pinned_vmo.get_inner_mut().is_none());
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = self;
        }
    }
}
