// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! CPU energy model, active power levels, idle power states, transition costs, and power domains.

use bitflags::bitflags;
use core::mem::{align_of, offset_of, size_of};
use core::ops;
use core::sync::atomic::{AtomicI64, Ordering};

use fbl::{RefPtr, Vector};
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::{
    ZX_MAX_NAME_LEN, ZX_PROCESSOR_POWER_CONTROL_ARM_PSCI, ZX_PROCESSOR_POWER_CONTROL_ARM_WFI,
    ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER, ZX_PROCESSOR_POWER_CONTROL_RISCV_SBI,
    ZX_PROCESSOR_POWER_CONTROL_RISCV_WFI, ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
    ZX_TIME_INFINITE, zx_duration_mono_t,
};

#[cfg(test)]
use super::energy_model_ffi::cpp_power_domain_create_for_test;
use super::energy_model_ffi::{
    cpp_power_domain_controller, cpp_power_domain_cpus, cpp_power_domain_id,
    cpp_power_domain_model, cpp_power_domain_recycle, cpp_power_domain_scheduler_control_enabled,
    cpp_power_domain_set_scheduler_control_enabled,
    cpp_power_domain_total_normalized_utilization_ptr,
    cpp_power_level_controller_control_interface,
    cpp_power_level_controller_get_current_power_level, cpp_power_level_controller_is_serving,
};
use super::{ControlInterface, ProcessingRate, Utilization};
#[cfg(not(feature = "userspace"))]
use {
    super::energy_model_ffi::{cpp_power_domain_create, cpp_power_management_set_power_domain_set},
    super::pdev_power_level_controller_ffi::cpp_pdev_power_level_controller_get,
    super::power_level_controller::to_string as control_interface_to_string,
    crate::kernel::mp::SMP_MAX_CPUS,
    crate::kernel::percpu::PerCpu,
    crate::kernel::scheduler::Scheduler,
    crate::kernel::types::{INVALID_CPU, cpu_mask_t, cpu_num_to_mask, remove_cpu_from_mask},
    debug::dprintf,
    fbl::Array,
    kprint::kprintln,
    zx_types::{ZX_CPU_PERF_LIMIT_TYPE_RATE, zx_cpu_perf_limit_t},
};

/// Power level option bit indicating that an active power level may be transitioned on a per-CPU
/// basis without affecting other CPUs in the same power domain.
pub const POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT: u64 =
    ZX_PROCESSOR_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT;

/// Maximum number of power domains supported in a [`PowerDomainSet`].
const MAX_POWER_DOMAINS: usize = 4;

impl TryFrom<u64> for ControlInterface {
    type Error = Status;

    #[inline]
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            ZX_PROCESSOR_POWER_CONTROL_CPU_DRIVER => Ok(Self::CpuDriver),
            ZX_PROCESSOR_POWER_CONTROL_ARM_PSCI => Ok(Self::ArmPsci),
            ZX_PROCESSOR_POWER_CONTROL_ARM_WFI => Ok(Self::ArmWfi),
            ZX_PROCESSOR_POWER_CONTROL_RISCV_SBI => Ok(Self::RiscvSbi),
            ZX_PROCESSOR_POWER_CONTROL_RISCV_WFI => Ok(Self::RiscvWfi),
            _ => Err(Status::INVALID_ARGS),
        }
    }
}

/// Copies up to `ZX_MAX_NAME_LEN - 1` bytes up to the first NUL byte from `src`.
fn copy_str_name(src: &str) -> ([u8; ZX_MAX_NAME_LEN], usize) {
    let bytes = src.as_bytes();
    let max_len = bytes.len().min(ZX_MAX_NAME_LEN - 1);
    let name_len = bytes[..max_len].iter().position(|&b| b == 0).unwrap_or(max_len);
    let mut name = [0u8; ZX_MAX_NAME_LEN];
    name[..name_len].copy_from_slice(&bytes[..name_len]);
    (name, name_len)
}

/// Steady-state power model for a processor idle state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdlePowerModel {
    /// Core clock is gated while the domain rail stays at the current active OPP.
    ClockGated,
    /// Core or cluster power switch opens; steady-state power is independent of active OPP.
    PowerGated {
        /// Steady-state residual power in nanowatts consumed while power-gated (`0` when fully
        /// gated or uncharacterized).
        residual_power_nw: u64,
    },
}

bitflags! {
    /// Flags describing architectural properties of an [`IdlePowerState`].
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct IdleStateFlags: u32 {
        /// The per-CPU architectural timer stops while the core is in this idle state.
        const LOCAL_TIMER_STOPS = 1 << 0;
    }
}

/// Input descriptor for constructing an active-to-active [`PowerLevelTransition`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessorPowerLevelTransition {
    /// Time required for the transition to take effect.
    pub latency: zx_duration_mono_t,
    /// Energy consumed in nanojoules to perform the transition.
    pub energy_nj: u64,
    /// Source active power level index.
    pub from: u8,
    /// Target active power level index.
    pub to: u8,
}

/// Configuration descriptor for an active processor OPP when registering a [`PowerDomain`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ActivePowerLevelConfig<'a> {
    /// Power level options (e.g. [`POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT`]).
    pub options: u64,
    /// Non-zero processing rate associated with this active OPP.
    pub processing_rate: u64,
    /// Active power consumption in nanowatts while executing at this OPP.
    pub active_power_nw: u64,
    /// Steady-state static leakage power in nanowatts while clock-gated at this OPP's rail voltage.
    pub static_power_nw: u64,
    /// Control interface handling transitions to this active OPP.
    pub control_interface: ControlInterface,
    /// Control argument passed to the control interface.
    pub control_argument: u64,
    /// Diagnostic name string for debugging/inspect (truncated to `ZX_MAX_NAME_LEN - 1` bytes).
    pub diagnostic_name: &'a str,
}

/// Configuration descriptor for a processor idle state when registering a [`PowerDomain`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IdlePowerStateConfig<'a> {
    /// Steady-state idle power model (`ClockGated` vs. `PowerGated`).
    pub power_model: IdlePowerModel,
    /// Worst-case latency in nanoseconds to enter the idle state.
    pub entry_latency_ns: zx_duration_mono_t,
    /// Worst-case latency in nanoseconds to exit the idle state back to execution.
    pub exit_latency_ns: zx_duration_mono_t,
    /// Minimum idle duration in nanoseconds required to break even on entry and exit energy.
    pub min_residency_ns: zx_duration_mono_t,
    /// Bitmask of CPUs coordinated by this idle state (`0` when scoped solely to the calling CPU).
    pub scope_cpu_mask: u64,
    /// Architectural flags for this idle state.
    pub flags: IdleStateFlags,
    /// Control interface handling entry into this idle state.
    pub control_interface: ControlInterface,
    /// Control argument passed to the control interface.
    pub control_argument: u64,
    /// Diagnostic name string for debugging/inspect (truncated to `ZX_MAX_NAME_LEN - 1` bytes).
    pub diagnostic_name: &'a str,
}

impl IdlePowerStateConfig<'static> {
    /// Standard clock-gated ARM WFI idle state descriptor.
    pub const ARM_WFI: Self = Self {
        power_model: IdlePowerModel::ClockGated,
        entry_latency_ns: 0,
        exit_latency_ns: 0,
        min_residency_ns: 0,
        scope_cpu_mask: 0,
        flags: IdleStateFlags::empty(),
        control_interface: ControlInterface::ArmWfi,
        control_argument: 0,
        diagnostic_name: "WFI",
    };
}

/// Configuration descriptor for registering a processor [`PowerDomain`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerDomainConfig<'a> {
    /// Domain identifier.
    pub domain_id: u32,
    /// Bitmask of CPU core IDs belonging to this power domain.
    pub cpu_mask: u64,
    /// Supported active OPPs for this domain (must be non-empty).
    pub active_levels: &'a [ActivePowerLevelConfig<'a>],
    /// Supported idle states for this domain (may be empty).
    pub idle_states: &'a [IdlePowerStateConfig<'a>],
    /// Active-to-active power level transitions for this domain, indexed by positions in
    /// `active_levels`. When empty, all active transitions default to zero cost. When non-empty,
    /// any `(from, to)` pair not listed is treated as an invalid transition.
    pub active_transitions: &'a [ProcessorPowerLevelTransition],
}

/// Kernel representation of an active processor Operating Performance Point (OPP).
#[derive(Copy, Clone, Debug)]
pub struct ActivePowerLevel {
    /// Options (e.g. [`POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT`]).
    options: u64,
    /// Control interface used to transition to this active level.
    control: ControlInterface,
    /// Argument to be provided to the control interface.
    control_argument: u64,
    /// Normalized processing rate when operating at this active level.
    processing_rate: ProcessingRate,
    /// Active power consumption in nanowatts while executing at this OPP.
    active_power_nw: u64,
    /// Steady-state static leakage power in nanowatts while clock-gated at this OPP's rail voltage.
    static_power_nw: u64,
    /// Memoized energy efficiency cost `active_power_nw / processing_rate`.
    power_cost_nw_per_rate: u64,
    /// Diagnostic name.
    name: [u8; ZX_MAX_NAME_LEN],
    /// Length of the diagnostic name.
    name_len: usize,
    /// Sorted 0-based index of this level in its owning [`EnergyModel::active_levels`] slice.
    level: u8,
}

impl ActivePowerLevel {
    /// Scale for processing rates communicated to/from the userspace limits interface.
    pub const USER_PROCESSING_RATE_SCALE: u64 = 1000;

    /// Normalizes an integer processing rate relative to the maximum rate.
    #[inline]
    pub fn to_processing_rate(processing_rate: u64, max_processing_rate: u64) -> ProcessingRate {
        if max_processing_rate == 0 {
            core::hint::cold_path();
            return ProcessingRate::from_raw(0);
        }
        let quotient = ffl::FastFixed::<u64, 31>::from(
            ffl::FastFixed::<u64, 0>::from_raw(processing_rate)
                / ffl::FastFixed::<u64, 0>::from_raw(max_processing_rate),
        );
        ProcessingRate::from_raw(quotient.raw_value().min(i64::MAX as u64) as i64)
    }

    /// Converts a normalized `ProcessingRate` back to an integer rate relative to the maximum rate.
    #[inline]
    pub fn from_processing_rate(processing_rate: ProcessingRate, max_processing_rate: u64) -> u64 {
        let rate = ffl::FastFixed::<u64, 31>::from_raw(processing_rate.raw_value().max(0) as u64);
        ffl::FastFixed::<u64, 0>::from(
            rate * ffl::FastFixed::<u64, 0>::from_raw(max_processing_rate),
        )
        .raw_value()
    }

    /// Constructs an `ActivePowerLevel` from an [`ActivePowerLevelConfig`] descriptor.
    pub fn new(
        level_index: u8,
        config: &ActivePowerLevelConfig<'_>,
        max_processing_rate: u64,
    ) -> Self {
        let rate_scale =
            if max_processing_rate > 0 { max_processing_rate } else { config.processing_rate };
        let processing_rate = if config.processing_rate > 0 {
            Self::to_processing_rate(config.processing_rate, rate_scale)
                .max(ProcessingRate::from_raw(1))
        } else {
            ProcessingRate::from_raw(0)
        };
        let power_cost_nw_per_rate = config
            .active_power_nw
            .saturating_mul(rate_scale)
            .checked_div(config.processing_rate)
            .unwrap_or(0);
        let (name, name_len) = copy_str_name(config.diagnostic_name);
        Self {
            options: config.options,
            control: config.control_interface,
            control_argument: config.control_argument,
            processing_rate,
            active_power_nw: config.active_power_nw,
            static_power_nw: config.static_power_nw,
            power_cost_nw_per_rate,
            name,
            name_len,
            level: level_index,
        }
    }

    /// Processing rate when this active power level is selected.
    #[inline]
    pub const fn processing_rate(&self) -> ProcessingRate {
        self.processing_rate
    }

    /// Active power consumption in nanowatts while executing at this level.
    #[inline]
    pub const fn active_power_nw(&self) -> u64 {
        self.active_power_nw
    }

    /// Steady-state static leakage power in nanowatts while clock-gated at this level's rail
    /// voltage.
    #[inline]
    pub const fn static_power_nw(&self) -> u64 {
        self.static_power_nw
    }

    /// Active power cost of this level, normalized by its processing rate.
    #[inline]
    pub const fn power_cost_nw_per_rate(&self) -> u64 {
        self.power_cost_nw_per_rate
    }

    /// ID of the interface handling transitions to this active level.
    #[inline]
    pub const fn control(&self) -> ControlInterface {
        self.control
    }

    /// Argument interpreted by the control interface to transition to this active level.
    #[inline]
    pub const fn control_argument(&self) -> u64 {
        self.control_argument
    }

    /// Returns true if this level may be transitioned on a per-CPU basis without affecting other
    /// CPUs in the same power domain.
    #[inline]
    pub const fn targets_cpus(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) != 0
    }

    /// Returns true if transitioning to this level transitions all CPUs in the power domain
    /// together.
    #[inline]
    pub const fn targets_power_domain(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) == 0
    }

    /// Diagnostic name identifying this active power level.
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Sorted 0-based index of this level in [`EnergyModel::active_levels`].
    #[inline]
    pub const fn level(&self) -> u8 {
        self.level
    }
}

impl Default for ActivePowerLevel {
    #[inline]
    fn default() -> Self {
        Self {
            options: 0,
            control: ControlInterface::CpuDriver,
            control_argument: 0,
            processing_rate: ProcessingRate::from_raw(0),
            active_power_nw: 0,
            static_power_nw: 0,
            power_cost_nw_per_rate: 0,
            name: [0; ZX_MAX_NAME_LEN],
            name_len: 0,
            level: 0,
        }
    }
}

/// Kernel representation of a processor idle state (`WFI`, core retention/power-down, cluster
/// retention/power-down, or system idle).
#[derive(Copy, Clone, Debug)]
pub struct IdlePowerState {
    /// Steady-state idle power model (`ClockGated` vs. `PowerGated`).
    power_model: IdlePowerModel,
    /// Control interface used to enter this idle state.
    control: ControlInterface,
    /// Argument provided to the control interface (e.g. PSCI `power_state` parameter).
    control_argument: u64,
    /// Worst-case latency in nanoseconds to enter this idle state.
    entry_latency_ns: zx_duration_mono_t,
    /// Worst-case latency in nanoseconds to exit this idle state back to execution.
    exit_latency_ns: zx_duration_mono_t,
    /// Minimum idle duration in nanoseconds required to break even on entry and exit energy.
    min_residency_ns: zx_duration_mono_t,
    /// Bitmask of CPUs coordinated by this idle state (`0` when scoped solely to the calling CPU).
    scope_cpu_mask: u64,
    /// Diagnostic name.
    name: [u8; ZX_MAX_NAME_LEN],
    /// Length of the diagnostic name.
    name_len: usize,
    /// Architectural flags for this idle state.
    flags: IdleStateFlags,
    /// Sorted 0-based index of this state in [`EnergyModel::idle_states`].
    state_index: u8,
}

impl IdlePowerState {
    /// Constructs an `IdlePowerState` from an [`IdlePowerStateConfig`] descriptor.
    pub fn new(state_index: u8, config: &IdlePowerStateConfig<'_>) -> Self {
        let (name, name_len) = copy_str_name(config.diagnostic_name);
        Self {
            power_model: config.power_model,
            control: config.control_interface,
            control_argument: config.control_argument,
            entry_latency_ns: config.entry_latency_ns,
            exit_latency_ns: config.exit_latency_ns,
            min_residency_ns: config.min_residency_ns,
            scope_cpu_mask: config.scope_cpu_mask,
            name,
            name_len,
            flags: config.flags,
            state_index,
        }
    }

    /// Steady-state idle power model of this idle state.
    #[inline]
    pub const fn power_model(&self) -> IdlePowerModel {
        self.power_model
    }

    /// Worst-case latency in nanoseconds to enter this idle state.
    #[inline]
    pub const fn entry_latency_ns(&self) -> zx_duration_mono_t {
        self.entry_latency_ns
    }

    /// Worst-case latency in nanoseconds to exit this idle state back to execution.
    #[inline]
    pub const fn exit_latency_ns(&self) -> zx_duration_mono_t {
        self.exit_latency_ns
    }

    /// Minimum idle duration in nanoseconds required to break even on entry and exit energy.
    #[inline]
    pub const fn min_residency_ns(&self) -> zx_duration_mono_t {
        self.min_residency_ns
    }

    /// Bitmask of CPUs coordinated by this idle state (`0` when scoped solely to the calling CPU).
    #[inline]
    pub const fn scope_cpu_mask(&self) -> u64 {
        self.scope_cpu_mask
    }

    /// Architectural flags for this idle state.
    #[cfg(test)]
    #[inline]
    const fn flags(&self) -> IdleStateFlags {
        self.flags
    }

    /// Returns true if the per-CPU architectural timer stops while in this idle state.
    #[inline]
    pub const fn local_timer_stops(&self) -> bool {
        self.flags.contains(IdleStateFlags::LOCAL_TIMER_STOPS)
    }

    /// ID of the interface handling entry into this idle state.
    #[inline]
    pub const fn control(&self) -> ControlInterface {
        self.control
    }

    /// Argument interpreted by the control interface to enter this idle state.
    #[inline]
    pub const fn control_argument(&self) -> u64 {
        self.control_argument
    }

    /// Diagnostic name identifying this idle state.
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Sorted 0-based index of this state in [`EnergyModel::idle_states`].
    #[inline]
    pub const fn state_index(&self) -> u8 {
        self.state_index
    }
}

impl Default for IdlePowerState {
    #[inline]
    fn default() -> Self {
        Self {
            power_model: IdlePowerModel::ClockGated,
            control: ControlInterface::ArmWfi,
            control_argument: 0,
            entry_latency_ns: 0,
            exit_latency_ns: 0,
            min_residency_ns: 0,
            scope_cpu_mask: 0,
            name: [0; ZX_MAX_NAME_LEN],
            name_len: 0,
            flags: IdleStateFlags::empty(),
            state_index: 0,
        }
    }
}

/// Represents an entry in an active-to-active transition matrix, where the position in the matrix
/// denotes the source and target [`ActivePowerLevel`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerLevelTransition {
    /// Time required for the transition to take effect. In some cases it may mean for the actual
    /// voltage to stabilize.
    latency: zx_duration_mono_t,
    /// Amount of energy consumed to perform the transition.
    energy_cost_nj: u64,
}

impl PowerLevelTransition {
    /// Returns an invalid transition.
    #[inline]
    pub const fn invalid() -> Self {
        Self { latency: ZX_TIME_INFINITE, energy_cost_nj: u64::MAX }
    }

    /// Returns a zero-cost transition.
    #[inline]
    pub const fn zero() -> Self {
        Self { latency: 0, energy_cost_nj: 0 }
    }

    /// Constructs a `PowerLevelTransition` from a `ProcessorPowerLevelTransition`.
    #[inline]
    pub const fn new(transition: &ProcessorPowerLevelTransition) -> Self {
        Self { latency: transition.latency, energy_cost_nj: transition.energy_nj }
    }

    /// Latency for transitioning from a given active level to another.
    #[inline]
    pub const fn latency(&self) -> zx_duration_mono_t {
        self.latency
    }

    /// Energy cost in nanojoules (nJ) for transitioning from a given active level to another.
    #[inline]
    pub const fn energy_cost_nj(&self) -> u64 {
        self.energy_cost_nj
    }

    /// Whether the transition is valid or not.
    #[inline]
    pub const fn is_valid(&self) -> bool {
        let invalid = Self::invalid();
        self.latency != invalid.latency && self.energy_cost_nj != invalid.energy_cost_nj
    }
}

impl Default for PowerLevelTransition {
    #[inline]
    fn default() -> Self {
        Self::invalid()
    }
}

/// Square matrix view over an [`EnergyModel`]'s active-to-active [`PowerLevelTransition`] table.
#[derive(Copy, Clone, Debug)]
pub struct TransitionMatrix<'a> {
    /// Slice of active transitions flattened in row-major order.
    transitions: &'a [PowerLevelTransition],
    /// Number of rows (and columns) in the square matrix.
    num_rows: usize,
}

impl<'a> TransitionMatrix<'a> {
    /// Creates a `TransitionMatrix` over `transitions` with `num_rows` active levels.
    #[inline]
    fn new(transitions: &'a [PowerLevelTransition], num_rows: usize) -> Self {
        debug_assert!(transitions.len() == num_rows * num_rows);
        Self { transitions, num_rows }
    }

    /// Returns the number of rows (and columns) in the matrix.
    #[cfg(test)]
    #[inline]
    const fn num_rows(&self) -> usize {
        self.num_rows
    }

    /// Returns row `index` without an `Option` wrapper (`index < self.num_rows`).
    #[inline]
    const fn row(&self, index: usize) -> &'a [PowerLevelTransition] {
        assert!(index < self.num_rows);
        let start = index * self.num_rows;
        let (_, rest) = self.transitions.split_at(start);
        let (row, _) = rest.split_at(self.num_rows);
        row
    }

    /// Returns the transition row for active level `index`, or `None` if `index >= self.num_rows`.
    #[inline]
    pub const fn get(&self, index: usize) -> Option<&'a [PowerLevelTransition]> {
        if index < self.num_rows { Some(self.row(index)) } else { None }
    }
}

impl<'a> ops::Index<usize> for TransitionMatrix<'a> {
    type Output = [PowerLevelTransition];

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        self.row(index)
    }
}

/// Opaque Rust facade for C++ `power_management::PowerLevelController`.
///
/// # Safety Invariant
///
/// Any reference `&CppPowerLevelController` aliases a live, initialized C++
/// `power_management::PowerLevelController` instance; this zero-sized type is never constructed by
/// value in Rust.
#[repr(C)]
pub struct CppPowerLevelController {
    _facade: OpaqueFacade,
}

impl CppPowerLevelController {
    /// Determines whether the controller is serving requests or not.
    ///
    /// Allows scheduler instances to interrogate whether they should consider any active power
    /// levels or not.
    #[inline]
    pub fn is_serving(&self) -> bool {
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController`.
        unsafe { cpp_power_level_controller_is_serving(self) }
    }

    /// Returns the control interface this power level controller implements.
    #[inline]
    pub fn control_interface(&self) -> ControlInterface {
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController` with a
        // valid `ControlInterface` discriminant.
        unsafe { cpp_power_level_controller_control_interface(self) }
    }

    /// Returns the current device-specific power level (i.e. the value passed as a control
    /// argument) for the given `domain_id`, if supported by the controller. May be implemented by
    /// in-kernel CPU drivers that are capable of querying the current hardware state.
    #[inline]
    pub fn get_current_power_level(&self, domain_id: u32) -> Result<u64, Status> {
        let mut level = 0u64;
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController` and
        // `&mut level` is a valid writable `u64` on the stack.
        let status = unsafe {
            cpp_power_level_controller_get_current_power_level(self, domain_id, &mut level)
        };
        Status::ok(status)?;
        Ok(level)
    }
}

/// `EnergyModel` describes the orthogonal active OPPs (`ActivePowerLevel`) and idle states
/// (`IdlePowerState`) a processor power domain may enter, along with active-to-active transition
/// costs.
///
/// An energy model is immutable once initialized. Updating the active energy model requires
/// replacing it with a new instance on a newly registered [`PowerDomain`].
#[repr(C)]
pub struct EnergyModel {
    /// Active OPPs sorted by ascending `(processing_rate, active_power_nw)`.
    active_levels: Vector<ActivePowerLevel>,
    /// Idle states sorted shallowest to deepest (`ClockGated` before `PowerGated`, then ascending
    /// `(min_residency_ns, exit_latency_ns, entry_latency_ns, control, control_argument)`).
    idle_states: Vector<IdlePowerState>,
    /// Flattened `active_levels.len()^2` matrix of active-to-active transition costs.
    active_transitions: Vector<PowerLevelTransition>,
    /// Active level indices sorted by `(control, control_argument)`.
    active_control_lookup: Vector<usize>,
    /// Scale used to normalize raw integer processing rates.
    max_processing_rate_scale: u64,
}

zr::static_assert!(size_of::<EnergyModel>() == 104);
zr::static_assert!(align_of::<EnergyModel>() == 8);

impl EnergyModel {
    /// Creates an empty default `EnergyModel`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            active_levels: Vector::new(),
            idle_states: Vector::new(),
            active_transitions: Vector::new(),
            active_control_lookup: Vector::new(),
            max_processing_rate_scale: 1,
        }
    }

    /// Creates an [`EnergyModel`] from the active levels, idle states, and active transitions in a
    /// [`PowerDomainConfig`].
    pub fn create_from_config(
        config: &PowerDomainConfig<'_>,
        mut max_processing_rate: u64,
    ) -> Result<Self, Status> {
        let active_descs = config.active_levels;
        let idle_descs = config.idle_states;
        let transitions = config.active_transitions;

        if active_descs.is_empty()
            || active_descs.len() > (u8::MAX as usize) + 1
            || idle_descs.len() > (u8::MAX as usize) + 1
        {
            return Err(Status::INVALID_ARGS);
        }

        let mut local_max_rate = 0u64;
        for level in active_descs {
            if level.processing_rate == 0 {
                return Err(Status::INVALID_ARGS);
            }
            local_max_rate = local_max_rate.max(level.processing_rate);
        }

        if transitions.len() > active_descs.len() * active_descs.len() {
            return Err(Status::INVALID_ARGS);
        }

        for transition in transitions {
            if (transition.from as usize) >= active_descs.len()
                || (transition.to as usize) >= active_descs.len()
            {
                return Err(Status::INVALID_ARGS);
            }
        }

        let mut idle_states = Vector::new();
        if !idle_descs.is_empty() {
            idle_states.reserve(idle_descs.len()).map_err(|_| Status::NO_MEMORY)?;
            for (i, state_desc) in idle_descs.iter().enumerate() {
                if state_desc.entry_latency_ns < 0
                    || state_desc.exit_latency_ns < 0
                    || state_desc.min_residency_ns < 0
                    || IdleStateFlags::from_bits(state_desc.flags.bits()).is_none()
                {
                    return Err(Status::INVALID_ARGS);
                }
                let res = idle_states.push_back(IdlePowerState::new(i as u8, state_desc));
                assert!(res.is_ok());
            }

            // Order idle states from shallowest (`ClockGated` WFI) to deepest (`PowerGated`
            // states with ascending residency and wake/entry latencies), breaking ties by
            // `(control, control_argument)` so `state_index` ordering is deterministic.
            idle_states.sort_unstable_by(|a, b| {
                let key = |s: &IdlePowerState| {
                    (
                        matches!(s.power_model(), IdlePowerModel::PowerGated { .. }),
                        s.min_residency_ns(),
                        s.exit_latency_ns(),
                        s.entry_latency_ns(),
                        s.control() as u64,
                        s.control_argument(),
                        s.state_index(),
                    )
                };
                key(a).cmp(&key(b))
            });

            for (sorted_idx, idle_state) in idle_states.iter_mut().enumerate() {
                idle_state.state_index = sorted_idx as u8;
            }
        }

        if max_processing_rate == 0 {
            max_processing_rate = local_max_rate;
        }

        let mut active_levels = Vector::new();
        active_levels.reserve(active_descs.len()).map_err(|_| Status::NO_MEMORY)?;

        let mut active_control_lookup = Vector::new();
        active_control_lookup.reserve(active_descs.len()).map_err(|_| Status::NO_MEMORY)?;

        let mut active_transitions = Vector::new();
        let default_transition = if transitions.is_empty() {
            PowerLevelTransition::zero()
        } else {
            PowerLevelTransition::invalid()
        };
        active_transitions
            .resize(active_descs.len() * active_descs.len(), default_transition)
            .map_err(|_| Status::NO_MEMORY)?;

        for (i, level_desc) in active_descs.iter().enumerate() {
            let res = active_levels.push_back(ActivePowerLevel::new(
                i as u8,
                level_desc,
                max_processing_rate,
            ));
            assert!(res.is_ok());
            let res = active_control_lookup.push_back(i);
            assert!(res.is_ok());
        }

        // Sort active OPPs by `(processing_rate, active_power_nw, input_index)` without requiring
        // `static_power_nw` itself to be monotonic across adjacent low-voltage OPPs.
        active_levels.sort_unstable_by(|a, b| {
            (a.processing_rate(), a.active_power_nw(), a.level()).cmp(&(
                b.processing_rate(),
                b.active_power_nw(),
                b.level(),
            ))
        });

        // Map each original active index (temporarily held in `level`) to its sorted index, remap
        // the active transition matrix, and then update `level` to hold the sorted 0-based index.
        for sorted_idx in 0..active_levels.len() {
            active_control_lookup[active_levels[sorted_idx].level() as usize] = sorted_idx;
        }

        for transition in transitions {
            let i = active_control_lookup[transition.from as usize];
            let j = active_control_lookup[transition.to as usize];
            assert!(active_levels[i].level() == transition.from);
            assert!(active_levels[j].level() == transition.to);
            let offset = (i * active_levels.len()) + j;
            active_transitions[offset] = PowerLevelTransition::new(transition);
        }

        for (sorted_idx, active_level) in active_levels.iter_mut().enumerate() {
            active_level.level = sorted_idx as u8;
        }

        for (i, slot) in active_control_lookup.iter_mut().enumerate() {
            *slot = i;
        }
        active_control_lookup.sort_unstable_by(|&a, &b| {
            (active_levels[a].control() as u64, active_levels[a].control_argument())
                .cmp(&(active_levels[b].control() as u64, active_levels[b].control_argument()))
        });

        Ok(Self {
            active_levels,
            idle_states,
            active_transitions,
            active_control_lookup,
            max_processing_rate_scale: max_processing_rate,
        })
    }

    /// Creates a synthetic `EnergyModel` with `idle_states` idle states (shallowest `ClockGated`
    /// WFI followed by `PowerGated` PSCI states) and `active_levels` `CpuDriver` operating points
    /// (`control_argument == index`, `active_power_nw == (index + 1) * 100`,
    /// `static_power_nw == (index + 1) * 10`).
    #[cfg(any(test, ktest, feature = "userspace"))]
    pub fn create_for_test(idle_states: usize, active_levels: usize) -> Self {
        assert!(active_levels > 0 && active_levels <= (u8::MAX as usize) + 1);
        assert!(idle_states <= (u8::MAX as usize) + 1);

        let mut active_configs = Vector::new();
        active_configs.reserve(active_levels).expect("failed to reserve test active level configs");
        for i in 0..active_levels {
            let step = (i + 1) as u64;
            let res = active_configs.push_back(ActivePowerLevelConfig {
                options: 0,
                processing_rate: step * 10,
                active_power_nw: step * 100,
                static_power_nw: step * 10,
                control_interface: ControlInterface::CpuDriver,
                control_argument: i as u64,
                diagnostic_name: "opp",
            });
            assert!(res.is_ok());
        }

        let mut idle_configs = Vector::new();
        if idle_states > 0 {
            idle_configs.reserve(idle_states).expect("failed to reserve test idle state configs");
            for i in 0..idle_states {
                let cfg = if i == 0 {
                    IdlePowerStateConfig {
                        diagnostic_name: "idle",
                        ..IdlePowerStateConfig::ARM_WFI
                    }
                } else {
                    IdlePowerStateConfig {
                        power_model: IdlePowerModel::PowerGated { residual_power_nw: 0 },
                        entry_latency_ns: (i as i64) * 1_000,
                        exit_latency_ns: (i as i64) * 2_000,
                        min_residency_ns: (i as i64) * 10_000,
                        scope_cpu_mask: 0,
                        flags: IdleStateFlags::LOCAL_TIMER_STOPS,
                        control_interface: ControlInterface::ArmPsci,
                        control_argument: i as u64,
                        diagnostic_name: "idle",
                    }
                };
                let res = idle_configs.push_back(cfg);
                assert!(res.is_ok());
            }
        }

        let domain_config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active_configs,
            idle_states: &idle_configs,
            active_transitions: &[],
        };
        Self::create_from_config(&domain_config, 0).expect("failed to create test EnergyModel")
    }

    /// Scale used to normalize raw integer rates into fixed-point `ProcessingRate`.
    #[inline]
    pub const fn max_processing_rate_scale(&self) -> u64 {
        self.max_processing_rate_scale
    }

    /// Normalizes `processing_rate` using `max_processing_rate_scale`.
    #[inline]
    pub fn to_processing_rate(&self, processing_rate: u64) -> ProcessingRate {
        ActivePowerLevel::to_processing_rate(processing_rate, self.max_processing_rate_scale)
    }

    /// Converts `processing_rate` back to a raw integer rate using `max_processing_rate_scale`.
    #[inline]
    pub fn from_processing_rate(&self, processing_rate: ProcessingRate) -> u64 {
        ActivePowerLevel::from_processing_rate(processing_rate, self.max_processing_rate_scale)
    }

    /// Active OPPs sorted by ascending `(processing_rate, active_power_nw)`.
    #[inline]
    pub fn active_levels(&self) -> &[ActivePowerLevel] {
        &self.active_levels
    }

    /// Idle states sorted shallowest to deepest (`ClockGated` before `PowerGated`, then ascending
    /// `(min_residency_ns, exit_latency_ns, entry_latency_ns, control, control_argument)`).
    #[inline]
    pub fn idle_states(&self) -> &[IdlePowerState] {
        &self.idle_states
    }

    /// Returns the processing rate of the fastest active power level.
    #[inline]
    pub fn max_processing_rate(&self) -> ProcessingRate {
        self.active_levels
            .last()
            .map_or(ProcessingRate::from_raw(0), ActivePowerLevel::processing_rate)
    }

    /// Returns the `active_levels.len() x active_levels.len()` transition matrix.
    #[inline]
    pub fn transitions(&self) -> TransitionMatrix<'_> {
        TransitionMatrix::new(&self.active_transitions, self.active_levels.len())
    }

    /// Looks up the 0-based active power level index matching `(interface_id, control_argument)`.
    pub fn find_active_power_level(
        &self,
        interface_id: ControlInterface,
        control_argument: u64,
    ) -> Option<u8> {
        let idx = self.active_control_lookup.partition_point(|&i| {
            let a = &self.active_levels[i];
            (a.control() as u64, a.control_argument()) < (interface_id as u64, control_argument)
        });
        if let Some(&level_idx) = self.active_control_lookup.get(idx) {
            let level = &self.active_levels[level_idx];
            if level.control() == interface_id && level.control_argument() == control_argument {
                return Some(level_idx as u8);
            }
        }
        None
    }

    /// Returns the lowest active power level whose processing rate is greater than or equal to
    /// `processing_rate`, clamping rates above [`Self::max_processing_rate`] to the fastest active
    /// level.
    pub fn find_active_power_level_for_rate(
        &self,
        mut processing_rate: ProcessingRate,
    ) -> Option<&ActivePowerLevel> {
        processing_rate = processing_rate.min(self.max_processing_rate());
        let levels = self.active_levels();
        let idx =
            levels.partition_point(|power_level| power_level.processing_rate() < processing_rate);
        levels.get(idx)
    }

    /// Returns the steady-state power consumption in nanowatts while residing in
    /// `idle_states[idle_state_index]` with the domain rail at `active_level_index`.
    ///
    /// - For [`IdlePowerModel::ClockGated`], returns `active_levels[active_idx].static_power_nw()`
    ///   (or `0` if `active_level_index` is `None` or out of bounds).
    /// - For [`IdlePowerModel::PowerGated`], returns `residual_power_nw` independent of
    ///   `active_level_index`.
    #[inline]
    pub fn idle_power_nw(&self, idle_state_index: usize, active_level_index: Option<usize>) -> u64 {
        let Some(idle_state) = self.idle_states.get(idle_state_index) else {
            return 0;
        };
        match idle_state.power_model() {
            IdlePowerModel::ClockGated => active_level_index
                .and_then(|idx| self.active_levels.get(idx))
                .map_or(0, ActivePowerLevel::static_power_nw),
            IdlePowerModel::PowerGated { residual_power_nw } => residual_power_nw,
        }
    }
}

impl Default for EnergyModel {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

fbl::impl_opaque_ref_counted_facade!(
    /// PowerDomain establishes the relationship between a set of CPUs, the energy model that
    /// describes their characteristics, and the power level controller responsible for changing
    /// the active power levels for the set of CPUs.
    ///
    /// Instances of PowerDomain are safe for concurrent use.
    ///
    /// # Safety Invariant
    ///
    /// Any reference `&PowerDomain` aliases a live, initialized C++
    /// `power_management::PowerDomain` instance; this zero-sized type is never constructed by
    /// value in Rust.
    pub struct PowerDomain,
    cpp_power_domain_recycle,
);

impl PowerDomain {
    /// Creates a `PowerDomain` with the given `domain_id`, `cpu_mask`, `model`, and `controller`.
    ///
    /// # Safety
    ///
    /// `controller` must be a valid pointer exported via `fbl::ExportToRawPtr` whose reference
    /// count ownership is transferred to this function.
    #[cfg(not(feature = "userspace"))]
    #[inline]
    pub unsafe fn create(
        domain_id: u32,
        cpu_mask: u64,
        model: &mut EnergyModel,
        controller: *mut CppPowerLevelController,
    ) -> Result<RefPtr<Self>, Status> {
        // SAFETY: `model` is a valid initialized `EnergyModel`, and the caller guarantees
        // `controller` was exported via `fbl::ExportToRawPtr` (whose reference count ownership is
        // transferred to `fbl::ImportFromRawPtr` in `cpp_power_domain_create`).
        let domain_ptr = unsafe { cpp_power_domain_create(domain_id, cpu_mask, model, controller) };
        // SAFETY: `domain_ptr` is either null or exported via `fbl::ExportToRawPtr` with an
        // acquired reference count.
        unsafe { RefPtr::try_from_raw(domain_ptr) }.ok_or(Status::NO_MEMORY)
    }

    /// Creates a new `PowerDomain` with a test `PowerLevelController`.
    #[cfg(test)]
    #[inline]
    pub fn create_for_test(
        domain_id: u32,
        cpu_mask: u64,
        model: &mut EnergyModel,
    ) -> Result<RefPtr<Self>, Status> {
        // SAFETY: `model` is a valid initialized `EnergyModel`.
        let ptr = unsafe { cpp_power_domain_create_for_test(domain_id, cpu_mask, model) };
        // SAFETY: `cpp_power_domain_create_for_test` returns either null or a pointer exported via
        // `fbl::ExportToRawPtr` with an acquired reference count.
        unsafe { RefPtr::try_from_raw(ptr) }.ok_or(Status::NO_MEMORY)
    }

    /// ID representing the relationship between a set of CPUs and a power model.
    #[inline]
    pub fn id(&self) -> u32 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_id(self) }
    }

    /// Set of CPUs associated with `model()`.
    #[inline]
    pub fn cpus(&self) -> u64 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_cpus(self) }
    }

    /// Model describing the behavior of the power domain.
    #[inline]
    pub fn model(&self) -> &EnergyModel {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`, and `model_` lives
        // inline inside `PowerDomain` for the lifetime of `self`.
        unsafe { cpp_power_domain_model(self) }
    }

    /// The total normalized utilization of the set of processors associated with this power domain.
    ///
    /// Uses relaxed semantics, since the value does not need to synchronize with other memory
    /// accesses and inaccuracy is acceptable.
    #[inline]
    pub fn total_normalized_utilization(&self) -> Utilization {
        Utilization::from_raw(self.total_normalized_utilization_atomic().load(Ordering::Relaxed))
    }

    /// Handler for transitions where the target level's control interface is not kernel handled.
    #[inline]
    pub fn controller(&self) -> &CppPowerLevelController {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain` whose `controller_`
        // is non-null (`ZX_DEBUG_ASSERT(controller_ != nullptr)`) and lives as long as `self`.
        unsafe { cpp_power_domain_controller(self) }
    }

    /// Returns whether the kernel scheduler should send power level update requests to the
    /// controller. This is used by tests to prevent the scheduler from sending power level change
    /// requests through the fake control interface that could confuse the test. It does not
    /// prevent the kernel from exercising the control interface, however, only whether the
    /// scheduler will interact with the control interface to handle utilization changes.
    ///
    /// Uses relaxed semantics, since this variable will generally be synchronized by the lock
    /// protecting each `PowerState` as it is associated with this `PowerDomain`. However, an
    /// atomic is used to prevent formal data races if the value is read outside of external
    /// synchronization, which can't be statically checked internally.
    #[inline]
    pub fn scheduler_control_enabled(&self) -> bool {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_scheduler_control_enabled(self) }
    }

    /// Sets whether the kernel scheduler should send power level update requests through the
    /// control interface. When set to false, the scheduler must not send requests to avoid
    /// confusing tests.
    #[inline]
    pub fn set_scheduler_control_enabled(&self, enabled: bool) {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_set_scheduler_control_enabled(self, enabled) }
    }

    /// Returns a reference to the atomic `total_normalized_utilization_` counter of `self`.
    ///
    /// Callers may use `Ordering::Relaxed` for loads and read-modify-write operations (`fetch_add`
    /// and `fetch_sub`) because the domain utilization sum is a standalone heuristic metric that
    /// does not synchronize other memory accesses.
    #[inline]
    pub(super) fn total_normalized_utilization_atomic(&self) -> &AtomicI64 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`, and
        // `total_normalized_utilization_` is an inline `mutable std::atomic<int64_t>` with
        // identical size and alignment to `AtomicI64`.
        unsafe { cpp_power_domain_total_normalized_utilization_ptr(self) }
    }
}

/// Array type for storing power domains in a [`PowerDomainSet`].
pub type ArrayType = [Option<RefPtr<PowerDomain>>; MAX_POWER_DOMAINS];

/// Maintains an array of ref pointers to the power domains in the system. This is used for both
/// the power domain registry storage and processor-local caches of the power domain set.
/// Processor-local caches avoid cross-processor and central lock contention when consulting a
/// snapshot of the system power domains during sensitive scheduling operations.
#[repr(C)]
#[derive(Clone, Default)]
pub struct PowerDomainSet {
    /// Array of reference-counted power domains in the set.
    domains: ArrayType,
}

zr::static_assert!(size_of::<Option<RefPtr<PowerDomain>>>() == 8);
zr::static_assert!(align_of::<Option<RefPtr<PowerDomain>>>() == 8);
zr::static_assert!(size_of::<PowerDomainSet>() == 32);
zr::static_assert!(align_of::<PowerDomainSet>() == 8);
zr::static_assert!(offset_of!(PowerDomainSet, domains) == 0);

impl PowerDomainSet {
    /// Maximum number of power domains supported in a [`PowerDomainSet`].
    pub const MAX_POWER_DOMAINS: usize = MAX_POWER_DOMAINS;

    /// Creates an empty `PowerDomainSet`.
    #[inline]
    pub const fn new() -> Self {
        Self { domains: [const { None }; MAX_POWER_DOMAINS] }
    }

    /// Creates a PowerDomainSet from a span of PowerDomain references.
    /// Validates that domain count is <= MAX_POWER_DOMAINS, all pointers are non-null,
    /// each domain has a non-zero CPU mask, and no two domains share a domain ID or
    /// have intersecting CPU masks.
    pub fn create(domains: &[Option<RefPtr<PowerDomain>>]) -> Result<Self, Status> {
        if domains.len() > MAX_POWER_DOMAINS {
            return Err(Status::OUT_OF_RANGE);
        }

        for (i, domain) in domains.iter().enumerate() {
            let Some(domain) = domain.as_ref().filter(|d| d.cpus() != 0) else {
                return Err(Status::INVALID_ARGS);
            };
            for prev in domains[..i].iter().flatten() {
                if prev.id() == domain.id() || (prev.cpus() & domain.cpus()) != 0 {
                    return Err(Status::INVALID_ARGS);
                }
            }
        }

        Ok(Self { domains: core::array::from_fn(|i| domains.get(i).and_then(Option::clone)) })
    }

    /// Creates a `PowerDomainSet` from a slice of domain configurations.
    ///
    /// Validates domain configs, determines system-wide peak rate normalization, creates an
    /// `EnergyModel` for each domain, binds to the corresponding `PDevPowerLevelController`, and
    /// constructs the `PowerDomain` instances.
    pub fn create_from_configs(domain_configs: &[PowerDomainConfig<'_>]) -> Result<Self, Status> {
        #[cfg(not(feature = "userspace"))]
        {
            if domain_configs.is_empty() {
                return Ok(Self::new());
            }
            if domain_configs.len() > MAX_POWER_DOMAINS {
                return Err(Status::OUT_OF_RANGE);
            }

            // Validate configs and find system-wide max processing rate across all domains.
            let mut max_system_processing_rate = 0u64;
            for config in domain_configs {
                if config.cpu_mask == 0 {
                    return Err(Status::INVALID_ARGS);
                }
                for level in config.active_levels {
                    max_system_processing_rate =
                        max_system_processing_rate.max(level.processing_rate);
                }
            }

            let mut domain_array: ArrayType = [const { None }; MAX_POWER_DOMAINS];

            for (i, config) in domain_configs.iter().enumerate() {
                let mut energy_model =
                    match EnergyModel::create_from_config(config, max_system_processing_rate) {
                        Ok(model) => model,
                        Err(status) => {
                            dprintf!(
                                CRITICAL,
                                "POWER: Failed to create energy model for domain {}: {}\n",
                                config.domain_id,
                                status.into_raw()
                            );
                            return Err(status);
                        }
                    };

                let mut controller: *mut CppPowerLevelController = core::ptr::null_mut();
                // SAFETY: `&mut controller` is a valid writable pointer that receives a raw
                // pointer exported via `fbl::ExportToRawPtr` on `ZX_OK`.
                let status = unsafe {
                    cpp_pdev_power_level_controller_get(config.domain_id, &mut controller)
                };
                if let Err(err) = Status::ok(status) {
                    dprintf!(
                        CRITICAL,
                        "POWER: Failed to get PDevPowerLevelController for domain {}: {}\n",
                        config.domain_id,
                        err.into_raw()
                    );
                    return Err(err);
                }

                // SAFETY: `controller` was exported via `fbl::ExportToRawPtr` by
                // `cpp_pdev_power_level_controller_get` and ownership of its reference count is
                // transferred to `PowerDomain::create`.
                let Ok(domain) = (unsafe {
                    PowerDomain::create(
                        config.domain_id,
                        config.cpu_mask,
                        &mut energy_model,
                        controller,
                    )
                }) else {
                    dprintf!(
                        CRITICAL,
                        "POWER: Failed to allocate PowerDomain for domain {}\n",
                        config.domain_id
                    );
                    return Err(Status::NO_MEMORY);
                };
                domain.set_scheduler_control_enabled(true);

                domain_array[i] = Some(domain);
            }

            Self::create(&domain_array[..domain_configs.len()])
        }
        #[cfg(feature = "userspace")]
        {
            let _ = domain_configs;
            Err(Status::NOT_SUPPORTED)
        }
    }

    /// Creates a PowerDomainSet with the given PowerDomain as its only entry for testing.
    #[inline]
    pub fn create_for_test(domain: Option<&RefPtr<PowerDomain>>) -> Self {
        let mut domains = [const { None }; MAX_POWER_DOMAINS];
        domains[0] = domain.cloned();
        Self { domains }
    }

    /// Returns a borrowed reference to the power domain with the given id, or `None` if there
    /// isn't one. Returns a reference to avoid unnecessary ref count changes in contexts where the
    /// set is guaranteed not to change and maintain the lifetime of its power domain elements.
    #[inline]
    pub fn find_by_domain_id(&self, domain_id: u32) -> Option<&RefPtr<PowerDomain>> {
        for element in &self.domains {
            if element.as_ref().is_some_and(|element| element.id() == domain_id) {
                return element.as_ref();
            }
        }
        None
    }

    /// Returns a borrowed reference to the power domain for the given CPU id, or `None` if there
    /// isn't one. Returns a reference to avoid unnecessary ref count changes in contexts where the
    /// set is guaranteed not to change and maintain the lifetime of its power domain elements.
    #[inline]
    pub fn find_by_cpu_num(&self, cpu_num: u32) -> Option<&RefPtr<PowerDomain>> {
        #[cfg(not(feature = "userspace"))]
        let cpu_mask = {
            debug_assert!(cpu_num < cpu_mask_t::BITS);
            cpu_num_to_mask(cpu_num) as u64
        };
        #[cfg(feature = "userspace")]
        let cpu_mask = {
            debug_assert!(cpu_num < u64::BITS);
            1u64.checked_shl(cpu_num).unwrap_or(0)
        };
        for element in &self.domains {
            if element.as_ref().is_some_and(|element| (element.cpus() & cpu_mask) != 0) {
                return element.as_ref();
            }
        }
        None
    }

    /// Looks up the power cost for the given CPU operating at the given processing rate.
    #[inline]
    pub fn lookup_power_cost(&self, cpu_num: u32, processing_rate: ProcessingRate) -> u64 {
        #[expect(clippy::collapsible_if)]
        if let Some(power_domain) = self.find_by_cpu_num(cpu_num) {
            if let Some(power_level) =
                power_domain.model().find_active_power_level_for_rate(processing_rate)
            {
                return power_level.power_cost_nw_per_rate();
            }
        }
        0
    }

    /// Looks up the total normalized utilization for the power domain containing `cpu_num`.
    #[inline]
    pub fn lookup_total_normalized_utilization(&self, cpu_num: u32) -> Utilization {
        if let Some(power_domain) = self.find_by_cpu_num(cpu_num) {
            return power_domain.total_normalized_utilization();
        }
        Utilization::from_raw(0)
    }

    /// Visits each non-empty power domain element with the given callable.
    #[inline]
    pub fn visit(&self, mut visitor: impl FnMut(&RefPtr<PowerDomain>)) {
        #[expect(clippy::manual_flatten)]
        for element in &self.domains {
            if let Some(element) = element {
                visitor(element);
            }
        }
    }

    /// Dumps all power domains, active OPPs, and idle states to the kernel console.
    #[cfg(not(feature = "userspace"))]
    pub fn dump(&self) {
        self.visit(|domain| {
            kprintln!("Power domain {:u} (cpus={:#x}):", domain.id(), domain.cpus());
            kprintln!("  Active levels:");
            for (index, active_level) in domain.model().active_levels().iter().enumerate() {
                let rate =
                    ffl::String::new(active_level.processing_rate(), ffl::Mode::Dec, Some(6));
                let name = core::str::from_utf8(active_level.name()).unwrap_or("<invalid>");
                kprintln!(
                    concat!(
                        "    {:3u}. rate={:<8s} active={:10u} nw static={:10u} nw",
                        " cost={:10u} nw/rate control={:s}({:#x}) name={:s}",
                    ),
                    index,
                    rate.as_str(),
                    active_level.active_power_nw(),
                    active_level.static_power_nw(),
                    active_level.power_cost_nw_per_rate(),
                    control_interface_to_string(active_level.control()),
                    active_level.control_argument(),
                    name,
                );
            }
            kprintln!("  Idle states:");
            for (index, idle_state) in domain.model().idle_states().iter().enumerate() {
                let (model_str, residual_nw) = match idle_state.power_model() {
                    IdlePowerModel::ClockGated => ("ClockGated", 0),
                    IdlePowerModel::PowerGated { residual_power_nw } => {
                        ("PowerGated", residual_power_nw)
                    }
                };
                let name = core::str::from_utf8(idle_state.name()).unwrap_or("<invalid>");
                kprintln!(
                    concat!(
                        "    {:3u}. model={:<10s} residual={:10u} nw min_res={:d} ns",
                        " entry_lat={:d} ns exit_lat={:d} ns scope={:#x}",
                        " local_timer_stops={:u} control={:s}({:#x}) name={:s}",
                    ),
                    index,
                    model_str,
                    residual_nw,
                    idle_state.min_residency_ns(),
                    idle_state.entry_latency_ns(),
                    idle_state.exit_latency_ns(),
                    idle_state.scope_cpu_mask(),
                    u8::from(idle_state.local_timer_stops()),
                    control_interface_to_string(idle_state.control()),
                    idle_state.control_argument(),
                    name,
                );
            }
        });
    }

    /// Returns the count of non-empty array elements.
    #[inline]
    pub fn count(&self) -> usize {
        self.domains.iter().filter(|e| e.is_some()).count()
    }

    /// Returns true if all of the array elements are empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

/// Registers the provided power domain configurations and energy models with the kernel scheduler.
pub fn register_domains(domains: &[PowerDomainConfig<'_>]) -> Result<(), Status> {
    #[cfg(not(feature = "userspace"))]
    {
        let mut domain_set = match PowerDomainSet::create_from_configs(domains) {
            Ok(set) => set,
            Err(status) => {
                dprintf!(
                    CRITICAL,
                    "POWER: Failed to create PowerDomainSet: {}\n",
                    status.into_raw()
                );
                return Err(status);
            }
        };

        // SAFETY: `domain_set` is a valid initialized `PowerDomainSet`.
        unsafe {
            cpp_power_management_set_power_domain_set(&mut domain_set);
        }
        dprintf!(INFO, "POWER: Registered power domains in scheduler\n");
        Ok(())
    }
    #[cfg(feature = "userspace")]
    {
        let _ = domains;
        Err(Status::NOT_SUPPORTED)
    }
}

/// Sets processing rate limits (`min_rate` and `max_rate`) for the CPU cores identified by
/// `cpu_mask_in`.
pub fn set_rate_limits(cpu_mask_in: u64, min_rate: u64, max_rate: u64) -> Result<(), Status> {
    #[cfg(not(feature = "userspace"))]
    {
        let processor_count = PerCpu::processor_count().min(SMP_MAX_CPUS);
        let max_cpu_mask =
            if processor_count >= 64 { u64::MAX } else { (1u64 << processor_count) - 1 };
        if cpu_mask_in > max_cpu_mask {
            return Err(Status::INVALID_ARGS);
        }

        let mut cpu_mask = cpu_mask_in as cpu_mask_t;
        let cpu_count = cpu_mask.count_ones() as usize;
        if cpu_count == 0 {
            return Ok(());
        }

        let mut limits =
            Array::<zx_cpu_perf_limit_t>::try_new(cpu_count).map_err(|_| Status::NO_MEMORY)?;

        for i in 0..cpu_count {
            let cpu = remove_cpu_from_mask(&mut cpu_mask);
            debug_assert!(cpu != INVALID_CPU);
            limits[i] = zx_cpu_perf_limit_t {
                logical_cpu_number: cpu,
                limit_type: ZX_CPU_PERF_LIMIT_TYPE_RATE,
                min: min_rate,
                max: max_rate,
            };
        }
        debug_assert!(cpu_mask == 0);

        // SAFETY: `limits` contains distinct valid logical CPU numbers extracted from `cpu_mask`,
        // and no scheduler `queue_lock_` is held by the caller.
        unsafe {
            Scheduler::update_processing_limits(&limits);
        }
        Ok(())
    }
    #[cfg(feature = "userspace")]
    {
        let _ = (cpu_mask_in, min_rate, max_rate);
        Err(Status::NOT_SUPPORTED)
    }
}

/// Returns whether early boot performance boosting is enabled via kernel boot options.
#[inline]
pub fn boot_boost_enabled() -> bool {
    #[cfg(not(feature = "userspace"))]
    {
        let boot_options = boot_options::BootOptions::as_ptr();
        // SAFETY: If `boot_options` is non-null, it points to the valid global `BootOptions`.
        if let Some(boot_options) = unsafe { boot_options.as_ref() } {
            boot_options.power_boot_boost
        } else {
            true
        }
    }
    #[cfg(feature = "userspace")]
    {
        true
    }
}

/// Unit tests for the processor energy model.
#[cfg(test)]
mod tests {
    use fbl::{RefPtr, Vector};

    use super::super::energy_model_ffi::rust_energy_model_find_active_power_level;
    use super::{
        ActivePowerLevel, ActivePowerLevelConfig, ControlInterface, EnergyModel, IdlePowerModel,
        IdlePowerState, IdlePowerStateConfig, IdleStateFlags,
        POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PowerDomain, PowerDomainConfig, PowerDomainSet,
        PowerLevelTransition, ProcessingRate, ProcessorPowerLevelTransition, Status,
        ZX_MAX_NAME_LEN,
    };

    /// Creates a test `PowerDomain` with 3 idle states, 7 active OPPs, and the given CPU mask.
    fn make_domain(id: u32, cpus: &[u32]) -> RefPtr<PowerDomain> {
        let mut model = EnergyModel::create_for_test(3, 7);
        let mut mask = 0u64;
        for &cpu in cpus {
            mask |= 1u64 << cpu;
        }
        PowerDomain::create_for_test(id, mask, &mut model).expect("Failed to create PowerDomain")
    }

    /// Verifies `ActivePowerLevel::to_processing_rate` and `from_processing_rate`.
    #[test]
    fn to_from_rate() {
        let min_rate = ProcessingRate::from_raw(0);
        let max_rate = ProcessingRate::from(ffl::Fixed::<i64, 31>::from_integer(1));
        let min_raw_rate = 0u64;
        let max_raw_rate = 1024u64;

        assert_eq!(
            min_rate.raw_value(),
            ActivePowerLevel::to_processing_rate(min_raw_rate, max_raw_rate).raw_value()
        );
        assert_eq!(
            max_rate.raw_value(),
            ActivePowerLevel::to_processing_rate(max_raw_rate, max_raw_rate).raw_value()
        );
        assert_eq!(min_raw_rate, ActivePowerLevel::from_processing_rate(min_rate, max_raw_rate));
        assert_eq!(max_raw_rate, ActivePowerLevel::from_processing_rate(max_rate, max_raw_rate));
        assert_eq!(ActivePowerLevel::to_processing_rate(100, 0).raw_value(), 0);
    }

    /// Verifies `ActivePowerLevel::new` and `Default` with domain-wide and per-CPU options.
    #[test]
    fn active_power_level_ctor() {
        let default_level = ActivePowerLevel::default();
        assert_eq!(default_level.level(), 0);
        assert_eq!(default_level.processing_rate().raw_value(), 0);
        assert_eq!(default_level.active_power_nw(), 0);
        assert_eq!(default_level.static_power_nw(), 0);
        assert_eq!(default_level.power_cost_nw_per_rate(), 0);
        assert_eq!(default_level.control(), ControlInterface::CpuDriver);
        assert_eq!(default_level.control_argument(), 0);
        assert!(default_level.name().is_empty());
        assert!(default_level.targets_power_domain());
        assert!(!default_level.targets_cpus());

        let desc = ActivePowerLevelConfig {
            options: 0,
            processing_rate: 456,
            active_power_nw: 789_000,
            static_power_nw: 12_300,
            control_interface: ControlInterface::CpuDriver,
            control_argument: 12345,
            diagnostic_name: "foobar one two three",
        };

        let level = ActivePowerLevel::new(0, &desc, ActivePowerLevel::USER_PROCESSING_RATE_SCALE);

        assert_eq!(level.level(), 0);
        assert_eq!(
            level.processing_rate().raw_value(),
            ActivePowerLevel::to_processing_rate(
                desc.processing_rate,
                ActivePowerLevel::USER_PROCESSING_RATE_SCALE
            )
            .raw_value()
        );
        assert_eq!(level.active_power_nw(), desc.active_power_nw);
        assert_eq!(level.static_power_nw(), desc.static_power_nw);
        assert_eq!(level.power_cost_nw_per_rate(), 1_730_263);
        assert_eq!(level.control(), desc.control_interface);
        assert_eq!(level.control_argument(), desc.control_argument);
        assert!(level.name() == b"foobar one two three");
        assert!(level.targets_power_domain());
        assert!(!level.targets_cpus());

        let self_scaled = ActivePowerLevel::new(0, &desc, 0);
        assert_eq!(self_scaled.processing_rate().raw_value(), 1 << 31);
        assert_eq!(self_scaled.power_cost_nw_per_rate(), desc.active_power_nw);

        let tiny_rate = ActivePowerLevel::new(
            0,
            &ActivePowerLevelConfig { processing_rate: 1, ..desc },
            3_000_000_000,
        );
        assert_eq!(tiny_rate.processing_rate().raw_value(), 1);

        let zero_rate = ActivePowerLevel::new(
            0,
            &ActivePowerLevelConfig { processing_rate: 0, ..desc },
            ActivePowerLevel::USER_PROCESSING_RATE_SCALE,
        );
        assert_eq!(zero_rate.processing_rate().raw_value(), 0);
        assert_eq!(zero_rate.power_cost_nw_per_rate(), 0);

        let per_cpu_desc =
            ActivePowerLevelConfig { options: POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, ..desc };
        let per_cpu =
            ActivePowerLevel::new(7, &per_cpu_desc, ActivePowerLevel::USER_PROCESSING_RATE_SCALE);
        assert_eq!(per_cpu.level(), 7);
        assert!(!per_cpu.targets_power_domain());
        assert!(per_cpu.targets_cpus());
    }

    /// Verifies `IdlePowerState::new` and `Default` for `ClockGated` and `PowerGated` states.
    #[test]
    fn idle_power_state_ctor() {
        let default_state = IdlePowerState::default();
        assert_eq!(default_state.state_index(), 0);
        assert!(default_state.power_model() == IdlePowerModel::ClockGated);
        assert_eq!(default_state.entry_latency_ns(), 0);
        assert_eq!(default_state.exit_latency_ns(), 0);
        assert_eq!(default_state.min_residency_ns(), 0);
        assert_eq!(default_state.scope_cpu_mask(), 0);
        assert_eq!(default_state.control(), ControlInterface::ArmWfi);
        assert_eq!(default_state.control_argument(), 0);
        assert!(!default_state.local_timer_stops());
        assert!(default_state.name().is_empty());

        let psci_desc = IdlePowerStateConfig {
            power_model: IdlePowerModel::PowerGated { residual_power_nw: 25_000 },
            entry_latency_ns: 110_000,
            exit_latency_ns: 450_000,
            min_residency_ns: 1_900_000,
            scope_cpu_mask: 0xe0,
            flags: IdleStateFlags::LOCAL_TIMER_STOPS,
            control_interface: ControlInterface::ArmPsci,
            control_argument: 0x4100_0043,
            diagnostic_name: "PSCI_DOMAIN",
        };
        let psci = IdlePowerState::new(2, &psci_desc);
        assert_eq!(psci.state_index(), 2);
        assert!(psci.power_model() == IdlePowerModel::PowerGated { residual_power_nw: 25_000 });
        assert_eq!(psci.entry_latency_ns(), 110_000);
        assert_eq!(psci.exit_latency_ns(), 450_000);
        assert_eq!(psci.min_residency_ns(), 1_900_000);
        assert_eq!(psci.scope_cpu_mask(), 0xe0);
        assert_eq!(psci.flags().bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());
        assert!(psci.local_timer_stops());
        assert_eq!(psci.control(), ControlInterface::ArmPsci);
        assert_eq!(psci.control_argument(), 0x4100_0043);
        assert!(psci.name() == b"PSCI_DOMAIN");
    }

    /// Verifies `PowerLevelTransition::new` and `PowerLevelTransition::default`.
    #[test]
    fn power_level_transition_ctor() {
        let default_desc = ProcessorPowerLevelTransition::default();
        assert_eq!(default_desc.latency, 0);
        assert_eq!(default_desc.energy_nj, 0);
        assert_eq!(default_desc.from, 0);
        assert_eq!(default_desc.to, 0);

        let default_transition = PowerLevelTransition::default();
        assert!(!default_transition.is_valid());
        assert!(default_transition == PowerLevelTransition::invalid());
        assert!(PowerLevelTransition::zero().is_valid());

        let desc = ProcessorPowerLevelTransition { latency: 456, energy_nj: 1234, from: 0, to: 1 };

        let transition = PowerLevelTransition::new(&desc);
        assert_eq!(transition.latency(), desc.latency);
        assert_eq!(transition.energy_cost_nj(), desc.energy_nj);
        assert!(transition.is_valid());
    }

    /// Verifies `EnergyModel::create_from_config` sorting, transition remapping, and
    /// `idle_power_nw`.
    #[test]
    fn energy_model_create() {
        let empty_model = EnergyModel::default();
        assert!(empty_model.active_levels().is_empty());
        assert!(empty_model.idle_states().is_empty());
        assert_eq!(empty_model.transitions().num_rows(), 0);
        assert!(empty_model.transitions().get(0).is_none());
        assert_eq!(empty_model.max_processing_rate().raw_value(), 0);
        assert_eq!(empty_model.idle_power_nw(0, Some(0)), 0);
        assert!(
            empty_model.find_active_power_level_for_rate(ProcessingRate::from_raw(0)).is_none()
        );

        // Unsorted active OPPs with non-monotonic `static_power_nw` (1_800_000 nW at rate 2 vs.
        // 1_500_000 nW at rate 4, matching real low-voltage silicon leakage curves).
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 4,
                active_power_nw: 100,
                static_power_nw: 3_000_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "opp_high_power",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 2,
                active_power_nw: 50,
                static_power_nw: 1_800_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 2,
                diagnostic_name: "opp_low",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 4,
                active_power_nw: 80,
                static_power_nw: 1_500_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "opp_mid",
            },
        ];

        // Unsorted idle states: C3 (deepest), WFI (ClockGated), C2 (shallow PowerGated).
        let idle_states = [
            IdlePowerStateConfig {
                power_model: IdlePowerModel::PowerGated { residual_power_nw: 0 },
                entry_latency_ns: 200_000,
                exit_latency_ns: 500_000,
                min_residency_ns: 2_000_000,
                scope_cpu_mask: 0x1c,
                flags: IdleStateFlags::LOCAL_TIMER_STOPS,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 0x4100_0033,
                diagnostic_name: "c3",
            },
            IdlePowerStateConfig {
                power_model: IdlePowerModel::ClockGated,
                entry_latency_ns: 0,
                exit_latency_ns: 0,
                min_residency_ns: 0,
                scope_cpu_mask: 0,
                flags: IdleStateFlags::empty(),
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: "wfi",
            },
            IdlePowerStateConfig {
                power_model: IdlePowerModel::PowerGated { residual_power_nw: 50_000 },
                entry_latency_ns: 40_000,
                exit_latency_ns: 90_000,
                min_residency_ns: 400_000,
                scope_cpu_mask: 0,
                flags: IdleStateFlags::LOCAL_TIMER_STOPS,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 0x4000_0002,
                diagnostic_name: "c2",
            },
        ];

        // Active transitions indexed by original `active_levels` positions:
        // - orig 1 (`opp_low`, sorted 0) -> orig 2 (`opp_mid`, sorted 1): latency 11, energy 22
        // - orig 2 (`opp_mid`, sorted 1) -> orig 0 (`opp_high_power`, sorted 2): latency 33,
        //   energy 44
        let transitions_desc = [
            ProcessorPowerLevelTransition { latency: 11, energy_nj: 22, from: 1, to: 2 },
            ProcessorPowerLevelTransition { latency: 33, energy_nj: 44, from: 2, to: 0 },
        ];

        let config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active_levels,
            idle_states: &idle_states,
            active_transitions: &transitions_desc,
        };
        let model = EnergyModel::create_from_config(&config, 0)
            .expect("EnergyModel::create_from_config should succeed");

        // Sorted active levels: 0 = opp_low, 1 = opp_mid, 2 = opp_high_power.
        let sorted_active = model.active_levels();
        assert_eq!(sorted_active.len(), 3);
        assert!(sorted_active[0].name() == b"opp_low");
        assert!(sorted_active[1].name() == b"opp_mid");
        assert!(sorted_active[2].name() == b"opp_high_power");
        for (i, lvl) in sorted_active.iter().enumerate() {
            assert_eq!(lvl.level(), i as u8);
            assert!(
                model.find_active_power_level(lvl.control(), lvl.control_argument())
                    == Some(i as u8)
            );
            let mut out_idx = u8::MAX;
            assert!(rust_energy_model_find_active_power_level(
                &model,
                lvl.control() as u64,
                lvl.control_argument(),
                &mut out_idx,
            ));
            assert_eq!(out_idx, i as u8);
        }
        assert!(
            sorted_active[0].processing_rate().raw_value()
                <= sorted_active[1].processing_rate().raw_value()
        );
        assert!(sorted_active[1].active_power_nw() <= sorted_active[2].active_power_nw());
        // Non-monotonic static power preserved without affecting sort order.
        assert_eq!(sorted_active[0].static_power_nw(), 1_800_000);
        assert_eq!(sorted_active[1].static_power_nw(), 1_500_000);
        assert_eq!(sorted_active[2].static_power_nw(), 3_000_000);

        let active_for_zero = model.find_active_power_level_for_rate(ProcessingRate::from_raw(0));
        assert!(active_for_zero.is_some());
        assert_eq!(active_for_zero.unwrap().level(), 0);

        // Rate 4 matches both sorted_active[1] (power 80) and sorted_active[2] (power 100);
        // `find_active_power_level_for_rate` picks the lower-power level 1.
        let active_for_max = model.find_active_power_level_for_rate(model.to_processing_rate(4));
        assert!(active_for_max.is_some());
        assert_eq!(active_for_max.unwrap().level(), 1);

        // Sorted idle states: 0 = wfi, 1 = c2, 2 = c3.
        let sorted_idle = model.idle_states();
        assert_eq!(sorted_idle.len(), 3);
        assert!(sorted_idle[0].name() == b"wfi");
        assert!(sorted_idle[1].name() == b"c2");
        assert!(sorted_idle[2].name() == b"c3");
        for (i, state) in sorted_idle.iter().enumerate() {
            assert_eq!(state.state_index(), i as u8);
        }

        // TransitionMatrix is N_active x N_active (3 x 3).
        let transitions = model.transitions();
        assert_eq!(transitions.num_rows(), 3);
        assert!(transitions.get(3).is_none());
        assert_eq!(transitions[0][1].latency(), 11);
        assert_eq!(transitions[0][1].energy_cost_nj(), 22);
        assert_eq!(transitions[1][2].latency(), 33);
        assert_eq!(transitions[1][2].energy_cost_nj(), 44);
        assert!(!transitions[0][2].is_valid());

        // `idle_power_nw` tracks per-OPP static leakage for `ClockGated` WFI and fixed residual
        // power for `PowerGated` states.
        assert_eq!(model.idle_power_nw(0, None), 0);
        assert_eq!(model.idle_power_nw(0, Some(0)), 1_800_000);
        assert_eq!(model.idle_power_nw(0, Some(1)), 1_500_000);
        assert_eq!(model.idle_power_nw(0, Some(2)), 3_000_000);
        assert_eq!(model.idle_power_nw(0, Some(3)), 0);
        assert_eq!(model.idle_power_nw(1, None), 50_000);
        assert_eq!(model.idle_power_nw(1, Some(0)), 50_000);
        assert_eq!(model.idle_power_nw(1, Some(2)), 50_000);
        assert_eq!(model.idle_power_nw(2, Some(2)), 0);
        assert_eq!(model.idle_power_nw(99, Some(0)), 0);

        assert!(model.find_active_power_level(ControlInterface::CpuDriver, 495).is_none());
        let mut out_idx = 0u8;
        assert!(!rust_energy_model_find_active_power_level(&model, 495, 0, &mut out_idx));
    }

    /// Verifies active transition remapping across arbitrary sort permutations and ties.
    #[test]
    fn power_model_create_remaps_transitions_for_any_sort_order() {
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 3,
                active_power_nw: 30,
                static_power_nw: 3,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "high",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 1,
                active_power_nw: 10,
                static_power_nw: 1,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "low",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 2,
                active_power_nw: 20,
                static_power_nw: 2,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 2,
                diagnostic_name: "mid_a",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 2,
                active_power_nw: 20,
                static_power_nw: 2,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 3,
                diagnostic_name: "mid_b",
            },
        ];

        let transitions = [
            ProcessorPowerLevelTransition { latency: 10, energy_nj: 1, from: 1, to: 0 },
            ProcessorPowerLevelTransition { latency: 20, energy_nj: 2, from: 2, to: 0 },
            ProcessorPowerLevelTransition { latency: 30, energy_nj: 3, from: 3, to: 0 },
        ];

        let config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active_levels,
            idle_states: &[],
            active_transitions: &transitions,
        };
        let model = EnergyModel::create_from_config(&config, 0)
            .expect("EnergyModel::create_from_config should succeed");

        assert_eq!(model.active_levels().len(), 4);
        assert!(model.active_levels()[0].name() == b"low");
        assert_eq!(model.active_levels()[0].level(), 0);
        assert!(model.active_levels()[3].name() == b"high");
        assert_eq!(model.active_levels()[3].level(), 3);

        let clamped_high = model.find_active_power_level_for_rate(ProcessingRate::from(
            ffl::Fixed::<i64, 31>::from_integer(2),
        ));
        assert!(clamped_high.is_some());
        assert_eq!(clamped_high.unwrap().level(), 3);

        let mid_a = model.find_active_power_level(ControlInterface::CpuDriver, 2).unwrap();
        let mid_b = model.find_active_power_level(ControlInterface::CpuDriver, 3).unwrap();
        assert_eq!(model.transitions()[0][3].latency(), 10);
        assert_eq!(model.transitions()[mid_a as usize][3].latency(), 20);
        assert_eq!(model.transitions()[mid_b as usize][3].latency(), 30);
        assert!(!model.transitions()[3][0].is_valid());
    }

    /// Verifies `EnergyModel` orders `ClockGated` before `PowerGated` and breaks timing ties
    /// deterministically.
    #[test]
    fn idle_state_sort_keys_and_tie_breakers() {
        let active = [ActivePowerLevelConfig {
            options: 0,
            processing_rate: 100,
            active_power_nw: 1_000,
            static_power_nw: 100,
            control_interface: ControlInterface::CpuDriver,
            control_argument: 0,
            diagnostic_name: "opp",
        }];
        let base_pg = IdlePowerStateConfig {
            power_model: IdlePowerModel::PowerGated { residual_power_nw: 0 },
            entry_latency_ns: 20,
            exit_latency_ns: 50,
            min_residency_ns: 100,
            scope_cpu_mask: 0,
            flags: IdleStateFlags::empty(),
            control_interface: ControlInterface::ArmPsci,
            control_argument: 0,
            diagnostic_name: "",
        };
        let idle = [
            // Tied timing with `tie_arg_1`, larger `control_interface` (`RiscvSbi > ArmPsci`) ->
            // last.
            IdlePowerStateConfig {
                control_interface: ControlInterface::RiscvSbi,
                control_argument: 1,
                diagnostic_name: "tie_ctrl_sbi",
                ..base_pg
            },
            // Tied timing + control with `tie_arg_1`, larger `control_argument` -> earlier than
            // `RiscvSbi`.
            IdlePowerStateConfig { control_argument: 2, diagnostic_name: "tie_arg_2", ..base_pg },
            // Tied residency + exit_latency with `tie_arg_1`, smaller `entry_latency_ns` ->
            // earlier.
            IdlePowerStateConfig {
                entry_latency_ns: 10,
                control_argument: 9,
                diagnostic_name: "smaller_entry",
                ..base_pg
            },
            // Tied timing + control with `tie_arg_2`, smaller `control_argument` -> earlier.
            IdlePowerStateConfig { control_argument: 1, diagnostic_name: "tie_arg_1", ..base_pg },
            // Tied residency with `smaller_entry`, smaller `exit_latency_ns` -> earlier.
            IdlePowerStateConfig {
                entry_latency_ns: 99,
                exit_latency_ns: 30,
                control_argument: 8,
                diagnostic_name: "smaller_exit",
                ..base_pg
            },
            // `PowerGated` with zero residency/latencies still sorts after `ClockGated`.
            IdlePowerStateConfig {
                entry_latency_ns: 0,
                exit_latency_ns: 0,
                min_residency_ns: 0,
                control_argument: 7,
                diagnostic_name: "pg_zero",
                ..base_pg
            },
            // `ClockGated` with non-zero residency/latencies still sorts to index 0.
            IdlePowerStateConfig {
                power_model: IdlePowerModel::ClockGated,
                entry_latency_ns: 500,
                exit_latency_ns: 500,
                min_residency_ns: 500,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: "cg_nonzero",
                ..base_pg
            },
        ];

        let config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active,
            idle_states: &idle,
            active_transitions: &[],
        };
        let model = EnergyModel::create_from_config(&config, 0)
            .expect("EnergyModel::create_from_config should succeed");
        let states = model.idle_states();
        assert_eq!(states.len(), 7);
        assert!(states[0].name() == b"cg_nonzero");
        assert!(states[1].name() == b"pg_zero");
        assert!(states[2].name() == b"smaller_exit");
        assert!(states[3].name() == b"smaller_entry");
        assert!(states[4].name() == b"tie_arg_1");
        assert!(states[5].name() == b"tie_arg_2");
        assert!(states[6].name() == b"tie_ctrl_sbi");
        for (idx, state) in states.iter().enumerate() {
            assert_eq!(state.state_index(), idx as u8);
        }
    }

    /// Verifies `EnergyModel::create_from_config` with empty transitions and empty idle states.
    #[test]
    fn energy_model_create_with_empty_transitions_and_empty_idle_states() {
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 4,
                active_power_nw: 8,
                static_power_nw: 1,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "0",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 8,
                active_power_nw: 16,
                static_power_nw: 2,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "1",
            },
        ];
        let config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active_levels,
            idle_states: &[],
            active_transitions: &[],
        };

        let model = EnergyModel::create_from_config(&config, 0)
            .expect("EnergyModel::create_from_config should succeed");

        assert!(model.idle_states().is_empty());
        assert_eq!(model.idle_power_nw(0, Some(0)), 0);
        for row in 0..model.active_levels().len() {
            for col in 0..model.active_levels().len() {
                let transition = model.transitions()[row][col];
                assert!(transition == PowerLevelTransition::zero());
            }
        }
    }

    /// Verifies heterogeneous multi-domain scaling across little and big domains.
    #[test]
    fn heterogeneous_multi_domain_scaling() {
        let wfi = [IdlePowerStateConfig::ARM_WFI];
        let little_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 50,
                active_power_nw: 100,
                static_power_nw: 10,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "opp0",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 150,
                active_power_nw: 300,
                static_power_nw: 20,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "opp1",
            },
        ];
        let big_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 500,
                active_power_nw: 1000,
                static_power_nw: 50,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "opp0",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 1000,
                active_power_nw: 2500,
                static_power_nw: 90,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "opp1",
            },
        ];

        const SYSTEM_MAX_PROCESSING_RATE: u64 = 1000;

        let little_config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 0x03,
            active_levels: &little_levels,
            idle_states: &wfi,
            active_transitions: &[],
        };
        let little_model =
            EnergyModel::create_from_config(&little_config, SYSTEM_MAX_PROCESSING_RATE)
                .expect("little model should succeed");
        assert_eq!(little_model.max_processing_rate_scale(), SYSTEM_MAX_PROCESSING_RATE);
        let levels = little_model.active_levels();
        assert_eq!(levels.len(), 2);
        assert_eq!(
            levels[0].processing_rate().raw_value(),
            ActivePowerLevel::to_processing_rate(50, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        assert_eq!(
            levels[1].processing_rate().raw_value(),
            ActivePowerLevel::to_processing_rate(150, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        assert_eq!(little_model.from_processing_rate(levels[1].processing_rate()), 150);

        let big_config = PowerDomainConfig {
            domain_id: 1,
            cpu_mask: 0x0c,
            active_levels: &big_levels,
            idle_states: &wfi,
            active_transitions: &[],
        };
        let big_model = EnergyModel::create_from_config(&big_config, SYSTEM_MAX_PROCESSING_RATE)
            .expect("big model should succeed");
        let big_model_levels = big_model.active_levels();
        assert_eq!(big_model_levels.len(), 2);
        assert_eq!(
            big_model_levels[1].processing_rate().raw_value(),
            ProcessingRate::from(ffl::Fixed::<i64, 31>::from_integer(1)).raw_value()
        );
    }

    /// Verifies `PowerDomainSet::find_by_domain_id` and `PowerDomainSet` lookup helpers.
    #[test]
    fn find_domain() {
        let domain_0 = make_domain(0, &[1, 2, 3]);
        let domain_1 = make_domain(1, &[4, 5, 6]);
        let domain_2 = make_domain(2, &[7, 8, 9]);

        let domains = [
            Some(RefPtr::clone(&domain_0)),
            Some(RefPtr::clone(&domain_1)),
            Some(RefPtr::clone(&domain_2)),
        ];
        let domain_set = PowerDomainSet::create(&domains).expect("PowerDomainSet::create");

        for domain in [&domain_0, &domain_1, &domain_2] {
            let found_domain = domain_set.find_by_domain_id(domain.id());
            assert!(found_domain.is_some_and(|found_domain| RefPtr::ptr_eq(found_domain, domain)));
        }
        assert!(domain_set.find_by_domain_id(112345567).is_none());

        assert!(domain_set.find_by_cpu_num(5).is_some_and(|d| RefPtr::ptr_eq(d, &domain_1)));
        assert!(domain_set.find_by_cpu_num(0).is_none());
        assert_eq!(domain_set.lookup_power_cost(0, ProcessingRate::from_raw(0)), 0);

        let min_cost = domain_0.model().active_levels()[0].power_cost_nw_per_rate();
        assert_eq!(domain_set.lookup_power_cost(1, ProcessingRate::from_raw(0)), min_cost);
        let top_level = domain_0.model().active_levels().last().expect("non-empty active levels");
        let max_rate_above = ProcessingRate::from(ffl::Fixed::<i64, 31>::from_integer(2));
        assert_eq!(
            domain_set.lookup_power_cost(2, max_rate_above),
            top_level.power_cost_nw_per_rate()
        );
    }

    /// Verifies `PowerDomainSet::create` validation rules.
    #[test]
    fn create_validation() {
        let domain_0 = make_domain(0, &[0, 1, 2]);
        let domain_1 = make_domain(1, &[4, 5, 6]);
        let domain_2 = make_domain(2, &[8, 9, 10]);

        let valid_domains = [
            Some(RefPtr::clone(&domain_0)),
            Some(RefPtr::clone(&domain_1)),
            Some(RefPtr::clone(&domain_2)),
        ];
        let domain_set = PowerDomainSet::create(&valid_domains).expect("PowerDomainSet::create");
        assert_eq!(domain_set.count(), 3);

        let mut visited = 0usize;
        let mut all_in_range = true;
        domain_set.visit(|visited_domain| {
            visited += 1;
            all_in_range &= [&domain_0, &domain_1, &domain_2]
                .iter()
                .any(|domain| RefPtr::ptr_eq(domain, visited_domain));
        });
        assert_eq!(visited, 3);
        assert!(all_in_range);

        // Duplicate domain ID.
        let duplicate_id_domains = [Some(make_domain(0, &[0, 1])), Some(make_domain(0, &[2, 3]))];
        assert_eq!(PowerDomainSet::create(&duplicate_id_domains).err(), Some(Status::INVALID_ARGS));

        // Overlapping CPU mask.
        let overlapping_domains = [Some(make_domain(0, &[0, 1])), Some(make_domain(1, &[1, 2]))];
        assert_eq!(PowerDomainSet::create(&overlapping_domains).err(), Some(Status::INVALID_ARGS));

        // Empty CPU mask.
        let zero_cpu_domain = [Some(make_domain(0, &[]))];
        assert_eq!(PowerDomainSet::create(&zero_cpu_domain).err(), Some(Status::INVALID_ARGS));

        // Null domain reference.
        let null_domains: [Option<RefPtr<PowerDomain>>; 1] = [None];
        assert_eq!(PowerDomainSet::create(&null_domains).err(), Some(Status::INVALID_ARGS));

        // Exceeding MAX_POWER_DOMAINS (limit 4).
        let too_many_domains = [
            Some(make_domain(0, &[0])),
            Some(make_domain(1, &[1])),
            Some(make_domain(2, &[2])),
            Some(make_domain(3, &[3])),
            Some(make_domain(4, &[4])),
        ];
        assert_eq!(PowerDomainSet::create(&too_many_domains).err(), Some(Status::OUT_OF_RANGE));
    }

    /// Verifies `EnergyModel::create_from_config` name truncation and scaled cost calculation.
    #[test]
    fn create_from_config() {
        const LONG_NAME: &str = "0123456789abcdef0123456789abcdef_overflow";
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 200,
                active_power_nw: 2_500_000,
                static_power_nw: 300_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "OPP_FAST",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 100,
                active_power_nw: 1_000_000,
                static_power_nw: 400_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 7,
                diagnostic_name: LONG_NAME,
            },
        ];
        let idle_states = [
            IdlePowerStateConfig {
                power_model: IdlePowerModel::PowerGated { residual_power_nw: 0 },
                entry_latency_ns: 110_000,
                exit_latency_ns: 450_000,
                min_residency_ns: 1_900_000,
                scope_cpu_mask: 0xe0,
                flags: IdleStateFlags::LOCAL_TIMER_STOPS,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 0x4100_0043,
                diagnostic_name: "C3\0ignored",
            },
            IdlePowerStateConfig {
                power_model: IdlePowerModel::ClockGated,
                entry_latency_ns: 0,
                exit_latency_ns: 0,
                min_residency_ns: 0,
                scope_cpu_mask: 0,
                flags: IdleStateFlags::empty(),
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: "",
            },
        ];
        // Original index 1 (rate 100, sorted 0) -> original index 0 (rate 200, sorted 1).
        let active_transitions =
            [ProcessorPowerLevelTransition { from: 1, to: 0, latency: 50_000, energy_nj: 42 }];
        let config = PowerDomainConfig {
            domain_id: 2,
            cpu_mask: 0x60,
            active_levels: &active_levels,
            idle_states: &idle_states,
            active_transitions: &active_transitions,
        };

        let model = EnergyModel::create_from_config(&config, 0).expect("create_from_config");
        assert_eq!(model.active_levels().len(), 2);
        assert_eq!(model.idle_states().len(), 2);

        let wfi = &model.idle_states()[0];
        assert_eq!(wfi.state_index(), 0);
        assert!(wfi.power_model() == IdlePowerModel::ClockGated);
        assert_eq!(wfi.control(), ControlInterface::ArmWfi);
        assert!(!wfi.local_timer_stops());
        assert!(wfi.name().is_empty());

        let c3 = &model.idle_states()[1];
        assert_eq!(c3.state_index(), 1);
        assert!(c3.power_model() == IdlePowerModel::PowerGated { residual_power_nw: 0 });
        assert_eq!(c3.scope_cpu_mask(), 0xe0);
        assert!(c3.local_timer_stops());
        assert!(c3.name() == b"C3");

        let slow_opp = &model.active_levels()[0];
        assert_eq!(slow_opp.level(), 0);
        assert!(slow_opp.name() == &LONG_NAME.as_bytes()[..ZX_MAX_NAME_LEN - 1]);

        let transition = model.transitions()[0][1];
        assert!(transition.is_valid());
        assert_eq!(transition.latency(), 50_000);
        assert_eq!(transition.energy_cost_nj(), 42);
        assert!(!model.transitions()[1][0].is_valid());

        assert_eq!(model.idle_power_nw(0, Some(0)), 400_000);
        assert_eq!(model.idle_power_nw(0, Some(1)), 300_000);
        assert_eq!(model.idle_power_nw(1, Some(0)), 0);

        const SYSTEM_MAX_RATE: u64 = 400;
        let scaled =
            EnergyModel::create_from_config(&config, SYSTEM_MAX_RATE).expect("scaled model");
        assert_eq!(scaled.max_processing_rate_scale(), SYSTEM_MAX_RATE);
        assert_eq!(scaled.active_levels()[1].processing_rate().raw_value(), 1i64 << 30);
        assert_eq!(scaled.active_levels()[0].power_cost_nw_per_rate(), 4_000_000);
    }

    /// Verifies `EnergyModel::create_from_config` accepts and applies a full `N^2` transition
    /// table.
    #[test]
    fn create_from_config_accepts_full_transition_table() {
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 100,
                active_power_nw: 1000,
                static_power_nw: 50,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "OPP0",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 200,
                active_power_nw: 2000,
                static_power_nw: 80,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "OPP1",
            },
        ];
        let full_transitions = [
            ProcessorPowerLevelTransition { from: 0, to: 0, latency: 0, energy_nj: 0 },
            ProcessorPowerLevelTransition { from: 0, to: 1, latency: 10, energy_nj: 20 },
            ProcessorPowerLevelTransition { from: 1, to: 0, latency: 15, energy_nj: 25 },
            ProcessorPowerLevelTransition { from: 1, to: 1, latency: 0, energy_nj: 0 },
        ];
        let config = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &active_levels,
            idle_states: &[],
            active_transitions: &full_transitions,
        };
        let model = EnergyModel::create_from_config(&config, 0).expect("create_from_config");
        assert_eq!(model.transitions()[0][1].latency(), 10);
        assert_eq!(model.transitions()[0][1].energy_cost_nj(), 20);
        assert_eq!(model.transitions()[1][0].latency(), 15);
        assert_eq!(model.transitions()[1][0].energy_cost_nj(), 25);
    }

    /// Verifies `EnergyModel::create_from_config` bounds and rejects invalid active, idle, and
    /// transition inputs.
    #[test]
    fn create_from_config_errors() {
        let valid_level = [ActivePowerLevelConfig {
            options: 0,
            processing_rate: 100,
            active_power_nw: 1000,
            static_power_nw: 50,
            control_interface: ControlInterface::CpuDriver,
            control_argument: 0,
            diagnostic_name: "OPP",
        }];
        let zero_rate_level = [ActivePowerLevelConfig { processing_rate: 0, ..valid_level[0] }];
        let valid_idle = IdlePowerStateConfig::ARM_WFI;
        let neg_entry_idle = [IdlePowerStateConfig { entry_latency_ns: -1, ..valid_idle }];
        let neg_exit_idle = [IdlePowerStateConfig { exit_latency_ns: -1, ..valid_idle }];
        let neg_residency_idle = [IdlePowerStateConfig { min_residency_ns: -1, ..valid_idle }];
        let bad_flags_idle = [IdlePowerStateConfig {
            flags: IdleStateFlags::from_bits_retain(1 << 30),
            ..valid_idle
        }];

        // Exactly 256 active levels succeeds (`level` fits in `u8`); 257 is rejected.
        let mut too_many_levels = Vector::new();
        assert!(too_many_levels.reserve(257).is_ok());
        for i in 0..257usize {
            assert!(
                too_many_levels
                    .push_back(ActivePowerLevelConfig {
                        processing_rate: (i + 1) as u64,
                        control_argument: i as u64,
                        ..valid_level[0]
                    })
                    .is_ok()
            );
        }
        let max_active_cfg = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &too_many_levels[..256],
            idle_states: &[],
            active_transitions: &[],
        };
        let max_active_model =
            EnergyModel::create_from_config(&max_active_cfg, 0).expect("256 active levels");
        assert_eq!(max_active_model.active_levels().len(), 256);
        assert_eq!(max_active_model.active_levels()[255].level(), 255);

        // Exactly 256 idle states succeeds (`state_index` fits in `u8`); 257 is rejected.
        let mut too_many_idle = Vector::new();
        assert!(too_many_idle.reserve(257).is_ok());
        for i in 0..257usize {
            assert!(
                too_many_idle
                    .push_back(IdlePowerStateConfig { control_argument: i as u64, ..valid_idle })
                    .is_ok()
            );
        }
        let max_idle_cfg = PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &valid_level,
            idle_states: &too_many_idle[..256],
            active_transitions: &[],
        };
        let max_idle_model =
            EnergyModel::create_from_config(&max_idle_cfg, 0).expect("256 idle states");
        assert_eq!(max_idle_model.idle_states().len(), 256);
        assert_eq!(max_idle_model.idle_states()[255].state_index(), 255);

        let too_many_transitions = [
            ProcessorPowerLevelTransition { from: 0, to: 0, latency: 1, energy_nj: 1 },
            ProcessorPowerLevelTransition { from: 0, to: 0, latency: 1, energy_nj: 1 },
        ];
        let from_out_of_range =
            [ProcessorPowerLevelTransition { from: 1, to: 0, latency: 1, energy_nj: 1 }];
        let to_out_of_range =
            [ProcessorPowerLevelTransition { from: 0, to: 1, latency: 1, energy_nj: 1 }];

        let cases = [
            (
                "empty active levels",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &[],
                    idle_states: &[],
                    active_transitions: &[],
                },
            ),
            (
                "zero processing rate",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &zero_rate_level,
                    idle_states: &[],
                    active_transitions: &[],
                },
            ),
            (
                "257 active levels",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &too_many_levels,
                    idle_states: &[],
                    active_transitions: &[],
                },
            ),
            (
                "257 idle states",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &too_many_idle,
                    active_transitions: &[],
                },
            ),
            (
                "negative entry latency",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &neg_entry_idle,
                    active_transitions: &[],
                },
            ),
            (
                "negative exit latency",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &neg_exit_idle,
                    active_transitions: &[],
                },
            ),
            (
                "negative min residency",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &neg_residency_idle,
                    active_transitions: &[],
                },
            ),
            (
                "unknown idle state flags",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &bad_flags_idle,
                    active_transitions: &[],
                },
            ),
            (
                "too many transitions",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &[],
                    active_transitions: &too_many_transitions,
                },
            ),
            (
                "from index out of range",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &[],
                    active_transitions: &from_out_of_range,
                },
            ),
            (
                "to index out of range",
                PowerDomainConfig {
                    domain_id: 0,
                    cpu_mask: 1,
                    active_levels: &valid_level,
                    idle_states: &[],
                    active_transitions: &to_out_of_range,
                },
            ),
        ];

        for (name, config) in &cases {
            assert_eq!(
                EnergyModel::create_from_config(config, 0).err(),
                Some(Status::INVALID_ARGS),
                "{}",
                *name
            );
        }
    }
}

/// Kernel-only unit tests for power domain registration and CPU rate limit validation.
#[cfg(ktest)]
#[unittest::suite(name = "power_domain_kernel_rust")]
mod kernel_tests {
    use unittest::{assert_true, expect_err, expect_true};

    use super::{
        ActivePowerLevelConfig, ControlInterface, PerCpu, PowerDomainConfig, PowerDomainSet,
        SMP_MAX_CPUS, Status, set_rate_limits,
    };

    /// Verifies `set_rate_limits` rejects CPU masks with bits beyond `processor_count`.
    #[test]
    fn set_rate_limits_rejects_out_of_range_cpu_mask() {
        let processor_count = PerCpu::processor_count().min(SMP_MAX_CPUS);
        if processor_count < 64 {
            expect_err!(set_rate_limits(1u64 << processor_count, 0, 1000), Status::INVALID_ARGS);
            expect_err!(set_rate_limits(u64::MAX, 0, 1000), Status::INVALID_ARGS);
        }
    }

    /// Verifies `PowerDomainSet::create_from_configs` input validation branches.
    #[test]
    fn power_domain_set_create_from_configs_validation() {
        let empty = PowerDomainSet::create_from_configs(&[]);
        assert_true!(empty.is_ok());
        expect_true!(empty.unwrap().is_empty());

        let valid_level = [ActivePowerLevelConfig {
            options: 0,
            processing_rate: 100,
            active_power_nw: 1000,
            static_power_nw: 50,
            control_interface: ControlInterface::CpuDriver,
            control_argument: 0,
            diagnostic_name: "OPP",
        }];
        let zero_mask = [PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 0,
            active_levels: &valid_level,
            idle_states: &[],
            active_transitions: &[],
        }];
        expect_err!(PowerDomainSet::create_from_configs(&zero_mask), Status::INVALID_ARGS);

        let empty_levels = [PowerDomainConfig {
            domain_id: 0,
            cpu_mask: 1,
            active_levels: &[],
            idle_states: &[],
            active_transitions: &[],
        }];
        expect_err!(PowerDomainSet::create_from_configs(&empty_levels), Status::INVALID_ARGS);

        let too_many = [empty_levels[0]; PowerDomainSet::MAX_POWER_DOMAINS + 1];
        expect_err!(PowerDomainSet::create_from_configs(&too_many), Status::OUT_OF_RANGE);
    }
}
