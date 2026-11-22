// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod sizes {
    pub const KB: usize = 1024;
    pub const MB: usize = 1024 * KB;
    pub const GB: usize = 1024 * MB;
}

/// Address space unit tests.
#[cfg(ktest)]
#[unittest::suite]
mod aspace_rs {
    use super::sizes::GB;
    use crate::arch_rs::vm::is_user_accessible_range;
    use crate::arch_rs::{
        KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, USER_ASPACE_BASE, USER_ASPACE_SIZE,
        USER_RESTRICTED_ASPACE_SIZE,
    };
    use crate::kernel::deadline::Deadline;
    use crate::kernel::event::AutounsignalEvent;
    use crate::kernel::thread;
    use crate::kernel::types::{PAddr, VAddr};
    use crate::platform_rs::timer::InstantMono;
    use crate::user_copy::internal::validate_user_accessible_range;
    use crate::user_memory::UserMemory;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_USER,
        ARCH_MMU_FLAG_PERM_WRITE, ARCH_UNMAP_OPTION_ENLARGE, ArchMmuFlags, ArchVmAspace,
        ExistingEntryAction, NonTerminalAction, TerminalAction,
    };
    use crate::vm::attribution::{AttributionCounts, FractionalBytes};
    use crate::vm::fault::flag as pf_flag;
    use crate::vm::page::{VmPage, VmPageDoublyLinkedList};
    use crate::vm::page_source::MultiPageRequest;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm::{
        __code_end, __code_start, __data_start, __executable_start, __relro_end, __relro_start,
        __rodata_end, __rodata_start, _end, vaddr_to_paddr,
    };
    use crate::vm::vm_address_region::{
        self as vmar, MemoryPriority, OwnedVmAddressRegionOrMapping, VmAddressRegion,
        VmAddressRegionOpChildren,
    };
    use crate::vm::vm_address_region_enumerator::{
        MappingsOnly, VmAddressRegionEnumerator, VmAddressRegionEnumeratorType, VmarsAndMappings,
    };
    use crate::vm::vm_aspace::{
        GUEST_PHYSICAL_ASPACE_BASE, GUEST_PHYSICAL_ASPACE_SIZE, ShareOpt, Type, VmAspace,
        VmAspaceLockClass, VmAspaceMutex, VmAspaceRegionLockClass, VmUsage, vmm_flag,
    };
    use crate::vm::vm_mapping::VmMapping;
    use crate::vm::vm_object::{Resizability, SnapshotType, VmObject, VmObjectReadWriteOptions};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm::{pmm, vmm};
    use crate::vm_unittests::test_helper::{
        ARCH_RW_FLAGS, ARCH_RW_USER_FLAGS, alloc_user, fill_and_test, fill_and_test_user,
        make_committed_pager_vmo, make_partially_committed_pager_vmo,
        make_private_attribution_counts, verify_continuous_attribution_bytes,
        verify_mapped_page_range,
    };
    use core::ffi::c_void;
    use core::marker::PhantomData;
    use core::mem::{DropGuard, MaybeUninit, size_of, size_of_val};
    use core::ptr::from_ref;
    use core::sync::atomic::{AtomicBool, Ordering};
    use fbl::RefPtr;
    use kprint::kprintln;
    use ksync::LockToken;
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_err, assert_nonnull, assert_ok, assert_true, expect_eq, expect_err,
        expect_false, expect_ne, expect_ok, expect_true, subtest, unwrap_ok, unwrap_some,
    };
    use zx_status::Status;
    use zx_types::{ZX_MAX_PAGE_SHIFT, ZX_PAGER_VMO_STATS_MODIFIED};

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    struct KernelRegion {
        name: &'static str,
        base: usize,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    }

    fn kernel_regions() -> [KernelRegion; 4] {
        let code_start = (&raw const __code_start).addr();
        let code_end = (&raw const __code_end).addr();
        let rodata_start = (&raw const __rodata_start).addr();
        let rodata_end = (&raw const __rodata_end).addr();
        let relro_start = (&raw const __relro_start).addr();
        let relro_end = (&raw const __relro_end).addr();
        let data_start = (&raw const __data_start).addr();
        let end = (&raw const _end).addr();
        [
            KernelRegion {
                name: "kernel_code",
                base: code_start,
                size: page::round_up(code_end - code_start),
                arch_mmu_flags: ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
            },
            KernelRegion {
                name: "kernel_rodata",
                base: rodata_start,
                size: page::round_up(rodata_end - rodata_start),
                arch_mmu_flags: ARCH_MMU_FLAG_PERM_READ,
            },
            KernelRegion {
                name: "kernel_relro",
                base: relro_start,
                size: page::round_up(relro_end - relro_start),
                arch_mmu_flags: ARCH_MMU_FLAG_PERM_READ,
            },
            KernelRegion {
                name: "kernel_data_bss",
                base: data_start,
                size: page::round_up(end - data_start),
                arch_mmu_flags: ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            },
        ]
    }

    /// Allocates a contiguous region in kernel space, reads/writes it, then destroys it.
    #[test]
    fn vmm_alloc_contiguous_smoke_test() {
        let alloc_size = 256 * 1024;

        // allocate a region of memory
        let kaspace = VmAspace::kernel_aspace();
        // SAFETY: Creating a mapping of a new contiguous VMO is sound.
        let base = unsafe {
            kaspace.alloc_contiguous(b"test", alloc_size, None, 0, vmm_flag::COMMIT, ARCH_RW_FLAGS)
        };
        let base = unwrap_ok!(base, "VmAspace::AllocContiguous region of memory");
        let ptr: *mut c_void = core::ptr::with_exposed_provenance_mut(base);
        assert_nonnull!(ptr, "VmAspace::AllocContiguous region of memory");

        // fill with known pattern and test
        let slice: *mut MaybeUninit<u8> = ptr.cast();
        // SAFETY: `ptr` points to `alloc_size` bytes of valid memory allocated in `kaspace`.
        let slice = unsafe { core::slice::from_raw_parts_mut(slice, alloc_size) };
        let (_buf, result) = fill_and_test(slice);
        expect_true!(result);

        // test that it is indeed contiguous
        kprintln!("testing that region is contiguous");
        let mut last_pa = 0u64;
        for i in 0..(alloc_size / PAGE_SIZE_USIZE) {
            // SAFETY: `ptr` points to a contiguous allocation of at least `alloc_size` bytes.
            let va = unsafe { ptr.add(i * PAGE_SIZE_USIZE) };
            let pa = u64::from(vaddr_to_paddr(va));
            if last_pa != 0 {
                expect_eq!(pa, last_pa + PAGE_SIZE, "region is contiguous");
            }
            last_pa = pa;
        }

        // free the region
        // SAFETY: `base` is in a mapping of the root VMAR whose whole VMO range is pinned by a pin
        // this code owns, and nothing uses that mapping any more.
        let err = unsafe { kaspace.free_region(base) };
        expect_ok!(err, "VmAspace::FreeRegion region of memory");
    }

    /// Allocates a new address space and creates a few regions in it, then destroys it.
    #[test]
    fn multiple_regions_test() {
        let alloc_size: usize = 16 * 1024;

        let aspace =
            unwrap_some!(VmAspace::create(Type::User, b"test aspace"), "VmAspace::Create pointer");

        // SAFETY: Running on an initialized thread.
        let old_aspace = unsafe { thread::current_active_aspace_ptr() };
        // SAFETY: `aspace` remains valid while active.
        unsafe { vmm::set_active_aspace(Some(VmAspace::as_non_null(&aspace))) };

        // allocate region 0
        let ptr0 = unwrap_ok!(
            alloc_user(&aspace, b"test0", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr0, alloc_size));

        // allocate region 1
        let ptr1 = unwrap_ok!(
            alloc_user(&aspace, b"test1", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr1, alloc_size));

        // allocate region 2
        let ptr2 = unwrap_ok!(
            alloc_user(&aspace, b"test2", alloc_size),
            "VmAspace::Alloc region of memory"
        );
        // fill with known pattern and test
        expect_true!(fill_and_test_user(ptr2, alloc_size));

        // SAFETY: It is sound to restore the previous address space.
        unsafe { vmm::set_active_aspace(old_aspace) };

        // free the address space all at once
        let err = aspace.destroy();
        expect_ok!(err, "VmAspace::Destroy");
    }

    /// Checks that `alloc_contiguous` fails when missing `vmm_flag::COMMIT`.
    #[test]
    fn vmm_alloc_contiguous_missing_flag_commit_fails() {
        // should have vmm_flag::COMMIT
        let zero_vmm_flags = 0u32;
        // SAFETY: Creating a mapping of a new contiguous VMO is sound.
        let err = unsafe {
            VmAspace::kernel_aspace().alloc_contiguous(
                b"test",
                PAGE_SIZE_USIZE,
                None,
                0,
                zero_vmm_flags,
                ARCH_RW_FLAGS,
            )
        };
        assert_err!(err, Status::INVALID_ARGS);
    }

    /// Checks that `alloc_contiguous` fails with zero size.
    #[test]
    fn vmm_alloc_contiguous_zero_size_fails() {
        let zero_size = 0usize;
        // SAFETY: Creating a mapping of a new contiguous VMO is sound.
        let err = unsafe {
            VmAspace::kernel_aspace().alloc_contiguous(
                b"test",
                zero_size,
                None,
                0,
                vmm_flag::COMMIT,
                ARCH_RW_FLAGS,
            )
        };
        assert_err!(err, Status::INVALID_ARGS);
    }

    /// Allocates a vm address space object directly, allows it to go out of scope.
    #[test]
    fn vmaspace_create_smoke_test() {
        let aspace = VmAspace::create(Type::User, b"test aspace").expect("VmAspace::create failed");
        let err = aspace.destroy();
        expect_ok!(err, "VmAspace::Destroy");
    }

    /// Verifies that creating an address space with out-of-range bounds fails.
    #[test]
    fn vmaspace_create_invalid_ranges() {
        // Test when base < valid base.
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE - PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                KERNEL_ASPACE_BASE - PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                Type::Kernel,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE.wrapping_sub(PAGE_SIZE_USIZE),
                PAGE_SIZE_USIZE,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );

        // Test when base + size exceeds valid range.
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE,
                USER_ASPACE_SIZE + PAGE_SIZE_USIZE,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                KERNEL_ASPACE_BASE,
                KERNEL_ASPACE_SIZE + PAGE_SIZE_USIZE,
                Type::Kernel,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE,
                GUEST_PHYSICAL_ASPACE_SIZE + PAGE_SIZE_USIZE,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );

        // Test when size is 0.
        expect_true!(
            VmAspace::create_with_opts(USER_ASPACE_BASE, 0, Type::User, b"test", ShareOpt::None)
                .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE,
                0,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );

        // Test when base or size is not page aligned.
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE + 1,
                PAGE_SIZE_USIZE,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                USER_ASPACE_BASE,
                PAGE_SIZE_USIZE + 1,
                Type::User,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE + 1,
                PAGE_SIZE_USIZE,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(
            VmAspace::create_with_opts(
                GUEST_PHYSICAL_ASPACE_BASE,
                PAGE_SIZE_USIZE + 1,
                Type::GuestPhysical,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );

        // Test kernel ranges, which only the singleton kernel aspace may cover.
        expect_true!(
            VmAspace::create_with_opts(
                KERNEL_ASPACE_BASE,
                PAGE_SIZE_USIZE,
                Type::Kernel,
                b"test",
                ShareOpt::None
            )
            .is_none()
        );
        expect_true!(VmAspace::create(Type::Kernel, b"test").is_none());
    }

    /// Allocates a vm address space object directly, maps something on it, and drops it.
    #[test]
    fn vmaspace_alloc_smoke_test() {
        // Allocates a vm address space object directly, maps something on it,
        // allows it to go out of scope.
        let mut aspace = VmAspace::create(Type::User, b"test aspace2");

        let _ptr = unwrap_ok!(
            alloc_user(aspace.as_ref().unwrap(), b"test", PAGE_SIZE_USIZE),
            "allocating region\n"
        );

        // destroy the aspace, which should drop all the internal refs to it
        let err = aspace.as_ref().unwrap().destroy();
        expect_ok!(err, "VmAspace::Destroy");

        // drop the ref held by this pointer
        drop(aspace.take());
    }

    /// Wrapper for harvesting access bits that informs the page queues.
    fn harvest_access_bits(
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) {
        let _scanner_disable = AutoVmScannerDisable::new();
        VmAspace::harvest_all_user_accessed_bits(non_terminal_action, terminal_action);
    }

    /// Consume the (scalar) value, ensuring that the operation to calculate the value can not be
    /// optimized out/ deemed as unused by the compiler. I.e. this function can be used as a wrapper
    /// to a calculation to ensure it will be in the binary.
    fn consume_value<T>(value: T) {
        // The compiler must materialize the value into a register, since it doesn't
        // know that the register's value isn't actually used.
        core::hint::black_box(value);
    }

    /// Touch mappings in an aspace and ensure we can correctly harvest the accessed bits.
    /// This test takes an optional tag that is placed in the top byte of the address when
    /// performing a user_copy.
    fn vmaspace_accessed_test(tag: u8) -> bool {
        let run = subtest!(|tag: u8| {
            let _scanner_disable = AutoVmScannerDisable::new();

            // Create some memory we can map touch to test accessed tracking on. Needs to be created
            // from user pager backed memory as harvesting is allowed to be limited to just that.
            let (vmo, [page]) = unwrap_ok!(make_committed_pager_vmo::<1>(
                /*trap_dirty=*/ false, /*resizable=*/ false,
            ));
            let mem = unwrap_some!(UserMemory::create_from_vmo(
                VmObjectPaged::into_vm_object(vmo),
                tag,
                0
            ));

            assert_ok!(mem.commit_and_map(0..PAGE_SIZE_USIZE));

            // Initial accessed state is undefined, so harvest it away.
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

            // Grab the current queue for the page and then rotate the page queues. This means any
            // future, correct, access harvesting should result in a new page queue.
            // SAFETY: `page` is valid and attached to a VM object.
            let mut current_queue =
                unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };
            pmm::page_queues().rotate_reclaim_queues();

            // Read from the mapping to (hopefully) set the accessed bit.
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            // Harvest it to move it in the page queue.
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };

            // Rotating and harvesting again should not make the queue change since we have not
            // accessed it.
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_eq!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });

            // Set the accessed bit again, and make sure it does now harvest.
            pmm::page_queues().rotate_reclaim_queues();
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });

            // Set the accessed bit and update age without harvesting.
            consume_value(unwrap_ok!(mem.get::<i32>(0)));
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };

            // Now if we rotate and update again, we should re-age the page.
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
            // SAFETY: `page` is valid and attached to a VM object.
            current_queue = unsafe { page.as_ref().get_page_queue_ref().load(Ordering::SeqCst) };
            pmm::page_queues().rotate_reclaim_queues();
            harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAge);
            // SAFETY: `page` is valid and attached to a VM object.
            expect_ne!(current_queue, unsafe {
                page.as_ref().get_page_queue_ref().load(Ordering::SeqCst)
            });
        });
        run(tag)
    }

    /// Touch mappings in an aspace and ensure accessed bits are correctly harvested.
    #[test]
    fn vmaspace_accessed_test_untagged() {
        expect_true!(vmaspace_accessed_test(0));
    }

    /// Reruns the accessed-bit test with tagged user pointers.
    #[test]
    fn vmaspace_accessed_test_tagged() {
        // Rerun the `vmaspace_accessed_test` tests with tags in the top byte of user pointers. This
        // tests that the subsequent accessed faults are handled successfully, even if the FAR
        // contains a tag.

        // TODO(ethanws): Make this entire test conditional on #[cfg!(target_arch = "aarch64")] when
        // lib/unittest gets this functionality.
        if cfg!(target_arch = "aarch64") {
            expect_true!(vmaspace_accessed_test(0xAB));
        } else {
            kprintln!("Skipping vmaspace_accessed_test_tagged; not aarch64.");
        }
    }

    /// Ensure user requested VMO read/write handles faults after access bits are harvested.
    #[test]
    fn vmaspace_usercopy_accessed_fault_test() {
        // Ensure that if a user requested VMO read/write operation would hit a page that has had
        // its accessed bits harvested that any resulting fault (on ARM) can be handled.
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create some memory we can map touch to test accessed tracking on. Needs to be created
        // from user pager backed memory as harvesting is allowed to be limited to just that.
        let (mapping_vmo, [_page]) = unwrap_ok!(make_committed_pager_vmo::<1>(
            /*trap_dirty=*/ false, /*resizable=*/ false
        ));
        let mem = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(mapping_vmo),
            0,
            0
        ));

        assert_ok!(mem.commit_and_map(0..PAGE_SIZE_USIZE));

        // Need a separate VMO to read/write from.
        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        // Touch the mapping to make sure it is committed and mapped.
        unwrap_ok!(mem.put::<u8>(42, 0));

        // Harvest any accessed bits.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);

        // Read from the VMO into the mapping that has been harvested.
        let (res, read_actual) =
            vmo.read_user(mem.user_out::<u8>(), 0, size_of::<u8>(), VmObjectReadWriteOptions::NONE);
        assert_ok!(res);
        assert_eq!(read_actual, size_of::<u8>());
    }

    /// Test that page tables that do not get accessed can be successfully unmapped and freed.
    #[test]
    fn vmaspace_free_unaccessed_page_tables_test() {
        // Test that page tables that do not get accessed can be successfully unmapped and freed.

        // Disable for RISC-V for now, since the `ArchMmmu` code for this architecture currently
        // does not track accessed bits in intermediate page tables, and thus has no reasonable
        // way to honor `NonTerminalAction::FreeUnaccessed` on harvest calls.
        if cfg!(target_arch = "riscv64") {
            kprintln!("Skipping on RISC-V");
            return true;
        }

        let _scanner_disable = AutoVmScannerDisable::new();

        let num_pages: usize = 512 * 3;
        let middle_page: usize = num_pages / 2;
        let middle_offset: usize = middle_page * PAGE_SIZE_USIZE;
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            0,
            PAGE_SIZE * (num_pages as u64)
        ));

        // Construct an additional aspace to use for mappings and touching pages. This allows us to
        // control whether the aspace is considered active, which can effect reclamation and
        // scanning.
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));

        let aspace = DropGuard::new(aspace, |aspace| {
            let _ = aspace.destroy();
        });

        let mem = unwrap_some!(
            UserMemory::create_in_aspace(VmObjectPaged::into_vm_object(vmo), &aspace, 0, 0),
            "UserMemory::create_in_aspace"
        );

        // Put the state we need to share in a struct so we can easily share it with the thread.
        struct State<'a> {
            mem: &'a UserMemory,
            touch_event: &'a AutounsignalEvent,
            complete_event: &'a AutounsignalEvent,
            running: AtomicBool,
            middle_offset: usize,
        }

        stack_pin_init!(let touch_event = AutounsignalEvent::init_unsignaled());
        stack_pin_init!(let complete_event = AutounsignalEvent::init_unsignaled());

        let state = State {
            mem: &mem,
            touch_event: &touch_event,
            complete_event: &complete_event,
            running: AtomicBool::new(true),
            middle_offset,
        };

        // Spin up a kernel thread in the aspace we made. This thread will just continuously wait on
        // an event, touching the mapping whenever it is signaled.
        extern "C" fn thread_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a live `State` valid for the thread duration.
            let state = unsafe { arg.cast::<State<'_>>().as_ref_unchecked() };

            while state.running.load(Ordering::SeqCst) {
                let _ = state.touch_event.wait(&Deadline::infinite());
                // Check running again so we do not try and touch mem if attempting to shutdown
                // suddenly.
                if state.running.load(Ordering::SeqCst) {
                    state
                        .mem
                        .put::<u8>(42, state.middle_offset)
                        .expect("failed to put byte to UserMemory");
                    // Signal the event back
                    state.complete_event.signal();
                }
            }
            0
        }

        let arg = from_ref(&state).cast_mut().cast::<c_void>();
        // SAFETY: `thread_body` and `arg` are safe to execute on a new thread.
        let thread =
            unwrap_ok!(unsafe { thread::create(c"test-thread".as_ptr(), thread_body, arg) });
        // SAFETY: `thread` points to a live `Thread` that has not started running, no chain lock
        // transaction is active on this CPU, and a reference to `aspace` is held elsewhere while it
        // is assigned to the thread.
        unsafe { VmAspace::attach_to_thread(VmAspace::as_non_null(&aspace), thread) };
        // SAFETY: `thread` is valid and not yet joined.
        unsafe { thread.resume() };

        let _cleanup_thread = DropGuard::new(thread, |thread| {
            state.running.store(false, Ordering::SeqCst);
            state.touch_event.signal();
            // SAFETY: `thread` is valid and joined once.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        });

        // Helper to synchronously wait for the thread to perform a touch.
        let touch = || {
            state.touch_event.signal();
            let _ = state.complete_event.wait(&Deadline::infinite());
        };

        expect_ok!(mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Touch the mapping to ensure its accessed.
        touch();

        // Attempting to map should fail, as it's already mapped.
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        touch();
        // Harvest the accessed information, this should not actually unmap it, even if we ask it
        // to.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        touch();
        // Harvest the accessed information, then attempt to do it again so that it gets unmapped.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_ok!(mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Touch the mapping to ensure its accessed.
        touch();

        // Harvest the page accessed information, but retain the non-terminals.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        // We can do this a few times.
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        // Now if we attempt to free unaccessed the non-terminal should still be accessed and so
        // nothing should get unmapped.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        // If we are not requesting a free, then we should be able to harvest repeatedly.
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        harvest_access_bits(NonTerminalAction::Retain, TerminalAction::UpdateAgeAndHarvest);
    }

    /// Drop all user page tables while a unified aspace is in the aspace list.
    #[test]
    fn vmaspace_drop_all_user_page_tables_unified_test() {
        let private_aspace_base = USER_ASPACE_BASE;
        let private_aspace_size = USER_RESTRICTED_ASPACE_SIZE;
        let shared_aspace_base = private_aspace_base + private_aspace_size + PAGE_SIZE_USIZE;
        let shared_aspace_size = USER_ASPACE_BASE + USER_ASPACE_SIZE - shared_aspace_base;
        let restricted_aspace = unwrap_some!(VmAspace::create_with_opts(
            private_aspace_base,
            private_aspace_size,
            Type::User,
            b"test restricted aspace",
            ShareOpt::Restricted,
        ));
        let restricted_aspace = DropGuard::new(restricted_aspace, |restricted_aspace| {
            let _ = restricted_aspace.destroy();
        });
        let shared_aspace = unwrap_some!(VmAspace::create_with_opts(
            shared_aspace_base,
            shared_aspace_size,
            Type::User,
            b"test shared aspace",
            ShareOpt::Shared,
        ));
        let shared_aspace = DropGuard::new(shared_aspace, |shared_aspace| {
            let _ = shared_aspace.destroy();
        });
        // SAFETY: `shared_aspace` and `restricted_aspace` were created as shared and restricted
        // respectively, with `restricted_aspace` below `shared_aspace` and sharing no top-level
        // page table entry. `restricted_aspace` was just created and nothing has been mapped into
        // it or built from it, so it has no mappings and is not part of another unified aspace.
        // `_unified_aspace` destroys `unified_aspace`, which is its only reference, or panics,
        // which halts the kernel, and reverse declaration order drops it before the strong
        // references `shared_aspace` and `restricted_aspace`.
        let unified_aspace = unwrap_some!(unsafe {
            VmAspace::create_unified(&shared_aspace, &restricted_aspace, b"test unified aspace")
        });
        let _unified_aspace = DropGuard::new(unified_aspace, |unified_aspace| {
            unified_aspace.destroy().expect("destroy unified aspace");
        });
        VmAspace::drop_all_user_page_tables();
    }

    /// Touch mappings in a unified aspace and ensure accessed bits are correctly harvested.
    #[test]
    fn vmaspace_unified_accessed_test() {
        // Touch mappings in both the shared and restricted region of a unified aspace and ensure we
        // can correctly harvest accessed bits.

        // Disable for RISC-V for now, since the `ArchMmmu` code for this architecture currently
        // does not track accessed bits in intermediate page tables, and thus has no reasonable
        // way to honor `NonTerminalAction::FreeUnaccessed` on harvest calls.
        if cfg!(target_arch = "riscv64") {
            kprintln!("Skipping on RISC-V");
            return true;
        }

        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a unified aspace.
        let private_aspace_base = USER_ASPACE_BASE;
        let private_aspace_size = USER_RESTRICTED_ASPACE_SIZE;
        let shared_aspace_base = private_aspace_base + private_aspace_size + PAGE_SIZE_USIZE;
        let shared_aspace_size = USER_ASPACE_BASE + USER_ASPACE_SIZE - shared_aspace_base;
        let restricted_aspace = unwrap_some!(VmAspace::create_with_opts(
            private_aspace_base,
            private_aspace_size,
            Type::User,
            b"test restricted aspace",
            ShareOpt::Restricted,
        ));
        let restricted_aspace = DropGuard::new(restricted_aspace, |restricted_aspace| {
            let _ = restricted_aspace.destroy();
        });
        let shared_aspace = unwrap_some!(VmAspace::create_with_opts(
            shared_aspace_base,
            shared_aspace_size,
            Type::User,
            b"test shared aspace",
            ShareOpt::Shared,
        ));
        let shared_aspace = DropGuard::new(shared_aspace, |shared_aspace| {
            let _ = shared_aspace.destroy();
        });
        // SAFETY: `shared_aspace` and `restricted_aspace` were created as shared and restricted
        // respectively, with `restricted_aspace` below `shared_aspace` and sharing no top-level
        // page table entry. `restricted_aspace` was just created and nothing has been mapped into
        // it or built from it, so it has no mappings and is not part of another unified aspace.
        // The guard on `unified_aspace` destroys it, which is its only reference, or panics, which
        // halts the kernel, and reverse declaration order drops it before the strong references
        // `shared_aspace` and `restricted_aspace`.
        let unified_aspace = unwrap_some!(unsafe {
            VmAspace::create_unified(&shared_aspace, &restricted_aspace, b"test unified aspace")
        });

        let unified_aspace = DropGuard::new(unified_aspace, |unified_aspace| {
            unified_aspace.destroy().expect("destroy unified aspace");
        });

        // Create regions of user memory that we can touch in both the shared and
        // restricted regions.
        let size: u64 = 4 * PAGE_SIZE;
        let shared_vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, size));
        let restricted_vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, size));

        let shared_mem = unwrap_some!(UserMemory::create_in_aspace(
            VmObjectPaged::into_vm_object(shared_vmo),
            &shared_aspace,
            0,
            0,
        ));
        let restricted_mem = unwrap_some!(UserMemory::create_in_aspace(
            VmObjectPaged::into_vm_object(restricted_vmo),
            &restricted_aspace,
            0,
            0,
        ));

        // Commit and map these regions to avoid page faults when we call `put` later on. We
        // have to do this because the `put` function invokes a `copy_to_user` that may trigger a
        // page fault, which the fault handler will try to resolve using the thread's current
        // aspace. That aspace, in turn, will be the unified aspace, which cannot resolve faults.
        let middle_offset: usize = (size / 2) as usize;
        expect_ok!(shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
        expect_ok!(restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));

        // Switch to the unified aspace.
        // NOTE: This test takes care to not trigger any page faults or accessed faults from this
        // point on, because the unified aspace cannot resolve faults. For a user thread, we would
        // rely on the process dispatcher to look up if the faulting address lies in the shared
        // aspace or the restricted aspace, and use one of those to resolve the fault instead.
        // We cannot exercise that behavior from within a kernel unit test.
        // SAFETY: Running on an initialized thread.
        let old_aspace = unsafe { thread::current_active_aspace_ptr() };
        // SAFETY: `unified_aspace` remains valid while active.
        unsafe { vmm::set_active_aspace(Some(VmAspace::as_non_null(&unified_aspace))) };
        let _reset_old_aspace = DropGuard::new(old_aspace, |old_aspace| {
            // SAFETY: `old_aspace` remains valid while active.
            unsafe { vmm::set_active_aspace(old_aspace) };
        });

        #[cfg(target_arch = "x86_64")]
        {
            // Touch the shared and restricted regions via the unified aspace. This will
            // guarantee that the accessed bits are set on x86, where the hardware sets the
            // accessed bits. On ARM, where we use software managed accessed bits, the
            // `commit_and_map` above will already have set them.
            unwrap_ok!(shared_mem.put::<u8>(42, middle_offset));
            unwrap_ok!(restricted_mem.put::<u8>(42, middle_offset));
        }

        // Harvest the accessed information. This should not actually unmap the pages.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_err!(
            shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );
        expect_err!(
            restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE),
            Status::ALREADY_EXISTS
        );

        #[cfg(target_arch = "x86_64")]
        {
            // Touch the memory again so that the accessed bits are guaranteed to be set.
            // We must do this because `commit_and_map` does not set the accessed flag on x86.
            //
            // We specifically want to avoid doing this on ARM because the harvest will have
            // cleared accessed bits, and this `put` now will trigger an accessed fault on the
            // unified aspace, which cannot resolve any faults. Moreover, the `commit_and_map`
            // above will have already set the non-terminal accessed bits on the walk down
            // to the page.
            unwrap_ok!(shared_mem.put::<u8>(43, middle_offset));
            unwrap_ok!(restricted_mem.put::<u8>(43, middle_offset));
        }

        // Harvest the accessed information, then attempt to do it again so that it gets
        // unmapped. The first `harvest_access_bits` call will clear the accessed bits, and
        // the second will unmap the memory.
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        harvest_access_bits(NonTerminalAction::FreeUnaccessed, TerminalAction::UpdateAgeAndHarvest);
        expect_ok!(shared_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
        expect_ok!(restricted_mem.commit_and_map(middle_offset..middle_offset + PAGE_SIZE_USIZE));
    }

    /// Tests that VmMappings that are marked mergeable behave correctly.
    #[test]
    fn vmaspace_merge_mapping_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));

        // Create a sub VMAR we'll use for all our testing.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        // Create two different vmos to make mappings into.
        let vmo1 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));
        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        // Declare some enums to make writing test cases more readable instead of having lots of
        // bools.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum MmuFlags {
            FlagType1,
            FlagType2,
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[allow(dead_code)]
        enum MarkMerge {
            Merge,
            NoMerge,
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum MergeResult {
            MergesLeft,
            DoesNotMerge,
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum BeyondStreamSize {
            Ok,
            Fault,
        }

        // To avoid boilerplate declare some tests in a data driven way.
        #[derive(Clone)]
        struct MappingDesc {
            vmar_offset: usize,
            vmo: Option<RefPtr<VmObjectPaged>>,
            vmo_offset: u64,
            flags: MmuFlags,
            beyond_stream_size: BeyondStreamSize,
            merge_result: MergeResult,
        }

        impl Default for MappingDesc {
            fn default() -> Self {
                Self {
                    vmar_offset: 0,
                    vmo: None,
                    vmo_offset: 0,
                    flags: MmuFlags::FlagType1,
                    beyond_stream_size: BeyondStreamSize::Ok,
                    merge_result: MergeResult::DoesNotMerge,
                }
            }
        }

        struct TestCase {
            mappings: [MappingDesc; 3],
        }

        let cases = [
            // Simple two mapping merge
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::MergesLeft,
                    },
                    MappingDesc::default(),
                ],
            },
            // Simple three mapping merge
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::MergesLeft,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE * 2,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE * 2,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::MergesLeft,
                    },
                ],
            },
            // Different mapping flags should block merge
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType2,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE * 2,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE * 2,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::MergesLeft,
                    },
                ],
            },
            // Discontiguous aspace, but contiguous vmo should not work.
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE * 2,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc::default(),
                ],
            },
            // Similar discontiguous vmo, but contiguous aspace should not work.
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE * 2,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc::default(),
                ],
            },
            // Leaving a contiguous hole also does not work, mapping needs to actually join.
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE * 2,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE * 2,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc::default(),
                ],
            },
            // Different vmo should not work.
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo2.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE * 2,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE * 2,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::MergesLeft,
                    },
                ],
            },
            // Two fault-beyond-stream-size mapping merge
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Fault,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Fault,
                        merge_result: MergeResult::MergesLeft,
                    },
                    MappingDesc::default(),
                ],
            },
            // Can't merge adjacent mappings if only one has fault-beyond-stream-size.
            TestCase {
                mappings: [
                    MappingDesc {
                        vmar_offset: 0,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: 0,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Fault,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc {
                        vmar_offset: PAGE_SIZE_USIZE,
                        vmo: Some(vmo1.clone()),
                        vmo_offset: PAGE_SIZE,
                        flags: MmuFlags::FlagType1,
                        beyond_stream_size: BeyondStreamSize::Ok,
                        merge_result: MergeResult::DoesNotMerge,
                    },
                    MappingDesc::default(),
                ],
            },
        ];

        for test in &cases {
            // Want to test all combinations of placing the mappings in subvmars, we just choose
            // this by iterating all the binary representations of 3 digits.
            for sub_vmar_combination in 0..0b1000 {
                let use_subvmar = [
                    (sub_vmar_combination & (1 << 0)) != 0,
                    (sub_vmar_combination & (1 << 1)) != 0,
                    (sub_vmar_combination & (1 << 2)) != 0,
                ];
                // Iterate all orders of marking mergeable. For 3 mappings there are 6
                // possibilities.
                for merge_order_combination in 0..6 {
                    let even_merge = (merge_order_combination % 2) == 0;
                    let first_merge = merge_order_combination / 2;
                    let merge_order = [
                        first_merge,
                        (first_merge + if even_merge { 1 } else { 2 }) % 3,
                        (first_merge + if even_merge { 2 } else { 1 }) % 3,
                    ];

                    // Instantiate the requested mappings.
                    let mut vmars: [Option<RefPtr<VmAddressRegion>>; 3] = [None, None, None];
                    let mut mappings: [Option<RefPtr<VmMapping>>; 3] = [None, None, None];
                    let mut merge_result = [
                        MergeResult::DoesNotMerge,
                        MergeResult::DoesNotMerge,
                        MergeResult::DoesNotMerge,
                    ];
                    for i in 0..3 {
                        if let Some(ref vmo) = test.mappings[i].vmo {
                            let mmu_flags = ARCH_MMU_FLAG_PERM_READ
                                | if test.mappings[i].flags == MmuFlags::FlagType1 {
                                    ARCH_MMU_FLAG_PERM_WRITE
                                } else {
                                    0
                                };
                            let vmar_flags = vmar::flag::SPECIFIC
                                | if test.mappings[i].beyond_stream_size == BeyondStreamSize::Fault
                                {
                                    vmar::flag::FAULT_BEYOND_STREAM_SIZE
                                } else {
                                    0
                                };
                            if use_subvmar[i] {
                                let sub = unwrap_ok!(vmar.create_sub_vmar(
                                    test.mappings[i].vmar_offset,
                                    PAGE_SIZE_USIZE,
                                    0,
                                    vmar::flag::SPECIFIC
                                        | vmar::flag::CAN_MAP_SPECIFIC
                                        | vmar::flag::CAN_MAP_READ
                                        | vmar::flag::CAN_MAP_WRITE,
                                    b"sub vmar",
                                ));
                                let map_result = unwrap_ok!(sub.create_vm_mapping_no_overwrite(
                                    0,
                                    PAGE_SIZE_USIZE,
                                    0,
                                    vmar_flags,
                                    VmObjectPaged::into_vm_object(vmo.clone()),
                                    test.mappings[i].vmo_offset,
                                    mmu_flags,
                                ));
                                vmars[i] = Some(sub);
                                mappings[i] = Some(map_result.mapping);
                            } else {
                                let map_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
                                    test.mappings[i].vmar_offset,
                                    PAGE_SIZE_USIZE,
                                    0,
                                    vmar_flags,
                                    VmObjectPaged::into_vm_object(vmo.clone()),
                                    test.mappings[i].vmo_offset,
                                    mmu_flags,
                                ));
                                mappings[i] = Some(map_result.mapping);
                            }
                        }
                        // By default we assume merging happens as declared in the test, unless
                        // either this or our immediate left is in a subvmar, in which case merging
                        // is blocked.
                        if use_subvmar[i] || (i > 0 && use_subvmar[i - 1]) {
                            merge_result[i] = MergeResult::DoesNotMerge;
                        } else {
                            merge_result[i] = test.mappings[i].merge_result;
                        }
                    }

                    // As we merge track expected mapping sizes and what we have merged
                    let mut merged = [false, false, false];
                    let mut expected_size = [PAGE_SIZE_USIZE, PAGE_SIZE_USIZE, PAGE_SIZE_USIZE];
                    // Mark each mapping as mergeable based on merge_order
                    for &mapping in &merge_order {
                        if test.mappings[mapping].vmo.is_some() {
                            VmMapping::mark_mergeable(mappings[mapping].take().unwrap());
                            merged[mapping] = true;
                            // See if we have anything pending from the right
                            if mapping < 2
                                && merged[mapping + 1]
                                && merge_result[mapping + 1] == MergeResult::MergesLeft
                            {
                                expected_size[mapping] += expected_size[mapping + 1];
                                expected_size[mapping + 1] = 0;
                            }
                            // See if we should merge to the left.
                            if merge_result[mapping] == MergeResult::MergesLeft
                                && mapping > 0
                                && merged[mapping - 1]
                            {
                                if expected_size[mapping - 1] == 0 {
                                    expected_size[mapping - 2] += expected_size[mapping];
                                } else {
                                    expected_size[mapping - 1] += expected_size[mapping];
                                }
                                expected_size[mapping] = 0;
                            }
                        }
                        // Validate sizes to ensure any expected merging happened.
                        for j in 0..3 {
                            if test.mappings[j].vmo.is_some() {
                                stack_pin_init!(let guard = vmar.lock().lock());
                                let map = unwrap_some!(vmar.find_mapping_locked(
                                    guard.token(),
                                    VAddr(test.mappings[j].vmar_offset + vmar.base().0),
                                ));
                                if expected_size[j] != 0 {
                                    expect_eq!(map.size(), expected_size[j]);
                                    expect_eq!(
                                        map.base().0,
                                        vmar.base().0 + test.mappings[j].vmar_offset
                                    );
                                }
                            }
                        }
                    }

                    // Destroy any mappings and VMARs.
                    for mapping in &test.mappings {
                        if mapping.vmo.is_some() {
                            // SAFETY: Unmapping test mappings in our test VMAR.
                            expect_ok!(unsafe {
                                vmar.unmap(
                                    VAddr(vmar.base().0 + mapping.vmar_offset),
                                    PAGE_SIZE_USIZE,
                                    VmAddressRegionOpChildren::Yes,
                                )
                            });
                        }
                    }
                }
            }
        }

        // Cleanup the address space.
        expect_ok!(vmar.destroy());
        expect_ok!(aspace.destroy());
    }

    /// Tests sparse VM mappings with an empty backing VMO.
    #[test]
    fn vm_mapping_sparse_mapping_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a large memory mapping with an empty backing VMO. Although this is a large virtual
        // address range, our later attempts to map it should be efficient.
        let memory_size = 16 * GB;
        let memory = UserMemory::create(memory_size).unwrap();

        // Memory backing the user memory is currently empty, so attempting to map in it should
        // succeed, albeit with nothing populated.
        expect_ok!(memory.map_existing(0..memory_size));

        // Commit a page in the middle, then re-map the whole thing and ensure the mapping is there.
        let val = 42u64;
        expect_ok!(
            memory.vmo_write(&val.to_ne_bytes()[..size_of_val(&val)], (memory_size / 2) as u64,)
        );
        expect_ok!(memory.map_existing(0..memory_size));
        expect_eq!(val, unwrap_ok!(memory.get::<u64>(memory_size / 2 / size_of::<u64>())));

        // Do the same test, but this time with the pages at the start and end of the range.
        expect_ok!(memory.vmo_write(&val.to_ne_bytes()[..size_of_val(&val)], 0));
        expect_ok!(memory.vmo_write(
            &val.to_ne_bytes()[..size_of_val(&val)],
            (memory_size - PAGE_SIZE_USIZE) as u64,
        ));
        expect_ok!(memory.map_existing(0..memory_size));
        expect_eq!(val, unwrap_ok!(memory.get::<u64>(0)));
        expect_eq!(
            val,
            unwrap_ok!(memory.get::<u64>((memory_size - PAGE_SIZE_USIZE) / size_of::<u64>()))
        );
    }

    /// Tests memory priority propagation with pager-backed VMOs and clones.
    #[test]
    fn vmaspace_priority_pager_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let (vmo, _) = unwrap_ok!(make_committed_pager_vmo::<1>(false, false));

        // Create a clone of the VMO.
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");

        // Map in the clone.
        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let _mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                vmo_child.clone(),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        // Validate the root and clone received the priority.
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Create a second child of the root.
        let vmo_child2: Option<RefPtr<VmObject>> = None;
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp2 = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");

        // This child should not have any priority.
        expect_false!(childp2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Destroy it should leave the rest of the tree unchanged.
        drop(vmo_child2);
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Remove priority and validate.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::Default));

        expect_false!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority propagation for VMO references.
    #[test]
    fn vmaspace_priority_reference_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace").expect("VmAspace::create failed");

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a reference of the VMO.
        let (vmo_reference, _first_child) =
            unwrap_ok!(vmo.create_child_reference(Resizability::NonResizable, 0, 0, true));
        let refp = VmObject::downcast_paged(vmo_reference.clone()).expect("is paged");

        // Reference should have same priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Remove the original mapping.
        let _ = mapping_result.mapping.destroy();
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Now map in the reference.
        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                vmo_reference,
                0,
                ARCH_RW_USER_FLAGS,
            )
        });
        let _ = mapping_result;

        // Reference and vmo should have same priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(refp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests that memory attribution and usage reporting work in a nested aspace hierarchy.
    #[test]
    fn vmaspace_nested_attribution_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let aspace =
            unwrap_some!(VmAspace::create(Type::User, b"test-aspace"), "VmAspace::Create pointer");
        let root_vmar = unwrap_some!(aspace.root_vmar(), "RootVmar pointer");

        // 8 page vmar.
        let vmar = unwrap_ok!(root_vmar.create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 8,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        // Child vmar that covers the first 4 pages of the previous vmar.
        let subvmar1 = unwrap_ok!(vmar.create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        // Grandchild vmar that covers the first 2 pages of the child.
        let subvmar2 = unwrap_ok!(subvmar1.create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 2,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        // Make 2 page vmo.
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::RESIZABLE,
            2 * PAGE_SIZE,
        ));

        // Map VMO to grandchild.
        expect_true!(aspace.is_user());
        let mapping_result = unwrap_ok!(subvmar2.create_vm_mapping_no_overwrite(
            0,
            2 * PAGE_SIZE_USIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ));

        // Before committing pages, only mapped_bytes is non-zero.
        expect_true!(
            unwrap_ok!(aspace.get_memory_usage())
                == VmUsage {
                    mapped_bytes: 2 * PAGE_SIZE_USIZE,
                    private_bytes: 0,
                    shared_bytes: 0,
                    scaled_shared_bytes: FractionalBytes::default(),
                }
        );

        // Commit 2 pages into mapping.
        assert_ok!(vmo.commit_range(0, 2 * PAGE_SIZE));

        // Verify that the two pages are counted for the parent vmar chain.
        let expected_counts = make_private_attribution_counts(2 * PAGE_SIZE, 0);
        assert_true!(expected_counts == mapping_result.mapping.get_attributed_memory());
        assert_true!(expected_counts == subvmar2.get_attributed_memory());
        assert_true!(expected_counts == subvmar1.get_attributed_memory());
        assert_true!(expected_counts == vmar.get_attributed_memory());
        assert_true!(expected_counts == root_vmar.get_attributed_memory());

        // Verify VmAspace::get_memory_usage reports the 2 private pages.
        expect_true!(
            unwrap_ok!(aspace.get_memory_usage())
                == VmUsage {
                    mapped_bytes: 2 * PAGE_SIZE_USIZE,
                    private_bytes: 2 * PAGE_SIZE_USIZE,
                    shared_bytes: 0,
                    scaled_shared_bytes: FractionalBytes::default(),
                }
        );

        // Share the first page via a copy-on-write clone so all four VmUsage fields are distinct.
        let clone = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true,
        ));
        expect_true!(
            unwrap_ok!(aspace.get_memory_usage())
                == VmUsage {
                    mapped_bytes: 2 * PAGE_SIZE_USIZE,
                    private_bytes: PAGE_SIZE_USIZE,
                    shared_bytes: PAGE_SIZE_USIZE,
                    scaled_shared_bytes: FractionalBytes::from_fraction(PAGE_SIZE, 2),
                }
        );
        drop(clone);

        // Destroy the aspace and verify get_memory_usage fails with INTERNAL.
        assert_ok!(aspace.destroy());
        expect_err!(aspace.get_memory_usage(), Status::INTERNAL);
    }

    /// Tests VmMapping memory attribution under commits and decommits on the VMO range.
    #[test]
    fn vm_mapping_attribution_commit_decommit_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a test VmAspace to temporarily switch to for creating test mappings.
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));
        let mut cleanup_aspace = zr::defer(|| {
            aspace.destroy().expect("destroy test aspace");
        });

        // Create a VMO to map.
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::RESIZABLE,
            16 * PAGE_SIZE,
        ));

        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));

        // Map the left half of the VMO.
        expect_eq!(aspace.is_user(), true);
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let mapping = unwrap_ok!(root_vmar.create_vm_mapping_no_overwrite(
            0,
            8 * PAGE_SIZE_USIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ))
        .mapping;

        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));
        expect_true!(mapping.get_attributed_memory() == AttributionCounts::default());

        // Commit pages a little into the mapping, and past it.
        let status = vmo.commit_range(4 * PAGE_SIZE, 8 * PAGE_SIZE);
        assert_ok!(status);
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(4 * PAGE_SIZE, 0)
        );

        // Decommit the pages committed above, returning the VMO to zero committed pages.
        let status = vmo.decommit_range(4 * PAGE_SIZE, 8 * PAGE_SIZE);
        assert_ok!(status);
        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));
        expect_true!(mapping.get_attributed_memory() == AttributionCounts::default());

        // Commit some pages in the VMO again.
        let status = vmo.commit_range(0, 10 * PAGE_SIZE);
        assert_ok!(status);
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(10 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 10 * PAGE_SIZE));
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );

        // Decommit pages in the vmo via the mapping.
        let status = mapping.decommit_range(0, mapping.size());
        assert_ok!(status);
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(2 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 2 * PAGE_SIZE));
        expect_true!(mapping.get_attributed_memory() == AttributionCounts::default());

        // Destroy the mapping.
        let status = mapping.destroy();
        assert_ok!(status);
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(2 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 2 * PAGE_SIZE));
        expect_true!(AttributionCounts::default() == mapping.get_attributed_memory());

        // Free the test address space.
        cleanup_aspace.cancel();
        let status = aspace.destroy();
        expect_ok!(status);
    }

    /// Tests VmMapping memory attribution under map and unmap operations on the mapping.
    #[test]
    fn vm_mapping_attribution_map_unmap_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a test VmAspace to temporarily switch to for creating test mappings.
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));
        let mut cleanup_aspace = zr::defer(|| {
            aspace.destroy().expect("destroy test aspace");
        });

        // Create a VMO to map.
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::RESIZABLE,
            16 * PAGE_SIZE,
        ));

        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));

        // Map the left half of the VMO.
        expect_eq!(aspace.is_user(), true);
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let mut mapping = unwrap_ok!(root_vmar.create_vm_mapping_no_overwrite(
            0,
            8 * PAGE_SIZE_USIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ))
        .mapping;

        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));
        expect_true!(mapping.get_attributed_memory() == AttributionCounts::default());

        // Commit pages in the vmo via the mapping.
        let status = mapping.map_range(0, mapping.size(), true, false);
        assert_ok!(status);
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 8 * PAGE_SIZE));
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );

        // Unmap from the right end of the mapping.
        let old_base = mapping.base().0;
        // SAFETY: The unmapped page is a page-aligned strict subrange of a mapping in an isolated
        // test user address space, so nothing accesses it or holds a Rust reference into it.
        let status = unsafe {
            mapping
                .debug_unmap(mapping.base().0 + mapping.size() - PAGE_SIZE_USIZE, PAGE_SIZE_USIZE)
        };
        assert_ok!(status);
        let region = unwrap_some!(aspace.find_region(old_base));
        mapping = unwrap_some!(region.into_vm_mapping());
        expect_eq!(old_base, mapping.base().0);
        assert_eq!(7 * PAGE_SIZE_USIZE, mapping.size());
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 8 * PAGE_SIZE));
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(7 * PAGE_SIZE, 0)
        );

        // Unmap from the center of the mapping.
        // SAFETY: The unmapped page is a page-aligned strict subrange of a mapping in an isolated
        // test user address space, so nothing accesses it or holds a Rust reference into it.
        let status =
            unsafe { mapping.debug_unmap(mapping.base().0 + 4 * PAGE_SIZE_USIZE, PAGE_SIZE_USIZE) };
        assert_ok!(status);
        let region = unwrap_some!(aspace.find_region(old_base));
        mapping = unwrap_some!(region.into_vm_mapping());
        expect_eq!(old_base, mapping.base().0);
        assert_eq!(4 * PAGE_SIZE_USIZE, mapping.size());
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 8 * PAGE_SIZE));
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(4 * PAGE_SIZE, 0)
        );

        // Unmap from the left end of the mapping.
        // SAFETY: The unmapped page is a page-aligned strict subrange of a mapping in an isolated
        // test user address space, so nothing accesses it or holds a Rust reference into it.
        let status = unsafe { mapping.debug_unmap(mapping.base().0, PAGE_SIZE_USIZE) };
        assert_ok!(status);
        let region = unwrap_some!(aspace.find_region(old_base + PAGE_SIZE_USIZE));
        mapping = unwrap_some!(region.into_vm_mapping());
        expect_ne!(old_base, mapping.base().0);
        expect_eq!(3 * PAGE_SIZE_USIZE, mapping.size());
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(8 * PAGE_SIZE, 0)
        );
        expect_true!(verify_continuous_attribution_bytes(&vmo, 8 * PAGE_SIZE));
        expect_true!(
            mapping.get_attributed_memory() == make_private_attribution_counts(3 * PAGE_SIZE, 0)
        );

        // Free the test address space.
        cleanup_aspace.cancel();
        let status = aspace.destroy();
        expect_ok!(status);
    }

    /// Tests VmMapping memory attribution when adjacent mappings are merged.
    #[test]
    fn vm_mapping_attribution_merge_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a test VmAspace to temporarily switch to for creating test mappings.
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));
        let mut cleanup_aspace = zr::defer(|| {
            aspace.destroy().expect("destroy test aspace");
        });
        expect_eq!(aspace.is_user(), true);
        let root_vmar = unwrap_some!(aspace.root_vmar());

        // Create a VMO to map.
        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::RESIZABLE,
            16 * PAGE_SIZE,
        ));

        expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
        expect_true!(verify_continuous_attribution_bytes(&vmo, 0));

        // Create some contiguous mappings, marked unmergeable (default behavior) to begin with.
        #[derive(Default)]
        struct MappingEntry {
            ref_: Option<RefPtr<VmMapping>>,
            expected_attribution_counts: AttributionCounts,
        }
        let mut mappings: [MappingEntry; 4] = Default::default();

        let mut offset: u64 = 0;
        const SIZE: u64 = 4 * PAGE_SIZE;
        for mapping_entry in &mut mappings {
            let mapping_result = unwrap_ok!(root_vmar.create_vm_mapping_no_overwrite(
                offset as usize,
                SIZE as usize,
                0,
                vmar::flag::SPECIFIC,
                VmObjectPaged::into_vm_object(vmo.clone()),
                offset,
                ARCH_RW_USER_FLAGS,
            ));
            mapping_entry.ref_ = Some(mapping_result.mapping);
            expect_true!(vmo.get_attributed_memory() == AttributionCounts::default());
            expect_true!(verify_continuous_attribution_bytes(&vmo, 0));
            expect_true!(
                unwrap_some!(mapping_entry.ref_.as_ref()).get_attributed_memory()
                    == mapping_entry.expected_attribution_counts
            );
            offset += SIZE;
        }
        expect_eq!(offset, 16 * PAGE_SIZE);

        // Commit pages in the VMO.
        let status = vmo.commit_range(0, 16 * PAGE_SIZE);
        assert_ok!(status);
        for mapping_entry in &mut mappings {
            mapping_entry.expected_attribution_counts =
                make_private_attribution_counts(4 * PAGE_SIZE, 0);
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(16 * PAGE_SIZE, 0)
            );
            expect_true!(
                unwrap_some!(mapping_entry.ref_.as_ref()).get_attributed_memory()
                    == mapping_entry.expected_attribution_counts
            );
        }

        // Mark mappings 0 and 2 mergeable. This should not change anything since they're separated
        // by an unmergeable mapping.
        VmMapping::mark_mergeable(unwrap_some!(mappings[0].ref_.take()));
        VmMapping::mark_mergeable(unwrap_some!(mappings[2].ref_.take()));
        let root_base = root_vmar.base().0;
        for (i, mapping_entry) in mappings.iter().enumerate() {
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(16 * PAGE_SIZE, 0)
            );
            let region = unwrap_some!(aspace.find_region(root_base + (SIZE as usize) * i));
            let map = unwrap_some!(region.into_vm_mapping());
            expect_true!(map.get_attributed_memory() == mapping_entry.expected_attribution_counts);
        }

        // Mark mapping 3 mergeable. This will merge mappings 2 and 3, destroying mapping 3 and
        // moving all of its pages into mapping 2.
        VmMapping::mark_mergeable(unwrap_some!(mappings[3].ref_.take()));
        let counts3 = mappings[3].expected_attribution_counts;
        mappings[2].expected_attribution_counts += counts3;
        for (i, mapping_entry) in mappings.iter().enumerate().take(3) {
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(16 * PAGE_SIZE, 0)
            );
            let region = unwrap_some!(aspace.find_region(root_base + (SIZE as usize) * i));
            let map = unwrap_some!(region.into_vm_mapping());
            expect_true!(map.get_attributed_memory() == mapping_entry.expected_attribution_counts);
        }

        // Mark mapping 1 mergeable. This will merge mappings 0, 1 and 2, with only mapping 0
        // surviving the merge. All the VMO's pages will have been moved to mapping 0.
        VmMapping::mark_mergeable(unwrap_some!(mappings[1].ref_.take()));
        let counts1 = mappings[1].expected_attribution_counts;
        let counts2 = mappings[2].expected_attribution_counts;
        mappings[0].expected_attribution_counts += counts1;
        mappings[0].expected_attribution_counts += counts2;
        expect_true!(
            vmo.get_attributed_memory() == make_private_attribution_counts(16 * PAGE_SIZE, 0)
        );
        let region = unwrap_some!(aspace.find_region(root_base));
        let map = unwrap_some!(region.into_vm_mapping());
        expect_true!(map.get_attributed_memory() == mappings[0].expected_attribution_counts);

        // Free the test address space.
        cleanup_aspace.cancel();
        let status = aspace.destroy();
        expect_ok!(status);
    }

    /// Tests optimistic page fault extension up to `PAGE_FAULT_MAX_OPTIMISTIC_PAGES`.
    #[test]
    fn vm_mapping_page_fault_optimisation_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        const MAX_OPT_PAGES: usize = VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES;

        // Size the allocation of the VMO / mapping to be double the optimistic extension so we can
        // validate that it is limited by the optimistic cap, not the size of the VMO.
        const ALLOC_SIZE: usize = MAX_OPT_PAGES * 2 * PAGE_SIZE_USIZE;
        const ALIGN_POW2: u8 = ALLOC_SIZE.ilog2() as u8;

        // Mapped & fully committed VMO.
        let committed_vmo =
            unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, ALLOC_SIZE as u64,));

        let mapping = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(committed_vmo.clone()),
            0,
            ALIGN_POW2,
        ));

        assert_ok!(committed_vmo.commit_range(0, ALLOC_SIZE as u64));

        // Trigger a page fault on the first page in the VMO/Mapping.
        assert_ok!(mapping.put::<i32>(42, 0));

        // Optimisation will fault the minimum of MAX_OPT_PAGES pages and the end of the VMO,
        // protection range, mapping or page table. We have ensured that all of these will be >
        // MAX_OPT_PAGES in this case.
        // SAFETY: Unit tests run on an initialized kernel thread with a valid active `VmAspace`.
        assert_true!(unsafe {
            verify_mapped_page_range(mapping.base(), ALLOC_SIZE, MAX_OPT_PAGES)
        });

        // Mapped but not committed VMO.
        let uncommitted_vmo =
            unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, ALLOC_SIZE as u64,));

        let mapping2 = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(uncommitted_vmo),
            0,
            ALIGN_POW2,
        ));

        // Trigger a page fault on the first page in the VMO/Mapping.
        assert_ok!(mapping2.put::<i32>(42, 0));

        // As the VMO is uncommitted, only the requested page should have been faulted.
        // SAFETY: Unit tests run on an initialized kernel thread with a valid active `VmAspace`.
        assert_true!(unsafe { verify_mapped_page_range(mapping2.base(), ALLOC_SIZE, 1) });

        // Single committed page.
        let onepage_committed_vmo =
            unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, ALLOC_SIZE as u64,));

        let mapping3 = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(onepage_committed_vmo.clone()),
            0,
            ALIGN_POW2,
        ));

        assert_ok!(onepage_committed_vmo.commit_range(0, PAGE_SIZE));

        // Trigger a page fault on the first page in the VMO/Mapping.
        assert_ok!(mapping3.put::<i32>(42, 0));

        // Only the requested page should have been faulted.
        // SAFETY: Unit tests run on an initialized kernel thread with a valid active `VmAspace`.
        assert_true!(unsafe { verify_mapped_page_range(mapping3.base(), ALLOC_SIZE, 1) });

        // 4 committed pages.
        zr::static_assert!(4 <= MAX_OPT_PAGES);
        let partially_committed_vmo =
            unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, ALLOC_SIZE as u64,));

        let mapping4 = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(partially_committed_vmo.clone()),
            0,
            ALIGN_POW2,
        ));

        assert_ok!(partially_committed_vmo.commit_range(0, 4 * PAGE_SIZE));

        // Trigger a page fault on the first page in the VMO/Mapping.
        assert_ok!(mapping4.put::<i32>(42, 0));

        // Only the already committed pages should be committed.
        // SAFETY: Unit tests run on an initialized kernel thread with a valid active `VmAspace`.
        assert_true!(unsafe { verify_mapped_page_range(mapping4.base(), ALLOC_SIZE, 4) });
    }

    /// Validate that the page fault optimisation correctly respects page table boundaries.
    #[test]
    fn vm_mapping_page_fault_optimization_pt_limit_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        const MAX_OPT_PAGES: usize = VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES;
        // Size our top level vmar allocation to be two page tables in size, ensuring that we will
        // both have a page table boundary crossing in the allocation, as well as some amount of
        // allocation on either side of it.
        let page_table_size: usize = ArchVmAspace::next_user_page_table_offset(0);
        let vmar_size: usize = page_table_size * 2;
        // Align our allocation on a page table boundary, ensuring we have 1 page table worth of
        // space before and after our PT crossing point.
        let vmar_align: u8 = page_table_size.ilog2() as u8;
        // Size the allocation of the VMO / mapping to be double the optimistic extension so we can
        // validate that it is limited by the optimistic cap, not the size of the VMO.
        const MAP_SIZE: usize = MAX_OPT_PAGES * 2 * PAGE_SIZE_USIZE;

        // Allocate our large top level vmar in root vmar of the current aspace.
        // SAFETY: Unit tests execute on a valid kernel thread whose active address space remains
        // valid for the duration of this test.
        let active_aspace = unwrap_some!(unsafe { thread::current_active_aspace() });
        let root_vmar = unwrap_some!(active_aspace.root_vmar());
        let vmar = unwrap_ok!(root_vmar.create_sub_vmar(
            0,
            vmar_size,
            vmar_align,
            vmar::flag::CAN_MAP_READ
                | vmar::flag::CAN_MAP_WRITE
                | vmar::flag::CAN_MAP_EXECUTE
                | vmar::flag::CAN_MAP_SPECIFIC,
            b"unittest",
        ));
        let _cleanup_vmar = zr::defer(|| {
            vmar.destroy().expect("destroy vmar");
        });

        let next_pt_base = ArchVmAspace::next_user_page_table_offset(vmar.base().0);
        // If our alignment was specified correctly the next pt should be exactly one pt from our
        // base.
        assert_eq!(vmar.base().0 + page_table_size, next_pt_base);

        // Try touching at different distances from the start of the next page table and validate
        // that mappings are not added beyond it.
        for page_offset in 0..=(MAX_OPT_PAGES + 1) {
            // Create a subvmar at the correct offset that will precisely hold our mapping.
            let offset = page_table_size - PAGE_SIZE_USIZE * page_offset;
            let sub_vmar = unwrap_ok!(vmar.create_sub_vmar(
                offset,
                MAP_SIZE,
                0,
                vmar::flag::CAN_MAP_READ
                    | vmar::flag::CAN_MAP_WRITE
                    | vmar::flag::CAN_MAP_EXECUTE
                    | vmar::flag::SPECIFIC,
                b"unittest",
            ));
            let _cleanup_sub_vmar = zr::defer(|| {
                sub_vmar.destroy().expect("destroy sub_vmar");
            });

            let committed_vmo =
                unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, MAP_SIZE as u64,));

            let mapping = unwrap_some!(UserMemory::create_in_vmar(
                VmObjectPaged::into_vm_object(committed_vmo.clone()),
                sub_vmar.clone(),
                0,
                0,
            ));

            assert_ok!(committed_vmo.commit_range(0, MAP_SIZE as u64));

            // Trigger a page fault on the first page of the mapping.
            assert_ok!(mapping.put::<i32>(42, 0));

            // We expect the number of pages that are mapped in to be clipped at the page table
            // boundary, which would be `page_offset`. The two exceptions to this are if
            // page_offset is greater than MAX_OPT_PAGES, in which case that becomes the cap, or if
            // the page_offset is 0, in which case we are actually at the *start* of the next page
            // table, and so MAX_OPT_PAGES should get mapped.
            let expected_pages = if page_offset == 0 {
                MAX_OPT_PAGES
            } else {
                core::cmp::min(MAX_OPT_PAGES, page_offset)
            };

            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            assert_true!(unsafe {
                verify_mapped_page_range(mapping.base(), MAP_SIZE, expected_pages)
            });
        }
    }

    /// Tests faulting page ranges via `soft_fault_in_range`, `page_fault_locked`, and `read_user`.
    #[test]
    fn vm_mapping_page_fault_range_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        const TEST_PAGES: usize = VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES * 2;
        const ALLOC_SIZE: usize = TEST_PAGES * PAGE_SIZE_USIZE;
        const READ_FLAGS: u32 = pf_flag::USER;
        const WRITE_FLAGS: u32 = pf_flag::USER | pf_flag::WRITE;
        // Aligning the mapping is for when testing the optimistic fault handler to ensure that
        // there are no spurious failures due to crossing a page table boundary.
        const ALIGN_POW2: u8 = ALLOC_SIZE.ilog2() as u8;

        let vmo = unwrap_ok!(VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::RESIZABLE,
            ALLOC_SIZE as u64,
        ));

        let mapping = unwrap_some!(UserMemory::create_from_vmo(
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ALIGN_POW2,
        ));

        // Faulting even 1 additional page should prevent optimistic faulting.
        {
            // Decommit and recommit the VMO to ensure no page table mappings.
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            expect_ok!(vmo.commit_range(0, ALLOC_SIZE as u64));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });

            // Fault a two page range should only give two pages.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), READ_FLAGS, PAGE_SIZE_USIZE * 2)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 2) });

            // Reset and fault a single page to validate optimistic faulting would otherwise have
            // happened.
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            expect_ok!(vmo.commit_range(0, ALLOC_SIZE as u64));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), READ_FLAGS, PAGE_SIZE_USIZE)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(
                    mapping.base(),
                    ALLOC_SIZE,
                    VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES,
                )
            });
        }

        // Will map in pages that are not committed on read without allocating.
        {
            // Start with one page committed.
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            expect_ok!(vmo.commit_range(0, PAGE_SIZE));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(PAGE_SIZE, 0)
            );

            // Read faulting the range should map without allocating.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), READ_FLAGS, ALLOC_SIZE)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(mapping.base(), ALLOC_SIZE, TEST_PAGES)
            });
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(PAGE_SIZE, 0)
            );
        }

        // Write faulting should cause allocations
        {
            // Start with one page committed.
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            expect_ok!(vmo.commit_range(0, PAGE_SIZE));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });
            expect_true!(
                vmo.get_attributed_memory() == make_private_attribution_counts(PAGE_SIZE, 0)
            );

            // Write faulting the range should both map and allocate the pages.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), WRITE_FLAGS, ALLOC_SIZE)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(mapping.base(), ALLOC_SIZE, TEST_PAGES)
            });
            expect_true!(
                vmo.get_attributed_memory()
                    == make_private_attribution_counts(ALLOC_SIZE as u64, 0)
            );
        }

        // Faulting a partial range should not overrun
        {
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });
            expect_true!(vmo.get_attributed_memory() == make_private_attribution_counts(0, 0));

            // Write faulting the range should both map and allocate the requested pages, but no
            // more.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), WRITE_FLAGS, ALLOC_SIZE / 2)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(mapping.base(), ALLOC_SIZE, TEST_PAGES / 2)
            });
            expect_true!(
                vmo.get_attributed_memory()
                    == make_private_attribution_counts((ALLOC_SIZE / 2) as u64, 0)
            );
        }

        // Should not error if > VMO length.
        {
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            // Shrink the VMO so that it is smaller than the mapping.
            expect_ok!(vmo.resize((ALLOC_SIZE / 2) as u64));
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });

            // Attempt to fault the entire mapping range, which is now larger than the VMO.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), WRITE_FLAGS, ALLOC_SIZE)
            });
            // Only half should have been mapped and what is now the whole VMO should be committed.
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(mapping.base(), ALLOC_SIZE, TEST_PAGES / 2)
            });
            expect_true!(
                vmo.get_attributed_memory()
                    == make_private_attribution_counts((ALLOC_SIZE / 2) as u64, 0)
            );

            // Restore the VMO size.
            expect_ok!(vmo.resize(ALLOC_SIZE as u64));
        }

        // Will respect protection boundaries.
        {
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            // Remove write permissions from half the mapping.
            expect_ok!(mapping.protect(ARCH_MMU_FLAG_PERM_READ, ALLOC_SIZE / 2));

            // Attempt to write fault the entire mapping.
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(mapping.base(), WRITE_FLAGS, ALLOC_SIZE)
            });
            // Only the writable half should have been mapped and committed.
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(mapping.base(), ALLOC_SIZE, TEST_PAGES / 2)
            });
            expect_true!(
                vmo.get_attributed_memory()
                    == make_private_attribution_counts((ALLOC_SIZE / 2) as u64, 0)
            );

            // Reset protections.
            expect_ok!(mapping.protect(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE, 0));
        }

        // Will mark modified if even one writable page is mapped even if mapping aborts early due
        // to an error.
        {
            // Create a pager backed VMO with one page committed and map it.
            let (paged_vmo, [_page]) = unwrap_ok!(make_partially_committed_pager_vmo::<1>(
                TEST_PAGES, false, false, true,
            ));
            let paged_mapping = unwrap_some!(UserMemory::create_from_vmo(
                VmObjectPaged::into_vm_object(paged_vmo.clone()),
                0,
                0,
            ));

            // Consume any existing modified flag.
            assert_ok!(paged_vmo.query_pager_vmo_stats(true).map(|_| ()));
            let stats = unwrap_ok!(paged_vmo.query_pager_vmo_stats(true));
            expect_eq!(stats.modified, 0u32);

            // Perform a fault that will have to generate a page request. To avoid blocking on the
            // page request we must directly call the page_fault_locked method instead of the
            // VmAspace fault.
            {
                let base = paged_mapping.base();
                let mut result: (Result<(), Status>, u32);
                let mut retry_count: usize = 0;
                loop {
                    ksync::lock!(let mut guard = paged_mapping.mapping().lock().lock());
                    stack_pin_init!(let page_request = MultiPageRequest::new());
                    // Although the first page is supplied to paged_vmo, attempting to map it could
                    // still fail due to either it being deduped to a marker, or it being a loaned
                    // page and needing to be swapped. Both of these cases require an allocation,
                    // which could need to wait. This wait request should only be due to the pmm
                    // random delayed allocations, and so we can just ignore it and try again.
                    // SAFETY: `paged_mapping.mapping()` is alive in a user aspace, the object lock
                    // is not held, `base` is page-aligned and within the mapping, `TEST_PAGES - 1`
                    // is less than `usize::MAX`, and `page_request` has no active request.
                    result = unsafe {
                        paged_mapping.mapping().page_fault_locked(
                            guard.as_mut().token_mut(),
                            VAddr(base),
                            WRITE_FLAGS,
                            TEST_PAGES - 1,
                            page_request.as_ref(),
                        )
                    };
                    page_request.as_ref().cancel_requests();
                    retry_count += 1;
                    if !(result.0 == Err(Status::SHOULD_WAIT) && result.1 == 0 && retry_count < 100)
                    {
                        break;
                    }
                }
                expect_err!(result.0, Status::SHOULD_WAIT);
                expect_eq!(result.1, 1u32);
            }

            // The one previously committed page should have been mapped in and the VMO marked
            // modified.
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe { verify_mapped_page_range(paged_mapping.base(), ALLOC_SIZE, 1) });

            let stats = unwrap_ok!(paged_vmo.query_pager_vmo_stats(true));
            expect_eq!(stats.modified, ZX_PAGER_VMO_STATS_MODIFIED);
        }

        // Read fault on copy-on-write hierarchy with some leaf pages will map both parent and
        // child pages without committing extra pages into the child.
        {
            expect_ok!(vmo.commit_range(0, ALLOC_SIZE as u64));
            // Create a snapshot with some committed pages and map it in.
            let child_vmo = unwrap_ok!(vmo.create_clone(
                Resizability::NonResizable,
                SnapshotType::Full,
                0,
                ALLOC_SIZE as u64,
                true,
            ));
            expect_ok!(child_vmo.commit_range(0, PAGE_SIZE));
            expect_ok!(child_vmo.commit_range((ALLOC_SIZE / 2) as u64, PAGE_SIZE));
            let child_mapping = unwrap_some!(UserMemory::create_from_vmo(child_vmo.clone(), 0, 0));

            // Read fault the entire range. Everything should get mapped with the child's memory
            // attribution being unchanged.
            let original_counts = child_vmo.get_attributed_memory();
            // SAFETY: Called from a valid kernel thread with an attached user address space and no
            // locks held.
            expect_ok!(unsafe {
                thread::soft_fault_in_range(child_mapping.base(), READ_FLAGS, ALLOC_SIZE)
            });
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            expect_true!(unsafe {
                verify_mapped_page_range(child_mapping.base(), ALLOC_SIZE, TEST_PAGES)
            });
            expect_true!(original_counts == child_vmo.get_attributed_memory());
            expect_true!(verify_continuous_attribution_bytes(
                &child_vmo,
                (original_counts.uncompressed_bytes + original_counts.compressed_bytes) as u64,
            ));
        }

        // Calling read_user will fault the requested range.
        {
            expect_ok!(vmo.decommit_range(0, ALLOC_SIZE as u64));
            expect_ok!(vmo.commit_range(0, ALLOC_SIZE as u64));

            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            assert_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 0) });

            let (status, read_actual) = vmo.read_user(
                mapping.user_out::<u8>(),
                0,
                PAGE_SIZE_USIZE * 2,
                VmObjectReadWriteOptions::NONE,
            );
            assert_ok!(status);
            assert_eq!(read_actual, PAGE_SIZE_USIZE * 2);

            // The page fault optimisation should not have been triggered so the exact range is
            // mapped.
            // SAFETY: Unit tests run on an initialized kernel thread with a valid active
            // `VmAspace`.
            assert_true!(unsafe { verify_mapped_page_range(mapping.base(), ALLOC_SIZE, 2) });
        }
    }

    /// Tests mapping noncontiguous physical pages with `ExistingEntryAction::Error`.
    #[test]
    fn arch_noncontiguous_map() {
        // Get some phys pages to test on
        let mut phys = [PAddr(0); 3];
        stack_pin_init!(let phys_list = VmPageDoublyLinkedList::new());
        let mut status = pmm::alloc_pages(phys.len(), 0, phys_list.as_mut());
        assert_ok!(status, "non contig map alloc");
        {
            for (i, p) in phys_list.iter().enumerate() {
                phys[i] = p.paddr();
            }
        }
        let free_phys = zr::defer(|| {
            // SAFETY: All pages in `phys_list` were allocated above and are unmapped before drop.
            unsafe { pmm::free_list(phys_list) };
        });

        {
            const BASE: usize = USER_ASPACE_BASE + 10 * PAGE_SIZE_USIZE;

            // SAFETY: `[USER_ASPACE_BASE, USER_ASPACE_BASE + USER_ASPACE_SIZE)` is a valid user
            // address space range, and the deferred guards ensure mappings are unmapped and
            // `destroy` is called before `aspace` is dropped.
            stack_pin_init!(
                let aspace = unsafe { ArchVmAspace::new(USER_ASPACE_BASE, USER_ASPACE_SIZE, 0) }
            );
            // SAFETY: `aspace` was just constructed and is not concurrently accessed.
            status = unsafe { aspace.init() };
            let mut cleanup_destroy = zr::defer(|| {
                // SAFETY: `aspace` is inactive and has no remaining mappings.
                unsafe { aspace.destroy() }.expect("destroy aspace");
            });
            assert_ok!(status, "failed to init aspace");
            let mut cleanup_unmap = zr::defer(|| {
                // SAFETY: `aspace` is initialized, non-unified, and inactive, so unmapping its
                // entire range on an early failure path is sound.
                unsafe {
                    aspace.unmap(
                        USER_ASPACE_BASE,
                        USER_ASPACE_SIZE / PAGE_SIZE_USIZE,
                        ARCH_UNMAP_OPTION_ENLARGE,
                    )
                }
                .expect("unmap aspace");
            });

            // Attempt to map a set of vm_page_t
            // SAFETY: `aspace` is initialized and non-unified, and `phys` holds `phys.len()` valid
            // physical page addresses mapped into a valid user virtual range.
            status = unsafe {
                aspace.map(
                    BASE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Error,
                )
            };
            assert_ok!(status, "failed first map");

            // Expect that the map succeeded
            for (i, &expected_phys) in phys.iter().enumerate() {
                let query_res = aspace.query(BASE + i * PAGE_SIZE_USIZE);
                expect_ok!(query_res.map(|_| ()), "bad first map");
                if let Ok((paddr, mmu_flags)) = query_res {
                    expect_eq!(expected_phys.0, paddr.0, "bad first map");
                    expect_eq!(ARCH_MMU_FLAG_PERM_READ, mmu_flags, "bad first map");
                }
            }

            // Attempt to map again, should fail
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Error,
                )
            };
            expect_err!(status, Status::ALREADY_EXISTS, "double map");

            // Attempt to map partially overlapping, should fail
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE + 2 * PAGE_SIZE_USIZE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Error,
                )
            };
            expect_err!(status, Status::ALREADY_EXISTS, "double map");
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE - 2 * PAGE_SIZE_USIZE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Error,
                )
            };
            expect_err!(status, Status::ALREADY_EXISTS, "double map");

            // No entries should have been created by the partial failures
            expect_err!(
                aspace.query(BASE - 2 * PAGE_SIZE_USIZE),
                Status::NOT_FOUND,
                "bad first map"
            );
            expect_err!(aspace.query(BASE - PAGE_SIZE_USIZE), Status::NOT_FOUND, "bad first map");
            expect_err!(
                aspace.query(BASE + 3 * PAGE_SIZE_USIZE),
                Status::NOT_FOUND,
                "bad first map"
            );
            expect_err!(
                aspace.query(BASE + 4 * PAGE_SIZE_USIZE),
                Status::NOT_FOUND,
                "bad first map"
            );

            // Unmap all remaining entries
            // The partial failures did not create any new entries, so only entries
            // created by the first map should be unmapped.
            // SAFETY: `aspace` is initialized, non-unified, and inactive, and `[BASE, BASE + 3 *
            // PAGE_SIZE_USIZE)` lies within its bounds.
            status = unsafe { aspace.unmap(BASE, phys.len(), ARCH_UNMAP_OPTION_ENLARGE) };
            assert_ok!(status, "failed unmap");
            cleanup_unmap.cancel();

            cleanup_destroy.cancel();
            // SAFETY: `aspace` is inactive and all mappings have been unmapped.
            status = unsafe { aspace.destroy() };
            expect_ok!(status, "failed to destroy aspace");
        }

        drop(free_phys);
    }

    /// Tests mapping noncontiguous physical pages with `ExistingEntryAction::Upgrade`.
    #[test]
    fn arch_noncontiguous_map_with_upgrade() {
        // Get some phys pages to test on
        let mut phys = [PAddr(0); 3];
        stack_pin_init!(let phys_list = VmPageDoublyLinkedList::new());
        let mut status = pmm::alloc_pages(phys.len(), 0, phys_list.as_mut());
        assert_ok!(status, "non contig map alloc");
        {
            for (i, p) in phys_list.iter().enumerate() {
                phys[i] = p.paddr();
            }
        }
        let free_phys = zr::defer(|| {
            // SAFETY: All pages in `phys_list` were allocated above and are unmapped before drop.
            unsafe { pmm::free_list(phys_list) };
        });

        {
            const BASE: usize = USER_ASPACE_BASE + 10 * PAGE_SIZE_USIZE;
            const WINDOW_BASE: usize = BASE - 2 * PAGE_SIZE_USIZE;

            // SAFETY: `[USER_ASPACE_BASE, USER_ASPACE_BASE + USER_ASPACE_SIZE)` is a valid user
            // address space range, and the deferred guards ensure mappings are unmapped and
            // `destroy` is called before `aspace` is dropped.
            stack_pin_init!(
                let aspace = unsafe { ArchVmAspace::new(USER_ASPACE_BASE, USER_ASPACE_SIZE, 0) }
            );
            // SAFETY: `aspace` was just constructed and is not concurrently accessed.
            status = unsafe { aspace.init() };
            let mut cleanup_destroy = zr::defer(|| {
                // SAFETY: `aspace` is inactive and has no remaining mappings.
                unsafe { aspace.destroy() }.expect("destroy aspace");
            });
            assert_ok!(status, "failed to init aspace");
            let mut cleanup_unmap = zr::defer(|| {
                // SAFETY: `aspace` is initialized, non-unified, and inactive, so unmapping its
                // entire range on an early failure path is sound.
                unsafe {
                    aspace.unmap(
                        USER_ASPACE_BASE,
                        USER_ASPACE_SIZE / PAGE_SIZE_USIZE,
                        ARCH_UNMAP_OPTION_ENLARGE,
                    )
                }
                .expect("unmap aspace");
            });

            // Attempt to map a set of vm_page_t
            // SAFETY: `aspace` is initialized and non-unified, and `phys` holds `phys.len()` valid
            // physical page addresses mapped into a valid user virtual range.
            status = unsafe {
                aspace.map(
                    BASE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Error,
                )
            };
            assert_ok!(status, "failed first map");

            // Attempt to map with upgrades allowed, should succeed
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ExistingEntryAction::Upgrade,
                )
            };
            expect_ok!(status, "map upgrade failed");

            // Attempt to map with upgrades allowed, should succeed but not remap anything
            // b/c downgrade to read not allowed
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Upgrade,
                )
            };
            expect_ok!(status, "map upgrade failed");

            // Expect that the upgrade maps succeeded
            for i in 0..phys.len() {
                const MAP_WINDOW_PADDR_INDEX: [usize; 3] = [0, 1, 2];
                const MAP_WINDOW_MMU_FLAGS: [ArchMmuFlags; 3] = [
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                ];

                let query_res = aspace.query(BASE + i * PAGE_SIZE_USIZE);
                expect_ok!(query_res.map(|_| ()), "bad map upgrade");
                if let Ok((paddr, mmu_flags)) = query_res {
                    expect_eq!(phys[MAP_WINDOW_PADDR_INDEX[i]].0, paddr.0, "bad map upgrade");
                    expect_eq!(MAP_WINDOW_MMU_FLAGS[i], mmu_flags, "bad map upgrade");
                }
            }

            // Attempt to map partially overlapping with upgrades allowed, should succeed
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE + 2 * PAGE_SIZE_USIZE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ExistingEntryAction::Upgrade,
                )
            };
            expect_ok!(status, "map upgrade failed");
            // SAFETY: `aspace` is an initialized, inactive, non-unified user `ArchVmAspace`, and
            // `phys` points to `phys.len()` initialized `PAddr`s without concurrent mutation.
            status = unsafe {
                aspace.map(
                    BASE - 2 * PAGE_SIZE_USIZE,
                    phys.as_mut_ptr(),
                    phys.len(),
                    ARCH_MMU_FLAG_PERM_READ,
                    ExistingEntryAction::Upgrade,
                )
            };
            expect_ok!(status, "map upgrade failed");

            // Expect that the `Upgrade` maps succeeded
            // We check the entire [BASE - 2, BASE + 4] "window" covered by the partial maps
            const MAP_WINDOW_SIZE: usize = 7;
            for i in 0..MAP_WINDOW_SIZE {
                const MAP_WINDOW_PADDR_INDEX: [usize; MAP_WINDOW_SIZE] = [0, 1, 2, 1, 0, 1, 2];
                const MAP_WINDOW_MMU_FLAGS: [ArchMmuFlags; MAP_WINDOW_SIZE] = [
                    ARCH_MMU_FLAG_PERM_READ,
                    ARCH_MMU_FLAG_PERM_READ,
                    ARCH_MMU_FLAG_PERM_READ,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                ];

                let query_res = aspace.query(WINDOW_BASE + i * PAGE_SIZE_USIZE);
                expect_ok!(query_res.map(|_| ()), "bad map upgrade");
                if let Ok((paddr, mmu_flags)) = query_res {
                    expect_eq!(phys[MAP_WINDOW_PADDR_INDEX[i]].0, paddr.0, "bad map upgrade");
                    expect_eq!(MAP_WINDOW_MMU_FLAGS[i], mmu_flags, "bad map upgrade");
                }
            }

            // Unmap any remaining entries
            // SAFETY: `aspace` is initialized, non-unified, and inactive, and `[WINDOW_BASE,
            // WINDOW_BASE + MAP_WINDOW_SIZE * PAGE_SIZE_USIZE)` lies within its bounds.
            status =
                unsafe { aspace.unmap(WINDOW_BASE, MAP_WINDOW_SIZE, ARCH_UNMAP_OPTION_ENLARGE) };
            assert_ok!(status, "failed unmap");
            cleanup_unmap.cancel();

            cleanup_destroy.cancel();
            // SAFETY: `aspace` is inactive and all mappings have been unmapped.
            status = unsafe { aspace.destroy() };
            expect_ok!(status, "failed to destroy aspace");
        }

        drop(free_phys);
    }

    /// Get the `mmu_flags` of the given `vaddr` of the given `aspace`.
    ///
    /// Return 0 if the page is unmapped or on error.
    fn get_vaddr_flags(aspace: &ArchVmAspace, vaddr: usize) -> ArchMmuFlags {
        let Ok((_unused_paddr, mmu_flags)) = aspace.query(vaddr) else {
            return 0;
        };
        mmu_flags
    }

    /// Determine if the given page is mapped in.
    fn is_vaddr_mapped(aspace: &ArchVmAspace, vaddr: usize) -> bool {
        get_vaddr_flags(aspace, vaddr) != 0
    }

    /// Tests splitting large pages in `ArchVmAspace::protect`.
    #[test]
    fn arch_vm_aspace_protect_split_pages() {
        const READ_ONLY: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ;
        const READ_WRITE: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

        // Create a basic address space, starting from vaddr 0.
        // SAFETY: `[0, USER_ASPACE_SIZE)` is a valid user address space configuration, and
        // `_destroy` and `_cleanup` unmap all mappings and destroy `aspace` before drop.
        stack_pin_init!(let aspace = unsafe { ArchVmAspace::new(0, USER_ASPACE_SIZE, 0) });
        let _destroy = zr::defer(|| {
            // SAFETY: `aspace` is inactive and has no remaining mappings when `_destroy` runs.
            unsafe { aspace.destroy() }.expect("destroy aspace");
        });
        // SAFETY: `aspace` was just constructed and is not concurrently accessed.
        assert_ok!(unsafe { aspace.init() });
        let _cleanup = zr::defer(|| {
            // SAFETY: `aspace` is initialized, non-unified, and inactive, so unmapping its full
            // range is sound.
            unsafe {
                aspace.unmap(0, USER_ASPACE_SIZE / PAGE_SIZE_USIZE, ARCH_UNMAP_OPTION_ENLARGE)
            }
            .expect("unmap aspace");
        });

        // Map in a large contiguous area, which should be mapped by two large pages.
        const ZX_MAX_PAGE_SIZE: usize = 1usize << ZX_MAX_PAGE_SHIFT;
        zr::static_assert!(ZX_MAX_PAGE_SIZE > PAGE_SIZE_USIZE);
        const REGION_SIZE: usize = 16 * GB; // 16 GiB.
        // SAFETY: `aspace` is an initialized, isolated non-unified user `ArchVmAspace` that is
        // never activated, so mapping `[0, REGION_SIZE)` to physical `[0, REGION_SIZE)` is sound.
        assert_ok!(unsafe {
            aspace.map_contiguous(
                /*vaddr=*/ 0,
                /*paddr=*/ PAddr(0),
                /*count=*/ REGION_SIZE / PAGE_SIZE_USIZE,
                READ_ONLY,
            )
        });

        // Attempt to protect a subrange in the middle of the region, which will require splitting
        // pages.
        const PROTECTED_RANGE: usize = REGION_SIZE / 2 - PAGE_SIZE_USIZE;
        const PROTECTED_PAGES: usize = 2;
        // SAFETY: `aspace` is an initialized, isolated, non-unified user `ArchVmAspace`,
        // `[PROTECTED_RANGE, PROTECTED_RANGE + PROTECTED_PAGES * PAGE_SIZE_USIZE)` is mapped, and
        // `ARCH_UNMAP_OPTION_HARVEST` is not set so synthetic physical addresses are permitted.
        assert_ok!(unsafe {
            aspace.protect(
                PROTECTED_RANGE,
                /*count=*/ PROTECTED_PAGES,
                READ_WRITE,
                ARCH_UNMAP_OPTION_ENLARGE,
            )
        });

        // Ensure the pages inside the range changed.
        expect_eq!(get_vaddr_flags(&aspace, PROTECTED_RANGE), READ_WRITE);
        expect_eq!(get_vaddr_flags(&aspace, PROTECTED_RANGE + PAGE_SIZE_USIZE), READ_WRITE);

        // Ensure the pages surrounding the range did not change.
        expect_eq!(get_vaddr_flags(&aspace, PROTECTED_RANGE - PAGE_SIZE_USIZE), READ_ONLY);
        expect_eq!(
            get_vaddr_flags(&aspace, PROTECTED_RANGE + PROTECTED_PAGES * PAGE_SIZE_USIZE),
            READ_ONLY
        );
    }

    /// Tests out-of-memory handling when splitting large pages in `ArchVmAspace::protect`.
    #[test]
    fn arch_vm_aspace_protect_split_pages_out_of_memory() {
        const READ_ONLY: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ;
        const READ_WRITE: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

        // Create a custom allocator that we can cause to stop returning allocations.
        //
        // ArchVmAspace doesn't allow us to send state to the allocator, so we use an `AtomicBool`
        // for `Sync` interior mutability across the stateless `extern "C"` callback without
        // `static mut`.
        static ALLOW_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
        /// # Safety
        ///
        /// The caller must ensure that `p` and `pa` are non-null, properly aligned, and valid for
        /// non-overlapping writes. On `Ok(())`, `*p` and `*pa` are initialized with a freshly
        /// allocated, exclusively owned PMM `VmPage` in `ALLOC` state and its physical address.
        unsafe extern "C" fn allocator(
            _alloc_flags: u32,
            p: *mut *mut VmPage,
            pa: *mut PAddr,
        ) -> Result<(), Status> {
            if !ALLOW_ALLOCATIONS.load(Ordering::SeqCst) {
                return Err(Status::NO_MEMORY);
            }
            let (page, paddr) = pmm::alloc_page(0)?;
            // SAFETY: By this function's `# Safety` contract, `p` and `pa` are non-null, aligned,
            // and valid for non-overlapping writes; ownership of `page` transfers to the caller.
            unsafe {
                p.write(page.as_raw());
                pa.write(paddr);
            }
            Ok(())
        }
        ALLOW_ALLOCATIONS.store(true, Ordering::SeqCst);

        // Create a basic address space, starting from vaddr 0.
        // SAFETY: `[0, USER_ASPACE_SIZE)` is a valid user address space configuration, `allocator`
        // satisfies `ArchVmAspace::new_with_page_alloc_fn`'s contract, and `_destroy` and
        // `_cleanup` unmap all mappings and destroy `aspace` before drop.
        stack_pin_init!(
            let aspace =
                unsafe { ArchVmAspace::new_with_page_alloc_fn(0, USER_ASPACE_SIZE, 0, allocator) }
        );
        let _destroy = zr::defer(|| {
            // SAFETY: `aspace` is inactive and has no remaining mappings when `_destroy` runs.
            unsafe { aspace.destroy() }.expect("destroy aspace");
        });
        // SAFETY: `aspace` was just constructed and is not concurrently accessed.
        assert_ok!(unsafe { aspace.init() });
        let _cleanup = zr::defer(|| {
            // SAFETY: `aspace` is initialized, non-unified, and inactive, so unmapping its full
            // range is sound.
            unsafe {
                aspace.unmap(0, USER_ASPACE_SIZE / PAGE_SIZE_USIZE, ARCH_UNMAP_OPTION_ENLARGE)
            }
            .expect("unmap aspace");
        });

        // Map in a large contiguous area, large enough to use large pages to fill.
        const REGION_SIZE: usize = 16 * GB; // 16 GiB.
        // SAFETY: `aspace` is an initialized, isolated non-unified user `ArchVmAspace` that is
        // never activated, so mapping `[0, REGION_SIZE)` to physical `[0, REGION_SIZE)` is sound.
        assert_ok!(unsafe {
            aspace.map_contiguous(
                /*vaddr=*/ 0,
                /*paddr=*/ PAddr(0),
                /*count=*/ REGION_SIZE / PAGE_SIZE_USIZE,
                READ_ONLY,
            )
        });

        // Prevent further allocations.
        ALLOW_ALLOCATIONS.store(false, Ordering::SeqCst);

        // Attempt to protect a subrange in the middle of the region, which will require splitting
        // pages. Expect this to fail.
        const PROTECTED_RANGE: usize = REGION_SIZE / 2 - PAGE_SIZE_USIZE;
        const PROTECTED_SIZE: usize = 2 * PAGE_SIZE_USIZE;
        // SAFETY: `aspace` is an initialized, isolated, non-unified user `ArchVmAspace`,
        // `[PROTECTED_RANGE, PROTECTED_RANGE + PROTECTED_SIZE)` is mapped, and
        // `ARCH_UNMAP_OPTION_HARVEST` is not set so synthetic physical addresses are permitted.
        let status = unsafe {
            aspace.protect(
                PROTECTED_RANGE,
                /*count=*/ 2,
                READ_WRITE,
                ARCH_UNMAP_OPTION_ENLARGE,
            )
        };
        expect_err!(status, Status::NO_MEMORY);

        // The pages surrounding our protect range should still be mapped.
        expect_eq!(get_vaddr_flags(&aspace, PROTECTED_RANGE - PAGE_SIZE_USIZE), READ_ONLY);
        expect_eq!(get_vaddr_flags(&aspace, PROTECTED_RANGE + PROTECTED_SIZE), READ_ONLY);

        // The pages we tried to protect should still be mapped, albeit permissions might
        // be changed.
        expect_true!(is_vaddr_mapped(&aspace, PROTECTED_RANGE));
        expect_true!(is_vaddr_mapped(&aspace, PROTECTED_RANGE + PAGE_SIZE_USIZE));
    }

    /// Tests that all VM kernel regions are mapped with expected MMU flags and gaps are in a VMAR.
    #[test]
    fn vm_kernel_region_test() {
        let executable_start = (&raw const __executable_start).addr();
        let end = (&raw const _end).addr();
        let kernel_regions = kernel_regions();

        let root_vmar = unwrap_some!(VmAspace::kernel_aspace().root_vmar());
        let kernel_vmar = unwrap_some!(root_vmar.find_region(VAddr(executable_start)));
        expect_false!(kernel_vmar.is_mapping());
        let kernel_vmar_region = unwrap_some!(kernel_vmar.into_vm_address_region());
        let mut base = executable_start;
        while base < end {
            let mut within_region = false;
            for kernel_region in &kernel_regions {
                // This would not overflow because the region base and size are hard-coded.
                if kernel_region.size != 0
                    && base >= kernel_region.base
                    && base + PAGE_SIZE_USIZE <= kernel_region.base + kernel_region.size
                {
                    // If this page exists within a kernel region, then it should be within a
                    // VmMapping with the correct arch MMU flags.
                    within_region = true;
                    // Every page from __code_start to _end should either be a VmMapping or a VMAR.
                    let region = unwrap_some!(kernel_vmar_region.find_region(VAddr(base)));
                    expect_true!(region.is_mapping());
                    let mapping = unwrap_some!(region.into_vm_mapping());
                    ksync::lock!(let guard = mapping.lock().lock());
                    expect_eq!(
                        kernel_region.arch_mmu_flags,
                        mapping.arch_mmu_flags_locked(guard.token(), VAddr(base)),
                        kernel_region.name
                    );
                    break;
                }
            }
            if !within_region {
                let region =
                    root_vmar.find_region(VAddr(base)).and_then(|r| r.into_vm_address_region());
                expect_eq!(
                    region.as_ref().map_or(core::ptr::null(), RefPtr::as_ptr),
                    RefPtr::as_ptr(&kernel_vmar_region)
                );
            }
            base += PAGE_SIZE_USIZE;
        }
    }

    /// Tests memory priority propagation through hierarchies.
    #[test]
    fn vmaspace_priority_propagation_test() {
        // Test that memory priority gets propagated through hierarchies and into newly created
        // objects.
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        let _mapping_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ));

        // Set the priority in our vmar and validate it propagates to the VMO and the aspace.
        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a new VMAR and VMO and map them into the high priority vmar. Memory priority
        // should propagate.
        let sub_vmar = unwrap_ok!(vmar.create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 16,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test sub-vmar",
        ));

        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        let _mapping2_result = unwrap_ok!(sub_vmar.create_vm_mapping_no_overwrite(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo2.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ));
        expect_true!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Change the priority of the sub vmar. It should not effect the original vmar / vmo
        // priority.
        let status = sub_vmar.set_memory_priority(MemoryPriority::Default);
        expect_ok!(status);
        expect_false!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(vmar.destroy());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_ok!(aspace.destroy());
    }

    /// Test that overwriting a mapping maintains priority counts.
    #[test]
    fn vmaspace_priority_mapping_overwrite_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        let mapping_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
            0,
            PAGE_SIZE_USIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ));
        let mapping = mapping_result.mapping;

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Overwrite the mapping with a new one from a new VMO.
        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let _mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                mapping.base().0 - vmar.base().0,
                mapping.size(),
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo2.clone()),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        // Original VMO should have lost its priority, and the VMO for our new mapping should have
        // gained.
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(vmo2.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Test that unmapping parts of a mapping preserves priority.
    #[test]
    fn vmaspace_priority_unmap_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 8));

        let mapping_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
            0,
            PAGE_SIZE_USIZE * 8,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_RW_USER_FLAGS,
        ));

        // Set the priority in our vmar and validate it propagates to the VMO and the aspace.
        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        let base = mapping_result.base;

        // Unmap one page from either end of the mapping, ensuring memory priority did not change.
        expect_ok!(unsafe {
            vmar.unmap(VAddr(base), PAGE_SIZE_USIZE, VmAddressRegionOpChildren::No)
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 7),
                PAGE_SIZE_USIZE,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Unmap a page from the middle. This will split this into two mappings.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 4),
                PAGE_SIZE_USIZE,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());
        // Now completely unmap one portion. This will destroy one of the mappings, but the VMO
        // should still have priority from the other mapping that was previously split.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE),
                PAGE_SIZE_USIZE * 3,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Unmapping the rest of the other portion should finally cause the priority to be removed.
        expect_ok!(unsafe {
            vmar.unmap(
                VAddr(base + PAGE_SIZE_USIZE * 5),
                PAGE_SIZE_USIZE * 2,
                VmAddressRegionOpChildren::No,
            )
        });
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority with merged mappings.
    #[test]
    fn vmaspace_priority_merged_mapping_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        // Create a mapping for the first page of the VMO, and mark it mergeable
        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        VmMapping::mark_mergeable(mapping_result.mapping);

        // Map in the second page.
        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE * 2,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                PAGE_SIZE,
                ARCH_RW_USER_FLAGS,
            )
        });

        VmMapping::mark_mergeable(mapping_result.mapping);

        // Query the vmar, should have a single mapping of the combined size.
        let region = unwrap_some!(vmar.find_region(VAddr(vmar.base().0 + PAGE_SIZE_USIZE)));
        let map = unwrap_some!(region.into_vm_mapping());
        expect_eq!(PAGE_SIZE_USIZE * 2, map.size());

        // Now destroy the mapping and check the VMO loses priority.
        expect_ok!(map.destroy());

        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        expect_ok!(aspace.destroy());
    }

    /// Tests memory priority propagation with bidirectional clones.
    #[test]
    fn vmaspace_priority_bidir_clone_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test-aspace"));

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a clone of the VMO.
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");

        // Child should not have priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Destroying the clone should leave memory priority unchanged of the original.
        drop(vmo_child);
        drop(childp);
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Remove the mapping.
        expect_ok!(mapping_result.mapping.destroy());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Create a new clone of the VMO and map in the clone.
        let vmo_child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            true,
        ));
        let childp = VmObject::downcast_paged(vmo_child.clone()).expect("is paged");
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let _mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                vmo_child,
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Now destroy the parent VMO and ensure child retains priority.
        drop(vmo);
        expect_true!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
        expect_false!(childp.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
    }

    /// Tests memory priority propagation to a child VMO slice.
    #[test]
    fn vmaspace_priority_slice_test() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let status = vmar.set_memory_priority(MemoryPriority::High);
        expect_ok!(status);

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 2));

        // SAFETY: The mapped range is in a test user aspace, so nothing relies on it staying
        // mapped and no Rust reference into it is live.
        let _mapping_result = unwrap_ok!(unsafe {
            vmar.create_vm_mapping(
                PAGE_SIZE_USIZE,
                PAGE_SIZE_USIZE,
                0,
                vmar::flag::SPECIFIC_OVERWRITE,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                ARCH_RW_USER_FLAGS,
            )
        });

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Create a slice of the VMO.
        let vmo_slice = unwrap_ok!(vmo.create_child_slice(0, PAGE_SIZE, true));
        let slicep = vmo_slice.as_paged().unwrap();

        // Slice inherits priority.
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(slicep.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        // Change priority of the VMAR should remove from the VMO.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::Default));
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_false!(aspace.is_high_memory_priority());

        // Re-enable priority and verify.
        expect_ok!(vmar.set_memory_priority(MemoryPriority::High));
        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(slicep.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
        expect_true!(aspace.is_high_memory_priority());

        // Destroy slice and unmap.
        drop(vmo_slice);

        expect_true!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());

        expect_ok!(aspace.destroy());
        expect_false!(vmo.debug_get_cow_pages().unwrap().debug_is_high_memory_priority());
    }

    /// Attempt force a high priority region to be writeable.
    #[test]
    fn vm_mapping_force_writeable_high_priority() {
        let aspace = VmAspace::create(Type::User, b"test-aspace");
        assert_true!(aspace.is_some());
        let aspace = aspace.unwrap();

        // Create VMAR and a VMO and map it in.
        let vmar = unwrap_ok!(aspace.root_vmar().unwrap().create_sub_vmar(
            0,
            PAGE_SIZE_USIZE * 64,
            0,
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE,
            b"test vmar",
        ));

        let vmar = DropGuard::new(vmar, |vmar| {
            let _ = vmar.destroy();
        });

        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE * 4));

        // Create a read-only user mapping. Since there is no ARCH_MMU_FLAG_PERM_WRITE,
        // force_writable won't trivially succeed.
        let arch_read_user_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_USER;
        let mapping_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
            0,
            PAGE_SIZE_USIZE * 4,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo),
            0,
            arch_read_user_flags,
        ));

        let status = vmar.set_memory_priority(vmar::MemoryPriority::High);
        expect_ok!(status);

        let force_result = VmMapping::force_writable(mapping_result.mapping);
        expect_ok!(force_result.map(|_| ()));
    }

    #[derive(Clone, Copy)]
    struct ChildRegion {
        mapping: bool,
        page_offset_begin: usize,
        page_offset_end: usize,
    }

    // Helper class for writing tests against the pausable VmAddressRegionEnumerator
    trait EnumeratorItem {
        fn region_or_mapping(&self) -> &vmar::RegionOrMappingCommon;
    }

    impl EnumeratorItem for vmar::VmAddressRegionOrMapping<'_> {
        fn region_or_mapping(&self) -> &vmar::RegionOrMappingCommon {
            self
        }
    }

    impl EnumeratorItem for &VmMapping {
        fn region_or_mapping(&self) -> &vmar::RegionOrMappingCommon {
            self
        }
    }

    struct EnumeratorTestHelper<T: VmAddressRegionEnumeratorType> {
        vmo: Option<RefPtr<VmObjectPaged>>,
        test_vmar: Option<RefPtr<VmAddressRegion>>,
        _type: PhantomData<T>,
    }

    impl<T: VmAddressRegionEnumeratorType> EnumeratorTestHelper<T> {
        fn new() -> Self {
            Self { vmo: None, test_vmar: None, _type: PhantomData }
        }

        fn init(&mut self, aspace: &RefPtr<VmAspace>) -> Result<(), Status> {
            self.destroy();
            let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, GB as u64)?;
            let test_vmar = aspace.root_vmar().unwrap().create_sub_vmar(
                0,
                GB,
                0,
                vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ,
                b"test vmar",
            )?;
            self.vmo = Some(vmo);
            self.test_vmar = Some(test_vmar);
            Ok(())
        }

        fn add_regions(&self, regions: &[ChildRegion]) -> Result<(), Status> {
            let test_vmar = self.test_vmar.as_ref().unwrap();
            let vmo = self.vmo.as_ref().unwrap();
            for region in regions {
                assert!(region.page_offset_end > region.page_offset_begin);
                let offset = region.page_offset_begin * PAGE_SIZE_USIZE;
                let vaddr = VAddr(test_vmar.base().0 + offset);
                // See if there's a child VMAR that we should be making this in instead of our test
                // root.
                let mut vmar = test_vmar.clone();
                while let Some(next) = vmar
                    .find_region(vaddr)
                    .and_then(OwnedVmAddressRegionOrMapping::into_vm_address_region)
                {
                    vmar = next;
                }
                // Create either a mapping or vmar as requested.
                let size = (region.page_offset_end - region.page_offset_begin) * PAGE_SIZE_USIZE;
                let relative_offset = vaddr.0 - vmar.base().0;
                if region.mapping {
                    vmar.create_vm_mapping_no_overwrite(
                        relative_offset,
                        size,
                        0,
                        vmar::flag::CAN_MAP_READ | vmar::flag::SPECIFIC,
                        VmObjectPaged::into_vm_object(vmo.clone()),
                        0,
                        ARCH_MMU_FLAG_PERM_READ,
                    )?;
                } else {
                    vmar.create_sub_vmar(
                        relative_offset,
                        size,
                        0,
                        vmar::flag::CAN_MAP_READ
                            | vmar::flag::SPECIFIC
                            | vmar::flag::CAN_MAP_SPECIFIC,
                        b"vmar",
                    )?;
                }
            }
            Ok(())
        }

        fn enumerator<'a>(
            &'a self,
            page_offset_begin: usize,
            page_offset_end: usize,
            token: &LockToken<'_, VmAspaceLockClass>,
        ) -> VmAddressRegionEnumerator<'a, T> {
            let test_vmar = self.test_vmar.as_ref().unwrap();
            let min_addr = test_vmar.base().0 + page_offset_begin * PAGE_SIZE_USIZE;
            let max_addr = test_vmar.base().0 + page_offset_end * PAGE_SIZE_USIZE;
            // SAFETY: The tests hold the aspace lock, and modify no region list, except while the
            // enumerator is paused.
            unsafe { VmAddressRegionEnumerator::new(test_vmar, min_addr, max_addr, token) }
        }

        fn resume(
            &self,
            enumerator: &mut VmAddressRegionEnumerator<'_, T>,
            token: &LockToken<'_, VmAspaceLockClass>,
        ) {
            // SAFETY: The tests hold the aspace lock, and modify no region list, until the
            // enumerator is paused again or dropped.
            unsafe { enumerator.resume(token) };
        }

        fn expect_regions(
            &self,
            enumerator: &mut VmAddressRegionEnumerator<'_, T>,
            token: &LockToken<'_, VmAspaceLockClass>,
            regions: &[ChildRegion],
        ) -> bool
        where
            for<'b> T::Item<'b>: EnumeratorItem,
        {
            let test_vmar = self.test_vmar.as_ref().unwrap();
            for region in regions {
                assert!(region.page_offset_end > region.page_offset_begin);
                let Some((item, _depth)) = enumerator.next(token) else {
                    return false;
                };
                let region_or_mapping = item.region_or_mapping();
                if region.mapping != region_or_mapping.is_mapping() {
                    return false;
                }
                if region_or_mapping.base().0
                    != test_vmar.base().0 + region.page_offset_begin * PAGE_SIZE_USIZE
                {
                    return false;
                }
                if region_or_mapping.size()
                    != (region.page_offset_end - region.page_offset_begin) * PAGE_SIZE_USIZE
                {
                    return false;
                }
            }
            true
        }

        fn unmap(&self, page_offset_begin: usize, page_offset_end: usize) -> Result<(), Status> {
            assert!(page_offset_end > page_offset_begin);
            let test_vmar = self.test_vmar.as_ref().unwrap();
            let vaddr = VAddr(test_vmar.base().0 + page_offset_begin * PAGE_SIZE_USIZE);
            let size = (page_offset_end - page_offset_begin) * PAGE_SIZE_USIZE;
            // Attempt to unmap, walking down into child vmars if the unmap fails due to it causing
            // a subvmar to be partially unmapped.
            let mut vmar = Some(test_vmar.clone());
            while let Some(cur_vmar) = vmar {
                // SAFETY: Unmapping test regions within our test VMAR.
                let res = unsafe { cur_vmar.unmap(vaddr, size, VmAddressRegionOpChildren::Yes) };
                if res != Err(Status::INVALID_ARGS) {
                    return res;
                }
                let Some(next) = cur_vmar.find_region(vaddr) else {
                    return res;
                };
                vmar = next.into_vm_address_region();
            }
            Err(Status::NOT_FOUND)
        }

        fn lock(&self) -> &VmAspaceMutex {
            self.test_vmar.as_ref().unwrap().lock()
        }

        fn destroy(&mut self) {
            if let Some(test_vmar) = self.test_vmar.take() {
                let _ = test_vmar.destroy();
            }
            self.vmo = None;
        }
    }

    impl<T: VmAddressRegionEnumeratorType> Drop for EnumeratorTestHelper<T> {
        fn drop(&mut self) {
            self.destroy();
        }
    }

    /// Tests pausable VmAddressRegionEnumerator across VMAR and mapping mutations.
    #[test]
    fn address_region_enumerator_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));

        // Smoke test of a single region.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[ChildRegion {
                mapping: true,
                page_offset_begin: 0,
                page_offset_end: 1,
            }]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 1, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Unmap while iterating a subvmar and resume in the parent.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 7 },
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                ChildRegion { mapping: true, page_offset_begin: 3, page_offset_end: 4 },
                ChildRegion { mapping: true, page_offset_begin: 5, page_offset_end: 6 },
                ChildRegion { mapping: true, page_offset_begin: 7, page_offset_end: 8 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 10, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 7 },
                    ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                ],
            ));
            enumerator.pause(guard.token());
            // Unmap the entire subvmar we created
            guard.as_mut().call_unlocked(|| {
                let _ = test.unmap(0, 7);
            });
            test.resume(&mut enumerator, guard.token());
            // Last mapping should still be there.
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 7, page_offset_end: 8 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Pause immediately without enumerating when the start is a subvmar.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 2 },
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 2, guard.token());
            enumerator.pause(guard.token());
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 2 },
                    ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                ],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Add future mapping.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 },
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 3, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 }],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                let _ = test.add_regions(&[ChildRegion {
                    mapping: true,
                    page_offset_begin: 2,
                    page_offset_end: 3,
                }]);
            });
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                    ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 },
                ],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Replace the next mapping.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 },
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 3, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 }],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                let _ = test.unmap(1, 2);
                let _ = test.add_regions(&[ChildRegion {
                    mapping: true,
                    page_offset_begin: 1,
                    page_offset_end: 3,
                }]);
            });
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 3 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Add earlier regions.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 },
                ChildRegion { mapping: true, page_offset_begin: 3, page_offset_end: 4 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 4, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 }],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                let _ = test.add_regions(&[
                    ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 1 },
                    ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 32 },
                ]);
            });
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 3, page_offset_end: 4 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Replace current.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 3, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 }],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                let _ = test.unmap(1, 2);
                let _ = test.add_regions(&[ChildRegion {
                    mapping: true,
                    page_offset_begin: 0,
                    page_offset_end: 2,
                }]);
            });
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Replace current and next with a single mapping.
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 },
                ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 3 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 3, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 1, page_offset_end: 2 }],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                let _ = test.unmap(1, 3);
                let _ = test.add_regions(&[ChildRegion {
                    mapping: true,
                    page_offset_begin: 0,
                    page_offset_end: 3,
                }]);
            });
            test.resume(&mut enumerator, guard.token());
            expect_false!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 3 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Start enumerating part way into a mapping.
        {
            let mut test = EnumeratorTestHelper::<MappingsOnly>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 6 },
                ChildRegion { mapping: true, page_offset_begin: 0, page_offset_end: 2 },
                ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 4 },
                ChildRegion { mapping: true, page_offset_begin: 6, page_offset_end: 7 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(3, 7, guard.token());
            enumerator.pause(guard.token());
            test.resume(&mut enumerator, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: true, page_offset_begin: 2, page_offset_end: 4 },
                    ChildRegion { mapping: true, page_offset_begin: 6, page_offset_end: 7 },
                ],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Delete depth that was just yielded
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 10 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 9 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 8 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 7 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 10, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 10 },
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 9 },
                ],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                test.unmap(0, 9).unwrap();
                test.add_regions(&[
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 8 },
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 7 },
                ])
                .unwrap();
            });
            test.resume(&mut enumerator, guard.token());
            // Subtree was deleted and the new one will not be yielded.
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Delete next depth to be yielded
        {
            let mut test = EnumeratorTestHelper::<VmarsAndMappings>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 10 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 9 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 8 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 7 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            let mut enumerator = test.enumerator(0, 10, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 10 },
                    ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 9 },
                ],
            ));
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(|| {
                test.unmap(0, 8).unwrap();
                test.add_regions(&[ChildRegion {
                    mapping: false,
                    page_offset_begin: 0,
                    page_offset_end: 7,
                }])
                .unwrap();
            });
            test.resume(&mut enumerator, guard.token());
            // Subtree was deleted and the new one will not be yielded.
            expect_false!(enumerator.next(guard.token()).is_some());
        }
        // Nested mapping enumeration.
        {
            let mut test = EnumeratorTestHelper::<MappingsOnly>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 1000 },
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 100 },
                ChildRegion { mapping: false, page_offset_begin: 100, page_offset_end: 200 },
                ChildRegion { mapping: false, page_offset_begin: 200, page_offset_end: 300 },
                ChildRegion { mapping: true, page_offset_begin: 10, page_offset_end: 20 },
                ChildRegion { mapping: true, page_offset_begin: 110, page_offset_end: 120 },
                ChildRegion { mapping: true, page_offset_begin: 210, page_offset_end: 220 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            // Attempt to query for each mapping using its exact location.
            for probe in [10usize, 110, 210] {
                let mut enumerator = test.enumerator(probe, probe + 5, guard.token());
                // The enumerator will accurately yield only this mapping.
                // Furthermore, because mappings and VMARs are sorted, the enumerator naturally
                // short-circuits as soon as `itr_->base() >= max_addr_` without iterating the rest
                // of the tree.
                expect_true!(test.expect_regions(
                    &mut enumerator,
                    guard.token(),
                    &[ChildRegion {
                        mapping: true,
                        page_offset_begin: probe,
                        page_offset_end: probe + 10,
                    }],
                ));
                expect_false!(enumerator.next(guard.token()).is_some());
            }
        }
        // Sub-VMAR entirely below min_addr_ is skipped.
        {
            let mut test = EnumeratorTestHelper::<MappingsOnly>::new();
            assert_ok!(test.init(&aspace));
            expect_ok!(test.add_regions(&[
                ChildRegion { mapping: false, page_offset_begin: 0, page_offset_end: 100 },
                ChildRegion { mapping: true, page_offset_begin: 10, page_offset_end: 20 },
                ChildRegion { mapping: false, page_offset_begin: 100, page_offset_end: 200 },
                ChildRegion { mapping: true, page_offset_begin: 110, page_offset_end: 120 },
            ]));
            stack_pin_init!(let guard = test.lock().lock());
            // Querying starting at 105. This should skip descending into the first sub-VMAR
            // (0..100) because all its children are < min_addr_, and correctly fall through to
            // traverse the next sibling VMAR.
            let mut enumerator = test.enumerator(105, 120, guard.token());
            expect_true!(test.expect_regions(
                &mut enumerator,
                guard.token(),
                &[ChildRegion { mapping: true, page_offset_begin: 110, page_offset_end: 120 }],
            ));
            expect_false!(enumerator.next(guard.token()).is_some());
        }

        expect_ok!(aspace.destroy());
    }

    /// Tests `region_list::get_alloc_spot` deterministic gap selection when `prng` is `None`.
    #[test]
    fn region_list_get_alloc_spot_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let size = 16 * PAGE_SIZE_USIZE;
        let vmar_flags =
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE;
        let test_vmar = unwrap_ok!(root_vmar.create_sub_vmar(0, size, 0, vmar_flags, b"test vmar"));
        let base = test_vmar.base();
        let align_pow2 = page::SHIFT as u8;

        let get_spot = |alloc_size: usize| {
            stack_pin_init!(let region_guard = test_vmar.region_lock().lock());
            vmar::region_list::get_alloc_spot(
                &test_vmar,
                region_guard.token(),
                align_pow2,
                /*entropy=*/ 0,
                alloc_size,
                base,
                size,
                /*prng=*/ None,
                VAddr(usize::MAX),
            )
        };
        let insert_sub_vmar = |spot: VAddr, alloc_size: usize| {
            test_vmar.create_sub_vmar(
                spot.0 - base.0,
                alloc_size,
                0,
                vmar::flag::SPECIFIC | vmar_flags,
                b"sub vmar",
            )
        };

        // Allocate 1 page, should be allocated at [+0, +PAGE_SIZE].
        let alloc_size = PAGE_SIZE_USIZE;
        let alloc_spot = unwrap_ok!(get_spot(alloc_size));
        expect_eq!(base.0, alloc_spot.0);
        let _r0 = unwrap_ok!(insert_sub_vmar(alloc_spot, alloc_size));

        // Manually insert a sub region at [+2*PAGE_SIZE, +3*PAGE_SIZE].
        let r_mid =
            unwrap_ok!(insert_sub_vmar(VAddr(base.0 + 2 * PAGE_SIZE_USIZE), PAGE_SIZE_USIZE));

        // Try to allocate 2 pages; since the gap is too small, we allocate at
        // [+3*PAGE_SIZE, +5*PAGE_SIZE].
        let alloc_size = 2 * PAGE_SIZE_USIZE;
        let alloc_spot = unwrap_ok!(get_spot(alloc_size));
        expect_eq!(base.0 + 3 * PAGE_SIZE_USIZE, alloc_spot.0);
        let _r1 = unwrap_ok!(insert_sub_vmar(alloc_spot, alloc_size));

        expect_ok!(r_mid.destroy());

        // After removing the region, we now have a gap at [+PAGE_SIZE, +3*PAGE_SIZE].
        let alloc_size = 2 * PAGE_SIZE_USIZE;
        let alloc_spot = unwrap_ok!(get_spot(alloc_size));
        expect_eq!(base.0 + PAGE_SIZE_USIZE, alloc_spot.0);
        let _r2 = unwrap_ok!(insert_sub_vmar(alloc_spot, alloc_size));

        // Now that all earlier gaps are filled, the next region should start at +5*PAGE_SIZE.
        let alloc_size = PAGE_SIZE_USIZE;
        let alloc_spot = unwrap_ok!(get_spot(alloc_size));
        expect_eq!(base.0 + 5 * PAGE_SIZE_USIZE, alloc_spot.0);
        let _r3 = unwrap_ok!(insert_sub_vmar(alloc_spot, alloc_size));

        // Allocate all the remaining space from [+6*PAGE_SIZE, base + size).
        let alloc_size = size - 6 * PAGE_SIZE_USIZE;
        let alloc_spot = unwrap_ok!(get_spot(alloc_size));
        expect_eq!(base.0 + 6 * PAGE_SIZE_USIZE, alloc_spot.0);

        expect_ok!(aspace.destroy());
    }

    /// Tests `region_list::get_alloc_spot` returning `Status::NO_RESOURCES` when no gap fits.
    #[test]
    fn region_list_get_alloc_spot_no_memory_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let size = 4 * PAGE_SIZE_USIZE;
        let vmar_flags =
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE;
        let test_vmar = unwrap_ok!(root_vmar.create_sub_vmar(0, size, 0, vmar_flags, b"test vmar"));
        let base = test_vmar.base();
        let align_pow2 = page::SHIFT as u8;

        let _sub = unwrap_ok!(test_vmar.create_sub_vmar(
            0,
            size - PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC | vmar_flags,
            b"sub vmar",
        ));

        // There is only a 1 page gap, and we are asking for two pages, so `Status::NO_RESOURCES`
        // should be returned both without and with a PRNG.
        let alloc_size = 2 * PAGE_SIZE_USIZE;
        {
            stack_pin_init!(let region_guard = test_vmar.region_lock().lock());
            let res = vmar::region_list::get_alloc_spot(
                &test_vmar,
                region_guard.token(),
                align_pow2,
                /*entropy=*/ 0,
                alloc_size,
                base,
                size,
                /*prng=*/ None,
                VAddr(usize::MAX),
            );
            expect_err!(res, Status::NO_RESOURCES);

            let res_prng = vmar::region_list::get_alloc_spot(
                &test_vmar,
                region_guard.token(),
                align_pow2,
                /*entropy=*/ 8,
                alloc_size,
                base,
                size,
                Some(aspace.aslr_prng()),
                VAddr(usize::MAX),
            );
            expect_err!(res_prng, Status::NO_RESOURCES);
        }

        expect_ok!(aspace.destroy());
    }

    /// Tests `region_list::get_alloc_spot` with a PRNG and VMAR spot allocation helpers.
    #[test]
    fn region_list_get_alloc_spot_prng_and_helpers_test() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"test aspace"));
        let prng = aspace.aslr_prng();

        // Exercise `crypto::Prng::rand_int` bounds directly.
        expect_eq!(0, prng.rand_int(1));
        for bound in [2u64, 3, 7, 64, 1 << 20] {
            expect_true!(prng.rand_int(bound) < bound);
        }

        let root_vmar = unwrap_some!(aspace.root_vmar());
        let size = 4 * PAGE_SIZE_USIZE;
        let vmar_flags =
            vmar::flag::CAN_MAP_SPECIFIC | vmar::flag::CAN_MAP_READ | vmar::flag::CAN_MAP_WRITE;
        let test_vmar = unwrap_ok!(root_vmar.create_sub_vmar(0, size, 0, vmar_flags, b"test vmar"));
        let base = test_vmar.base();
        let align_pow2 = page::SHIFT as u8;

        // Occupy [+PAGE_SIZE, +2*PAGE_SIZE), leaving 3 candidate 1-page spots:
        // [+0, +PAGE_SIZE), [+2*PAGE_SIZE, +3*PAGE_SIZE), and [+3*PAGE_SIZE, +4*PAGE_SIZE).
        let _occupied = unwrap_ok!(test_vmar.create_sub_vmar(
            PAGE_SIZE_USIZE,
            PAGE_SIZE_USIZE,
            0,
            vmar::flag::SPECIFIC | vmar_flags,
            b"occupied",
        ));

        {
            stack_pin_init!(let region_guard = test_vmar.region_lock().lock());
            let token = region_guard.token();

            // With `entropy = 36`, `max_candidate_spaces = 1 << 36` while
            // `candidate_spot_count = 3`, exercising the two-pass retry path in `get_alloc_spot`
            // when `selected_index >= 3`.
            for _ in 0..8 {
                let spot = unwrap_ok!(vmar::region_list::get_alloc_spot(
                    &test_vmar,
                    token,
                    align_pow2,
                    /*entropy=*/ 36,
                    PAGE_SIZE_USIZE,
                    base,
                    size,
                    Some(prng),
                    VAddr(usize::MAX),
                ));
                expect_true!(
                    spot == base
                        || spot == VAddr(base.0 + 2 * PAGE_SIZE_USIZE)
                        || spot == VAddr(base.0 + 3 * PAGE_SIZE_USIZE)
                );
            }

            // Exercise `alloc_spot_locked_region` directly on `test_vmar`.
            let spot = unwrap_ok!(test_vmar.alloc_spot_locked_region(
                token,
                2 * PAGE_SIZE_USIZE,
                align_pow2,
                ARCH_MMU_FLAG_PERM_READ,
                VAddr(usize::MAX),
            ));
            expect_eq!(base.0 + 2 * PAGE_SIZE_USIZE, spot.0);

            let no_res = test_vmar.alloc_spot_locked_region(
                token,
                3 * PAGE_SIZE_USIZE,
                align_pow2,
                ARCH_MMU_FLAG_PERM_READ,
                VAddr(usize::MAX),
            );
            expect_err!(no_res, Status::NO_RESOURCES);

            // Exercise `upper_bound_neighbours` and `check_gap_locked_region` directly on both
            // the 1-page gap before `_occupied` and the 2-page gap after `_occupied`.
            let (before_first, after_first) = vmar::region_list::upper_bound_neighbours(
                &test_vmar,
                token,
                VAddr(base.0 + PAGE_SIZE_USIZE - 1),
            );
            expect_true!(before_first.is_none());
            expect_true!(after_first.is_some());
            expect_true!(
                test_vmar.check_gap_locked_region(
                    token,
                    before_first,
                    after_first,
                    base,
                    PAGE_SIZE_USIZE,
                    PAGE_SIZE_USIZE,
                    0,
                    ARCH_MMU_FLAG_PERM_READ,
                ) == Some(base)
            );
            expect_true!(
                test_vmar
                    .check_gap_locked_region(
                        token,
                        before_first,
                        after_first,
                        base,
                        PAGE_SIZE_USIZE,
                        2 * PAGE_SIZE_USIZE,
                        0,
                        ARCH_MMU_FLAG_PERM_READ,
                    )
                    .is_none()
            );

            let (before_second, after_second) = vmar::region_list::upper_bound_neighbours(
                &test_vmar,
                token,
                VAddr(base.0 + 2 * PAGE_SIZE_USIZE),
            );
            expect_true!(before_second.is_some());
            expect_true!(after_second.is_none());
            expect_true!(
                test_vmar.check_gap_locked_region(
                    token,
                    before_second,
                    after_second,
                    VAddr(base.0 + 2 * PAGE_SIZE_USIZE),
                    PAGE_SIZE_USIZE,
                    2 * PAGE_SIZE_USIZE,
                    0,
                    ARCH_MMU_FLAG_PERM_READ,
                ) == Some(VAddr(base.0 + 2 * PAGE_SIZE_USIZE))
            );
        }

        expect_ok!(aspace.destroy());
    }

    struct TestRegionList {
        aspace: RefPtr<VmAspace>,
        regions: RefPtr<VmAddressRegion>,
    }

    impl TestRegionList {
        fn new() -> Self {
            let aspace = VmAspace::create(Type::User, b"test aspace").expect("aspace creation");
            // SAFETY: The region is initialized by `RefPtr::pin_init`, in storage that
            // `allocate_uninit` returned.
            let init =
                unsafe { VmAddressRegion::new_root(&aspace, aspace.base(), aspace.size(), 0) };
            let regions = RefPtr::pin_init(init).expect("region list allocation");
            regions.trace_new();
            Self { aspace, regions }
        }

        fn insert_region(
            &self,
            region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
            token: &mut LockToken<'_, VmAspaceLockClass>,
            base: usize,
            size: usize,
        ) -> bool {
            // SAFETY: The region is initialized by `RefPtr::pin_init`, in storage that
            // `allocate_uninit` returned.
            let init =
                unsafe { VmAddressRegion::new_child(&self.regions, base, size, 0, b"test region") };
            let Ok(test_region) = RefPtr::pin_init(init) else {
                return false;
            };
            test_region.trace_new();
            if !vmar::region_list::is_range_available(
                &self.regions,
                region_token,
                VAddr(base),
                size,
            ) {
                return false;
            }
            // SAFETY: `test_region` is kept alive by `test_region`, and no child of `self.regions`
            // overlaps `[base, base + size)`.
            unsafe {
                vmar::region_list::insert_region(
                    &self.regions,
                    region_token,
                    token,
                    test_region.as_region_or_mapping(),
                )
            }
            .is_ok()
        }

        fn remove_region(
            &self,
            region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
            token: &mut LockToken<'_, VmAspaceLockClass>,
            base: usize,
        ) -> bool {
            let Some(region) = vmar::region_list::find_region(&self.regions, token, VAddr(base))
            else {
                return false;
            };
            let region = region.to_owned();
            // SAFETY: `region` is a child of `self.regions` and is kept alive by `region`.
            unsafe {
                vmar::region_list::remove_region(&self.regions, region_token, token, &region)
            };
            true
        }
    }

    impl Drop for TestRegionList {
        fn drop(&mut self) {
            self.aspace.destroy().expect("aspace destroy");
        }
    }

    /// Tests `region_list::find_spot_at_index`.
    #[test]
    fn region_list_find_spot_at_index_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;
        let size: usize = 0x0001000000000000;
        // Set the align to be 0x1000.
        let align_pow2 = 12;
        // Allocate 1 page, should be allocated at [+0, +0x1000].
        let alloc_size = 0x1000;
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let alloc_spot = unwrap_some!(result.ok());
        expect_eq!(base, alloc_spot.0);

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            alloc_spot.0,
            alloc_size,
        ));

        // Manually insert a sub region at [+0x2000, 0x3000].
        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x2000,
            alloc_size,
        ));

        // Try to allocate 2 pages, since the gap is too small, we would allocate at
        // [0x3000, 0x5000].
        let alloc_size = 0x2000;
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let alloc_spot = unwrap_some!(result.ok());
        expect_eq!(base + 0x3000, alloc_spot.0);
        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            alloc_spot.0,
            alloc_size,
        ));

        expect_true!(test_list.remove_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x2000,
        ));

        // After we remove the region, we now have a gap at [0x1000, 0x3000].
        let alloc_size = 0x2000;
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let alloc_spot = unwrap_some!(result.ok());
        expect_eq!(base + 0x1000, alloc_spot.0);
        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            alloc_spot.0,
            alloc_size,
        ));

        // Now we have filled all the gaps, next region should start at 0x5000.
        let alloc_size = 0x1000;
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let alloc_spot = unwrap_some!(result.ok());
        expect_eq!(base + 0x5000, alloc_spot.0);
        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            alloc_spot.0,
            alloc_size,
        ));

        // Test for possible overflow cases. We try to allocate all the rest of the spaces. The
        // last region should be from [0x6000, base + size - 1], we should be able to find this
        // region and allocate all the size from it.
        let alloc_size = size - 0x6000;
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let alloc_spot = unwrap_some!(result.ok());
        expect_eq!(base + 0x6000, alloc_spot.0);
    }

    /// Tests `region_list::find_spot_at_index` finding no candidate spot.
    #[test]
    fn region_list_find_spot_at_index_no_memory_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;
        let size: usize = 0x0001000000000000;
        // Set the align to be 0x1000.
        let align_pow2 = 12;

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base,
            size - 0x1000,
        ));

        let alloc_size = 0x2000;
        // There is only a 1 page gap, and we are asking for two pages, so no candidate spot should
        // be found.
        let result = vmar::region_list::find_spot_at_index(
            regions,
            region_guard.token(),
            /*target_index=*/ 0,
            align_pow2,
            alloc_size,
            VAddr(base),
            size,
            VAddr(usize::MAX),
        );
        let failed = unwrap_some!(result.err());
        expect_eq!(0, failed.candidate_spot_count);
    }

    /// Tests `region_list::find_region`.
    #[test]
    fn region_list_find_region_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;

        let region = vmar::region_list::find_region(regions, guard.token(), VAddr(base));
        expect_true!(region.is_none());

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x1000,
            0x1000,
        ));

        let region = vmar::region_list::find_region(regions, guard.token(), VAddr(base + 1));
        expect_true!(region.is_none());

        let region = vmar::region_list::find_region(regions, guard.token(), VAddr(base + 0x1001));
        let region = unwrap_some!(region);
        expect_eq!(base + 0x1000, region.base().0);
        expect_eq!(0x1000, region.size());
    }

    /// Tests `region_list::include_or_higher`.
    #[test]
    fn region_list_include_or_higher_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x1000,
            0x1000,
        ));

        let itr = vmar::region_list::include_or_higher(regions, guard.token(), VAddr(base + 1));
        let region = unwrap_some!(itr.get());
        expect_eq!(base + 0x1000, region.base().0);
        expect_eq!(0x1000, region.size());

        let itr =
            vmar::region_list::include_or_higher(regions, guard.token(), VAddr(base + 0x1001));
        let region = unwrap_some!(itr.get());
        expect_eq!(base + 0x1000, region.base().0);
        expect_eq!(0x1000, region.size());

        let itr =
            vmar::region_list::include_or_higher(regions, guard.token(), VAddr(base + 0x2000));
        expect_false!(itr.is_valid());
    }

    /// Tests `region_list::upper_bound_locked`.
    #[test]
    fn region_list_upper_bound_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x1000,
            0x1000,
        ));

        let itr =
            vmar::region_list::upper_bound_locked(regions, guard.token(), VAddr(base + 0xFFF));
        let region = unwrap_some!(itr.get());
        expect_eq!(base + 0x1000, region.base().0);
        expect_eq!(0x1000, region.size());

        let itr =
            vmar::region_list::upper_bound_locked(regions, guard.token(), VAddr(base + 0x1000));
        expect_false!(itr.is_valid());
    }

    /// Tests `region_list::is_range_available`.
    #[test]
    fn region_list_is_range_available_test() {
        let test_list = TestRegionList::new();
        let regions = &*test_list.regions;
        ksync::lock!(let mut region_guard = regions.region_lock().lock());
        ksync::lock!(let mut guard = regions.lock().lock());
        let base: usize = 0xFFFF000000000000;

        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x1000,
            0x1000,
        ));
        assert_true!(test_list.insert_region(
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
            base + 0x3000,
            0x1000,
        ));

        let available = |base: usize, size: usize| {
            vmar::region_list::is_range_available(regions, region_guard.token(), VAddr(base), size)
        };
        expect_true!(available(base, 0x1000));
        expect_false!(available(base, 0x1001));
        expect_false!(available(base + 1, 0x1000));
        expect_true!(available(base + 0x2000, 1));
        expect_false!(available(base + 0x1FFF, 0x2000));

        expect_true!(available(0xFFFFFFFFFFFFFFFF, 1));
        expect_false!(available(base, 0x0001000000000000));
    }

    /// Check if a range of addresses is accessible to the user. If `spectre_validation` is true, this is
    /// done by checking if `validate_user_accessible_range` returns `{0,0}`. Otherwise, check using
    /// `is_user_accessible_range`.
    fn check_user_accessible_range(
        mut vaddr: usize,
        mut len: usize,
        spectre_validation: bool,
    ) -> bool {
        if spectre_validation {
            // If the address and length were not modified, then the pair is valid.
            let old_vaddr = vaddr;
            let old_len = len;
            validate_user_accessible_range(&mut vaddr, &mut len);
            return vaddr == old_vaddr && len == old_len;
        }

        is_user_accessible_range(vaddr, len)
    }

    fn check_user_accessible_range_test(spectre_validation: bool) -> bool {
        let run = subtest!(|spectre_validation: bool| {
            use crate::arch_rs::USER_ASPACE_BASE;

            let mut va: usize;
            let mut len: usize;

            // Test address of zero.
            va = 0;
            len = PAGE_SIZE_USIZE;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test address and length of zero (both are valid).
            va = 0;
            len = 0;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test very end of address space and zero length (this is invalid since the start has bit 55 set
            // despite zero length).
            va = usize::MAX;
            len = 0;
            expect_false!(check_user_accessible_range(va, len, spectre_validation));

            // Test a regular user address.
            va = USER_ASPACE_BASE;
            len = PAGE_SIZE_USIZE;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test zero-length on a regular user address.
            va = USER_ASPACE_BASE;
            len = 0;
            expect_true!(check_user_accessible_range(va, len, spectre_validation));

            // Test overflow past 64 bits.
            va = USER_ASPACE_BASE;
            len = (usize::MAX - va).wrapping_add(1);
            expect_false!(check_user_accessible_range(va, len, spectre_validation));

            #[cfg(target_arch = "aarch64")]
            {
                use crate::arch_rs::vm::is_user_accessible;

                // On aarch64, an address is accessible to the user if bit 55 is zero.

                // Test starting on a bad user address.
                let bad_addr_mask = 1usize << 55;
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = PAGE_SIZE_USIZE;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test zero-length on a bad user address.
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = 0;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test 2^55 is in the range of `[va, va+len)`, ending on a bad user address.
                va = USER_ASPACE_BASE;
                len = bad_addr_mask;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test this returns false if any address within the range of `[va, va+len)`
                // contains a value where bit 55 is set. This also implies there are many
                // gaps in ranges above 2^56.
                //
                // Here both the start and end values are valid, but this range contains an
                // address that is invalid.
                va = 0;
                len = 0x017f_ffff_ffff_ffff; // Bits 0-56 (except 55) are set.
                assert_true!(is_user_accessible(va));
                assert_true!(is_user_accessible(va + len));
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test the range of the largest value less than 2^55 and the smallest value
                // greater than 2^55 where bit 55 == 0.
                va = (1usize << 55) - 1;
                len = 0x0080_0000_0000_0001; // End = `va` + `len` = 2^56.
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Be careful not to just check that 2^55 is in the range. We really want to
                // check whenever bit 55 is flipped in the range.
                va = 0x017f_ffff_ffff_ffff; // Start above 2^56. Bit 55 is not set.
                // End = `va` + `len` = `0x200_0000_0000_0000`. This is above 2^56 and bit 55 also is not set.
                len = 0x0080_0000_0000_0001;
                assert_true!(is_user_accessible(va));
                assert_true!(is_user_accessible(va + len));
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                va = USER_ASPACE_BASE;
                len = (1usize << 57) + 1;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test a range above 2^56 where bit 55 is never set.
                va = 0x0170_0000_0000_0000;
                len = 0x000f_ffff_ffff_ffff;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));

                // Test a range right below 2^55 where bit 55 is never set.
                va = 0x0070_0000_0000_0000;
                len = 0x000f_ffff_ffff_ffff;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));

                // Test the last valid user space address with a tag of 0.
                va = usize::MAX;
                va &= !(0xffusize << 56); // Set tag to zero.
                va &= !bad_addr_mask; // Ensure valid user address.
                len = 0;
                expect_true!(check_user_accessible_range(va, len, spectre_validation));
            }

            #[cfg(target_arch = "x86_64")]
            {
                // On x86_64, an address is accessible to the user if bits 48-63 are zero.

                // Test a bad user address.
                let bad_addr_mask = 1usize << 48;
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = PAGE_SIZE_USIZE;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test zero-length on a bad user address.
                va = bad_addr_mask | USER_ASPACE_BASE;
                len = 0;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));

                // Test ending on a bad user address.
                va = USER_ASPACE_BASE;
                len = bad_addr_mask;
                expect_false!(check_user_accessible_range(va, len, spectre_validation));
            }
        });
        run(spectre_validation)
    }

    /// Tests `is_user_accessible_range`.
    #[test]
    fn arch_is_user_accessible_range() {
        expect_true!(check_user_accessible_range_test(false));
    }

    /// Tests `validate_user_accessible_range`.
    #[test]
    fn validate_user_address_range() {
        expect_true!(check_user_accessible_range_test(true));
    }

    /// Doesn't do anything, just prints all aspaces.
    #[test]
    fn dump_all_aspaces() {
        // Doesn't do anything, just prints all aspaces.
        // Should be run after all other tests so that people can manually comb
        // through the output for leaked test aspaces.

        // Set to true for debugging.
        if false {
            kprintln!("verify there are no test aspaces left around");
            VmAspace::dump_all_aspaces();
        }
    }
}
