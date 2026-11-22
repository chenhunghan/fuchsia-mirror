// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The kernel's raw lock implementations for `ksync`.
//!
//! This module is compiled directly into the kernel crate (it is a `kernel_rust_mod`, mounted at
//! `crate::ksync_kernel`), rather than being part of the generic `ksync` crate. As a result, the
//! `ksync` crate and any other non-kernel crate cannot name the kernel's raw locks
//! ([`TrackedMutex`], [`TrackedCriticalMutex`], [`TrackedSpinlock`], [`TrackedMonitoredSpinlock`],
//! [`TrackedBrwLockPi`], and [`TrackedBrwLockNoPi`]) directly.
//! Such generic code must instead be parameterized over its [`ksync::RawLock`] (or
//! [`ksync::RawSharedLock`]) type, with the final usage site in the kernel crate supplying the
//! concrete raw lock.
//!
//! The kernel mutex raw locks are defined alongside the kernel
//! [`Mutex`](crate::kernel::mutex::Mutex) in `zircon/kernel/kernel/mutex.rs`, the kernel
//! spinlock raw locks alongside the kernel [`SpinLock`](crate::kernel::spinlock::SpinLock) in
//! `zircon/kernel/kernel/spinlock.rs`, and the kernel reader-writer raw locks alongside the kernel
//! [`BrwLock`](crate::kernel::brwlock::BrwLock) in `zircon/kernel/kernel/brwlock.rs`. All are
//! re-exported here.

pub use crate::kernel::brwlock::{
    BrwLockReaderPolicy, BrwLockWriterPolicy, TrackedBrwLockNoPi, TrackedBrwLockPi,
};
pub use crate::kernel::mutex::{
    CriticalMutexOrderedPolicy, CriticalMutexPolicy, MutexSpinMaxDurationPolicy,
    TrackedCriticalMutex, TrackedMutex,
};
pub use crate::kernel::spinlock::{
    IrqSavePolicy, MonitoredSpinlockGuardState, NoIrqSavePolicy, TrackedMonitoredSpinlock,
    TrackedSpinlock, TryAcquireResult,
};

/// A kernel spinlock (`KMutex` over a [`TrackedSpinlock`]).
pub type KSpinlock<Class> = ksync::KMutex<Class, TrackedSpinlock>;

/// A kernel monitored spinlock (`KMutex` over a [`TrackedMonitoredSpinlock`]).
pub type KMonitoredSpinlock<Class> = ksync::KMutex<Class, TrackedMonitoredSpinlock>;

/// Declares a singleton spinlock.
// Not every kernel configuration uses every singleton macro.
#[allow(unused_macros)]
macro_rules! declare_singleton_spinlock {
    ($(#[$meta:meta])* $vis:vis $name:ident $(, $($opts:tt)*)?) => {
        ::ksync::declare_singleton_lock!(
            $(#[$meta])* $vis $name, $crate::ksync_kernel::TrackedSpinlock $(, $($opts)*)?
        );
    };
}
#[allow(unused_imports)]
pub(crate) use declare_singleton_spinlock;

/// Declares a singleton monitored spinlock (integrated with the kernel lockup detector).
///
/// The optional trailing `cpu_align` or `cpu_align_exclusive` argument controls the placement of
/// the singleton's static storage, aligning it to a cache line (like C++ `__CPU_ALIGN`) and, for
/// `cpu_align_exclusive`, placing it in the `.data.cpu_align_exclusive` section. For example:
///
/// ```ignore
/// crate::ksync_kernel::declare_singleton_monitored_spinlock!(MyGlobalLock, cpu_align_exclusive);
/// ```
// Not every kernel configuration uses every singleton macro.
#[allow(unused_macros)]
macro_rules! declare_singleton_monitored_spinlock {
    ($(#[$meta:meta])* $vis:vis $name:ident $(, $($opts:tt)*)?) => {
        ::ksync::declare_singleton_lock!(
            $(#[$meta])* $vis $name, $crate::ksync_kernel::TrackedMonitoredSpinlock $(, $($opts)*)?
        );
    };
}
#[allow(unused_imports)]
pub(crate) use declare_singleton_monitored_spinlock;
