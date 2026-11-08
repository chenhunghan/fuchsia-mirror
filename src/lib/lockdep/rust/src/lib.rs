// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

/// Flags to specify which rules to apply to a lock class during validation.
pub type LockFlags = u16;

/// Apply only common rules that apply to all locks.
pub const LOCK_FLAGS_NONE: LockFlags = 0;
/// Apply the irq-safety rules in addition to the common rules for all locks.
pub const LOCK_FLAGS_IRQ_SAFE: LockFlags = 1 << 0;
/// Apply the nestable rules in addition to the common rules for all locks.
///
/// Several instances of a nestable class may be held at once by one thread, provided each is
/// acquired with an external order that is strictly greater than that of every instance of the
/// class already held. Lockdep validates same-class nesting solely by comparing orders on the
/// current thread; it does not record dependency edges within a class. An acquisition without an
/// explicit order records order `0`, so acquiring a nestable lock unordered while holding any
/// instance of its class fails validation, whereas acquiring with a nonzero order while holding
/// an unordered instance passes without checking that the held instance genuinely belongs below
/// that order. Only mix unordered and ordered acquisitions of one class when the unordered
/// instance truly occupies order `0` in the class's ordering.
pub const LOCK_FLAGS_NESTABLE: LockFlags = 1 << 1;
/// Apply the multi-acquire rules in additioon to the common rules for all
/// locks.
pub const LOCK_FLAGS_MULTI_ACQUIRE: LockFlags = 1 << 2;
/// Apply the leaf lock rules in addition to the common rules for all locks.
/// NOTE: Use this flag with caution. See https://fxbug.dev/459856993.
pub const LOCK_FLAGS_LEAF: LockFlags = 1 << 3;
/// Do not report validation errors. This flag prevents recursive validation
/// of locks that are acquired by reporting routines.
pub const LOCK_FLAGS_REPORTING_DISABLED: LockFlags = 1 << 4;
/// There is only one member of this locks class.
pub const LOCK_FLAGS_SINGLETON_LOCK: LockFlags = 1 << 5;
/// Abort the program with an error if a lock is improperly acquired more
/// than once in the same context.
pub const LOCK_FLAGS_RE_ACQUIRE_FATAL: LockFlags = 1 << 6;
/// Do not add this acquisition to the active list. This may be required for
/// locks that are used to protect context switching logic.
pub const LOCK_FLAGS_ACTIVE_LIST_DISABLED: LockFlags = 1 << 7;
/// Do not track this lock.
pub const LOCK_FLAGS_TRACKING_DISABLED: LockFlags = 1 << 8;

// Keep dependency alive even when lockdep is disabled to satisfy unused dependency lints.
use zr as _;

/// Trait implemented by lock class types to expose their dynamic LockClassId pointer and the
/// validation flags the class is registered with.
pub trait LockClass {
    const ID: *mut core::ffi::c_void;

    /// The `LOCK_FLAGS_*` of this class, including the raw lock's own flags.
    ///
    /// A class registered from Rust must report the flags it is registered with; a class
    /// mirroring a C++-owned lock (whose `ID` is null because the class state lives in C++) must
    /// report that lock's flags; a class that is not tracked at all (`#[mutex(untracked)]` or an
    /// untracked storage type, also with a null `ID`) reports [`LOCK_FLAGS_NONE`].
    ///
    /// This is a lockdep-validity requirement, not a soundness one: a mismatch degrades lock
    /// validation or trips a debug assertion but cannot cause memory unsafety.
    const FLAGS: LockFlags;
}

#[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
mod enabled {
    core::cfg_select! {
        feature = "lock_dep" => {
            const LOCK_CLASS_STATE_SIZE: usize = 1608;
            const LOCK_CLASS_REGISTRATION_SIZE: usize = 1624;
        }
        feature = "lock_metadata_only" => {
            const LOCK_CLASS_STATE_SIZE: usize = 8;
            const LOCK_CLASS_REGISTRATION_SIZE: usize = 24;
        }
    }

    #[repr(C, align(8))]
    #[derive(Default)]
    struct LockClassStateStorage(zr::OpaqueBytes<LOCK_CLASS_STATE_SIZE>);

    impl LockClassStateStorage {
        const fn uninit() -> Self {
            Self(zr::OpaqueBytes::uninit())
        }
    }

    /// A registration entry for a Rust lock class.
    ///
    /// This struct is registered with the C++ lockdep implementation via a linker section. The
    /// layout of this struct is known to C++.
    #[repr(C)]
    pub struct LockClassRegistration {
        name: *const kstring::interned_string::InternedString,
        flags: u16,
        state_storage: LockClassStateStorage,
    }

    unsafe impl Sync for LockClassRegistration {}
    unsafe impl Send for LockClassRegistration {}

    impl LockClassRegistration {
        pub const fn new(name: &'static ::kstring::interned_string::InternedString) -> Self {
            Self { name, flags: 0, state_storage: LockClassStateStorage::uninit() }
        }

        pub const fn with_flags(
            name: &'static ::kstring::interned_string::InternedString,
            flags: crate::LockFlags,
        ) -> Self {
            crate::validate_lock_flags(flags);
            Self { name, flags, state_storage: LockClassStateStorage::uninit() }
        }

        #[inline]
        pub const fn get(&self) -> *mut core::ffi::c_void {
            self.state_storage.0.get() as *mut _
        }
    }

    zr::static_assert!(
        core::mem::size_of::<LockClassRegistration>() == LOCK_CLASS_REGISTRATION_SIZE
    );
    zr::static_assert!(core::mem::align_of::<LockClassRegistration>() == 8);
}

#[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
pub use enabled::LockClassRegistration;

#[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
mod disabled {
    /// A registration entry for a Rust lock class (stub for disabled lockdep).
    pub struct LockClassRegistration;

    impl LockClassRegistration {
        pub const fn new(_name: &'static ::kstring::interned_string::InternedString) -> Self {
            Self
        }

        pub const fn with_flags(
            _name: &'static ::kstring::interned_string::InternedString,
            flags: crate::LockFlags,
        ) -> Self {
            crate::validate_lock_flags(flags);
            Self
        }

        #[inline]
        pub const fn get(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }
    }
}

#[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
pub use disabled::LockClassRegistration;

/// Asserts that the current thread holds no tracked locks.
///
/// When lock validation is enabled, this checks the current thread's lock state
/// and panics or generates a kernel oops if any tracked locks are held.
/// When lock validation is disabled, this is a zero-cost no-op.
#[inline(always)]
pub fn assert_no_locks_held() {
    #[cfg(feature = "lock_dep")]
    {
        unsafe extern "C" {
            fn lockdep_assert_no_locks_held();
        }
        // SAFETY: `lockdep_assert_no_locks_held` only inspects thread-local lock state
        // and does not violate Rust memory safety or aliasing invariants.
        unsafe { lockdep_assert_no_locks_held() }
    }
}

/// Checks that `flags` is a valid combination of `LOCK_FLAGS_*`, mirroring the `static_assert`
/// checks in C++ `DefaultLockFlags` and `SingletonLockFlags` (`<lockdep/lock_traits.h>`), and
/// panics (fails compilation in `const` context) otherwise.
///
/// This is applied to every [`LockClassRegistration`] and, by the `ksync` guards, to
/// [`LockClass::FLAGS`] of every lock class that is locked, so that manual `LockClass`
/// implementations are checked too.
pub const fn validate_lock_flags(flags: LockFlags) {
    let is_multi_acquire = (flags & LOCK_FLAGS_MULTI_ACQUIRE) != 0;
    let is_nestable = (flags & LOCK_FLAGS_NESTABLE) != 0;
    let is_re_acquire_fatal = (flags & LOCK_FLAGS_RE_ACQUIRE_FATAL) != 0;
    let is_singleton = (flags & LOCK_FLAGS_SINGLETON_LOCK) != 0;

    assert!(
        !(is_multi_acquire && is_nestable),
        "LOCK_FLAGS_MULTI_ACQUIRE and LOCK_FLAGS_NESTABLE are mutually exclusive"
    );
    assert!(
        !(is_multi_acquire && is_re_acquire_fatal),
        "LOCK_FLAGS_MULTI_ACQUIRE and LOCK_FLAGS_RE_ACQUIRE_FATAL are mutually exclusive"
    );
    assert!(
        !(is_singleton && is_multi_acquire),
        "LOCK_FLAGS_MULTI_ACQUIRE may not be used with a singleton lock"
    );
    assert!(
        !(is_singleton && is_nestable),
        "LOCK_FLAGS_NESTABLE may not be used with a singleton lock"
    );
}
