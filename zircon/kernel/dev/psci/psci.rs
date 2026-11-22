// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The Arm Power State Coordination Interface (PSCI) driver: CPU on/off, CPU and system suspend,
//! and system off/reset through the firmware.
//!
//! Currently these routines assume CPU topologies that are described only in AFF0 and AFF1. If a
//! system is architected such that AFF2 or AFF3 are non-zero then this code will need to be
//! revisited.

use crate::arch_rs::arm64::smccc::{arm_smccc_hvc, arm_smccc_smc};
use crate::arch_rs::arm64::{
    arch_curr_cpu_num, arch_max_num_cpus, arm64_get_boot_el, arm64_secondary_resume_start_paddr,
};
use crate::arch_rs::ints_disabled;
use crate::counters;
use crate::pdev_power::{PdevPowerOps, PowerCpuState, PowerRebootFlags, pdev_register_power};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use debug::{dprintf, ltrace_entry, ltracef};
#[cfg(ktest)]
use unittest as _;
use zbi::{ArmPsciCpuSuspendStateFlags, DcfgArmPsciCpuSuspendState, DcfgArmPsciDriver};
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

// Known PSCI calls as of PSCI 1.2, Document DEN0022E, and SMCCC version discovery, generated from
// <dev/psci.h>.
use psci_bindings::{
    PSCI64_AFFINITY_INFO, PSCI64_CPU_DEFAULT_SUSPEND, PSCI64_CPU_FREEZE, PSCI64_CPU_OFF,
    PSCI64_CPU_ON, PSCI64_CPU_SUSPEND, PSCI64_MEM_PROTECT, PSCI64_MEM_PROTECT_RANGE,
    PSCI64_MIGRATE, PSCI64_MIGRATE_INFO_TYPE, PSCI64_MIGRATE_INFO_UP_CPU, PSCI64_NODE_HW_STATE,
    PSCI64_PSCI_FEATURES, PSCI64_PSCI_SET_SUSPEND_MODE, PSCI64_PSCI_STAT_COUNT,
    PSCI64_PSCI_STAT_RESIDENCY, PSCI64_PSCI_VERSION, PSCI64_SMCCC_VERSION, PSCI64_SYSTEM_OFF,
    PSCI64_SYSTEM_RESET, PSCI64_SYSTEM_RESET2, PSCI64_SYSTEM_SUSPEND,
};

// See section 5.2.2 of "Arm Power State Coordination Interface", DEN0022F.b.
//
// The PSCI return values from <dev/psci.h>, widened to the `i64` that PSCI calls return. bindgen
// types these macros as `u32` (`PSCI_SUCCESS`) or `i32` (the negative errors), and `match`
// patterns cannot convert, so these consts widen them losslessly with `as`.

/// PSCI call succeeded.
const PSCI_SUCCESS: i64 = psci_bindings::PSCI_SUCCESS as i64;
/// Operation is not implemented or not supported.
const PSCI_NOT_SUPPORTED: i64 = psci_bindings::PSCI_NOT_SUPPORTED as i64;
/// One or more arguments to the PSCI call are invalid.
const PSCI_INVALID_PARAMETERS: i64 = psci_bindings::PSCI_INVALID_PARAMETERS as i64;
/// Operation is not permitted in the current system state.
const PSCI_DENIED: i64 = psci_bindings::PSCI_DENIED as i64;
/// Target CPU or power domain is already powered on.
const PSCI_ALREADY_ON: i64 = psci_bindings::PSCI_ALREADY_ON as i64;
/// A `CPU_ON` request for the target CPU is already in progress.
const PSCI_ON_PENDING: i64 = psci_bindings::PSCI_ON_PENDING as i64;
/// Internal error in the PSCI implementation.
const PSCI_INTERNAL_FAILURE: i64 = psci_bindings::PSCI_INTERNAL_FAILURE as i64;
/// Target core is not present (e.g., for `MIGRATE`).
const PSCI_NOT_PRESENT: i64 = psci_bindings::PSCI_NOT_PRESENT as i64;
/// Target core is disabled and cannot be powered on.
const PSCI_DISABLED: i64 = psci_bindings::PSCI_DISABLED as i64;
/// Entry point address is invalid.
const PSCI_INVALID_ADDRESS: i64 = psci_bindings::PSCI_INVALID_ADDRESS as i64;

zr::static_assert!(core::mem::size_of::<DcfgArmPsciDriver>() == 104);
zr::static_assert!(core::mem::align_of::<DcfgArmPsciDriver>() == 8);
zr::static_assert!(core::mem::size_of::<DcfgArmPsciCpuSuspendState>() == 32);
zr::static_assert!(core::mem::align_of::<DcfgArmPsciCpuSuspendState>() == 8);

// The number of PSCI CPU_SUSPEND calls that completed successfully and resulted in a powered down
// state.
counters::define_kcounter!(
    COUNTER_PSCI_CPU_SUSPEND_POWERED_DOWN,
    "psci.cpu_suspend.ok.powered_down",
    Sum
);

// The number of PSCI CPU_SUSPEND calls that completed successfully, but where state was retained
// (i.e. did not reach a powered down state for whatever reason).
counters::define_kcounter!(COUNTER_PSCI_CPU_SUSPEND_RETAINED, "psci.cpu_suspend.ok.retained", Sum);

// The number of PSCI CPU_SUSPEND calls that failed with PSCI_INVALID_PARAMETERS.
counters::define_kcounter!(
    COUNTER_PSCI_CPU_SUSPEND_INVALID_PARAMETERS,
    "psci.cpu_suspend.invalid_parameters",
    Sum
);

// The number of PSCI CPU_SUSPEND calls that failed with PSCI_DENIED.
counters::define_kcounter!(COUNTER_PSCI_CPU_SUSPEND_DENIED, "psci.cpu_suspend.psci_denied", Sum);

// The number of PSCI SYSTEM_SUSPEND calls that completed successfully and resulted in a powered
// down state.
counters::define_kcounter!(
    COUNTER_PSCI_SYSTEM_SUSPEND_POWERED_DOWN,
    "psci.system_suspend.ok.powered_down",
    Sum
);

// The number of PSCI SYSTEM_SUSPEND calls that completed successfully, but where state was retained
// (i.e. did not reach a powered down state for whatever reason).
counters::define_kcounter!(
    COUNTER_PSCI_SYSTEM_SUSPEND_RETAINED,
    "psci.system_suspend.ok.retained",
    Sum
);

// The number of PSCI SYSTEM_SUSPEND calls that failed with PSCI_INVALID_PARAMETERS.
counters::define_kcounter!(
    COUNTER_PSCI_SYSTEM_SUSPEND_INVALID_PARAMETERS,
    "psci.system_suspend.invalid_parameters",
    Sum
);

// The number of PSCI SYSTEM_SUSPEND calls that failed with PSCI_DENIED.
counters::define_kcounter!(
    COUNTER_PSCI_SYSTEM_SUSPEND_DENIED,
    "psci.system_suspend.psci_denied",
    Sum
);

// The number of PSCI SYSTEM_SUSPEND calls that failed with PSCI_ALREADY_ON.
counters::define_kcounter!(
    COUNTER_PSCI_SYSTEM_SUSPEND_ALREADY_ON,
    "psci.system_suspend.already_on",
    Sum
);

/// Specifies which power_state format is used by this PSCI implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PowerStateFormat {
    Unknown,
    /// Section 5.4.2.1 of "Arm Power State Coordination Interface", DEN0022F.b.
    Original,
    /// Section 5.4.2.2 of "Arm Power State Coordination Interface", DEN0022F.b.
    Extended,
}

/// Returns true iff `power_state` is a "powerdown" power_state (as opposed to a standby or
/// retention state) under `power_state_format`.
fn psci_is_powerdown_power_state(power_state_format: PowerStateFormat, power_state: u32) -> bool {
    match power_state_format {
        // In the original format, StateType is bit-16. If StateType is 1, it's a powerdown state.
        PowerStateFormat::Original => (power_state & (1 << 16)) != 0,
        // In the extended format, StateType is bit-30.
        PowerStateFormat::Extended => (power_state & (1 << 30)) != 0,
        format => panic!("unknown power_state_format {}", format as u32),
    }
}

/// Represents a power state that can be used with the PSCI CPU_SUSPEND call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuSuspendPowerState {
    /// The value to pass to a PSCI CPU_SUSPEND call.
    value: u32,
    /// Whether this value is a powerdown state or not.
    is_powerdown_state: bool,
}

/// Stores the power state values to be used with PSCI CPU_SUSPEND, organized by the scope they
/// target.
#[derive(Clone, Copy, Debug)]
struct CpuSuspendPowerStates {
    /// Contains a power state value targeting the calling CPU or `None` if not supported.
    target_cpu: Option<CpuSuspendPowerState>,

    /// Optionally contains a power state value targeting the enclosing power domain (think cluster
    /// or whole system). Will only contain a value if `target_cpu` contains a value.
    target_power_domain: Option<CpuSuspendPowerState>,
}

/// Configuration and probed capabilities of the PSCI driver.
///
/// Written once by [`psci_init`] during single-CPU early boot with interrupts disabled before
/// secondary CPUs start, and read-only thereafter. Boards without a PSCI driver never call
/// `psci_init` and retain [`PsciState::DEFAULT`].
#[derive(Clone, Copy)]
struct PsciState {
    psci_set_suspend_mode_supported: bool,
    psci_system_suspend_supported: bool,
    shutdown_args: [u64; 3],
    reboot_args: [u64; 3],
    reboot_bootloader_args: [u64; 3],
    reboot_recovery_args: [u64; 3],
    reset_command: u32,
    power_state_format: PowerStateFormat,
    power_states: CpuSuspendPowerStates,
    /// The routine used to issue PSCI calls.
    // Safety invariant: `psci_hvc_call` is stored only when `arm64_get_boot_el() < 2` (so every
    // CPU runs at EL1).
    do_psci_call: PsciCallProc,
}

impl PsciState {
    /// Initial driver state before [`psci_init`] runs: SMC conduit, `PSCI64_SYSTEM_RESET` with
    /// zeroed reboot/shutdown arguments, and no optional suspend capabilities enabled.
    const DEFAULT: Self = Self {
        psci_set_suspend_mode_supported: false,
        psci_system_suspend_supported: false,
        shutdown_args: [0, 0, 0],
        reboot_args: [0, 0, 0],
        reboot_bootloader_args: [0, 0, 0],
        reboot_recovery_args: [0, 0, 0],
        reset_command: PSCI64_SYSTEM_RESET,
        power_state_format: PowerStateFormat::Unknown,
        power_states: CpuSuspendPowerStates { target_cpu: None, target_power_domain: None },
        do_psci_call: psci_smc_call,
    };
}

/// Maximum number of usable PSCI `CPU_SUSPEND` idle states retained from the physboot handoff.
///
/// The devicetree boot shim emits each per-CPU idle state once and each domain idle state once per
/// referencing PSCI power domain (for example, 2 per-CPU states + 4 clusters * 1-2 cluster states
/// + 1 system state <= 11 entries on 4-cluster topologies), with headroom to 16.
pub const MAX_CPU_SUSPEND_STATES: usize = 16;

/// A usable PSCI `CPU_SUSPEND` idle state retained from the ZBI handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsciCpuSuspendState {
    /// Raw `power_state` parameter passed to `PSCI_CPU_SUSPEND`.
    pub power_state: u32,
    /// Validated ZBI state flags (`LOCAL_TIMER_STOPS`, `TARGETS_POWER_DOMAIN`).
    pub flags: ArmPsciCpuSuspendStateFlags,
    /// True if entering this state powers down the core or domain (losing architectural context),
    /// false for standby or retention states (`StateType == 0`).
    pub is_powerdown: bool,
    /// True if the boot loader emitted the all-ones (`u64::MAX`) fallback `cpu_mask` because
    /// devicetree idle-state topology links could not be mapped to CPUs.
    pub cpu_mask_is_fallback: bool,
    /// Worst-case latency to enter the state, in microseconds.
    pub entry_latency_us: u32,
    /// Worst-case latency to exit the state, in microseconds.
    pub exit_latency_us: u32,
    /// Minimum residency for energy break-even, in microseconds.
    pub min_residency_us: u32,
    /// Mask of CPUs eligible to enter a per-CPU state, or CPUs that must all be idle for a
    /// `TARGETS_POWER_DOMAIN` state. Clamped to the CPUs present in the system when returned by
    /// [`psci_get_cpu_suspend_states`].
    pub cpu_mask: u64,
}

impl PsciCpuSuspendState {
    /// Zero-initialized placeholder for fixed-size query buffers.
    pub const EMPTY: Self = Self {
        power_state: 0,
        flags: ArmPsciCpuSuspendStateFlags::empty(),
        is_powerdown: false,
        cpu_mask_is_fallback: false,
        entry_latency_us: 0,
        exit_latency_us: 0,
        min_residency_us: 0,
        cpu_mask: 0,
    };
}

/// Usable PSCI `CPU_SUSPEND` idle states retained during [`psci_init`] for per-domain energy
/// modeling. Kept separate from [`PsciState`] so hot-path by-value [`state()`] reads do not copy
/// the 16-entry table.
struct RetainedCpuSuspendStates {
    states: [PsciCpuSuspendState; MAX_CPU_SUSPEND_STATES],
    retained_count: usize,
}

impl RetainedCpuSuspendStates {
    const EMPTY: Self =
        Self { states: [PsciCpuSuspendState::EMPTY; MAX_CPU_SUSPEND_STATES], retained_count: 0 };

    fn get_states(&self, all_cpus: u64, out: &mut [PsciCpuSuspendState]) -> usize {
        let count = out.len().min(self.retained_count);
        for (dst, src) in out[..count].iter_mut().zip(&self.states[..count]) {
            *dst = *src;
            dst.cpu_mask = src.cpu_mask & all_cpus;
        }
        self.retained_count
    }
}

// TODO(https://fxbug.dev/540178114): Use the shared `zr` type for boot-initialized statics once
// `BootInitCell`, `BootStatic`, `BootGlobal`, and `BootCell` are consolidated.
/// Static storage initialized to a default value at compile time, written at most once during
/// single-CPU early boot by [`psci_init`] with interrupts disabled before secondary CPUs are
/// started, and read-only thereafter.
#[repr(transparent)]
struct BootInitCell<T>(UnsafeCell<T>);

impl<T> BootInitCell<T> {
    const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    #[inline]
    const fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

// SAFETY: `BootInitCell<T>` is module-private and `as_ptr` only returns a raw pointer without
// dereferencing it, so safe code cannot perform unsynchronized reads or writes through
// `&BootInitCell<T>`. Every dereference in this module is either the single-CPU boot-time write by
// `psci_init` (with interrupts disabled before secondary CPUs start) or a read-only access
// (`state()` by-value copy or `&*RETAINED_SUSPEND_STATES.as_ptr()` shared borrow) that cannot
// execute concurrently with or alias that write. `T: Send + Sync` permits values written on the
// boot CPU to be copied and shared across CPUs.
unsafe impl<T: Send + Sync> Sync for BootInitCell<T> {}

/// The PSCI driver state. See [`PsciState`].
static PSCI_STATE: BootInitCell<PsciState> = BootInitCell::new(PsciState::DEFAULT);

/// Retained usable PSCI `CPU_SUSPEND` idle states. Written at most once by [`psci_init`] and
/// read-only thereafter via [`psci_get_cpu_suspend_states`].
static RETAINED_SUSPEND_STATES: BootInitCell<RetainedCpuSuspendStates> =
    BootInitCell::new(RetainedCpuSuspendStates::EMPTY);

/// Returns a copy of the PSCI driver state.
fn state() -> PsciState {
    // SAFETY: `PSCI_STATE` is statically initialized to `PsciState::DEFAULT` and written at most
    // once by `psci_init` on the boot CPU with interrupts disabled before secondary CPUs start
    // (`lk_main -> platform_early_init -> PlatformDriverHandoffEarly -> PsciInit -> rust_psci_init
    // -> psci_init`). `psci_init` builds its state on the stack and performs a single store without
    // calling `state()`, and no reference to `PSCI_STATE` is ever formed, so no write can race
    // with or alias this by-value read.
    unsafe { *PSCI_STATE.as_ptr() }
}

fn psci_status_to_zx_status(psci_result: u64) -> Result<(), Status> {
    let status = psci_result as i64;
    match status {
        PSCI_SUCCESS => Ok(()),
        PSCI_NOT_SUPPORTED | PSCI_DISABLED => Err(Status::NOT_SUPPORTED),
        PSCI_INVALID_PARAMETERS => Err(Status::INVALID_ARGS),
        PSCI_INVALID_ADDRESS => Err(Status::OUT_OF_RANGE),
        PSCI_DENIED => Err(Status::ACCESS_DENIED),
        PSCI_ALREADY_ON => Err(Status::BAD_STATE),
        PSCI_ON_PENDING => Err(Status::SHOULD_WAIT),
        PSCI_INTERNAL_FAILURE => Err(Status::INTERNAL),
        PSCI_NOT_PRESENT => Err(Status::NOT_FOUND),
        _ => Err(Status::BAD_STATE),
    }
}

/// Issues a PSCI call via the SMC conduit.
///
/// # Safety
///
/// The caller must ensure that `function` and `arg0..=arg2` conform to the ARM SMCCC / PSCI
/// specification and that executing the call at EL3 preserves Rust memory safety.
unsafe extern "C" fn psci_smc_call(function: u32, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    ltracef!("0x{:x} 0x{:x} 0x{:x} 0x{:x}\n", function, arg0, arg1, arg2);
    arm_smccc_smc(function, arg0, arg1, arg2, 0, 0, 0, 0).x0
}

/// Issues a PSCI call via the HVC conduit.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU is executing at EL1 (`CurrentEL.EL == 1`).
/// - `function` and `arg0..=arg2` conform to the ARM SMCCC / PSCI specification and executing the
///   call at the hypervisor level preserves Rust memory safety.
unsafe extern "C" fn psci_hvc_call(function: u32, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    // SAFETY: The caller guarantees `CurrentEL.EL == 1` and that `function` and `arg0..=arg2`
    // (with remaining parameter registers zeroed) are SMCCC-conformant and safe to execute at the
    // hypervisor level.
    unsafe { arm_smccc_hvc(function, arg0, arg1, arg2, 0, 0, 0, 0).x0 }
}

/// A routine that issues a PSCI call: `psci_smc_call` or `psci_hvc_call`.
type PsciCallProc = unsafe extern "C" fn(u32, u64, u64, u64) -> u64;

/// Issues a PSCI call through the conduit selected by `psci_init`.
///
/// # Safety
///
/// The caller must ensure that `function` and `arg0..=arg2` conform to the ARM SMCCC / PSCI
/// specification and that executing the call through the configured firmware conduit preserves
/// Rust memory safety (for example, `PSCI64_CPU_ON` must pass a valid kernel entry point and
/// context; raw calls from the `psci` debug console command are operator-authorized).
unsafe fn do_psci_call(function: u32, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    // SAFETY: By `PsciState::do_psci_call`'s safety invariant, `psci_hvc_call` is installed only
    // when `arm64_get_boot_el() < 2`, in which case every CPU enters the kernel at EL1 via
    // `ArmDropToEl1` (`CurrentEL.EL == 1`). The caller guarantees that `function` and `arg0..=arg2`
    // are SMCCC/PSCI-conformant and safe for firmware to execute.
    unsafe { (state().do_psci_call)(function, arg0, arg1, arg2) }
}

/// Parse and print `config`, returning the legacy two-slot `CPU_SUSPEND` power states and
/// populating `retained` with up to [`MAX_CPU_SUSPEND_STATES`] usable idle states for per-domain
/// energy modeling.
fn parse_cpu_suspend_power_states(
    power_state_format: PowerStateFormat,
    config: &[DcfgArmPsciCpuSuspendState],
    retained: &mut RetainedCpuSuspendStates,
) -> CpuSuspendPowerStates {
    debug_assert!(power_state_format != PowerStateFormat::Unknown);

    let mut power_states = CpuSuspendPowerStates { target_cpu: None, target_power_domain: None };
    retained.retained_count = 0;

    if config.is_empty() {
        dprintf!(INFO, "PSCI power_states: none\n");
        return power_states;
    }

    const KNOWN_FLAGS: u32 = ArmPsciCpuSuspendStateFlags::LOCAL_TIMER_STOPS.bits()
        | ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN.bits();

    dprintf!(
        INFO,
        "PSCI power_state format: {}\n",
        if power_state_format == PowerStateFormat::Original { "original" } else { "extended" }
    );
    dprintf!(INFO, "PSCI power_states:\n");

    let mut has_retained_cpu_state = false;

    // Iterate over every element. The elements are in no particular order. The last-one-wins
    // behavior of `power_states` is somewhat arbitrary.
    for elem in config {
        let flags = elem.flags.bits();
        // Ignore any power state that contains flags we don't know about as they may indicate a
        // requirement that must be met before invoking that power state.
        let skip = (flags & !KNOWN_FLAGS) != 0;

        let is_powerdown_state =
            psci_is_powerdown_power_state(power_state_format, elem.power_state);
        dprintf!(
            INFO,
            "  id=0x{:x}, power_state=0x{:x}, flags=0x{:x}, powerdown={}{}\n",
            elem.id,
            elem.power_state,
            flags,
            u8::from(is_powerdown_state),
            if skip { " SKIPPED" } else { "" }
        );
        dprintf!(
            INFO,
            "    entry_latency_us={}, exit_latency_us={}, min_residency_us={}, cpu_mask=0x{:x}\n",
            elem.entry_latency_us,
            elem.exit_latency_us,
            elem.min_residency_us,
            elem.cpu_mask
        );

        if skip {
            continue;
        }

        let targets_domain = elem.flags.contains(ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN);
        let power_state = CpuSuspendPowerState { value: elem.power_state, is_powerdown_state };
        if !targets_domain {
            power_states.target_cpu = Some(power_state);
        } else {
            power_states.target_power_domain = Some(power_state);
        }

        if retained.retained_count < retained.states.len() {
            retained.states[retained.retained_count] = PsciCpuSuspendState {
                power_state: elem.power_state,
                flags: elem.flags,
                is_powerdown: is_powerdown_state,
                cpu_mask_is_fallback: elem.cpu_mask == u64::MAX,
                entry_latency_us: elem.entry_latency_us,
                exit_latency_us: elem.exit_latency_us,
                min_residency_us: elem.min_residency_us,
                cpu_mask: elem.cpu_mask,
            };
            retained.retained_count += 1;
            if !targets_domain {
                has_retained_cpu_state = true;
            }
        } else {
            dprintf!(
                CRITICAL,
                "PSCI: WARNING: not retaining id=0x{:x} power_state=0x{:x}: \
                 more than {} power_states\n",
                elem.id,
                elem.power_state,
                MAX_CPU_SUSPEND_STATES
            );
        }
    }

    if !has_retained_cpu_state {
        retained.retained_count = 0;
    }

    power_states
}

const fn cpu_mask_for_count(num_cpus: u32) -> u64 {
    if num_cpus >= u64::BITS { u64::MAX } else { (1u64 << num_cpus) - 1 }
}

fn system_cpu_mask() -> u64 {
    cpu_mask_for_count(arch_max_num_cpus())
}

/// Copies up to `states.len()` of the retained usable PSCI `CPU_SUSPEND` idle states into `states`
/// and returns the total number of retained usable idle states, or `0` if no retained usable
/// CPU-targeted idle state exists.
///
/// Domain-targeted entries (`TARGETS_POWER_DOMAIN`) are included as configured in the ZBI
/// regardless of the active PSCI suspend mode; callers must only use them when OS-initiated
/// suspend mode is enabled. CPU masks in the returned entries are clamped to the CPUs in the
/// system (with [`PsciCpuSuspendState::cpu_mask_is_fallback`] preserving whether the unclamped ZBI
/// mask was `u64::MAX`), so this must only be called after the CPU topology has been initialized.
pub fn psci_get_cpu_suspend_states(states: &mut [PsciCpuSuspendState]) -> usize {
    let all_cpus = system_cpu_mask();
    // SAFETY: `RETAINED_SUSPEND_STATES` is statically initialized to
    // `RetainedCpuSuspendStates::EMPTY` and written at most once by `psci_init` on the boot CPU
    // with interrupts disabled before secondary CPUs start (`psci_init` does not call
    // `psci_get_cpu_suspend_states`). No mutable reference or write to `RETAINED_SUSPEND_STATES`
    // can race with or alias this read-only borrow.
    let retained = unsafe { &*RETAINED_SUSPEND_STATES.as_ptr() };
    retained.get_states(all_cpus, states)
}

/// Holds register state to be saved/restored across a suspend/resume cycle.
///
/// Must be kept in sync with `psci_do_suspend`, `psci_do_system_suspend`, and `psci_do_resume`.
///
/// Layout (see save_resume_context in psci-suspend.S):
///   regs[0..13]:  daif, sctlr_el1, tcr_el1, vbar_el1, mdscr_el1, cpacr_el1,
///                 osdlr_el1, sp, sp_el0, contextidr_el1, tpidr_el1,
///                 tpidrro_el0, tpidr_el0, zcr_el1
///   regs[14..25]: x18-x29
///   regs[26]:     lr
///   regs[27]:     PSTATE.PAN
///   regs[28]:     vtcr_el2 (when running in EL2)
///   regs[29]:     unused (padding)
#[repr(C)]
#[derive(Debug, Default)]
struct PsciCpuResumeContext {
    regs: [u64; 2 * 15],
}

zr::static_assert!(core::mem::size_of::<PsciCpuResumeContext>() == 240);
zr::static_assert!(core::mem::align_of::<PsciCpuResumeContext>() == 8);

unsafe extern "C" {
    /// Saves register state in `context`, then issues a `PSCI64_CPU_SUSPEND` call using
    /// `psci_call`, passing along the supplied `power_state`, `entry`, and `context` arguments.
    ///
    /// This function is designed to work with `psci_do_resume` and `arm64_secondary_resume_start`.
    ///
    /// In all cases, this function will always appear to return to its caller. Depending on the
    /// PSCI implementation and the conditions, it may do so directly, or in the case of a
    /// successful CPU_SUSPEND, it may do so via `arm64_secondary_resume_start` and
    /// `psci_do_resume`.
    ///
    /// If the return value is less than or equal to zero, then control did not branch to `entry`
    /// and the return value should be interpreted as a PSCI return value (see section 5.4.5 of
    /// DEN0022F.b):
    ///
    ///   PSCI_SUCCESS - The CPU_SUSPEND call succeeded. However, the CPU did not reach a powerdown
    ///   state because of a pending interrupt, or simply because the requested `power_state` is
    ///   not a powerdown state.
    ///
    ///   PSCI_INVALID_PARAMETERS - The `power_state` is invalid, or a low-power state was
    ///   requested for a higher-than-core-level topology node (e.g. cluster) and at least one of
    ///   the children in that node is in a local low-power state that is incompatible with the
    ///   request.
    ///
    ///   PSCI_DENIED - A low-power state is requested for a higher-than-core-level topology node
    ///   (e.g. cluster) and all the cores that are in an incompatible state with the request are
    ///   running, as opposed to being in a low-power state.
    ///
    ///   PSCI_INVALID_ADDRESS - `entry` is not a valid physical address.
    ///
    /// If the return value is greater than zero, then the CPU did in fact suspend and resume
    /// execution at `entry` before returning to the caller.
    ///
    /// Implemented in assembly.
    ///
    /// # Safety
    ///
    /// The caller must meet the requirements of [`psci_cpu_suspend`], and:
    ///
    /// - `psci_call` must be a valid PSCI conduit (`psci_smc_call` or `psci_hvc_call`, as selected
    ///   by `psci_init`).
    /// - `entry` must be the physical address of `arm64_secondary_resume_start`.
    /// - `context` must be valid for writes for the duration of the call, and must stay valid for
    ///   reads until `psci_do_resume` has restored the CPU state from it on the resume path (that
    ///   is, until this function returns, by either path).
    fn psci_do_suspend(
        psci_call: PsciCallProc,
        power_state: u32,
        entry: u64,
        context: *mut PsciCpuResumeContext,
    ) -> i64;

    /// Like `psci_do_suspend`, but issues PSCI SYSTEM_SUSPEND (entry, context) rather than
    /// CPU_SUSPEND. The return value is interpreted the same way: if it is greater than zero, the
    /// system did in fact suspend and the calling CPU resumed execution at `entry` before
    /// returning to the caller.
    ///
    /// Implemented in assembly.
    ///
    /// # Safety
    ///
    /// The caller must meet the requirements of [`psci_system_suspend`], and:
    ///
    /// - `psci_call` must be a valid PSCI conduit (`psci_smc_call` or `psci_hvc_call`, as selected
    ///   by `psci_init`).
    /// - `entry` must be the physical address of `arm64_secondary_resume_start`.
    /// - `context` must be valid for writes for the duration of the call, and must stay valid for
    ///   reads until `psci_do_resume` has restored the CPU state from it on the resume path (that
    ///   is, until this function returns, by either path).
    fn psci_do_system_suspend(
        psci_call: PsciCallProc,
        entry: u64,
        context: *mut PsciCpuResumeContext,
    ) -> i64;
}

/// Powers off the system. Only returns if the call fails.
pub extern "C" fn psci_system_off() -> Result<(), Status> {
    let [arg0, arg1, arg2] = state().shutdown_args;
    // SAFETY: `PSCI64_SYSTEM_OFF` with ZBI-configured `shutdown_args` either shuts down the system
    // without returning or returns a PSCI status code.
    psci_status_to_zx_status(unsafe { do_psci_call(PSCI64_SYSTEM_OFF, arg0, arg1, arg2) })
}

/// Returns the PSCI version, with the major version in bits 16-31 and the minor version in bits
/// 0-15.
pub fn psci_get_version() -> u32 {
    // SAFETY: `PSCI64_PSCI_VERSION` is a read-only PSCI query taking no arguments.
    unsafe { do_psci_call(PSCI64_PSCI_VERSION, 0, 0, 0) as u32 }
}

/// Powers down the calling cpu - only returns if call fails.
pub extern "C" fn psci_cpu_off() -> Result<(), Status> {
    // SAFETY: `PSCI64_CPU_OFF` takes no arguments and either powers down the calling CPU without
    // returning or returns a PSCI error code.
    psci_status_to_zx_status(unsafe { do_psci_call(PSCI64_CPU_OFF, 0, 0, 0) })
}

/// Powers on the CPU whose MPIDR is `mpid`, starting it at physical address `entry` with
/// `context`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `entry` is the physical address of a valid kernel CPU entry point (e.g.
///   `arm64_secondary_start_paddr()`) that accepts `context` in `x0`.
/// - `context` is valid for that entry point (e.g. a bootstrap stack created by
///   `arm64_create_secondary_stack` for the target CPU).
/// - `mpid` does not name a CPU that is currently executing kernel code in an incompatible state.
pub unsafe fn psci_cpu_on(mpid: u64, entry: u64, context: u64) -> Result<(), Status> {
    ltracef!("CPU_ON mpid {:#x}, entry {:#x}\n", mpid, entry);
    // SAFETY: The caller guarantees that `entry`, `context`, and `mpid` satisfy `PSCI64_CPU_ON`'s
    // requirements so firmware starts the target CPU cleanly into valid kernel code.
    psci_status_to_zx_status(unsafe { do_psci_call(PSCI64_CPU_ON, mpid, entry, context) })
}

/// The `PdevPowerOps::cpu_on` entry point for [`psci_cpu_on`].
extern "C" fn psci_cpu_on_op(mpid: u64, entry: u64, context: u64) -> Result<(), Status> {
    // SAFETY: `PdevPowerOps::cpu_on` callers (via `power_cpu_on`) supply the kernel secondary
    // entry physical address and bootstrap context (or operator-authorized console arguments).
    unsafe { psci_cpu_on(mpid, entry, context) }
}

/// Specifies whether a requested power state can target more than the calling CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsciCpuSuspendMaxScope {
    CpuOnly,
    CpuAndMore,
}

/// Returns an appropriate power_state value to be used with [`psci_cpu_suspend`].
///
/// `scope` specifies the maximum topological scope that the selected power state is allowed to
/// target.
///
/// `last` indicates whether the calling CPU is the last one to enter suspend.
///
/// # Panics
///
/// If it selects the power state targeting the calling CPU and none was configured, which is when
/// [`psci_is_cpu_suspend_supported`] returns false.
pub fn psci_select_cpu_suspend_power_state(scope: PsciCpuSuspendMaxScope, last: bool) -> u32 {
    if scope == PsciCpuSuspendMaxScope::CpuAndMore
        && last
        && let Some(power_state) = state().power_states.target_power_domain
    {
        return power_state.value;
    }
    let target_cpu = state().power_states.target_cpu;
    debug_assert!(target_cpu.is_some());
    target_cpu.expect("no CPU_SUSPEND power_state targets the calling CPU").value
}

/// Returns true if a [`psci_cpu_suspend`] might result in a powerdown event for the calling CPU.
/// This function allows callers to skip saving/restoring some CPU state in the case where the
/// suspend operation will not powerdown the calling CPU.
pub fn psci_might_powerdown() -> bool {
    let power_states = state().power_states;
    power_states.target_cpu.is_some_and(|s| s.is_powerdown_state)
        || power_states.target_power_domain.is_some_and(|s| s.is_powerdown_state)
}

/// Whether or not the CPU powered down (lost state) during a CPU_SUSPEND operation. See
/// [`psci_cpu_suspend`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuPoweredDown {
    No,
    Yes,
}

/// Enters a PSCI CPU_SUSPEND state on the calling CPU.
///
/// `power_state` specifies the power state to enter. See also
/// [`psci_select_cpu_suspend_power_state`].
///
/// On success, returns either `CpuPoweredDown::No` or `CpuPoweredDown::Yes`, depending on whether
/// the CPU actually powered down during the operation.
///
/// Returns `Status::NOT_SUPPORTED` if CPU_SUSPEND is not supported on this platform.
///
/// Returns `Status::INVALID_ARGS` if the requested power_state is invalid, or a low-power state
/// was requested for a higher-than-core-level topology node (e.g. cluster) and at least one of the
/// children in that node is in a local low-power state that is incompatible with the request. This
/// is a "normal" error that callers must be prepared to handle.
///
/// Returns `Status::ACCESS_DENIED` if a low-power state was requested for a higher-than-core-level
/// topology node (e.g. cluster) and all the cores that are in an incompatible state with the
/// request are running, as opposed to being in a low-power state. This is a "normal" error that
/// callers must be prepared to handle (typically with a retry).
///
/// # Panics
///
/// If PSCI returns any result other than PSCI_SUCCESS, PSCI_INVALID_PARAMETERS, PSCI_DENIED, or a
/// positive value (e.g. PSCI_INVALID_ADDRESS or PSCI_NOT_SUPPORTED).
///
/// # Safety
///
/// Prior to calling, interrupts must be disabled, preemption must be disabled, and the caller must
/// be pinned to the calling CPU.
///
/// The calling thread must be a kernel-only thread. Because this routine saves only the minimum
/// required register state, it is an error to call this function on a thread that has a "user
/// half" (ThreadDispatcher).
pub unsafe fn psci_cpu_suspend(power_state: u32) -> Result<CpuPoweredDown, Status> {
    ltrace_entry!();

    debug_assert!(ints_disabled());

    if !psci_is_cpu_suspend_supported() {
        return Err(Status::NOT_SUPPORTED);
    }

    let entry_pa = u64::from(arm64_secondary_resume_start_paddr());
    let mut context = PsciCpuResumeContext::default();
    let psci_call = state().do_psci_call;

    ltracef!(
        "cpu {}, psci_call_routine 0x{:x}, power_state 0x{:x}, entry 0x{:x}, context {:p}\n",
        arch_curr_cpu_num(),
        (psci_call as *const ()).addr(),
        power_state,
        entry_pa,
        &context
    );
    // SAFETY: The caller meets `psci_cpu_suspend`'s requirements. `psci_call` is the conduit
    // `psci_init` selected, `entry_pa` relies on the safety-usable invariant of
    // `arm64_secondary_resume_start_paddr`, and `context` is a local of this frame, live until
    // `psci_do_suspend` returns on either path.
    let result = unsafe { psci_do_suspend(psci_call, power_state, entry_pa, &raw mut context) };

    if result > 0 {
        // We took the "long way" and restored CPU context from a powerdown state.
        COUNTER_PSCI_CPU_SUSPEND_POWERED_DOWN.add(1);
        return Ok(CpuPoweredDown::Yes);
    }

    match result {
        PSCI_SUCCESS => {
            COUNTER_PSCI_CPU_SUSPEND_RETAINED.add(1);
            Ok(CpuPoweredDown::No)
        }
        PSCI_INVALID_PARAMETERS => {
            COUNTER_PSCI_CPU_SUSPEND_INVALID_PARAMETERS.add(1);
            Err(Status::INVALID_ARGS)
        }
        PSCI_DENIED => {
            COUNTER_PSCI_CPU_SUSPEND_DENIED.add(1);
            Err(Status::ACCESS_DENIED)
        }
        _ => panic!(
            "cpu {}, psci_call_routine 0x{:x}, power_state 0x{:x}, entry 0x{:x}, context {:p}, \
             result {}",
            arch_curr_cpu_num(),
            (psci_call as *const ()).addr(),
            power_state,
            entry_pa,
            &context,
            result
        ),
    }
}

/// Enters a PSCI SYSTEM_SUSPEND state.
///
/// Only the calling CPU's register state is saved and restored (as for [`psci_cpu_suspend`]). Any
/// system-level state which may be lost while the system is suspended (e.g. interrupt controller
/// distributor state, or peripheral state) is the caller's responsibility.
///
/// Returns `Ok(())` if the call succeeded. Note that this does not necessarily mean that the
/// system actually suspended; PSCI implementations may return success without suspending (e.g.
/// Trusted Firmware-A does so when an interrupt is pending).
///
/// Returns `Status::NOT_SUPPORTED` if SYSTEM_SUSPEND is not supported on this platform.
///
/// Returns `Status::INVALID_ARGS` if the parameters were rejected by PSCI.
///
/// Returns `Status::ACCESS_DENIED` if the request was denied by PSCI. Note that some PSCI
/// implementations (e.g. Trusted Firmware-A) return PSCI_DENIED, not PSCI_ALREADY_ON, if another
/// CPU is still powered on.
///
/// Returns `Status::BAD_STATE` if PSCI reported that another CPU was still powered on
/// (PSCI_ALREADY_ON).
///
/// Panics if PSCI reports any other error (e.g. PSCI_INVALID_ADDRESS, which would indicate a
/// kernel bug).
///
/// TODO(https://fxbug.dev/568438295): Determine whether GIC distributor state must be saved and
/// restored by the caller on platforms which lose it during SYSTEM_SUSPEND.
///
/// # Safety
///
/// Prior to calling, all other CPUs must be powered off, interrupts must be disabled, preemption
/// must be disabled, and the caller must be running on the boot CPU.
///
/// The calling thread must be a kernel-only thread. Because this routine saves only the minimum
/// required register state, it is an error to call this function on a thread that has a "user
/// half" (ThreadDispatcher).
pub unsafe fn psci_system_suspend() -> Result<(), Status> {
    ltrace_entry!();

    debug_assert!(ints_disabled());

    if !psci_is_system_suspend_supported() {
        return Err(Status::NOT_SUPPORTED);
    }

    let entry_pa = u64::from(arm64_secondary_resume_start_paddr());
    let mut context = PsciCpuResumeContext::default();
    let psci_call = state().do_psci_call;

    ltracef!(
        "cpu {}, psci_call_routine 0x{:x}, entry 0x{:x}, context {:p}\n",
        arch_curr_cpu_num(),
        (psci_call as *const ()).addr(),
        entry_pa,
        &context
    );
    // SAFETY: The caller meets `psci_system_suspend`'s requirements. `psci_call` is the conduit
    // `psci_init` selected, `entry_pa` relies on the safety-usable invariant of
    // `arm64_secondary_resume_start_paddr`, and `context` is a local of this frame, live until
    // `psci_do_system_suspend` returns on either path.
    let result = unsafe { psci_do_system_suspend(psci_call, entry_pa, &raw mut context) };

    if result > 0 {
        // We took the "long way" and restored CPU context from a powerdown state.
        COUNTER_PSCI_SYSTEM_SUSPEND_POWERED_DOWN.add(1);
        return Ok(());
    }

    match result {
        PSCI_SUCCESS => {
            COUNTER_PSCI_SYSTEM_SUSPEND_RETAINED.add(1);
            Ok(())
        }
        PSCI_INVALID_PARAMETERS => {
            COUNTER_PSCI_SYSTEM_SUSPEND_INVALID_PARAMETERS.add(1);
            Err(Status::INVALID_ARGS)
        }
        PSCI_DENIED => {
            COUNTER_PSCI_SYSTEM_SUSPEND_DENIED.add(1);
            Err(Status::ACCESS_DENIED)
        }
        PSCI_ALREADY_ON => {
            COUNTER_PSCI_SYSTEM_SUSPEND_ALREADY_ON.add(1);
            Err(Status::BAD_STATE)
        }
        // Any other result (e.g. PSCI_INVALID_ADDRESS, if `entry_pa` were bad) indicates a kernel
        // bug, so deliberately panic rather than return an error.
        _ => panic!(
            "cpu {}, psci_call_routine 0x{:x}, entry 0x{:x}, context {:p}, result {}",
            arch_curr_cpu_num(),
            (psci_call as *const ()).addr(),
            entry_pa,
            &context,
            result
        ),
    }
}

/// Returns the PSCI AFFINITY_INFO of the CPU whose MPIDR is `mpid`: 0 if it is on, 1 if it is
/// off, 2 if it is pending on, or a negative PSCI error.
fn psci_get_affinity_info(mpid: u64) -> i64 {
    // SAFETY: `PSCI64_AFFINITY_INFO` is a read-only PSCI query for `mpid` at lowest affinity level.
    unsafe { do_psci_call(PSCI64_AFFINITY_INFO, mpid, 0, 0) as i64 }
}

/// Returns the power state of the CPU whose MPIDR is `mpid`.
pub fn psci_get_cpu_state(mpid: u64) -> Result<PowerCpuState, Status> {
    let aff_info = psci_get_affinity_info(mpid);
    match aff_info {
        0 => Ok(PowerCpuState::On),
        1 => Ok(PowerCpuState::Off),
        2 => Ok(PowerCpuState::OnPending),
        _ => psci_status_to_zx_status(aff_info as u64).and(Err(Status::BAD_STATE)),
    }
}

/// The `PdevPowerOps::get_cpu_state` entry point for [`psci_get_cpu_state`].
extern "C" fn psci_get_cpu_state_op(
    mpid: u64,
    out_state: *mut PowerCpuState,
) -> Result<(), Status> {
    if out_state.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let state = psci_get_cpu_state(mpid)?;
    // SAFETY: `out_state` was checked non-null above, and `PdevPowerOps::get_cpu_state` callers
    // supply a pointer valid for writes of `PowerCpuState`.
    unsafe { out_state.write(state) };
    Ok(())
}

/// Used when calling SYSTEM_RESET2 directly.
pub fn psci_system_reset2_raw(reset_type: u32, cookie: u32) -> Result<(), Status> {
    dprintf!(INFO, "PSCI SYSTEM_RESET2: {:#x} {:#x}\n", reset_type, cookie);

    // SAFETY: `PSCI64_SYSTEM_RESET2` either resets the system without returning or returns a PSCI
    // error code.
    let psci_status =
        unsafe { do_psci_call(PSCI64_SYSTEM_RESET2, u64::from(reset_type), u64::from(cookie), 0) };

    dprintf!(INFO, "PSCI SYSTEM_RESET2 returns {}\n", psci_status as i64);

    psci_status_to_zx_status(psci_status)
}

/// Resets the system according to `flags`. Only returns if the call fails.
extern "C" fn psci_system_reset(flags: PowerRebootFlags) -> Result<(), Status> {
    let state = state();
    let [arg0, arg1, arg2] = match flags {
        PowerRebootFlags::Bootloader => state.reboot_bootloader_args,
        PowerRebootFlags::Recovery => state.reboot_recovery_args,
        _ => state.reboot_args,
    };

    let reset_command = state.reset_command;
    dprintf!(INFO, "PSCI reboot: {:#x} {:#x} {:#x} {:#x}\n", reset_command, arg0, arg1, arg2);
    // SAFETY: `reset_command` (`PSCI64_SYSTEM_RESET` or `PSCI64_SYSTEM_RESET2`) with ZBI-configured
    // reset arguments either resets the system without returning or returns a PSCI error code.
    psci_status_to_zx_status(unsafe { do_psci_call(reset_command, arg0, arg1, arg2) })
}

/// Performs a SYSTEM_RESET with no arguments. Only returns if the call fails.
pub fn psci_system_reset_cold() -> Result<(), Status> {
    // SAFETY: `PSCI64_SYSTEM_RESET` with zeroed arguments either resets the system without
    // returning or returns a PSCI error code.
    psci_status_to_zx_status(unsafe { do_psci_call(PSCI64_SYSTEM_RESET, 0, 0, 0) })
}

/// The PSCI CPU_SUSPEND coordination modes (see PSCI_SET_SUSPEND_MODE in DEN0022F.b).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsciSuspendMode {
    PlatformCoordinated = 0,
    OsInitiated = 1,
}

/// Tracks the active PSCI `CPU_SUSPEND` coordination mode (`PlatformCoordinated` until a
/// successful [`psci_set_suspend_mode`] call switches it). `Relaxed` ordering suffices because no
/// dependent state is published through this value.
struct ActiveSuspendMode(AtomicU32);

impl ActiveSuspendMode {
    const fn new() -> Self {
        Self(AtomicU32::new(PsciSuspendMode::PlatformCoordinated as u32))
    }

    fn set_with(
        &self,
        mode: PsciSuspendMode,
        do_call: impl FnOnce(u32, u64, u64, u64) -> u64,
    ) -> Result<(), Status> {
        let status =
            psci_status_to_zx_status(do_call(PSCI64_PSCI_SET_SUSPEND_MODE, mode as u64, 0, 0));
        if status.is_ok() {
            self.0.store(mode as u32, Ordering::Relaxed);
        }
        status
    }

    fn get(&self) -> PsciSuspendMode {
        if self.0.load(Ordering::Relaxed) == PsciSuspendMode::OsInitiated as u32 {
            PsciSuspendMode::OsInitiated
        } else {
            PsciSuspendMode::PlatformCoordinated
        }
    }
}

static ACTIVE_SUSPEND_MODE: ActiveSuspendMode = ActiveSuspendMode::new();

/// Sets the PSCI CPU_SUSPEND coordination mode (see PSCI_SET_SUSPEND_MODE in DEN0022F.b).
///
/// Returns `Ok(())` if the mode was set.
///
/// Returns `Status::NOT_SUPPORTED` if PSCI_SET_SUSPEND_MODE or the requested `mode` is not
/// supported.
///
/// Returns `Status::INVALID_ARGS` if `mode` is invalid.
///
/// Returns `Status::ACCESS_DENIED` if the mode cannot be switched in the current system state.
/// Switching modes requires every core to be either running or OFF (through CPU_OFF or never
/// booted), with no power domain in a low-power state.
///
/// Other PSCI results are converted by `psci_status_to_zx_status()` (e.g. PSCI_DISABLED ->
/// `Status::NOT_SUPPORTED`, unrecognized values -> `Status::BAD_STATE`).
pub fn psci_set_suspend_mode(mode: PsciSuspendMode) -> Result<(), Status> {
    ACTIVE_SUSPEND_MODE.set_with(mode, |func, arg0, arg1, arg2| {
        // SAFETY: `PSCI64_PSCI_SET_SUSPEND_MODE` configures firmware suspend coordination with a
        // valid `PsciSuspendMode` discriminant and returns a PSCI status code.
        unsafe { do_psci_call(func, arg0, arg1, arg2) }
    })
}

/// Returns the active PSCI `CPU_SUSPEND` coordination mode (`PlatformCoordinated` by default until
/// a successful [`psci_set_suspend_mode`] call changes it).
pub fn psci_get_suspend_mode() -> PsciSuspendMode {
    ACTIVE_SUSPEND_MODE.get()
}

/// Returns true iff PSCI version is 1.0 or better and SET_SUSPEND_MODE is supported.
pub fn psci_is_set_suspend_mode_supported() -> bool {
    state().psci_set_suspend_mode_supported
}

/// Returns true iff PSCI version is 1.0 or better, and CPU_SUSPEND is supported, and we have valid
/// ZBI-supplied power_state configuration.
pub fn psci_is_cpu_suspend_supported() -> bool {
    state().power_states.target_cpu.is_some()
}

/// Returns true iff PSCI version is 1.0 or better and SYSTEM_SUSPEND is supported.
pub fn psci_is_system_suspend_supported() -> bool {
    state().psci_system_suspend_supported
}

/// The power operations that [`psci_init`] registers with the pdev power driver.
static PSCI_OPS: PdevPowerOps = PdevPowerOps {
    reboot: Some(psci_system_reset),
    shutdown: Some(psci_system_off),
    cpu_off: Some(psci_cpu_off),
    cpu_on: Some(psci_cpu_on_op),
    get_cpu_state: Some(psci_get_cpu_state_op),
    opp_set: None,
    opp_get: None,
    opp_get_domain_count: None,
    system_resume: None,
};

/// Initializes the PSCI driver.
///
/// # Safety
///
/// Must be called at most once, on the boot CPU with interrupts disabled, before any secondary CPU
/// is started.
unsafe fn psci_init(
    config: &DcfgArmPsciDriver,
    psci_cpu_suspend_config: &[DcfgArmPsciCpuSuspendState],
) {
    debug_assert!(ints_disabled());

    // Satisfies `PsciState::do_psci_call`'s safety invariant: `psci_hvc_call` is selected only
    // when `arm64_get_boot_el() < 2` (`CurrentEL.EL == 1`).
    let do_call: PsciCallProc =
        if config.use_hvc != 0 && arm64_get_boot_el() < 2 { psci_hvc_call } else { psci_smc_call };

    let mut init_state = PsciState {
        shutdown_args: config.shutdown_args,
        reboot_args: config.reboot_args,
        reboot_bootloader_args: config.reboot_bootloader_args,
        reboot_recovery_args: config.reboot_recovery_args,
        do_psci_call: do_call,
        ..PsciState::DEFAULT
    };
    let mut retained_states = RetainedCpuSuspendStates::EMPTY;

    // read information about the psci implementation
    // SAFETY: `do_call` is `psci_hvc_call` only when `CurrentEL.EL == 1`, and `PSCI64_PSCI_VERSION`
    // is a read-only PSCI query taking no arguments.
    let result = unsafe { do_call(PSCI64_PSCI_VERSION, 0, 0, 0) as u32 };
    let major = (result >> 16) & 0xffff;
    let minor = result & 0xffff;
    dprintf!(INFO, "PSCI version {}.{}\n", major, minor);

    if major >= 1 && major != 0xffff {
        // query features
        dprintf!(INFO, "PSCI supported features:\n");

        // Prints info about the features and returns supported flags or `None` if not supported.
        let probe_feature = |feature: u32, feature_name: &str| -> Option<u32> {
            // SAFETY: `do_call` is `psci_hvc_call` only when `CurrentEL.EL == 1`, and
            // `PSCI64_PSCI_FEATURES` is a read-only PSCI feature query.
            let result = unsafe { do_call(PSCI64_PSCI_FEATURES, u64::from(feature), 0, 0) as u32 };
            if (result as i32) < 0 {
                // Not supported
                return None;
            }
            dprintf!(INFO, "\t{} (0x{:x})\n", feature_name, result);
            Some(result)
        };

        let cpu_suspend_result = probe_feature(PSCI64_CPU_SUSPEND, "CPU_SUSPEND");
        probe_feature(PSCI64_CPU_OFF, "CPU_OFF");
        probe_feature(PSCI64_CPU_ON, "CPU_ON");
        probe_feature(PSCI64_AFFINITY_INFO, "AFFINITY_INFO");
        probe_feature(PSCI64_MIGRATE, "MIGRATE");
        probe_feature(PSCI64_MIGRATE_INFO_TYPE, "MIGRATE_INFO_TYPE");
        probe_feature(PSCI64_MIGRATE_INFO_UP_CPU, "MIGRATE_INFO_UP_CPU");
        probe_feature(PSCI64_SYSTEM_OFF, "SYSTEM_OFF");
        probe_feature(PSCI64_SYSTEM_RESET, "SYSTEM_RESET");
        if probe_feature(PSCI64_SYSTEM_RESET2, "SYSTEM_RESET2").is_some() {
            // Prefer RESET2 if present. It explicitly supports arguments, but some vendors have
            // extended RESET to behave the same way.
            init_state.reset_command = PSCI64_SYSTEM_RESET2;
        }
        probe_feature(PSCI64_CPU_FREEZE, "CPU_FREEZE");
        probe_feature(PSCI64_CPU_DEFAULT_SUSPEND, "CPU_DEFAULT_SUSPEND");
        probe_feature(PSCI64_NODE_HW_STATE, "NODE_HW_STATE");
        init_state.psci_system_suspend_supported =
            probe_feature(PSCI64_SYSTEM_SUSPEND, "SYSTEM_SUSPEND").is_some();
        init_state.psci_set_suspend_mode_supported =
            probe_feature(PSCI64_PSCI_SET_SUSPEND_MODE, "PSCI_SET_SUSPEND_MODE").is_some();
        probe_feature(PSCI64_PSCI_STAT_RESIDENCY, "PSCI_STAT_RESIDENCY");
        probe_feature(PSCI64_PSCI_STAT_COUNT, "PSCI_STAT_COUNT");
        probe_feature(PSCI64_MEM_PROTECT, "MEM_PROTECT");
        probe_feature(PSCI64_MEM_PROTECT_RANGE, "MEM_PROTECT_RANGE");

        probe_feature(PSCI64_SMCCC_VERSION, "PSCI64_SMCCC_VERSION");

        // Print the power_state format and power_states after printing all the supported features.
        if let Some(options) = cpu_suspend_result {
            // Determine and set the power_state_format.
            // If bit-1 is zero, then it's original format.
            let power_state_format = if (options & (1 << 1)) == 0 {
                PowerStateFormat::Original
            } else {
                PowerStateFormat::Extended
            };
            init_state.power_state_format = power_state_format;
            init_state.power_states = parse_cpu_suspend_power_states(
                power_state_format,
                psci_cpu_suspend_config,
                &mut retained_states,
            );
        }
    }

    // SAFETY: The caller guarantees that `psci_init` runs at most once on the boot CPU with
    // interrupts disabled before any secondary CPU starts, and neither `PSCI_STATE` nor
    // `RETAINED_SUSPEND_STATES` is accessed during `psci_init` (and no `&mut` reference to either
    // static is ever formed). Thus nothing else accesses either static concurrently.
    unsafe {
        *PSCI_STATE.as_ptr() = init_state;
        *RETAINED_SUSPEND_STATES.as_ptr() = retained_states;
    }

    // Register with the pdev power driver.
    pdev_register_power(&PSCI_OPS);
}

/// The `PsciInit` entry point of `<dev/psci.h>`: initializes the PSCI driver with `config` and the
/// `psci_cpu_suspend_config_count` entries at `psci_cpu_suspend_config`.
///
/// # Safety
///
/// If `psci_cpu_suspend_config_count` is non-zero, `psci_cpu_suspend_config` must be non-null,
/// aligned, and point to `psci_cpu_suspend_config_count` consecutive initialized entries in one
/// allocation, valid for reads for the duration of the call. The caller must also meet the
/// requirements of [`psci_init`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_psci_init(
    config: &DcfgArmPsciDriver,
    psci_cpu_suspend_config: *const DcfgArmPsciCpuSuspendState,
    psci_cpu_suspend_config_count: usize,
) {
    // SAFETY: The caller guarantees that, if `psci_cpu_suspend_config_count` is non-zero,
    // `psci_cpu_suspend_config` is non-null, aligned, and points to that many initialized entries
    // in one allocation that outlive the call.
    let psci_cpu_suspend_config =
        unsafe { zr::slice_from_raw_parts(psci_cpu_suspend_config, psci_cpu_suspend_config_count) };
    // SAFETY: The caller meets the requirements of `psci_init`.
    unsafe { psci_init(config, psci_cpu_suspend_config) };
}

#[cfg(console_enabled)]
mod console {
    use super::*;
    use crate::arch_rs::arm64::{arm64_mpid, arm64_secondary_start_paddr};
    use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
    use core::ffi::c_int;
    use kprint::kprint;

    /// # Safety
    ///
    /// If `argc > 0`, `argv` must point to `argc` initialized `CmdArgs` whose `arg_str` fields are
    /// null or point to valid NUL-terminated strings.
    unsafe extern "C" fn cmd_psci(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argc > 0` and `argv` is non-null (checked above), and the caller guarantees that
        // `argv` points to `argc` initialized `CmdArgs`.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let notenoughargs = || -> c_int {
            kprint!("not enough arguments\n");
            kprint!("{:s} system_reset\n", argv[0].as_str());
            kprint!("{:s} system_off\n", argv[0].as_str());
            kprint!("{:s} cpu_on <mpidr>\n", argv[0].as_str());
            kprint!("{:s} affinity_info <cluster> <cpu>\n", argv[0].as_str());
            kprint!("{:s} <function_id> [arg0] [arg1] [arg2]\n", argv[0].as_str());
            -1
        };

        if argc < 2 {
            return notenoughargs();
        }

        if argv[1].as_str() == "system_reset" {
            let _ = psci_system_reset(PowerRebootFlags::Normal);
        } else if argv[1].as_str() == "system_off" {
            let _ = psci_system_off();
        } else if argv[1].as_str() == "cpu_on" {
            if argc < 3 {
                return notenoughargs();
            }

            let secondary_entry_paddr = u64::from(arm64_secondary_start_paddr());
            // SAFETY: `cmd_psci` is an operator-invoked debug console command;
            // `secondary_entry_paddr` relies on `arm64_secondary_start_paddr`'s safety-usable
            // invariant.
            let ret = Status::result_into_raw(unsafe {
                psci_cpu_on(argv[2].arg_uint, secondary_entry_paddr, 0)
            }) as u32;
            kprint!("psci_cpu_on returns {:u}\n", ret);
        } else if argv[1].as_str() == "affinity_info" {
            if argc < 4 {
                return notenoughargs();
            }

            let ret = psci_get_affinity_info(arm64_mpid(argv[2].arg_uint, argv[3].arg_uint));
            kprint!("affinity info returns {}\n", ret);
        } else {
            let function = argv[1].arg_uint as u32;
            let arg0 = if argc >= 3 { argv[2].arg_uint } else { 0 };
            let arg1 = if argc >= 4 { argv[3].arg_uint } else { 0 };
            let arg2 = if argc >= 5 { argv[4].arg_uint } else { 0 };

            // SAFETY: `cmd_psci` is an operator-invoked debug console command that explicitly
            // permits issuing raw PSCI calls.
            let ret = unsafe { do_psci_call(function, arg0, arg1, arg2) };
            kprint!("do_psci_call returned {:u}\n", ret);
        }
        0
    }

    static_command!(
        CMD_PSCI,
        c"psci".as_ptr(),
        c"execute PSCI command".as_ptr(),
        cmd_psci,
        CMD_AVAIL_ALWAYS
    );
}

/// In-kernel unit tests for PSCI `CPU_SUSPEND` state parsing, retention, and query helpers.
#[cfg(ktest)]
#[unittest::suite(name = "psci_tests")]
mod tests {
    use super::{
        ActiveSuspendMode, ArmPsciCpuSuspendStateFlags, DcfgArmPsciCpuSuspendState,
        MAX_CPU_SUSPEND_STATES, PSCI_DENIED, PSCI_NOT_SUPPORTED, PSCI_SUCCESS,
        PSCI64_PSCI_SET_SUSPEND_MODE, PowerStateFormat, PsciCpuSuspendState, PsciSuspendMode,
        RetainedCpuSuspendStates, Status, cpu_mask_for_count, parse_cpu_suspend_power_states,
        psci_get_cpu_suspend_states, psci_get_suspend_mode, psci_is_cpu_suspend_supported,
        psci_is_set_suspend_mode_supported, system_cpu_mask,
    };
    use unittest::{assert_eq, assert_err, assert_false, assert_ok, assert_true, unwrap_some};

    const CANARY_STATE: PsciCpuSuspendState = PsciCpuSuspendState {
        power_state: 0xcafe_babe,
        flags: ArmPsciCpuSuspendStateFlags::empty(),
        is_powerdown: true,
        cpu_mask_is_fallback: true,
        entry_latency_us: 999,
        exit_latency_us: 999,
        min_residency_us: 999,
        cpu_mask: 0xffff_ffff_ffff_ffff,
    };

    const fn make_zbi_state(
        id: u32,
        power_state: u32,
        flags: u32,
        entry_latency_us: u32,
        exit_latency_us: u32,
        min_residency_us: u32,
        cpu_mask: u64,
    ) -> DcfgArmPsciCpuSuspendState {
        DcfgArmPsciCpuSuspendState {
            id,
            power_state,
            flags: ArmPsciCpuSuspendStateFlags::from_bits_retain(flags),
            entry_latency_us,
            exit_latency_us,
            min_residency_us,
            cpu_mask,
        }
    }

    /// Tests that `get_states` returns `0` when no usable per-CPU state was retained.
    #[test]
    fn test_returns_zero_without_usable_cpu_targeted_state() {
        let mut retained = RetainedCpuSuspendStates::EMPTY;
        let mut out = [CANARY_STATE; 2];

        // Domain-targeted and unknown-flag entries only (no usable per-CPU entry).
        const DOMAIN_AND_UNKNOWN: [DcfgArmPsciCpuSuspendState; 2] = [
            make_zbi_state(1, 0x4001_0010, 0x2, 100, 200, 1_000, 0x0f),
            make_zbi_state(2, 0x4000_0002, 0x80, 10, 20, 50, 0x0f),
        ];
        let power_states = parse_cpu_suspend_power_states(
            PowerStateFormat::Extended,
            &DOMAIN_AND_UNKNOWN,
            &mut retained,
        );
        assert_true!(power_states.target_cpu.is_none());
        assert_eq!(retained.get_states(0xff, &mut out), 0);
        assert_true!(out[0] == CANARY_STATE);

        // Sixteen domain-targeted entries fill the retention buffer and push the only per-CPU
        // entry out to index 16: legacy `target_cpu` still sees it, while `get_states` returns 0
        // because no per-CPU state fit in the retention table.
        const DOMAIN_THEN_CPU: [DcfgArmPsciCpuSuspendState; MAX_CPU_SUSPEND_STATES + 1] = {
            let mut arr = [make_zbi_state(1, 0x4001_0010, 0x2, 100, 200, 1_000, 0x0f);
                MAX_CPU_SUSPEND_STATES + 1];
            arr[MAX_CPU_SUSPEND_STATES] = make_zbi_state(17, 0x4000_0002, 0x1, 10, 20, 50, 0x0f);
            arr
        };
        let power_states = parse_cpu_suspend_power_states(
            PowerStateFormat::Extended,
            &DOMAIN_THEN_CPU,
            &mut retained,
        );
        assert_true!(power_states.target_cpu.is_some());
        assert_eq!(retained.get_states(0xff, &mut out), 0);
        assert_true!(out[0] == CANARY_STATE);

        // Populate `retained` with a valid per-CPU state, then verify an empty config resets it.
        const ONE_CPU_STATE: [DcfgArmPsciCpuSuspendState; 1] =
            [make_zbi_state(1, 0x4000_0002, 0x1, 10, 20, 50, 0x0f)];
        let _ = parse_cpu_suspend_power_states(
            PowerStateFormat::Extended,
            &ONE_CPU_STATE,
            &mut retained,
        );
        assert_eq!(retained.get_states(0xff, &mut out), 1);

        let power_states =
            parse_cpu_suspend_power_states(PowerStateFormat::Extended, &[], &mut retained);
        assert_true!(power_states.target_cpu.is_none());
        assert_true!(power_states.target_power_domain.is_none());
        assert_eq!(retained.get_states(0xff, &mut out), 0);
    }

    /// Tests unknown-flag filtering, partial slices, 64-bit clamping, and fallback mask detection.
    #[test]
    fn test_skips_unknown_flags_clamps_masks_and_flags_fallback() {
        const CONFIG: [DcfgArmPsciCpuSuspendState; 4] = [
            // Boot-shim all-ones fallback (`u64::MAX`): flagged before clamping.
            make_zbi_state(1, 0x4000_0002, 0x1, 50, 100, 500, u64::MAX),
            // Unknown flag bit `0x4`: skipped.
            make_zbi_state(2, 0x4000_0009, 0x5, 1, 1, 1, 0x0f),
            // Explicit system mask (`0xff != u64::MAX`): not flagged as fallback.
            make_zbi_state(3, 0x4001_0010, 0x3, 200, 500, 2_000, 0xff),
            // Explicit cluster mask (`0x3c`).
            make_zbi_state(4, 0x0000_0001, 0x0, 10, 20, 60, 0x3c),
        ];

        let mut retained = RetainedCpuSuspendStates::EMPTY;
        let power_states =
            parse_cpu_suspend_power_states(PowerStateFormat::Extended, &CONFIG, &mut retained);
        assert_eq!(unwrap_some!(power_states.target_cpu).value, 0x0000_0001);
        assert_eq!(unwrap_some!(power_states.target_power_domain).value, 0x4001_0010);

        // Partial output slice writes only `partial[0..2]` while returning total count `3`,
        // leaving `partial[2]` untouched.
        let mut partial = [CANARY_STATE; 3];
        assert_eq!(retained.get_states(0x0f, &mut partial[..2]), 3);
        assert_eq!(partial[0].power_state, 0x4000_0002);
        assert_eq!(partial[0].cpu_mask, 0x0f);
        assert_true!(partial[0].cpu_mask_is_fallback);
        assert_eq!(partial[1].power_state, 0x4001_0010);
        assert_eq!(partial[1].cpu_mask, 0x0f);
        assert_false!(partial[1].cpu_mask_is_fallback);
        assert_true!(partial[2] == CANARY_STATE);

        // Full query with 4-CPU mask `0x0f` clamps `0x3c` to `0x0c` and leaves trailing canary.
        let mut full = [CANARY_STATE; 4];
        assert_eq!(retained.get_states(0x0f, &mut full), 3);
        assert_eq!(full[0].power_state, 0x4000_0002);
        assert_eq!(full[0].flags.bits(), 0x1);
        assert_true!(full[0].is_powerdown);
        assert_true!(full[0].cpu_mask_is_fallback);
        assert_eq!(full[0].entry_latency_us, 50);
        assert_eq!(full[0].exit_latency_us, 100);
        assert_eq!(full[0].min_residency_us, 500);
        assert_eq!(full[0].cpu_mask, 0x0f);
        assert_eq!(full[1].power_state, 0x4001_0010);
        assert_eq!(full[1].flags.bits(), 0x3);
        assert_true!(full[1].is_powerdown);
        assert_false!(full[1].cpu_mask_is_fallback);
        assert_eq!(full[1].entry_latency_us, 200);
        assert_eq!(full[1].exit_latency_us, 500);
        assert_eq!(full[1].min_residency_us, 2_000);
        assert_eq!(full[1].cpu_mask, 0x0f);
        assert_eq!(full[2].power_state, 0x0000_0001);
        assert_eq!(full[2].flags.bits(), 0x0);
        assert_false!(full[2].is_powerdown);
        assert_false!(full[2].cpu_mask_is_fallback);
        assert_eq!(full[2].entry_latency_us, 10);
        assert_eq!(full[2].exit_latency_us, 20);
        assert_eq!(full[2].min_residency_us, 60);
        assert_eq!(full[2].cpu_mask, 0x0c);
        assert_true!(full[3] == CANARY_STATE);

        // Verify 64-bit `all_cpus` masks above bit 31 are preserved without 32-bit narrowing.
        let mut wide = [CANARY_STATE; 1];
        assert_eq!(retained.get_states(0x8000_0000_0000_000f, &mut wide), 3);
        assert_eq!(wide[0].cpu_mask, 0x8000_0000_0000_000f);
        assert_true!(wide[0].cpu_mask_is_fallback);
    }

    /// Tests `is_powerdown` classification under both Original and Extended `power_state` formats.
    #[test]
    fn test_classifies_powerdown_bit_by_format() {
        let mut retained = RetainedCpuSuspendStates::EMPTY;
        let mut out = [CANARY_STATE; 2];

        // Original-format bit 16 set while Extended-format bit 30 is clear: retention in Extended,
        // power-down in Original.
        const BIT16_ONLY: [DcfgArmPsciCpuSuspendState; 1] =
            [make_zbi_state(1, 0x0001_0001, 0, 10, 20, 50, 0x0f)];
        let _ =
            parse_cpu_suspend_power_states(PowerStateFormat::Extended, &BIT16_ONLY, &mut retained);
        assert_eq!(retained.get_states(0x0f, &mut out), 1);
        assert_false!(out[0].is_powerdown);

        let _ =
            parse_cpu_suspend_power_states(PowerStateFormat::Original, &BIT16_ONLY, &mut retained);
        assert_eq!(retained.get_states(0x0f, &mut out), 1);
        assert_true!(out[0].is_powerdown);

        // Extended-format bit 30 set while Original-format bit 16 is clear: retention in Original,
        // power-down in Extended.
        const BIT30_ONLY: [DcfgArmPsciCpuSuspendState; 1] =
            [make_zbi_state(1, 0x4000_0001, 0, 10, 20, 50, 0x0f)];
        let _ =
            parse_cpu_suspend_power_states(PowerStateFormat::Original, &BIT30_ONLY, &mut retained);
        assert_eq!(retained.get_states(0x0f, &mut out), 1);
        assert_false!(out[0].is_powerdown);

        let _ =
            parse_cpu_suspend_power_states(PowerStateFormat::Extended, &BIT30_ONLY, &mut retained);
        assert_eq!(retained.get_states(0x0f, &mut out), 1);
        assert_true!(out[0].is_powerdown);
    }

    /// Tests that retained PSCI states cap at `MAX_CPU_SUSPEND_STATES`.
    #[test]
    fn test_caps_retained_states_at_max_capacity() {
        const MANY_STATES: [DcfgArmPsciCpuSuspendState; MAX_CPU_SUSPEND_STATES + 2] = {
            let mut arr = [make_zbi_state(0, 0, 0, 10, 20, 50, 0xff); MAX_CPU_SUSPEND_STATES + 2];
            let mut i = 0;
            while i < arr.len() {
                let id = (i + 1) as u32;
                let powerdown_bit = if i % 2 == 1 { 1u32 << 16 } else { 0 };
                arr[i] = make_zbi_state(id, powerdown_bit | id, 0, 10, 20, 50, 0xff);
                i += 1;
            }
            arr
        };

        let mut retained = RetainedCpuSuspendStates::EMPTY;
        let _ =
            parse_cpu_suspend_power_states(PowerStateFormat::Original, &MANY_STATES, &mut retained);

        let mut out = [PsciCpuSuspendState::EMPTY; MAX_CPU_SUSPEND_STATES];
        assert_eq!(retained.get_states(0x3f, &mut out), MAX_CPU_SUSPEND_STATES);
        for (i, state) in out.iter().enumerate() {
            let id = (i + 1) as u32;
            let expected_power_state = if i % 2 == 1 { (1u32 << 16) | id } else { id };
            assert_eq!(state.power_state, expected_power_state);
            assert_eq!(state.is_powerdown, i % 2 == 1);
            assert_false!(state.cpu_mask_is_fallback);
            assert_eq!(state.cpu_mask, 0x3f);
        }
    }

    /// Tests live PSCI state querying, CPU-mask clamping against `arch_max_num_cpus`, and mode.
    #[test]
    fn test_queries_live_states_and_clamps_to_present_cpus() {
        assert_eq!(cpu_mask_for_count(0), 0);
        assert_eq!(cpu_mask_for_count(1), 0x1);
        assert_eq!(cpu_mask_for_count(8), 0xff);
        assert_eq!(cpu_mask_for_count(63), 0x7fff_ffff_ffff_ffff);
        assert_eq!(cpu_mask_for_count(64), u64::MAX);
        assert_eq!(cpu_mask_for_count(65), u64::MAX);

        let max_cpus = super::arch_max_num_cpus();
        assert_true!(max_cpus > 0);
        assert_eq!(system_cpu_mask().count_ones(), max_cpus.min(u64::BITS));

        let mut out = [CANARY_STATE; MAX_CPU_SUSPEND_STATES];
        let live_count = psci_get_cpu_suspend_states(&mut out);
        if !psci_is_cpu_suspend_supported() {
            assert_eq!(live_count, 0);
        }
        let written = live_count.min(out.len());
        for state in &out[..written] {
            if max_cpus < u64::BITS {
                assert_eq!(state.cpu_mask >> max_cpus, 0);
            }
        }
        if written < out.len() {
            assert_true!(out[written] == CANARY_STATE);
        }
        if !psci_is_cpu_suspend_supported() || !psci_is_set_suspend_mode_supported() {
            assert_eq!(psci_get_suspend_mode(), PsciSuspendMode::PlatformCoordinated);
        }
    }

    /// Tests suspend-mode tracking across firmware success and rejection responses.
    #[test]
    fn test_tracks_active_suspend_mode_transitions() {
        let tracker = ActiveSuspendMode::new();
        let mut last_call = (0u32, u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(tracker.get(), PsciSuspendMode::PlatformCoordinated);

        // A denied switch to OS-initiated mode leaves `PlatformCoordinated` active.
        let denied = tracker.set_with(PsciSuspendMode::OsInitiated, |func, arg0, arg1, arg2| {
            last_call = (func, arg0, arg1, arg2);
            PSCI_DENIED as u64
        });
        assert_err!(denied, Status::ACCESS_DENIED);
        assert_eq!(last_call.0, PSCI64_PSCI_SET_SUSPEND_MODE);
        assert_eq!(last_call.1, 1);
        assert_eq!(last_call.2, 0);
        assert_eq!(last_call.3, 0);
        assert_eq!(tracker.get(), PsciSuspendMode::PlatformCoordinated);

        // A successful switch updates the tracked mode to `OsInitiated`.
        assert_ok!(tracker.set_with(PsciSuspendMode::OsInitiated, |func, arg0, arg1, arg2| {
            last_call = (func, arg0, arg1, arg2);
            PSCI_SUCCESS as u64
        }));
        assert_eq!(last_call.0, PSCI64_PSCI_SET_SUSPEND_MODE);
        assert_eq!(last_call.1, 1);
        assert_eq!(tracker.get(), PsciSuspendMode::OsInitiated);

        // A failed switch back to `PlatformCoordinated` preserves `OsInitiated`.
        let unsupported =
            tracker.set_with(PsciSuspendMode::PlatformCoordinated, |func, arg0, arg1, arg2| {
                last_call = (func, arg0, arg1, arg2);
                PSCI_NOT_SUPPORTED as u64
            });
        assert_err!(unsupported, Status::NOT_SUPPORTED);
        assert_eq!(last_call.0, PSCI64_PSCI_SET_SUSPEND_MODE);
        assert_eq!(last_call.1, 0);
        assert_eq!(tracker.get(), PsciSuspendMode::OsInitiated);

        // A successful switch back restores `PlatformCoordinated`.
        assert_ok!(tracker.set_with(
            PsciSuspendMode::PlatformCoordinated,
            |func, arg0, arg1, arg2| {
                last_call = (func, arg0, arg1, arg2);
                PSCI_SUCCESS as u64
            }
        ));
        assert_eq!(last_call.0, PSCI64_PSCI_SET_SUSPEND_MODE);
        assert_eq!(last_call.1, 0);
        assert_eq!(last_call.2, 0);
        assert_eq!(last_call.3, 0);
        assert_eq!(tracker.get(), PsciSuspendMode::PlatformCoordinated);
    }
}
