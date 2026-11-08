// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012 Google, Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
use crate::jtrace::jtrace_invalidate;
use crate::kernel::types::cpu_num_t;
use crate::pdev_power::PowerCpuState;
use crate::pdev_watchdog::{hw_watchdog_pet, hw_watchdog_set_enabled};
use crate::persistent_debuglog_rs::persistent_dlog_invalidate;
use crate::platform_rs::crashlog::PlatformCrashlog;
use boot_options::BootOptions;
#[cfg(console_enabled)]
use core::ffi::{CStr, c_int};
use core::slice;
#[cfg(console_enabled)]
use debug::dprintf;
use zx_status::Status;
#[cfg(target_arch = "aarch64")]
use zx_types::zx_status_t;

/// Action to perform when halting the platform.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformHaltAction {
    /// Spin forever.
    Halt = 0,

    /// Reset the CPU.
    Reboot = 1,

    /// Reboot into the bootloader.
    RebootBootloader = 2,

    /// Reboot into the recovery partition.
    RebootRecovery = 3,

    /// Shutdown and power off.
    Shutdown = 4,
}

/// Software reason for a recent crash or reboot.
///
/// TODO(https://fxbug.dev/534467534): For now, these values are copied from
/// `zircon/system/public/zircon/boot/crash-reason.h`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZirconCrashReason {
    /// 0 is reserved for "Invalid".  It will never be used by a functioning
    /// crash-logger.
    Invalid = 0,

    /// "Unknown" indicates that the system does not know the reason for a recent
    /// crash.  The primary use of this reason is to be something which can be left
    /// in the crashlog in case the system spontaneously reboots without a chance to
    /// gracefully finalize the log, perhaps because of something like a hardware
    /// watchdog timer.
    Unknown = 1,

    /// "No Crash" indicates that the system deliberately rebooted in an
    /// orderly fashion.  No crash occurred.
    NoCrash = 2,

    /// "OOM" indicates a crash triggered by the system because of an unrecoverable
    /// out-of-memory situation.
    Oom = 3,

    /// "Panic" indicates a crash triggered by the system because of an unrecoverable
    /// kernel panic situation.
    Panic = 4,

    /// "Software watchdog" indicates a crash triggered by a kernel level software
    /// watchdog construct.  Note that this is distinct from a hardware based WDT.
    /// If the system reboots because of a hardware watchdog, it will have no chance
    /// to record the reboot reason, and the crashlog will indicate "unknown".  The
    /// HW reboot reason may be known, but only if the bootloader reports it to us.
    SoftwareWatchdog = 5,

    /// "Userspace root job termination" indicates a crash triggered by the system
    /// because it detected the termination of the userspace root job, most likely
    /// because one of its critical processes crashed.
    UserspaceRootJobTermination = 6,
}

impl TryFrom<u32> for ZirconCrashReason {
    type Error = u32;

    /// Converts a raw `zircon_crash_reason_t` value, returning it as the error if it does not name
    /// a known reason.
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        match raw {
            x if x == Self::Invalid as u32 => Ok(Self::Invalid),
            x if x == Self::Unknown as u32 => Ok(Self::Unknown),
            x if x == Self::NoCrash as u32 => Ok(Self::NoCrash),
            x if x == Self::Oom as u32 => Ok(Self::Oom),
            x if x == Self::Panic as u32 => Ok(Self::Panic),
            x if x == Self::SoftwareWatchdog as u32 => Ok(Self::SoftwareWatchdog),
            x if x == Self::UserspaceRootJobTermination as u32 => {
                Ok(Self::UserspaceRootJobTermination)
            }
            _ => Err(raw),
        }
    }
}

/// Specifies whether a CPU suspend operation is limited to the calling CPU or may power down the
/// encapsulating power domain.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformAllowDomainPowerDown {
    /// Only suspend the calling CPU.
    No = 0,
    /// Allow powering down the encapsulating power domain as well.
    Yes = 1,
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PlatformCpuStateResult {
    status: zx_status_t,
    state: u32,
}

#[cfg(target_arch = "aarch64")]
zr::static_assert_size_and_align!(PlatformCpuStateResult, 8, 4);

/// Whether [`platform_panic_start_with_option`] should halt other CPUs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicStartHaltOtherCpus {
    No,
    Yes,
}

unsafe extern "C" {
    fn cpp_platform_specific_halt(
        suggested_action: PlatformHaltAction,
        reason: ZirconCrashReason,
        halt_on_panic: bool,
    ) -> !;
    fn cpp_platform_panic_start(halt_other_cpus: bool);
}

pub use super::debug::{platform_serial_is_suspended, platform_serial_suspended_at};

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    /// Returns `true` iff this platform supports `platform_suspend_cpu`.
    fn cpp_platform_supports_suspend_cpu() -> bool;
    /// Suspends the calling CPU.
    fn cpp_platform_suspend_cpu(allow_domain: bool) -> zx_status_t;
    /// Returns `true` iff this platform supports `platform_suspend_system`.
    fn cpp_platform_supports_suspend_system() -> bool;
    /// Suspends the entire system from the boot CPU after secondary CPUs have powered off.
    fn cpp_platform_suspend_system() -> zx_status_t;
    /// Queries the current hardware power state of `cpu_id`.
    fn cpp_platform_get_cpu_state(cpu_id: cpu_num_t) -> PlatformCpuStateResult;
}

/// Returns `true` iff this platform supports [`platform_suspend_cpu`].
#[inline]
pub fn platform_supports_suspend_cpu() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Pure platform capability query with no preconditions.
        unsafe { cpp_platform_supports_suspend_cpu() }
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::power::platform_supports_suspend_cpu()
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::rust_platform_supports_suspend_cpu()
    }
}

/// Suspends the calling CPU.
///
/// On success, the CPU will enter an implementation-defined suspend state.
///
/// If `allow_domain` is [`PlatformAllowDomainPowerDown::No`], then the operation will only target
/// the calling CPU. However, if [`PlatformAllowDomainPowerDown::Yes`] is specified, then, depending
/// on the implementation, the power domain encapsulating the calling CPU may also be suspended and
/// powered down.
///
/// Prior to calling this function, it's critical to set up some kind of interrupt that will wake
/// the CPU and resume execution. Upon successful suspend and resume, this call returns `Ok(())`.
///
/// Possible error results include (but are not limited to):
/// - `Err(Status::NOT_SUPPORTED)` if unsupported on this platform.
/// - `Err(Status::ACCESS_DENIED)` or `Err(Status::INVALID_ARGS)` if the suspend power state is
///   incompatible with the current power state of other CPUs. This is a somewhat normal error and
///   must be handled by callers.
///
/// TODO(https://fxbug.dev/417556024): Consider adding a platform-specific or custom result type to
/// differentiate between success-but-did-not-power-down and powered-down.
///
/// TODO(https://fxbug.dev/417558115): Consider checking the state of the secondary CPUs in order to
/// determine which power state value to use so that we can eliminate the need to return
/// `ZX_ERR_ACCESS_DENIED` or `ZX_ERR_INVALID_ARGS` in "normal" situations.
///
/// TODO(https://fxbug.dev/417558115): Currently, the caller is responsible for determining if
/// pausing/resuming the monotonic clock is necessary and then actually doing it. More thought needs
/// to be given to where that logic should live. Right now, it's done in
/// `IdlePowerThread::update_monotonic_clock`, however, it might be better to move some of that
/// logic down into the platform layer (perhaps within `platform_suspend_cpu`).
///
/// # Safety
///
/// Must be called on a kernel-only thread pinned to the calling CPU with both interrupts and
/// preemption disabled and a wakeup interrupt source configured.
#[inline]
pub unsafe fn platform_suspend_cpu(
    allow_domain: PlatformAllowDomainPowerDown,
) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds the kernel-only thread, CPU-pinned, IRQ-disabled, and
        // preempt-disabled preconditions of `platform_suspend_cpu`.
        Status::ok(unsafe {
            cpp_platform_suspend_cpu(allow_domain == PlatformAllowDomainPowerDown::Yes)
        })
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::power::platform_suspend_cpu(
            allow_domain == PlatformAllowDomainPowerDown::Yes,
        )
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::rust_platform_suspend_cpu(
            allow_domain == PlatformAllowDomainPowerDown::Yes,
        )
    }
}

/// Returns `true` iff this platform supports [`platform_suspend_system`].
#[inline]
pub fn platform_supports_suspend_system() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Pure platform capability query with no preconditions.
        unsafe { cpp_platform_supports_suspend_system() }
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::power::platform_supports_suspend_system()
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::rust_platform_supports_suspend_system()
    }
}

/// Suspends the entire system from the boot CPU after all secondary CPUs have been powered off.
///
/// Prior to calling this function, it's critical to set up some kind of interrupt that will wake
/// the boot CPU and resume execution.
///
/// Returns:
/// - `Ok(())` on success. Note that this does not necessarily mean that the system actually
///   suspended (e.g. it may return immediately if an interrupt is pending).
/// - `Err(Status::NOT_SUPPORTED)` if unsupported on this platform.
/// - `Err(Status::BAD_STATE)`, without suspending, if any secondary CPU is online or the platform
///   cannot confirm that every secondary CPU has powered off.
/// - Other errors if the platform rejects the request.
///
/// # Safety
///
/// Must be called on the kernel-only boot CPU (`curr_cpu_num() == BOOT_CPU_ID`) idle/power thread
/// with interrupts and preemption disabled and at least one wakeup source configured.
#[inline]
pub unsafe fn platform_suspend_system() -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds the boot-CPU, kernel-only thread, IRQ-disabled, and
        // preempt-disabled preconditions of `platform_suspend_system`.
        Status::ok(unsafe { cpp_platform_suspend_system() })
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::power::platform_suspend_system()
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::rust_platform_suspend_system()
    }
}

/// Queries the current power state of `cpu_id` from the platform layer.
#[inline]
pub fn platform_get_cpu_state(cpu_id: cpu_num_t) -> Result<PowerCpuState, Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Pure platform CPU power state query with no memory safety preconditions.
        let res = unsafe { cpp_platform_get_cpu_state(cpu_id) };
        Status::ok(res.status)?;
        match PowerCpuState::try_from(res.state) {
            Ok(state) => Ok(state),
            Err(_) => {
                core::hint::cold_path();
                Err(Status::BAD_STATE)
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::platform::platform_get_cpu_state(cpu_id)
    }
    #[cfg(target_arch = "riscv64")]
    {
        let mut state = PowerCpuState::Off;
        crate::generic_riscv64::platform::rust_platform_get_cpu_state(cpu_id, &mut state)?;
        Ok(state)
    }
}

#[inline]
fn platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    // SAFETY: `cpp_platform_specific_halt` halts or reboots the system and never returns.
    unsafe { cpp_platform_specific_halt(suggested_action, reason, halt_on_panic) }
}

/// Common platform halt path. This handles some tasks we always want to make
/// sure we handle before dropping into the common platform specific halt
/// routine.
pub fn platform_halt(suggested_action: PlatformHaltAction, reason: ZirconCrashReason) -> ! {
    // Disable the automatic uptime updating.  We are going to attempt to
    // deliberately halt the system, and we don't want the crashlog to indicate a
    // spontaneous reboot.
    PlatformCrashlog::get().enable_crashlog_uptime_updates(false);

    // We are haling on purpose.  Disable the watchdog (if we have one, and if we
    // can) if we plan to halt instead of instigate a reboot.  If we are going to
    // try to actually reboot, pet the dog one last time to give ourselves the
    // maximum amount of time to arrange our graceful reboot.
    let mut halt_on_panic = BootOptions::get().halt_on_panic;

    // If we are crashing while usermode has suspended the debug UART, there is no
    // way for anyone to interact with the panic shell, and we cannot print
    // anything to the UART.  Rather than silently hanging, reboot so that the
    // crashlog (which will note that the UART was suspended) can be recovered.
    if halt_on_panic && reason != ZirconCrashReason::NoCrash && platform_serial_is_suspended() {
        halt_on_panic = false;
    }

    if halt_on_panic {
        // SAFETY: Safe to call after early boot; dispatches to the registered static watchdog
        // driver or the no-op default driver.
        let _ = unsafe { hw_watchdog_set_enabled(false) };
    } else {
        // SAFETY: Safe to call after early boot; dispatches to the registered static watchdog
        // driver or the no-op default driver.
        unsafe { hw_watchdog_pet() };
    }

    // Render and stash our crashlog.  crashlog_to_string will decide what should
    // be in the crashlog payload based on the provided crash reason.
    {
        let mut rendered_crashlog_len = 0;
        let crashlog = PlatformCrashlog::get();
        let target = crashlog.get_render_target();
        if target.len() > 0 {
            // SAFETY: `target` was returned by `crashlog.get_render_target()` with
            // `target.len() > 0`, so by `crashlog::Interface`'s safety contract it is non-null,
            // valid for writes of `target.len()` bytes, and unaliased during `platform_halt`.
            let target = unsafe { slice::from_raw_parts_mut(target.cast::<u8>(), target.len()) };
            rendered_crashlog_len = crate::crashlog::crashlog_to_string(target, reason as u32);
        }
        // SAFETY: Called during single-threaded platform halt after
        // `enable_crashlog_uptime_updates(false)` and after rendering to `target` has completed.
        unsafe {
            crashlog.finalize(reason, rendered_crashlog_len);
        }
    }

    // This is a graceful reboot, invalidate the persistent dlog (if we have one)
    // and persistent trace buffer (if we have one) so that we don't attempt to
    // recover it during a reboot.
    if reason == ZirconCrashReason::NoCrash {
        persistent_dlog_invalidate();
        jtrace_invalidate();
    }

    // Finally, fall into the platform specific halt handler.
    platform_specific_halt(suggested_action, reason, halt_on_panic);
}

/// FFI trampoline for [`platform_halt`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
) -> ! {
    platform_halt(suggested_action, reason)
}

/// Prepares the platform to print a panic: stops the other CPUs and switches the
/// console to a mode that works without them.
#[inline]
pub fn platform_panic_start() {
    platform_panic_start_with_option(PanicStartHaltOtherCpus::Yes);
}

/// Prepares the platform to print a panic, optionally halting other CPUs.
#[inline]
pub fn platform_panic_start_with_option(option: PanicStartHaltOtherCpus) {
    // SAFETY: no preconditions; safe to call from any CPU at any time.
    unsafe { cpp_platform_panic_start(matches!(option, PanicStartHaltOtherCpus::Yes)) }
}

/// # Safety
///
/// If `argc > 1`, `argv` must be valid for at least 2 elements and `argv[1].arg_str` must be a
/// valid NUL-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_reboot(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    let is_panic_shell = (flags & CMD_FLAG_PANIC) != 0;

    // If we are already panicking, don't repeat the first half of `platform_halt`. Instead,
    // just finish the reboot.
    if is_panic_shell {
        platform_specific_halt(
            PlatformHaltAction::Reboot,
            ZirconCrashReason::Panic,
            /*halt_on_panic=*/ false,
        );
        // unreachable
    }

    // SAFETY: The console dispatcher guarantees `argv` is valid for `argc` elements when `argc > 1`
    // and that `arg_str` points to a valid NUL-terminated string.
    if argc > 1 && unsafe { CStr::from_ptr((*argv.add(1)).arg_str) } == c"bootloader" {
        dprintf!(INFO, "rebooting to the bootloader . . .\n");
        platform_halt(PlatformHaltAction::RebootBootloader, ZirconCrashReason::NoCrash);
        // unreachable
    }
    platform_halt(PlatformHaltAction::Reboot, ZirconCrashReason::NoCrash);
}

#[cfg(console_enabled)]
static_command!(
    CMD_REBOOT,
    c"reboot".as_ptr(),
    c"reboot the system".as_ptr(),
    cmd_reboot,
    CMD_AVAIL_ALWAYS
);
