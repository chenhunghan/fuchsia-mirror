// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::vm_aspace::VmAspace;
use super::vm_object::VmObject;
use core::ptr::NonNull;
use fbl::{HasRefCount, OpaqueRefCountedFacade, Recyclable, RefCounted, RefPtr};
use vm_address_region_bindings as bindings;
use zr::ToMutPtr;
use zx_status::Status;

/// A representation of the mapping of a VMO into the address space
#[repr(C)]
pub struct VmMapping {
    _facade: OpaqueRefCountedFacade,
}

impl HasRefCount for VmMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        let raw = unsafe { bindings::cpp_vm_mapping_get_ref_counted(self.as_ffi_ptr()) };
        // SAFETY: `raw` points to the `fbl::RefCounted` subobject of `self`.
        unsafe { &*raw.cast::<RefCounted>() }
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmMapping {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` is the last reference to a live `VmMapping`.
        unsafe { bindings::cpp_vm_mapping_free(ptr.as_ptr().cast()) }
    }
}

impl VmMapping {
    fn as_ffi_ptr(&self) -> *mut bindings::VmMapping {
        self.to_mut_ptr().cast()
    }

    /// Marks a mapping as eligible for merging with adjacent compatible mappings.
    pub fn mark_mergeable(mapping: RefPtr<VmMapping>) {
        // SAFETY: `RefPtr::into_raw` transfers ownership of the reference to `ImportFromRawPtr`.
        unsafe {
            bindings::cpp_vm_mapping_mark_mergeable(RefPtr::into_raw(mapping).cast_mut().cast())
        }
    }

    /// Destroys this mapping, unmapping all pages and removing dependencies on the underlying VMO.
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe { bindings::cpp_vm_mapping_destroy(self.as_ffi_ptr()) })
    }

    /// Returns a reference to the address space this mapping belongs to.
    pub fn aspace(&self) -> &RefPtr<VmAspace> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        unsafe { &*(bindings::cpp_vm_mapping_aspace(self.as_ffi_ptr()).cast()) }
    }

    /// Returns the base virtual address of this mapping.
    pub fn base(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        unsafe { bindings::cpp_vm_mapping_base(self.as_ffi_ptr()) }
    }

    /// Returns the size in bytes of this mapping.
    pub fn size(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        unsafe { bindings::cpp_vm_mapping_size(self.as_ffi_ptr()) }
    }

    /// Returns the creation flags of this mapping.
    pub fn flags(&self) -> u32 {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        unsafe { bindings::cpp_vm_mapping_flags(self.as_ffi_ptr()) }
    }

    /// Returns the offset into the underlying VMO for this mapping.
    pub fn object_offset(&self) -> u64 {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        unsafe { bindings::cpp_vm_mapping_object_offset(self.as_ffi_ptr()) }
    }

    /// Convenience wrapper for vmo()->DecommitRange() with the necessary
    /// offset modification and locking.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_decommit_range(self.as_ffi_ptr(), offset, len)
        })
    }

    /// Map in pages from the underlying vm object, optionally committing pages as it goes.
    /// |ignore_existing| controls whether existing hardware mappings in the specified range should
    /// be ignored or treated as an error. |ignore_existing| should only be set to true for user
    /// mappings where populating mappings may already be racy with multiple threads, and where we
    /// are already tolerant of mappings being arbitrarily created and destroyed.
    pub fn map_range(
        &self,
        offset: usize,
        len: usize,
        commit: bool,
        ignore_existing: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_map_range(
                self.as_ffi_ptr(),
                offset,
                len,
                commit,
                ignore_existing,
            )
        })
    }

    /// Unlocked convenience wrapper around unmap for testing.
    pub fn debug_unmap(&self, base: usize, size: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe { bindings::cpp_vm_mapping_debug_unmap(self.as_ffi_ptr(), base, size) })
    }

    /// Unlocked convenience wrapper around protect for testing.
    pub fn debug_protect(
        &self,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_debug_protect(
                self.as_ffi_ptr(),
                base,
                size,
                new_arch_mmu_flags,
            )
        })
    }

    /// Returns the underlying VMO backing this mapping.
    pub fn vmo(&self) -> Option<RefPtr<VmObject>> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        let raw = unsafe { bindings::cpp_vm_mapping_vmo(self.as_ffi_ptr()) };
        // SAFETY: `raw` is null or an owned reference to a live `VmObject`.
        unsafe { RefPtr::try_from_raw(raw.cast()) }
    }

    /// Informs the mapping that a write is going to be performed to the backing VMO.
    ///
    /// If necessary, creates a private clone of the VMO and returns a new mapping.
    pub fn force_writable(&self) -> Result<RefPtr<VmMapping>, Status> {
        let mut out: *mut bindings::VmMapping = core::ptr::null_mut();
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping` and `out` is writable.
        let status =
            unsafe { bindings::cpp_vm_mapping_force_writable(self.as_ffi_ptr(), &mut out) };
        Status::ok(status)?;
        // SAFETY: `out` is null or an owned reference to a live `VmMapping`.
        Ok(unsafe {
            RefPtr::try_from_raw(out.cast()).expect("Should never be null with OK status")
        })
    }
}
