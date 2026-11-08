// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher_ffi::{
    cpp_dispatcher_add_observer, cpp_dispatcher_cancel, cpp_dispatcher_cancel_by_key,
    cpp_dispatcher_clear_signals, cpp_dispatcher_current_handle_count,
    cpp_dispatcher_decrement_handle_count, cpp_dispatcher_get_name, cpp_dispatcher_get_ref_counted,
    cpp_dispatcher_get_related_koid, cpp_dispatcher_get_type,
    cpp_dispatcher_increment_handle_count, cpp_dispatcher_is_waitable,
    cpp_dispatcher_make_port_observer, cpp_dispatcher_notify_observers_locked,
    cpp_dispatcher_on_zero_handles, cpp_dispatcher_poll_signals,
    cpp_dispatcher_raise_signals_locked, cpp_dispatcher_recycle, cpp_dispatcher_remove_observer,
    cpp_dispatcher_set_name, cpp_dispatcher_signals_state_locked, cpp_dispatcher_update_state,
    cpp_dispatcher_update_state_locked, cpp_dispatcher_user_signal_peer,
    cpp_dispatcher_user_signal_self,
};
use super::handle::HandleRef;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::ksync_kernel::TrackedCriticalMutex;
use core::convert::Infallible;
use core::marker::PhantomData;
use fbl::{Recyclable, RefPtr, pin_make_ref_counted, ref_counted};
use kalloc::AllocError;
use ksync::{KMutex, LockToken, guarded};
use relaxed_atomic::RelaxedAtomicU64;
use zx_status::Status;
use zx_types::zx_rights_t;

/// Common trait defining ops for kernel Dispatcher facades.
pub trait DispatcherOps {
    type LockClass;
    const TYPE: zx_types::zx_obj_type_t;

    fn dispatcher(&self) -> *const Dispatcher;

    #[inline]
    fn current_handle_count(&self) -> u32 {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe { cpp_dispatcher_current_handle_count(self.dispatcher()) }
    }

    #[inline]
    fn on_zero_handles(&self) {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe {
            cpp_dispatcher_on_zero_handles(self.dispatcher());
        }
    }

    #[inline]
    fn clear_signals(&self, signals: zx_types::zx_signals_t) {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe {
            cpp_dispatcher_clear_signals(self.dispatcher(), signals);
        }
    }

    #[inline]
    fn update_state(&self, clear_mask: u32, set_mask: u32) {
        self.update_state_with_strobe(clear_mask, set_mask, 0);
    }

    #[inline]
    fn update_state_with_strobe(&self, clear_mask: u32, set_mask: u32, strobe_mask: u32) {
        // SAFETY: self.dispatcher() returns a valid pointer to an initialized Dispatcher.
        unsafe {
            cpp_dispatcher_update_state(self.dispatcher(), clear_mask, set_mask, strobe_mask);
        }
    }

    #[inline]
    fn update_state_locked(
        &self,
        token: &LockToken<'_, Self::LockClass>,
        clear_mask: u32,
        set_mask: u32,
    ) {
        self.update_state_with_strobe_locked(token, clear_mask, set_mask, 0);
    }

    #[inline]
    fn update_state_with_strobe_locked(
        &self,
        _token: &LockToken<'_, Self::LockClass>,
        clear_mask: u32,
        set_mask: u32,
        strobe_mask: u32,
    ) {
        // SAFETY: self.dispatcher() is valid, and the proof token guarantees the state lock is
        // held.
        unsafe {
            cpp_dispatcher_update_state_locked(
                self.dispatcher(),
                clear_mask,
                set_mask,
                strobe_mask,
            );
        }
    }

    #[inline]
    fn raise_signals_locked(
        &self,
        _token: &LockToken<'_, Self::LockClass>,
        signals: zx_types::zx_signals_t,
    ) -> zx_types::zx_signals_t {
        // SAFETY: self.dispatcher() is valid, and the proof token guarantees the state lock is
        // held.
        unsafe { cpp_dispatcher_raise_signals_locked(self.dispatcher(), signals) }
    }

    #[inline]
    fn notify_observers_locked_with_queue(
        &self,
        _token: &LockToken<'_, Self::LockClass>,
        signals: zx_types::zx_signals_t,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        let queue_ptr = queue_to_own.map_or(core::ptr::null_mut(), OwnedWaitQueue::as_ptr);
        // SAFETY: self.dispatcher() is valid, queue_ptr is null or a valid OwnedWaitQueue, and
        // the proof token guarantees the state lock is held.
        unsafe {
            cpp_dispatcher_notify_observers_locked(self.dispatcher(), signals, queue_ptr);
        }
    }

    #[inline]
    fn signals_state_locked(
        &self,
        _token: &LockToken<'_, Self::LockClass>,
    ) -> zx_types::zx_signals_t {
        // SAFETY: self.dispatcher() is valid, and the proof token guarantees the state lock is
        // held.
        unsafe { cpp_dispatcher_signals_state_locked(self.dispatcher()) }
    }
}

/// Helper macro to declare facade structs and implement common facade traits for Dispatcher
/// subtypes.
macro_rules! impl_dispatcher_facade {
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $obj_type:expr) => {
        $crate::object::dispatcher::impl_dispatcher_facade!($(#[$meta])* $vis struct $type, $obj_type, ());
    };
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $obj_type:expr, $lock_class:ty) => {
        $(#[$meta])*
        #[repr(C, align(8))]
        $vis struct $type {
            _facade: fbl::OpaqueRefCountedFacade<$crate::object::Dispatcher>,
        }

        impl core::ops::Deref for $type {
            type Target = $crate::object::Dispatcher;
            #[inline]
            fn deref(&self) -> &Self::Target {
                // SAFETY: `self` is a valid facade reference, and the base `Dispatcher`
                // is part of the same allocation.
                unsafe { &*<Self as $crate::object::DispatcherOps>::dispatcher(self) }
            }
        }

        // SAFETY: `$type` is a `#[repr(C)]` facade struct that starts with `Dispatcher`
        // at offset 0 and is layout-compatible with `Dispatcher`.
        unsafe impl fbl::IsOpaqueRefCounted for $type {
            type TargetBase = $crate::object::Dispatcher;
        }

        impl $crate::object::DispatcherOps for $type {
            const TYPE: zx_types::zx_obj_type_t = $obj_type;
            type LockClass = $lock_class;

            #[inline]
            fn dispatcher(&self) -> *const $crate::object::Dispatcher {
                self as *const Self as *const $crate::object::Dispatcher
            }
        }
    };
}
pub(crate) use impl_dispatcher_facade;

/// Helper macro to declare facade structs and implement common facade traits and state access
/// methods for Dispatcher subtypes with state.
macro_rules! impl_dispatcher_facade_with_state {
    (
        @base
        $(#[$meta:meta])* $vis:vis struct $type:ident,
        $state:ident,
        $obj_type:expr,
        $offset_const:expr,
        $lock_class:ty,
        $get_lock:expr
    ) => {
        paste::paste! {
            $crate::object::dispatcher::impl_dispatcher_facade!($(#[$meta])* $vis struct $type, $obj_type, $lock_class);

            impl $type {
                /// Returns a reference to the underlying state object.
                #[inline]
                pub fn state(&self) -> &$state {
                    // SAFETY: The state object is located at a verified offset within the
                    // same allocation as the facade.
                    unsafe {
                        &*(self as *const Self)
                            .wrapping_byte_add($offset_const as usize)
                            .cast::<$state>()
                    }
                }
            }

            /// Returns a pointer to the mutex inside `$state`.
            ///
            /// # Safety
            ///
            /// `ptr` must point to a valid, initialized `$state`.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_get_lock>](
                ptr: *const $state,
            ) -> *mut ksync::KMutex<$lock_class, $crate::ksync_kernel::TrackedCriticalMutex> {
                let lock_ref: &ksync::KMutex<
                    $lock_class,
                    $crate::ksync_kernel::TrackedCriticalMutex,
                > = ($get_lock)(ptr);
                zr::ToMutPtr::to_mut_ptr(lock_ref)
            }

            /// Destroys a `$state` in-place.
            ///
            /// # Safety
            ///
            /// The caller must ensure `state` is a valid reference to an initialized `$state`, and
            /// must not use the state (or the enclosing dispatcher) after this function returns.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_destroy>](
                state: &mut $state,
            ) {
                // SAFETY: The caller is destroying the dispatcher and will not use it again.
                unsafe {
                    core::ptr::drop_in_place(state);
                }
            }
        }
    };
    ($(#[$meta:meta])* $vis:vis struct $type:ident, $state:ident, $obj_type:expr, $offset_const:expr) => {
        paste::paste! {
            $crate::object::dispatcher::impl_dispatcher_facade_with_state!(
                @base
                $(#[$meta])* $vis struct $type,
                $state,
                $obj_type,
                $offset_const,
                [<$state LockClass>],
                // SAFETY: The caller of `rust_<type>_state_get_lock` guarantees `ptr` points
                // to a valid, initialized `$state`.
                |ptr: *const $state| unsafe { &(*ptr).lock }
            );
        }
    };
}
pub(crate) use impl_dispatcher_facade_with_state;

/// Helper macro to declare facade structs and implement common facade traits, state access methods,
/// and peered dispatcher operations (init_peer, get_related_koid, user_signal_self,
/// user_signal_peer, on_zero_handles) for PeeredDispatcher subtypes with state.
macro_rules! impl_peered_dispatcher_facade_with_state {
    (
        $(#[$meta:meta])* $vis:vis struct $type:ident,
        $state:ident,
        $obj_type:expr,
        $offset_const:expr,
        allowed_signals: $allowed_signals:expr $(,)?
    ) => {
        $crate::object::dispatcher::impl_dispatcher_facade_with_state!(
            @base
            $(#[$meta])* $vis struct $type,
            $state,
            $obj_type,
            $offset_const,
            $crate::object::dispatcher::PeerHolderMuClass<$type>,
            // SAFETY: The caller of `rust_<type>_state_get_lock` guarantees `ptr` points
            // to a valid, initialized `$state`.
            |ptr: *const $state| unsafe { &(&(*ptr).peered.holder).mu }
        );

        impl $type {
            /// Initializes the peer reference and peer KOID.
            pub(crate) fn init_peer(&self, peer: fbl::RefPtr<Self>) {
                let peer_koid = peer.get_koid();
                ksync::lock!(let mut guard = self.state().peered.lock());
                *guard.as_mut().peer_mut() = Some(peer);
                self.state().peered.set_peer_koid(peer_koid);
            }

            /// Returns the related KOID of the peer dispatcher.
            #[inline]
            pub fn get_related_koid(&self) -> zx_types::zx_koid_t {
                self.state().peered.peer_koid()
            }

            /// Signals this dispatcher endpoint.
            pub fn user_signal_self(
                &self,
                clear_mask: u32,
                set_mask: u32,
            ) -> Result<(), zx_status::Status> {
                if (set_mask & !$allowed_signals) != 0 || (clear_mask & !$allowed_signals) != 0 {
                    return Err(zx_status::Status::INVALID_ARGS);
                }
                ksync::lock!(let guard = self.state().peered.lock());
                self.update_state_locked(guard.token(), clear_mask, set_mask);
                Ok(())
            }

            /// Signals the peer dispatcher endpoint.
            pub fn user_signal_peer(
                &self,
                clear_mask: u32,
                set_mask: u32,
            ) -> Result<(), zx_status::Status> {
                if (set_mask & !$allowed_signals) != 0 || (clear_mask & !$allowed_signals) != 0 {
                    return Err(zx_status::Status::INVALID_ARGS);
                }
                ksync::lock!(let guard = self.state().peered.lock());
                let peer = guard.peer().as_ref().ok_or(zx_status::Status::PEER_CLOSED)?;
                peer.update_state_locked(guard.token(), clear_mask, set_mask);
                Ok(())
            }

            /// Handles zero handles condition by clearing the peer reference and calling
            /// `on_zero_handles_locked` on self and `on_peer_zero_handles_locked` on peer.
            pub fn on_zero_handles(&self) {
                ksync::lock!(let mut guard = self.state().peered.lock());
                let peer = guard.as_mut().peer_mut().take();
                self.on_zero_handles_locked(guard.as_mut().token_mut());

                if let Some(peer) = peer {
                    // SAFETY: `peer` is the paired endpoint sharing the same `PeerHolder` mutex
                    // held by `guard`.
                    unsafe {
                        *peer.state().peered.guard_mu_mut(guard.as_mut().token_mut()).peer_mut() =
                            None;
                    }
                    peer.on_peer_zero_handles_locked(guard.as_mut().token_mut());
                }
            }
        }
    };
}
pub(crate) use impl_peered_dispatcher_facade_with_state;

/// Helper macro to generate standard `rust_<type>_state_init` and peered FFI trampolines.
macro_rules! impl_peered_dispatcher_state_init {
    ($type:ident, $state:ident $(, $arg:ident : $arg_ty:ty)* $(,)?) => {
        paste::paste! {
            /// Initializes a `$state` in-place using `$state::init(holder, ...)`.
            ///
            /// # Safety
            ///
            /// `ptr` must point to uninitialized memory of at least `size_of::<$state>()` bytes.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_init>](
                ptr: *mut $state,
                holder: fbl::RefPtr<$crate::object::dispatcher::PeerHolder<$type>>,
                $( $arg : $arg_ty ),*
            ) {
                // SAFETY: `ptr` points to uninitialized memory allocated for `$state`.
                unsafe {
                    let _ = pin_init::PinInit::__pinned_init(
                        $state::init(holder, $( $arg ),*),
                        ptr,
                    );
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _get_related_koid>](
                dispatcher: &$type,
            ) -> zx_types::zx_koid_t {
                dispatcher.get_related_koid()
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _user_signal_self>](
                dispatcher: &$type,
                clear_mask: u32,
                set_mask: u32,
            ) -> zx_types::zx_status_t {
                zx_status::Status::result_into_raw(dispatcher.user_signal_self(clear_mask, set_mask))
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _user_signal_peer>](
                dispatcher: &$type,
                clear_mask: u32,
                set_mask: u32,
            ) -> zx_types::zx_status_t {
                zx_status::Status::result_into_raw(dispatcher.user_signal_peer(clear_mask, set_mask))
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _on_zero_handles>](
                dispatcher: &$type,
            ) {
                dispatcher.on_zero_handles();
            }
        }
    };
}
pub(crate) use impl_peered_dispatcher_state_init;

/// Helper macro to generate standard `rust_<type>_state_init` FFI trampolines.
macro_rules! impl_dispatcher_state_init {
    ($type:ident, $state:ident $(, $arg:ident : $arg_ty:ty)* $(,)?) => {
        paste::paste! {
            /// Initializes a `$state` in-place using `$state::init(dispatcher, ...)`.
            ///
            /// # Safety
            ///
            /// `ptr` must point to uninitialized memory of at least `size_of::<$state>()` bytes,
            /// and `dispatcher` must point to the enclosing `$type`.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn [<rust_ $type:snake _state_init>](
                ptr: *mut $state,
                dispatcher: *const $type,
                $( $arg : $arg_ty ),*
            ) {
                // SAFETY: `ptr` points to uninitialized memory allocated for `$state`.
                unsafe {
                    let _ = pin_init::PinInit::__pinned_init(
                        $state::init(dispatcher, $( $arg ),*),
                        ptr,
                    );
                }
            }
        }
    };
}
pub(crate) use impl_dispatcher_state_init;

fbl::impl_opaque_ref_counted_facade!(
    /// Base facade type for kernel Dispatchers.
    #[repr(align(8))]
    pub struct Dispatcher,
    cpp_dispatcher_recycle,
    cpp_dispatcher_get_ref_counted,
);

impl Dispatcher {
    /// Returns the ZX object type of this Dispatcher.
    #[inline]
    pub fn get_type(&self) -> zx_types::zx_obj_type_t {
        // SAFETY: self is a valid reference to an initialized Dispatcher.
        unsafe { cpp_dispatcher_get_type(self) }
    }

    /// Returns the kernel object ID (KOID) of this Dispatcher.
    #[inline]
    pub fn get_koid(&self) -> zx_types::zx_koid_t {
        // SAFETY: self is a valid reference to an initialized Dispatcher.
        unsafe { super::dispatcher_ffi::cpp_dispatcher_get_koid(self) }
    }

    /// Returns the related koid of this Dispatcher.
    #[inline]
    pub fn get_related_koid(&self) -> zx_types::zx_koid_t {
        // SAFETY: self is a valid reference to an initialized Dispatcher.
        unsafe { cpp_dispatcher_get_related_koid(self) }
    }

    /// Adds an observer to this dispatcher.
    ///
    /// # Safety
    ///
    /// `observer` must point to a valid `SignalObserver`. `handle` must be a valid handle pointer
    /// or null.
    pub unsafe fn add_observer(
        &self,
        observer: *mut core::ffi::c_void,
        handle: *const core::ffi::c_void,
        signals: zx_types::zx_signals_t,
    ) -> Result<(), Status> {
        let status = unsafe { cpp_dispatcher_add_observer(self, observer, handle, signals) };
        Status::ok(status)
    }

    /// Removes an observer from this dispatcher.
    ///
    /// # Safety
    ///
    /// `observer` must point to a valid `SignalObserver`.
    pub unsafe fn remove_observer(
        &self,
        observer: *mut core::ffi::c_void,
        out_signals: &mut zx_types::zx_signals_t,
    ) -> bool {
        unsafe { cpp_dispatcher_remove_observer(self, observer, out_signals) }
    }

    /// Safely downcasts a `&Dispatcher` reference to a specific facade reference `&T` if the
    /// dispatcher types match.
    #[inline]
    pub fn downcast<T: DispatcherOps>(&self) -> Option<&T> {
        if T::TYPE == zx_types::ZX_OBJ_TYPE_NONE || self.get_type() == T::TYPE {
            // SAFETY: `T` implements `DispatcherOps` and its `TYPE` matches `self.get_type()`.
            // All facade types (`ThreadDispatcher`, `ProcessDispatcher`, etc.) are `#[repr(C)]`
            // layout-compatible with `Dispatcher`.
            unsafe { Some(&*(self as *const Self as *const T)) }
        } else {
            None
        }
    }

    /// Gets the name of the dispatcher.
    pub fn get_name(&self, out_name: &mut [u8; zx_types::ZX_MAX_NAME_LEN]) -> Result<(), Status> {
        // SAFETY: self is a valid Dispatcher and out_name points to a valid buffer of ZX_MAX_NAME_LEN bytes.
        let status = unsafe { cpp_dispatcher_get_name(self, out_name.as_mut_ptr().cast()) };
        Status::ok(status)
    }

    /// Sets the name of the dispatcher.
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        // SAFETY: self is a valid Dispatcher and name points to name.len() bytes of valid memory.
        let status = unsafe { cpp_dispatcher_set_name(self, name.as_ptr().cast(), name.len()) };
        Status::ok(status)
    }

    /// Returns the current number of handles pointing to this dispatcher.
    pub fn current_handle_count(&self) -> u32 {
        // SAFETY: self is a valid Dispatcher reference.
        unsafe { cpp_dispatcher_current_handle_count(self) }
    }

    /// Increments the handle count pointing to this dispatcher.
    pub fn increment_handle_count(&self) {
        // SAFETY: self is a valid Dispatcher reference.
        unsafe { cpp_dispatcher_increment_handle_count(self) };
    }

    /// Decrements the handle count pointing to this dispatcher. Returns true if count reached zero.
    pub fn decrement_handle_count(&self) -> bool {
        // SAFETY: self is a valid Dispatcher reference.
        unsafe { cpp_dispatcher_decrement_handle_count(self) }
    }

    /// Returns whether this dispatcher supports waiting on signals.
    #[inline]
    pub fn is_waitable(&self) -> bool {
        // SAFETY: self is a valid Dispatcher reference.
        unsafe { cpp_dispatcher_is_waitable(self) }
    }

    /// Cancels observers waiting on `handle` for this dispatcher.
    ///
    /// May only be called when `self.is_waitable()` is true.
    #[inline]
    pub fn cancel(&self, handle: HandleRef<'_>) {
        // SAFETY: self is a valid Dispatcher reference and handle is a valid HandleRef.
        unsafe { cpp_dispatcher_cancel(self, handle.as_ptr().cast()) }
    }

    /// Cancels wait operations on this dispatcher for the given handle pointer.
    pub fn cancel_raw(&self, handle: *const core::ffi::c_void) {
        // SAFETY: self is a valid Dispatcher reference.
        unsafe { cpp_dispatcher_cancel(self, handle) };
    }

    /// Like [`Self::cancel`], but issued via `zx_port_cancel()`.
    ///
    /// Returns `true` if an observer was canceled.
    ///
    /// # Panics
    ///
    /// Panics if `self.is_waitable()` is false.
    #[inline]
    pub fn cancel_by_key(
        &self,
        handle: HandleRef<'_>,
        port: &super::port_dispatcher::PortDispatcher,
        key: u64,
    ) -> bool {
        assert!(self.is_waitable());
        // SAFETY: `self` is a valid `Dispatcher` reference, `handle` is a valid `HandleRef`, and
        // `port` is a valid `PortDispatcher` reference.
        unsafe { cpp_dispatcher_cancel_by_key(self, handle.as_ptr().cast(), port, key) }
    }

    /// Signals this dispatcher directly from user space (`zx_object_signal`).
    #[inline]
    pub fn user_signal_self(&self, clear_mask: u32, set_mask: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Dispatcher` reference.
        let status = unsafe { cpp_dispatcher_user_signal_self(self, clear_mask, set_mask) };
        Status::ok(status)
    }

    /// Signals this dispatcher's peer from user space (`zx_object_signal_peer`).
    #[inline]
    pub fn user_signal_peer(&self, clear_mask: u32, set_mask: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Dispatcher` reference.
        let status = unsafe { cpp_dispatcher_user_signal_peer(self, clear_mask, set_mask) };
        Status::ok(status)
    }

    /// Makes a port observer which will be triggered when any `signals` are asserted or canceled
    /// when `key` is canceled or `handle` is closed.
    ///
    /// `handle` must be borrowed while holding a read lock on the handle table containing it.
    ///
    /// If `options` does not contain `ZX_WAIT_ASYNC_EDGE`, the signal state is checked on entry
    /// and the observer may be triggered immediately.
    #[inline]
    pub fn make_port_observer(
        &self,
        options: u32,
        handle: HandleRef<'_>,
        signals: zx_types::zx_signals_t,
        key: u64,
        port: &super::port_dispatcher::PortDispatcher,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Dispatcher` reference, `handle` is a valid `HandleRef`
        // protected by the handle table lock, and `port` is a valid `PortDispatcher` reference.
        let status = unsafe {
            cpp_dispatcher_make_port_observer(
                self,
                options,
                handle.as_ptr().cast(),
                signals,
                key,
                port,
            )
        };
        Status::ok(status)
    }

    /// Returns the basic handle information for this dispatcher.
    pub fn get_handle_info(&self, rights: zx_rights_t) -> zx_types::zx_info_handle_basic_t {
        zx_types::zx_info_handle_basic_t {
            koid: self.get_koid(),
            rights,
            type_: self.get_type(),
            related_koid: self.get_related_koid(),
            ..Default::default()
        }
    }

    /// Polls the currently active signals on this dispatcher.
    ///
    /// Might be raced with other threads modifying the signals, so only use when checking for
    /// signals that once set, aren't unset, or when synchronization is done by other means.
    #[inline]
    pub fn poll_signals(&self) -> zx_types::zx_signals_t {
        // SAFETY: `self` is a valid `Dispatcher` reference.
        unsafe { cpp_dispatcher_poll_signals(self) }
    }

    /// Cancels observers matching `handle`, `port`, and `key`.
    #[cfg(ktest)]
    #[inline]
    pub fn cancel_by_key_raw(
        &self,
        handle: *const core::ffi::c_void,
        port: *const core::ffi::c_void,
        key: u64,
    ) -> bool {
        assert!(self.is_waitable());
        // SAFETY: `self` is a valid `Dispatcher` reference, and `cpp_dispatcher_cancel_by_key`
        // only compares `handle` and `port` by address without dereferencing them.
        unsafe { cpp_dispatcher_cancel_by_key(self, handle, port.cast(), key) }
    }
}

impl DispatcherOps for Dispatcher {
    type LockClass = ();
    const TYPE: zx_types::zx_obj_type_t = zx_types::ZX_OBJ_TYPE_NONE;

    fn dispatcher(&self) -> *const Dispatcher {
        self
    }
}

/// Peered dispatchers have opposing endpoints to coordinate state with. For example, writing into
/// one endpoint of a Channel needs to modify `zx_signals_t` state (for the readability bit) on the
/// opposite side. To coordinate their state, they share a mutex, which is held by the
/// `PeerHolder`. Both endpoints have a `RefPtr` back to the `PeerHolder`; no one else ever does.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[repr(C)]
pub struct PeerHolder<Endpoint> {
    #[mutex]
    pub mu: KMutex<TrackedCriticalMutex>,
    _phantom: PhantomData<Endpoint>,
}

impl<Endpoint> PeerHolder<Endpoint> {
    /// Creates a new `PeerHolder`.
    pub fn create() -> Result<RefPtr<Self>, AllocError> {
        pin_make_ref_counted!(Self {
            mu <- KMutex::init(),
            _phantom: PhantomData,
        })
    }
}

/// State shared by peered dispatcher subtypes.
///
/// Peered dispatchers have opposing endpoints to coordinate state with (such as peer KOID, the peer
/// reference, and the shared `PeerHolder` mutex).
#[guarded]
#[repr(C)]
pub struct PeeredState<T: fbl::IsOpaqueRefCounted> {
    peer_koid: RelaxedAtomicU64,
    pub holder: RefPtr<PeerHolder<T>>,
    #[guarded_by(mu)]
    pub peer: Option<RefPtr<T>>,
    #[mutex(PeerHolderMuClass<T>)]
    pub mu: KMutex<ksync::PhantomMutex>,
}

impl<T: fbl::IsOpaqueRefCounted> PeeredState<T> {
    /// Initializes a `PeeredState` with the given `PeerHolder`.
    pub fn init(holder: RefPtr<PeerHolder<T>>) -> impl pin_init::PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            peer_koid: RelaxedAtomicU64::new(0),
            holder,
            peer: ksync::KCell::new(None),
            mu: KMutex::new(ksync::PhantomMutex),
        })
    }

    /// Returns the peer KOID as `zx_koid_t`, or `0` if not yet initialized.
    #[inline]
    pub fn peer_koid(&self) -> zx_types::zx_koid_t {
        self.peer_koid.load()
    }

    /// Sets the peer KOID.
    #[inline]
    pub fn set_peer_koid(&self, koid: zx_types::zx_koid_t) {
        self.peer_koid.store(koid);
    }

    /// Locks the underlying shared `PeerHolder` mutex (`self.holder.mu`) and returns a guard for
    /// accessing fields protected by `self.mu`.
    #[inline]
    pub fn lock(
        &self,
    ) -> impl pin_init::PinInit<PeeredStateMuGuard<'_, T, TrackedCriticalMutex>, Infallible> {
        // SAFETY: `self.holder` is the immutable `PeerHolder` whose `mu` synchronizes `self`.
        unsafe { self.lock_mu(&self.holder.mu) }
    }
}

/// Dispatcher state tracker unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "statetracker")]
mod tests {
    use super::{Dispatcher, DispatcherOps};
    use crate::object::EventDispatcher;
    use crate::object::dispatcher_ffi::{
        cpp_callback_signal_observer_destroy, cpp_callback_signal_observer_init,
    };
    use core::cell::Cell;
    use core::ffi::c_void;
    use core::marker::PhantomPinned;
    use core::ptr;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true, unwrap_ok};
    use zr::OpaqueBytes;
    use zx_types::{ZX_SIGNAL_NONE, zx_signals_t};

    const TEST_SIGNAL1: zx_signals_t = 1;
    const TEST_SIGNAL2: zx_signals_t = 2;

    #[repr(align(8))]
    struct ObserverStorage(OpaqueBytes<80>);

    struct TestSignalObserverState {
        signals: Cell<zx_signals_t>,
        cancel_called: Cell<bool>,
        match_called: Cell<bool>,
        port: *const c_void,
        key: u64,
    }

    struct TestSignalObserver<'a> {
        storage: ObserverStorage,
        observer_ptr: Cell<*mut c_void>,
        dispatcher: &'a Dispatcher,
        state: TestSignalObserverState,
        _pin: PhantomPinned,
    }

    impl<'a> TestSignalObserver<'a> {
        fn new_in_place(dispatcher: &'a Dispatcher) -> Self {
            Self::new_with_key(dispatcher, ptr::null(), 0)
        }

        fn new_with_key(dispatcher: &'a Dispatcher, port: *const c_void, key: u64) -> Self {
            Self {
                storage: ObserverStorage(OpaqueBytes::uninit()),
                observer_ptr: Cell::new(ptr::null_mut()),
                dispatcher,
                state: TestSignalObserverState {
                    signals: Cell::new(ZX_SIGNAL_NONE),
                    cancel_called: Cell::new(false),
                    match_called: Cell::new(false),
                    port,
                    key,
                },
                _pin: PhantomPinned,
            }
        }

        fn init(&self) -> *mut c_void {
            /// # Safety
            ///
            /// `ctx` must point to a valid `TestSignalObserverState`.
            unsafe extern "C" fn on_match(ctx: *const c_void, signals: zx_signals_t) {
                // SAFETY: `ctx` points to a valid `TestSignalObserverState`.
                let state = unsafe { &*ctx.cast::<TestSignalObserverState>() };
                assert!(!state.cancel_called.get() && !state.match_called.get());
                state.signals.set(signals);
                state.match_called.set(true);
            }
            /// # Safety
            ///
            /// `ctx` must point to a valid `TestSignalObserverState`.
            unsafe extern "C" fn on_cancel(ctx: *const c_void, signals: zx_signals_t) {
                // SAFETY: `ctx` points to a valid `TestSignalObserverState`.
                let state = unsafe { &*ctx.cast::<TestSignalObserverState>() };
                assert!(!state.cancel_called.get() && !state.match_called.get());
                state.signals.set(signals);
                state.cancel_called.set(true);
            }
            /// # Safety
            ///
            /// `ctx` must point to a valid `TestSignalObserverState`.
            unsafe extern "C" fn matches_key(
                ctx: *const c_void,
                port: *const c_void,
                key: u64,
            ) -> bool {
                // SAFETY: `ctx` points to a valid `TestSignalObserverState`.
                let state = unsafe { &*ctx.cast::<TestSignalObserverState>() };
                !state.port.is_null() && port == state.port && key == state.key
            }

            if self.observer_ptr.get().is_null() {
                // SAFETY: `self.storage` is 8-byte aligned and 80 bytes large (`OpaqueBytes<80>`
                // provides interior mutability), and `self.state` remains pinned on the caller's
                // stack for the lifetime of `self`.
                let ptr = unsafe {
                    cpp_callback_signal_observer_init(
                        self.storage.0.get().cast(),
                        ptr::from_ref(&self.state).cast(),
                        on_match,
                        on_cancel,
                        matches_key,
                    )
                };
                self.observer_ptr.set(ptr);
            }
            self.observer_ptr.get()
        }

        fn match_called(&self) -> bool {
            self.state.match_called.get()
        }

        fn cancel_called(&self) -> bool {
            self.state.cancel_called.get()
        }

        fn signals(&self) -> zx_signals_t {
            self.state.signals.get()
        }
    }

    impl Drop for TestSignalObserver<'_> {
        fn drop(&mut self) {
            let obs_ptr = self.observer_ptr.get();
            if !obs_ptr.is_null() {
                let mut signals: zx_signals_t = 0;
                // SAFETY: `obs_ptr` was initialized by `cpp_callback_signal_observer_init`.
                // Ensure it is unlinked from `self.dispatcher` before destroying the C++
                // `CallbackSignalObserver` so `~DoublyLinkedListable()` does not assert if a test
                // exits early while the observer is still registered.
                unsafe {
                    let _ = self.dispatcher.remove_observer(obs_ptr, &mut signals);
                    cpp_callback_signal_observer_destroy(obs_ptr);
                }
            }
        }
    }

    /// TestBasicMatch
    #[test]
    fn test_basic_match() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();
        let observer = TestSignalObserver::new_in_place(tracker);
        let obs_ptr = observer.init();

        // Add observer.
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_ok!(unsafe { tracker.add_observer(obs_ptr, ptr::null(), TEST_SIGNAL1) });
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());

        // Trigger a signal we are not watching.
        tracker.update_state(0, TEST_SIGNAL2);
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());

        // Trigger the observer's signal.
        tracker.update_state(0, TEST_SIGNAL1);
        expect_true!(observer.match_called());
        expect_false!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL1 | TEST_SIGNAL2, observer.signals());
    }

    /// TestAlreadyMatched
    #[test]
    fn test_already_matched() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();

        // Set the signal.
        tracker.update_state(0, TEST_SIGNAL1);

        // Add observer waiting on the already-set signal.
        let observer = TestSignalObserver::new_in_place(tracker);
        let obs_ptr = observer.init();
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_ok!(unsafe { tracker.add_observer(obs_ptr, ptr::null(), TEST_SIGNAL1) });
        expect_true!(observer.match_called());
        expect_false!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL1, observer.signals());
    }

    /// TestCancelled
    #[test]
    fn test_cancelled() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();
        let observer = TestSignalObserver::new_in_place(tracker);
        let obs_ptr = observer.init();
        let dummy_handle: usize = 0;
        let handle_ptr = ptr::from_ref(&dummy_handle).cast::<c_void>();
        let dummy_handle2: usize = 0;
        let handle2_ptr = ptr::from_ref(&dummy_handle2).cast::<c_void>();

        // Add observer.
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_ok!(unsafe { tracker.add_observer(obs_ptr, handle_ptr, TEST_SIGNAL1) });

        // Unrelated signal arrives.
        tracker.update_state(0, TEST_SIGNAL2);
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());

        // Cancel a different handle.
        tracker.cancel_raw(handle2_ptr);
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());

        // Cancel observer.
        tracker.cancel_raw(handle_ptr);
        expect_false!(observer.match_called());
        expect_true!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL2, observer.signals());
    }

    /// TestRemoveObserver
    #[test]
    fn test_remove_observer() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();
        let observer = TestSignalObserver::new_in_place(tracker);
        let obs_ptr = observer.init();

        // Add observer.
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_ok!(unsafe { tracker.add_observer(obs_ptr, ptr::null(), TEST_SIGNAL1) });

        // Unrelated signal arrives.
        tracker.update_state(0, TEST_SIGNAL2);
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());

        // Remove observer.
        let mut signals: zx_signals_t = 0;
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_true!(unsafe { tracker.remove_observer(obs_ptr, &mut signals) });
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL2, signals);

        // Removing again fails.
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_false!(unsafe { tracker.remove_observer(obs_ptr, &mut signals) });

        // Trigger the observer's signal after removal.
        tracker.update_state(0, TEST_SIGNAL1);
        expect_false!(observer.match_called());
        expect_false!(observer.cancel_called());
    }

    /// TestRemoveObserverAfterMatch
    #[test]
    fn test_remove_observer_after_match() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();
        let observer = TestSignalObserver::new_in_place(tracker);
        let obs_ptr = observer.init();

        // Add observer.
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_ok!(unsafe { tracker.add_observer(obs_ptr, ptr::null(), TEST_SIGNAL1) });

        // Trigger the observer's signal.
        tracker.update_state(0, TEST_SIGNAL1);
        expect_true!(observer.match_called());
        expect_false!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL1, observer.signals());

        // Remove observer.
        let mut signals: zx_signals_t = 0;
        // SAFETY: `obs_ptr` is a valid `SignalObserver`.
        expect_false!(unsafe { tracker.remove_observer(obs_ptr, &mut signals) });
        expect_true!(observer.match_called());
        expect_false!(observer.cancel_called());
        expect_eq!(TEST_SIGNAL1, observer.signals());
    }

    /// TestRemoveByKey
    #[test]
    fn test_remove_by_key() {
        let (handle, _rights) = unwrap_ok!(EventDispatcher::create());
        let tracker = handle.dispatcher();
        let dummy_handle: usize = 0;
        let handle_ptr = ptr::from_ref(&dummy_handle).cast::<c_void>();
        let dummy_handle2: usize = 0;
        let handle2_ptr = ptr::from_ref(&dummy_handle2).cast::<c_void>();
        let dummy_port: usize = 0;
        let port_ptr = ptr::from_ref(&dummy_port).cast::<c_void>();
        let dummy_port2: usize = 0;
        let port2_ptr = ptr::from_ref(&dummy_port2).cast::<c_void>();
        const KEY1: u64 = 1;
        const KEY2: u64 = 2;

        // Add two observers with different keys.
        let observer1 = TestSignalObserver::new_with_key(tracker, port_ptr, KEY1);
        let observer2 = TestSignalObserver::new_with_key(tracker, port_ptr, KEY2);
        let obs1_ptr = observer1.init();
        let obs2_ptr = observer2.init();
        // SAFETY: `obs1_ptr` and `obs2_ptr` are valid `SignalObserver`s.
        expect_ok!(unsafe { tracker.add_observer(obs1_ptr, handle_ptr, TEST_SIGNAL1) });
        expect_ok!(unsafe { tracker.add_observer(obs2_ptr, handle_ptr, TEST_SIGNAL2) });

        // Cancelling with wrong handle, wrong port, or wrong key does not match.
        expect_false!(tracker.cancel_by_key_raw(handle2_ptr, port_ptr, KEY1));
        expect_false!(tracker.cancel_by_key_raw(handle_ptr, port2_ptr, KEY1));
        expect_false!(tracker.cancel_by_key_raw(handle_ptr, port_ptr, 0x321));

        // Cancel observer 1 by key.
        expect_true!(tracker.cancel_by_key_raw(handle_ptr, port_ptr, KEY1));
        expect_false!(observer1.match_called());
        expect_true!(observer1.cancel_called());
        expect_false!(observer2.match_called());
        expect_false!(observer2.cancel_called());

        // Ensure observer 2 still works.
        tracker.update_state(0, TEST_SIGNAL1 | TEST_SIGNAL2);
        expect_false!(observer1.match_called());
        expect_true!(observer2.match_called());
    }
}
