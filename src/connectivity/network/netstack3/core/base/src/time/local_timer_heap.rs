// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A local timer heap for use in netstack3 core.

use alloc::collections::{BinaryHeap, binary_heap};
use core::hash::Hash;
use core::time::Duration;

use netstack3_hashmap::{HashMap, hash_map};

use crate::{CoreTimerContext, Instant, InstantBindingsTypes, TimerBindingsTypes, TimerContext};

/// A local timer heap that keeps timers for core modules.
///
/// `LocalTimerHeap` manages its wakeups through a [`TimerContext`].
///
/// `K` is the key that timers are keyed on. `V` is optional sidecar data to be
/// kept with each timer.
///
/// Note that to provide fast timer deletion, `LocalTimerHeap` requires `K` to
/// be `Clone` and the implementation assumes that the clone is cheap.
#[derive(Debug)]
pub struct LocalTimerHeap<K, V, BT: TimerBindingsTypes + InstantBindingsTypes> {
    next_wakeup: BT::Timer,
    heap: KeyedHeap<K, V, BT::Instant>,
}

impl<K, V, BC> LocalTimerHeap<K, V, BC>
where
    K: Hash + Eq + Clone,
    BC: TimerContext,
{
    /// Creates a new `LocalTimerHeap` with wakeup dispatch ID `dispatch_id`.
    pub fn new(bindings_ctx: &mut BC, dispatch_id: BC::DispatchId) -> Self {
        let next_wakeup = bindings_ctx.new_timer(dispatch_id);
        Self { next_wakeup, heap: KeyedHeap::new() }
    }

    /// Like [`new`] but uses `CC` to covert the `dispatch_id` to match the type required by `BC`.
    pub fn new_with_context<D, CC: CoreTimerContext<D, BC>>(
        bindings_ctx: &mut BC,
        dispatch_id: D,
    ) -> Self {
        Self::new(bindings_ctx, CC::convert_timer(dispatch_id))
    }

    /// Schedules `timer` with `value` at or after `at`.
    ///
    /// If `timer` was already scheduled, returns the previously scheduled
    /// instant and the associated value.
    pub fn schedule_instant(
        &mut self,
        bindings_ctx: &mut BC,
        timer: K,
        value: V,
        at: BC::Instant,
    ) -> Option<(BC::Instant, V)> {
        let (prev_value, timer_change) = self.heap.schedule(timer, value, at);
        self.apply_timer_change(bindings_ctx, timer_change);
        prev_value
    }

    /// Like [`schedule_instant`] but does the instant math from current time.
    ///
    /// # Panics
    ///
    /// Panics if the current `BC::Instant` cannot be represented by adding
    /// duration `after`.
    pub fn schedule_after(
        &mut self,
        bindings_ctx: &mut BC,
        timer: K,
        value: V,
        after: Duration,
    ) -> Option<(BC::Instant, V)> {
        let time = bindings_ctx.now().checked_add(after).unwrap();
        self.schedule_instant(bindings_ctx, timer, value, time)
    }

    /// Pops an expired timer from the heap, if any.
    pub fn pop(&mut self, bindings_ctx: &mut BC) -> Option<(K, V)> {
        let (popped, timer_change) = self.heap.pop_if(|t| t <= bindings_ctx.now());
        self.apply_timer_change(bindings_ctx, timer_change);
        popped
    }

    /// Returns the scheduled instant and associated value for `timer`, if it's
    /// scheduled.
    pub fn get(&self, timer: &K) -> Option<(BC::Instant, &V)> {
        self.heap.map.get(timer).map(|MapEntry { time, value, synced_with_heap: _ }| (*time, value))
    }

    /// Cancels `timer`, returning the scheduled instant and associated value if
    /// any.
    pub fn cancel(&mut self, bindings_ctx: &mut BC, timer: &K) -> Option<(BC::Instant, V)> {
        let (scheduled, timer_change) = self.heap.cancel(timer);
        self.apply_timer_change(bindings_ctx, timer_change);
        scheduled
    }

    /// Gets an iterator over the installed timers.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V, &BC::Instant)> {
        self.heap
            .map
            .iter()
            .map(|(k, MapEntry { time, value, synced_with_heap: _ })| (k, value, time))
    }

    fn apply_timer_change(
        &mut self,
        bindings_ctx: &mut BC,
        timer_change: Option<TimerChange<BC::Instant>>,
    ) {
        let _: Option<BC::Instant> = match timer_change {
            Some(TimerChange::Scheduled(time)) => {
                bindings_ctx.schedule_timer_instant(time, &mut self.next_wakeup)
            }
            Some(TimerChange::Canceled) => bindings_ctx.cancel_timer(&mut self.next_wakeup),
            None => None,
        };
    }

    /// Removes all timers.
    pub fn clear(&mut self, bindings_ctx: &mut BC) {
        let Self { next_wakeup, heap } = self;
        heap.clear();
        let _: Option<BC::Instant> = bindings_ctx.cancel_timer(next_wakeup);
    }

    /// Returns true if there are no timers installed.
    pub fn is_empty(&self) -> bool {
        self.heap.map.is_empty()
    }
}

/// Describes a change to the next wakeup time in [`KeyedHeap`].
enum TimerChange<T> {
    Canceled,
    Scheduled(T),
}

/// A timer heap that is keyed on `K`.
///
/// This type is used to support [`LocalTimerHeap`].
///
/// It is self-healing: all operations maintain the invariant that the top of
/// `heap` (if any) is a valid entry synchronized with `map`.
#[derive(Debug)]
struct KeyedHeap<K, V, T> {
    // Implementation note: The map is the source of truth for the desired
    // firing time for a timer `K`. The heap has a copy of the scheduled time
    // that is *always* compared to the map before firing the timer.
    //
    // That allows timers to be rescheduled to a later time during `heal`, which
    // reduces memory utilization for the heap by avoiding the stale entry.
    map: HashMap<K, MapEntry<T, V>>,
    heap: BinaryHeap<HeapEntry<T, K>>,
}

/// The lower bound for heap garbage collection.
const GC_MIN_HEAP_SIZE: usize = 20;

impl<K: Hash + Eq + Clone, V, T: Instant> KeyedHeap<K, V, T> {
    fn new() -> Self {
        Self { map: HashMap::new(), heap: BinaryHeap::new() }
    }

    /// Garbage collects stale entries from `heap`.
    ///
    /// Bounds the heap size to 2 times the map size (or `GC_MIN_HEAP_SIZE` for
    /// small heaps). Returns `true` if garbage collection was performed.
    fn maybe_gc(&mut self) -> bool {
        let Self { map, heap } = self;
        let threshold = GC_MIN_HEAP_SIZE.max(map.len() * 2);
        if heap.len() > threshold {
            // NB: Rebuilding the heap from scratch is expected to be more
            // efficient than removing the stale entries. At least half the
            // heap is stale.
            heap.clear();
            heap.extend(map.iter_mut().map(
                |(key, MapEntry { time, value: _, synced_with_heap })| {
                    *synced_with_heap = true;
                    HeapEntry { time: *time, key: key.clone() }
                },
            ));
            true
        } else {
            false
        }
    }

    /// Schedules `key` with associated `value` at time `at`.
    ///
    /// Returns the previously associated value and firing time for `key` +
    /// a [`TimerChange`] if the next timer to fire changed with this operation.
    fn schedule(&mut self, key: K, value: V, at: T) -> (Option<(T, V)>, Option<TimerChange<T>>) {
        let Self { map, heap } = self;
        let was_front =
            heap.peek().is_some_and(|HeapEntry { time: _, key: top_key }| top_key == &key);
        let is_new_front =
            heap.peek().map(|HeapEntry { time: top_time, key: _ }| at <= *top_time).unwrap_or(true);
        let (heap_entry, prev) = match map.entry(key) {
            hash_map::Entry::Occupied(mut o) => {
                let MapEntry { time: prev_time, value: _, synced_with_heap } = o.get();
                // Only create a new `HeapEntry` if the already scheduled time
                // is later than the new time. The new `MapEntry` is synced
                // with the heap if we created a new `HeapEntry` or if the two
                // times match and the old `MapEntry` was synced with the heap.
                let (synced_with_heap, heap_entry) = match at.cmp(prev_time) {
                    core::cmp::Ordering::Less => {
                        (true, Some(HeapEntry { time: at, key: o.key().clone() }))
                    }
                    core::cmp::Ordering::Equal => (*synced_with_heap, None),
                    core::cmp::Ordering::Greater => (false, None),
                };
                let MapEntry { time, value, synced_with_heap: _ } =
                    o.insert(MapEntry { time: at, value, synced_with_heap });
                (heap_entry, Some((time, value)))
            }
            hash_map::Entry::Vacant(v) => {
                let heap_entry = Some(HeapEntry { time: at, key: v.key().clone() });
                let _: &mut MapEntry<_, _> =
                    v.insert(MapEntry { time: at, value, synced_with_heap: true });
                (heap_entry, None)
            }
        };
        let did_gc = match heap_entry {
            Some(heap_entry) => {
                heap.push(heap_entry);
                self.maybe_gc()
            }
            None => false,
        };
        if was_front && !is_new_front {
            // NB: If GC ran, the heap is already healed.
            let new_top = if did_gc {
                self.heap.peek().map(|HeapEntry { time, key: _ }| *time)
            } else {
                self.heal()
            }
            .expect("heap cannot be empty after a `schedule` operation");
            return (prev, Some(TimerChange::Scheduled(new_top)));
        }
        (prev, is_new_front.then_some(TimerChange::Scheduled(at)))
    }

    /// Cancels the timer with `key`.
    ///
    /// Returns the scheduled instant and value for `key` if it was scheduled +
    /// a [`TimerChange`] if the next timer to fire changed with this operation.
    fn cancel(&mut self, key: &K) -> (Option<(T, V)>, Option<TimerChange<T>>) {
        let Self { heap, map } = self;
        let Some(MapEntry { time, value, synced_with_heap: _ }) = map.remove(key) else {
            return (None, None);
        };
        // The front of the heap will be changed if we're cancelling the top.
        let was_front = heap.peek().is_some_and(|HeapEntry { time: _, key: top }| key == top);
        let did_gc = self.maybe_gc();
        let timer_change = was_front.then(|| {
            // NB: If GC ran, the heap is already healed.
            let new_top = if did_gc {
                self.heap.peek().map(|HeapEntry { time, key: _ }| *time)
            } else {
                self.heal()
            };
            match new_top {
                Some(new_top) => TimerChange::Scheduled(new_top),
                None => TimerChange::Canceled,
            }
        });
        (Some((time, value)), timer_change)
    }

    /// Pops the first valid entry if `f` returns `true`.
    ///
    /// Returns the popped value if one is found *and* `f` returns true + a
    /// [`TimerChange`] if the next timer to fire changed with this operation.
    ///
    /// NB: This API is a bit wonky, but unfortunately we can't seem to be able
    /// to express a type that would be equivalent to `BinaryHeap`'s `PeekMut`.
    /// This is the next best thing.
    fn pop_if<F: FnOnce(T) -> bool>(&mut self, f: F) -> (Option<(K, V)>, Option<TimerChange<T>>) {
        let Self { heap, map } = self;
        let Some(peek_mut) = heap.peek_mut() else {
            return (None, None);
        };
        if !f(peek_mut.time) {
            return (None, None);
        }
        let HeapEntry { time: heap_time, key } = binary_heap::PeekMut::pop(peek_mut);
        let MapEntry { time: scheduled_for, value, synced_with_heap } =
            map.remove(&key).expect("top of heap must be present in map");
        // NB: `KeyedHeap` is self-healing, so ensure the top of the map is
        // always a valid entry.
        debug_assert_eq!(heap_time, scheduled_for);
        debug_assert!(synced_with_heap);
        let timer_change = match self.heal() {
            Some(new_top) => TimerChange::Scheduled(new_top),
            None => TimerChange::Canceled,
        };
        (Some((key, value)), Some(timer_change))
    }

    /// Heals the heap of stale entries, returning the firing time of the top
    /// valid entry, if any.
    fn heal(&mut self) -> Option<T> {
        let Self { heap, map } = self;
        loop {
            let peek_mut = heap.peek_mut()?;
            let HeapEntry { time: heap_time, key } = &*peek_mut;
            // Always check the map state for the given key, since it's the
            // source of truth for desired firing time.

            // NB: We assume here that the key is cheaply cloned and that
            // cloning it is faster than possibly hashing it more than once.
            match map.entry(key.clone()) {
                hash_map::Entry::Vacant(_) => {
                    // This `HeapEntry` is stale. This may happen if either
                    //   1) the original timer was canceled, or
                    //   2) the original timer was rescheduled to an earlier
                    //      time, and has since fired.
                    // Pop and continue looking.
                    let _: HeapEntry<_, _> = binary_heap::PeekMut::pop(peek_mut);
                }
                hash_map::Entry::Occupied(mut map_entry) => {
                    let MapEntry { time: scheduled_for, value: _, synced_with_heap } =
                        map_entry.get_mut();

                    match heap_time.cmp(scheduled_for) {
                        core::cmp::Ordering::Equal => {
                            // Map and heap agree on firing time, this is the top of
                            // the heap.
                            *synced_with_heap = true;
                            return Some(*scheduled_for);
                        }
                        core::cmp::Ordering::Less => {
                            // When rescheduling a timer, we only touch the heap
                            // if rescheduling to an earlier time. In this case
                            // the map is telling us this is scheduled for
                            // later. We check `synced_with_heap` to determine
                            // whether this entry is stale and should be dropped
                            // or whether the entry must be resynchronized and
                            // put back in the heap.
                            let HeapEntry { time: _, key } = binary_heap::PeekMut::pop(peek_mut);
                            if !*synced_with_heap {
                                heap.push(HeapEntry { time: *scheduled_for, key });
                                *synced_with_heap = true;
                            }
                        }
                        core::cmp::Ordering::Greater => {
                            // Heap time greater than scheduled time is
                            // effectively unobservable because any earlier
                            // entry would've been popped from the heap already
                            // and thus the entry would be missing from the map.
                            // Even a catastrophic cancel => reschedule cycle
                            // can't make us observe this branch given the heap
                            // ordering properties.
                            unreachable!(
                                "observed heap time: {:?} later than the scheduled time {:?}",
                                heap_time, scheduled_for
                            );
                        }
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        let Self { map, heap } = self;
        map.clear();
        heap.clear();
    }
}

/// The entry kept in [`LocalTimerHeap`]'s internal hash map.
#[derive(Debug)]
struct MapEntry<T, V> {
    time: T,
    value: V,
    /// Whether `heap` contains a [`HeapEntry`] corresponding to this
    /// [`MapEntry`] with a matching `time`.
    synced_with_heap: bool,
}

/// A reusable struct to place a value and a timestamp in a [`BinaryHeap`].
///
/// Its `Ord` implementation is tuned to make [`BinaryHeap`] a min heap.
#[derive(Debug)]
struct HeapEntry<T, K> {
    time: T,
    key: K,
}

// Boilerplate to implement a heap entry.
impl<T: Instant, K> PartialEq for HeapEntry<T, K> {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl<T: Instant, K> Eq for HeapEntry<T, K> {}

impl<T: Instant, K> Ord for HeapEntry<T, K> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Note that we flip the argument order here to make the `BinaryHeap` a
        // min heap.
        Ord::cmp(&other.time, &self.time)
    }
}

impl<T: Instant, K> PartialOrd for HeapEntry<T, K> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(Ord::cmp(self, other))
    }
}

#[cfg(any(test, feature = "testutils"))]
mod testutil {
    use core::fmt::Debug;
    use core::ops::RangeBounds;

    use super::*;

    impl<K, V, BC> LocalTimerHeap<K, V, BC>
    where
        K: Hash + Eq + Clone + Debug,
        V: Debug + Eq + Clone + PartialEq,
        BC: TimerContext,
    {
        /// Asserts installed timers with an iterator of `(key, value, instant)`
        /// tuples.
        #[track_caller]
        pub fn assert_timers(&self, timers: impl IntoIterator<Item = (K, V, BC::Instant)>) {
            let wanted = timers.into_iter().map(|(k, v, i)| (k, (v, i))).collect::<HashMap<_, _>>();
            let actual = self
                .heap
                .map
                .iter()
                .map(|(k, MapEntry { value, time, synced_with_heap: _ })| {
                    (k.clone(), (value.clone(), *time))
                })
                .collect::<HashMap<_, _>>();
            assert_eq!(actual, wanted);
        }

        /// Like [`LocalTimerHeap::assert_timers`], but asserts based on a
        /// duration after `bindings_ctx.now()`.
        #[track_caller]
        pub fn assert_timers_after(
            &self,
            bindings_ctx: &mut BC,
            timers: impl IntoIterator<Item = (K, V, Duration)>,
        ) {
            let now = bindings_ctx.now();
            self.assert_timers(timers.into_iter().map(|(k, v, d)| (k, v, now.panicking_add(d))))
        }

        /// Assets that the next time to fire has `key` and `value`.
        #[track_caller]
        pub fn assert_top(&mut self, key: &K, value: &V) {
            // NB: `KeyedHeap` is self-healing, so ensure the top of the map is
            // always a valid entry.
            let top = self.heap.heap.peek().map(|HeapEntry { time: heap_time, key }| {
                let MapEntry { time: map_time, value, synced_with_heap } =
                    self.heap.map.get(key).expect("top of heap must be present in map");
                assert_eq!(heap_time, map_time);
                assert!(synced_with_heap);
                (key, value)
            });
            assert_eq!(top, Some((key, value)));
        }

        /// Asserts that the given timer is installed with an instant at the
        /// provided range.
        #[track_caller]
        pub fn assert_range<
            'a,
            R: RangeBounds<BC::Instant> + Debug,
            I: IntoIterator<Item = (&'a K, R)>,
        >(
            &'a self,
            expect: I,
        ) {
            for (timer, range) in expect {
                let time = self
                    .get(timer)
                    .map(|(t, _)| t)
                    .unwrap_or_else(|| panic!("timer {timer:?} not present"));
                assert!(range.contains(&time), "timer {timer:?} is at {time:?} not in {range:?}");
            }
        }

        /// Asserts that the given timer is installed with an instant at the
        /// provided range, returning its information.
        #[track_caller]
        pub fn assert_range_single<'a, R: RangeBounds<BC::Instant> + Debug>(
            &'a self,
            timer: &K,
            range: R,
        ) -> (BC::Instant, &'a V) {
            let (time, value) =
                self.get(timer).unwrap_or_else(|| panic!("timer {timer:?} not present"));
            assert!(range.contains(&time), "timer {timer:?} is at {time:?} not in {range:?}");
            (time, value)
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use crate::InstantContext;
    use crate::testutil::{FakeAtomicInstant, FakeInstant, FakeInstantCtx};

    use super::*;

    #[derive(Default)]
    struct FakeTimerCtx {
        instant: FakeInstantCtx,
    }

    impl InstantBindingsTypes for FakeTimerCtx {
        type Instant = FakeInstant;
        type AtomicInstant = FakeAtomicInstant;
    }

    impl InstantContext for FakeTimerCtx {
        fn now(&self) -> Self::Instant {
            self.instant.now()
        }
    }

    impl TimerBindingsTypes for FakeTimerCtx {
        type Timer = FakeTimer;
        type DispatchId = ();
        type UniqueTimerId = !;
    }

    impl TimerContext for FakeTimerCtx {
        fn new_timer(&mut self, (): Self::DispatchId) -> Self::Timer {
            FakeTimer::default()
        }

        fn schedule_timer_instant(
            &mut self,
            time: Self::Instant,
            timer: &mut Self::Timer,
        ) -> Option<Self::Instant> {
            timer.scheduled.replace(time)
        }

        fn cancel_timer(&mut self, timer: &mut Self::Timer) -> Option<Self::Instant> {
            timer.scheduled.take()
        }

        fn scheduled_instant(&self, timer: &mut Self::Timer) -> Option<Self::Instant> {
            timer.scheduled.clone()
        }

        fn unique_timer_id(&self, _: &Self::Timer) -> Self::UniqueTimerId {
            unimplemented!()
        }
    }

    #[derive(Default, Debug)]
    struct FakeTimer {
        scheduled: Option<FakeInstant>,
    }

    #[derive(Eq, PartialEq, Debug, Ord, PartialOrd, Copy, Clone, Hash)]
    struct TimerId(usize);

    type LocalTimerHeap = super::LocalTimerHeap<TimerId, (), FakeTimerCtx>;

    impl LocalTimerHeap {
        fn heap_len(&self) -> usize {
            self.heap.heap.len()
        }

        #[track_caller]
        fn assert_heap_entries<I: IntoIterator<Item = (FakeInstant, TimerId)>>(&self, i: I) {
            let mut want = i.into_iter().collect::<Vec<_>>();
            want.sort();
            let mut got = self
                .heap
                .heap
                .iter()
                .map(|HeapEntry { time, key }| (*time, *key))
                .collect::<Vec<_>>();
            got.sort();
            assert_eq!(got, want);
        }

        #[track_caller]
        fn assert_map_entries<I: IntoIterator<Item = (FakeInstant, TimerId)>>(&self, i: I) {
            let want = i.into_iter().map(|(t, k)| (k, t)).collect::<HashMap<_, _>>();
            let got = self
                .heap
                .map
                .iter()
                .map(|(k, MapEntry { time, value: (), synced_with_heap: _ })| (*k, *time))
                .collect::<HashMap<_, _>>();
            assert_eq!(got, want);
        }
    }

    const TIMER1: TimerId = TimerId(1);
    const TIMER2: TimerId = TimerId(2);
    const TIMER3: TimerId = TimerId(3);

    const T1: FakeInstant = FakeInstant { offset: Duration::from_secs(1) };
    const T2: FakeInstant = FakeInstant { offset: Duration::from_secs(2) };
    const T3: FakeInstant = FakeInstant { offset: Duration::from_secs(3) };
    const T4: FakeInstant = FakeInstant { offset: Duration::from_secs(4) };
    const T5: FakeInstant = FakeInstant { offset: Duration::from_secs(5) };

    #[test]
    fn schedule_instant() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.next_wakeup.scheduled, None);
        heap.assert_heap_entries([]);

        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T2), None);
        heap.assert_heap_entries([(T2, TIMER2)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));

        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));

        assert_eq!(heap.schedule_instant(&mut ctx, TIMER3, (), T3), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));
    }

    #[test]
    fn schedule_after() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.next_wakeup.scheduled, None);
        let long_duration = Duration::from_secs(5);
        let short_duration = Duration::from_secs(1);

        let long_instant = ctx.now().checked_add(long_duration).unwrap();
        let short_instant = ctx.now().checked_add(short_duration).unwrap();

        assert_eq!(heap.schedule_after(&mut ctx, TIMER1, (), long_duration), None);
        assert_eq!(heap.next_wakeup.scheduled, Some(long_instant));
        heap.assert_heap_entries([(long_instant, TIMER1)]);
        heap.assert_map_entries([(long_instant, TIMER1)]);

        assert_eq!(
            heap.schedule_after(&mut ctx, TIMER1, (), short_duration),
            Some((long_instant, ()))
        );
        assert_eq!(heap.next_wakeup.scheduled, Some(short_instant));
        heap.assert_heap_entries([(short_instant, TIMER1), (long_instant, TIMER1)]);
        heap.assert_map_entries([(short_instant, TIMER1)]);
    }

    #[test]
    fn cancel() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T2), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER3, (), T3), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));

        assert_eq!(heap.cancel(&mut ctx, &TIMER1), Some((T1, ())));
        heap.assert_heap_entries([(T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));

        assert_eq!(heap.cancel(&mut ctx, &TIMER1), None);

        assert_eq!(heap.cancel(&mut ctx, &TIMER3), Some((T3, ())));
        // Timer3 is still in the heap, hasn't had a chance to cleanup.
        heap.assert_heap_entries([(T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T2, TIMER2)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));

        assert_eq!(heap.cancel(&mut ctx, &TIMER2), Some((T2, ())));
        heap.assert_heap_entries([]);
        heap.assert_map_entries([]);
        assert_eq!(heap.next_wakeup.scheduled, None);
    }

    #[test]
    fn pop() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T2), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER3, (), T3), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));

        assert_eq!(heap.pop(&mut ctx), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));

        ctx.instant.time = T1;
        assert_eq!(heap.pop(&mut ctx), Some((TIMER1, ())));
        heap.assert_heap_entries([(T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));
        assert_eq!(heap.pop(&mut ctx), None);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));

        ctx.instant.time = T3;
        assert_eq!(heap.pop(&mut ctx), Some((TIMER2, ())));
        heap.assert_heap_entries([(T3, TIMER3)]);
        heap.assert_map_entries([(T3, TIMER3)]);

        assert_eq!(heap.next_wakeup.scheduled, Some(T3));
        assert_eq!(heap.pop(&mut ctx), Some((TIMER3, ())));
        heap.assert_heap_entries([]);
        heap.assert_map_entries([]);
        assert_eq!(heap.next_wakeup.scheduled, None);

        assert_eq!(heap.pop(&mut ctx), None);
    }

    #[test]
    fn reschedule() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T2), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER3, (), T3), None);
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));

        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T4), Some((T2, ())));
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T1, TIMER1), (T4, TIMER2), (T3, TIMER3)]);

        ctx.instant.time = T4;
        // Popping TIMER1 makes the heap entry update.
        assert_eq!(heap.pop(&mut ctx), Some((TIMER1, ())));
        heap.assert_heap_entries([(T4, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T4, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T3));

        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T2), Some((T4, ())));
        heap.assert_heap_entries([(T2, TIMER2), (T4, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T2, TIMER2), (T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T2));

        assert_eq!(heap.pop(&mut ctx), Some((TIMER2, ())));
        // Still has stale TIMER2 entry.
        heap.assert_heap_entries([(T4, TIMER2), (T3, TIMER3)]);
        heap.assert_map_entries([(T3, TIMER3)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T3));

        assert_eq!(heap.pop(&mut ctx), Some((TIMER3, ())));
        heap.assert_heap_entries([]);
        heap.assert_map_entries([]);
        assert_eq!(heap.next_wakeup.scheduled, None);
        assert_eq!(heap.pop(&mut ctx), None);
    }

    #[test]
    fn reschedule_later_heals_stale_entries() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());

        // Schedule TIMER1 at T3, then reschedule earlier to T2 and T1.
        // All three entries are in `heap`.
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T3), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T2), Some((T3, ())));
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), Some((T2, ())));
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER1), (T3, TIMER1)]);
        heap.assert_map_entries([(T1, TIMER1)]);

        // Rescheduling TIMER1 to a later instant T4 heals all three earlier
        // entries from `heap` and replaces them with a single entry at T4.
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T4), Some((T1, ())));
        heap.assert_heap_entries([(T4, TIMER1)]);
        heap.assert_map_entries([(T4, TIMER1)]);
        assert_eq!(heap.next_wakeup.scheduled, Some(T4));
    }

    #[test]
    fn reschedule_later_heals_stale_entries_with_multiple_timers() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());

        // Schedule TIMER1 at T4, and TIMER2 at T3. Then reschedule TIMER1
        // earlier to T2 and T1.
        // All four entries are in `heap`.
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T4), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T3), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T2), Some((T4, ())));
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), Some((T2, ())));
        heap.assert_heap_entries([(T1, TIMER1), (T2, TIMER1), (T3, TIMER2), (T4, TIMER1)]);
        heap.assert_map_entries([(T1, TIMER1), (T3, TIMER2)]);

        // Rescheduling TIMER1 from T1 to T5 heals the entries up until the
        // next valid entry (T3, TIMER2). Stale entries after it are not healed.
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T5), Some((T1, ())));
        heap.assert_heap_entries([(T3, TIMER2), (T4, TIMER1), (T5, TIMER1)]);
        heap.assert_map_entries([(T3, TIMER2), (T5, TIMER1)]);

        // When TIMER2 fires at T3, `pop_if` heals the remaining stale entry
        // (T4, TIMER1).
        assert_eq!(heap.next_wakeup.scheduled, Some(T3));
        ctx.instant.time = T3;
        assert_eq!(heap.pop(&mut ctx), Some((TIMER2, ())));
        heap.assert_heap_entries([(T5, TIMER1)]);
        heap.assert_map_entries([(T5, TIMER1)]);
    }

    // Regression test for a bug where the timer heap would not reschedule the
    // next wakeup when it has two timers for the exact same instant at the top.
    #[test]
    fn multiple_timers_same_instant() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T1), None);
        assert_eq!(heap.next_wakeup.scheduled.take(), Some(T1));

        ctx.instant.time = T1;

        // Ordering is not guaranteed, just assert that we're getting timers.
        assert!(heap.pop(&mut ctx).is_some());
        assert_eq!(heap.next_wakeup.scheduled, Some(T1));
        assert!(heap.pop(&mut ctx).is_some());
        assert_eq!(heap.next_wakeup.scheduled, None);
        assert_eq!(heap.pop(&mut ctx), None);
    }

    #[test]
    fn clear() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), T1), None);
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER2, (), T1), None);
        heap.clear(&mut ctx);
        heap.assert_map_entries([]);
        assert_eq!(heap.next_wakeup.scheduled, None);
    }

    #[test]
    fn reschedule_single_timer_will_trigger_gc() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());

        let mut next_time = FakeInstant { offset: Duration::from_secs(1000) };
        assert_eq!(heap.schedule_instant(&mut ctx, TIMER1, (), next_time), None);

        // Reschedule the timer to earlier, which will generate stale entries.
        // The GC won't run until the `GC_MIN_HEAP_SIZE` threshold has been
        // exceeded.
        for i in 1..GC_MIN_HEAP_SIZE + 1 {
            let prev_time = next_time;
            next_time = next_time.checked_sub(Duration::from_secs(1)).unwrap();
            assert_eq!(
                heap.schedule_instant(&mut ctx, TIMER1, (), next_time),
                Some((prev_time, ()))
            );
            if i == GC_MIN_HEAP_SIZE {
                // Final Iteration: GC should have been triggered.
                assert_eq!(heap.heap_len(), 1);
            } else {
                // Intermediate Iteration: GC should not have been triggered.
                assert_eq!(heap.heap_len(), 1 + i);
            }
        }
    }

    #[test]
    fn reschedule_multiple_timers_will_trigger_gc() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());

        // Setup `GC_MIN_HEAP_SIZE` independent timers.
        let num_timers = GC_MIN_HEAP_SIZE;
        for i in 0..num_timers {
            assert_eq!(heap.schedule_instant(&mut ctx, TimerId(i), (), T3), None);
        }
        assert_eq!(heap.heap_len(), num_timers);

        // Reschedule every timer to earlier, which will generate stale entries.
        // The GC won't run because the `2*num_timers` threshold hasn't been
        // exceeded.
        for i in 0..num_timers {
            assert_eq!(heap.schedule_instant(&mut ctx, TimerId(i), (), T2), Some((T3, ())));
        }
        assert_eq!(heap.heap_len(), num_timers * 2);

        // Now that we're at the threshold, scheduling a timer again will
        // trigger the GC.
        assert_eq!(heap.schedule_instant(&mut ctx, TimerId(0), (), T1), Some((T2, ())));
        assert_eq!(heap.heap_len(), num_timers);
    }

    #[test]
    fn cancel_will_trigger_gc() {
        let mut ctx = FakeTimerCtx::default();
        let mut heap = LocalTimerHeap::new(&mut ctx, ());

        // Setup a single timer at T1, and then `GC_MIN_HEAP_SIZE` independent
        // timers at T2.
        let num_timers = GC_MIN_HEAP_SIZE;
        assert_eq!(heap.schedule_instant(&mut ctx, TimerId(num_timers), (), T1), None);
        for i in 0..num_timers {
            assert_eq!(heap.schedule_instant(&mut ctx, TimerId(i), (), T2), None);
        }
        assert_eq!(heap.heap_len(), num_timers + 1);

        // Cancel the timers at T2. Because of the existence of the timer at T1,
        // the heap's front doesn't change, and the canceled timers will remain
        // as stale entries.
        //
        // We should be able to cancel num_timers / 2 timers, before GC
        // exceeds the `2*num_timers` threshold and is triggered.
        let tipping_point = num_timers / 2;
        for i in 0..tipping_point {
            assert_eq!(heap.cancel(&mut ctx, &TimerId(i)), Some((T2, ())));
            assert_eq!(heap.heap_len(), num_timers + 1);
        }
        assert_eq!(heap.cancel(&mut ctx, &TimerId(tipping_point)), Some((T2, ())));
        assert_eq!(heap.heap_len(), num_timers - tipping_point);
    }
}
