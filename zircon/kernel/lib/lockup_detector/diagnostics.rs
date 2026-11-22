// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Diagnostics dumped when the lockup detector notices a CPU in trouble.

#[cfg(target_arch = "aarch64")]
use core::mem::MaybeUninit;

#[cfg(target_arch = "x86_64")]
use boot_options::BootOptions;
use libc::stdio::{FILE, fprintf};
use zx_status::Status;

#[cfg(target_arch = "x86_64")]
use crate::arch_rs::InterruptDisableGuard;
#[cfg(target_arch = "aarch64")]
use crate::arch_rs::arm64::dap::{
    Arm64DapProcessorState, arm64_dap_is_enabled, arm64_dap_read_processor_state,
};
#[cfg(target_arch = "aarch64")]
use crate::arch_rs::arm64::{arm64_mmu_translate, vm::is_kernel_address};
use crate::arch_rs::ints_disabled as arch_ints_disabled;
#[cfg(target_arch = "x86_64")]
use crate::arch_rs::x86::arch::print_frame_to;
#[cfg(target_arch = "x86_64")]
use crate::backtrace::GLOBAL_CPU_CONTEXT_EXCHANGE;
#[cfg(target_arch = "aarch64")]
use crate::backtrace::{Backtrace, FrameType};
use crate::kernel::percpu::PerCpu;
#[cfg(target_arch = "x86_64")]
use crate::kernel::scheduler;
use crate::kernel::stats::CpuStats;
use crate::kernel::types::cpu_num_t;
#[cfg(target_arch = "x86_64")]
use crate::kernel::types::{INVALID_CPU, cpu_mask_t, cpu_num_to_mask, remove_cpu_from_mask};
#[cfg(target_arch = "aarch64")]
use crate::kernel::types::{PAddr, VAddr};
#[cfg(target_arch = "x86_64")]
use crate::platform_rs::timer::zx_msec;

/// How bad the condition being reported is: an [`Oops`](Self::Oops) is logged, while a
/// [`Fatal`](Self::Fatal) condition is written to the crashlog before the system reboots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailureSeverity {
    Oops,
    Fatal,
}

/// Returns true if the system has the ability to dump the state of another CPU.
///
/// Intended to be used in conjunction with [`dump_registers_and_backtrace`].
///
/// If this function returns false, [`dump_registers_and_backtrace`] should not be called.  Of
/// course, even if this function returns true, [`dump_registers_and_backtrace`] may still fail.
///
/// This is [`arm64_dap_is_enabled`].
#[cfg(target_arch = "aarch64")]
pub fn can_dump_registers_and_backtrace() -> bool {
    arm64_dap_is_enabled()
}

/// Attempt to dump the registers and backtrace of the target `cpu`.
///
/// This is a destructive operation and may leave the target CPU in an unknown state.  It should
/// only be called as part of a panic.
///
/// Example:
///
/// ```ignore
/// if can_dump_registers_and_backtrace() {
///     // Initiate a panic sequence to ensure that subsequent printfs are sent to serial,
///     // however, don't try to halt any CPUs because we want to query the target's state.
///     platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
///     kprintln!("cpu-{} is unresponsive, attempting to dump its state", target_cpu);
///
///     // Attempt to dump state.
///     if let Err(status) = unsafe { dump_registers_and_backtrace(target_cpu, stdout) } {
///         kprintln!("failed to dump state for cpu-{}, status {}", target_cpu, status);
///     }
///     platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
/// }
/// ```
///
/// Errors (non-exhaustive list):
///
///   `Status::NOT_SUPPORTED` - there is no debug facility present to support this.
///   `Status::TIMED_OUT` - timed out trying to get CPU context.
///
/// Unless [`can_dump_registers_and_backtrace`] is true, this returns `Status::NOT_SUPPORTED`
/// without touching `cpu`.
///
/// # Safety
///
/// - `cpu` must stay powered up for the duration of the call, not offline or in a power-down
///   state.
/// - Unless interrupts are disabled on the calling CPU, its current `Thread` must be initialized.
/// - This must only be called during a terminal kernel panic path where permanently halting `cpu`
///   into hardware debug state and clobbering its `x0` register cannot violate safety invariants
///   on `cpu`.
/// - `output_target` must point to a valid `FILE` for the duration of the call.
///
/// The read needs only the power and `Thread` bullets. The terminal-panic bullet stays because
/// the backtrace below reads memory through the `x18` the read returns, and its SAFETY relies on
/// that being read from a halted `cpu` during a terminal panic.
///
/// # Correctness
///
/// As for [`arm64_dap_read_processor_state`]: `cpu` must be less than `max_num_cpus()` and must
/// not be the calling CPU, and the caller must not be able to migrate to it.
#[cfg(target_arch = "aarch64")]
pub unsafe fn dump_registers_and_backtrace(
    cpu: cpu_num_t,
    output_target: *mut FILE,
) -> Result<(), Status> {
    if !can_dump_registers_and_backtrace() {
        return Err(Status::NOT_SUPPORTED);
    }

    // TODO(https://fxbug.dev/42175838): Update the DAP to make use of
    // lockup_detector_diagnostic_query_timeout_ms.
    let mut state = MaybeUninit::<Arm64DapProcessorState>::uninit();
    // SAFETY: The caller keeps `cpu` powered up for the call, and gives this CPU a current
    // `Thread` unless interrupts are disabled.
    let result = unsafe { arm64_dap_read_processor_state(cpu, &mut state) };

    if let Err(err) = result {
        // SAFETY: The caller guarantees `output_target` is a valid `FILE`, and the format string's
        // `%d` matches the `zx_status_t` argument.
        unsafe {
            fprintf(output_target, c"Failed to read DAP state (res %d)\n".as_ptr(), err.into_raw())
        };
        return Err(err);
    }
    // SAFETY: `arm64_dap_read_processor_state` returned `Ok`, so it filled in all of `state`.
    let state = unsafe { state.assume_init_ref() };

    // SAFETY: The caller guarantees `output_target` is a valid `FILE`.
    unsafe {
        fprintf(output_target, c"DAP state:\n".as_ptr());
        state.dump(output_target);
        fprintf(output_target, c"\n".as_ptr());
    }

    if cfg!(sanitize = "shadow-call-stack") {
        let mut bt = Backtrace::new();
        // SAFETY: `state` was captured from halted `cpu` during a terminal panic, so if `cpu` was
        // in EL1 with a mapped kernel shadow-call-stack page, that page is valid readable kernel
        // memory disjoint from `bt` on this CPU's stack.
        let status = unsafe { get_backtrace_from_dap_state(state, &mut bt) };
        // SAFETY: The caller guarantees `output_target` is a valid `FILE`, and each format
        // string's conversions match its arguments.
        unsafe {
            match status {
                Err(Status::BAD_STATE) => {
                    fprintf(
                        output_target,
                        c"DAP backtrace: CPU-%u not in kernel mode.\n".as_ptr(),
                        cpu,
                    );
                }
                Err(Status::INVALID_ARGS) => {
                    fprintf(output_target, c"DAP backtrace: invalid SCSP.\n".as_ptr());
                }
                Err(Status::OUT_OF_RANGE) => {
                    fprintf(output_target, c"DAP backtrace: not a kernel address.\n".as_ptr());
                }
                Err(Status::NOT_FOUND) => {
                    fprintf(output_target, c"DAP backtrace: not mapped.\n".as_ptr());
                }
                _ => {
                    fprintf(
                        output_target,
                        c"DAP backtrace: %d\n".as_ptr(),
                        Status::result_into_raw(status),
                    );
                }
            }
            if bt.size() > 0 {
                bt.print_without_version(output_target);
            }
        }
        status
    } else {
        Ok(())
    }
}

/// A value <= 0 means the diagnostic query is disabled.
#[cfg(target_arch = "x86_64")]
fn get_timeout() -> i64 {
    zx_msec(BootOptions::get().lockup_detector_diagnostic_query_timeout_ms as i64)
}

/// Returns true if the system has the ability to dump the state of another CPU.
///
/// Intended to be used in conjunction with [`dump_registers_and_backtrace`].
///
/// If this function returns false, [`dump_registers_and_backtrace`] should not be called.  Of
/// course, even if this function returns true, [`dump_registers_and_backtrace`] may still fail.
#[cfg(target_arch = "x86_64")]
pub fn can_dump_registers_and_backtrace() -> bool {
    get_timeout() > 0
}

/// Attempt to dump the registers and backtrace of the target `cpu`.
///
/// This is a destructive operation and may leave the target CPU in an unknown state.  It should
/// only be called as part of a panic.
///
/// Example:
///
/// ```ignore
/// if can_dump_registers_and_backtrace() {
///     // Initiate a panic sequence to ensure that subsequent printfs are sent to serial,
///     // however, don't try to halt any CPUs because we want to query the target's state.
///     platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
///     kprintln!("cpu-{} is unresponsive, attempting to dump its state", target_cpu);
///
///     // Attempt to dump state.
///     if let Err(status) = unsafe { dump_registers_and_backtrace(target_cpu, stdout) } {
///         kprintln!("failed to dump state for cpu-{}, status {}", target_cpu, status);
///     }
///     platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
/// }
/// ```
///
/// Errors (non-exhaustive list):
///
///   `Status::NOT_SUPPORTED` - there is no debug facility present to support this.
///   `Status::TIMED_OUT` - timed out trying to get CPU context.
///
/// # Safety
///
/// - `(cpu as usize) < crate::kernel::mp::SMP_MAX_CPUS`.
/// - `cpu` must not be the calling CPU, and the caller must not be able to migrate to `cpu` for
///   the duration of the call.
/// - The caller must be running at CPL 0 on a terminal kernel panic path.
/// - `output_target` must point to a valid `FILE` for the duration of the call.
///
/// # Correctness
///
/// `cpu` must be a valid CPU number less than `max_num_cpus()`.
#[cfg(target_arch = "x86_64")]
pub unsafe fn dump_registers_and_backtrace(
    cpu: cpu_num_t,
    output_target: *mut FILE,
) -> Result<(), Status> {
    let timeout = get_timeout();
    if timeout == 0 {
        // SAFETY: The caller guarantees `output_target` is a valid `FILE`.
        unsafe { fprintf(output_target, c"diagnostic query disabled (timeout is 0)\n".as_ptr()) };
        return Err(Status::NOT_SUPPORTED);
    }

    // Make sure interrupts are disabled before calling `request_context`.
    let _irqd = InterruptDisableGuard::new();

    // First, dump the context for the unresponsive CPU.  Then, dump the contexts of the other CPUs.
    let mut target_cpu = cpu;
    let mut remaining_cpus: cpu_mask_t =
        scheduler::peek_active_mask() & !cpu_num_to_mask(target_cpu);
    let mut result = Ok(());
    loop {
        // SAFETY: Interrupts are disabled by `_irqd`, and `target_cpu` is either the caller's
        // valid `cpu` or was taken from the active CPU mask.
        let status = unsafe { GLOBAL_CPU_CONTEXT_EXCHANGE.request_context(target_cpu, timeout) };
        match status {
            Err(err) => {
                // SAFETY: The caller guarantees `output_target` is a valid `FILE`, and the format
                // string's `%u` and `%d` match the `cpu_num_t` and `zx_status_t` arguments.
                unsafe {
                    fprintf(
                        output_target,
                        c"failed to get context of CPU-%u: %d\n".as_ptr(),
                        target_cpu,
                        err.into_raw(),
                    )
                };
                if result.is_ok() {
                    result = Err(err);
                }
            }
            Ok(context) => {
                // SAFETY: The caller guarantees CPL 0 and that `output_target` is a valid `FILE`,
                // and the format strings' `%u` matches the `cpu_num_t` argument.
                unsafe {
                    fprintf(output_target, c"CPU-%u context follows\n".as_ptr(), target_cpu);
                    context.backtrace.print_without_version(output_target);
                    print_frame_to(&context.frame, output_target);
                    fprintf(output_target, c"end of CPU-%u context\n".as_ptr(), target_cpu);
                }
            }
        }
        target_cpu = remove_cpu_from_mask(&mut remaining_cpus);
        if target_cpu == INVALID_CPU {
            break;
        }
    }

    result
}

/// Returns true if the system has the ability to dump the state of another CPU.
///
/// Intended to be used in conjunction with [`dump_registers_and_backtrace`].
///
/// If this function returns false, [`dump_registers_and_backtrace`] should not be called.  Of
/// course, even if this function returns true, [`dump_registers_and_backtrace`] may still fail.
#[cfg(target_arch = "riscv64")]
pub fn can_dump_registers_and_backtrace() -> bool {
    false
}

/// Attempt to dump the registers and backtrace of the target `cpu`.
///
/// This is a destructive operation and may leave the target CPU in an unknown state.  It should
/// only be called as part of a panic.
///
/// Example:
///
/// ```ignore
/// if can_dump_registers_and_backtrace() {
///     // Initiate a panic sequence to ensure that subsequent printfs are sent to serial,
///     // however, don't try to halt any CPUs because we want to query the target's state.
///     platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
///     kprintln!("cpu-{} is unresponsive, attempting to dump its state", target_cpu);
///
///     // Attempt to dump state.
///     if let Err(status) = unsafe { dump_registers_and_backtrace(target_cpu, stdout) } {
///         kprintln!("failed to dump state for cpu-{}, status {}", target_cpu, status);
///     }
///     platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
/// }
/// ```
///
/// Errors (non-exhaustive list):
///
///   `Status::NOT_SUPPORTED` - there is no debug facility present to support this.
///   `Status::TIMED_OUT` - timed out trying to get CPU context.
///
/// # Safety
///
/// - `cpu` must not be the calling CPU, and the caller must not be able to migrate to `cpu` for
///   the duration of the call.
/// - This must only be called during a terminal kernel panic path.
/// - `output_target` must point to a valid `FILE` for the duration of the call.
///
/// # Correctness
///
/// `cpu` must be a valid CPU number less than `max_num_cpus()`.
#[cfg(target_arch = "riscv64")]
pub unsafe fn dump_registers_and_backtrace(
    _cpu: cpu_num_t,
    _output_target: *mut FILE,
) -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64", target_arch = "riscv64")))]
compile_error!("Unknown architecture!");

/// # Safety
///
/// Interrupts must be disabled on the calling CPU, `(cpu as usize) < PerCpu::processor_count()`,
/// `output_target` must point to a valid `FILE` for the duration of the call, `cpu`'s scheduler
/// `queue_lock_` must not already be held on the calling CPU, and no `ChainLockTransaction` may
/// be active on the calling CPU. When `severity == FailureSeverity::Fatal`:
/// - `cpu` must be distinct from the calling CPU, and the system must be on a terminal kernel
///   panic path.
/// - On `aarch64`, `cpu` must stay powered up for the duration of the call.
/// - On `x86_64`, `(cpu as usize) < crate::kernel::mp::SMP_MAX_CPUS` and the caller must be
///   running at CPL 0.
///
/// # Correctness
///
/// For a fatal report, as for [`dump_registers_and_backtrace`]: `cpu` must be less than
/// `crate::arch_rs::max_num_cpus()`.
pub(super) unsafe fn dump_common_diagnostics(
    cpu: cpu_num_t,
    output_target: *mut FILE,
    severity: FailureSeverity,
) {
    debug_assert!(arch_ints_disabled());

    // SAFETY: The caller guarantees `(cpu as usize) < PerCpu::processor_count()`.
    let percpu = unsafe { PerCpu::get(cpu) };
    // SAFETY: `percpu.stats` is an initialized `CpuStats` whose counters are only ever updated
    // with atomic operations.
    let stats = unsafe { CpuStats::read_atomic_fields_relaxed(percpu.stats.get()) };
    // SAFETY: The caller guarantees `output_target` is a valid `FILE`, and the format string's
    // `%lu` conversions match the `u64` arguments.
    unsafe {
        fprintf(
            output_target,
            c"timer_ints: %lu, interrupts: %lu\n".as_ptr(),
            stats.timer_ints,
            stats.interrupts,
        )
    };

    // SAFETY: The caller guarantees interrupts are disabled, `output_target` is a valid `FILE`,
    // `cpu`'s scheduler `queue_lock_` is not held on the calling CPU, and no
    // `ChainLockTransaction` is active on the calling CPU.
    unsafe {
        percpu.scheduler.dump_to(output_target, false);
        percpu.scheduler.dump_active_thread_to(output_target);
    }

    if severity == FailureSeverity::Fatal {
        // SAFETY: The caller guarantees `output_target` is a valid `FILE`.
        unsafe { fprintf(output_target, c"\n".as_ptr()) };
        // SAFETY: Interrupts are disabled, so this CPU cannot migrate and, on `aarch64`, needs no
        // current `Thread`, and `output_target` is a valid `FILE`. For `FailureSeverity::Fatal`
        // the caller guarantees the architecture's preconditions (`cpu != curr_cpu_num()`,
        // terminal panic path, `cpu` powered up on `aarch64`, and `(cpu as usize) < SMP_MAX_CPUS`
        // at CPL 0 on `x86_64`).
        let status = unsafe { dump_registers_and_backtrace(cpu, output_target) };
        if let Err(err) = status {
            // SAFETY: The caller guarantees `output_target` is a valid `FILE`, and the format
            // string's `%d` matches the `zx_status_t` argument.
            unsafe {
                fprintf(
                    output_target,
                    c"failed to dump registers and backtrace: %d\n".as_ptr(),
                    err.into_raw(),
                )
            };
        }
    }
}

/// Using the supplied DAP state, obtain a backtrace, taking care to not fault.
///
/// This function is exposed to facilitate testing.  See [`dump_registers_and_backtrace`] for the
/// platform and architecture independent interface.
///
/// Resets `out_bt` and then fills it in as much as possible.  The backtrace may be truncated if
/// the SCS crosses a page boundary.  The contents of `out_bt` are valid even on error.
///
/// Errors:
///   `Status::BAD_STATE` - if the CPU is not in kernel mode.
///   `Status::INVALID_ARGS` - if the SCSP pointer is null or unaligned.
///   `Status::OUT_OF_RANGE` - if the stack is outside kernel address space.
///   `Status::NOT_FOUND` - if the stack is not mapped.
///
/// # Safety
///
/// If `state` encodes EL1 (`state.get_el_level() == 1`) and `state.r[18]` (`scsp`) is a non-null,
/// pointer-aligned kernel virtual address whose preceding slot (`scsp - size_of::<usize>()`)
/// translates as a readable EL1 kernel page, every pointer-sized slot from the start of that page
/// through `scsp - size_of::<usize>()` must be initialized, non-MMIO kernel memory valid for
/// volatile `usize` reads and disjoint from `out_bt` for the duration of the call.
#[cfg(target_arch = "aarch64")]
pub(super) unsafe fn get_backtrace_from_dap_state(
    state: &Arm64DapProcessorState,
    out_bt: &mut Backtrace,
) -> Result<(), Status> {
    // Don't attempt to do any backtracing unless this looks like the thread is in the kernel right
    // now.  The PC might be completely bogus, but even if it is in a legit user mode process, I'm
    // not sure of a good way to print the symbolizer context for that process, or to figure out if
    // the process is using a shadow call stack or not.
    if state.get_el_level() != 1 {
        return Err(Status::BAD_STATE);
    }

    // Build a backtrace using the PC as frame 0's address and the LR as frame 1's address.
    out_bt.reset();
    out_bt.push_back(state.pc as usize);
    out_bt.set_first_frame_type(FrameType::PreciseLocation);
    out_bt.push_back(state.r[30] as usize);

    // Is the Shadow Call Stack Pointer (x18) properly aligned?
    const PTR_SIZE: usize = size_of::<*const ()>();
    let mut scsp = state.r[18] as usize;
    if scsp & (PTR_SIZE - 1) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // SCSP has post-increment semantics so back up one slot so that it points to a stored value.
    if scsp == 0 {
        return Err(Status::INVALID_ARGS);
    }
    scsp -= PTR_SIZE;

    // Is the address in the kernel's address space?
    if !is_kernel_address(scsp) {
        return Err(Status::OUT_OF_RANGE);
    }

    // And is it mapped?
    let mut pa_unused = PAddr(0);
    arm64_mmu_translate(
        VAddr(scsp),
        &mut pa_unused,
        /* user */ false,
        /* write */ false,
    )?;

    // The SCSP looks legit.  Copy the return address values, but don't cross a page boundary.
    while out_bt.size() < Backtrace::MAX_SIZE {
        // SAFETY: `scsp` is pointer-aligned and on the page just verified as mapped for EL1 reads
        // (the loop below stops before crossing a page boundary), and the caller guarantees this
        // page holds initialized, non-MMIO kernel memory disjoint from `out_bt`. A volatile read
        // forms no reference to stack memory that the dumped CPU may still own.
        let ret_addr = unsafe { core::ptr::with_exposed_provenance::<usize>(scsp).read_volatile() };
        out_bt.push_back(ret_addr);

        // Are we about to cross a page boundary?
        zr::static_assert!(page::SIZE.is_power_of_two(), "page::SIZE is not a power of 2!  Wut??");
        if scsp & (page::SIZE - 1) == 0 {
            break;
        }
        scsp -= PTR_SIZE;
    }

    Ok(())
}
