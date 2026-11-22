// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::{Handle, HandleRef, KernelHandle};
use super::port_dispatcher::{PortDispatcher, PortDispatcherState, PortPacket};
use super::signal_observer::SignalObserver;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use zx_types::{zx_signals_t, zx_status_t};

crate::object::dispatcher::impl_dispatcher_state_init!(
    PortDispatcher,
    PortDispatcherState,
    options: u32,
);

// C++ FFI declarations
unsafe extern "C" {
    /// Allocates and initializes a new `PortDispatcher` wrapped in `handle_out`.
    ///
    /// # Safety
    ///
    /// `handle_out` must be non-null, properly aligned, and point to writable uninitialized
    /// storage for a `KernelHandle<PortDispatcher>`.
    pub(super) fn cpp_port_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<PortDispatcher>>,
    ) -> zx_status_t;
}

/// Handles zero handles condition for the port dispatcher.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_on_zero_handles(disp: &PortDispatcher) {
    disp.on_zero_handles();
}

/// Creates and initializes a `PortObserver` registered with `disp`, returning its
/// [`SignalObserver`] base pointer through `out_observer`.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `handle` must point to a valid `Handle` and the caller must hold a read lock on the handle
/// table containing it for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_make_observer(
    disp: &PortDispatcher,
    options: u32,
    handle: &Handle,
    key: u64,
    signals: zx_signals_t,
    out_observer: &mut *mut SignalObserver,
) -> zx_status_t {
    let handle = {
        // SAFETY: `handle` is a live `Handle` and the caller guarantees the handle table lock is
        // held for the duration of this call, which bounds the borrow.
        unsafe { HandleRef::from_raw(NonNull::from(handle)) }
    };
    zx_status::Status::result_into_raw(disp.make_observer(options, handle, key, signals).map(
        |observer| {
            // `signal_observer` is at offset 0 of `PortObserver`; casting from `observer.as_ptr()`
            // preserves provenance over the entire `PortObserver`.
            *out_observer = observer.as_ptr().cast::<SignalObserver>();
        },
    ))
}

/// Queues a port packet to the port.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`, and `port_packet` must be null or point
/// to a valid, initialized `PortPacket` that is not in any container.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_queue(
    disp: &PortDispatcher,
    port_packet: *mut PortPacket,
) -> zx_status_t {
    let Some(port_packet) = NonNull::new(port_packet) else {
        return zx_status::Status::INVALID_ARGS.into_raw();
    };
    let res = {
        // SAFETY: `disp` is valid and `port_packet` is non-null and valid per the caller contract.
        unsafe { disp.queue(port_packet) }
    };
    zx_status::Status::result_into_raw(res)
}

/// Cancels a specific queued port packet.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`, and `port_packet` must point to a valid
/// `PortPacket` that is either not in any container or queued on `disp`, and whose `allocator`
/// (if non-null) points to a live `PortAllocator`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_cancel_queued_packet(
    disp: &PortDispatcher,
    port_packet: *mut PortPacket,
) -> bool {
    // SAFETY: Caller guarantees `port_packet` validity.
    unsafe { disp.cancel_queued_packet(port_packet) }
}
