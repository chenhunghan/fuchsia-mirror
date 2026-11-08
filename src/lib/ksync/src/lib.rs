// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![deny(
    dead_code,
    unused_parens,
    unused_results,
    unsafe_op_in_unsafe_fn,
    missing_unsafe_on_extern,
    unsafe_attr_outside_unsafe,
    non_local_definitions,
    mismatched_lifetime_syntaxes,
    clippy::all,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default
)]

#[cfg(test)]
extern crate self as ksync;

pub use kstring::declare_interned_string;
pub use ksync_macro::{declare_singleton_lock, guarded};
pub use pin_init;

/// Locks a mutex.
///
/// Usage:
///   `ksync::lock!(let mut guard = self.lock_mu());`
///   Locks the mutex and binds a mutable pin to `guard`. Useful when you need to mutate
///   guarded fields via `guard.as_mut().fields_mut()`.
///
///   `ksync::lock!(let guard = self.lock_mu());`
///   Locks the mutex and binds an immutable pin to `guard`. Useful for read-only access
///   to guarded fields via `guard.fields()`.
///
///   `ksync::lock!(self.lock_mu());`
///   Locks the mutex and keeps it locked until the end of the scope, without binding the guard.
#[macro_export]
macro_rules! lock {
    (let mut $guard:ident = $lock_init:expr) => {
        $crate::lock!(let $guard = $lock_init);
        let mut $guard = $guard;
    };
    (let $guard:ident = $lock_init:expr) => {
        let lock_init = $lock_init;
        let $guard = ::core::pin::pin!($crate::GuardSlot::uninit());
        #[allow(unused_mut)]
        let mut $guard = $crate::GuardSlot::init($guard, lock_init);
    };
    ($lock_init:expr) => {
        $crate::lock!(let _guard = $lock_init);
    };
}

/// Pinned stack slot holding the guard of a [`lock!`]. Use [`lock!`] instead of this primitive.
///
/// This is `pin_init::stack_pin_init!`'s `StackInit`, except that [`GuardSlot::init`] is always
/// inlined and keeps the re-initialization path (which [`lock!`] never takes) out of line. Once
/// inlined, the dead re-initialization path folds away, so the guard's acquisition and release
/// are inlined into the caller and the guard can be kept in registers. Otherwise the size of the
/// re-initialization path (which drops a guard, i.e. inlines a release) can keep `init`, and so
/// the lock acquisition, out of line.
///
/// # Invariants
///
/// If `self.is_init` is true, then `self.value` is initialized.
#[doc(hidden)]
pub struct GuardSlot<T> {
    value: core::mem::MaybeUninit<T>,
    is_init: bool,
}

impl<T> GuardSlot<T> {
    /// Creates a new, uninitialized `GuardSlot`.
    #[inline(always)]
    pub fn uninit() -> Self {
        Self { value: core::mem::MaybeUninit::uninit(), is_init: false }
    }

    /// Initializes the contents with `init` and returns them, pinned.
    #[inline(always)]
    pub fn init(
        self: core::pin::Pin<&mut Self>,
        init: impl pin_init::PinInit<T, core::convert::Infallible>,
    ) -> core::pin::Pin<&mut T> {
        // SAFETY: We never move out of `this`.
        let this = unsafe { core::pin::Pin::into_inner_unchecked(self) };
        // The value is currently initialized, so it needs to be dropped before the memory can be
        // reused (this is a safety guarantee of `Pin`).
        if this.is_init {
            this.drop_value();
        }
        // SAFETY: The memory slot is valid and this type ensures that it will stay pinned.
        let Ok(()) = unsafe { init.__pinned_init(this.value.as_mut_ptr()) };
        // INVARIANT: `this.value` is initialized above.
        this.is_init = true;
        // SAFETY: The slot is now pinned, since we never give access to `&mut T`.
        unsafe { core::pin::Pin::new_unchecked(this.value.assume_init_mut()) }
    }

    /// Drops the initialized contents. Must only be called when `self.is_init` is true.
    #[cold]
    #[inline(never)]
    fn drop_value(&mut self) {
        debug_assert!(self.is_init);
        self.is_init = false;
        // SAFETY: Only called when `self.is_init` was true, so `self.value` is initialized.
        unsafe { self.value.assume_init_drop() };
    }
}

impl<T> Drop for GuardSlot<T> {
    #[inline]
    fn drop(&mut self) {
        if self.is_init {
            // SAFETY: As we are being dropped, we only call this once. And since `self.is_init` is
            // true, `self.value` is initialized.
            unsafe { self.value.assume_init_drop() };
        }
    }
}

/// A static C-string tag identifying the source location (`"file:line"`) where a lock is acquired.
///
/// Used with `TrackedMonitoredSpinlock` to pass critical section names to the kernel lockup
/// detector.
/// Construct via the [`source_tag!`] macro, which is the Rust equivalent of C++ `SOURCE_TAG`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SourceTag(&'static core::ffi::CStr);

impl SourceTag {
    /// Creates a `SourceTag` from a static null-terminated byte slice without checking for
    /// interior null bytes.
    ///
    /// # Safety
    ///
    /// `bytes` must be a valid null-terminated C string (ending in `\0` with no interior `\0`
    /// bytes).
    #[inline]
    pub const unsafe fn from_bytes_with_nul_unchecked(bytes: &'static [u8]) -> Self {
        // SAFETY: The caller guarantees `bytes` is a valid null-terminated byte slice.
        Self(unsafe { core::ffi::CStr::from_bytes_with_nul_unchecked(bytes) })
    }

    /// Creates a `SourceTag` from a static `CStr`.
    #[inline]
    pub const fn from_cstr(cstr: &'static core::ffi::CStr) -> Self {
        Self(cstr)
    }

    /// Returns a pointer to the underlying null-terminated C string.
    #[inline]
    pub const fn as_ptr(&self) -> *const core::ffi::c_char {
        self.0.as_ptr()
    }

    /// Returns the underlying `CStr`.
    #[inline]
    pub const fn as_cstr(&self) -> &'static core::ffi::CStr {
        self.0
    }
}

impl Default for SourceTag {
    #[inline]
    fn default() -> Self {
        Self(c"<unknown>")
    }
}

/// Expands to a [`SourceTag`] containing the file path and line number (`"file:line"`) of the
/// call site, equivalent to C++ `SOURCE_TAG`.
#[macro_export]
macro_rules! source_tag {
    () => {{
        // SAFETY: `concat!(file!(), ":", line!(), "\0")` produces a static string literal ending
        // in a single NUL byte and containing no interior NUL bytes.
        const TAG: $crate::SourceTag = unsafe {
            $crate::SourceTag::from_bytes_with_nul_unchecked(
                concat!(file!(), ":", line!(), "\0").as_bytes(),
            )
        };
        TAG
    }};
}

mod brwlock;
mod kcell;
mod kmutex;
mod konce_cell;
mod lock_token;
mod lockdep_tracked;
mod phantom_mutex;
mod raw_lock;
mod singleton;

#[cfg(not(feature = "kernel"))]
mod raw_userspace_mutex;

#[cfg(all(not(feature = "kernel"), test))]
mod raw_userspace_brwlock;

pub use brwlock::{BrwLockPi, BrwLockPiReadGuard, BrwLockPiWriteGuard};
pub use kcell::{KCell, KCellInit, kcell_init};
pub use kmutex::{
    AliasedLock, KMutex, KMutexAliasedGuard, KMutexGuard, KMutexGuardMultiple, aliased_lock,
    aliased_lock_policy, aliased_lock_policy_with, aliased_lock_with, guard_multiple,
    guard_multiple_policy, guard_multiple_policy_with, guard_multiple_with,
};
pub use konce_cell::{KOnceCell, KOnceCellGuard};
pub use lock_token::{ExclusiveLockToken, LockToken, LockTokenRead, LockTokenWrite};
pub use lockdep::{
    LOCK_FLAGS_IRQ_SAFE, LOCK_FLAGS_MULTI_ACQUIRE, LOCK_FLAGS_NESTABLE, LOCK_FLAGS_NONE,
    LOCK_FLAGS_SINGLETON_LOCK, LOCK_FLAGS_TRACKING_DISABLED, LockClass, LockClassRegistration,
    LockFlags,
};
pub use lockdep_tracked::{
    LockdepTracked, LockdepTrackedGuardState, OrderedGuardState, OrderedLockPolicy,
};
pub use phantom_mutex::PhantomMutex;
pub use raw_lock::{
    ConstRawLock, ExclusiveLockPolicy, LockPolicy, RawAssertHeldLock, RawLock, RawSharedLock,
    RawUntrackedIrqLock, RawUntrackedLock,
};
pub use singleton::SingletonMutex;

// The kernel's raw lock implementations (`TrackedMutex`, `TrackedCriticalMutex`,
// `TrackedSpinlock`, `TrackedMonitoredSpinlock`, `RawBrwLockPi`, and `RawBrwLockNoPi`) are not
// part of this crate: they live in the kernel crate itself, and are re-exported from its
// `crate::ksync_kernel` (see `//zircon/kernel/lib/ksync`). Generic code using
// `ksync` must therefore be parameterized over its `RawLock` type, with the kernel supplying the
// concrete raw lock. Only userspace implementations are provided here, for host builds and tests.
#[cfg(not(feature = "kernel"))]
pub use raw_userspace_mutex::{RawMutex, RawMutexStorage};

#[cfg(all(not(feature = "kernel"), test))]
pub use raw_userspace_brwlock::{
    RawBrwLockPi, RawBrwLockPiReadPolicy, RawBrwLockPiStorage, RawBrwLockPiWritePolicy,
};
