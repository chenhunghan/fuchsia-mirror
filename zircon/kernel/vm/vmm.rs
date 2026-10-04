// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::vm_aspace::VmAspace;
use vmm_bindings as bindings;
use zr::ToMutPtr;

/// set the current user aspace as active on the current thread.
/// [`None`] is a valid argument, which unmaps the current user address space
///
/// # Safety
///
/// `aspace` remains valid while active and the previous active aspace is restored before `aspace` is destroyed.
pub unsafe fn set_active_aspace(aspace: Option<&VmAspace>) {
    let aspace_ptr: *mut vm_aspace_bindings::VmAspace =
        aspace.map_or(core::ptr::null_mut(), |a| a.to_mut_ptr().cast());
    // SAFETY: Caller attests to the preconditions.
    unsafe { bindings::cpp_vmm_set_active_aspace(aspace_ptr) };
}
