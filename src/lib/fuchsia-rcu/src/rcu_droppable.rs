// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use fuchsia_rcu_traits::RcuDroppable;

// SAFETY: RcuPtr has a trivial drop and does not own or drop the pointee.
unsafe impl<T: 'static> RcuDroppable for crate::rcu_ptr::RcuPtr<T> {}
// SAFETY: RcuDroppableArc drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuDroppableArc<T> {}
// SAFETY: RcuBox drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuBox<T> {}
// SAFETY: RcuOptionArc drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuOptionArc<T> {}
// SAFETY: RcuOptionBox drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuOptionBox<T> {}
// SAFETY: RcuWeak holds a non owning reference and won't deallocate T on rcu_drop.
unsafe impl<T: Send + Sync + 'static> RcuDroppable for crate::RcuWeak<T> {}
// SAFETY: rcu_drop of an RcuArc will rcu_drop T which is safe since T: RcuDroppable.
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuArc<T> {}

// The types we define are dead code as we don't use them. We only care that the macro parses them
// successfully.
#[expect(dead_code)]
#[cfg(test)]
mod tests {
    use crate::RcuDroppable;
    use std::sync::Arc;

    #[derive(RcuDroppable)]
    struct UnitStruct;

    #[derive(RcuDroppable)]
    struct TupleStruct(u32, String);

    #[derive(RcuDroppable)]
    struct NamedStruct<T> {
        foo: T,
        bar: Vec<u8>,
    }

    #[derive(RcuDroppable)]
    enum TestEnum<T> {
        A(T),
        B { val: String },
        C,
    }

    #[derive(RcuDroppable)]
    struct ComplexStruct {
        atomic: std::sync::atomic::AtomicUsize,
        range: std::ops::Range<usize>,
        non_zero: std::num::NonZeroU32,
        mutex: fuchsia_sync::Mutex<u32>,
        rwlock: fuchsia_sync::RwLock<String>,
        rcu_arc: crate::RcuArc<u32>,
        weak: std::sync::Weak<u32>,
        hash_map: std::collections::HashMap<u32, String>,
        vmo: zx::Vmo,
        clock: zx::Clock<zx::BootTimeline, zx::MonotonicTimeline>,
    }

    fn assert_rcu_droppable<T: super::RcuDroppable>() {}

    #[test]
    fn test_derive_rcu_droppable() {
        assert_rcu_droppable::<UnitStruct>();
        assert_rcu_droppable::<TupleStruct>();
        assert_rcu_droppable::<NamedStruct<u32>>();
        assert_rcu_droppable::<TestEnum<u64>>();
        assert_rcu_droppable::<ComplexStruct>();
        assert_rcu_droppable::<fuchsia_sync::Completion>();
        assert_rcu_droppable::<fuchsia_sync::Condvar>();
        assert_rcu_droppable::<zx::BootDuration>();
        assert_rcu_droppable::<zx::BootInstant>();
        assert_rcu_droppable::<zx::BootTimeline>();
        assert_rcu_droppable::<zx::Clock>();
        assert_rcu_droppable::<zx::Koid>();
        assert_rcu_droppable::<zx::MonotonicDuration>();
        assert_rcu_droppable::<zx::MonotonicDurationTicks>();
        assert_rcu_droppable::<zx::MonotonicInstant>();
        assert_rcu_droppable::<zx::MonotonicTicks>();
        assert_rcu_droppable::<zx::MonotonicTimeline>();
        assert_rcu_droppable::<zx::Profile>();
        assert_rcu_droppable::<zx::Rights>();
        assert_rcu_droppable::<zx::Vmo>();
    }

    #[test]
    fn test_rcu_drop_handles() {
        // Verify zx::Vmo can be stored and dropped in RcuOptionArc.
        let vmo = zx::Vmo::create(0).expect("failed to create VMO");
        let rcu_vmo = crate::RcuOptionArc::new(Some(Arc::new(vmo)));
        rcu_vmo.update(None);
        crate::rcu_run_callbacks();
    }
}
