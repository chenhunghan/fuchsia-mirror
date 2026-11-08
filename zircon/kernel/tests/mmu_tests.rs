// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// mmu tests
#[cfg(ktest)]
#[unittest::suite(name = "mmu")]
mod mmu_tests {
    use core::cell::Cell;
    use core::convert::Infallible;
    use core::ffi::c_void;
    use core::pin::Pin;
    use core::ptr::from_ref;
    use core::sync::atomic::{AtomicBool, Ordering};
    use lazy_init::{LazyInit, NoCheck};
    use pin_init::{PinInit, pin_data, pin_init, stack_pin_init};
    use unittest::{
        assert_ok, assert_true, expect_eq, expect_err, expect_ok, expect_true, unwrap_ok,
        unwrap_some,
    };
    use zx_status::Status;

    use crate::arch_rs::{USER_ASPACE_BASE, USER_ASPACE_SIZE};
    use crate::kernel::thread;
    use crate::kernel::types::PAddr;
    use crate::platform_rs::timer::{DurationMono, InstantMono};
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_UNMAP_OPTION_ENLARGE,
        ARCH_UNMAP_OPTION_NONE, ArchMmuFlags, ArchVmAspace, ExistingEntryAction,
    };
    use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr};
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_address_region::flag as vmar_flag;
    use crate::vm::vm_aspace::{Type, VmAspace};
    use crate::vm::vm_cow_pages::{DeferredOps, RangeChangeOp, VmCowRange};
    use crate::vm::vm_object_paged::VmObjectPaged;

    const PAGE_SIZE: usize = page::SIZE;
    const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;
    const PGTABLE_L2_SHIFT: usize = page::SHIFT + page::TABLE_LEVEL_SHIFT;
    const PGTABLE_L1_SHIFT: usize = PGTABLE_L2_SHIFT + page::TABLE_LEVEL_SHIFT;

    /// Most mmu tests want a 'sufficiently large' aspace to play in, these constants define an
    /// aspace that is large without having a discontinuity over the sign extended canonical
    /// addresses.
    const ASPACE_BASE: usize = USER_ASPACE_BASE;
    const ASPACE_SIZE: usize = USER_ASPACE_SIZE;

    /// create large unaligned region and ensure it can be unmapped
    #[test]
    fn test_large_unaligned_region() {
        // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range and this
        // test upholds the `init` -> operations -> `destroy` lifecycle.
        stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
        // SAFETY: `aspace` was just constructed and is initialized once before use.
        let err = unsafe { aspace.init() };
        expect_ok!(err, "init aspace");

        let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

        // We want our region to be misaligned by at least a page, and for
        // it to straddle the PDP.
        let va: usize = (1usize << PGTABLE_L1_SHIFT) - (1usize << PGTABLE_L2_SHIFT) + 2 * PAGE_SIZE;
        // Make sure ALLOC_SIZE is less than 1 PD page, to exercise the
        // non-terminal code path.
        const ALLOC_SIZE: usize = (1usize << PGTABLE_L2_SHIFT) - PAGE_SIZE;

        // Map a single page to force the lower PDP of the target region
        // to be created
        // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace` that
        // is never switched to or dereferenced.
        let err = unsafe { aspace.map_contiguous(va - 3 * PAGE_SIZE, PAddr(0), 1, arch_rw_flags) };
        expect_ok!(err, "map single page");

        // Map the last page of the region
        // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe {
            aspace.map_contiguous(va + ALLOC_SIZE - PAGE_SIZE, PAddr(0), 1, arch_rw_flags)
        };
        expect_ok!(err, "map last page");

        let err = aspace.query(va + ALLOC_SIZE - PAGE_SIZE).map(|_| ());
        expect_ok!(err, "last entry is mapped");

        // Attempt to unmap the target region (analogous to unmapping a demand
        // paged region that has only had its last page touched)
        // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe { aspace.unmap(va, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
        expect_ok!(err, "unmap unallocated region");

        let err = aspace.query(va + ALLOC_SIZE - PAGE_SIZE);
        expect_err!(err, Status::NOT_FOUND, "last entry is not mapped anymore");

        // Unmap the single page from earlier
        // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe { aspace.unmap(va - 3 * PAGE_SIZE, 1, ARCH_UNMAP_OPTION_ENLARGE) };
        expect_ok!(err, "unmap single page");

        // SAFETY: All mappings in `aspace` have been unmapped and `aspace` is not used afterward.
        let err = unsafe { aspace.destroy() };
        expect_ok!(err, "destroy aspace");
    }

    /// create large unaligned region without mapping and ensure it can be unmapped
    #[test]
    fn test_large_unaligned_region_without_map() {
        {
            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range and
            // this test upholds the `init` -> operations -> `destroy` lifecycle.
            stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
            // SAFETY: `aspace` was just constructed and is initialized once before use.
            let err = unsafe { aspace.init() };
            expect_ok!(err, "init aspace");

            let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

            // We want our region to be misaligned by a page, and for it to
            // straddle the PDP
            let va: usize = (1usize << PGTABLE_L1_SHIFT) - (1usize << PGTABLE_L2_SHIFT) + PAGE_SIZE;
            // Make sure ALLOC_SIZE is bigger than 1 PD page, to exercise the
            // non-terminal code path.
            const ALLOC_SIZE: usize = 3usize << PGTABLE_L2_SHIFT;

            // Map a single page to force the lower PDP of the target region
            // to be created
            // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.map_contiguous(va - 2 * PAGE_SIZE, PAddr(0), 1, arch_rw_flags) };
            expect_ok!(err, "map single page");

            // Attempt to unmap the target region (analogous to unmapping a demand
            // paged region that has not been touched)
            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.unmap(va, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap unallocated region");

            // Unmap the single page from earlier
            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe { aspace.unmap(va - 2 * PAGE_SIZE, 1, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap single page");

            // SAFETY: All mappings in `aspace` have been unmapped and `aspace` is not used
            // afterward.
            let err = unsafe { aspace.destroy() };
            expect_ok!(err, "destroy aspace");
        }
    }

    /// creating large vm region, and change permissions
    #[test]
    fn test_large_region_protect() {
        const VA: usize = 1usize << PGTABLE_L1_SHIFT;
        // Force a large page.
        const ALLOC_SIZE: usize = 1usize << PGTABLE_L2_SHIFT;
        const ALLOC_END: usize = VA + ALLOC_SIZE;

        let target_vaddrs: [usize; 6] = [
            VA,
            VA + PAGE_SIZE,
            VA + 2 * PAGE_SIZE,
            ALLOC_END - 3 * PAGE_SIZE,
            ALLOC_END - 2 * PAGE_SIZE,
            ALLOC_END - PAGE_SIZE,
        ];

        for (i, &target_vaddr) in target_vaddrs.iter().enumerate() {
            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range and
            // each loop iteration upholds the `init` -> operations -> `destroy` lifecycle.
            stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
            // SAFETY: `aspace` was just constructed and is initialized once before use.
            let err = unsafe { aspace.init() };
            expect_ok!(err, "init aspace");

            let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

            // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.map_contiguous(VA, PAddr(0), ALLOC_SIZE / PAGE_SIZE, arch_rw_flags)
            };
            expect_ok!(err, "map large page");

            // SAFETY: Updating protections in an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.protect(target_vaddr, 1, ARCH_MMU_FLAG_PERM_READ, ARCH_UNMAP_OPTION_ENLARGE)
            };
            expect_ok!(err, "protect single page");

            for (j, &query_vaddr) in target_vaddrs.iter().enumerate() {
                let mut mmu_flags: ArchMmuFlags = 0;
                let mut pa = PAddr(0);
                expect_ok!(aspace.query(query_vaddr).map(|(p, f)| {
                    pa = p;
                    mmu_flags = f;
                }));
                expect_eq!(query_vaddr - VA, pa.0);

                expect_eq!(if i == j { ARCH_MMU_FLAG_PERM_READ } else { arch_rw_flags }, mmu_flags);
            }

            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.unmap(VA, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap large page");
            // SAFETY: All mappings in `aspace` have been unmapped and `aspace` is dropped next.
            let err = unsafe { aspace.destroy() };
            expect_ok!(err, "destroy aspace");
        }
    }

    crate::ksync_kernel::declare_singleton_mutex!(
        /// Since `toggle_page_alloc_fn` needs global state to operate, define a lock to ensure we
        /// are only running a single instance of these tests at a time.
        TogglePageAllocLock
    );
    // `AtomicBool` provides `Sync` interior mutability across the stateless `PageAllocFn` C
    // callback while test runs are serialized by `TogglePageAllocLock`.
    static FAIL_PAGE_ALLOCS: AtomicBool = AtomicBool::new(false);

    /// # Safety
    ///
    /// `p` and `pa` must be non-null, valid, aligned, and non-overlapping for writing a
    /// `*mut VmPage` and `PAddr` respectively.
    unsafe extern "C" fn toggle_page_alloc_fn(
        alloc_flags: u32,
        p: *mut *mut VmPage,
        pa: *mut PAddr,
    ) -> Result<(), Status> {
        if FAIL_PAGE_ALLOCS.load(Ordering::SeqCst) {
            return Err(Status::NO_MEMORY);
        }
        let (page, paddr) = pmm::alloc_page(alloc_flags)?;
        // SAFETY: Caller guarantees `p` and `pa` are non-null, aligned, non-overlapping, and valid
        // for writes.
        unsafe {
            *p = page.as_raw();
            *pa = paddr;
        }
        Ok(())
    }

    /// create large vm region and unmap single pages
    #[test]
    fn test_large_region_unmap() {
        ksync::lock!(let _guard = TogglePageAllocLock::lock());

        const VA: usize = 1usize << PGTABLE_L1_SHIFT;
        // Force a large page.
        const ALLOC_SIZE: usize = 1usize << PGTABLE_L2_SHIFT;
        const ALLOC_END: usize = VA + ALLOC_SIZE;

        let target_vaddrs: [usize; 6] = [
            VA,
            VA + PAGE_SIZE,
            VA + 2 * PAGE_SIZE,
            ALLOC_END - 3 * PAGE_SIZE,
            ALLOC_END - 2 * PAGE_SIZE,
            ALLOC_END - PAGE_SIZE,
        ];

        for (i, &target_vaddr) in target_vaddrs.iter().enumerate() {
            FAIL_PAGE_ALLOCS.store(false, Ordering::SeqCst);
            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space, this loop
            // upholds the `init` -> operations -> `destroy` lifecycle, and `toggle_page_alloc_fn`
            // satisfies `PageAllocFn` without re-entering `ArchVmAspace` or unwinding.
            stack_pin_init!(let aspace = unsafe {
                ArchVmAspace::new_with_page_alloc_fn(
                    ASPACE_BASE,
                    ASPACE_SIZE,
                    0,
                    toggle_page_alloc_fn,
                )
            });
            // SAFETY: `aspace` was just constructed and is initialized once before use.
            let err = unsafe { aspace.init() };
            expect_ok!(err, "init aspace");

            let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

            // Use `map_contiguous` to create a mapping that should get backed by a large page.
            // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.map_contiguous(VA, PAddr(0), ALLOC_SIZE / PAGE_SIZE, arch_rw_flags)
            };
            expect_ok!(err, "map large page");

            // Unmap a single small page out of the larger page.
            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe { aspace.unmap(target_vaddr, 1, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap single page");

            // Ensure the single page was unmapped, but the rest of the large page is still present.
            for (j, &query_vaddr) in target_vaddrs.iter().enumerate() {
                let result = aspace.query(query_vaddr).map(|_| ());
                if i == j {
                    expect_err!(result, Status::NOT_FOUND, "query page");
                } else {
                    expect_ok!(result, "query page");
                }
            }

            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.unmap(VA, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap remaining pages");

            // Map in the large page again.
            // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.map_contiguous(VA, PAddr(0), ALLOC_SIZE / PAGE_SIZE, arch_rw_flags)
            };
            expect_ok!(err, "map large page");

            // Simulate OOM by failing allocations.
            FAIL_PAGE_ALLOCS.store(true, Ordering::SeqCst);
            // Attempt to unmap a single small page, but allow over unmapping.
            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe { aspace.unmap(target_vaddr, 1, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap single page");

            // The entire large page should have ended up unmapped.
            for &query_vaddr in &target_vaddrs {
                let result = aspace.query(query_vaddr);
                expect_err!(result, Status::NOT_FOUND, "query page");
            }

            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.unmap(VA, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap remaining pages");

            // Map in the large page again.
            FAIL_PAGE_ALLOCS.store(false, Ordering::SeqCst);
            // SAFETY: Mapping physical address 0 into an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.map_contiguous(VA, PAddr(0), ALLOC_SIZE / PAGE_SIZE, arch_rw_flags)
            };
            expect_ok!(err, "map large page");

            // Simulate OOM by failing allocations.
            FAIL_PAGE_ALLOCS.store(true, Ordering::SeqCst);
            // Attempt to unmap a single small page, but disallow over unmapping.
            // let err = unsafe { aspace.unmap(target_vaddr, 1, ARCH_UNMAP_OPTION_NONE) };
            // expect_err!(err, Status::NO_MEMORY, "unmap single page");

            // All mappings should still be present.
            for &query_vaddr in &target_vaddrs {
                let result = aspace.query(query_vaddr).map(|_| ());
                expect_ok!(result, "query page");
            }

            // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
            let err =
                unsafe { aspace.unmap(VA, ALLOC_SIZE / PAGE_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            expect_ok!(err, "unmap remaining pages");
            // SAFETY: All mappings in `aspace` have been unmapped and `aspace` is dropped next.
            let err = unsafe { aspace.destroy() };
            expect_ok!(err, "destroy aspace");
        }
    }

    crate::ksync_kernel::declare_singleton_mutex!(
        /// Serializes `test_mapping_oom` runs that share the global `NODE` page list.
        TestMappingOomLock
    );
    crate::ksync_kernel::declare_singleton_spinlock!(
        /// Spinlock protecting `NODE` (may be acquired while `ArchVmAspace` holds its mutex).
        TestPageAllocNodeLock
    );
    #[pin_data]
    struct TestPageAllocNode {
        // Safety invariant: every queued node in `list` is a live, uniquely owned PMM `VmPage` in
        // `ALLOC` state.
        #[pin]
        list: VmPageDoublyLinkedList,
    }

    // SAFETY: `VmPage` entries reside in global PMM arena memory and can be transferred and freed
    // across threads, while access to `list` is serialized by `TestPageAllocNodeLock`.
    unsafe impl Send for TestPageAllocNode {}

    impl TestPageAllocNode {
        fn init() -> impl PinInit<Self, Infallible> {
            pin_init!(Self { list <- VmPageDoublyLinkedList::new() })
        }
    }

    static NODE: LazyInit<ksync::KCell<TestPageAllocNode, TestPageAllocNodeLock>, NoCheck> =
        LazyInit::uninit();

    fn mmu_tests_node_init(_level: init::LkInitLevel) {
        // SAFETY: Single-threaded initialization during early boot before any test runs.
        unsafe {
            Pin::static_ref(&NODE)
                .init_pin(ksync::KCell::pin_init(TestPageAllocNode::init()))
                .unwrap();
        }
    }

    init::lk_init_hook!(
        mmu_tests_node_init_hook,
        mmu_tests_node_init,
        init::LK_INIT_LEVEL_EARLIEST
    );

    fn with_node<R>(f: impl FnOnce(Pin<&mut VmPageDoublyLinkedList>) -> R) -> R {
        let lock = TestPageAllocNodeLock::get();
        ksync::lock!(let mut guard = lock.lock());
        // SAFETY: `TestPageAllocNodeLock` is the singleton spinlock guarding `NODE`, and `NODE`
        // was initialized in place via `init_pin` at `LK_INIT_LEVEL_EARLIEST` before any test runs
        // and is never moved.
        let node = unsafe { NODE.get_unchecked().get_pinned_mut_locked(guard.token_mut()) };
        f(node.project().list)
    }

    /// # Safety
    ///
    /// `p` and `pa` must be non-null, valid, aligned, and non-overlapping for writing a
    /// `*mut VmPage` and `PAddr` respectively.
    unsafe extern "C" fn test_page_alloc_fn(
        _unused: u32,
        p: *mut *mut VmPage,
        pa: *mut PAddr,
    ) -> Result<(), Status> {
        let Some(page_nn) = with_node(|node| node.pop_front()) else {
            return Err(Status::NO_MEMORY);
        };
        let page = VmPagePtr::new(page_nn);
        // SAFETY: `page` is owned by this test allocator so inspecting `paddr` is safe, and caller
        // guarantees `p` and `pa` are non-null, aligned, non-overlapping, and valid for writes.
        unsafe {
            *p = page.as_raw();
            *pa = page.paddr();
        }
        Ok(())
    }

    /// trigger oom failures when creating a mapping
    #[test]
    fn test_mapping_oom() {
        ksync::lock!(let _guard = TestMappingOomLock::lock());

        const MAPPING_PAGE_COUNT: usize = 8;
        const MAPPING_SIZE: usize = MAPPING_PAGE_COUNT * PAGE_SIZE;
        const MAPPING_START: usize = (1usize << PGTABLE_L1_SHIFT) - MAPPING_SIZE / 2;

        // Allocate the pages which will be mapped into the test aspace.
        let mapping_pages: [Cell<Option<VmPagePtr>>; MAPPING_PAGE_COUNT] =
            [const { Cell::new(None) }; MAPPING_PAGE_COUNT];
        let mut mapping_paddrs = [PAddr(0); MAPPING_PAGE_COUNT];

        let _undo = zr::defer(|| {
            for mapping_page in &mapping_pages {
                if let Some(page) = mapping_page.take() {
                    // SAFETY: `page` was allocated by `pmm::alloc_page` below and is no longer
                    // mapped into any `ArchVmAspace`.
                    unsafe { pmm::free_page(page) };
                }
            }
        });

        for (i, mapping_paddr) in mapping_paddrs.iter_mut().enumerate() {
            let (page, paddr) = unwrap_ok!(pmm::alloc_page(0));
            mapping_pages[i].set(Some(page));
            *mapping_paddr = paddr;
        }

        let _cleanup_node = zr::defer(|| {
            while let Some(page_nn) = with_node(|node| node.pop_front()) {
                // SAFETY: Every entry in `NODE` was freshly allocated via `pmm::alloc_page` below
                // and was never consumed by an `ArchVmAspace`.
                unsafe { pmm::free_page(VmPagePtr::new(page_nn)) };
            }
        });

        // Try to create the mapping with a limited number of pages available to
        // the aspace. Start with only 1 available and continue until the map operation
        // succeeds without running out of memory.
        let mut map_success = false;
        let mut avail_mmu_pages: u64 = 1;
        while !map_success {
            for _ in 0..avail_mmu_pages {
                let (page, _) = unwrap_ok!(pmm::alloc_page(0), "alloc fail");
                with_node(|node| {
                    // SAFETY: `page` was freshly allocated from the PMM, is in `ALLOC` state and
                    // not in any list, and remains live until popped and transferred or freed.
                    unsafe { node.push_front_raw(page.as_non_null()) };
                });
            }

            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space, this loop
            // upholds the `init` -> operations -> `destroy` lifecycle, and `test_page_alloc_fn`
            // acquires only leaf `TestPageAllocNodeLock` and writes a freshly allocated PMM
            // `VmPage` popped from `NODE` and its `PAddr` on `Ok(())`.
            stack_pin_init!(let aspace = unsafe {
                ArchVmAspace::new_with_page_alloc_fn(
                    ASPACE_BASE,
                    ASPACE_SIZE,
                    0,
                    test_page_alloc_fn,
                )
            });
            // SAFETY: `aspace` was just constructed and is initialized once before use.
            let err = unsafe { aspace.init() };
            assert_ok!(err, "init aspace");

            let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

            // SAFETY: `mapping_paddrs` contains `MAPPING_PAGE_COUNT` valid physical page addresses
            // mapped into an isolated, unactivated test `ArchVmAspace`.
            let err = unsafe {
                aspace.map(
                    MAPPING_START,
                    mapping_paddrs.as_mut_ptr(),
                    MAPPING_PAGE_COUNT,
                    arch_rw_flags,
                    ExistingEntryAction::Error,
                )
            };
            if err.is_ok() {
                map_success = true;
                // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
                expect_ok!(unsafe {
                    aspace.unmap(MAPPING_START, MAPPING_PAGE_COUNT, ARCH_UNMAP_OPTION_ENLARGE)
                });
            } else {
                expect_err!(err, Status::NO_MEMORY);
                avail_mmu_pages += 1;
                // validate that all of the pages were consumed
                expect_true!(with_node(|node| node.is_empty()));
            }

            // Destroying the aspace verifies that everything was cleaned up
            // when the mapping failed part way through.
            // SAFETY: Any mappings in `aspace` were unmapped or rolled back on `map` error, and
            // `aspace` is dropped at the end of this iteration.
            let err = unsafe { aspace.destroy() };
            assert_ok!(err, "destroy aspace");
            assert_true!(with_node(|node| node.is_empty()));
        }
    }

    /// construct an aspace without calling init and verify drop and destroy succeed
    #[test]
    fn test_uninitialized_aspace() {
        {
            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range, and
            // dropping an `ArchVmAspace` when no `init*` method was called is valid.
            stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
            expect_eq!(aspace.arch_table_phys().0, 0);
        }

        {
            // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range, and
            // calling `destroy` before drop when no `init*` method was called is valid.
            stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
            expect_eq!(aspace.arch_table_phys().0, 0);
            // SAFETY: `aspace` is inactive and `destroy` permits uninitialized aspaces.
            expect_ok!(unsafe { aspace.destroy() }, "destroy uninitialized aspace");
        }
    }

    /// skip existing entry when mapping multiple pages
    #[test]
    fn test_skip_existing_mapping() {
        const MAP_BASE: usize = ASPACE_BASE;
        const PHYS_BASE: usize = 0;
        const NUM_PAGES: usize = 8;
        const MID_PAGE: usize = NUM_PAGES / 2;

        // SAFETY: `ASPACE_BASE` and `ASPACE_SIZE` define a valid user address space range and this
        // test upholds the `init` -> operations -> `destroy` lifecycle.
        stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(ASPACE_BASE, ASPACE_SIZE, 0) });
        // SAFETY: `aspace` was just constructed and is initialized once before use.
        let err = unsafe { aspace.init() };
        expect_ok!(err);

        let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

        let mut page_addresses = [PAddr(0); NUM_PAGES];
        for (i, addr) in page_addresses.iter_mut().enumerate() {
            *addr = PAddr(PHYS_BASE + (PAGE_SIZE * i));
        }

        // Map in the middle page by itself first, using the final settings.
        // SAFETY: Mapping 1 entry from `page_addresses` into an isolated test `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE + MID_PAGE * PAGE_SIZE,
                &raw mut page_addresses[MID_PAGE],
                1,
                arch_rw_flags,
                ExistingEntryAction::Error,
            )
        };
        expect_ok!(err);

        // Now map in all the pages.
        // SAFETY: Mapping `NUM_PAGES` entries from `page_addresses` into an isolated test
        // `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE,
                page_addresses.as_mut_ptr(),
                NUM_PAGES,
                arch_rw_flags,
                ExistingEntryAction::Skip,
            )
        };
        expect_ok!(err);

        // Validate all the pages.
        for (i, &expected_paddr) in page_addresses.iter().enumerate() {
            let mut paddr = PAddr(0);
            let mut mmu_flags: ArchMmuFlags = 0;
            let err = aspace.query(MAP_BASE + i * PAGE_SIZE).map(|(p, f)| {
                paddr = p;
                mmu_flags = f;
            });
            expect_ok!(err);
            expect_eq!(paddr.0, expected_paddr.0);
            expect_eq!(mmu_flags, arch_rw_flags);
        }
        // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe { aspace.unmap(MAP_BASE, NUM_PAGES, ARCH_UNMAP_OPTION_ENLARGE) };
        expect_ok!(err);

        // Now try mapping in the middle page with different permissions.
        // SAFETY: Mapping 1 entry from `page_addresses` into an isolated test `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE + MID_PAGE * PAGE_SIZE,
                &raw mut page_addresses[MID_PAGE],
                1,
                ARCH_MMU_FLAG_PERM_READ,
                ExistingEntryAction::Error,
            )
        };
        expect_ok!(err);
        // SAFETY: Mapping `NUM_PAGES` entries from `page_addresses` into an isolated test
        // `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE,
                page_addresses.as_mut_ptr(),
                NUM_PAGES,
                arch_rw_flags,
                ExistingEntryAction::Skip,
            )
        };
        expect_ok!(err);
        for (i, &expected_paddr) in page_addresses.iter().enumerate() {
            let mut paddr = PAddr(0);
            let mut mmu_flags: ArchMmuFlags = 0;
            let err = aspace.query(MAP_BASE + i * PAGE_SIZE).map(|(p, f)| {
                paddr = p;
                mmu_flags = f;
            });
            expect_ok!(err);
            expect_eq!(paddr.0, expected_paddr.0);
            if i == MID_PAGE {
                expect_eq!(mmu_flags, ARCH_MMU_FLAG_PERM_READ);
            } else {
                expect_eq!(mmu_flags, arch_rw_flags);
            }
        }
        // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe { aspace.unmap(MAP_BASE, NUM_PAGES, ARCH_UNMAP_OPTION_ENLARGE) };
        expect_ok!(err);

        // Now map the middle page using a completely different physical address.
        let mut other_paddr = PAddr(PAGE_SIZE * 42);
        // SAFETY: Mapping 1 entry from `other_paddr` into an isolated test `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE + MID_PAGE * PAGE_SIZE,
                &raw mut other_paddr,
                1,
                ARCH_MMU_FLAG_PERM_READ,
                ExistingEntryAction::Error,
            )
        };
        expect_ok!(err);
        // SAFETY: Mapping `NUM_PAGES` entries from `page_addresses` into an isolated test
        // `ArchVmAspace`.
        let err = unsafe {
            aspace.map(
                MAP_BASE,
                page_addresses.as_mut_ptr(),
                NUM_PAGES,
                arch_rw_flags,
                ExistingEntryAction::Skip,
            )
        };
        expect_ok!(err);
        for (i, &expected_paddr) in page_addresses.iter().enumerate() {
            let mut paddr = PAddr(0);
            let mut mmu_flags: ArchMmuFlags = 0;
            let err = aspace.query(MAP_BASE + i * PAGE_SIZE).map(|(p, f)| {
                paddr = p;
                mmu_flags = f;
            });
            expect_ok!(err);
            if i == MID_PAGE {
                expect_eq!(mmu_flags, ARCH_MMU_FLAG_PERM_READ);
                expect_eq!(paddr.0, other_paddr.0);
            } else {
                expect_eq!(mmu_flags, arch_rw_flags);
                expect_eq!(paddr.0, expected_paddr.0);
            }
        }
        // SAFETY: Unmapping from an isolated, unactivated test `ArchVmAspace`.
        let err = unsafe { aspace.unmap(MAP_BASE, NUM_PAGES, ARCH_UNMAP_OPTION_ENLARGE) };
        expect_ok!(err);

        // SAFETY: All mappings in `aspace` have been unmapped and `aspace` is not used afterward.
        let err = unsafe { aspace.destroy() };
        expect_ok!(err);
    }

    /// splitting a large page is atomic
    #[test]
    fn test_large_region_atomic() {
        // Attempts to validate that unmapping part of a large page will not cause parallel threads
        // accessing other parts of the page to fault. This test is only probabilistic and is
        // heavily timing and micro architectural dependent, but could serve as a canary.
        if VmAspace::kernel_aspace().arch_aspace().unmap_only_enlarge_on_oom() {
            // Force a large page.
            const ALLOC_SIZE: usize = 1usize << PGTABLE_L2_SHIFT;

            const TARGET_OFFSETS: [usize; 6] = [
                0,
                PAGE_SIZE,
                2 * PAGE_SIZE,
                ALLOC_SIZE - 3 * PAGE_SIZE,
                ALLOC_SIZE - 2 * PAGE_SIZE,
                ALLOC_SIZE - PAGE_SIZE,
            ];

            for (i, &target_offset) in TARGET_OFFSETS.iter().enumerate() {
                // Allocate a large page in the current kernel aspace. Need to allocate in the
                // current aspace and not a test aspace so that we can directly access the mappings.
                let kaspace = VmAspace::kernel_aspace();
                let vmar = unwrap_some!(kaspace.root_vmar());

                let vmo = unwrap_ok!(VmObjectPaged::create(
                    pmm::ALLOC_FLAG_ANY,
                    VmObjectPaged::ALWAYS_PINNED,
                    ALLOC_SIZE as u64,
                ));

                let arch_rw_flags: ArchMmuFlags =
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

                let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
                    0,
                    ALLOC_SIZE,
                    0,
                    vmar_flag::CAN_MAP_READ
                        | vmar_flag::CAN_MAP_WRITE
                        | vmar_flag::DEBUG_DYNAMIC_KERNEL_MAPPING,
                    VmObjectPaged::into_vm_object(vmo),
                    0,
                    arch_rw_flags,
                    b"test",
                ));

                assert_ok!(mapping_result.mapping.map_range(0, ALLOC_SIZE, false, false));

                let va = mapping_result.base;
                let mapping = mapping_result.mapping;

                let _cleanup_mapping = zr::defer(|| {
                    let _ = mapping.destroy();
                });

                // Spin up a thread to start touching pages in the mapping.
                struct State {
                    va: usize,
                    current_offset: usize,
                    running: AtomicBool,
                }
                let state = State { va, current_offset: i, running: AtomicBool::new(true) };
                extern "C" fn thread_body(arg: *mut c_void) -> i32 {
                    // SAFETY: `arg` points to `state` on the stack, which outlives this thread
                    // (joined in `_cleanup_thread`) and is only accessed via shared reference.
                    let state = unsafe { &*arg.cast_const().cast::<State>() };

                    while state.running.load(Ordering::SeqCst) {
                        for (i, &offset) in TARGET_OFFSETS.iter().enumerate() {
                            if state.current_offset == i {
                                continue;
                            }
                            let addr = core::ptr::with_exposed_provenance::<u64>(state.va + offset);
                            // Force read from the address
                            // SAFETY: `addr` points to a mapped, 8-byte aligned page in `mapping`
                            // that is not unmapped while this thread runs
                            // (`i != state.current_offset`), and the empty inline assembly block
                            // consumes `val` in a register with no side effects.
                            unsafe {
                                let val = addr.read_volatile();
                                core::arch::asm!(
                                    "/* {0} */",
                                    in(reg) val,
                                    options(nomem, nostack, preserves_flags),
                                );
                            }
                        }
                    }
                    0
                }

                // SAFETY: `thread_body` is a valid thread entry point and `&state` outlives the
                // thread due to `_cleanup_thread` joining before `state` drops.
                let thread = unwrap_ok!(unsafe {
                    thread::create_with_priority(
                        c"test-thread".as_ptr(),
                        thread_body,
                        from_ref(&state).cast_mut().cast::<c_void>(),
                        thread::DEFAULT_PRIORITY,
                    )
                });
                // SAFETY: `thread` was just created and has not been resumed yet.
                unsafe { thread.resume() };

                let _cleanup_thread = zr::defer(|| {
                    state.running.store(false, Ordering::SeqCst);
                    // SAFETY: `thread` was resumed above and is joined once here.
                    assert!(unsafe { thread.join(InstantMono::INFINITE) }.is_ok());
                });

                // Wait a moment to let the other thread start touching.
                let _ = thread::sleep_relative(DurationMono::from_millis(50));

                // Unmap a single page.
                // SAFETY: Unmapping the single page at `target_offset` that the worker thread skips
                // (`state.current_offset == i`), within a dynamic kernel test mapping.
                let status = unsafe {
                    kaspace.arch_aspace().unmap(va + target_offset, 1, ARCH_UNMAP_OPTION_NONE)
                };
                expect_ok!(status, "unmap single page");

                // If the other thread didn't cause a kernel panic by having a page fault, then
                // success.
            }
        }
    }

    /// test that unmap ignores pinned pages
    #[test]
    fn test_unmap_ignore_pinned() {
        const SMALL_PAGES: usize = 4;
        const SMALL_PAGES_U64: u64 = SMALL_PAGES as u64;
        const EXTRA_UNMAP_PAGES: u64 = 100;
        const ALT_PAGES: usize = 20;
        const ALT_PAGES_U64: u64 = ALT_PAGES as u64;

        // Test that range_change_update_locked ignores pinned pages.
        let _scanner_disable = AutoVmScannerDisable::new();

        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));
        let _cleanup_aspace = zr::defer(|| {
            assert!(aspace.destroy().is_ok());
        });

        let arch_aspace = aspace.arch_aspace();
        let arch_rw_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

        let vmar = unwrap_some!(aspace.root_vmar());

        {
            // Ensure an unpinned range gets completely unmapped.
            let vmo = unwrap_ok!(VmObjectPaged::create(
                pmm::ALLOC_FLAG_ANY,
                0,
                SMALL_PAGES_U64 * PAGE_SIZE_U64,
            ));
            assert_ok!(vmo.commit_range(0, SMALL_PAGES_U64 * PAGE_SIZE_U64));

            let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
                0,
                SMALL_PAGES * PAGE_SIZE,
                0,
                vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                arch_rw_flags,
                b"test",
            ));
            assert_ok!(mapping_result.mapping.map_range(0, SMALL_PAGES * PAGE_SIZE, false, false));
            let mapping = mapping_result.mapping;
            let _cleanup_mapping = zr::defer(|| {
                assert!(mapping.destroy().is_ok());
            });

            let va = mapping_result.base;

            for i in 0..SMALL_PAGES {
                expect_ok!(arch_aspace.query(va + i * PAGE_SIZE).map(|_| ()));
            }

            let cow = unwrap_some!(vmo.debug_get_cow_pages());
            stack_pin_init!(let deferred = DeferredOps::new(&cow));
            ksync::lock!(let guard = cow.lock());
            cow.range_change_update_locked(
                guard.token(),
                VmCowRange::new(0, SMALL_PAGES_U64 * PAGE_SIZE_U64),
                RangeChangeOp::Unmap,
                Some(deferred),
            );

            for i in 0..SMALL_PAGES {
                expect_err!(arch_aspace.query(va + i * PAGE_SIZE), Status::NOT_FOUND);
            }
        }

        {
            // Ensure a fully pinned range doesn't get unmapped.
            let vmo = unwrap_ok!(VmObjectPaged::create(
                pmm::ALLOC_FLAG_ANY,
                0,
                SMALL_PAGES_U64 * PAGE_SIZE_U64,
            ));
            assert_ok!(vmo.commit_range_pinned(0, SMALL_PAGES_U64 * PAGE_SIZE_U64, false));
            let vmo_for_unpin = vmo.clone();
            let _unpin = zr::defer(|| {
                vmo_for_unpin.unpin(0, SMALL_PAGES_U64 * PAGE_SIZE_U64);
            });

            let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
                0,
                SMALL_PAGES * PAGE_SIZE,
                0,
                vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                arch_rw_flags,
                b"test",
            ));
            assert_ok!(mapping_result.mapping.map_range(0, SMALL_PAGES * PAGE_SIZE, false, false));
            let mapping = mapping_result.mapping;
            let _cleanup_mapping = zr::defer(|| {
                assert!(mapping.destroy().is_ok());
            });

            let va = mapping_result.base;

            let cow = unwrap_some!(vmo.debug_get_cow_pages());
            stack_pin_init!(let deferred = DeferredOps::new(&cow));
            ksync::lock!(let guard = cow.lock());
            cow.range_change_update_locked(
                guard.token(),
                VmCowRange::new(0, SMALL_PAGES_U64 * PAGE_SIZE_U64),
                RangeChangeOp::Unmap,
                Some(deferred),
            );

            for i in 0..SMALL_PAGES {
                expect_ok!(arch_aspace.query(va + i * PAGE_SIZE).map(|_| ()));
            }
        }

        {
            // Unmap a greater range than exists in the VMO or in the mapping.
            let vmo = unwrap_ok!(VmObjectPaged::create(
                pmm::ALLOC_FLAG_ANY,
                0,
                SMALL_PAGES_U64 * PAGE_SIZE_U64,
            ));
            assert_ok!(vmo.commit_range(0, SMALL_PAGES_U64 * PAGE_SIZE_U64));
            assert_ok!(vmo.commit_range_pinned(PAGE_SIZE_U64, 2 * PAGE_SIZE_U64, false));
            let vmo_for_unpin = vmo.clone();
            let _unpin = zr::defer(|| {
                vmo_for_unpin.unpin(PAGE_SIZE_U64, 2 * PAGE_SIZE_U64);
            });

            let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
                0,
                SMALL_PAGES * PAGE_SIZE,
                0,
                vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                arch_rw_flags,
                b"test",
            ));
            assert_ok!(mapping_result.mapping.map_range(0, SMALL_PAGES * PAGE_SIZE, false, false));
            let mapping = mapping_result.mapping;
            let _cleanup_mapping = zr::defer(|| {
                assert!(mapping.destroy().is_ok());
            });

            let va = mapping_result.base;

            let cow = unwrap_some!(vmo.debug_get_cow_pages());
            stack_pin_init!(let deferred = DeferredOps::new(&cow));
            ksync::lock!(let guard = cow.lock());
            // Note: Large len here. This is okay.
            cow.range_change_update_locked(
                guard.token(),
                VmCowRange::new(0, EXTRA_UNMAP_PAGES * PAGE_SIZE_U64),
                RangeChangeOp::Unmap,
                Some(deferred),
            );

            #[expect(clippy::erasing_op, clippy::identity_op)]
            {
                expect_err!(arch_aspace.query(va + 0 * PAGE_SIZE), Status::NOT_FOUND);
                expect_ok!(arch_aspace.query(va + 1 * PAGE_SIZE).map(|_| ()));
                expect_ok!(arch_aspace.query(va + 2 * PAGE_SIZE).map(|_| ()));
                expect_err!(arch_aspace.query(va + 3 * PAGE_SIZE), Status::NOT_FOUND);
            }
        }

        {
            // Alternating pinned pages.
            let vmo = unwrap_ok!(VmObjectPaged::create(
                pmm::ALLOC_FLAG_ANY,
                0,
                ALT_PAGES_U64 * PAGE_SIZE_U64,
            ));
            assert_ok!(vmo.commit_range(0, ALT_PAGES_U64 * PAGE_SIZE_U64));

            for i in (0..ALT_PAGES_U64).step_by(2) {
                assert_ok!(vmo.commit_range_pinned(i * PAGE_SIZE_U64, PAGE_SIZE_U64, false));
            }
            let vmo_for_unpin = vmo.clone();
            let _unpin = zr::defer(|| {
                for i in (0..ALT_PAGES_U64).step_by(2) {
                    vmo_for_unpin.unpin(i * PAGE_SIZE_U64, PAGE_SIZE_U64);
                }
            });

            let mapping_result = unwrap_ok!(vmar.create_vm_mapping(
                0,
                ALT_PAGES * PAGE_SIZE,
                0,
                vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                arch_rw_flags,
                b"test",
            ));
            assert_ok!(mapping_result.mapping.map_range(0, ALT_PAGES * PAGE_SIZE, false, false));
            let mapping = mapping_result.mapping;
            let _cleanup_mapping = zr::defer(|| {
                assert!(mapping.destroy().is_ok());
            });

            let va = mapping_result.base;

            let cow = unwrap_some!(vmo.debug_get_cow_pages());
            stack_pin_init!(let deferred = DeferredOps::new(&cow));
            ksync::lock!(let guard = cow.lock());
            cow.range_change_update_locked(
                guard.token(),
                VmCowRange::new(0, ALT_PAGES_U64 * PAGE_SIZE_U64),
                RangeChangeOp::Unmap,
                Some(deferred),
            );

            for i in 0..ALT_PAGES {
                if i % 2 == 0 {
                    expect_ok!(arch_aspace.query(va + i * PAGE_SIZE).map(|_| ()));
                } else {
                    expect_err!(arch_aspace.query(va + i * PAGE_SIZE), Status::NOT_FOUND);
                }
            }
        }
    }
}
