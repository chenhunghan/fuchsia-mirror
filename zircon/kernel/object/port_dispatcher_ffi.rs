// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::{Handle, HandleRef, KernelHandle};
use super::port_dispatcher::{
    PortDispatcher, PortDispatcherState, PortObserver, PortPacket, PortSignalObserver,
};
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
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
    pub fn cpp_port_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<PortDispatcher>>,
        rights_out: *mut MaybeUninit<zx_types::zx_rights_t>,
    ) -> zx_status_t;

    pub(super) fn cpp_port_signal_observer_init(
        observer: *mut MaybeUninit<PortSignalObserver>,
        options: u32,
    );
    pub(super) fn cpp_port_signal_observer_destroy(observer: *mut PortSignalObserver);
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

/// Invoked by C++ `PortSignalObserver::OnMatch` when watched signals match.
///
/// # Safety
///
/// `observer` must point to the `signal_observer` field at offset 0 of a valid, live
/// `PortObserver` with provenance over the entire `PortObserver`, and the watched `Dispatcher`'s
/// lock must be held. `queue_to_own` must be either null or a valid pointer to an
/// `OwnedWaitQueue` that remains live for the duration of the call. This call may destroy
/// `observer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_observer_on_match(
    observer: *mut PortSignalObserver,
    options: u32,
    signals: zx_signals_t,
    queue_to_own: *mut OwnedWaitQueue,
) {
    // SAFETY: Caller guarantees `queue_to_own` is null or valid for the duration of the call.
    let queue_to_own = unsafe { queue_to_own.as_ref() };
    // SAFETY: `signal_observer` is at offset 0 of `PortObserver` and caller upholds the
    // `PortObserver::on_match` contract.
    unsafe {
        PortObserver::on_match(observer.cast::<PortObserver>(), options, signals, queue_to_own);
    }
}

/// Invoked by C++ `PortSignalObserver::OnCancel` when the watched handle is closed.
///
/// # Safety
///
/// `observer` must point to the `signal_observer` field at offset 0 of a valid, live
/// `PortObserver` with provenance over the entire `PortObserver`, and the watched `Dispatcher`'s
/// lock must be held. This call may destroy `observer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_observer_on_cancel(
    observer: *mut PortSignalObserver,
    signals: zx_signals_t,
) {
    // SAFETY: `signal_observer` is at offset 0 of `PortObserver` and caller upholds the
    // `PortObserver::on_cancel` contract.
    unsafe {
        PortObserver::on_cancel(observer.cast::<PortObserver>(), signals);
    }
}

/// Invoked by C++ `PortSignalObserver::MatchesKey` to check whether `observer` matches
/// `(port, key)`.
///
/// # Safety
///
/// `observer` must point to the `signal_observer` field at offset 0 of a valid, live
/// `PortObserver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_observer_matches_key(
    observer: *const PortSignalObserver,
    port: *const core::ffi::c_void,
    key: u64,
) -> bool {
    // SAFETY: `signal_observer` is at offset 0 of `PortObserver` and `observer` is live.
    unsafe { (*observer.cast::<PortObserver>()).matches_key(port, key) }
}

/// Creates and initializes a `PortObserver` registered with `disp`, returning its
/// `PortSignalObserver` base pointer through `out_observer`.
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
    out_observer: &mut *mut PortSignalObserver,
) -> zx_status_t {
    // SAFETY: `handle` is a live `Handle` and the caller guarantees the handle table lock is held
    // for the duration of this call, which bounds the borrow.
    let handle = unsafe { HandleRef::from_raw(NonNull::from(handle)) };
    zx_status::Status::result_into_raw(disp.make_observer(options, handle, key, signals).map(
        |observer| {
            *out_observer = observer.as_ptr().cast::<PortSignalObserver>();
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
    // SAFETY: `disp` is valid and `port_packet` is non-null and valid per the caller contract.
    zx_status::Status::result_into_raw(unsafe { disp.queue(port_packet) })
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
