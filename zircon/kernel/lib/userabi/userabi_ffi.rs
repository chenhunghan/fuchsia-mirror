// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::VDso;
use crate::vm::vm_object::VmObject;
use libc::stdio::FILE;
use zx_types::zx_ticks_t;

pub fn boot_options_show(defaults: bool, out: &FILE) {
    // SAFETY: `out` points to a valid `FILE` for the duration of the call.
    unsafe { cpp_boot_options_show(defaults, out) }
}

unsafe extern "C" {
    fn cpp_boot_options_show(defaults: bool, out: *const FILE);
}

/// FFI entry point for [`VDso::vmo_is_vdso`]. C++ passes a non-null `const VmObject*`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_vmo_is_vdso(vmo: &VmObject) -> bool {
    VDso::vmo_is_vdso(vmo)
}

/// FFI entry point for [`VDso::valid_code_mapping`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_valid_code_mapping(vmo_offset: u64, size: usize) -> bool {
    VDso::valid_code_mapping(vmo_offset, size)
}

/// FFI entry point for [`VDso::set_monotonic_ticks_offset`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_set_monotonic_ticks_offset(new_offset: zx_ticks_t) {
    VDso::set_monotonic_ticks_offset(new_offset);
}
