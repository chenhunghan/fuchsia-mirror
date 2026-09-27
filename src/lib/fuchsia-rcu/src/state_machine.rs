// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic_stack::{AtomicListIterator, AtomicStack};
use crate::rcu_droppable::RcuDroppable;
use fuchsia_sync::{Completion, Mutex};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use std::thread_local;
use std::time::Duration;

#[cfg(feature = "rseq_backend")]
use crate::read_counters::RcuReadCounters;

type RcuCallback = Box<dyn FnOnce() + Send + Sync + 'static>;

struct RcuControlBlock {
    /// The generation counter.
    ///
    /// The generation counter is incremented whenever the state machine leaves the `Idle` state.
    generation: AtomicUsize,

    /// The read counters.
    ///
    /// Readers increment the counter for the generation that they are reading from. For example,
    /// if the `generation` is even, then readers increment the counter for the `read_counters[0]`.
    /// If the `generation` is odd, then readers increment the counter for the `read_counters[1]`.
    #[cfg(not(feature = "rseq_backend"))]
    read_counters: [AtomicUsize; 2],

    #[cfg(feature = "rseq_backend")]
    read_counters: RcuReadCounters,

    /// The chain of callbacks that are waiting to be run.
    ///
    /// Writers add callbacks to this chain after writing to the object. The callbacks are run when
    /// all currently in-flight read operations have completed.
    callback_chain: AtomicStack<RcuCallback>,

    /// The futex used to put the background advancer thread to sleep when there are no callbacks.
    advancer_thread_state: zx::Futex,

    /// Callbacks that are ready to run after the next grace period.
    waiting_callbacks: Mutex<AtomicListIterator<RcuCallback>>,
}

const ADVANCER_THREAD_SLEEPING: i32 = 0;
const ADVANCER_THREAD_ACTIVE: i32 = 1;

/// The number of times to spin checking for active readers before yielding.
const ADVANCER_SPIN_LIMIT: u32 = 64;
/// The number of spin/yield iterations before falling back to sleeping.
const ADVANCER_YIELD_LIMIT: u32 = 66;
/// Initial sleep duration when active readers are still present after spinning and yielding.
const ADVANCER_INITIAL_SLEEP: Duration = Duration::from_micros(50);
/// Maximum sleep duration for exponential backoff during long stalls.
const ADVANCER_MAX_SLEEP: Duration = Duration::from_millis(1);

impl RcuControlBlock {
    /// Create a new control block for the RCU state machine.
    const fn new() -> Self {
        #[cfg(feature = "rseq_backend")]
        let read_counters = RcuReadCounters::new();

        #[cfg(not(feature = "rseq_backend"))]
        let read_counters = [AtomicUsize::new(0), AtomicUsize::new(0)];

        Self {
            generation: AtomicUsize::new(0),
            read_counters,
            callback_chain: AtomicStack::new(),
            advancer_thread_state: zx::Futex::new(ADVANCER_THREAD_SLEEPING),
            waiting_callbacks: Mutex::new(AtomicListIterator::empty()),
        }
    }
}

/// The control block for the RCU state machine.
static RCU_CONTROL_BLOCK: RcuControlBlock = RcuControlBlock::new();

struct RcuThreadBlock {
    /// The number of times the thread has nested into a read lock.
    nesting_level: AtomicUsize,

    /// The index of the read counter that the thread incremented when it entered its outermost read
    /// lock.
    counter_index: AtomicU8,
}

impl RcuThreadBlock {
    /// Creates a new `RcuThreadBlock`.
    const fn new() -> Self {
        Self { nesting_level: AtomicUsize::new(0), counter_index: AtomicU8::new(0) }
    }

    /// Returns true if the thread is holding a read lock.
    fn holding_read_lock(&self) -> bool {
        self.nesting_level.load(Ordering::Relaxed) > 0
    }
}
thread_local! {
    /// Thread-specific data for the RCU state machine.
    ///
    /// This data is used to track the nesting level of read locks and the index of the read counter
    /// that the thread incremented when it entered its outermost read lock.
    static RCU_THREAD_BLOCK: RcuThreadBlock = const { RcuThreadBlock::new() };
}

/// An RAII guard that keeps the current thread registered with RCU and RSEQ.
///
/// Unregisters the thread when dropped.
#[derive(Debug)]
#[must_use = "the thread is unregistered when the guard is dropped"]
pub struct RcuThreadRegistration {
    _marker: PhantomData<*const ()>,
}

impl RcuThreadRegistration {
    /// Leaks the registration, keeping the current thread registered indefinitely.
    #[inline]
    pub fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for RcuThreadRegistration {
    fn drop(&mut self) {
        unregister_thread();
    }
}

/// Registers the current thread for RCU and RSEQ.
pub fn register_thread() -> RcuThreadRegistration {
    #[cfg(feature = "rseq_backend")]
    fuchsia_rseq::rseq_register_thread_with_cs(crate::read_counters::rcu_critical_section());

    RcuThreadRegistration { _marker: PhantomData }
}

/// Unregisters the current thread from RCU and RSEQ.
pub fn unregister_thread() {
    #[cfg(feature = "rseq_backend")]
    fuchsia_rseq::rseq_unregister_thread();
}

/// Exposes the thread-local counters for RCU stall detection.
pub fn with_thread_block_counters<F>(f: F)
where
    F: FnOnce(*const AtomicUsize, *const AtomicU8),
{
    RCU_THREAD_BLOCK.with(|thread_block| {
        f(&thread_block.nesting_level as *const _, &thread_block.counter_index as *const _);
    });
}

/// Acquire a read lock.
///
/// This function is used to acquire a read lock on the RCU state machine. The RCU state machine
/// defers calling callbacks until all currently in-flight read operations have completed.
///
/// Must be balanced by a call to `rcu_read_unlock` on the same thread.
#[inline]
pub(crate) fn rcu_read_lock() {
    RCU_THREAD_BLOCK.with(|thread_block| {
        let nesting_level = thread_block.nesting_level.load(Ordering::Relaxed);
        if nesting_level > 0 {
            // If this thread already has a read lock, increment the nesting level instead of the
            // incrementing the read counter. This approach is a performance optimization to reduce
            // the number of atomic operations that need to be performed.
            thread_block.nesting_level.store(nesting_level + 1, Ordering::Relaxed);
        } else {
            // This is the outermost read lock. Increment the read counter.
            let control_block = &RCU_CONTROL_BLOCK;

            // There's a race here where we capture `index` and then go on to increment the read
            // counter.  The choice of `index` here isn't actually important for correctness because
            // we always wait at least two grace periods before calling the callbacks, so it doesn't
            // matter which counter we increment.  It does mean that a thread waiting for the read
            // counter to drop to zero, could actually find that the read counter increases before
            // it eventually reaches zero, which should be fine.
            let index = control_block.generation.load(Ordering::Relaxed) & 1;

            #[cfg(feature = "rseq_backend")]
            {
                control_block.read_counters.begin(index);
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
            }

            #[cfg(not(feature = "rseq_backend"))]
            {
                // Synchronization point [A] (see design.md)
                control_block.read_counters[index].fetch_add(1, Ordering::SeqCst);
            }

            thread_block.counter_index.store(index as u8, Ordering::Relaxed);
            thread_block.nesting_level.store(1, Ordering::Relaxed);
        }
    });
}

/// Release a read lock.
///
/// This function is used to release a read lock on the RCU state machine. See `rcu_read_lock` for
/// more details.
#[inline]
pub(crate) fn rcu_read_unlock() {
    RCU_THREAD_BLOCK.with(|thread_block| {
        let nesting_level = thread_block.nesting_level.load(Ordering::Relaxed);
        if nesting_level > 1 {
            // If the nesting level is greater than 1, this is not the outermost read lock.
            // Decrement the nesting level instead of the read counter.
            thread_block.nesting_level.store(nesting_level - 1, Ordering::Relaxed);
        } else {
            // This is the outermost read lock. Decrement the read counter.
            let index = thread_block.counter_index.load(Ordering::Relaxed) as usize;
            let control_block = &RCU_CONTROL_BLOCK;

            #[cfg(feature = "rseq_backend")]
            {
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
                control_block.read_counters.end(index);
            }

            #[cfg(not(feature = "rseq_backend"))]
            {
                // Synchronization point [B] (see design.md)
                control_block.read_counters[index].fetch_sub(1, Ordering::SeqCst);
            }

            thread_block.nesting_level.store(0, Ordering::Relaxed);
            thread_block.counter_index.store(u8::MAX, Ordering::Relaxed);
        }
    });
}

/// Read the value of an RCU pointer.
///
/// This function cannot be called unless the current thread is holding a read lock. The returned
/// pointer is valid until the read lock is released.
pub(crate) fn rcu_read_pointer<T>(ptr: &AtomicPtr<T>) -> *const T {
    // Synchronization point [D] (see design.md)
    ptr.load(Ordering::Acquire)
}

/// Assign a new value to an RCU pointer.
///
/// Concurrent readers may continue to reference the old value of the pointer until the RCU state
/// machine has made sufficient progress. To clean up the old value of the pointer, use `rcu_call`
/// or `rcu_drop`, which defer processing until all in-flight read operations have completed.
pub(crate) fn rcu_assign_pointer<T>(ptr: &AtomicPtr<T>, new_ptr: *mut T) {
    // Synchronization point [E] (see design.md)
    ptr.store(new_ptr, Ordering::Release);
}

/// Replace the value of an RCU pointer.
///
/// Concurrent readers may continue to reference the old value of the pointer until the RCU state
/// machine has made sufficient progress. To clean up the old value of the pointer, use `rcu_call`
/// or `rcu_drop`, which defer processing until all in-flight read operations have completed.
pub(crate) fn rcu_replace_pointer<T>(ptr: &AtomicPtr<T>, new_ptr: *mut T) -> *mut T {
    // Synchronization point [F] (see design.md)
    ptr.swap(new_ptr, Ordering::AcqRel)
}

/// Call a callback to run after all in-flight read operations have completed.
///
/// To wait until the callback is ready to run, call `rcu_synchronize()`. Note that
/// `rcu_synchronize()` requires an advancer thread or a thread calling `rcu_run_callbacks()` to
/// make progress and invoke callbacks. The callback might be called from an arbitrary thread.
///
/// NOTE: The order in which callbacks are called is not guaranteed since they can be called
/// concurrently from multiple threads.
pub(crate) fn rcu_call(callback: impl FnOnce() + Send + Sync + 'static) {
    #[cfg(not(feature = "rseq_backend"))]
    {
        // We need to synchronize with rcu_read_lock.  We need to ensure that all prior stores are
        // visible to threads that have called rcu_read_lock.  We must synchronize with both read
        // counters using a store operation.  We don't need to change the value.
        std::sync::atomic::fence(Ordering::Release);

        RCU_CONTROL_BLOCK.read_counters[0].fetch_add(0, Ordering::Relaxed);
        RCU_CONTROL_BLOCK.read_counters[1].fetch_add(0, Ordering::Relaxed);
    }

    // Synchronization point [G] (see design.md)
    RCU_CONTROL_BLOCK.callback_chain.push_front(Box::new(callback));

    // Wake the rcu advancer thread if it is sleeping on the futex.
    let thread_state = &RCU_CONTROL_BLOCK.advancer_thread_state;

    // This write is required to be SeqCst to ensure total ordering with additions to the
    // callback_chain. See the comment in rcu_advancer_wait_for_work for details.
    if thread_state.swap(ADVANCER_THREAD_ACTIVE, Ordering::SeqCst) == ADVANCER_THREAD_SLEEPING {
        thread_state.wake(1);
    }
}

/// Schedule the object to be dropped after all in-flight read operations have completed.
///
/// To wait until the object is dropped, call `rcu_synchronize()`. Note that `rcu_synchronize()`
/// requires an advancer thread or a thread calling `rcu_run_callbacks()` to make progress and drop
/// the object.
///
/// To be safely passed to [rcu_drop] either directly, or indirectly by an rcu container, the type
/// must implement the marker trait [RcuDroppable] to indicate:
/// - Dropping T must not take locks or otherwise block.
/// - It is safe to drop T from an arbitrary thread.
/// - There is no guarantee as to _when_ T will actually be dropped unless `rcu_synchronize()` or
///   `rcu_run_callbacks()` is called.
pub fn rcu_drop<T: RcuDroppable + Sync>(value: T) {
    rcu_call(move || {
        std::mem::drop(value);
    });
}

/// Check if there are any active readers for the given generation.
fn has_active_readers(generation: usize) -> bool {
    let index = generation & 1;

    #[cfg(feature = "rseq_backend")]
    {
        return RCU_CONTROL_BLOCK.read_counters.has_active(index);
    }

    #[cfg(not(feature = "rseq_backend"))]
    {
        // Synchronization point [C] (see design.md)
        RCU_CONTROL_BLOCK.read_counters[index].load(Ordering::SeqCst) > 0
    }
}

/// Wake the rcu advancer thread if it's sleeping.
pub fn rcu_advancer_wake() {
    // Scheduling a no-op callback is a convenient way to both wake the rcu advancer and also have
    // it consider the wakeup to be non spurious so it doesn't immediately go back to sleep.
    rcu_call(|| {});
}

/// Blocks the current thread until all in-flight read operations have completed for the given
/// generation.
///
/// Postcondition: The number of active readers for the given generation is zero.
fn rcu_advancer_wait_for_readers(generation: usize) {
    let mut spins = 0u32;
    let mut sleep_duration = ADVANCER_INITIAL_SLEEP;
    while has_active_readers(generation) {
        // In practice, we tend to see a bimodel distribution of read locks that either release
        // within a few hundred ns, or around a few µs.
        //
        // Then, we see long tail of cases where the release can take > 1ms because a thread got
        // context switched out while holding a read lock.
        //
        // We attempt to model this behavior by first spinning, then slowly backing off.
        if spins < ADVANCER_SPIN_LIMIT {
            std::hint::spin_loop();
            spins += 1;
        } else if spins < ADVANCER_YIELD_LIMIT {
            std::thread::yield_now();
            spins += 1;
        } else {
            std::thread::sleep(sleep_duration);
            sleep_duration = std::cmp::min(sleep_duration * 2, ADVANCER_MAX_SLEEP);
        }
    }
}

/// Advance the RCU state machine.
///
/// This function blocks until all in-flight read operations have completed for the current
/// generation and all callbacks have been run.
fn rcu_grace_period() {
    let callbacks = {
        let mut waiting_callbacks = RCU_CONTROL_BLOCK.waiting_callbacks.lock();

        // We are in the *Idle* state.

        // Swap out the callbacks that we can run when this grace period has passed with the
        // callbacks that can run after the next period.
        // Synchronization point [H] (see design.md)
        let callbacks =
            std::mem::replace(&mut *waiting_callbacks, RCU_CONTROL_BLOCK.callback_chain.take());

        // Issue an IPI to all CPUs to force them to serialize their execution.
        // This ensures that all prior stores by all writers are visible to
        // any thread that subsequently enters an RCU read-side critical section.
        #[cfg(feature = "rseq_backend")]
        unsafe {
            zx::sys::zx_membarrier_sync_process_data()
        };

        let generation = RCU_CONTROL_BLOCK.generation.fetch_add(1, Ordering::Relaxed);

        // Enter the *Waiting* state
        rcu_advancer_wait_for_readers(generation);

        // Return to the *Idle* state.
        callbacks
    };

    for callback in callbacks {
        callback();
    }
}

/// Block until all in-flight read operations have completed for callbacks registered prior to this
/// call.
///
/// Note: This function does not advance the RCU state machine itself; it registers a callback and
/// blocks until that callback is run. If no thread is calling `rcu_run_callbacks()` (for example,
/// via a dedicated advancer thread), this function will block indefinitely.
pub fn rcu_synchronize() {
    RCU_THREAD_BLOCK.with(|block| {
        assert!(!block.holding_read_lock());
    });

    let completion = std::sync::Arc::new(Completion::new());
    let c = completion.clone();
    rcu_call(move || {
        c.signal();
    });

    // If callbacks have run, then we know all read operations must have completed for the
    // generation we registered the callback on.
    completion.wait();
}

/// Check if there is any work waiting to be processed by the RCU advancer.
fn has_pending_work() -> bool {
    !RCU_CONTROL_BLOCK.callback_chain.is_empty()
        || !RCU_CONTROL_BLOCK.waiting_callbacks.lock().is_empty()
}

/// Blocks the calling advancer thread until RCU callbacks are scheduled.
pub fn rcu_advancer_wait_for_work() {
    let thread_state = &RCU_CONTROL_BLOCK.advancer_thread_state;
    while !has_pending_work() {
        // This needs to be SeqCst to make the has_pending_work() call synchronize properly, as
        // with the write in rcu_call. Without a SeqCst, it's possible that a waker sees our write,
        // and thus doesn't wake us, and simultaneously, we don't see the pending work, and thus go
        // to sleep, causing a lost wakeup.
        //
        // With the SeqCst total ordering, we're guaranteed that either a thread scheduling
        // callbacks doesn't observe our write, and thus tries to wake us, or that we observe the
        // added callback, and thus don't sleep.
        thread_state.store(ADVANCER_THREAD_SLEEPING, Ordering::SeqCst);

        // Double-check after storing SLEEPING to prevent race with rcu_call, rcu_synchronize, or
        // rcu_advancer_wake.
        if has_pending_work() {
            break;
        }

        // In the case of a spurious wakeup, we recheck has_pending_work and if there is no work to
        // be done, attempt to return to sleep.
        let _ = thread_state.wait(ADVANCER_THREAD_SLEEPING, None, zx::MonotonicInstant::INFINITE);
    }
    thread_state.store(ADVANCER_THREAD_ACTIVE, Ordering::Relaxed);
}

/// Advances the RCU state machine if work is pending and runs ready callbacks.
///
/// If callbacks are pending, this runs two grace periods and invokes any ready callbacks.
///
/// Returns `true` if callbacks were processed, or `false` if no work was pending.
pub fn rcu_run_callbacks() -> bool {
    RCU_THREAD_BLOCK.with(|block| {
        assert!(!block.holding_read_lock());
    });

    let thread_state = &RCU_CONTROL_BLOCK.advancer_thread_state;
    thread_state.store(ADVANCER_THREAD_ACTIVE, Ordering::Relaxed);

    if has_pending_work() {
        rcu_grace_period();
        rcu_grace_period();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_rcu_delay_regression() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let moved_flag = flag.clone();

        rcu_call(move || {
            moved_flag.store(true, Ordering::SeqCst);
        });

        rcu_grace_period();

        assert!(
            !flag.load(Ordering::SeqCst),
            "Callback executed too early! RCU requires 2 grace periods delay."
        );

        rcu_grace_period();
        assert!(flag.load(Ordering::SeqCst), "Callback should have executed after 2 grace periods");
    }

    #[test]
    fn test_rcu_synchronize() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                rcu_advancer_wait_for_work();
                rcu_run_callbacks();
            }
            rcu_run_callbacks();
        });

        let completion = Arc::new(Completion::new());
        let c = completion.clone();

        rcu_call(move || {
            c.signal();
        });

        rcu_synchronize();

        // Callbacks within a batch are not guaranteed to execute in a specific order,
        // so the callback may complete slightly before or after rcu_synchronize() returns.
        completion.wait();

        stop.store(true, Ordering::Relaxed);
        rcu_advancer_wake();
        handle.join().unwrap();
        RCU_CONTROL_BLOCK.advancer_thread_state.store(ADVANCER_THREAD_SLEEPING, Ordering::SeqCst);
    }

    #[test]
    fn test_rcu_run_callbacks() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let moved_flag = flag.clone();

        rcu_call(move || {
            moved_flag.store(true, Ordering::SeqCst);
        });

        assert!(rcu_run_callbacks());
        assert!(
            flag.load(Ordering::SeqCst),
            "Callback should have executed after rcu_run_callbacks()"
        );
        // Second step has no pending work.
        assert!(!rcu_run_callbacks());
        RCU_CONTROL_BLOCK.advancer_thread_state.store(ADVANCER_THREAD_SLEEPING, Ordering::SeqCst);
    }

    #[test]
    fn test_rcu_advancer_thread_wake() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                rcu_advancer_wait_for_work();
                rcu_run_callbacks();
            }
            rcu_run_callbacks();
        });

        rcu_call(move || {
            flag_clone.store(true, Ordering::SeqCst);
        });

        // Wait for advancer thread to wake up and process callback.
        let start = std::time::Instant::now();
        while !flag.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "Timed out waiting for advancer thread to process callback"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        stop.store(true, Ordering::Relaxed);
        // Wake the thread if it went back to sleep so it can terminate.
        rcu_advancer_wake();
        handle.join().unwrap();
        RCU_CONTROL_BLOCK.advancer_thread_state.store(ADVANCER_THREAD_SLEEPING, Ordering::SeqCst);
    }

    #[test]
    fn test_rcu_synchronize_no_callbacks() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                rcu_advancer_wait_for_work();
                rcu_run_callbacks();
            }
            rcu_run_callbacks();
        });

        // Calling rcu_synchronize() with no prior callbacks should wake the advancer,
        // wait for the grace periods to complete, and return successfully.
        rcu_synchronize();

        stop.store(true, Ordering::Relaxed);
        rcu_advancer_wake();
        handle.join().unwrap();
        RCU_CONTROL_BLOCK.advancer_thread_state.store(ADVANCER_THREAD_SLEEPING, Ordering::SeqCst);
    }
}
