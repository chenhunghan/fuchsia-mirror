// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! CPU energy model, power levels, transition costs, and power domains.

use core::ffi::c_char;
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

#[cfg(ktest)]
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
    crate::kernel::mp::SMP_MAX_CPUS,
    crate::kernel::percpu::PerCpu,
    crate::kernel::scheduler::Scheduler,
    crate::kernel::types::{INVALID_CPU, cpu_mask_t, cpu_num_to_mask, remove_cpu_from_mask},
    debug::dprintf,
    fbl::Array,
    zx_types::{ZX_CPU_PERF_LIMIT_TYPE_RATE, zx_cpu_perf_limit_t},
};

/// Power level option bit indicating that a power level may be transitioned on a per-CPU basis
/// without affecting other CPUs in the same power domain.
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

/// FFI representation of a processor power level for energy model registration.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ProcessorPowerLevelFfi {
    /// Power level options (e.g. [`POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT`]).
    pub options: u32,
    /// Processing rate associated with this power level.
    pub processing_rate: u64,
    /// Power consumption coefficient in nanowatts.
    pub power_coefficient_nw: u64,
    /// Power control interface (0 for [`ControlInterface::ArmWfi`], 1 for
    /// [`ControlInterface::CpuDriver`]).
    pub control_interface: u32,
    /// Control argument passed to the power control interface.
    pub control_argument: u64,
    /// Optional diagnostic name string pointer for debugging/inspect.
    pub diagnostic_name: *const c_char,
}

zr::static_assert!(size_of::<ProcessorPowerLevelFfi>() == 48);
zr::static_assert!(align_of::<ProcessorPowerLevelFfi>() == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, options) == 0);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, processing_rate) == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, power_coefficient_nw) == 16);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, control_interface) == 24);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, control_argument) == 32);
zr::static_assert!(offset_of!(ProcessorPowerLevelFfi, diagnostic_name) == 40);

/// FFI representation of a power domain configuration for energy model registration.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerDomainConfigFfi {
    /// Domain identifier.
    pub domain_id: u32,
    /// Bitmask of CPU core IDs belonging to this power domain.
    pub cpu_mask: u64,
    /// Pointer to the array of supported power levels.
    pub levels: *const ProcessorPowerLevelFfi,
    /// Number of power levels in the `levels` array.
    pub level_count: usize,
}

zr::static_assert!(size_of::<PowerDomainConfigFfi>() == 32);
zr::static_assert!(align_of::<PowerDomainConfigFfi>() == 8);
zr::static_assert!(offset_of!(PowerDomainConfigFfi, domain_id) == 0);
zr::static_assert!(offset_of!(PowerDomainConfigFfi, cpu_mask) == 8);
zr::static_assert!(offset_of!(PowerDomainConfigFfi, levels) == 16);
zr::static_assert!(offset_of!(PowerDomainConfigFfi, level_count) == 24);

/// Input descriptor for constructing a [`PowerLevel`].
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ProcessorPowerLevel {
    /// Power level options (e.g. [`POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT`]).
    pub options: u64,
    /// Processing rate associated with this power level.
    pub processing_rate: u64,
    /// Power consumption coefficient in nanowatts.
    pub power_coefficient_nw: u64,
    /// Control interface handling transitions to this power level.
    pub control_interface: ControlInterface,
    /// Control argument passed to the control interface.
    pub control_argument: u64,
    /// Optional NUL-terminated diagnostic name string pointer.
    pub diagnostic_name: *const c_char,
}

impl Default for ProcessorPowerLevel {
    #[inline]
    fn default() -> Self {
        Self {
            options: 0,
            processing_rate: 0,
            power_coefficient_nw: 0,
            control_interface: ControlInterface::CpuDriver,
            control_argument: 0,
            diagnostic_name: core::ptr::null(),
        }
    }
}

zr::static_assert!(size_of::<ProcessorPowerLevel>() == 48);
zr::static_assert!(align_of::<ProcessorPowerLevel>() == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevel, options) == 0);
zr::static_assert!(offset_of!(ProcessorPowerLevel, processing_rate) == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevel, power_coefficient_nw) == 16);
zr::static_assert!(offset_of!(ProcessorPowerLevel, control_interface) == 24);
zr::static_assert!(offset_of!(ProcessorPowerLevel, control_argument) == 32);
zr::static_assert!(offset_of!(ProcessorPowerLevel, diagnostic_name) == 40);

/// Input descriptor for constructing a [`PowerLevelTransition`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessorPowerLevelTransition {
    /// Time required for the transition to take effect.
    pub latency: zx_duration_mono_t,
    /// Energy consumed in nanojoules to perform the transition.
    pub energy_nj: u64,
    /// Source power level index.
    pub from: u8,
    /// Target power level index.
    pub to: u8,
}

zr::static_assert!(size_of::<ProcessorPowerLevelTransition>() == 24);
zr::static_assert!(align_of::<ProcessorPowerLevelTransition>() == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevelTransition, latency) == 0);
zr::static_assert!(offset_of!(ProcessorPowerLevelTransition, energy_nj) == 8);
zr::static_assert!(offset_of!(ProcessorPowerLevelTransition, from) == 16);
zr::static_assert!(offset_of!(ProcessorPowerLevelTransition, to) == 17);

/// Power level classification.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerLevelType {
    /// Entity is not eligible for active work.
    Idle = 0,
    /// Entity is eligible for work, but the rate at which work is completed is determined by the
    /// active power level.
    Active = 1,
}

/// Kernel representation of `zx_processor_power_level_t` with useful accessors and option support.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PowerLevel {
    /// Options.
    options: u64,
    /// Control interface used to transition to this level.
    control: ControlInterface,
    /// Argument to be provided to the control interface.
    control_argument: u64,
    /// Processing rate.
    processing_rate: ProcessingRate,
    /// Power coefficient in nanowatts.
    power_coefficient_nw: u64,
    /// Power cost. Memoized value of `power_coefficient_nw / processing_rate`.
    power_cost_nw_per_rate: u64,
    /// Diagnostic name.
    name: [u8; ZX_MAX_NAME_LEN],
    /// Length of the diagnostic name.
    name_len: usize,
    /// Level as described in the model shared with user.
    level: u8,
}

zr::static_assert!(size_of::<PowerLevel>() == 96);
zr::static_assert!(align_of::<PowerLevel>() == 8);
zr::static_assert!(offset_of!(PowerLevel, options) == 0);
zr::static_assert!(offset_of!(PowerLevel, control) == 8);
zr::static_assert!(offset_of!(PowerLevel, control_argument) == 16);
zr::static_assert!(offset_of!(PowerLevel, processing_rate) == 24);
zr::static_assert!(offset_of!(PowerLevel, power_coefficient_nw) == 32);
zr::static_assert!(offset_of!(PowerLevel, power_cost_nw_per_rate) == 40);
zr::static_assert!(offset_of!(PowerLevel, name) == 48);
zr::static_assert!(offset_of!(PowerLevel, name_len) == 80);
zr::static_assert!(offset_of!(PowerLevel, level) == 88);

impl PowerLevel {
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

    /// Constructs a `PowerLevel` from a `ProcessorPowerLevel` descriptor.
    ///
    /// # Safety
    ///
    /// If `level.diagnostic_name` is non-null, it must point to a valid NUL-terminated C string
    /// or at least `ZX_MAX_NAME_LEN - 1` readable bytes.
    pub unsafe fn new(
        level_index: u8,
        level: &ProcessorPowerLevel,
        max_processing_rate: u64,
    ) -> Self {
        let rate_scale =
            if max_processing_rate > 0 { max_processing_rate } else { level.processing_rate };
        let processing_rate = Self::to_processing_rate(level.processing_rate, rate_scale);
        #[expect(clippy::manual_checked_ops)]
        let power_cost_nw_per_rate = if level.processing_rate > 0 {
            (level.power_coefficient_nw * rate_scale) / level.processing_rate
        } else {
            0
        };
        let mut name = [0u8; ZX_MAX_NAME_LEN];
        let mut name_len = 0;
        if !level.diagnostic_name.is_null() {
            while name_len < name.len() - 1 {
                // SAFETY: The caller guarantees that diagnostic_name points to a valid
                // NUL-terminated C string or at least ZX_MAX_NAME_LEN - 1 readable bytes.
                let b = unsafe { *level.diagnostic_name.add(name_len).cast::<u8>() };
                if b == 0 {
                    break;
                }
                name[name_len] = b;
                name_len += 1;
            }
        }
        Self {
            options: level.options,
            control: level.control_interface,
            control_argument: level.control_argument,
            processing_rate,
            power_coefficient_nw: level.power_coefficient_nw,
            power_cost_nw_per_rate,
            name,
            name_len,
            level: level_index,
        }
    }

    /// Power level type. Idle and Active power levels are orthogonal, that is, an entity may be
    /// idle while keeping its active power level unchanged. This means that the actual power level
    /// of an entity should be determined by the tuple `<Idle Power Level*, Active Power Level>`,
    /// where if `Idle Power Level` is absent then that means that the entity is active and the
    /// active power level should be used.
    ///
    /// This situation happens for example, when a CPU transitions from an active power level A
    /// (which may be interpreted as a known OPP or P-State) into an idle state, such as suspension,
    /// idle thread or even powering it off.
    #[inline]
    pub const fn r#type(&self) -> PowerLevelType {
        if self.processing_rate.raw_value() == 0 {
            PowerLevelType::Idle
        } else {
            PowerLevelType::Active
        }
    }

    /// Processing rate when this power level is active. This is key to determining the available
    /// bandwidth of the entity.
    #[inline]
    pub const fn processing_rate(&self) -> ProcessingRate {
        self.processing_rate
    }

    /// Relative to the system power consumption, determines how much power is being consumed at
    /// this level. This allows determining if this power level should be a candidate when
    /// operating under a given energy budget.
    #[inline]
    pub const fn power_coefficient_nw(&self) -> u64 {
        self.power_coefficient_nw
    }

    /// Power cost of this power level, normalized by the rate of this power level.
    #[inline]
    pub const fn power_cost_nw_per_rate(&self) -> u64 {
        self.power_cost_nw_per_rate
    }

    /// ID of the interface handling transitions TO this power level.
    #[inline]
    pub const fn control(&self) -> ControlInterface {
        self.control
    }

    /// Argument to be interpreted by the control interface in order to transition to this level.
    ///
    /// The control interface is only aware of this argument, and power levels are identified by
    /// this argument.
    #[inline]
    pub const fn control_argument(&self) -> u64 {
        self.control_argument
    }

    /// This level may be transitioned on a per-CPU basis, without affecting other entities in the
    /// same power domain.
    #[inline]
    pub const fn targets_cpus(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) != 0
    }

    /// This level may be transitioned on a per-power-domain basis, that is, all other entities in
    /// the power domain will be transitioned together.
    ///
    /// This means that underlying hardware elements are shared and it is not possible to
    /// transition a single member of the power domain.
    #[inline]
    pub const fn targets_power_domain(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) == 0
    }

    /// Name used to identify this power level, for diagnostic purposes.
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Power level index as understood from the original model perspective.
    #[inline]
    pub const fn level(&self) -> u8 {
        self.level
    }
}

impl Default for PowerLevel {
    #[inline]
    fn default() -> Self {
        Self {
            options: 0,
            control: ControlInterface::CpuDriver,
            control_argument: 0,
            processing_rate: ProcessingRate::from_raw(0),
            power_coefficient_nw: 0,
            power_cost_nw_per_rate: 0,
            name: [0; ZX_MAX_NAME_LEN],
            name_len: 0,
            level: 0,
        }
    }
}

/// Represents an entry in a transition matrix, where the position in the matrix denotes
/// the source and target power level. This construct just denotes the properties of that
/// cell.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerLevelTransition {
    /// Time required for the transition to take effect. In some cases it may mean for the actual
    /// voltage to stabilize.
    latency: zx_duration_mono_t,
    /// Amount of energy consumed to perform the transition.
    energy_cost_nj: u64,
}

zr::static_assert!(size_of::<PowerLevelTransition>() == 16);
zr::static_assert!(align_of::<PowerLevelTransition>() == 8);
zr::static_assert!(offset_of!(PowerLevelTransition, latency) == 0);
zr::static_assert!(offset_of!(PowerLevelTransition, energy_cost_nj) == 8);

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

    /// Latency for transitioning from a given level to another.
    #[inline]
    pub const fn latency(&self) -> zx_duration_mono_t {
        self.latency
    }

    /// Energy cost in nanojoules (nj) for transitioning from a given level to another.
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

/// Represents a view of the `zx_processor_power_level_transition_t` array as a square matrix.
/// As per a view's concept, this view is only valid so long as the original object remains valid
/// and is tied to its lifecycle.
///
/// Additionally transition matrices are required to be square matrices, since they describe
/// transitions from every existing level to every other level.
#[derive(Copy, Clone, Debug)]
pub struct TransitionMatrix<'a> {
    /// Slice of transitions flattened in row-major order.
    transitions: &'a [PowerLevelTransition],
    /// Number of rows (and columns) in the square matrix.
    num_rows: usize,
}

impl<'a> TransitionMatrix<'a> {
    /// Creates a `TransitionMatrix` over `entries` with the given `row_len`
    /// (`num_rows == row_len`).
    #[inline]
    fn new(transitions: &'a [PowerLevelTransition], num_rows: usize) -> Self {
        debug_assert!(!transitions.is_empty());
        debug_assert!(num_rows != 0);
        debug_assert!(transitions.len().is_multiple_of(num_rows));
        debug_assert!(transitions.len() / num_rows == num_rows);
        Self { transitions, num_rows }
    }

    /// Returns row `index` without an `Option` wrapper (`index < self.num_rows`).
    #[inline]
    const fn row(&self, index: usize) -> &'a [PowerLevelTransition] {
        let start = index * self.num_rows;
        let (_, rest) = self.transitions.split_at(start);
        let (row, _) = rest.split_at(self.num_rows);
        row
    }

    /// Returns the transition row for level `index`, or `None` if `index >= self.num_rows`.
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

/// `EnergyModel` describes the power consumption rates of the available active and
/// idle states a processor may enter, which interfaces to use to effect state
/// transitions, and properties of the state transitions, such as energy cost and
/// latency. This information is used to make efficient scheduling and load
/// balancing decisions that meet the power vs. performance tradeoffs specified
/// by a product.
///
/// An energy model is constant once initialized. Updating the active energy
/// model requires replacing it with a new instance and incrementing a generation
/// count to detect stale values derived from the previous energy model.
#[repr(C)]
pub struct EnergyModel {
    /// Sorted power levels in the model.
    power_levels: Vector<PowerLevel>,
    /// Flattened square matrix of transition costs between power levels.
    transitions: Vector<PowerLevelTransition>,
    /// Lookup indices sorted by `(control, control_argument)`.
    control_lookup: Vector<usize>,
    /// Count of idle power levels at the front of `power_levels`.
    idle_power_levels: usize,
    /// Scale used to normalize raw integer processing rates.
    max_processing_rate_scale: u64,
}

zr::static_assert!(size_of::<EnergyModel>() == 88);
zr::static_assert!(align_of::<EnergyModel>() == 8);

impl EnergyModel {
    /// Creates an empty default `EnergyModel`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            power_levels: Vector::new(),
            transitions: Vector::new(),
            control_lookup: Vector::new(),
            idle_power_levels: 0,
            max_processing_rate_scale: 1,
        }
    }

    /// Creates an `EnergyModel` from `levels`, `transitions`, and `max_processing_rate`.
    ///
    /// # Safety
    ///
    /// For every element in `levels`, if `diagnostic_name` is non-null, it must point to a valid
    /// NUL-terminated C string or at least `ZX_MAX_NAME_LEN - 1` readable bytes.
    pub unsafe fn create(
        levels: &[ProcessorPowerLevel],
        transitions: &[ProcessorPowerLevelTransition],
        mut max_processing_rate: u64,
    ) -> Result<Self, Status> {
        // Allocations below would be UB.
        if levels.is_empty() {
            return Err(Status::INVALID_ARGS);
        }

        // PowerLevel stores its original index as a uint8_t.
        if levels.len() > (u8::MAX as usize) + 1 {
            return Err(Status::INVALID_ARGS);
        }

        if transitions.len() > levels.len() * levels.len() {
            return Err(Status::INVALID_ARGS);
        }

        // Validate that transitions are to and from valid levels.
        for transition in transitions {
            if (transition.from as usize) >= levels.len() {
                return Err(Status::INVALID_ARGS);
            }

            if (transition.to as usize) >= levels.len() {
                return Err(Status::INVALID_ARGS);
            }
        }

        if max_processing_rate == 0 {
            for level in levels {
                max_processing_rate = max_processing_rate.max(level.processing_rate);
            }
            if max_processing_rate == 0 {
                max_processing_rate = 1;
            }
        }

        let mut power_levels = Vector::new();
        power_levels.reserve(levels.len()).map_err(|_| Status::NO_MEMORY)?;

        let mut power_levels_lookup = Vector::new();
        power_levels_lookup.reserve(levels.len()).map_err(|_| Status::NO_MEMORY)?;

        // If we assume symmetry in the matrix, we could reduce the amount of space required to
        // support transitions by more than half, since any element below the main diagonal would
        // be equivalent to its mirror, and the diagonal itself would be 0.
        let mut power_level_transitions = Vector::new();
        let default_value = if transitions.is_empty() {
            PowerLevelTransition::zero()
        } else {
            PowerLevelTransition::invalid()
        };

        power_level_transitions
            .resize(levels.len() * levels.len(), default_value)
            .map_err(|_| Status::NO_MEMORY)?;

        let mut idle_levels = 0usize;
        // We assert below, because all the space required for these operation has been
        // preallocated.
        for (i, level_desc) in levels.iter().enumerate() {
            // SAFETY: Caller guarantees `level_desc.diagnostic_name` is either null, a valid
            // NUL-terminated C string, or at least `ZX_MAX_NAME_LEN - 1` readable bytes.
            let res = power_levels
                .push_back(unsafe { PowerLevel::new(i as u8, level_desc, max_processing_rate) });
            // These were preallocated above.
            assert!(res.is_ok());
            let res = power_levels_lookup.push_back(i);
            // These were preallocated above.
            assert!(res.is_ok());
            if power_levels[i].r#type() == PowerLevelType::Idle {
                idle_levels += 1;
            }
        }

        // This will naturally partition idle and active states, having all idle states at the
        // beginning.
        power_levels.sort_unstable_by(|a, b| {
            if a.processing_rate() == b.processing_rate() {
                a.power_coefficient_nw().cmp(&b.power_coefficient_nw())
            } else {
                a.processing_rate().cmp(&b.processing_rate())
            }
        });

        // Map each original level index to its index in the sorted array. Each PowerLevel retains
        // its original index in level(), so this holds for any sort permutation, including ties.
        for i in 0..power_levels.len() {
            power_levels_lookup[power_levels[i].level() as usize] = i;
        }

        // Fill up the square matrix from level i to level j, where i and j, are indexes into the
        // power_level array.
        for transition in transitions {
            let i = power_levels_lookup[transition.from as usize];
            let j = power_levels_lookup[transition.to as usize];

            // Double check that translation is correct.
            assert!(power_levels[i].level() == transition.from);
            assert!(power_levels[j].level() == transition.to);

            let transition_offset = (i * power_levels.len()) + j;
            power_level_transitions[transition_offset] = PowerLevelTransition::new(transition);
        }

        // Reset the power level lookup, and turn it into a lookup by control interface, such that
        // the set path doesn't have to traverse every level.
        for (i, slot) in power_levels_lookup.iter_mut().enumerate() {
            *slot = i;
        }

        // Generate lookup table based on control interface and control argument tuple.
        power_levels_lookup.sort_unstable_by(|&a, &b| {
            if (power_levels[a].control() as u64) == (power_levels[b].control() as u64) {
                power_levels[a].control_argument().cmp(&power_levels[b].control_argument())
            } else {
                (power_levels[a].control() as u64).cmp(&(power_levels[b].control() as u64))
            }
        });

        Ok(Self {
            power_levels,
            transitions: power_level_transitions,
            control_lookup: power_levels_lookup,
            idle_power_levels: idle_levels,
            max_processing_rate_scale: max_processing_rate,
        })
    }

    /// Scale used to normalize raw integer rates into fixed-point `ProcessingRate`.
    #[inline]
    pub const fn max_processing_rate_scale(&self) -> u64 {
        self.max_processing_rate_scale
    }

    /// Normalizes `processing_rate` using `max_processing_rate_scale`.
    #[inline]
    pub fn to_processing_rate(&self, processing_rate: u64) -> ProcessingRate {
        PowerLevel::to_processing_rate(processing_rate, self.max_processing_rate_scale)
    }

    /// Converts `processing_rate` back to a raw integer rate using `max_processing_rate_scale`.
    #[inline]
    pub fn from_processing_rate(&self, processing_rate: ProcessingRate) -> u64 {
        PowerLevel::from_processing_rate(processing_rate, self.max_processing_rate_scale)
    }

    /// Returns the total number of power levels in the model.
    #[inline]
    pub fn levels_len(&self) -> usize {
        self.power_levels.len()
    }

    /// Returns the number of idle power levels in the model.
    #[inline]
    pub const fn idle_levels_len(&self) -> usize {
        self.idle_power_levels
    }

    /// All power levels described in the model, sorted by processing power and energy consumption.
    ///
    /// (1) The processing rate of power level `i` is less than or equal to the processing rate of
    /// power level `j`, where `i <= j`.
    ///
    /// (2) The processing rate of power level `i` is equal to the processing rate of power level
    /// `j` and the energy cost of power level `i` is less than or equal to the energy cost of
    /// power level `j`, where `i <= j`.
    #[inline]
    pub fn levels(&self) -> &[PowerLevel] {
        &self.power_levels
    }

    /// Following the same rules as [`Self::levels`] but returns only the set of power levels whose
    /// type is [`PowerLevelType::Idle`]. This set may be empty.
    #[inline]
    pub fn idle_levels(&self) -> &[PowerLevel] {
        &self.power_levels[..self.idle_power_levels]
    }

    /// Returns the idle power level with the maximum power consumption.
    #[inline]
    pub const fn max_idle_power_level(&self) -> Option<u8> {
        if self.idle_power_levels > 0 { Some((self.idle_power_levels - 1) as u8) } else { None }
    }

    /// Returns the power coefficient of the idle power level with the maximum power consumption.
    /// This idle power level typically corresponds to clock gating, such that the power
    /// consumption is almost entirely leakage power loss.
    #[inline]
    pub fn max_idle_power_coefficient_nw(&self) -> Option<u64> {
        if self.idle_power_levels > 0 {
            Some(self.power_levels[self.idle_power_levels - 1].power_coefficient_nw())
        } else {
            None
        }
    }

    /// Returns the control interface of the idle power level with the maximum power consumption.
    #[inline]
    pub fn max_idle_power_level_interface(&self) -> Option<ControlInterface> {
        if self.idle_power_levels > 0 {
            Some(self.power_levels[self.idle_power_levels - 1].control())
        } else {
            None
        }
    }

    /// Returns the processing rate of the fastest power level.
    #[inline]
    pub fn max_processing_rate(&self) -> ProcessingRate {
        self.levels().last().map_or(ProcessingRate::from_raw(0), PowerLevel::processing_rate)
    }

    /// Following the same rules as [`Self::levels`] but returns only the set of power levels whose
    /// type is [`PowerLevelType::Active`]. This set may be empty.
    #[inline]
    pub fn active_levels(&self) -> &[PowerLevel] {
        &self.power_levels[self.idle_power_levels..]
    }

    /// Returns the flat transition slice backing [`Self::transitions`].
    #[inline]
    pub(super) fn transitions_slice(&self) -> &[PowerLevelTransition] {
        &self.transitions
    }

    /// Returns a transition matrix, where the entry `<i, j>` represents the transition costs for
    /// transitioning from `i` to `j`.
    #[inline]
    pub fn transitions(&self) -> TransitionMatrix<'_> {
        TransitionMatrix::new(&self.transitions, self.power_levels.len())
    }

    /// Looks up the power level index matching `(interface_id, control_argument)`.
    pub fn find_power_level(
        &self,
        interface_id: ControlInterface,
        control_argument: u64,
    ) -> Option<u8> {
        let idx = self.control_lookup.partition_point(|&i| {
            let a = &self.power_levels[i];
            let a_ctrl = a.control() as u64;
            let b_ctrl = interface_id as u64;
            a_ctrl < b_ctrl || (a_ctrl == b_ctrl && a.control_argument() < control_argument)
        });
        if let Some(&level_idx) = self.control_lookup.get(idx) {
            let level = &self.power_levels[level_idx];
            if level.control() == interface_id && level.control_argument() == control_argument {
                return Some(level_idx as u8);
            }
        }
        None
    }

    /// Returns the lowest active power level that is compatible with (i.e. has a
    /// processing rate greater or equal to) the given processing rate. However, to
    /// simplify power calculations, the maximum active power level is returned if
    /// the given rate exceeds the maximum processing rate for the power domain.
    pub fn find_active_power_level_for_rate(
        &self,
        mut processing_rate: ProcessingRate,
    ) -> Option<&PowerLevel> {
        // Return the maximum power level if the processing rate exceeds the maximum
        // rate.
        processing_rate = processing_rate.min(self.max_processing_rate());

        let levels = self.active_levels();
        let idx =
            levels.partition_point(|power_level| power_level.processing_rate() < processing_rate);
        if idx != levels.len() { Some(&levels[idx]) } else { None }
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
    #[cfg(ktest)]
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

    /// Creates a PowerDomainSet from a span of FFI domain configurations.
    /// Validates domain configs, determines system-wide peak rate normalization,
    /// creates an EnergyModel for each domain, binds to the corresponding
    /// PDevPowerLevelController, and constructs the PowerDomain instances.
    ///
    /// # Safety
    ///
    /// For every element `config` in `domain_configs`, if `config.level_count > 0` and
    /// `config.levels` is non-null, `config.levels` must point to a valid array of
    /// `config.level_count` `ProcessorPowerLevelFfi` structs, and each non-null
    /// `diagnostic_name` must point to a valid NUL-terminated C string.
    pub unsafe fn create_from_configs(
        domain_configs: &[PowerDomainConfigFfi],
    ) -> Result<Self, Status> {
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
                if config.levels.is_null() && config.level_count > 0 {
                    return Err(Status::INVALID_ARGS);
                }
                if config.cpu_mask == 0 {
                    return Err(Status::INVALID_ARGS);
                }
                for j in 0..config.level_count {
                    // SAFETY: `config.levels` was checked non-null for `config.level_count > 0`,
                    // and caller provides a valid array of `config.level_count` elements.
                    let ffi_level = unsafe { &*config.levels.add(j) };
                    max_system_processing_rate =
                        max_system_processing_rate.max(ffi_level.processing_rate);
                }
            }
            if max_system_processing_rate == 0 {
                max_system_processing_rate = 1;
            }

            let mut domain_array: ArrayType = [const { None }; MAX_POWER_DOMAINS];

            for (i, config) in domain_configs.iter().enumerate() {
                let Ok(mut levels) = Array::<ProcessorPowerLevel>::try_new(config.level_count)
                else {
                    dprintf!(
                        CRITICAL,
                        "POWER: Failed to allocate power levels array for domain {}\n",
                        config.domain_id
                    );
                    return Err(Status::NO_MEMORY);
                };

                for j in 0..config.level_count {
                    // SAFETY: `config.levels` is valid for `config.level_count` elements.
                    let ffi_level = unsafe { &*config.levels.add(j) };
                    levels[j] = ProcessorPowerLevel {
                        options: ffi_level.options as u64,
                        processing_rate: ffi_level.processing_rate,
                        power_coefficient_nw: ffi_level.power_coefficient_nw,
                        control_interface: if ffi_level.control_interface == 0 {
                            ControlInterface::ArmWfi
                        } else {
                            ControlInterface::CpuDriver
                        },
                        control_argument: ffi_level.control_argument,
                        diagnostic_name: if ffi_level.diagnostic_name.is_null() {
                            c"".as_ptr()
                        } else {
                            ffi_level.diagnostic_name
                        },
                    };
                }

                // SAFETY: Caller guarantees each `diagnostic_name` in `levels` is a valid
                // NUL-terminated C string.
                let energy_model_res =
                    unsafe { EnergyModel::create(&levels, &[], max_system_processing_rate) };
                let mut energy_model = match energy_model_res {
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

    /// Looks up the active power coefficient for the given CPU operating at the given processing
    /// rate. Returns 0 if there is no power domain for the given CPU or if there is no active
    /// power level that lower bounds the given processing rate.
    #[inline]
    pub fn lookup_active_power_coefficient(
        &self,
        cpu_num: u32,
        processing_rate: ProcessingRate,
    ) -> u64 {
        #[expect(clippy::collapsible_if)]
        if let Some(power_domain) = self.find_by_cpu_num(cpu_num) {
            if let Some(power_level) =
                power_domain.model().find_active_power_level_for_rate(processing_rate)
            {
                return power_level.power_coefficient_nw();
            }
        }
        0
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

    /// Looks up the active power level for the given CPU operating at the given processing rate.
    #[inline]
    pub fn lookup_power_level(&self, cpu_num: u32, processing_rate: ProcessingRate) -> Option<u8> {
        #[expect(clippy::collapsible_if)]
        if let Some(power_domain) = self.find_by_cpu_num(cpu_num) {
            if let Some(power_level) =
                power_domain.model().find_active_power_level_for_rate(processing_rate)
            {
                return Some(power_level.level());
            }
        }
        None
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

    /// Returns a const reference to the underlying power domain array.
    #[inline]
    pub const fn domains(&self) -> &ArrayType {
        &self.domains
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
///
/// # Safety
///
/// For every element `config` in `domains`, if `config.level_count > 0` and `config.levels` is
/// non-null, `config.levels` must point to a valid array of `config.level_count`
/// `ProcessorPowerLevelFfi` structs, and each non-null `diagnostic_name` must point to a valid
/// NUL-terminated C string or at least `ZX_MAX_NAME_LEN - 1` readable bytes.
pub unsafe fn register_domains(domains: &[PowerDomainConfigFfi]) -> Result<(), Status> {
    #[cfg(not(feature = "userspace"))]
    {
        // SAFETY: Caller upholds the pointer validity requirements on `domains`.
        let domain_set_res = unsafe { PowerDomainSet::create_from_configs(domains) };
        let mut domain_set = match domain_set_res {
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
#[cfg(ktest)]
#[unittest::suite(name = "energy_model_rust")]
mod tests {
    use fbl::{RefPtr, Vector};
    use unittest::{
        assert_eq, assert_true, expect_eq, expect_err, expect_false, expect_le, expect_true,
    };

    use super::{
        ControlInterface, EnergyModel, POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PerCpu, PowerDomain,
        PowerDomainSet, PowerLevel, PowerLevelTransition, PowerLevelType, ProcessingRate,
        ProcessorPowerLevel, ProcessorPowerLevelTransition, SMP_MAX_CPUS, Status, set_rate_limits,
    };
    use crate::power_management::energy_model_ffi::rust_energy_model_find_power_level;

    /// Creates a test `PowerDomain` with a default 10-level energy model and the given CPU mask.
    fn make_domain(id: u32, cpus: &[u32]) -> RefPtr<PowerDomain> {
        const MAX_IDLE_LEVEL: usize = 2;
        let test_power_levels: [ProcessorPowerLevel; 10] =
            core::array::from_fn(|i| ProcessorPowerLevel {
                options: if i <= MAX_IDLE_LEVEL {
                    POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT
                } else {
                    0
                },
                processing_rate: if i <= MAX_IDLE_LEVEL { 0 } else { (i as u64) * 10 },
                power_coefficient_nw: (i as u64) + 1,
                control_interface: if i < MAX_IDLE_LEVEL {
                    ControlInterface::ArmPsci
                } else if i == MAX_IDLE_LEVEL {
                    ControlInterface::ArmWfi
                } else {
                    ControlInterface::CpuDriver
                },
                control_argument: i as u64,
                diagnostic_name: c"fake".as_ptr(),
            });

        // SAFETY: All `diagnostic_name` pointers in `test_power_levels` are C string literals.
        let mut model = unsafe { EnergyModel::create(&test_power_levels, &[], 0) }
            .expect("failed to create test EnergyModel");

        let mut mask = 0u64;
        for &cpu in cpus {
            mask |= 1u64 << cpu;
        }
        PowerDomain::create_for_test(id, mask, &mut model).expect("Failed to create PowerDomain")
    }

    macro_rules! check_level {
        ($actual:expr, $expected:expr) => {{
            let actual: &PowerLevel = $actual;
            let expected: &PowerLevel = $expected;
            expect_eq!(actual.level(), expected.level());
            expect_eq!(actual.control(), expected.control());
            expect_eq!(actual.control_argument(), expected.control_argument());
            expect_true!(actual.name() == expected.name());
            expect_eq!(actual.power_coefficient_nw(), expected.power_coefficient_nw());
            expect_eq!(
                actual.processing_rate().raw_value(),
                expected.processing_rate().raw_value()
            );
            expect_eq!(actual.r#type(), expected.r#type());
            expect_eq!(actual.targets_cpus(), expected.targets_cpus());
            expect_eq!(actual.targets_power_domain(), expected.targets_power_domain());
        }};
    }

    /// Verifies `PowerLevel::to_processing_rate` and `PowerLevel::from_processing_rate`.
    #[test]
    fn to_from_rate() {
        let min_rate = ProcessingRate::from_raw(0);
        let max_rate = ProcessingRate::from(ffl::Fixed::<i64, 31>::from_integer(1));
        let min_raw_rate = 0u64;
        let max_raw_rate = 1024u64;

        expect_eq!(
            min_rate.raw_value(),
            PowerLevel::to_processing_rate(min_raw_rate, max_raw_rate).raw_value()
        );
        expect_eq!(
            max_rate.raw_value(),
            PowerLevel::to_processing_rate(max_raw_rate, max_raw_rate).raw_value()
        );
        expect_eq!(min_raw_rate, PowerLevel::from_processing_rate(min_rate, max_raw_rate));
        expect_eq!(max_raw_rate, PowerLevel::from_processing_rate(max_rate, max_raw_rate));
    }

    /// Verifies `PowerLevel::new` with domain-dependent options.
    #[test]
    fn power_level_ctor() {
        let default_desc = ProcessorPowerLevel::default();
        expect_eq!(default_desc.options, 0);
        expect_eq!(default_desc.processing_rate, 0);
        expect_eq!(default_desc.power_coefficient_nw, 0);
        expect_eq!(default_desc.control_interface, ControlInterface::CpuDriver);
        expect_eq!(default_desc.control_argument, 0);
        expect_true!(default_desc.diagnostic_name.is_null());

        let default_level = PowerLevel::default();
        expect_eq!(default_level.level(), 0);
        expect_eq!(default_level.processing_rate().raw_value(), 0);
        expect_eq!(default_level.power_coefficient_nw(), 0);
        expect_eq!(default_level.power_cost_nw_per_rate(), 0);
        expect_eq!(default_level.control(), ControlInterface::CpuDriver);
        expect_eq!(default_level.control_argument(), 0);
        expect_eq!(default_level.r#type(), PowerLevelType::Idle);
        expect_true!(default_level.name().is_empty());
        expect_true!(default_level.targets_power_domain());
        expect_false!(default_level.targets_cpus());

        let desc = ProcessorPowerLevel {
            options: 0,
            processing_rate: 456,
            power_coefficient_nw: 789,
            control_interface: ControlInterface::ArmPsci,
            control_argument: 12345,
            diagnostic_name: c"foobar one two three".as_ptr(),
        };

        // SAFETY: `desc.diagnostic_name` is a valid NUL-terminated C string literal.
        let level = unsafe { PowerLevel::new(0, &desc, PowerLevel::USER_PROCESSING_RATE_SCALE) };

        expect_eq!(level.level(), 0);
        expect_eq!(
            level.processing_rate().raw_value(),
            PowerLevel::to_processing_rate(
                desc.processing_rate,
                PowerLevel::USER_PROCESSING_RATE_SCALE
            )
            .raw_value()
        );
        expect_eq!(level.power_coefficient_nw(), desc.power_coefficient_nw);
        expect_eq!(level.control(), desc.control_interface);
        expect_eq!(level.control_argument(), desc.control_argument);
        expect_eq!(level.r#type(), PowerLevelType::Active);
        expect_true!(level.name() == b"foobar one two three");
        expect_true!(level.targets_power_domain());
        expect_false!(level.targets_cpus());
    }

    /// Verifies `PowerLevel::new` with `POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT`.
    #[test]
    fn power_level_ctor2() {
        let desc = ProcessorPowerLevel {
            options: POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
            processing_rate: 123,
            power_coefficient_nw: 789,
            control_interface: ControlInterface::ArmPsci,
            control_argument: 12345,
            diagnostic_name: c"foobar one two three".as_ptr(),
        };

        // SAFETY: `desc.diagnostic_name` is a valid NUL-terminated C string literal.
        let level = unsafe { PowerLevel::new(123, &desc, PowerLevel::USER_PROCESSING_RATE_SCALE) };

        expect_eq!(level.level(), 123);
        expect_eq!(
            level.processing_rate().raw_value(),
            PowerLevel::to_processing_rate(
                desc.processing_rate,
                PowerLevel::USER_PROCESSING_RATE_SCALE
            )
            .raw_value()
        );
        expect_eq!(level.power_coefficient_nw(), desc.power_coefficient_nw);
        expect_eq!(level.control(), desc.control_interface);
        expect_eq!(level.control_argument(), desc.control_argument);
        expect_eq!(level.r#type(), PowerLevelType::Active);
        expect_true!(level.name() == b"foobar one two three");
        expect_false!(level.targets_power_domain());
        expect_true!(level.targets_cpus());
    }

    /// Verifies `PowerLevelTransition::new` and `PowerLevelTransition::default`.
    #[test]
    fn power_level_transition_ctor() {
        let default_desc = ProcessorPowerLevelTransition::default();
        expect_eq!(default_desc.latency, 0);
        expect_eq!(default_desc.energy_nj, 0);
        expect_eq!(default_desc.from, 0);
        expect_eq!(default_desc.to, 0);

        let default_transition = PowerLevelTransition::default();
        expect_false!(default_transition.is_valid());
        expect_true!(default_transition == PowerLevelTransition::invalid());
        expect_true!(PowerLevelTransition::zero().is_valid());

        let desc = ProcessorPowerLevelTransition { latency: 456, energy_nj: 1234, from: 0, to: 1 };

        let transition = PowerLevelTransition::new(&desc);
        expect_eq!(transition.latency(), desc.latency);
        expect_eq!(transition.energy_cost_nj(), desc.energy_nj);
        expect_true!(transition.is_valid());
    }

    /// Verifies `EnergyModel::create` with levels and transitions.
    #[test]
    fn energy_model_create() {
        let empty_model = EnergyModel::default();
        expect_eq!(empty_model.levels_len(), 0);
        expect_eq!(empty_model.idle_levels_len(), 0);
        expect_true!(empty_model.levels().is_empty());
        expect_true!(empty_model.idle_levels().is_empty());
        expect_true!(empty_model.active_levels().is_empty());
        expect_true!(empty_model.max_idle_power_level().is_none());
        expect_true!(empty_model.max_idle_power_coefficient_nw().is_none());
        expect_true!(empty_model.max_idle_power_level_interface().is_none());
        expect_eq!(empty_model.max_processing_rate().raw_value(), 0);
        expect_true!(
            empty_model.find_active_power_level_for_rate(ProcessingRate::from_raw(0)).is_none()
        );

        let power_levels = [
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 1,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 1,
                diagnostic_name: c"0".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 4,
                power_coefficient_nw: 8,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 3,
                diagnostic_name: c"1".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 2,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: c"2".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 4,
                power_coefficient_nw: 10,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: c"3".as_ptr(),
            },
        ];

        let transitions_desc = [
            ProcessorPowerLevelTransition { latency: 1, energy_nj: 2, from: 1, to: 0 },
            ProcessorPowerLevelTransition { latency: 2, energy_nj: 3, from: 2, to: 0 },
            ProcessorPowerLevelTransition { latency: 3, energy_nj: 4, from: 3, to: 0 },
            ProcessorPowerLevelTransition { latency: 1, energy_nj: 2, from: 0, to: 1 },
            ProcessorPowerLevelTransition { latency: 3, energy_nj: 4, from: 2, to: 1 },
            ProcessorPowerLevelTransition { latency: 4, energy_nj: 5, from: 3, to: 1 },
            ProcessorPowerLevelTransition { latency: 2, energy_nj: 3, from: 0, to: 2 },
            ProcessorPowerLevelTransition { latency: 3, energy_nj: 4, from: 1, to: 2 },
            ProcessorPowerLevelTransition { latency: 5, energy_nj: 6, from: 3, to: 2 },
            ProcessorPowerLevelTransition { latency: 3, energy_nj: 4, from: 0, to: 3 },
            ProcessorPowerLevelTransition { latency: 4, energy_nj: 5, from: 1, to: 3 },
            ProcessorPowerLevelTransition { latency: 5, energy_nj: 6, from: 2, to: 3 },
        ];

        // SAFETY: All `diagnostic_name` pointers in `power_levels` are C string literals.
        let energy_model = unsafe { EnergyModel::create(&power_levels, &transitions_desc, 0) };
        assert_true!(energy_model.is_ok());
        let energy_model = energy_model.unwrap();

        // Proper transformation of the model and the transition table.
        assert_eq!(energy_model.levels().len(), 4);
        expect_eq!(energy_model.levels_len(), 4);
        expect_eq!(energy_model.idle_levels_len(), 2);
        let levels = energy_model.levels();
        for i in 0..levels.len() - 1 {
            let j = i + 1;
            expect_le!(
                levels[i].processing_rate().raw_value(),
                levels[j].processing_rate().raw_value()
            );
            if levels[i].processing_rate() == levels[j].processing_rate() {
                expect_le!(levels[i].power_coefficient_nw(), levels[j].power_coefficient_nw());
            }
            let lvl_i = levels[i].level();
            let lvl_j = levels[j].level();
            // SAFETY: `diagnostic_name` pointers in `power_levels` are C string literals.
            let expected_i = unsafe { PowerLevel::new(lvl_i, &power_levels[lvl_i as usize], 0) };
            // SAFETY: `diagnostic_name` pointers in `power_levels` are C string literals.
            let expected_j = unsafe { PowerLevel::new(lvl_j, &power_levels[lvl_j as usize], 0) };
            check_level!(&levels[i], &expected_i);
            check_level!(&levels[j], &expected_j);
        }

        let transitions = energy_model.transitions();
        expect_true!(transitions.get(levels.len()).is_none());
        for i in 0..levels.len() {
            let row = transitions.get(i);
            assert_true!(row.is_some());
            for j in 0..levels.len() {
                let transition = transitions[i][j];
                expect_true!(transition == row.unwrap()[j]);
                let og_i = levels[i].level();
                let og_j = levels[j].level();
                let og_transition = transitions_desc
                    .iter()
                    .find(|t| t.from == og_i && t.to == og_j)
                    .map_or_else(PowerLevelTransition::invalid, PowerLevelTransition::new);
                expect_eq!(transition.latency(), og_transition.latency());
                expect_eq!(transition.energy_cost_nj(), og_transition.energy_cost_nj());
                expect_eq!(transition.is_valid(), i != j);
            }
        }

        // Properly partitioned.
        assert_eq!(energy_model.idle_levels().len(), 2);
        expect_eq!(energy_model.idle_levels()[0].level(), 0);
        expect_eq!(energy_model.idle_levels()[1].level(), 2);
        expect_true!(energy_model.max_idle_power_level() == Some(1));
        expect_true!(energy_model.max_idle_power_coefficient_nw() == Some(2));
        expect_true!(
            energy_model.max_idle_power_level_interface() == Some(ControlInterface::ArmWfi)
        );

        assert_eq!(energy_model.active_levels().len(), 2);
        expect_eq!(energy_model.active_levels()[0].level(), 1);
        expect_eq!(energy_model.active_levels()[1].level(), 3);
        expect_eq!(
            energy_model.max_processing_rate().raw_value(),
            energy_model.to_processing_rate(4).raw_value()
        );

        let active_for_zero =
            energy_model.find_active_power_level_for_rate(ProcessingRate::from_raw(0));
        assert_true!(active_for_zero.is_some());
        expect_eq!(active_for_zero.unwrap().level(), 1);

        let active_for_max =
            energy_model.find_active_power_level_for_rate(energy_model.to_processing_rate(4));
        assert_true!(active_for_max.is_some());
        expect_eq!(active_for_max.unwrap().level(), 1);

        // Sorted by tuple <Control Interface, Control Argument>
        for (i, level) in levels.iter().enumerate() {
            expect_true!(
                energy_model.find_power_level(level.control(), level.control_argument())
                    == Some(i as u8)
            );
        }
        expect_true!(energy_model.find_power_level(ControlInterface::ArmPsci, 495).is_none());
        let mut out_level = 0u8;
        expect_false!(rust_energy_model_find_power_level(&energy_model, 495, 0, &mut out_level));
    }

    /// Verifies `EnergyModel::create` remaps transitions for any sort order.
    #[test]
    fn power_model_create_remaps_transitions_for_any_sort_order() {
        // Sorting reorders these levels as WFI (originally 1), tied idle states C2a/C2b (3 and 4),
        // low (2), high (0). The original-to-sorted index mapping contains a cycle longer than 2,
        // which is not its own inverse, as well as two levels with identical (processing_rate,
        // power) keys.
        let power_levels = [
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 2,
                power_coefficient_nw: 20,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: c"high".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 1,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: c"wfi".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 1,
                power_coefficient_nw: 10,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: c"low".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 5,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 10,
                diagnostic_name: c"c2a".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 5,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 20,
                diagnostic_name: c"c2b".as_ptr(),
            },
        ];

        let transitions = [
            ProcessorPowerLevelTransition { latency: 10, energy_nj: 1, from: 1, to: 2 },
            ProcessorPowerLevelTransition { latency: 20, energy_nj: 2, from: 1, to: 0 },
            ProcessorPowerLevelTransition { latency: 30, energy_nj: 3, from: 2, to: 0 },
            ProcessorPowerLevelTransition { latency: 40, energy_nj: 4, from: 3, to: 0 },
            ProcessorPowerLevelTransition { latency: 50, energy_nj: 5, from: 4, to: 0 },
        ];

        // SAFETY: All `diagnostic_name` pointers in `power_levels` are C string literals.
        let model_result = unsafe { EnergyModel::create(&power_levels, &transitions, 0) };
        assert_true!(model_result.is_ok());
        let model = model_result.unwrap();

        assert_eq!(model.levels().len(), 5);
        expect_true!(model.levels()[0].name() == b"wfi");
        expect_true!(model.levels()[3].name() == b"low");
        expect_true!(model.levels()[4].name() == b"high");

        let low = model.find_active_power_level_for_rate(model.to_processing_rate(1));
        assert_true!(low.is_some());
        expect_true!(low.unwrap().name() == b"low");

        let high = model.find_active_power_level_for_rate(model.to_processing_rate(2));
        assert_true!(high.is_some());
        expect_true!(high.unwrap().name() == b"high");

        let clamped_high = model.find_active_power_level_for_rate(ProcessingRate::from(
            ffl::Fixed::<i64, 31>::from_integer(2),
        ));
        assert_true!(clamped_high.is_some());
        expect_true!(clamped_high.unwrap().name() == b"high");

        let c2a_idx = model.find_power_level(ControlInterface::ArmPsci, 10);
        let c2b_idx = model.find_power_level(ControlInterface::ArmPsci, 20);
        assert_true!(c2a_idx.is_some());
        assert_true!(c2b_idx.is_some());

        // Sorted indices: wfi = 0, {c2a, c2b} = {1, 2}, low = 3, high = 4.
        expect_eq!(model.transitions()[0][3].latency(), 10);
        expect_eq!(model.transitions()[0][4].latency(), 20);
        expect_eq!(model.transitions()[3][4].latency(), 30);
        expect_eq!(model.transitions()[c2a_idx.unwrap() as usize][4].latency(), 40);
        expect_eq!(model.transitions()[c2b_idx.unwrap() as usize][4].latency(), 50);
        expect_false!(model.transitions()[4][0].is_valid());
    }

    /// Verifies `EnergyModel::create` rejects configurations with more than 256 levels.
    #[test]
    fn power_model_create_rejects_more_than_256_levels() {
        let mut levels = Vector::new();
        assert_true!(levels.reserve(257).is_ok());
        for i in 0..257usize {
            assert_true!(
                levels
                    .push_back(ProcessorPowerLevel {
                        options: 0,
                        processing_rate: (i + 1) as u64,
                        power_coefficient_nw: (i + 1) as u64,
                        control_interface: ControlInterface::CpuDriver,
                        control_argument: i as u64,
                        diagnostic_name: c"opp".as_ptr(),
                    })
                    .is_ok()
            );
        }
        // SAFETY: All `diagnostic_name` pointers in `levels` are C string literals.
        expect_err!(unsafe { EnergyModel::create(&levels, &[], 0) }, Status::INVALID_ARGS);
    }

    /// Verifies `EnergyModel::create` with empty transitions initializes transitions to zero.
    #[test]
    fn energy_model_create_with_empty_transitions_is_ok() {
        let power_levels = [
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 1,
                control_interface: ControlInterface::ArmPsci,
                control_argument: 1,
                diagnostic_name: c"0".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 4,
                power_coefficient_nw: 8,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 3,
                diagnostic_name: c"1".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 2,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: c"2".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 4,
                power_coefficient_nw: 10,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: c"3".as_ptr(),
            },
        ];

        // SAFETY: All `diagnostic_name` pointers in `power_levels` are C string literals.
        let energy_model = unsafe { EnergyModel::create(&power_levels, &[], 0) };
        assert_true!(energy_model.is_ok());
        let energy_model = energy_model.unwrap();

        // Proper transformation of the model and the transition table.
        assert_eq!(energy_model.levels().len(), 4);
        let levels = energy_model.levels();
        for i in 0..levels.len() - 1 {
            let j = i + 1;
            expect_le!(
                levels[i].processing_rate().raw_value(),
                levels[j].processing_rate().raw_value()
            );
            if levels[i].processing_rate() == levels[j].processing_rate() {
                expect_le!(levels[i].power_coefficient_nw(), levels[j].power_coefficient_nw());
            }
            let lvl_i = levels[i].level();
            let lvl_j = levels[j].level();
            // SAFETY: `diagnostic_name` pointers in `power_levels` are C string literals.
            let expected_i = unsafe { PowerLevel::new(lvl_i, &power_levels[lvl_i as usize], 0) };
            // SAFETY: `diagnostic_name` pointers in `power_levels` are C string literals.
            let expected_j = unsafe { PowerLevel::new(lvl_j, &power_levels[lvl_j as usize], 0) };
            check_level!(&levels[i], &expected_i);
            check_level!(&levels[j], &expected_j);
        }

        for row in 0..energy_model.levels().len() {
            for col in 0..energy_model.levels().len() {
                let transition = energy_model.transitions()[row][col];
                expect_eq!(
                    transition.energy_cost_nj(),
                    PowerLevelTransition::zero().energy_cost_nj()
                );
                expect_eq!(transition.latency(), PowerLevelTransition::zero().latency());
            }
        }

        assert_eq!(energy_model.idle_levels().len(), 2);
        expect_eq!(energy_model.idle_levels()[0].level(), 0);
        expect_eq!(energy_model.idle_levels()[1].level(), 2);

        assert_eq!(energy_model.active_levels().len(), 2);
        expect_eq!(energy_model.active_levels()[0].level(), 1);
        expect_eq!(energy_model.active_levels()[1].level(), 3);

        // Sorted by tuple <Control Interface, Control Argument>
        for (i, level) in levels.iter().enumerate() {
            expect_true!(
                energy_model.find_power_level(level.control(), level.control_argument())
                    == Some(i as u8)
            );
        }
        expect_true!(energy_model.find_power_level(ControlInterface::ArmPsci, 495).is_none());
        let mut out_level = 0u8;
        expect_false!(rust_energy_model_find_power_level(&energy_model, 495, 0, &mut out_level));
    }

    /// Verifies `set_rate_limits` rejects CPU masks with bits beyond `processor_count`.
    #[test]
    fn set_rate_limits_rejects_out_of_range_cpu_mask() {
        let processor_count = PerCpu::processor_count().min(SMP_MAX_CPUS);
        if processor_count < 64 {
            expect_err!(set_rate_limits(1u64 << processor_count, 0, 1000), Status::INVALID_ARGS);
            expect_err!(set_rate_limits(u64::MAX, 0, 1000), Status::INVALID_ARGS);
        }
    }

    /// Verifies heterogeneous multi-domain scaling across little and big domains.
    #[test]
    fn heterogeneous_multi_domain_scaling() {
        // Domain 0: Little cores, max raw processing rate = 150.
        let little_levels = [
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 10,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: c"wfi".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 50,
                power_coefficient_nw: 100,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: c"opp0".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 150,
                power_coefficient_nw: 300,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: c"opp1".as_ptr(),
            },
        ];

        // Domain 1: Big cores, max raw processing rate = 1000.
        let big_levels = [
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 0,
                power_coefficient_nw: 50,
                control_interface: ControlInterface::ArmWfi,
                control_argument: 0,
                diagnostic_name: c"wfi".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 500,
                power_coefficient_nw: 1000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: c"opp0".as_ptr(),
            },
            ProcessorPowerLevel {
                options: 0,
                processing_rate: 1000,
                power_coefficient_nw: 2500,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: c"opp1".as_ptr(),
            },
        ];

        const SYSTEM_MAX_PROCESSING_RATE: u64 = 1000;

        // Create Domain 0 EnergyModel normalized to system peak rate (1000).
        // SAFETY: All `diagnostic_name` pointers in `little_levels` are C string literals.
        let little_model =
            unsafe { EnergyModel::create(&little_levels, &[], SYSTEM_MAX_PROCESSING_RATE) };
        assert_true!(little_model.is_ok());
        let little_model = little_model.unwrap();
        expect_eq!(little_model.max_processing_rate_scale(), SYSTEM_MAX_PROCESSING_RATE);

        // Domain 0 levels should be normalized against 1000 rather than local max (150).
        let levels = little_model.levels();
        assert_eq!(levels.len(), 3);
        expect_eq!(levels[0].processing_rate().raw_value(), 0);
        expect_eq!(
            levels[1].processing_rate().raw_value(),
            PowerLevel::to_processing_rate(50, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        expect_eq!(
            levels[2].processing_rate().raw_value(),
            PowerLevel::to_processing_rate(150, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );

        // to_processing_rate / from_processing_rate on little model must use denominator 1000.
        expect_eq!(
            little_model.to_processing_rate(150).raw_value(),
            PowerLevel::to_processing_rate(150, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        expect_eq!(little_model.from_processing_rate(levels[2].processing_rate()), 150);

        // Create Domain 1 EnergyModel normalized to system peak rate (1000).
        // SAFETY: All `diagnostic_name` pointers in `big_levels` are C string literals.
        let big_model =
            unsafe { EnergyModel::create(&big_levels, &[], SYSTEM_MAX_PROCESSING_RATE) };
        assert_true!(big_model.is_ok());
        let big_model = big_model.unwrap();
        expect_eq!(big_model.max_processing_rate_scale(), SYSTEM_MAX_PROCESSING_RATE);

        let big_model_levels = big_model.levels();
        assert_eq!(big_model_levels.len(), 3);
        expect_eq!(big_model_levels[0].processing_rate().raw_value(), 0);
        expect_eq!(
            big_model_levels[1].processing_rate().raw_value(),
            PowerLevel::to_processing_rate(500, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        expect_eq!(
            big_model_levels[2].processing_rate().raw_value(),
            PowerLevel::to_processing_rate(1000, SYSTEM_MAX_PROCESSING_RATE).raw_value()
        );
        expect_eq!(
            big_model_levels[2].processing_rate().raw_value(),
            ProcessingRate::from(ffl::Fixed::<i64, 31>::from_integer(1)).raw_value()
        );
    }

    /// Verifies `PowerDomainSet::find_by_domain_id`.
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
        let domain_set_result = PowerDomainSet::create(&domains);
        assert_true!(domain_set_result.is_ok());
        let domain_set = domain_set_result.unwrap();

        for domain in [&domain_0, &domain_1, &domain_2] {
            let found_domain = domain_set.find_by_domain_id(domain.id());
            expect_true!(
                found_domain.is_some_and(|found_domain| RefPtr::ptr_eq(found_domain, domain))
            );
        }
        expect_true!(domain_set.find_by_domain_id(112345567).is_none());
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
        let domain_set_result = PowerDomainSet::create(&valid_domains);
        assert_true!(domain_set_result.is_ok());
        let domain_set = domain_set_result.unwrap();
        expect_eq!(domain_set.count(), 3);

        let mut visited = 0usize;
        let mut all_in_range = true;
        domain_set.visit(|visited_domain| {
            visited += 1;
            all_in_range &= [&domain_0, &domain_1, &domain_2]
                .iter()
                .any(|domain| RefPtr::ptr_eq(domain, visited_domain));
        });
        expect_eq!(visited, 3);
        expect_true!(all_in_range);

        // Duplicate domain ID.
        let duplicate_id_domains = [Some(make_domain(0, &[0, 1])), Some(make_domain(0, &[2, 3]))];
        expect_err!(PowerDomainSet::create(&duplicate_id_domains), Status::INVALID_ARGS);

        // Overlapping CPU mask.
        let overlapping_domains = [Some(make_domain(0, &[0, 1])), Some(make_domain(1, &[1, 2]))];
        expect_err!(PowerDomainSet::create(&overlapping_domains), Status::INVALID_ARGS);

        // Null domain reference.
        let null_domains: [Option<RefPtr<PowerDomain>>; 1] = [None];
        expect_err!(PowerDomainSet::create(&null_domains), Status::INVALID_ARGS);

        // Exceeding MAX_POWER_DOMAINS (limit 4).
        let too_many_domains = [
            Some(make_domain(0, &[0])),
            Some(make_domain(1, &[1])),
            Some(make_domain(2, &[2])),
            Some(make_domain(3, &[3])),
            Some(make_domain(4, &[4])),
        ];
        expect_err!(PowerDomainSet::create(&too_many_domains), Status::OUT_OF_RANGE);
    }
}
