// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Rust bindings and safe/unsafe accessors for physboot handoff structures.

#![no_std]

#[expect(non_camel_case_types, non_upper_case_globals)]
mod sys {
    include!(rustenv_path::envpath!("HANDOFF_BINDINGS_RS"));
}

pub use libarch::EarlyTicks;
pub use sys::*;

use core::ffi::{CStr, c_char};
use core::mem::MaybeUninit;

unsafe extern "C" {
    // Matches the `kBootConstants` label defined in `.rodata.kBootConstants`
    // in `zircon/kernel/top/handoff.rs`.
    #[link_name = "kBootConstants"]
    static BOOT_CONSTANTS: BootConstants;

    // Matches `gPhysHandoff` defined in `zircon/kernel/top/handoff.rs`.
    #[link_name = "gPhysHandoff"]
    static mut PHYS_HANDOFF: *mut PhysHandoff;

    fn cpp_phys_vmo_log(vmo: *const PhysVmo, prefix: *const c_char);
    fn cpp_phys_handoff_call_init_array(handoff: *const PhysHandoff);
    fn cpp_phys_handoff_extra_vmos(
        handoff: *const PhysHandoff,
        count: *mut usize,
    ) -> *const PhysVmo;
    fn cpp_phys_handoff_vmars(handoff: &PhysHandoff, count: &mut usize) -> *const PhysVmar;
    fn cpp_phys_handoff_temporary_vmar(handoff: &PhysHandoff) -> *const PhysVmar;
    fn cpp_phys_handoff_heap_vmar(handoff: &PhysHandoff) -> *const PhysVmar;
    fn cpp_phys_vmar_mappings(vmar: &PhysVmar, count: &mut usize) -> *const PhysMapping;
    fn cpp_phys_vmar_permissions(vmar: &PhysVmar, out: *mut PhysMapping_Permissions);
    fn cpp_phys_vmar_log(vmar: &PhysVmar, prefix: *const c_char);
    fn cpp_phys_mapping_permissions_readable(perms: &PhysMapping_Permissions) -> bool;
    fn cpp_phys_mapping_permissions_writable(perms: &PhysMapping_Permissions) -> bool;
    fn cpp_phys_mapping_permissions_executable(perms: &PhysMapping_Permissions) -> bool;
}

impl BootConstants {
    /// Returns a shared reference to the global `BootConstants` populated by physboot.
    pub fn get() -> &'static Self {
        // SAFETY: `BOOT_CONSTANTS` is populated in `.rodata` by physboot before kernel entry
        // and is never mutated afterward.
        unsafe { &BOOT_CONSTANTS }
    }
}

impl PhysBootTimes {
    /// Returns the early boot timestamp sampled at `i`.
    pub const fn get(&self, i: PhysBootTimes_Index) -> EarlyTicks {
        self.timestamps_[i as usize]
    }
}

impl PhysVmObject {
    /// Returns the NUL-terminated name of this hand-off VM object.
    #[inline]
    pub fn name(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.name).expect("PhysVmObject::name is NUL-terminated")
    }
}

impl PhysVmo {
    /// Maximum number of extra VMOs expected in the hand-off beyond the
    /// explicitly enumerated ones.
    pub const MAX_EXTRA_HANDOFF_PHYS_VMOS: usize = PhysVmo_kMaxExtraHandoffPhysVmos;

    /// Returns the page-aligned size of the VMO's physical memory range.
    pub const fn size_bytes<const PAGE_SIZE: usize>(&self) -> usize {
        (self.stream_size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }

    /// Returns the VMO name up to the first NUL byte, or `""` if it is not UTF-8.
    pub fn name(&self) -> &str {
        self._base.name().to_str().unwrap_or("")
    }

    /// Logs this VMO's physical range and name with `prefix`.
    pub fn log(&self, prefix: &CStr) {
        // SAFETY: `self` and `prefix` are valid pointers for the duration of the call, and
        // `PhysVmo::Log` bounds `name` to `name.size()`.
        unsafe { cpp_phys_vmo_log(self, prefix.as_ptr()) }
    }
}

impl PhysElfImage {
    /// Sentinel value in `vmar.mappings[n].paddr` indicating zero-fill pages
    /// rather than pages backed by `PhysVmo`.
    pub const ZERO_FILL: usize = PhysElfImage_kZeroFill;
}

impl PhysMapping {
    /// Returns the NUL-terminated name of this mapping.
    #[inline]
    pub fn name(&self) -> &CStr {
        self._base.name()
    }
}

impl PhysMapping_Permissions {
    #[inline]
    pub fn readable(self) -> bool {
        // SAFETY: `self` is a valid `PhysMapping::Permissions`.
        unsafe { cpp_phys_mapping_permissions_readable(&self) }
    }

    #[inline]
    pub fn writable(self) -> bool {
        // SAFETY: `self` is a valid `PhysMapping::Permissions`.
        unsafe { cpp_phys_mapping_permissions_writable(&self) }
    }

    #[inline]
    pub fn executable(self) -> bool {
        // SAFETY: `self` is a valid `PhysMapping::Permissions`.
        unsafe { cpp_phys_mapping_permissions_executable(&self) }
    }
}

impl PhysVmar {
    /// Returns the NUL-terminated name of this VMAR.
    #[inline]
    pub fn name(&self) -> &CStr {
        self._base.name()
    }

    /// Logs the VMAR and its mappings.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `PhysVmar` initialized by `physboot`, and temporary hand-off memory
    /// backing `self.mappings` must still be mapped.
    #[inline]
    pub unsafe fn log(&self, prefix: &CStr) {
        // SAFETY: Caller guarantees `self` is a valid `PhysVmar` and temporary hand-off memory is
        // mapped; `prefix` is NUL-terminated.
        unsafe { cpp_phys_vmar_log(self, prefix.as_ptr()) };
    }

    /// Returns the union of all mapping permissions in this VMAR.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `PhysVmar` initialized by `physboot`, and temporary hand-off memory
    /// backing `self.mappings` must still be mapped.
    #[inline]
    pub unsafe fn permissions(&self) -> PhysMapping_Permissions {
        let mut out = MaybeUninit::uninit();
        // SAFETY: Caller guarantees `self` is a valid `PhysVmar` and temporary hand-off memory is
        // mapped; `out` is valid writable storage.
        unsafe {
            cpp_phys_vmar_permissions(self, out.as_mut_ptr());
            out.assume_init()
        }
    }

    /// Returns the mappings within this VMAR.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `PhysVmar` initialized by `physboot`, and temporary hand-off memory
    /// backing `self.mappings` must remain mapped for the lifetime of the returned slice.
    #[inline]
    pub unsafe fn mappings(&self) -> &[PhysMapping] {
        let mut count = 0;
        // SAFETY: Caller guarantees `self` is a valid `PhysVmar` and temporary hand-off memory
        // remains mapped.
        unsafe { zr::slice_from_raw_parts(cpp_phys_vmar_mappings(self, &mut count), count) }
    }
}

impl PhysHandoff {
    /// Returns the raw `gPhysHandoff` pointer (null before `post_handoff_bootstrap` or after
    /// `end_handoff`).
    ///
    /// # Safety
    ///
    /// Must not be called concurrently with `post_handoff_bootstrap` or `end_handoff` mutating
    /// `gPhysHandoff`.
    pub unsafe fn as_ptr() -> *const Self {
        // SAFETY: Caller guarantees no concurrent write to `gPhysHandoff` is in progress.
        unsafe { PHYS_HANDOFF.cast_const() }
    }

    /// Returns a shared reference to the global `PhysHandoff` structure.
    ///
    /// # Safety
    ///
    /// Must only be called after `post_handoff_bootstrap` sets `gPhysHandoff` and before
    /// `end_handoff` unmaps and frees temporary handoff memory. The returned reference must
    /// not be held across `end_handoff`.
    #[inline]
    pub unsafe fn get<'a>() -> &'a Self {
        // SAFETY: Caller guarantees `gPhysHandoff` has been initialized and temporary handoff
        // memory has not yet been freed by `end_handoff`.
        unsafe { Self::as_ptr().as_ref() }.expect("PhysHandoff::get() called with null pointer")
    }

    /// Invokes the ELF `DT_INIT_ARRAY` constructors handed off from physboot.
    ///
    /// # Safety
    ///
    /// Must be called at most once on the boot CPU during early boot after early thread and
    /// debuglog initialization.
    pub unsafe fn call_init_array(&self) {
        // SAFETY: `self` is valid and the caller guarantees single-call early-boot execution.
        unsafe { cpp_phys_handoff_call_init_array(self) }
    }

    /// Returns the temporary handoff slice of extra `PhysVmo`s.
    ///
    /// # Safety
    ///
    /// Must only be called before `end_handoff()` unmaps and frees temporary handoff memory,
    /// and the returned slice must not be used after `end_handoff()`.
    pub unsafe fn extra_vmos(&self) -> &[PhysVmo] {
        let mut count = 0;
        // SAFETY: `self` is a valid `PhysHandoff`, `count` is a valid out-pointer, and the
        // caller guarantees temporary handoff memory remains live.
        let ptr = unsafe { cpp_phys_handoff_extra_vmos(self, &mut count) };
        // SAFETY: `ptr` and `count` describe a valid array (or empty span) in temporary handoff
        // memory for the lifetime of `self`.
        unsafe { zr::slice_from_raw_parts(ptr, count) }
    }

    /// Returns the kernel's virtual heap VMAR description, if any.
    #[inline]
    pub fn heap_vmar(&self) -> Option<&PhysVmar> {
        // SAFETY: `self` is a valid `PhysHandoff` reference and `heap_vmar` (`std::optional`) is
        // stored inline inside `PhysHandoff`, so any non-null pointer returned by
        // `cpp_phys_handoff_heap_vmar` is valid and aligned for the lifetime of `self`.
        unsafe { cpp_phys_handoff_heap_vmar(self).as_ref() }
    }

    /// Returns the permanent VMARs to construct.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `PhysHandoff` initialized by `physboot`, and temporary hand-off
    /// memory backing `self.vmars` must remain mapped for the lifetime of the returned slice.
    #[inline]
    pub unsafe fn vmars(&self) -> &[PhysVmar] {
        let mut count = 0;
        // SAFETY: Caller guarantees `self` is a valid `PhysHandoff` and temporary hand-off memory
        // remains mapped.
        unsafe { zr::slice_from_raw_parts(cpp_phys_handoff_vmars(self, &mut count), count) }
    }

    /// Returns the VMAR comprising all temporary hand-off mappings.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `PhysHandoff` initialized by `physboot`, and temporary hand-off
    /// memory backing `self.temporary_vmar` must remain mapped for the lifetime of the returned
    /// reference.
    #[inline]
    pub unsafe fn temporary_vmar(&self) -> &PhysVmar {
        // SAFETY: Caller guarantees `self` is a valid `PhysHandoff` and temporary hand-off memory
        // remains mapped.
        unsafe { cpp_phys_handoff_temporary_vmar(self).as_ref() }
            .expect("PhysHandoff::temporary_vmar is non-null")
    }
}
