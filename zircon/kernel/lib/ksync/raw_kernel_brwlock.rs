// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use ksync::{ExclusiveLockPolicy, LockPolicy, LockdepTracked, RawLock, RawSharedLock};
use pin_init::{PinInit, pin_data};

unsafe extern "C" {
    fn cpp_brwlock_pi_init(lock: *mut c_void, class_id: *const c_void);
    fn cpp_brwlock_pi_destroy(lock: *mut c_void);
    fn cpp_brwlock_pi_acquire_read(lock: *mut c_void);
    fn cpp_brwlock_pi_release_read(lock: *mut c_void);
    fn cpp_brwlock_pi_acquire_write(lock: *mut c_void);
    fn cpp_brwlock_pi_release_write(lock: *mut c_void);
    fn cpp_brwlock_no_pi_init(lock: *mut c_void, class_id: *const c_void);
    fn cpp_brwlock_no_pi_destroy(lock: *mut c_void);
    fn cpp_brwlock_no_pi_acquire_read(lock: *mut c_void);
    fn cpp_brwlock_no_pi_release_read(lock: *mut c_void);
    fn cpp_brwlock_no_pi_acquire_write(lock: *mut c_void);
    fn cpp_brwlock_no_pi_release_write(lock: *mut c_void);
}

/// Size in bytes of the C++ `BrwLockNoPi` object.
const BRWLOCK_NO_PI_STORAGE_SIZE: usize =
    if crate::kernel::spin_tracing::RWLOCK_NAME_TRACING_ENABLED { 80 } else { 72 };

/// Opaque layout block matching the Zircon C++ `BrwLockPi` object.
#[cfg(not(target_arch = "riscv64"))]
#[pin_data(PinnedDrop)]
#[repr(C, align(16))]
pub struct RawBrwLockPiStorage {
    #[pin]
    storage: zr::OpaqueBytes<128>,
}

/// Opaque layout block matching the Zircon C++ `BrwLockPi` object.
#[cfg(target_arch = "riscv64")]
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawBrwLockPiStorage {
    #[pin]
    storage: zr::OpaqueBytes<BRWLOCK_NO_PI_STORAGE_SIZE>,
}

/// Opaque layout block matching the Zircon C++ `BrwLockNoPi` object.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawBrwLockNoPiStorage {
    #[pin]
    storage: zr::OpaqueBytes<BRWLOCK_NO_PI_STORAGE_SIZE>,
}

macro_rules! impl_raw_brwlock {
    (
        storage: $storage:ident,
        read_policy: $read_policy:ident,
        write_policy: $write_policy:ident,
        tracked: $tracked:ident,
        cpp_name: $cpp_name:literal,
        ffi_init: $ffi_init:ident,
        ffi_destroy: $ffi_destroy:ident,
        ffi_acquire_read: $ffi_acquire_read:ident,
        ffi_release_read: $ffi_release_read:ident,
        ffi_acquire_write: $ffi_acquire_write:ident,
        ffi_release_write: $ffi_release_write:ident $(,)?
    ) => {
        // SAFETY: `$storage` is safe to share and access across threads because it delegates to
        // the thread-safe Zircon kernel `$cpp_name` implementation.
        unsafe impl Sync for $storage {}
        // SAFETY: `$storage` is safe to send across threads because it delegates to the
        // thread-safe Zircon kernel `$cpp_name` implementation.
        unsafe impl Send for $storage {}

        // SAFETY: `$storage` can only be constructed in-place via `RawLock::init` (`$ffi_init`)
        // and is `!Unpin`, and all read/write guards borrow the lock for their lifetime, so
        // `$ffi_destroy` is called on a live, unheld C++ lock.
        zr::unsafe_pinned_drop_ffi!($storage, $ffi_destroy);

        #[doc = concat!("Policy for acquiring and releasing [`", stringify!($storage), "`]")]
        /// in shared (read) mode.
        pub struct $read_policy;

        // SAFETY: `$ffi_acquire_read` and `$ffi_release_read` enforce shared reader
        // synchronization on `$storage`.
        unsafe impl LockPolicy<$storage> for $read_policy {
            type AcquireArgs = ();
            type GuardState = ();

            /// Acquires `lock` in shared (read) mode.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::acquire`] contract.
            #[inline]
            unsafe fn acquire(lock: &$storage, _state: &mut Self::GuardState, _args: ()) {
                // SAFETY: Every `&$storage` was initialized in-place by `RawLock::init` and
                // remains pinned because `$storage` is `!Unpin`.
                unsafe {
                    $ffi_acquire_read(lock.as_mut_ptr());
                }
            }

            /// Re-acquires `lock` in shared (read) mode after a temporary release.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::reacquire`] contract.
            #[inline]
            unsafe fn reacquire(lock: &$storage, state: &mut Self::GuardState) {
                // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
                unsafe {
                    Self::acquire(lock, state, ());
                }
            }

            /// Releases `lock` from shared (read) mode.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::release`] contract (`lock` must be currently
            /// held in shared (read) mode by the calling thread).
            #[inline]
            unsafe fn release(lock: &$storage, _state: &mut Self::GuardState) {
                // SAFETY: Every `&$storage` is an initialized, pinned lock, and the caller
                // guarantees the current thread holds the read lock.
                unsafe {
                    $ffi_release_read(lock.as_mut_ptr());
                }
            }
        }

        #[doc = concat!("Policy for acquiring and releasing [`", stringify!($storage), "`]")]
        /// in exclusive (write) mode.
        pub struct $write_policy;

        // SAFETY: `$ffi_acquire_write` and `$ffi_release_write` enforce exclusive mutual
        // exclusion on `$storage`.
        unsafe impl LockPolicy<$storage> for $write_policy {
            type AcquireArgs = ();
            type GuardState = ();

            /// Acquires `lock` in exclusive (write) mode.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::acquire`] contract.
            #[inline]
            unsafe fn acquire(lock: &$storage, _state: &mut Self::GuardState, _args: ()) {
                // SAFETY: Every `&$storage` was initialized in-place by `RawLock::init` and
                // remains pinned because `$storage` is `!Unpin`.
                unsafe {
                    $ffi_acquire_write(lock.as_mut_ptr());
                }
            }

            /// Re-acquires `lock` in exclusive (write) mode after a temporary release.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::reacquire`] contract.
            #[inline]
            unsafe fn reacquire(lock: &$storage, state: &mut Self::GuardState) {
                // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
                unsafe {
                    Self::acquire(lock, state, ());
                }
            }

            /// Releases `lock` from exclusive (write) mode.
            ///
            /// # Safety
            ///
            /// Caller must uphold the [`LockPolicy::release`] contract (`lock` must be currently
            /// held in exclusive (write) mode by the calling thread).
            #[inline]
            unsafe fn release(lock: &$storage, _state: &mut Self::GuardState) {
                // SAFETY: Every `&$storage` is an initialized, pinned lock, and the caller
                // guarantees the current thread holds the write lock.
                unsafe {
                    $ffi_release_write(lock.as_mut_ptr());
                }
            }
        }

        // SAFETY: `$write_policy` enforces exclusive mutual exclusion on `$storage`.
        unsafe impl ExclusiveLockPolicy<$storage> for $write_policy {}

        // SAFETY: `$storage` is a physical kernel rwlock with a unique storage address.
        unsafe impl RawLock for $storage {
            type DefaultPolicy = $write_policy;

            /// Returns a pinned initializer for `Self` with the given lockdep `class_id`.
            ///
            /// # Safety
            ///
            /// `class_id` must be either null or a valid static `lockdep::LockClassId` pointer.
            #[inline]
            unsafe fn init(
                class_id: *const c_void,
            ) -> impl PinInit<Self, core::convert::Infallible> {
                // SAFETY: `Self` has the same size and alignment as the C++ lock (enforced by
                // compile-time assertions), `$ffi_init` placement-new initializes it in `slot`,
                // and the caller guarantees `class_id` is null or a valid static
                // `lockdep::LockClassId`.
                zr::pin_init_ffi!($ffi_init, class_id)
            }

            #[inline]
            fn as_mut_ptr(&self) -> *mut c_void {
                self.storage.get().cast::<c_void>()
            }
        }

        // SAFETY: `$read_policy` acquires `$storage` in shared (read) mode, which allows multiple
        // concurrent readers but excludes the exclusive (write) mode acquired by `$write_policy`,
        // the default and only `ExclusiveLockPolicy` for `$storage`.
        unsafe impl RawSharedLock for $storage {
            type SharedPolicy = $read_policy;
        }

        #[doc = concat!("Lockdep-tracked kernel `", $cpp_name, "`")]
        #[doc = concat!("matching the Zircon C++ `Lock<", $cpp_name, ">` layout.")]
        pub type $tracked = LockdepTracked<$storage>;
    };
}

impl_raw_brwlock! {
    storage: RawBrwLockPiStorage,
    read_policy: RawBrwLockPiReadPolicy,
    write_policy: RawBrwLockPiWritePolicy,
    tracked: RawBrwLockPi,
    cpp_name: "BrwLockPi",
    ffi_init: cpp_brwlock_pi_init,
    ffi_destroy: cpp_brwlock_pi_destroy,
    ffi_acquire_read: cpp_brwlock_pi_acquire_read,
    ffi_release_read: cpp_brwlock_pi_release_read,
    ffi_acquire_write: cpp_brwlock_pi_acquire_write,
    ffi_release_write: cpp_brwlock_pi_release_write,
}

impl_raw_brwlock! {
    storage: RawBrwLockNoPiStorage,
    read_policy: RawBrwLockNoPiReadPolicy,
    write_policy: RawBrwLockNoPiWritePolicy,
    tracked: RawBrwLockNoPi,
    cpp_name: "BrwLockNoPi",
    ffi_init: cpp_brwlock_no_pi_init,
    ffi_destroy: cpp_brwlock_no_pi_destroy,
    ffi_acquire_read: cpp_brwlock_no_pi_acquire_read,
    ffi_release_read: cpp_brwlock_no_pi_release_read,
    ffi_acquire_write: cpp_brwlock_no_pi_acquire_write,
    ffi_release_write: cpp_brwlock_no_pi_release_write,
}

#[cfg(all(not(target_arch = "riscv64"), not(with_lock_dep)))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 128);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 16);
};

#[cfg(all(not(target_arch = "riscv64"), with_lock_dep))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 144);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 16);
};

#[cfg(all(target_arch = "riscv64", not(with_lock_dep)))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == BRWLOCK_NO_PI_STORAGE_SIZE);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 8);
};

#[cfg(all(target_arch = "riscv64", with_lock_dep))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == BRWLOCK_NO_PI_STORAGE_SIZE + 8);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 8);
};

#[cfg(not(with_lock_dep))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockNoPi>() == BRWLOCK_NO_PI_STORAGE_SIZE);
    assert!(core::mem::align_of::<RawBrwLockNoPi>() == 8);
};

#[cfg(with_lock_dep)]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockNoPi>() == BRWLOCK_NO_PI_STORAGE_SIZE + 8);
    assert!(core::mem::align_of::<RawBrwLockNoPi>() == 8);
};
