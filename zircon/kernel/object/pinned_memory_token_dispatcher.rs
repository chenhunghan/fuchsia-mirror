// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::iommu::{Pmt, RequireContiguousMapping};
pub use crate::iommu::{QueryAddressResult, dev_vaddr_t};
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::vm::pinned_vm_object::PinnedVmObject;
use debug::ltrace_entry;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_PMT_RIGHTS, ZX_OBJ_TYPE_PMT, zx_rights_t};

use super::KernelHandle;
use super::bus_transaction_initiator_dispatcher::BusTransactionInitiatorDispatcher;
use super::pinned_memory_token_dispatcher_ffi::cpp_pinned_memory_token_dispatcher_create;

use object_constants_rs as object_constants;

const LOCAL_TRACE: u32 = 0;

zr::static_assert_size_and_align!(
    PinnedMemoryTokenDispatcherState,
    object_constants::kPinnedMemoryTokenDispatcherStateSize,
    object_constants::kPinnedMemoryTokenDispatcherStateAlign,
);

define_kcounter!(
    DISPATCHER_PINNED_MEMORY_TOKEN_CREATE_COUNT,
    "dispatcher.pinned_memory_token.create",
    Sum
);
define_kcounter!(
    DISPATCHER_PINNED_MEMORY_TOKEN_DESTROY_COUNT,
    "dispatcher.pinned_memory_token.destroy",
    Sum
);

/// Internal state storage for `PinnedMemoryTokenDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PinnedMemoryTokenDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"PIMT") }>,

    /// Whether `on_zero_handles` needs to unpin `pmt` on behalf of the kernel
    /// because ownership has not yet been handed off to user space (and
    /// `unpin` has not yet been called).
    #[guarded_by(lock)]
    kernel_cleanup_needed: bool,

    /// Reference to the containing BTI dispatcher.
    bti: RefPtr<BusTransactionInitiatorDispatcher>,

    /// Underlying `Pmt`.
    #[guarded_by(lock)]
    pmt: Option<Pmt>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,
}

impl PinnedMemoryTokenDispatcherState {
    /// Initializes the `PinnedMemoryTokenDispatcherState`.
    pub fn init(
        _dispatcher: *const PinnedMemoryTokenDispatcher,
        bti: &RefPtr<BusTransactionInitiatorDispatcher>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_PINNED_MEMORY_TOKEN_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            kernel_cleanup_needed: true.into(),
            bti: bti.clone(),
            pmt: None.into(),
            lock <- KMutex::init(),
        })
    }

    /// Stores the `Pmt` into state.
    fn set_pmt(&self, pmt: Pmt) {
        ksync::lock!(let mut guard = self.lock_lock());
        *guard.as_mut().fields_mut().pmt = Some(pmt);
    }
}

#[pinned_drop]
impl PinnedDrop for PinnedMemoryTokenDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_PINNED_MEMORY_TOKEN_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher representing a token of memory pinned by a Bus Transaction Initiator.
    pub struct PinnedMemoryTokenDispatcher,
    PinnedMemoryTokenDispatcherState,
    ZX_OBJ_TYPE_PMT,
    object_constants::kPinnedMemoryTokenDispatcherStateOffset
);

impl PinnedMemoryTokenDispatcher {
    /// Creates a new `PinnedMemoryTokenDispatcher` representing pinned pages in `pinned_vmo`.
    ///
    /// Sets the permissions of `pinned_vmo`'s pinned range to `perms` on behalf of `bti`.
    /// `perms` should be flags suitable for [`crate::iommu::Bti::map`].
    pub fn create(
        bti: RefPtr<BusTransactionInitiatorDispatcher>,
        pinned_vmo: PinnedVmObject,
        perms: u32,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        ltrace_entry!();
        debug_assert!(page::is_aligned(pinned_vmo.offset() as usize));
        debug_assert!(page::is_aligned(pinned_vmo.size() as usize));
        debug_assert!(pinned_vmo.vmo().is_some());

        // SAFETY: `cpp_pinned_memory_token_dispatcher_create` initializes `out` on `ZX_OK`.
        let handle = unsafe {
            KernelHandle::create(|out| cpp_pinned_memory_token_dispatcher_create(&bti, out))
        }?;

        // TODO(b/502262026): It feels like whether or not we require a contiguous
        // mapping should come as a flag from user mode, not as an expectation based
        // on whether or not the underlying VMO is actually physically contiguous.
        let contiguous_vmo =
            if pinned_vmo.vmo().expect("pinned_vmo must have a vmo").is_contiguous() {
                RequireContiguousMapping::Yes
            } else {
                RequireContiguousMapping::No
            };
        let pmt = handle.dispatcher().state().bti.bti().map(pinned_vmo, perms, contiguous_vmo)?;

        handle.dispatcher().state().set_pmt(pmt);

        Ok((handle, ZX_DEFAULT_PMT_RIGHTS))
    }

    /// Unpins and unmaps the memory which was managed by this PMT.
    pub fn unpin(&self) {
        self.state().canary.assert();
        ksync::lock!(let mut guard = self.state().lock_lock());
        // If kernel code calls `unpin()` directly without having cleared
        // `kernel_cleanup_needed` (for example, kernel tests or any in-kernel
        // caller that does not hand the PMT out to user space), clearing it here
        // avoids a redundant `release_pinned_memory()` call in `on_zero_handles()`.
        // When `unpin()` is called via `zx_pmt_unpin`, `kernel_cleanup_needed` was
        // already cleared before adding the handle to the user process's handle
        // table, so this assignment is a no-op in that case.
        *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
        let pmt = guard.fields().pmt.as_ref().expect("PMT must be initialized");
        pmt.release_pinned_memory();
    }

    /// Query the pinned and mapped VMO for a region specified by offset/size.
    pub fn query_address(&self, offset: u64, size: usize) -> Result<QueryAddressResult, Status> {
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        let pmt = guard.fields().pmt.as_ref().expect("PMT must be initialized");
        pmt.query_address(offset, size)
    }

    /// Marks that the handle for this PMT is about to be added to the user
    /// process's handle table (after all fallible steps have succeeded), so user
    /// space is now responsible for calling `zx_pmt_unpin` before closing all
    /// handles.
    pub fn clear_kernel_cleanup_needed(&self) {
        self.state().canary.assert();
        ksync::lock!(let mut guard = self.state().lock_lock());
        *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
    }

    /// Callback invoked when all handles to this dispatcher are closed.
    pub fn on_zero_handles(&self) {
        self.state().canary.assert();
        ksync::lock!(let mut guard = self.state().lock_lock());
        // We may not have a driver level PMT if we never fully constructed
        // successfully. In this case, we will be dropping the kernel handle to the
        // PMT Dispatcher without ever having assigned an iommu::Pmt to it.
        if *guard.fields().kernel_cleanup_needed {
            *guard.as_mut().fields_mut().kernel_cleanup_needed = false;
            if let Some(pmt) = guard.fields().pmt.as_ref() {
                pmt.release_pinned_memory();
            }
        }
        if let Some(pmt) = guard.fields().pmt.as_ref() {
            pmt.on_dispatcher_zero_handles();
        }
    }
}

/// Kernel unit tests for `PinnedMemoryTokenDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "pinned_memory_token_dispatcher_tests")]
mod tests {
    use super::BusTransactionInitiatorDispatcher;
    use crate::object::{IOMMU_FLAG_PERM_READ, IommuDispatcher};
    use crate::vm::pmm;
    use crate::vm::vm_object::VmObject;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use unittest::{expect_eq, expect_err, expect_false, expect_true, unwrap_ok};
    use zx_status::Status;
    use zx_types::{ZX_IOMMU_TYPE_STUB, zx_iommu_desc_stub_t};

    /// Tests PMT cleanup responsibility before and after `clear_kernel_cleanup_needed`.
    #[test]
    fn test_pmt_handoff_cleanup_responsibility() {
        let desc =
            fbl::Array::<u8>::try_new_zeroed_slice(core::mem::size_of::<zx_iommu_desc_stub_t>())
                .expect("failed to allocate stub iommu desc");
        let (iommu_handle, _iommu_rights) =
            IommuDispatcher::create(ZX_IOMMU_TYPE_STUB, desc).expect("failed to create stub iommu");

        let (bti_handle, _bti_rights) =
            BusTransactionInitiatorDispatcher::create(iommu_handle.dispatcher().iommu(), 0)
                .expect("failed to create bti");
        let bti_disp = bti_handle.dispatcher();

        let vmo_paged =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let vmo: fbl::RefPtr<VmObject> = fbl::RefPtr::from_ref(&vmo_paged);

        // 1. Pre-handoff drop: dropping the `KernelHandle` before `clear_kernel_cleanup_needed()`
        //    must unpin the PMT without quarantining it or faulting the BTI.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo.clone(), 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo");
            expect_eq!(bti_disp.pmo_count(), 1);
            expect_eq!(bti_disp.quarantine_count(), 0);
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());

        // 2. Post-handoff drop without `unpin()`: once `clear_kernel_cleanup_needed()` is called,
        //    dropping all handles without calling `unpin()` must quarantine the PMT.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo.clone(), 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo after pre-handoff drop");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 1);
        expect_eq!(bti_disp.quarantine_count(), 1);
        expect_true!(bti_disp.bti().in_fault_state());

        bti_disp.release_quarantine();
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());

        // 3. Post-handoff explicit `unpin()` followed by drop: must unpin cleanly without
        //    quarantining.
        {
            let (pmt_handle, _pmt_rights) = bti_disp
                .pin(vmo, 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo after release_quarantine");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();
            pmt_handle.dispatcher().unpin();
            drop(pmt_handle);
        }
        expect_eq!(bti_disp.pmo_count(), 0);
        expect_eq!(bti_disp.quarantine_count(), 0);
        expect_false!(bti_disp.bti().in_fault_state());
    }

    /// Tests that late BTI `release_quarantine` after `on_zero_handles` is a no-op (b/575647999).
    #[test]
    fn test_bti_release_quarantine_after_zero_handles() {
        let desc =
            fbl::Array::<u8>::try_new_zeroed_slice(core::mem::size_of::<zx_iommu_desc_stub_t>())
                .expect("failed to allocate stub iommu desc");
        let (iommu_handle, _iommu_rights) =
            IommuDispatcher::create(ZX_IOMMU_TYPE_STUB, desc).expect("failed to create stub iommu");

        let vmo_paged =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let vmo: fbl::RefPtr<VmObject> = fbl::RefPtr::from_ref(&vmo_paged);

        // 1. Orphaned BTI with only an active (non-quarantined) PMT: dropping the BTI handle
        //    orphans `StubBti` without emitting `DRIVER OOPS`; a late `release_quarantine` must
        //    short-circuit in the dispatcher without reaching `StubBti::release_quarantine`'s
        //    `!is_orphaned` assertion, and unpinning the active PMT naturally removes the BTI from
        //    `ORPHAN_LIST`.
        {
            let (bti_handle, _bti_rights) =
                BusTransactionInitiatorDispatcher::create(iommu_handle.dispatcher().iommu(), 0x67)
                    .expect("failed to create bti");
            let (pmt_handle, _pmt_rights) = bti_handle
                .dispatcher()
                .pin(vmo.clone(), 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();

            let racing = bti_handle.dispatcher().clone();
            drop(bti_handle);

            racing.release_quarantine();
            expect_eq!(racing.pmo_count(), 1);
            expect_eq!(racing.quarantine_count(), 0);
            expect_false!(racing.bti().in_fault_state());

            pmt_handle.dispatcher().unpin();
            drop(pmt_handle);
            expect_eq!(racing.pmo_count(), 0);
            expect_eq!(racing.quarantine_count(), 0);
            expect_false!(racing.bti().in_fault_state());
            drop(racing);
        }

        // 2. Orphaned BTI with a quarantined PMT: a late `release_quarantine` must leave the
        //    orphaned BTI's quarantine intact rather than draining `quarantined_pmts` and
        //    stranding `StubBti` on `ORPHAN_LIST`.
        let (bti_handle, _bti_rights) =
            BusTransactionInitiatorDispatcher::create(iommu_handle.dispatcher().iommu(), 0x68)
                .expect("failed to create bti");

        // Pin and leak a PMT so it enters `StubBti::quarantined_pmts` (emits `DRIVER WARNING`).
        {
            let (pmt_handle, _pmt_rights) = bti_handle
                .dispatcher()
                .pin(vmo, 0, page::SIZE as u64, IOMMU_FLAG_PERM_READ)
                .expect("failed to pin vmo");
            pmt_handle.dispatcher().clear_kernel_cleanup_needed();
            drop(pmt_handle);
        }
        expect_eq!(bti_handle.dispatcher().pmo_count(), 1);
        expect_eq!(bti_handle.dispatcher().quarantine_count(), 1);
        expect_true!(bti_handle.dispatcher().bti().in_fault_state());

        // Hold a strong `RefPtr` to the dispatcher (simulating a concurrent thread already inside
        // `sys_bti_release_quarantine`) while the last user handle to the BTI closes. Suppress the
        // intentional `DRIVER OOPS` log during `drop(bti_handle)` so `tefmocheck` does not flag it.
        let racing = bti_handle.dispatcher().clone();
        crate::iommu::set_suppress_quarantine_oops_for_test(true);
        drop(bti_handle);
        crate::iommu::set_suppress_quarantine_oops_for_test(false);

        // The late `release_quarantine` lost the race with `on_zero_handles` and must leave the
        // orphaned BTI's quarantine intact.
        racing.release_quarantine();
        expect_eq!(racing.pmo_count(), 1);
        expect_eq!(racing.quarantine_count(), 1);
        expect_true!(racing.bti().in_fault_state());

        // Clean up the test's orphaned `StubBti` and pinned page so `ORPHAN_LIST` and the PMM
        // remain clean for subsequent tests.
        racing.bti().reclaim_orphaned_for_test();
        expect_eq!(racing.pmo_count(), 0);
        expect_eq!(racing.quarantine_count(), 0);
        expect_false!(racing.bti().in_fault_state());
        drop(racing);
    }

    /// Tests that `StubPmt::query_address` enforces pinned sub-range bounds and error precedence.
    #[test]
    fn test_stub_pmt_query_address_bounds() {
        let desc =
            fbl::Array::<u8>::try_new_zeroed_slice(core::mem::size_of::<zx_iommu_desc_stub_t>())
                .expect("failed to allocate stub iommu desc");
        let (iommu_handle, _iommu_rights) =
            IommuDispatcher::create(ZX_IOMMU_TYPE_STUB, desc).expect("failed to create stub iommu");

        let (bti_handle, _bti_rights) =
            BusTransactionInitiatorDispatcher::create(iommu_handle.dispatcher().iommu(), 0x69)
                .expect("failed to create bti");
        let bti_disp = bti_handle.dispatcher();

        // Create a 4-page contiguous VMO and pin its middle 2 pages (pages 1 and 2) so that both
        // page 0 (before the pin) and page 3 (after the pin) remain unpinned inside the VMO.
        const VMO_SIZE: u64 = 4 * (page::SIZE as u64);
        const PIN_OFFSET: u64 = page::SIZE as u64;
        const PIN_SIZE: u64 = 2 * (page::SIZE as u64);

        let vmo_paged = VmObjectPaged::create_contiguous(pmm::ALLOC_FLAG_ANY, VMO_SIZE, 0)
            .expect("failed to create contiguous VMO");
        let vmo: fbl::RefPtr<VmObject> = fbl::RefPtr::from_ref(&vmo_paged);
        let base_paddr = u64::from(unwrap_ok!(vmo.lookup_contiguous(0, VMO_SIZE)));

        let (pmt_handle, _pmt_rights) =
            unwrap_ok!(bti_disp.pin(vmo, PIN_OFFSET, PIN_SIZE, IOMMU_FLAG_PERM_READ));
        let pmt_disp = pmt_handle.dispatcher();

        // In-bounds queries return physical addresses relative to the pinned sub-range.
        let full = unwrap_ok!(pmt_disp.query_address(0, PIN_SIZE as usize));
        expect_eq!(base_paddr + PIN_OFFSET, full.device_vaddr);
        expect_eq!(PIN_SIZE as usize, full.size);

        let second_page = unwrap_ok!(pmt_disp.query_address(page::SIZE as u64, page::SIZE));
        expect_eq!(base_paddr + PIN_OFFSET + (page::SIZE as u64), second_page.device_vaddr);
        expect_eq!(page::SIZE, second_page.size);

        let sub_page = unwrap_ok!(pmt_disp.query_address(page::SIZE as u64, 1));
        expect_eq!(base_paddr + PIN_OFFSET + (page::SIZE as u64), sub_page.device_vaddr);
        expect_eq!(page::SIZE, sub_page.size);

        // Basic parameter validation rejects unaligned offsets and zero sizes with `INVALID_ARGS`.
        expect_err!(pmt_disp.query_address(1, page::SIZE), Status::INVALID_ARGS);
        expect_err!(pmt_disp.query_address(0, 0), Status::INVALID_ARGS);
        expect_err!(pmt_disp.query_address(PIN_SIZE, 0), Status::INVALID_ARGS);

        // Queries past the end of the pin (into unpinned page 3 of the VMO), wrapping around
        // before the pin (into unpinned page 0), or overflowing `page::round_up` must fail with
        // `OUT_OF_RANGE`.
        expect_err!(pmt_disp.query_address(0, (PIN_SIZE as usize) + 1), Status::OUT_OF_RANGE);
        expect_err!(
            pmt_disp.query_address(0, (PIN_SIZE as usize) + page::SIZE),
            Status::OUT_OF_RANGE
        );
        expect_err!(
            pmt_disp.query_address(page::SIZE as u64, PIN_SIZE as usize),
            Status::OUT_OF_RANGE
        );
        expect_err!(pmt_disp.query_address(PIN_SIZE, page::SIZE), Status::OUT_OF_RANGE);
        expect_err!(
            pmt_disp.query_address(0u64.wrapping_sub(PIN_OFFSET), page::SIZE),
            Status::OUT_OF_RANGE
        );
        expect_err!(pmt_disp.query_address(0, usize::MAX), Status::OUT_OF_RANGE);
        expect_err!(pmt_disp.query_address(page::SIZE as u64, usize::MAX), Status::OUT_OF_RANGE);

        // Once unpinned, `BAD_STATE` takes precedence over `OUT_OF_RANGE`, while `INVALID_ARGS`
        // still takes precedence over `BAD_STATE`.
        pmt_disp.unpin();
        expect_err!(pmt_disp.query_address(0, page::SIZE), Status::BAD_STATE);
        expect_err!(pmt_disp.query_address(PIN_SIZE, page::SIZE), Status::BAD_STATE);
        expect_err!(pmt_disp.query_address(1, page::SIZE), Status::INVALID_ARGS);

        drop(pmt_handle);
        expect_eq!(0, bti_disp.pmo_count());
        expect_eq!(0, bti_disp.quarantine_count());
        expect_false!(bti_disp.bti().in_fault_state());
    }
}
