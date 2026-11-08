// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI declarations and C entry points for `EnergyModel`, `PowerLevel`, `PowerDomain`,
//! `PowerDomainSet`, and `CppPowerLevelController`.

use core::sync::atomic::AtomicI64;

use fbl::RefPtr;
use zx_status::Status;
use zx_types::zx_status_t;

use super::energy_model::{
    CppPowerLevelController, EnergyModel, PowerDomain, PowerDomainSet, PowerLevel,
    PowerLevelTransition, ProcessorPowerLevel, ProcessorPowerLevelTransition,
};
use super::{ControlInterface, ProcessingRate, Utilization};

unsafe extern "C" {
    /// Recycles a `PowerDomain` instance when its last `RefPtr` reference is dropped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid, non-null pointer to a `PowerDomain` whose
    /// reference count has just reached zero, and must only be called from `RefPtr` drop logic.
    pub fn cpp_power_domain_recycle(ptr: *mut PowerDomain);

    /// Returns the ID of `domain` (`PowerDomain::id`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_id(domain: &PowerDomain) -> u32;

    /// Returns the CPU mask of `domain` (`PowerDomain::cpus`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_cpus(domain: &PowerDomain) -> u64;

    /// Returns a reference to the `EnergyModel` of `domain` (`PowerDomain::model`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` for the
    /// lifetime of the returned reference.
    #[expect(improper_ctypes)]
    pub fn cpp_power_domain_model(domain: &PowerDomain) -> &EnergyModel;

    /// Returns a reference to the `CppPowerLevelController` of `domain`
    /// (`PowerDomain::controller`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` with a
    /// non-null `controller_` for the lifetime of the returned reference.
    pub fn cpp_power_domain_controller(domain: &PowerDomain) -> &CppPowerLevelController;

    /// Returns whether scheduler control is enabled for `domain`
    /// (`PowerDomain::scheduler_control_enabled`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_scheduler_control_enabled(domain: &PowerDomain) -> bool;

    /// Sets whether scheduler control is enabled for `domain`
    /// (`PowerDomain::SetSchedulerControlEnabled`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_set_scheduler_control_enabled(domain: &PowerDomain, enabled: bool);

    /// Returns a reference to `domain->total_normalized_utilization_`.
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` for the
    /// lifetime of the returned reference.
    pub fn cpp_power_domain_total_normalized_utilization_ptr(domain: &PowerDomain) -> &AtomicI64;

    /// Returns whether `controller` is serving requests (`PowerLevelController::is_serving`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`.
    pub fn cpp_power_level_controller_is_serving(controller: &CppPowerLevelController) -> bool;

    /// Returns the control interface of `controller`
    /// (`PowerLevelController::control_interface`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`
    /// whose `control_interface_` is a valid [`ControlInterface`] discriminant.
    pub fn cpp_power_level_controller_control_interface(
        controller: &CppPowerLevelController,
    ) -> ControlInterface;

    /// Queries the current power level argument from `controller` for `domain_id`
    /// (`PowerLevelController::GetCurrentPowerLevel`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`.
    pub fn cpp_power_level_controller_get_current_power_level(
        controller: &CppPowerLevelController,
        domain_id: u32,
        out_level: &mut u64,
    ) -> zx_status_t;

    /// Allocates and constructs a `PowerDomain` (`fbl::MakeRefCountedChecked<PowerDomain>`),
    /// adopting ownership of `controller` via `fbl::ImportFromRawPtr` and returning a raw pointer
    /// exported via `fbl::ExportToRawPtr` (or null on allocation failure).
    ///
    /// # Safety
    ///
    /// `model` must reference a valid initialized `EnergyModel`, and `controller` must be a raw
    /// pointer exported via `fbl::ExportToRawPtr`.
    #[cfg(not(feature = "userspace"))]
    #[expect(improper_ctypes)]
    pub fn cpp_power_domain_create(
        domain_id: u32,
        cpu_mask: u64,
        model: &mut EnergyModel,
        controller: *mut CppPowerLevelController,
    ) -> *const PowerDomain;

    /// Forwards `domain_set` to `Scheduler::SetPowerDomainSet`.
    ///
    /// # Safety
    ///
    /// `domain_set` must reference a valid initialized `PowerDomainSet`.
    #[cfg(not(feature = "userspace"))]
    pub fn cpp_power_management_set_power_domain_set(domain_set: &mut PowerDomainSet);

    /// Creates a fake `PowerDomain` for kernel unit testing, returning a raw pointer exported
    /// via `fbl::ExportToRawPtr` with an acquired reference count that must be adopted by the
    /// caller (e.g., via `RefPtr::try_from_raw`) to avoid leaking the test domain, or null on
    /// allocation failure.
    ///
    /// # Safety
    ///
    /// `model` must reference a valid initialized `EnergyModel`.
    #[cfg(ktest)]
    #[expect(improper_ctypes)]
    pub fn cpp_power_domain_create_for_test(
        domain_id: u32,
        cpu_mask: u64,
        model: &mut EnergyModel,
    ) -> *const PowerDomain;
}

/// FFI entry point for [`PowerLevel::to_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_level_to_processing_rate(
    processing_rate: u64,
    max_processing_rate: u64,
) -> ProcessingRate {
    PowerLevel::to_processing_rate(processing_rate, max_processing_rate)
}

/// FFI entry point for [`PowerLevel::from_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_level_from_processing_rate(
    processing_rate: ProcessingRate,
    max_processing_rate: u64,
) -> u64 {
    PowerLevel::from_processing_rate(processing_rate, max_processing_rate)
}

/// Initializes a `PowerLevel` in-place at `out_level` (`PowerLevel::PowerLevel`).
///
/// # Safety
///
/// If `level.diagnostic_name` is non-null, it must point to a valid NUL-terminated C string or at
/// least `ZX_MAX_NAME_LEN - 1` readable bytes, and `out_level` must be a valid, aligned pointer to
/// writable `PowerLevel` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_level_init(
    out_level: *mut PowerLevel,
    level_index: u8,
    level: &ProcessorPowerLevel,
    max_processing_rate: u64,
) {
    // SAFETY: If `level.diagnostic_name` is non-null, it must point to a valid NUL-terminated C
    // string or at least `ZX_MAX_NAME_LEN - 1` readable bytes, and `out_level` must be a valid,
    // aligned pointer to writable `PowerLevel` storage.
    unsafe { out_level.write(PowerLevel::new(level_index, level, max_processing_rate)) };
}

/// Initializes an empty `EnergyModel` in-place at `model`.
///
/// # Safety
///
/// The caller must ensure that `model` is non-null, aligned to `align_of::<EnergyModel>()`, and
/// points to writable storage of at least `size_of::<EnergyModel>()` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_energy_model_init(model: *mut EnergyModel) {
    // SAFETY: Caller guarantees `model` points to valid, aligned uninitialized storage.
    unsafe { model.write(EnergyModel::new()) };
}

/// Move-constructs an `EnergyModel` at uninitialized `dst` from initialized `src`, leaving `src`
/// in an empty default state.
///
/// # Safety
///
/// The caller must ensure that `dst` is non-null, aligned, and points to uninitialized storage for
/// an `EnergyModel` that does not alias `src`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_energy_model_move(dst: *mut EnergyModel, src: &mut EnergyModel) {
    // SAFETY: Caller guarantees `dst` points to valid uninitialized storage and does not alias
    // `src`.
    unsafe { dst.write(core::mem::take(src)) };
}

/// Move-assigns `src` into initialized `dst`, dropping the previous value of `dst` and leaving
/// `src` in an empty default state.
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_move_assign(dst: &mut EnergyModel, src: &mut EnergyModel) {
    *dst = core::mem::take(src);
}

/// Drops an `EnergyModel` in-place at `model`.
///
/// # Safety
///
/// The caller must ensure that `model` is non-null, aligned, and points to a live, initialized
/// `EnergyModel` that will not be accessed again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_energy_model_destroy(model: *mut EnergyModel) {
    // SAFETY: Caller guarantees `model` points to a valid, initialized `EnergyModel`.
    unsafe { core::ptr::drop_in_place(model) };
}

/// FFI entry point for [`EnergyModel::create`].
///
/// # Safety
///
/// If `level_count > 0`, `levels` must point to a valid array of `level_count` initialized
/// `ProcessorPowerLevel` structs, and each non-null `diagnostic_name` must point to a valid
/// NUL-terminated C string or at least `ZX_MAX_NAME_LEN - 1` readable bytes. If
/// `transition_count > 0`, `transitions` must point to a valid array of `transition_count`
/// initialized `ProcessorPowerLevelTransition` structs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_energy_model_create(
    levels: *const ProcessorPowerLevel,
    level_count: usize,
    transitions: *const ProcessorPowerLevelTransition,
    transition_count: usize,
    max_processing_rate: u64,
    out_model: &mut EnergyModel,
) -> zx_status_t {
    if (levels.is_null() && level_count > 0) || (transitions.is_null() && transition_count > 0) {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: If `level_count > 0`, `levels` was checked non-null and the caller guarantees it
    // points to `level_count` valid elements.
    let levels_slice = unsafe { zr::slice_from_raw_parts(levels, level_count) };
    // SAFETY: If `transition_count > 0`, `transitions` was checked non-null and the caller
    // guarantees it points to `transition_count` valid elements.
    let transitions_slice = unsafe { zr::slice_from_raw_parts(transitions, transition_count) };
    // SAFETY: Caller guarantees every non-null `diagnostic_name` in `levels_slice` points to a
    // valid NUL-terminated C string or at least `ZX_MAX_NAME_LEN - 1` readable bytes.
    let res = unsafe { EnergyModel::create(levels_slice, transitions_slice, max_processing_rate) };
    Status::result_into_raw(res.map(|model| {
        *out_model = model;
    }))
}

/// FFI entry point for [`EnergyModel::max_processing_rate_scale`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_processing_rate_scale(model: &EnergyModel) -> u64 {
    model.max_processing_rate_scale()
}

/// FFI entry point for [`EnergyModel::to_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_to_processing_rate(
    model: &EnergyModel,
    processing_rate: u64,
) -> ProcessingRate {
    model.to_processing_rate(processing_rate)
}

/// FFI entry point for [`EnergyModel::from_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_from_processing_rate(
    model: &EnergyModel,
    processing_rate: ProcessingRate,
) -> u64 {
    model.from_processing_rate(processing_rate)
}

/// FFI entry point for [`EnergyModel::levels`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_levels(
    model: &EnergyModel,
    out_len: &mut usize,
) -> *const PowerLevel {
    let levels = model.levels();
    *out_len = levels.len();
    levels.as_ptr()
}

/// FFI entry point for [`EnergyModel::idle_levels_len`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_idle_power_levels(model: &EnergyModel) -> usize {
    model.idle_levels_len()
}

/// FFI entry point for [`EnergyModel::max_idle_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_idle_power_level(
    model: &EnergyModel,
    out: &mut u8,
) -> bool {
    match model.max_idle_power_level() {
        Some(level) => {
            *out = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`EnergyModel::max_idle_power_coefficient_nw`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_idle_power_coefficient_nw(
    model: &EnergyModel,
    out: &mut u64,
) -> bool {
    match model.max_idle_power_coefficient_nw() {
        Some(coeff) => {
            *out = coeff;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`EnergyModel::max_idle_power_level_interface`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_idle_power_level_interface(
    model: &EnergyModel,
    out: &mut ControlInterface,
) -> bool {
    match model.max_idle_power_level_interface() {
        Some(iface) => {
            *out = iface;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`EnergyModel::max_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_processing_rate(model: &EnergyModel) -> ProcessingRate {
    model.max_processing_rate()
}

/// FFI entry point for [`EnergyModel::transitions_slice`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_transitions(
    model: &EnergyModel,
    out_len: &mut usize,
) -> *const PowerLevelTransition {
    let transitions = model.transitions_slice();
    *out_len = transitions.len();
    transitions.as_ptr()
}

/// FFI entry point for [`EnergyModel::find_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_find_power_level(
    model: &EnergyModel,
    interface_id: u64,
    control_argument: u64,
    out_level: &mut u8,
) -> bool {
    let Ok(iface) = ControlInterface::try_from(interface_id) else {
        return false;
    };
    match model.find_power_level(iface, control_argument) {
        Some(level) => {
            *out_level = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`EnergyModel::find_active_power_level_for_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_find_active_power_level_for_rate(
    model: &EnergyModel,
    processing_rate: ProcessingRate,
) -> *const PowerLevel {
    model
        .find_active_power_level_for_rate(processing_rate)
        .map_or(core::ptr::null(), core::ptr::from_ref)
}

/// FFI entry point for [`PowerDomainSet::create`].
///
/// # Safety
///
/// If `domain_count > 0`, `domains` must point to a valid array of `domain_count` elements of
/// `fbl::RefPtr<PowerDomain>` (layout-compatible with `Option<RefPtr<PowerDomain>>`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_domain_set_create(
    domains: *const Option<RefPtr<PowerDomain>>,
    domain_count: usize,
    out_set: &mut PowerDomainSet,
) -> zx_status_t {
    if domains.is_null() && domain_count > 0 {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: If `domain_count > 0`, `domains` was checked non-null and the caller guarantees it
    // points to `domain_count` valid `fbl::RefPtr<PowerDomain>` elements.
    let slice = unsafe { zr::slice_from_raw_parts(domains, domain_count) };
    Status::result_into_raw(PowerDomainSet::create(slice).map(|set| {
        *out_set = set;
    }))
}

/// FFI entry point for [`PowerDomainSet::create_for_test`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_create_for_test(
    domain: &Option<RefPtr<PowerDomain>>,
    out_set: &mut PowerDomainSet,
) {
    *out_set = PowerDomainSet::create_for_test(domain.as_ref());
}

/// FFI entry point for [`PowerDomainSet::find_by_domain_id`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_find_by_domain_id(
    set: &PowerDomainSet,
    domain_id: u32,
) -> *mut PowerDomain {
    set.find_by_domain_id(domain_id).map_or(core::ptr::null_mut(), |d| RefPtr::as_ptr(d).cast_mut())
}

/// FFI entry point for [`PowerDomainSet::find_by_cpu_num`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_find_by_cpu_num(
    set: &PowerDomainSet,
    cpu_num: u32,
) -> *mut PowerDomain {
    set.find_by_cpu_num(cpu_num).map_or(core::ptr::null_mut(), |d| RefPtr::as_ptr(d).cast_mut())
}

/// FFI entry point for [`PowerDomainSet::lookup_active_power_coefficient`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_lookup_active_power_coefficient(
    set: &PowerDomainSet,
    cpu_num: u32,
    processing_rate: ProcessingRate,
) -> u64 {
    set.lookup_active_power_coefficient(cpu_num, processing_rate)
}

/// FFI entry point for [`PowerDomainSet::lookup_power_cost`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_lookup_power_cost(
    set: &PowerDomainSet,
    cpu_num: u32,
    processing_rate: ProcessingRate,
) -> u64 {
    set.lookup_power_cost(cpu_num, processing_rate)
}

/// FFI entry point for [`PowerDomainSet::lookup_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_lookup_power_level(
    set: &PowerDomainSet,
    cpu_num: u32,
    processing_rate: ProcessingRate,
    out_level: &mut u8,
) -> bool {
    match set.lookup_power_level(cpu_num, processing_rate) {
        Some(level) => {
            *out_level = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerDomainSet::lookup_total_normalized_utilization`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_lookup_total_normalized_utilization(
    set: &PowerDomainSet,
    cpu_num: u32,
) -> Utilization {
    set.lookup_total_normalized_utilization(cpu_num)
}

/// FFI entry point for [`PowerDomainSet::count`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_count(set: &PowerDomainSet) -> usize {
    set.count()
}

/// FFI entry point for [`PowerDomainSet::is_empty`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_is_empty(set: &PowerDomainSet) -> bool {
    set.is_empty()
}
