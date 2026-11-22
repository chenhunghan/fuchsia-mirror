// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use zx_types::zx_signals_t;

use crate::kernel::owned_wait_queue::OwnedWaitQueue;

use super::signal_observer::SignalObserver;

/// Invoked by C++ `SignalObserver::OnMatch` under the watched `Dispatcher`'s lock.
///
/// # Safety
///
/// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
///   concrete observer with provenance over the entire concrete observer allocation, and must
///   already be unlinked (or never linked) from `Dispatcher::observers_`.
/// - The watched `Dispatcher`'s lock must be held by the caller.
/// - `queue_to_own` must be null or point to a valid `OwnedWaitQueue` that remains live for the
///   duration of the call.
/// - This call may destroy `observer`, so `observer` must not be accessed after it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_signal_observer_on_match(
    observer: *mut SignalObserver,
    signals: zx_signals_t,
    queue_to_own: *mut c_void,
) {
    // SAFETY: Caller guarantees `queue_to_own` is null or a valid `OwnedWaitQueue` for the
    // duration of the call, and upholds all `SignalObserver::on_match` preconditions.
    unsafe {
        SignalObserver::on_match(observer, signals, queue_to_own.cast::<OwnedWaitQueue>().as_ref());
    }
}

/// Invoked by C++ `SignalObserver::OnCancel` under the watched `Dispatcher`'s lock.
///
/// # Safety
///
/// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
///   concrete observer with provenance over the entire concrete observer allocation, and must
///   already be unlinked from `Dispatcher::observers_`.
/// - The watched `Dispatcher`'s lock must be held by the caller.
/// - This call may destroy `observer`, so `observer` must not be accessed after it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_signal_observer_on_cancel(
    observer: *mut SignalObserver,
    signals: zx_signals_t,
) {
    // SAFETY: Caller upholds all `SignalObserver::on_cancel` preconditions.
    unsafe {
        SignalObserver::on_cancel(observer, signals);
    }
}

/// Invoked by C++ `SignalObserver::MatchesKey` under the watched `Dispatcher`'s lock.
///
/// # Safety
///
/// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
///   concrete observer with provenance over the entire concrete observer allocation.
/// - The watched `Dispatcher`'s lock must be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_signal_observer_matches_key(
    observer: *const SignalObserver,
    port: *const c_void,
    key: u64,
) -> bool {
    // SAFETY: Caller upholds all `SignalObserver::matches_key` preconditions.
    unsafe { SignalObserver::matches_key(observer, port, key) }
}
