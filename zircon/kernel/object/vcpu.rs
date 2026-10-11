// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::guest::Guest;
use fbl::UniquePtr;
use zx_status::Status;
use zx_types::{
    zx_info_vcpu_t, zx_port_packet_t, zx_status_t, zx_vaddr_t, zx_vcpu_io_t, zx_vcpu_state_t,
};

unsafe extern "C" {
    fn cpp_vcpu_create(
        guest: *mut Guest,
        entry: zx_vaddr_t,
        vcpu_out: *mut *mut Vcpu,
    ) -> zx_status_t;
    fn cpp_vcpu_destroy(vcpu: *mut Vcpu);
    fn cpp_vcpu_enter(vcpu: *mut Vcpu, packet: *mut zx_port_packet_t) -> zx_status_t;
    fn cpp_vcpu_kick(vcpu: *mut Vcpu);
    fn cpp_vcpu_interrupt(vcpu: *mut Vcpu, vector: u32) -> zx_status_t;
    fn cpp_vcpu_read_state(vcpu: *mut Vcpu, vcpu_state: *mut zx_vcpu_state_t) -> zx_status_t;
    fn cpp_vcpu_write_state(vcpu: *mut Vcpu, vcpu_state: *const zx_vcpu_state_t) -> zx_status_t;
    fn cpp_vcpu_write_io_state(vcpu: *mut Vcpu, io_state: *const zx_vcpu_io_t) -> zx_status_t;
    fn cpp_vcpu_get_info(vcpu: *const Vcpu, info_out: *mut zx_info_vcpu_t);
}

fbl::impl_opaque_unique_facade!(
    /// Facade for the C++ `Vcpu` class.
    pub struct Vcpu,
    cpp_vcpu_destroy,
);

impl Vcpu {
    /// Returns a raw pointer to this `Vcpu` for FFI calls.
    #[inline]
    pub fn as_ffi(&self) -> *const Self {
        self as *const Self
    }

    /// Returns a mutable raw pointer to this `Vcpu` for FFI calls.
    #[inline]
    pub fn as_ffi_mut(&self) -> *mut Self {
        self as *const Self as *mut Self
    }

    /// Creates a new `Vcpu` within `guest`, which begins execution at `entry`.
    pub fn create(guest: &Guest, entry: zx_vaddr_t) -> Result<UniquePtr<Self>, Status> {
        let mut vcpu_raw = core::ptr::null_mut();
        // SAFETY: `guest.as_ffi_mut()` is a valid `Guest` pointer for the lifetime of `guest`,
        // and `vcpu_raw` is a valid out-pointer to receive the created `Vcpu*` on `ZX_OK`.
        let status = unsafe { cpp_vcpu_create(guest.as_ffi_mut(), entry, &mut vcpu_raw) };
        Status::ok(status)?;
        // SAFETY: `cpp_vcpu_create` succeeded and returned a non-null owning `Vcpu*` released
        // from `ktl::unique_ptr<Vcpu>`.
        Ok(unsafe { UniquePtr::from_raw(vcpu_raw) })
    }

    /// Enters the guest, returning once the guest traps, faults, or is kicked.
    pub fn enter(&self, packet: &mut zx_port_packet_t) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`, and
        // `packet` is a valid out-pointer to an initialized `zx_port_packet_t`.
        Status::ok(unsafe { cpp_vcpu_enter(self.as_ffi_mut(), packet) })
    }

    /// Kicks the VCPU out of the guest, causing a pending or future `enter` to return.
    pub fn kick(&self) {
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`.
        unsafe { cpp_vcpu_kick(self.as_ffi_mut()) }
    }

    /// Raises an interrupt on the VCPU.
    pub fn interrupt(&self, vector: u32) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`.
        Status::ok(unsafe { cpp_vcpu_interrupt(self.as_ffi_mut(), vector) })
    }

    /// Reads the VCPU's register state.
    pub fn read_state(&self) -> Result<zx_vcpu_state_t, Status> {
        let mut vcpu_state = zx_vcpu_state_t::default();
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`, and
        // `&mut vcpu_state` is a valid out-pointer to an initialized `zx_vcpu_state_t`.
        Status::ok(unsafe { cpp_vcpu_read_state(self.as_ffi_mut(), &mut vcpu_state) })?;
        Ok(vcpu_state)
    }

    /// Writes the VCPU's register state.
    pub fn write_state(&self, vcpu_state: &zx_vcpu_state_t) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`, and
        // `vcpu_state` is a valid pointer that is only read from.
        Status::ok(unsafe { cpp_vcpu_write_state(self.as_ffi_mut(), vcpu_state) })
    }

    /// Writes the VCPU's pending I/O state.
    pub fn write_io_state(&self, io_state: &zx_vcpu_io_t) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_mut()` is a valid `Vcpu` pointer for the lifetime of `self`, and
        // `io_state` is a valid pointer that is only read from.
        Status::ok(unsafe { cpp_vcpu_write_io_state(self.as_ffi_mut(), io_state) })
    }

    /// Returns information about the VCPU.
    pub fn get_info(&self) -> zx_info_vcpu_t {
        let mut info = zx_info_vcpu_t::default();
        // SAFETY: `self.as_ffi()` is a valid `Vcpu` pointer for the lifetime of `self`, and
        // `&mut info` is a valid out-pointer that `cpp_vcpu_get_info` always initializes.
        unsafe { cpp_vcpu_get_info(self.as_ffi(), &mut info) };
        info
    }
}
