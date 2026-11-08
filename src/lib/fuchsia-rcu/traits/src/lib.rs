// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Foundational trait definitions for RCU-safe reclamation.
//!
//! This crate defines the [`RcuDroppable`] marker trait, its `core`/`alloc`/`std` implementations
//! (plus implementations for a few `zx` types when targeting Fuchsia), and the
//! `#[derive(RcuDroppable)]` proc-macro re-export.
//!
//! It is separate from `fuchsia-rcu` so that foundational crates can implement `RcuDroppable` for
//! their types without depending on the RCU runtime.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use fuchsia_rcu_derive::RcuDroppable;

/// Marker trait for types that are safe to be dropped on the RCU worker / drop execution path.
///
/// # Safety
/// Types implementing this trait MUST NOT have `Drop` side-effects that block or be time or
/// thread-sensitive.
pub unsafe trait RcuDroppable: Send + 'static {}

macro_rules! impl_rcu_droppable {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: Built in numeric types, atomics, strings, and standard types without
            // custom blocking Drop logic do not block or produce thread-sensitive drop side effects.
            unsafe impl RcuDroppable for $t {}
        )*
    };
}

impl_rcu_droppable!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    str,
    core::num::NonZeroU8,
    core::num::NonZeroU16,
    core::num::NonZeroU32,
    core::num::NonZeroU64,
    core::num::NonZeroU128,
    core::num::NonZeroUsize,
    core::num::NonZeroI8,
    core::num::NonZeroI16,
    core::num::NonZeroI32,
    core::num::NonZeroI64,
    core::num::NonZeroI128,
    core::num::NonZeroIsize,
    core::sync::atomic::AtomicBool,
    core::sync::atomic::AtomicI8,
    core::sync::atomic::AtomicI16,
    core::sync::atomic::AtomicI32,
    core::sync::atomic::AtomicI64,
    core::sync::atomic::AtomicIsize,
    core::sync::atomic::AtomicU8,
    core::sync::atomic::AtomicU16,
    core::sync::atomic::AtomicU32,
    core::sync::atomic::AtomicU64,
    core::sync::atomic::AtomicUsize,
    core::ops::RangeFull,
    core::time::Duration,
    alloc::string::String,
    bstr::BString,
    bstr::BStr,
);

#[cfg(feature = "std")]
impl_rcu_droppable!(std::collections::hash_map::RandomState, std::time::Instant,);

// SAFETY: Static references have no drop implementation and thus no drop side effects.
unsafe impl<T: ?Sized + Sync + 'static> RcuDroppable for &'static T {}
// SAFETY: BuildHasherDefault is a zero-sized type with no drop side effects.
unsafe impl<H: 'static> RcuDroppable for core::hash::BuildHasherDefault<H> {}
// SAFETY: PhantomData has no drop side effects.
unsafe impl<T: ?Sized + Send + 'static> RcuDroppable for core::marker::PhantomData<T> {}
// SAFETY: Option<T> only drops its inner value T when present, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for Option<T> {}
// SAFETY: Result<T, E> only drops its inner T or E value, both of which are RcuDroppable.
unsafe impl<T: RcuDroppable, E: RcuDroppable> RcuDroppable for Result<T, E> {}
// SAFETY: Box<T> deallocates heap memory and drops T, which is RcuDroppable.
unsafe impl<T: ?Sized + RcuDroppable> RcuDroppable for alloc::boxed::Box<T> {}
// SAFETY: Arc<T> decrements the reference count and only drops T (which is RcuDroppable + Sync)
// when the last strong reference is released.
unsafe impl<T: ?Sized + RcuDroppable + Sync> RcuDroppable for alloc::sync::Arc<T> {}
// SAFETY: Weak<T> decrements the weak reference count without dropping the inner T value.
unsafe impl<T: ?Sized + Send + Sync + 'static> RcuDroppable for alloc::sync::Weak<T> {}
// SAFETY: Vec<T> deallocates buffer memory and drops elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for alloc::vec::Vec<T> {}
// SAFETY: Arrays drop their elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable, const N: usize> RcuDroppable for [T; N] {}
// SAFETY: Slice drops its elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for [T] {}
// SAFETY: AtomicPtr has a trivial drop and does not own or drop the pointee.
unsafe impl<T: 'static> RcuDroppable for core::sync::atomic::AtomicPtr<T> {}

// SAFETY: OnceLock<T> drops its inner value T when dropped.
#[cfg(feature = "std")]
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for std::sync::OnceLock<T> {}
// SAFETY: std::sync::mpsc::Sender closes the channel on drop without blocking.
#[cfg(feature = "std")]
unsafe impl<T: Send + 'static> RcuDroppable for std::sync::mpsc::Sender<T> {}
// SAFETY: std::sync::mpsc::SyncSender closes the channel on drop without blocking.
#[cfg(feature = "std")]
unsafe impl<T: Send + 'static> RcuDroppable for std::sync::mpsc::SyncSender<T> {}
// SAFETY: HashMap deallocates table memory and drops keys, values, and hasher, all of which are RcuDroppable.
#[cfg(feature = "std")]
unsafe impl<K: RcuDroppable, V: RcuDroppable, S: RcuDroppable> RcuDroppable
    for std::collections::HashMap<K, V, S>
{
}

// Range types
// SAFETY: Range only drops its start and end fields of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for core::ops::Range<T> {}
// SAFETY: RangeFrom only drops its start field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for core::ops::RangeFrom<T> {}
// SAFETY: RangeInclusive only drops its start and end fields of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for core::ops::RangeInclusive<T> {}
// SAFETY: RangeTo only drops its end field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for core::ops::RangeTo<T> {}
// SAFETY: RangeToInclusive only drops its end field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for core::ops::RangeToInclusive<T> {}

// Tuples
macro_rules! impl_tuple_rcu_droppable {
    ($($T:ident),+) => {
        // SAFETY: A tuple only drops its constituent elements, all of which are RcuDroppable.
        unsafe impl<$($T: RcuDroppable),+> RcuDroppable for ($($T,)+) {}
    };
}

impl_tuple_rcu_droppable!(A);
impl_tuple_rcu_droppable!(A, B);
impl_tuple_rcu_droppable!(A, B, C);
impl_tuple_rcu_droppable!(A, B, C, D);

// SAFETY: `lock_api::Mutex<R, T>` has no custom Drop logic; dropping it only drops the raw
// lock `R` and the inner value `T`, both of which are `RcuDroppable`.
unsafe impl<R: RcuDroppable + lock_api::RawMutex, T: ?Sized + RcuDroppable> RcuDroppable
    for lock_api::Mutex<R, T>
{
}

// SAFETY: `lock_api::RwLock<R, T>` has no custom Drop logic; dropping it only drops the raw
// lock `R` and the inner value `T`, both of which are `RcuDroppable`.
unsafe impl<R: RcuDroppable + lock_api::RawRwLock, T: ?Sized + RcuDroppable> RcuDroppable
    for lock_api::RwLock<R, T>
{
}

// Zircon types. `zx` is only a dependency when targeting Fuchsia.
#[cfg(target_os = "fuchsia")]
impl_rcu_droppable!(zx::BootTimeline, zx::Koid, zx::MonotonicTimeline, zx::Rights);

// SAFETY: Dropping `zx::Vmo` closes the underlying Zircon kernel handle, which is non-blocking
// and has no thread affinity.
#[cfg(target_os = "fuchsia")]
unsafe impl RcuDroppable for zx::Vmo {}

// SAFETY: Dropping `zx::Profile` closes the underlying Zircon kernel handle, which is non-blocking
// and has no thread affinity.
#[cfg(target_os = "fuchsia")]
unsafe impl RcuDroppable for zx::Profile {}

// SAFETY: Dropping `zx::Clock` closes the underlying Zircon kernel handle, which is non-blocking
// and has no thread affinity. The timeline parameters `T` and `U` are phantom markers with no
// drop side effects.
#[cfg(target_os = "fuchsia")]
unsafe impl<T: Send + 'static, U: Send + 'static> RcuDroppable for zx::Clock<T, U> {}

// SAFETY: `zx::Instant` is a `Copy` wrapper around an integer, so it has no drop side effects.
// The timeline parameters `T` and `U` are phantom markers.
#[cfg(target_os = "fuchsia")]
unsafe impl<T: Send + 'static, U: Send + 'static> RcuDroppable for zx::Instant<T, U> {}

// SAFETY: `zx::Duration` is a `Copy` wrapper around an integer, so it has no drop side effects.
// The timeline parameters `T` and `U` are phantom markers.
#[cfg(target_os = "fuchsia")]
unsafe impl<T: Send + 'static, U: Send + 'static> RcuDroppable for zx::Duration<T, U> {}

#[cfg(test)]
mod tests {
    use crate::RcuDroppable;

    fn assert_rcu_droppable<T: RcuDroppable>() {}

    #[derive(RcuDroppable)]
    struct UnitStruct;

    #[test]
    fn test_rcu_droppable_trait() {
        assert_rcu_droppable::<UnitStruct>();
        assert_rcu_droppable::<bstr::BString>();
    }
}
