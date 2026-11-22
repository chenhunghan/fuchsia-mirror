// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::diagnostics::{dump_aspace_vm_objects, ktrace_report_live_processes};
use super::process_dispatcher::ProcessDispatcher;
use crate::vm::vm_object::VmObject;
use core::ffi::c_char;
use zx_types::{zx_handle_t, zx_koid_t, zx_rights_t};

/// Emits a kernel object record for the kernel and every live process to the trace buffer.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_report_live_processes() {
    ktrace_report_live_processes();
}

/// Dumps the VMOs mapped under the VmAspace of `process`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_diagnostics_dump_aspace_vm_objects(
    process: &ProcessDispatcher,
    format_unit: c_char,
) {
    dump_aspace_vm_objects(process, format_unit);
}

unsafe extern "C" {
    #[link_name = "cpp_dump_vm_object"]
    pub(super) safe fn dump_vm_object(
        vmo: &VmObject,
        format_unit: c_char,
        handle: zx_handle_t,
        rights: zx_rights_t,
        koid: zx_koid_t,
    );
}

#[cfg(console_enabled)]
unsafe extern "C" {
    fn cpp_dump_all_vm_objects(hidden_only: bool, format_unit: c_char);
    fn cpp_dump_orphaned_slices(format_unit: c_char);
    fn cpp_dump_orphaned_mapped_vmos(format_unit: c_char);
    fn cpp_dump_process_vm_objects(id: zx_koid_t, format_unit: c_char);
    fn cpp_dump_vm_object_cow_tree(id: zx_koid_t);
}

/// Dumps all (or hidden-only) VMOs in the system using `format_unit`.
#[cfg(console_enabled)]
pub(super) fn dump_all_vm_objects(hidden_only: bool, format_unit: c_char) {
    // SAFETY: Takes only scalar arguments and synchronizes VMO list access internally.
    unsafe { cpp_dump_all_vm_objects(hidden_only, format_unit) };
}

/// Dumps summary statistics for orphaned slice VMOs using `format_unit`.
#[cfg(console_enabled)]
pub(super) fn dump_orphaned_slices(format_unit: c_char) {
    // SAFETY: Takes only a scalar format unit and synchronizes VMO list access internally.
    unsafe { cpp_dump_orphaned_slices(format_unit) };
}

/// Dumps summary statistics for orphaned mapped VMOs using `format_unit`.
#[cfg(console_enabled)]
pub(super) fn dump_orphaned_mapped_vmos(format_unit: c_char) {
    // SAFETY: Takes only a scalar format unit and synchronizes VMO list access internally.
    unsafe { cpp_dump_orphaned_mapped_vmos(format_unit) };
}

/// Dumps VMOs held or mapped by the process with koid `id` using `format_unit`.
#[cfg(console_enabled)]
pub(super) fn dump_process_vm_objects(id: zx_koid_t, format_unit: c_char) {
    // SAFETY: Takes only scalar arguments and synchronizes process/VMO access internally.
    unsafe { cpp_dump_process_vm_objects(id, format_unit) };
}

/// Dumps the copy-on-write tree for the VMO with koid `id`.
#[cfg(console_enabled)]
pub(super) fn dump_vm_object_cow_tree(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes VMO/COW-tree access internally.
    unsafe { cpp_dump_vm_object_cow_tree(id) };
}
