// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use fbl::{Canary, RefPtr};
use ksync::LockToken;
use zx_status::Status;
use zx_types::zx_signals_t;

use crate::kernel::event::Event;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;

use super::dispatcher::Dispatcher;
use super::handle::HandleRef;
use super::handle_table::HandleTableLockClass;
use super::signal_observer::{SignalObserver, SignalObserverType};

/// Helper struct for waiting on the `object_wait_one` and `object_wait_many` syscalls.
///
/// Participates in the [`SignalObserver`] intrusive list on [`Dispatcher`]. Every field is
/// wrapped in [`UnsafeCell`] (or [`AtomicU32`]) so `WaitSignalObserver` contains zero `Freeze`
/// bytes while another CPU may concurrently run [`Self::on_match_impl`] or
/// [`Self::on_cancel_impl`] under `Dispatcher`'s lock.
#[repr(C, align(8))]
pub struct WaitSignalObserver<'e> {
    base: SignalObserver,
    canary: UnsafeCell<Canary<{ fbl::magic(b"WTSO") }>>,
    final_signal_state: AtomicU32,
    /// Safety invariant: Between [`Self::begin`] and [`Self::end`], points to a live, initialized
    /// [`Event`] outliving `'e`.
    event: UnsafeCell<*const Event>,
    dispatcher: UnsafeCell<Option<RefPtr<Dispatcher>>>,
    _event: PhantomData<&'e Event>,
}

zr::static_assert!(core::mem::offset_of!(WaitSignalObserver<'_>, base) == 0);

impl Default for WaitSignalObserver<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'e> WaitSignalObserver<'e> {
    /// Creates a new `WaitSignalObserver`.
    pub fn new() -> Self {
        Self {
            base: SignalObserver::new(SignalObserverType::WaitSignal, 0),
            canary: UnsafeCell::new(Canary::new()),
            final_signal_state: AtomicU32::new(0),
            event: UnsafeCell::new(core::ptr::null()),
            dispatcher: UnsafeCell::new(None),
            _event: PhantomData,
        }
    }

    #[inline]
    fn assert_canary(&self) {
        // SAFETY: `self.canary` is initialized in `new` and only mutated in `Canary::drop` after
        // `end` has synchronized with any concurrent callbacks.
        unsafe {
            (*self.canary.get()).assert();
        }
    }

    /// Returns a pointer to the embedded [`SignalObserver`] base with `SharedReadWrite` provenance
    /// over the entire `WaitSignalObserver` (every byte of `Self` is inside `UnsafeCell`).
    #[inline]
    fn as_signal_observer_ptr(&self) -> *mut SignalObserver {
        core::ptr::from_ref(self).cast::<SignalObserver>().cast_mut()
    }

    /// Begins observing signals on the given handle while holding the handle table lock.
    ///
    /// Calling this requires a `LockToken<'_, HandleTableLockClass>` to prove that the handle
    /// table lock is held. If this succeeds, [`Self::end`] must be called before dropping this
    /// observer.
    ///
    /// # Safety
    ///
    /// - `self` must not already be registered (any prior `Ok(())` call to [`Self::begin`] must
    ///   have been paired with [`Self::end`]).
    /// - The calling thread must not hold `handle.dispatcher()`'s lock.
    /// - If this call returns `Ok(())`, the caller must call [`Self::end`] without holding the
    ///   handle table lock or `handle.dispatcher()`'s lock before calling [`Self::begin`] again,
    ///   dropping `self`, or destroying `event`; while registered, the caller must not rely on
    ///   [`Drop`], leak `self`, or forget `self`.
    pub unsafe fn begin(
        self: Pin<&Self>,
        _token: &LockToken<'_, HandleTableLockClass>,
        event: &'e Event,
        handle: &HandleRef<'_>,
        watched_signals: zx_signals_t,
    ) -> Result<(), Status> {
        let self_ref = self.get_ref();
        self_ref.assert_canary();
        // Calling `begin` twice without an intervening `end` would re-insert an already-linked
        // `SignalObserver` node and race on `self_ref.event`.
        // SAFETY: `WaitSignalObserver` is `!Sync`, and only `begin`, `end`, and `Drop` on the
        // owning thread access `self_ref.dispatcher`.
        unsafe {
            debug_assert!((*self_ref.dispatcher.get()).is_none());
        }

        // Store `event` before calling `add_observer`: `add_observer` synchronously invokes
        // `on_match_impl` if one of `watched_signals` is already active.
        // SAFETY: `self` is not currently registered with any `Dispatcher`, so no concurrent
        // callback can read `self_ref.event`.
        unsafe {
            *self_ref.event.get() = core::ptr::from_ref(event);
        }
        let dispatcher = handle.as_handle().dispatcher();
        let base_ptr = self_ref.as_signal_observer_ptr();

        // Wait for one of `watched_signals` to become active.
        //
        // Note that `watched_signals` may be 0, in which case we won't receive
        // a callback, but will remain queued until `end` is called.
        // SAFETY: `self` is pinned with zero `Freeze` bytes and is not mutably aliased until
        // `Drop` (which the caller runs only after `end`), `base_ptr` points to the
        // `SignalObserver` base at offset 0 with provenance over the entire `WaitSignalObserver`,
        // the caller guarantees `self` is not already registered (checked by `debug_assert!`
        // above), `self_ref.event` points to `event` which outlives registration per the caller's
        // contract, `dispatcher` is kept alive in `self_ref.dispatcher` until `end` unregisters
        // `self`, and the caller guarantees `dispatcher`'s lock is not held.
        unsafe {
            dispatcher.add_observer(base_ptr, handle.as_ptr().cast(), watched_signals)?;
        }

        // SAFETY: `WaitSignalObserver` is `!Sync`, only `begin`, `end`, and `Drop` on the owning
        // thread access `self_ref.dispatcher`, and `*self_ref.dispatcher.get()` is guaranteed to
        // be `None` at entry (per caller contract and `debug_assert!` above), so overwriting via
        // `ptr::write` avoids dead `RefPtr::drop` glue.
        unsafe {
            self_ref.dispatcher.get().write(Some(dispatcher.clone()));
        }
        Ok(())
    }

    /// Ends observing signals and returns the final observed signals.
    ///
    /// This should *not* be called under the handle table lock.
    pub fn end(self: Pin<&Self>) -> zx_signals_t {
        let self_ref = self.get_ref();
        self_ref.assert_canary();
        // SAFETY: `WaitSignalObserver` is `!Sync`, and only `begin`, `end`, and `Drop` on the
        // owning thread access `self_ref.dispatcher`.
        let dispatcher =
            unsafe { (*self_ref.dispatcher.get()).take() }.expect("dispatcher must be present");

        let base_ptr = self_ref.as_signal_observer_ptr();
        let mut signals: zx_signals_t = 0;
        // SAFETY: `base_ptr` points to the `SignalObserver` base at offset 0 of pinned `self` with
        // provenance over the entire `WaitSignalObserver`, `self` has zero `Freeze` bytes and is
        // not mutably aliased during `end`, `self` was only registered with `dispatcher` in
        // `begin` (never another `Dispatcher`), and `dispatcher`'s lock is not held per `begin`'s
        // caller contract. `remove_observer` acquires `dispatcher`'s lock, synchronizing with any
        // concurrent `on_match_impl` or `on_cancel_impl` callback.
        let was_removed = unsafe { dispatcher.remove_observer(base_ptr, &mut signals) };

        // If `was_removed` is false, it means a callback was fired and `final_signal_state` has a
        // value. `Acquire` pairs with the `Release` store in `on_match_impl` / `on_cancel_impl`.
        if !was_removed {
            return self_ref.final_signal_state.load(Ordering::Acquire);
        }

        // Otherwise, return the set of signals at the point of removal.
        signals
    }

    /// Callback when watched signals match (`SignalObserver::on_match`).
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, live `WaitSignalObserver` whose `event` points to a live,
    ///   initialized `Event` that remains valid and unaliased by mutable references for the
    ///   duration of this call.
    /// - The watched `Dispatcher`'s lock must be held by the caller.
    #[inline]
    pub(super) unsafe fn on_match_impl(
        this: *mut Self,
        signals: zx_signals_t,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `this` points to a live `WaitSignalObserver` under the watched `Dispatcher`'s
        // lock, and `canary`, `final_signal_state`, and `event` remain valid for the duration of
        // the callback (the waiting thread cannot complete `end` or destroy `event` until the
        // watched `Dispatcher`'s lock is released).
        unsafe {
            (*(*this).canary.get()).assert();
            // Save the signal state, and wake our waiter. `Release` pairs with the `Acquire` load
            // in `end` (in addition to `Dispatcher`'s lock synchronization in `remove_observer`).
            (*this).final_signal_state.store(signals, Ordering::Release);
            let event = &**(*this).event.get();
            event.signal_etc(Ok(()), queue_to_own);
        }
    }

    /// Callback when handle is cancelled or closed (`SignalObserver::on_cancel`).
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, live `WaitSignalObserver` whose `event` points to a live,
    ///   initialized `Event` that remains valid and unaliased by mutable references for the
    ///   duration of this call.
    /// - The watched `Dispatcher`'s lock must be held by the caller.
    #[inline]
    pub(super) unsafe fn on_cancel_impl(this: *mut Self, signals: zx_signals_t) {
        // SAFETY: `this` points to a live `WaitSignalObserver` under the watched `Dispatcher`'s
        // lock, and `canary`, `final_signal_state`, and `event` remain valid for the duration of
        // the callback (the waiting thread cannot complete `end` or destroy `event` until the
        // watched `Dispatcher`'s lock is released).
        unsafe {
            (*(*this).canary.get()).assert();
            // Save the signal state, and wake our waiter. `Release` pairs with the `Acquire` load
            // in `end` (in addition to `Dispatcher`'s lock synchronization in `remove_observer`).
            (*this)
                .final_signal_state
                .store(signals | zx_types::ZX_SIGNAL_HANDLE_CLOSED, Ordering::Release);
            let event = &**(*this).event.get();
            event.signal_result(Err(Status::CANCELED));
        }
    }
}

impl Drop for WaitSignalObserver<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.dispatcher.get_mut().is_none(),
            "WaitSignalObserver dropped without end()",
        );
        // SAFETY: `self.dispatcher` is guaranteed to be `None` on drop (`end()` takes it);
        // writing `None` without dropping lets LLVM elide dead `RefPtr<Dispatcher>` drop glue.
        unsafe {
            core::ptr::write(self.dispatcher.get_mut(), None);
        }
    }
}
