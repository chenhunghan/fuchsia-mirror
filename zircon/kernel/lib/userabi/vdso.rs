// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::userabi_ffi::HandoffEndElf;
use crate::object::{KernelHandle, VmObjectDispatcher};
use crate::vm::vm_object::VmObject;
use core::mem::MaybeUninit;
use zr::OpaqueFacade;
use zx_types::zx_rights_t;

/// Facade representing the kernel vDSO singleton (`VDso`).
#[repr(C)]
pub struct VDso {
    _opaque: OpaqueFacade,
}

impl VDso {
    pub const NUM_VDSO_VARIANTS: usize = 4;

    // This is called only once, at boot time.
    //
    // The created VDso will retain RefPtrs to the created VmObjectDispatchers,
    // but ownership of the wrapping handles are given to the caller.
    //
    // The RoDso VMO is created in vmo_kernel_handles[Variant::NEXT]
    // with the VDso variants in the other slots.
    pub fn create(
        elf_image: &HandoffEndElf,
    ) -> (
        &'static Self,
        [KernelHandle<VmObjectDispatcher>; Self::NUM_VDSO_VARIANTS],
        KernelHandle<VmObjectDispatcher>,
    ) {
        let mut raw_handles = [const { MaybeUninit::uninit() }; Self::NUM_VDSO_VARIANTS];
        let mut raw_time_values = MaybeUninit::uninit();
        // SAFETY: `elf_image` is a valid `HandoffEnd::Elf` reference, and `raw_handles` and
        // `raw_time_values` point to uninitialized storage that C++ placement-constructs via
        // `ffi::Uninitialized::Initialize` and populates with valid, non-empty `KernelHandle`s.
        unsafe {
            let vdso = cpp_vdso_create(elf_image, &mut raw_handles, &mut raw_time_values);
            (&*vdso, raw_handles.map(|slot| slot.assume_init()), raw_time_values.assume_init())
        }
    }

    /// Returns true if the given VMO is a vDSO VMO.
    pub fn vmo_is_vdso(vmo: &VmObject) -> bool {
        // SAFETY: `vmo.as_raw()` returns a valid `VmObject` pointer.
        unsafe { cpp_vmo_is_vdso(vmo.as_raw().cast()) }
    }

    pub fn vmo_rights(&self) -> zx_rights_t {
        // SAFETY: `self` points to the valid singleton `VDso` instance.
        unsafe { cpp_vdso_vmo_rights(self) }
    }
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// # Safety
    ///
    /// `vmo` must point to a valid `VmObject`.
    fn cpp_vmo_is_vdso(vmo: *const VmObject) -> bool;

    fn cpp_vdso_create(
        elf_image: *const HandoffEndElf,
        vmo_kernel_handles: *mut [MaybeUninit<KernelHandle<VmObjectDispatcher>>;
            VDso::NUM_VDSO_VARIANTS],
        time_values_handle: *mut MaybeUninit<KernelHandle<VmObjectDispatcher>>,
    ) -> *const VDso;

    fn cpp_vdso_vmo_rights(vdso: *const VDso) -> zx_rights_t;
}
