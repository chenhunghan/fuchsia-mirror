// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AtomicMonotonicInstant, LockDepGuard, LockDepWriteGuard, MappedLockDepGuard,
    MappedLockDepWriteGuard, MappedMutexGuard, MutexGuard, RwLockWriteGuard, RwSeqLockGuard,
};
use fuchsia_rcu::{
    RcuArc, RcuBox, RcuDroppable, RcuDroppableArc, RcuOptionArc, RcuOptionBox, RcuReadGuard,
    RcuReadScope, RcuWeak,
};
use fuchsia_rcu_collections::rcu_array::RcuArray;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16,
    AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Weak};

/// A wrapper around an RCU or atomic primitive `T` whose updates are serialized by an external
/// lock guarding state `S`, while reads remain lock-free.
///
/// Unlike [`crate::RcuLock`] and [`crate::DynamicRcuLock`], `LockGuarded` has zero memory
/// overhead: it does not embed its own mutex. Instead, writers must present a reference to a write
/// guard for `S` (any type implementing [`WriteProof<S>`]) to [`LockGuarded::write`] in order to
/// obtain a [`LockGuardedWrite`] handle that exposes the underlying mutation API on `T`.
///
/// Readers call the read methods exposed directly on `LockGuarded` without acquiring the lock.
#[repr(transparent)]
pub struct LockGuarded<T, S> {
    pub(crate) inner: T,
    _state: PhantomData<fn(&mut S)>,
}

// SAFETY: `LockGuarded<T, S>` only contains `T` and `PhantomData<fn(&mut S)>`, so dropping it
// only drops `T`.
unsafe impl<T: RcuDroppable, S: 'static> RcuDroppable for LockGuarded<T, S> {}

/// A write handle to the value inside a [`LockGuarded`], obtained by presenting a write guard for
/// `S` to [`LockGuarded::write`].
pub struct LockGuardedWrite<'a, T> {
    inner: &'a T,
    _not_send_or_sync: PhantomData<*const ()>,
}

/// A proof that the caller holds exclusive write access to the state `S` that guards a
/// [`LockGuarded<T, S>`].
///
/// This is a safe trait. It enforces lock discipline for writers, but memory safety must not
/// depend on it, because a `WriteProof<S>` does not prove that the guard belongs to the specific
/// [`LockGuarded`] instance being updated. Write operations whose soundness requires exclusive
/// access (such as [`RcuArray::ensure_at_least`], reachable through [`LockGuardedWrite`]'s
/// `Deref`) are `unsafe` instead.
pub trait WriteProof<S> {
    /// Debug-asserts that this write guard belongs to the same struct instance that owns the
    /// [`LockGuarded`] field at `field_ptr`, when the guard carries a pointer to its base struct.
    #[inline(always)]
    fn assert_owns(&self, _field_ptr: *const ()) {}
}

impl<S> WriteProof<S> for LockDepGuard<'_, S> {}
impl<S> WriteProof<S> for MappedLockDepGuard<'_, S> {}
impl<S> WriteProof<S> for LockDepWriteGuard<'_, S> {}
impl<S> WriteProof<S> for MappedLockDepWriteGuard<'_, S> {}
impl<S> WriteProof<S> for MutexGuard<'_, S> {}
impl<S> WriteProof<S> for MappedMutexGuard<'_, S> {}
impl<S> WriteProof<S> for RwLockWriteGuard<'_, S> {}
impl<S, G: DerefMut<Target = S>> WriteProof<S> for RwSeqLockGuard<'_, G> {}

impl<T, S> LockGuarded<T, S> {
    /// Creates a new `LockGuarded` wrapping `inner`.
    pub const fn new(inner: T) -> Self {
        Self { inner, _state: PhantomData }
    }

    /// Grants writer access to the underlying value given a reference to a write guard for `S`.
    #[inline(always)]
    #[track_caller]
    pub fn write<'a, G: WriteProof<S> + ?Sized>(&'a self, guard: &'a G) -> LockGuardedWrite<'a, T> {
        guard.assert_owns(self as *const Self as *const ());
        LockGuardedWrite { inner: &self.inner, _not_send_or_sync: PhantomData }
    }

    /// Returns a mutable reference to the underlying value when holding exclusive ownership of
    /// `self`.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consumes the `LockGuarded`, returning the underlying value.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Default, S> Default for LockGuarded<T, S> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T, S> From<T> for LockGuarded<T, S> {
    fn from(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: std::fmt::Debug, S> std::fmt::Debug for LockGuarded<T, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuarded")
            .field("inner", &self.inner)
            .field("state", &std::any::type_name::<S>())
            .finish()
    }
}

impl<T> Deref for LockGuardedWrite<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for LockGuardedWrite<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuardedWrite").field("inner", &self.inner).finish()
    }
}

/// A trait for atomic types that support lock-free reads via [`LockGuarded`].
pub trait AtomicRead {
    /// The value type returned by [`AtomicRead::load`].
    type Value;

    /// Loads a value from the atomic primitive.
    fn load(&self, order: Ordering) -> Self::Value;
}

macro_rules! impl_atomic_read {
    ($($atomic:ty => $val:ty),* $(,)?) => {
        $(
            impl AtomicRead for $atomic {
                type Value = $val;

                #[inline(always)]
                fn load(&self, order: Ordering) -> Self::Value {
                    <$atomic>::load(self, order)
                }
            }
        )*
    };
}

impl_atomic_read! {
    AtomicBool => bool,
    AtomicU8 => u8,
    AtomicU16 => u16,
    AtomicU32 => u32,
    AtomicU64 => u64,
    AtomicUsize => usize,
    AtomicI8 => i8,
    AtomicI16 => i16,
    AtomicI32 => i32,
    AtomicI64 => i64,
    AtomicIsize => isize,
    AtomicMonotonicInstant => zx::MonotonicInstant,
}

impl<T: AtomicRead, S> LockGuarded<T, S> {
    /// Loads a value from the underlying atomic without acquiring the write lock.
    #[inline(always)]
    pub fn load(&self, order: Ordering) -> T::Value {
        self.inner.load(order)
    }
}

impl<T: Send + Sync + 'static, S> LockGuarded<RcuArc<T>, S> {
    /// Reads the contents of the underlying [`RcuArc`] without acquiring the update lock.
    ///
    /// Returns `None` if the wrapped `Arc` is `None` or is no longer valid.
    pub fn upgrade(&self) -> Option<Arc<T>> {
        self.inner.upgrade()
    }

    /// Returns `true` if the underlying [`RcuArc`] currently contains a value.
    pub fn is_some(&self) -> bool {
        self.inner.is_some()
    }

    /// Returns `true` if the underlying [`RcuArc`] does not contain a value.
    pub fn is_none(&self) -> bool {
        self.inner.is_none()
    }
}

impl<T: RcuDroppable + Sync, S> LockGuarded<RcuArray<T>, S> {
    /// Returns a reference to the element at `index`, or `None` if `index` is out of bounds.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, index: usize) -> Option<&'a T> {
        self.inner.get(scope, index)
    }

    /// Returns a slice containing the entire array bound to `scope`.
    pub fn as_slice<'a>(&self, scope: &'a RcuReadScope) -> &'a [T] {
        self.inner.as_slice(scope)
    }
}

impl<T: RcuDroppable + Sync, S> LockGuarded<RcuBox<T>, S> {
    /// Reads the value of the wrapped [`RcuBox`] without acquiring the update lock.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.inner.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuBox`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.inner.as_ref(scope)
    }

    /// Returns a clone of the value of the wrapped [`RcuBox`].
    pub fn cloned(&self) -> T
    where
        T: Clone,
    {
        self.inner.cloned()
    }
}

impl<T: RcuDroppable + Sync, S> LockGuarded<RcuDroppableArc<T>, S> {
    /// Reads the value of the wrapped [`RcuDroppableArc`] without acquiring the update lock.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.inner.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuDroppableArc`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.inner.as_ref(scope)
    }

    /// Creates a new `Arc<T>` referencing the same object as the underlying [`RcuDroppableArc`].
    pub fn to_arc(&self) -> Arc<T> {
        self.inner.to_arc()
    }
}

impl<T: RcuDroppable + Sync, S> LockGuarded<RcuOptionArc<T>, S> {
    /// Reads the value of the wrapped [`RcuOptionArc`] without acquiring the update lock.
    pub fn read(&self) -> Option<RcuReadGuard<T>> {
        self.inner.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuOptionArc`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> Option<&'a T> {
        self.inner.as_ref(scope)
    }

    /// Creates a new `Option<Arc<T>>` referencing the same object as the underlying
    /// [`RcuOptionArc`].
    pub fn to_option_arc(&self) -> Option<Arc<T>> {
        self.inner.to_option_arc()
    }
}

impl<T: RcuDroppable + Sync, S> LockGuarded<RcuOptionBox<T>, S> {
    /// Reads the value of the wrapped [`RcuOptionBox`] without acquiring the update lock.
    pub fn read(&self) -> Option<RcuReadGuard<T>> {
        self.inner.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuOptionBox`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> Option<&'a T> {
        self.inner.as_ref(scope)
    }

    /// Returns `true` if the underlying [`RcuOptionBox`] currently contains a value.
    pub fn is_some(&self, scope: &RcuReadScope) -> bool {
        self.inner.is_some(scope)
    }

    /// Returns `true` if the underlying [`RcuOptionBox`] does not contain a value.
    pub fn is_none(&self, scope: &RcuReadScope) -> bool {
        self.inner.is_none(scope)
    }

    /// Returns a clone of the value of the wrapped [`RcuOptionBox`].
    pub fn cloned(&self) -> Option<T>
    where
        T: Clone,
    {
        self.inner.cloned()
    }
}

impl<T: Send + Sync + 'static, S> LockGuarded<RcuWeak<T>, S> {
    /// Attempts to upgrade the wrapped [`Weak`] pointer to an [`Arc`] without acquiring the update
    /// lock.
    pub fn upgrade(&self) -> Option<Arc<T>> {
        self.inner.upgrade()
    }

    /// Creates a new [`Weak`] pointer to the object referenced by the underlying [`RcuWeak`].
    pub fn to_weak(&self) -> Weak<T> {
        self.inner.to_weak()
    }

    /// Returns `true` if the wrapped weak pointer points to the same allocation as `other`.
    pub fn ptr_eq(&self, other: &Weak<T>) -> bool {
        self.inner.ptr_eq(other)
    }

    /// Returns the number of strong (`Arc`) pointers pointing to the allocation.
    pub fn strong_count(&self, scope: &RcuReadScope) -> usize {
        self.inner.strong_count(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LockDepMutex, LockDepRwLock, RwSeqLock, lock_ordering};
    use fuchsia_rcu::rcu_run_callbacks;

    lock_ordering! {
        Unlocked => LevelA,
    }

    fn assert_rcu_droppable<T: RcuDroppable>() {}

    #[::fuchsia::test]
    fn test_lock_guarded_droppable() {
        struct NonDroppableState(PhantomData<*mut ()>);

        assert_rcu_droppable::<LockGuarded<RcuArray<u32>, NonDroppableState>>();
    }

    #[::fuchsia::test]
    fn test_lock_guarded_rcu_and_atomic() {
        struct State {
            generation: usize,
        }

        let state: LockDepRwLock<State, LevelA> = LockDepRwLock::new(State { generation: 0 });
        let rcu_field: LockGuarded<RcuArc<u32>, State> =
            LockGuarded::from(RcuArc::new(Some(Arc::new(1))));
        let array_field: LockGuarded<RcuArray<u32>, State> = LockGuarded::default();
        let atomic_field: LockGuarded<AtomicUsize, State> = LockGuarded::new(AtomicUsize::new(10));

        assert!(rcu_field.is_some());
        assert!(!rcu_field.is_none());
        assert_eq!(*rcu_field.upgrade().unwrap(), 1);
        assert_eq!(atomic_field.load(Ordering::Relaxed), 10);

        {
            let mut write_guard = state.write();
            write_guard.generation += 1;
            let rcu_write = rcu_field.write(&write_guard);
            assert!(!format!("{rcu_write:?}").is_empty());
            rcu_write.update(Some(Arc::new(2)));
            // SAFETY: `write_guard` is the only write guard for `state`, which serializes updates
            // to `array_field`, and it is not shared with other threads.
            unsafe { array_field.write(&write_guard).ensure_at_least(3) };
            atomic_field.write(&write_guard).store(20, Ordering::Relaxed);
        }

        assert_eq!(state.read().generation, 1);
        assert_eq!(*rcu_field.upgrade().unwrap(), 2);
        assert_eq!(atomic_field.load(Ordering::Relaxed), 20);
        assert!(!format!("{rcu_field:?}").is_empty());
        {
            let scope = RcuReadScope::new();
            assert!(array_field.as_slice(&scope).len() >= 3);
        }

        let seq_state: RwSeqLock<LockDepMutex<State, LevelA>> =
            RwSeqLock::new(LockDepMutex::new(State { generation: 0 }));
        {
            let seq_guard = seq_state.lock();
            atomic_field.write(&seq_guard).store(30, Ordering::Relaxed);
        }
        assert_eq!(atomic_field.load(Ordering::Relaxed), 30);

        let mut owned: LockGuarded<AtomicUsize, State> = LockGuarded::default();
        *owned.get_mut().get_mut() = 42;
        assert_eq!(owned.into_inner().into_inner(), 42);

        rcu_run_callbacks();
    }
}
