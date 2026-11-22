// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// x64 platform tests.
#[unittest::suite(name = "x64_platform_tests")]
mod tests {
    use core::arch::asm;
    use core::mem::size_of;
    use core::ptr;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use boot_options::BootOptions;
    use kprint::kprintln;
    use unittest::{
        assert_eq, assert_true, expect_eq, expect_err, expect_ge, expect_le, expect_ne, expect_ok,
        unwrap_ok,
    };
    use zx_status::Status;
    #[cfg(console_enabled)]
    use zx_types::ZX_OK;

    use crate::arch_rs::x86::apic::ApicInterruptDeliveryMode;
    #[cfg(console_enabled)]
    use crate::arch_rs::x86::cpuid::RealCpuId;
    use crate::arch_rs::x86::descriptor::{Gdtr, IO_BITMAP_BYTES};
    use crate::arch_rs::x86::feature::{
        X86_FEATURE_RDTSCP, X86HypervisorList, x86_cpu_should_md_clear_on_user_return,
        x86_feature_test, x86_get_hypervisor,
    };
    #[cfg(console_enabled)]
    use crate::arch_rs::x86::hwp::{intel_hwp_init, intel_hwp_supported};
    use crate::arch_rs::x86::interrupts::X86_INT_NMI;
    use crate::arch_rs::x86::lapic::apic_send_mask_ipi;
    use crate::arch_rs::x86::mp::get_percpu;
    use crate::arch_rs::x86::msr::{read_msr_on_cpu, read_msr_safe_on_cpu, write_msr_on_cpu};
    #[cfg(console_enabled)]
    use crate::arch_rs::x86::platform_access::RealMsrAccess;
    use crate::arch_rs::x86::registers::{
        X86_MSR_IA32_FMASK, X86_MSR_IA32_LSTAR, X86_MSR_IA32_TSC_AUX,
    };
    use crate::arch_rs::x86::x86::{read_msr, write_msr};
    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num, max_num_cpus};
    #[cfg(console_enabled)]
    use crate::console::console_run_script_locked;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::mp::{self, MpIpiTarget, cpu_num_to_mask};
    use crate::kernel::scheduler::peek_active_mask;
    use crate::platform_pc::timer::{
        convert_raw_tsc_duration_to_nanoseconds, convert_raw_tsc_timestamp_to_clock_monotonic,
    };
    use crate::platform_rs::timer::{DurationMono, current_mono_time, zx_msec};

    // SAFETY: `__x86_indirect_thunk_r11` is an 18-byte `code_patching_hermetic_stub` (`16 <= 18`,
    // alignment 1) and each `*_maybe_mds_buff_overwrite` label precedes a 5-byte
    // `CASE_ID_MDS_TAA_MITIGATION` patch site (alignment 1) in `.text`, all patched once during
    // `physboot` and mapped read-only thereafter.
    unsafe extern "C" {
        #[link_name = "__x86_indirect_thunk_r11"]
        static X86_INDIRECT_THUNK_R11: [u8; 16];
        #[link_name = "interrupt_non_nmi_maybe_mds_buff_overwrite"]
        static INTERRUPT_NON_NMI_MAYBE_MDS_BUFF_OVERWRITE: [u8; 5];
        #[link_name = "interrupt_nmi_maybe_mds_buff_overwrite"]
        static INTERRUPT_NMI_MAYBE_MDS_BUFF_OVERWRITE: [u8; 5];
        #[link_name = "syscall_maybe_mds_buff_overwrite"]
        static SYSCALL_MAYBE_MDS_BUFF_OVERWRITE: [u8; 5];
    }

    /// 5-byte recommended multi-byte `NOP` (`0f 1f 44 00 00`, [intel/vol3]: Table 4-12).
    const X86_NOP5: [u8; 5] = [0x0f, 0x1f, 0x44, 0x00, 0x00];

    /// Tests basic read and write MSR variants across CPUs.
    #[test]
    fn test_x64_msrs() {
        {
            let _irqd = InterruptDisableGuard::new();
            // Test read_msr for an MSR that is known to always exist on x64.
            // SAFETY: `X86_MSR_IA32_LSTAR` is an architectural x86-64 MSR accessed at CPL 0 with
            // interrupts disabled.
            let val = unsafe { read_msr(X86_MSR_IA32_LSTAR) };
            expect_ne!(val, 0u64);

            // Test write_msr to write that value back.
            // SAFETY: Writing back the current `X86_MSR_IA32_LSTAR` value at CPL 0 with interrupts
            // disabled preserves syscall entry state.
            unsafe { write_msr(X86_MSR_IA32_LSTAR, val) };
        }

        // Asserting `Err` from `read_msr_safe` on an unimplemented MSR (such as AMD `MSRC001_2xxx`)
        // is omitted because QEMU TCG returns `0` instead of raising `#GP`.

        // Test read_msr_on_cpu and read_msr_safe_on_cpu.
        // SAFETY: `X86_MSR_IA32_FMASK` is an architectural x86-64 MSR accessed at CPL 0.
        let initial_fmask = unsafe { read_msr(X86_MSR_IA32_FMASK) };
        for i in 0..max_num_cpus() {
            if !mp::is_cpu_online(i) {
                continue;
            }
            // SAFETY: CPU `i` is online and `X86_MSR_IA32_FMASK` is an architectural x86-64 MSR.
            let fmask = unsafe { read_msr_on_cpu(i, X86_MSR_IA32_FMASK) };
            expect_eq!(unwrap_ok!(fmask), initial_fmask);
            // SAFETY: `read_msr_safe_on_cpu` catches `#GP(0)` faults and has no side effects.
            let safe_fmask = unsafe { read_msr_safe_on_cpu(i, X86_MSR_IA32_FMASK) };
            expect_eq!(unwrap_ok!(safe_fmask), initial_fmask);
            // Verify reading an unimplemented MSR does not panic across either the local fast path
            // or remote worker-thread path (QEMU TCG returns `Ok(0)` while KVM/hardware catches
            // `#GP(0)` and returns `Err`).
            // SAFETY: `0xdeadbeef` is an unimplemented MSR; `read_msr_safe_on_cpu` catches
            // `#GP(0)`.
            let _ = unsafe { read_msr_safe_on_cpu(i, 0xdeadbeef) };
        }

        let invalid_cpu = max_num_cpus();
        // SAFETY: `invalid_cpu` is out of range and returns `Err(Status::BAD_STATE)` without
        // issuing `rdmsr`.
        let invalid_res = unsafe { read_msr_safe_on_cpu(invalid_cpu, X86_MSR_IA32_FMASK) };
        expect_err!(invalid_res, Status::BAD_STATE);

        // Test write_msr_on_cpu.
        for i in 0..max_num_cpus() {
            if !mp::is_cpu_online(i) {
                continue;
            }
            // SAFETY: CPU `i` is online and writing back `initial_fmask` to `X86_MSR_IA32_FMASK`
            // preserves state.
            let res = unsafe { write_msr_on_cpu(i, X86_MSR_IA32_FMASK, initial_fmask) };
            expect_ok!(res);
        }

        // If RDTSCP is supported, check that the TSC_AUX MSR is correctly programmed.
        if x86_feature_test(X86_FEATURE_RDTSCP) {
            for i in 0..max_num_cpus() {
                if !mp::is_cpu_online(i) {
                    continue;
                }
                // SAFETY: CPU `i` is online and `X86_MSR_IA32_TSC_AUX` is supported when `RDTSCP`
                // is present.
                let cpuid = unsafe { read_msr_on_cpu(i, X86_MSR_IA32_TSC_AUX) };
                expect_eq!(unwrap_ok!(cpuid), u64::from(i));

                // `Relaxed` is sufficient because `mp::sync_exec` provides the cross-CPU
                // synchronization barrier before and after the IPI callback.
                let aux = AtomicU32::new(0);
                let mask = cpu_num_to_mask(i);
                let read_aux = || {
                    let aux_msr: u32;
                    // SAFETY: `X86_FEATURE_RDTSCP` was verified above and the callback runs at
                    // CPL 0; `rdtscp` reads TSC and `IA32_TSC_AUX` into `%eax`/`%edx`/`%ecx`
                    // without side effects.
                    unsafe {
                        asm!(
                            "rdtscp",
                            out("eax") _,
                            out("edx") _,
                            out("ecx") aux_msr,
                            options(nostack, preserves_flags),
                        );
                    }
                    aux.store(aux_msr, Ordering::Relaxed);
                };
                // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled.
                unsafe {
                    mp::sync_exec(MpIpiTarget::Mask, mask, read_aux);
                }
                expect_eq!(unwrap_ok!(cpuid), u64::from(aux.load(Ordering::Relaxed)));
            }
        }
    }

    /// Tests kernel `cpu` console commands.
    #[test]
    fn test_x64_msrs_k_commands() {
        #[cfg(console_enabled)]
        {
            expect_eq!(ZX_OK, console_run_script_locked("cpu features"));
            expect_eq!(ZX_OK, console_run_script_locked("cpu rdmsr 0 0x10"));
            expect_eq!(
                Status::BAD_STATE.into_raw(),
                console_run_script_locked("cpu rdmsr 9999 0x10")
            );
            expect_eq!(
                Status::INVALID_ARGS.into_raw(),
                console_run_script_locked("cpu rdmsr 0 0x100000000")
            );
            // Verify that reading an unimplemented MSR via the debug console does not panic the
            // kernel with #GP(0) (on TCG this returns 0, while on hardware/KVM
            // `read_msr_safe_on_cpu` catches the #GP fault and returns an error status).
            let _ = console_run_script_locked("cpu rdmsr 0 0xdeadbeef");
        }
    }

    /// Tests kernel `hwp` console commands.
    #[test]
    fn test_x64_hwp_k_commands() {
        #[cfg(console_enabled)]
        {
            let boot_opts = BootOptions::get();
            // Don't test at all if HWP disabled on the command line.
            if boot_opts.x86_hwp {
                // If we don't support HWP, expect every command to just return "not supported".
                if !intel_hwp_supported(&RealCpuId) {
                    expect_eq!(Status::NOT_SUPPORTED.into_raw(), console_run_script_locked("hwp"));
                } else {
                    let err_invalid_args = Status::INVALID_ARGS.into_raw();

                    // Test top-level parsing.
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp"));
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp invalid"));
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp 3"));

                    // Set policy.
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-policy"));
                    expect_eq!(
                        err_invalid_args,
                        console_run_script_locked("hwp set-policy invalid-policy")
                    );
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-policy 3"));
                    expect_eq!(
                        err_invalid_args,
                        console_run_script_locked("hwp set-policy performance 42")
                    );
                    expect_eq!(ZX_OK, console_run_script_locked("hwp set-policy performance"));
                    expect_eq!(ZX_OK, console_run_script_locked("hwp set-policy power-save"));

                    // Set Freq.
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-freq"));
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-freq 0"));
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-freq 256"));
                    expect_eq!(err_invalid_args, console_run_script_locked("hwp set-freq 10 10"));
                    expect_eq!(ZX_OK, console_run_script_locked("hwp set-freq 100"));
                    expect_eq!(ZX_OK, console_run_script_locked("hwp set-freq 255"));

                    // Restore the policy to default across all CPUs.
                    let policy = boot_opts.x86_hwp_policy;
                    // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled.
                    unsafe {
                        mp::sync_exec(MpIpiTarget::All, 0, || {
                            let mut msr = RealMsrAccess;
                            intel_hwp_init(&RealCpuId, &mut msr, policy);
                        });
                    }
                }
            }
        }
    }

    /// Tests Spectre v2 mitigation building blocks.
    #[test]
    fn test_spectre_v2_mitigations() {
        let sp_match: u8;

        // Execute x86_ras_fill and make sure %rsp is unchanged.
        // SAFETY: `call x86_ras_fill` uses 272 bytes of temporary kernel stack space (8-byte
        // return address plus 264 bytes inside `x86_ras_fill`), clobbers `RFLAGS` (accounted for
        // by omitting `options(nostack, preserves_flags)`), overwrites the RSB, and restores
        // `%rsp` and `%rcx` before returning.
        unsafe {
            asm!(
                "mov r11, rsp",
                "call x86_ras_fill",
                "cmp rsp, r11",
                "setz {sp_match}",
                sp_match = out(reg_byte) sp_match,
                out("r11") _,
            );
        }
        expect_eq!(sp_match != 0, true);

        // Test that retpoline thunks are correctly patched.
        // SAFETY: `__x86_indirect_thunk_r11` is an 18-byte hermetic code-patching stub in `.text`
        // that is patched during physboot and read-only thereafter.
        let check_buffer = unsafe { ptr::read(&raw const X86_INDIRECT_THUNK_R11) };

        if BootOptions::get().x86_disable_spec_mitigations {
            // If speculative execution mitigations are disabled or Enhanced IBRS is enabled, we
            // expect the retpoline thunk to be:
            // __x86_indirect_thunk:
            //   41 ff e3        jmp *%r11
            expect_eq!(check_buffer[0], 0x41u8);
            expect_eq!(check_buffer[1], 0xffu8);
            expect_eq!(check_buffer[2], 0xe3u8);
        } else {
            // We expect the generic thunk to be:
            // __x86_indirect_thunk:
            //  e8 ?? ?? ?? ?? call ...
            //
            // We cannot test the exact contents of the thunk as the call target depends on the
            // internal alignment. Instead check that the first byte is the call instruction we
            // expect.
            expect_eq!(check_buffer[0], 0xe8u8);
        }
    }

    /// Tests MDS mitigation building blocks.
    #[test]
    fn test_mds_taa_mitigation() {
        for src in [
            &raw const INTERRUPT_NON_NMI_MAYBE_MDS_BUFF_OVERWRITE,
            &raw const INTERRUPT_NMI_MAYBE_MDS_BUFF_OVERWRITE,
            &raw const SYSCALL_MAYBE_MDS_BUFF_OVERWRITE,
        ] {
            // SAFETY: Each `*_maybe_mds_buff_overwrite` label marks a 5-byte code-patched site in
            // `.text` (`call mds_buff_overwrite` or `NOP5`) patched during physboot and read-only
            // thereafter.
            let check_buffer = unsafe { ptr::read(src) };
            if x86_cpu_should_md_clear_on_user_return() {
                expect_eq!(check_buffer[0], 0xe8u8); // Expect a call to mds_buff_overwrite
            } else {
                // If speculative execution mitigations are disabled or we're not affected by MDS or
                // don't have MD_CLEAR, expect NOPs.
                for (actual, expected) in check_buffer.into_iter().zip(X86_NOP5) {
                    expect_eq!(actual, expected);
                }
            }
        }
    }

    /// Tests that the GDT is mapped and readable across all pages.
    #[test]
    fn test_gdt_mapping() {
        let mut gdtr = Gdtr::default();
        // SAFETY: `sgdt` writes the 10-byte GDTR descriptor into `gdtr` without side effects or
        // flag modifications.
        unsafe {
            asm!(
                "sgdt [{gdtr}]",
                gdtr = in(reg) &raw mut gdtr,
                options(nostack, preserves_flags),
            );
        }
        let limit = gdtr.limit;
        let base = gdtr.address;
        assert_eq!(limit, 0xffffu16);

        // Force a read of each page of the GDT.
        let num_entries = (usize::from(limit) + 1) / size_of::<u64>();
        let step = page::SIZE / size_of::<u64>();
        for i in (0..num_entries).step_by(step) {
            let entry_addr = base + i * size_of::<u64>();
            let _output: u64;
            // SAFETY: `gdt_setup` maps the entire `0x10000`-byte (`limit + 1`) GDT region at
            // `base` as readable kernel memory, so `entry_addr` is a mapped, 8-byte aligned
            // `u64` slot within the GDT.
            unsafe {
                asm!(
                    "mov {output}, [{entry}]",
                    output = out(reg) _output,
                    entry = in(reg) entry_addr,
                    options(nostack, preserves_flags, readonly),
                );
            }
        }
    }

    /// Tests NMI delivery across all active CPUs.
    #[test]
    fn test_nmi_spam() {
        // TODO(https://fxbug.dev/42169592): Unconditionally re-enable this test when
        // https://fxbug.dev/42169592 has been resolved.
        //
        // Spam all the CPUs with NMIs, see that nothing bad happens.
        if x86_get_hypervisor() == X86HypervisorList::Kvm {
            kprintln!("skipping test, see https://fxbug.dev/42169592");
        } else {
            const DURATION: DurationMono = DurationMono::from_millis(100);
            let deadline = Deadline::after_mono(DURATION, TimerSlack::none());
            while {
                apic_send_mask_ipi(
                    X86_INT_NMI.0,
                    peek_active_mask(),
                    ApicInterruptDeliveryMode::NMI,
                );
                current_mono_time().0 < deadline.when().0
            } {}
        }
    }

    /// Tests that the TSS I/O port bitmap is initialized to restrict access on all CPUs.
    #[test]
    fn test_x64_tss_io_bitmap() {
        // Inspect each online CPU's `default_tss.tss_bitmap` locally on that CPU with interrupts
        // disabled so no context switch, `IoBitmap` IPI, or hotplug initialization can
        // concurrently mutate `tss_bitmap` while it is read.
        let bsp_bitmap_ok = AtomicBool::new(true);
        let ap_bitmap_ok = AtomicBool::new(true);
        let guard_byte_ok = AtomicBool::new(true);

        let check_local_tss_bitmap = || {
            // SAFETY: `mp::sync_exec` executes on each online CPU at CPL 0 with interrupts and
            // preemption disabled (`%gs.base` initialized), satisfying `SyncPercpu`'s owning-CPU
            // invariant and excluding concurrent `with_curr_tss_bitmap_mut` writes.
            let bitmap_ptr = unsafe {
                let percpu = get_percpu();
                (&raw const (*percpu).default_tss.tss_bitmap).cast::<u8>()
            };
            let is_bsp = curr_cpu_num() == 0;
            for i in 0..IO_BITMAP_BYTES {
                // SAFETY: `i < IO_BITMAP_BYTES` is within `tss_bitmap`
                // (`[u8; IO_BITMAP_BYTES + 1]`), and interrupts are disabled on the owning CPU.
                let byte = unsafe { *bitmap_ptr.add(i) };
                if byte != 0xFF {
                    // `Relaxed` is sufficient because `mp::sync_exec` provides the cross-CPU
                    // synchronization barrier before and after the IPI callback.
                    if is_bsp {
                        bsp_bitmap_ok.store(false, Ordering::Relaxed);
                    } else {
                        ap_bitmap_ok.store(false, Ordering::Relaxed);
                    }
                }
            }
            // SAFETY: `IO_BITMAP_BYTES` is the valid trailing guard-byte index in `tss_bitmap`
            // (`[u8; IO_BITMAP_BYTES + 1]`), and interrupts are disabled on the owning CPU.
            let guard_byte = unsafe { *bitmap_ptr.add(IO_BITMAP_BYTES) };
            if guard_byte != 0xFF {
                guard_byte_ok.store(false, Ordering::Relaxed);
            }
        };

        // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled by the caller.
        unsafe {
            mp::sync_exec(MpIpiTarget::All, 0, check_local_tss_bitmap);
        }

        assert_true!(
            bsp_bitmap_ok.load(Ordering::Relaxed),
            "TSS bitmap byte was not initialized correctly on BSP"
        );
        assert_true!(ap_bitmap_ok.load(Ordering::Relaxed), "TSS bitmap byte not set on AP");
        assert_true!(guard_byte_ok.load(Ordering::Relaxed), "TSS guard byte missing.");
    }

    /// Tests `convert_raw_tsc_timestamp_to_clock_monotonic` for current and future TSC values.
    #[test]
    fn test_convert_raw_tsc_timestamp_to_clock_monotonic() {
        let before_mono = current_mono_time().0;
        let raw_tsc = libarch::cycles() as i64;
        let converted_mono = convert_raw_tsc_timestamp_to_clock_monotonic(raw_tsc);
        let after_mono = current_mono_time().0;

        // Allow a small tolerance for non-TSC wallclocks (such as HPET or PIT) where
        // `now_tsc` inside `convert_raw_tsc_timestamp_to_clock_monotonic` is sampled
        // slightly after `raw_tsc`.
        expect_ge!(converted_mono, before_mono.saturating_sub(zx_msec(10)));
        expect_le!(converted_mono, after_mono.saturating_add(zx_msec(10)));

        const FUTURE_TSC_DELTA: i64 = 1_000_000_000;
        let future_duration = convert_raw_tsc_duration_to_nanoseconds(FUTURE_TSC_DELTA);
        if future_duration > 0 {
            let future_before = current_mono_time().0;
            let future_tsc = (libarch::cycles() as i64).wrapping_add(FUTURE_TSC_DELTA);
            let future_converted = convert_raw_tsc_timestamp_to_clock_monotonic(future_tsc);
            let future_after = current_mono_time().0;

            expect_ge!(
                future_converted,
                future_before.saturating_add(future_duration - zx_msec(10))
            );
            expect_le!(
                future_converted,
                future_after.saturating_add(future_duration + zx_msec(10))
            );
        }
    }
}
