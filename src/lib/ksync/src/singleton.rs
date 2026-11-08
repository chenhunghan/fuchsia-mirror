// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::KMutex;

/// A statically initialized global singleton mutex.
pub type SingletonMutex<Class, M> = KMutex<Class, M>;

/// Declares a singleton userspace mutex.
///
/// This is only available outside the kernel. The kernel's `declare_singleton_mutex!` (along with
/// `declare_singleton_critical_mutex!`, `declare_singleton_spinlock!` and
/// `declare_singleton_monitored_spinlock!`) lives with the kernel's raw locks in
/// `//zircon/kernel/lib/ksync`.
///
/// This macro defines a struct with the given name and visibility, registers it as a singleton
/// lock with lockdep under the `rust_lock_classes` linker section, and provides accessors
/// (`Get()`, `get()`, `lock()`, and `lock_policy()`) to a statically initialized `KMutex`.
///
/// # Examples
///
/// ```rust
/// ksync::declare_singleton_mutex!(MyGlobalLock);
///
/// // Usage:
/// ksync::lock!(MyGlobalLock::get().lock());
/// // or
/// ksync::lock!(MyGlobalLock::lock());
/// ```
#[macro_export]
#[cfg(not(feature = "kernel"))]
macro_rules! declare_singleton_mutex {
    ($(#[$meta:meta])* $vis:vis $name:ident $(, $($opts:tt)*)?) => {
        $crate::declare_singleton_lock!($(#[$meta])* $vis $name, $crate::RawMutex $(, $($opts)*)?);
    };
}

#[cfg(not(feature = "kernel"))]
#[cfg(test)]
mod tests {
    declare_singleton_mutex!(TestSingleton);

    #[test]
    fn test_singleton_mutex() {
        let lock1 = TestSingleton::Get();
        let lock2 = TestSingleton::get();
        assert!(core::ptr::eq(lock1, lock2));

        {
            lock!(let _guard = TestSingleton::Get().lock());
        }

        {
            lock!(TestSingleton::lock());
        }
    }
}
