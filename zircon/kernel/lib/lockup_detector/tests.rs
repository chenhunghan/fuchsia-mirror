// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Unit tests for the lockup detector.

/// lockup_detector tests
#[unittest::suite(name = "lockup_detector")]
mod lockup_detector_tests {
    use core::ffi::CStr;
    #[cfg(target_arch = "aarch64")]
    use core::mem::DropGuard;
    use core::ptr;
    use core::sync::atomic::Ordering;

    #[cfg(target_arch = "aarch64")]
    use unittest::{
        assert_eq, assert_err, assert_false, assert_ok, assert_true, expect_true, subtest,
        unwrap_ok, unwrap_some,
    };
    use unittest::{expect_eq, expect_ge};
    use zr::defer;
    #[cfg(target_arch = "aarch64")]
    use zx_status::Status;

    #[cfg(target_arch = "aarch64")]
    use crate::arch_rs::arm64::dap::Arm64DapProcessorState;
    #[cfg(target_arch = "aarch64")]
    use crate::arch_rs::arm64::vm::is_kernel_address;
    use crate::arch_rs::curr_cpu_num as arch_curr_cpu_num;
    #[cfg(target_arch = "aarch64")]
    use crate::backtrace::Backtrace;
    use crate::kernel::thread::AutoPreemptDisabler;
    #[cfg(target_arch = "aarch64")]
    use crate::lockup_detector::diagnostics;
    use crate::lockup_detector::{
        CriticalSectionLockupChecker, LockupDetectorState, lockup_begin, lockup_end,
        lockup_timed_begin, lockup_timed_end,
    };
    use crate::platform_rs::timer::timer_current_boot_ticks;
    #[cfg(target_arch = "aarch64")]
    use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
    #[cfg(target_arch = "aarch64")]
    use crate::vm::pmm::ALLOC_FLAG_ANY;
    #[cfg(target_arch = "aarch64")]
    use crate::vm::vm_address_region;
    #[cfg(target_arch = "aarch64")]
    use crate::vm::vm_aspace::VmAspace;
    #[cfg(target_arch = "aarch64")]
    use crate::vm::vm_object_paged::VmObjectPaged;

    /// Nested untimed critical sections only track the outermost one.
    #[test]
    fn nested_critical_section() {
        let _ap_disabler = AutoPreemptDisabler::new();

        // For the context of this test, use the maximum threshold to prevent the detector from
        // "firing".
        let orig = CriticalSectionLockupChecker::threshold_ticks();
        let _cleanup = defer(move || CriticalSectionLockupChecker::set_threshold_ticks(orig));
        CriticalSectionLockupChecker::set_threshold_ticks(i64::MAX);

        let state = LockupDetectorState::get(arch_curr_cpu_num());
        let cs_state = &state.critical_section;

        expect_eq!(0, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));

        const OUTER: &CStr = c"NestedCriticalSectionTest-outer";
        // SAFETY: Preemption is disabled by `_ap_disabler`, and the call is balanced below.
        unsafe { lockup_begin(OUTER) };
        expect_eq!(1, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        const INNER: &CStr = c"NestedCriticalSectionTest-inner";
        // SAFETY: Preemption is disabled by `_ap_disabler`, and the call is balanced below.
        unsafe { lockup_begin(INNER) };
        expect_eq!(2, cs_state.depth.load(Ordering::Relaxed));
        // No change because only the outermost critical section is tracked.
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        // SAFETY: Preemption is disabled by `_ap_disabler`; balances `lockup_begin(INNER)`.
        unsafe { lockup_end() };
        expect_eq!(1, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        // SAFETY: Preemption is disabled by `_ap_disabler`; balances `lockup_begin(OUTER)`.
        unsafe { lockup_end() };
        expect_eq!(0, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed), ptr::null_mut());
    }

    /// Nested timed critical sections only time the outermost one.
    #[test]
    fn nested_timed_critical_section() {
        let _ap_disabler = AutoPreemptDisabler::new();

        // For the context of this test, use the maximum threshold to prevent the detector from
        // "firing".
        let orig = CriticalSectionLockupChecker::threshold_ticks();
        let _cleanup = defer(move || CriticalSectionLockupChecker::set_threshold_ticks(orig));
        CriticalSectionLockupChecker::set_threshold_ticks(i64::MAX);

        let state = LockupDetectorState::get(arch_curr_cpu_num());
        let cs_state = &state.critical_section;

        expect_eq!(0, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));

        let now = timer_current_boot_ticks().0;

        const OUTER: &CStr = c"NestedTimedCriticalSectionTest-outer";
        // SAFETY: Preemption is disabled by `_ap_disabler`, and the call is balanced below.
        unsafe { lockup_timed_begin(OUTER) };
        expect_eq!(1, cs_state.depth.load(Ordering::Relaxed));

        let begin_ticks = cs_state.begin_ticks.load(Ordering::Relaxed);
        expect_ge!(cs_state.begin_ticks.load(Ordering::Relaxed), now);
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        const INNER: &CStr = c"NestedTimedCriticalSectionTest-inner";
        // SAFETY: Preemption is disabled by `_ap_disabler`, and the call is balanced below.
        unsafe { lockup_timed_begin(INNER) };
        expect_eq!(2, cs_state.depth.load(Ordering::Relaxed));

        // No change because only the outermost critical section is tracked.
        expect_eq!(begin_ticks, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        // SAFETY: Preemption is disabled by `_ap_disabler`; balances `lockup_timed_begin(INNER)`.
        unsafe { lockup_timed_end() };
        expect_eq!(1, cs_state.depth.load(Ordering::Relaxed));

        expect_eq!(begin_ticks, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed).cast_const(), OUTER.as_ptr());

        // SAFETY: Preemption is disabled by `_ap_disabler`; balances `lockup_timed_begin(OUTER)`.
        unsafe { lockup_timed_end() };
        expect_eq!(0, cs_state.depth.load(Ordering::Relaxed));
        expect_eq!(0, cs_state.begin_ticks.load(Ordering::Relaxed));
        expect_eq!(cs_state.name.load(Ordering::Relaxed), ptr::null_mut());
    }

    /// `get_backtrace_from_dap_state` validates the SCSP and never crosses a page boundary.
    #[test]
    fn get_backtrace_from_dap_state() {
        #[cfg(not(target_arch = "aarch64"))]
        kprint::kprintln!("this is an arm64-only test, skipping");

        #[cfg(target_arch = "aarch64")]
        {
            const PC: u64 = 0xffffffff10000000;
            const LR: u64 = 0xffffffff10000001;
            const EDSCR_EL0: u32 = 0x3053a13;
            const EDSCR_EL1: u32 = 0x3053d13;

            // A DAP state at `PC`/`LR` in the exception level `edscr` encodes, with `scsp` in x18.
            let dap_state = |edscr: u32, scsp: u64| {
                let mut r = [0u64; 31];
                r[18] = scsp;
                r[30] = LR;
                Arm64DapProcessorState { r, pc: PC, edscr, ..Default::default() }
            };

            // CPU is in EL0.
            {
                let state = dap_state(EDSCR_EL0, 0);
                let mut bt = Backtrace::new();
                // SAFETY: `state` is in EL0, so `scsp` is never dereferenced.
                assert_err!(
                    unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) },
                    Status::BAD_STATE
                );
                assert_eq!(0, bt.size());
            }

            let check_backtrace =
                subtest!(|bt: &Backtrace,
                          size: usize,
                          slot0: usize,
                          slot1: usize,
                          top_of_stack_value: usize| {
                    assert_eq!(size, bt.size());
                    expect_eq!(slot0, bt.data()[0]);
                    expect_eq!(slot1, bt.data()[1]);
                    for (offset, &addr) in bt.data()[2..size].iter().enumerate() {
                        expect_eq!(top_of_stack_value - offset, addr);
                    }
                });

            // Misaligned SCSP.
            {
                let state = dap_state(EDSCR_EL1, 0xffff0000172cc4b1);
                let mut bt = Backtrace::new();
                // SAFETY: `scsp` is misaligned, so `get_backtrace_from_dap_state` returns before
                // dereferencing it.
                assert_err!(
                    unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) },
                    Status::INVALID_ARGS
                );
                expect_true!(check_backtrace(&bt, 2, PC as usize, LR as usize, 0));
            }

            // Null SCSP.
            {
                let state = dap_state(EDSCR_EL1, 0x0);
                let mut bt = Backtrace::new();
                // SAFETY: `scsp` is null, so `get_backtrace_from_dap_state` returns before
                // dereferencing it.
                assert_err!(
                    unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) },
                    Status::INVALID_ARGS
                );
                expect_true!(check_backtrace(&bt, 2, PC as usize, LR as usize, 0));
            }

            // SCSP is not a kernel address.
            {
                let state = dap_state(EDSCR_EL1, 0xdc050800);
                assert_false!(is_kernel_address(state.r[18] as usize));
                let mut bt = Backtrace::new();
                // SAFETY: `scsp` is outside kernel address space, so `get_backtrace_from_dap_state`
                // returns before dereferencing it.
                assert_err!(
                    unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) },
                    Status::OUT_OF_RANGE
                );
                expect_true!(check_backtrace(&bt, 2, PC as usize, LR as usize, 0));
            }

            // Create a region of four pages.  The middle two are mapped and the ends are "holes".
            const REGION_SIZE: usize = page::SIZE * 4;
            const VMO_SIZE: usize = page::SIZE * 2;
            const VMAR_FLAGS: u32 = vm_address_region::flag::CAN_MAP_SPECIFIC
                | vm_address_region::flag::CAN_MAP_READ
                | vm_address_region::flag::CAN_MAP_WRITE;
            const NAME: &[u8] = b"lockup_detector test";
            let root_vmar = unwrap_some!(VmAspace::kernel_aspace().root_vmar());
            let vmar = unwrap_ok!(root_vmar.create_sub_vmar(0, REGION_SIZE, 0, VMAR_FLAGS, NAME));
            let vmar = DropGuard::new(vmar, |vmar| {
                let _ = vmar.destroy();
            });

            // Create a VMO of two pages and map it in the middle.
            //
            //         mapping.base()
            //         V
            // [-hole-][page-1][page-2][-hole-]
            // ^
            // vmar.base()
            //
            let vmo = unwrap_ok!(VmObjectPaged::create(
                ALLOC_FLAG_ANY,
                VmObjectPaged::ALWAYS_PINNED,
                VMO_SIZE as u64
            ));
            let mapping_result = unwrap_ok!(vmar.create_vm_mapping_no_overwrite(
                page::SIZE,
                VMO_SIZE,
                0,
                vm_address_region::flag::SPECIFIC,
                VmObjectPaged::into_vm_object(vmo),
                0,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            ));
            // Eagerly fault in the pages.
            assert_ok!(mapping_result.mapping.map_range(0, VMO_SIZE, true, false));

            // Fill the two middle pages with some "return addresses".
            let num_elements = VMO_SIZE / size_of::<usize>();
            let p = core::ptr::with_exposed_provenance_mut::<usize>(mapping_result.base);
            for i in 0..num_elements {
                // SAFETY: `p` points to `num_elements` mapped, writable, pointer-aligned slots that
                // nothing else references while this test runs.
                unsafe { p.add(i).write(i) };
            }

            // SCSP points to middle of an unmapped region.
            //
            // [-hole-][page-1][page-2][-hole-]
            //    ^
            {
                let state = dap_state(EDSCR_EL1, (vmar.base().0 + 64) as u64);
                assert_true!(is_kernel_address(state.r[18] as usize));
                let mut bt = Backtrace::new();
                // SAFETY: `scsp - 8` lies in the unmapped hole, so `arm64_mmu_translate` fails
                // before any memory read occurs.
                assert_err!(
                    unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) },
                    Status::NOT_FOUND
                );
                expect_true!(check_backtrace(&bt, 2, PC as usize, LR as usize, 0));
            }

            // SCSP points to the first address of an unmapped page that follows a mapped page.
            //
            // [-hole-][page-1][page-2][-hole-]
            //                         ^
            {
                let state = dap_state(EDSCR_EL1, (vmar.base().0 + page::SIZE * 3) as u64);
                assert_true!(is_kernel_address(state.r[18] as usize));
                let mut bt = Backtrace::new();
                // See that we get a full backtrace.  The fact that SCSP pointed at an unmapped
                // page didn't matter because it was post-increment semantics (it was pointing at
                // an empty slot).
                // SAFETY: `scsp - 8` is the last slot of `page-2`, which is mapped, initialized
                // kernel RAM disjoint from `bt`.
                assert_ok!(unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) });
                // SAFETY: The slot before `r[18]` is the last slot of `page-2`, which is mapped and
                // was filled in above.
                let top_of_stack_value = unsafe {
                    core::ptr::with_exposed_provenance::<usize>(state.r[18] as usize).sub(1).read()
                };
                expect_true!(check_backtrace(
                    &bt,
                    Backtrace::MAX_SIZE,
                    PC as usize,
                    LR as usize,
                    top_of_stack_value
                ));
            }

            // SCS crosses a page boundary.  See that the backtrace does not.
            //
            // [-hole-][page-1][page-2][-hole-]
            //                  ^
            {
                let state = dap_state(EDSCR_EL1, (vmar.base().0 + page::SIZE * 2 + 16) as u64);
                assert_true!(is_kernel_address(state.r[18] as usize));
                let mut bt = Backtrace::new();
                // SAFETY: `state.r[18]` is at offset `+ 16` (`2 * size_of::<usize>()`) into
                // `page-2`, so `scsp - 8` (`state.r[18] - size_of::<usize>()`) lies in `page-2`,
                // which is mapped, initialized kernel RAM disjoint from `bt`.
                assert_ok!(unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) });
                // SAFETY: `state.r[18]` is at offset `+ 16` (`2 * size_of::<usize>()`) into
                // `page-2`, so `.sub(1)` remains `usize`-aligned and in bounds of `page-2`, which
                // is mapped and was filled in above.
                let top_of_stack_value = unsafe {
                    core::ptr::with_exposed_provenance::<usize>(state.r[18] as usize).sub(1).read()
                };
                expect_true!(check_backtrace(&bt, 4, PC as usize, LR as usize, top_of_stack_value));
            }

            // Unmapped page, followed by small SCS (less than Backtrace::MAX_SIZE).
            //
            // [-hole-][page-1][page-2][-hole-]
            //           ^
            {
                let state = dap_state(EDSCR_EL1, (vmar.base().0 + page::SIZE + 16) as u64);
                assert_true!(is_kernel_address(state.r[18] as usize));
                let mut bt = Backtrace::new();
                // SAFETY: `state.r[18]` is at offset `+ 16` (`2 * size_of::<usize>()`) into
                // `page-1`, so `scsp - 8` (`state.r[18] - size_of::<usize>()`) lies in `page-1`,
                // which is mapped, initialized kernel RAM disjoint from `bt`.
                assert_ok!(unsafe { diagnostics::get_backtrace_from_dap_state(&state, &mut bt) });
                // SAFETY: `state.r[18]` is at offset `+ 16` (`2 * size_of::<usize>()`) into
                // `page-1`, so `.sub(1)` remains `usize`-aligned and in bounds of `page-1`, which
                // is mapped and was filled in above.
                let top_of_stack_value = unsafe {
                    core::ptr::with_exposed_provenance::<usize>(state.r[18] as usize).sub(1).read()
                };
                expect_true!(check_backtrace(&bt, 4, PC as usize, LR as usize, top_of_stack_value));
            }
        }
    }
}
