// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::clock_dispatcher::{ClockDispatcher, ClockDispatcherState};
use super::handle::KernelHandle;
use crate::vm::vm_object_paged::VmObjectPaged;
use fbl::RefPtr;
use zx_types::{zx_status_t, zx_time_t};

// C++ FFI declarations
unsafe extern "C" {
    /// Allocates and constructs a C++ `ClockDispatcher` facade around a Rust
    /// `ClockDispatcherState`.
    ///
    /// Neither `KernelHandle<ClockDispatcher>` nor `VmObjectPaged` (which holds
    /// a `PhantomPinned`) has a layout Rust considers FFI-safe, so the lint has
    /// to be silenced here; both are opaque handles onto objects whose
    /// definitions live in C++, and only their addresses cross the boundary.
    ///
    /// # Safety
    ///
    /// `handle_out` must point to writable, uninitialized memory for a
    /// `KernelHandle<ClockDispatcher>`. If `vmo` is non-null, it must be a
    /// reference exported from an `fbl::RefPtr<VmObjectPaged>`, ownership of
    /// which is transferred to the new dispatcher.
    #[allow(improper_ctypes)]
    pub(crate) fn cpp_clock_dispatcher_create(
        options: u64,
        backstop_time: zx_time_t,
        vmo: *mut VmObjectPaged,
        handle_out: *mut core::mem::MaybeUninit<KernelHandle<ClockDispatcher>>,
    ) -> zx_status_t;
}

// FFI trampolines for C++ calling into Rust ClockDispatcherState

/// Initializes a `ClockDispatcherState` in-place into uninitialized memory.
///
/// # Safety
///
/// `ptr` must point to uninitialized memory of at least `size_of::<ClockDispatcherState>()`
/// bytes with proper alignment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_clock_dispatcher_state_init(
    ptr: *mut ClockDispatcherState,
    dispatcher: &ClockDispatcher,
    options: u64,
    backstop_time: zx_time_t,
    vmo: *mut VmObjectPaged,
) {
    let vmo_ref = if vmo.is_null() {
        None
    } else {
        // SAFETY: `vmo` was exported from an active `RefPtr<VmObjectPaged>`.
        Some(unsafe { RefPtr::from_raw(vmo) })
    };
    // SAFETY: `ptr` is valid uninitialized memory for `ClockDispatcherState`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(
            ClockDispatcherState::init(dispatcher, options, backstop_time, vmo_ref),
            ptr,
        );
    }
}

/// Trampoline callback for `get_name`.
///
/// # Safety
///
/// `out_name` must point to a buffer of at least `ZX_MAX_NAME_LEN` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_clock_dispatcher_get_name(
    disp: &ClockDispatcher,
    out_name: *mut core::ffi::c_char,
) -> zx_status_t {
    // SAFETY: `out_name` points to at least `ZX_MAX_NAME_LEN` bytes per caller contract.
    let name_buf = unsafe { &mut *out_name.cast::<[u8; zx_types::ZX_MAX_NAME_LEN]>() };
    disp.get_name(name_buf).map(|()| zx_types::ZX_OK).unwrap_or_else(|s| s.into_raw())
}

/// Trampoline callback for `set_name`.
///
/// # Safety
///
/// If `len > 0`, `name` must point to at least `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_clock_dispatcher_set_name(
    disp: &ClockDispatcher,
    name: *const core::ffi::c_char,
    len: usize,
) -> zx_status_t {
    let slice = if name.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: `name` is non-null and valid for `len` bytes per caller contract.
        unsafe { core::slice::from_raw_parts(name.cast::<u8>(), len) }
    };
    disp.set_name(slice).map(|()| zx_types::ZX_OK).unwrap_or_else(|s| s.into_raw())
}
