// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::port_dispatcher::PortDispatcher;
use crate::vm::vm_address_region::VmAddressRegion;
use fbl::{RefPtr, UniquePtr};
use zx_status::Status;
use zx_types::{zx_status_t, zx_vaddr_t};

unsafe extern "C" {
    fn cpp_guest_create(guest_out: *mut *mut Guest) -> zx_status_t;
    fn cpp_guest_root_vmar(guest: *const Guest) -> *mut VmAddressRegion;
    fn cpp_guest_destroy(guest: *mut Guest);
    fn cpp_guest_set_trap(
        guest: *mut Guest,
        kind: u32,
        addr: zx_vaddr_t,
        len: usize,
        port: *mut PortDispatcher,
        key: u64,
    ) -> zx_status_t;
}

fbl::impl_opaque_unique_facade!(
    /// Facade for the C++ `Guest` class.
    pub struct Guest,
    cpp_guest_destroy,
);

impl Guest {
    /// Returns a raw pointer to this `Guest` for FFI calls.
    #[inline]
    pub fn as_ffi(&self) -> *const Self {
        self as *const Self
    }

    /// Returns a mutable raw pointer to this `Guest` for FFI calls.
    #[inline]
    pub fn as_ffi_mut(&self) -> *mut Self {
        self as *const Self as *mut Self
    }

    /// Creates a new `Guest` instance.
    pub fn create() -> Result<UniquePtr<Self>, Status> {
        let mut guest_raw = core::ptr::null_mut();
        // SAFETY: `guest_raw` is a valid out-pointer to receive the created `Guest*` on `ZX_OK`.
        let status = unsafe { cpp_guest_create(&mut guest_raw) };
        Status::ok(status)?;
        // SAFETY: `cpp_guest_create` succeeded and returned a non-null owning `Guest*` released
        // from `ktl::unique_ptr<Guest>`.
        Ok(unsafe { UniquePtr::from_raw(guest_raw) })
    }

    /// Returns the root `VmAddressRegion` of the guest physical address space.
    pub fn root_vmar(&self) -> RefPtr<VmAddressRegion> {
        // SAFETY: `self.as_ffi()` is a valid `Guest` pointer, and `cpp_guest_root_vmar` returns
        // a non-null `VmAddressRegion*` exported via `fbl::ExportToRawPtr`.
        unsafe { RefPtr::from_raw(cpp_guest_root_vmar(self.as_ffi())) }
    }

    /// Sets a trap on the guest within the specified address range.
    pub fn set_trap(
        &self,
        kind: u32,
        addr: zx_vaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> Result<(), Status> {
        let port_raw = port.map_or(core::ptr::null_mut(), |p| RefPtr::into_raw(p).cast_mut());
        // SAFETY: `self.as_ffi_mut()` is a valid `Guest` pointer for the lifetime of `self`, and
        // `port_raw` is either null or a valid raw `PortDispatcher` pointer carrying an owned
        // reference that `cpp_guest_set_trap` adopts via `fbl::ImportFromRawPtr`.
        let status =
            unsafe { cpp_guest_set_trap(self.as_ffi_mut(), kind, addr, len, port_raw, key) };
        Status::ok(status)
    }
}
