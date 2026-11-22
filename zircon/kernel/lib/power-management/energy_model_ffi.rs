// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI declarations and C entry points for `EnergyModel`, `PowerDomain`, `PowerDomainSet`, and
//! `CppPowerLevelController`.

use core::sync::atomic::AtomicI64;

use fbl::RefPtr;
use zx_types::zx_status_t;

use super::energy_model::{CppPowerLevelController, EnergyModel, PowerDomain, PowerDomainSet};
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

    /// Creates a fake `PowerDomain` for unit testing, returning a raw pointer exported via
    /// `fbl::ExportToRawPtr` with an acquired reference count that must be adopted by the caller
    /// (e.g., via `RefPtr::try_from_raw`) to avoid leaking the test domain, or null on allocation
    /// failure.
    ///
    /// # Safety
    ///
    /// `model` must reference a valid initialized `EnergyModel`.
    #[cfg(test)]
    #[expect(improper_ctypes)]
    pub fn cpp_power_domain_create_for_test(
        domain_id: u32,
        cpu_mask: u64,
        model: &mut EnergyModel,
    ) -> *const PowerDomain;
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

/// FFI entry point for [`EnergyModel::create_for_test`].
#[cfg(any(ktest, feature = "userspace"))]
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_create_for_test(
    idle_states: usize,
    active_levels: usize,
    out_model: &mut EnergyModel,
) {
    *out_model = EnergyModel::create_for_test(idle_states, active_levels);
}

/// FFI entry point for [`EnergyModel::max_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_max_processing_rate(model: &EnergyModel) -> ProcessingRate {
    model.max_processing_rate()
}

/// FFI entry point for [`EnergyModel::find_active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_energy_model_find_active_power_level(
    model: &EnergyModel,
    interface_id: u64,
    control_argument: u64,
    out_level: &mut u8,
) -> bool {
    let Ok(iface) = ControlInterface::try_from(interface_id) else {
        return false;
    };
    match model.find_active_power_level(iface, control_argument) {
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
    out_level: &mut u8,
) -> bool {
    match model.find_active_power_level_for_rate(processing_rate) {
        Some(level) => {
            *out_level = level.level();
            true
        }
        None => false,
    }
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

/// FFI entry point for [`PowerDomainSet::lookup_power_cost`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_domain_set_lookup_power_cost(
    set: &PowerDomainSet,
    cpu_num: u32,
    processing_rate: ProcessingRate,
) -> u64 {
    set.lookup_power_cost(cpu_num, processing_rate)
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
