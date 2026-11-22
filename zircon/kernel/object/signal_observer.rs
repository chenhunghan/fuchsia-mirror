// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(ktest)]
use core::cell::Cell;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use fbl::DoublyLinkedListNode;
use object_constants_rs as object_constants;
use zx_types::{
    ZX_WAIT_ASYNC_BOOT_TIMESTAMP, ZX_WAIT_ASYNC_EDGE, ZX_WAIT_ASYNC_TIMESTAMP, zx_signals_t,
};

use crate::kernel::owned_wait_queue::OwnedWaitQueue;

use super::port_dispatcher::PortObserver;
use super::root_job_observer::RootJobObserver;
use super::wait_signal_observer::WaitSignalObserver;

/// The concrete observer type, used to statically dispatch the callbacks on [`SignalObserver`]
/// in place of a vtable. The value is packed into the top bits of `options`, so it must fit in
/// [`SignalObserver::TYPE_MASK`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum SignalObserverType {
    /// Stack-pinned waiter used by `sys_object_wait_one` and `sys_object_wait_many`.
    WaitSignal = 0,
    /// Asynchronous port observer used by `PortDispatcher`.
    Port = 1,
    /// Root job termination observer used by `RootJobObserver`.
    RootJob = 2,
    /// Only used by kernel unit tests (see [`CallbackSignalObserver`] below).
    #[cfg(ktest)]
    Callback = 3,
}

/// `SignalObserver` implementations may register to be called when a signal becomes active on a
/// particular `Dispatcher`.
///
/// Implementations must be thread compatible, but need not be thread safe.
///
/// `zircon/kernel/object/include/object/signal_observer.h` mirrors this layout so C++
/// `Dispatcher` can manage its intrusive `observers_` list directly.
#[derive(fbl::DoublyLinkedListContainable)]
#[repr(C, align(8))]
pub(super) struct SignalObserver {
    /// Intrusive list node managed by `Dispatcher::observers_` under `Dispatcher`'s lock.
    #[dll_node]
    node: DoublyLinkedListNode<SignalObserver>,
    /// Watched signal mask, guarded by `Dispatcher`'s lock.
    triggering_signals: UnsafeCell<zx_signals_t>,
    /// Packed [`SignalObserverType`] tag in the top two bits and per-observer options in the
    /// remaining bits. Wrapped in [`UnsafeCell`] so concrete observers embedding `SignalObserver`
    /// contain zero `Freeze` bytes in their base subobject.
    options: UnsafeCell<u32>,
    /// Watched handle identity, guarded by `Dispatcher`'s lock.
    handle: UnsafeCell<*const c_void>,
}

zr::static_assert_size_and_align!(
    SignalObserver,
    object_constants::kSignalObserverSize,
    object_constants::kSignalObserverAlign,
);
zr::static_assert!(
    core::mem::offset_of!(SignalObserver, node) == object_constants::kSignalObserverNodeOffset
);
zr::static_assert!(
    core::mem::offset_of!(SignalObserver, triggering_signals)
        == object_constants::kSignalObserverTriggeringSignalsOffset
);
zr::static_assert!(
    core::mem::offset_of!(SignalObserver, options)
        == object_constants::kSignalObserverOptionsOffset
);
zr::static_assert!(
    core::mem::offset_of!(SignalObserver, handle) == object_constants::kSignalObserverHandleOffset
);
zr::static_assert!(
    ((ZX_WAIT_ASYNC_TIMESTAMP | ZX_WAIT_ASYNC_EDGE | ZX_WAIT_ASYNC_BOOT_TIMESTAMP)
        & SignalObserver::TYPE_MASK)
        == 0
);

impl SignalObserver {
    const TYPE_SHIFT: u32 = 30;
    const TYPE_MASK: u32 = 0x3 << Self::TYPE_SHIFT;
    const OPTIONS_MASK: u32 = !Self::TYPE_MASK;

    /// Only concrete observer types may construct the base subobject: the callbacks below cast the
    /// `SignalObserver` pointer to the concrete observer type named by `observer_type`, so a
    /// standalone `SignalObserver` must not exist.
    #[inline]
    pub(super) const fn new(observer_type: SignalObserverType, options: u32) -> Self {
        debug_assert!(((observer_type as u32) & !(Self::TYPE_MASK >> Self::TYPE_SHIFT)) == 0);
        debug_assert!((options & Self::TYPE_MASK) == 0);
        let packed = ((observer_type as u32) << Self::TYPE_SHIFT) | (options & Self::OPTIONS_MASK);
        Self {
            node: DoublyLinkedListNode::new(),
            triggering_signals: UnsafeCell::new(0),
            options: UnsafeCell::new(packed),
            handle: UnsafeCell::new(core::ptr::null()),
        }
    }

    /// Unpacks the [`SignalObserverType`] encoded in `raw_options`.
    ///
    /// # Safety
    ///
    /// `raw_options` must have been read from an initialized [`SignalObserver`] constructed via
    /// [`Self::new`].
    #[inline]
    unsafe fn observer_type(raw_options: u32) -> SignalObserverType {
        match raw_options >> Self::TYPE_SHIFT {
            0 => SignalObserverType::WaitSignal,
            1 => SignalObserverType::Port,
            2 => SignalObserverType::RootJob,
            #[cfg(ktest)]
            3 => SignalObserverType::Callback,
            _ => {
                debug_assert!(false, "unexpected SignalObserver type");
                // SAFETY: `SignalObserver::new` only encodes valid `SignalObserverType`
                // discriminants in the top two bits of `options`, and the caller guarantees
                // `raw_options` comes from an initialized `SignalObserver`.
                unsafe { core::hint::unreachable_unchecked() }
            }
        }
    }

    /// Intended for use by concrete observer types like [`PortObserver`] that may require
    /// per-observer options.
    #[inline]
    fn unpack_options(raw_options: u32) -> u32 {
        raw_options & Self::OPTIONS_MASK
    }

    // The three callbacks below switch on the packed `SignalObserverType` and forward to the
    // concrete observer's handler. Concrete handlers deliberately do not reuse the base names on a
    // shared trait so they cannot be mistaken for dynamic overrides.

    /// Called when the set of active signals matches an expected set.
    ///
    /// At the time this is called, it is safe to delete this object: the caller will not interact
    /// with it again.
    ///
    /// WARNING: This is called under `Dispatcher`'s lock.
    ///
    /// # Safety
    ///
    /// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
    ///   concrete observer matching its packed [`SignalObserverType`], with provenance over the
    ///   entire concrete observer allocation.
    /// - The watched `Dispatcher`'s lock must be held by the caller, and `observer` must already
    ///   have been unlinked (or never linked) from `Dispatcher::observers_`.
    /// - Because concrete handlers (such as [`PortObserver::on_match`]) may destroy `observer`
    ///   before returning, `observer` must not be accessed after this call returns.
    #[inline]
    pub(super) unsafe fn on_match(
        observer: *mut Self,
        signals: zx_signals_t,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `observer` is valid at entry; copying `options` out before dispatching ensures
        // `observer` is never dereferenced after a self-deleting handler (such as
        // `PortObserver::on_match`) runs. Each arm casts `observer` to the offset-0 concrete type
        // identified by `observer_type` while preserving whole-allocation provenance, and the
        // caller holds the watched `Dispatcher`'s lock.
        unsafe {
            let raw_options = *(*observer).options.get();
            match Self::observer_type(raw_options) {
                SignalObserverType::WaitSignal => {
                    WaitSignalObserver::on_match_impl(observer.cast(), signals, queue_to_own);
                }
                SignalObserverType::Port => {
                    PortObserver::on_match(
                        observer.cast(),
                        Self::unpack_options(raw_options),
                        signals,
                        queue_to_own,
                    );
                }
                SignalObserverType::RootJob => {
                    core::hint::cold_path();
                    RootJobObserver::on_match_impl(observer, signals, queue_to_own);
                }
                #[cfg(ktest)]
                SignalObserverType::Callback => {
                    CallbackSignalObserver::on_match_impl(
                        observer.cast_const().cast(),
                        signals,
                        queue_to_own,
                    );
                }
            }
        }
    }

    /// Called when the registered handle (which refers to a handle to the `Dispatcher` object) is
    /// being destroyed/"closed"/transferred. (The object itself may also be destroyed shortly
    /// afterwards.)
    ///
    /// At the time this is called, it is safe to delete this object: the caller will not interact
    /// with it again.
    ///
    /// WARNING: This is called under `Dispatcher`'s lock.
    ///
    /// # Safety
    ///
    /// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
    ///   concrete observer matching its packed [`SignalObserverType`], with provenance over the
    ///   entire concrete observer allocation.
    /// - The watched `Dispatcher`'s lock must be held by the caller, and `observer` must already
    ///   have been unlinked from `Dispatcher::observers_`.
    /// - Because concrete handlers (such as [`PortObserver::on_cancel`]) may destroy `observer`
    ///   before returning, `observer` must not be accessed after this call returns.
    #[inline]
    pub(super) unsafe fn on_cancel(observer: *mut Self, signals: zx_signals_t) {
        // SAFETY: `observer` is valid at entry; copying `options` out before dispatching ensures
        // `observer` is never dereferenced after a self-deleting handler (such as
        // `PortObserver::on_cancel`) runs. Each arm casts `observer` to the offset-0 concrete type
        // identified by `observer_type` while preserving whole-allocation provenance, and the
        // caller holds the watched `Dispatcher`'s lock.
        unsafe {
            let raw_options = *(*observer).options.get();
            match Self::observer_type(raw_options) {
                SignalObserverType::WaitSignal => {
                    WaitSignalObserver::on_cancel_impl(observer.cast(), signals);
                }
                SignalObserverType::Port => {
                    PortObserver::on_cancel(observer.cast(), signals);
                }
                SignalObserverType::RootJob => {
                    core::hint::cold_path();
                    RootJobObserver::on_cancel_impl(observer, signals);
                }
                #[cfg(ktest)]
                SignalObserverType::Callback => {
                    CallbackSignalObserver::on_cancel_impl(observer.cast_const().cast(), signals);
                }
            }
        }
    }

    /// Determine if this observer matches the given `port` and `key`.
    ///
    /// If `true`, this object will be removed.
    ///
    /// WARNING: This is called under `Dispatcher`'s lock.
    ///
    /// # Safety
    ///
    /// - `observer` must point to the `SignalObserver` base subobject at offset 0 of a valid, live
    ///   concrete observer matching its packed [`SignalObserverType`], with provenance over the
    ///   entire concrete observer allocation.
    /// - The watched `Dispatcher`'s lock must be held by the caller.
    #[inline]
    pub(super) unsafe fn matches_key(observer: *const Self, port: *const c_void, key: u64) -> bool {
        // Only port observers carry a (port, key) identity; every other observer type never
        // matches.
        // SAFETY: `observer` is valid at entry, and each arm casts `observer` to the offset-0
        // concrete type identified by `observer_type` while preserving whole-allocation
        // provenance.
        unsafe {
            let raw_options = *(*observer).options.get();
            match Self::observer_type(raw_options) {
                SignalObserverType::Port => {
                    (*observer.cast::<PortObserver>()).matches_key(port, key)
                }
                #[cfg(ktest)]
                SignalObserverType::Callback => {
                    (*observer.cast::<CallbackSignalObserver>()).matches_key_impl(port, key)
                }
                SignalObserverType::WaitSignal | SignalObserverType::RootJob => false,
            }
        }
    }
}

/// A [`SignalObserver`] for kernel unit tests that forwards each callback to a function pointer
/// together with an opaque context (used by the `statetracker` suite).
#[cfg(ktest)]
#[repr(C)]
pub(super) struct CallbackSignalObserver {
    base: SignalObserver,
    ctx: Cell<*const c_void>,
    on_match: unsafe fn(*const c_void, zx_signals_t),
    on_cancel: unsafe fn(*const c_void, zx_signals_t),
    matches_key: unsafe fn(*const c_void, *const c_void, u64) -> bool,
}

#[cfg(ktest)]
zr::static_assert!(core::mem::offset_of!(CallbackSignalObserver, base) == 0);

#[cfg(ktest)]
impl CallbackSignalObserver {
    /// Creates a new `CallbackSignalObserver` with null context (updated via [`Self::set_ctx`]
    /// once pinned on the caller's stack).
    pub(super) fn new(
        on_match: unsafe fn(*const c_void, zx_signals_t),
        on_cancel: unsafe fn(*const c_void, zx_signals_t),
        matches_key: unsafe fn(*const c_void, *const c_void, u64) -> bool,
    ) -> Self {
        Self {
            base: SignalObserver::new(SignalObserverType::Callback, 0),
            ctx: Cell::new(core::ptr::null()),
            on_match,
            on_cancel,
            matches_key,
        }
    }

    /// Sets the opaque context pointer passed to the registered callbacks.
    pub(super) fn set_ctx(&self, ctx: *const c_void) {
        self.ctx.set(ctx);
    }

    /// Returns a pointer to the embedded [`SignalObserver`] base with provenance over the entire
    /// `CallbackSignalObserver`.
    pub(super) fn as_signal_observer_ptr(&self) -> *mut SignalObserver {
        // Every byte mutated through the returned pointer lives inside `UnsafeCell` (`base` and
        // `ctx`), while `on_match` / `on_cancel` / `matches_key` are only ever read through
        // `*const Self`.
        core::ptr::from_ref(self).cast::<SignalObserver>().cast_mut()
    }

    /// Reached through [`SignalObserver::on_match`] static dispatch.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live `CallbackSignalObserver` whose `ctx` satisfies the
    /// safety contract of its `on_match` callback.
    unsafe fn on_match_impl(
        this: *const Self,
        signals: zx_signals_t,
        _queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `this` points to a live `CallbackSignalObserver` and caller upholds the
        // callback's context invariant.
        unsafe {
            let observer = &*this;
            (observer.on_match)(observer.ctx.get(), signals);
        }
    }

    /// Reached through [`SignalObserver::on_cancel`] static dispatch.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live `CallbackSignalObserver` whose `ctx` satisfies the
    /// safety contract of its `on_cancel` callback.
    unsafe fn on_cancel_impl(this: *const Self, signals: zx_signals_t) {
        // SAFETY: `this` points to a live `CallbackSignalObserver` and caller upholds the
        // callback's context invariant.
        unsafe {
            let observer = &*this;
            (observer.on_cancel)(observer.ctx.get(), signals);
        }
    }

    /// Reached through [`SignalObserver::matches_key`] static dispatch.
    ///
    /// # Safety
    ///
    /// `self.ctx` must satisfy the safety contract of `self.matches_key`.
    unsafe fn matches_key_impl(&self, port: *const c_void, key: u64) -> bool {
        // SAFETY: Caller upholds the callback's context invariant.
        unsafe { (self.matches_key)(self.ctx.get(), port, key) }
    }
}
