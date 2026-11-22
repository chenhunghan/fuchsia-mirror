// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use starnix_uapi::errors::Errno;

use starnix_sync::{
    DynamicLockTracking, InterruptibleEvent, LockDepMutex, LockLevel, LockLevelToken,
    RwQueueInnerLock,
};
use std::collections::VecDeque;
use std::sync::Arc;

use lock_api as _;

#[cfg(any(test, debug_assertions))]
use lock_api::RawRwLock;

#[derive(Debug)]
pub struct RwQueue {
    inner: LockDepMutex<RwQueueInner, RwQueueInnerLock>,
    tracking: DynamicLockTracking,

    // Used to inform our deadlock detector about the waiters in the queue.
    #[cfg(any(test, debug_assertions))]
    tracer: tracer::MutexTracer,
}

impl RwQueue {
    pub fn new<L: LockLevel>() -> Self {
        Self {
            inner: Default::default(),
            tracking: DynamicLockTracking::new::<L>(),
            #[cfg(any(test, debug_assertions))]
            tracer: Default::default(),
        }
    }

    fn unlock_read(&self) {
        self.inner.lock().unlock_read();

        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        #[cfg(any(test, debug_assertions))]
        unsafe {
            self.tracer.unlock_shared();
        }
    }

    fn unlock_write(&self) {
        self.inner.lock().unlock_write();

        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        #[cfg(any(test, debug_assertions))]
        unsafe {
            self.tracer.unlock_exclusive();
        }
    }

    fn cancel_read(&self, event: &Arc<InterruptibleEvent>) {
        self.inner.lock().cancel_read(event);

        // SAFETY: Paired with `self.tracer.lock_shared()` in `Self::read`.
        #[cfg(any(test, debug_assertions))]
        unsafe {
            self.tracer.unlock_shared();
        }
    }

    fn cancel_write(&self, event: &Arc<InterruptibleEvent>) {
        self.inner.lock().cancel_write(event);

        // SAFETY: Paired with `self.tracer.lock_exclusive()` in `Self::write`.
        #[cfg(any(test, debug_assertions))]
        unsafe {
            self.tracer.unlock_exclusive();
        }
    }

    #[track_caller]
    pub fn read<'a>(&'a self, current_task: &CurrentTask) -> Result<RwQueueReadGuard<'a>, Errno> {
        let token = self.tracking.acquire();
        #[cfg(any(test, debug_assertions))]
        self.tracer.lock_shared();

        let mut inner = self.inner.lock();

        if !inner.try_read() {
            let event = InterruptibleEvent::new();
            let guard = event.begin_wait();

            inner.waiters.push_back(Waiter::Reader(event.clone()));

            std::mem::drop(inner);

            current_task.block_until(guard, zx::MonotonicInstant::INFINITE).map_err(|e| {
                self.cancel_read(&event);
                e
            })?;
        }
        Ok(RwQueueReadGuard { queue: self, _token: token })
    }

    #[track_caller]
    pub fn write<'a>(&'a self, current_task: &CurrentTask) -> Result<RwQueueWriteGuard<'a>, Errno> {
        let token = self.tracking.acquire();
        #[cfg(any(test, debug_assertions))]
        self.tracer.lock_exclusive();

        let mut inner = self.inner.lock();

        if !inner.try_write() {
            let event = InterruptibleEvent::new();
            let guard = event.begin_wait();

            inner.waiters.push_back(Waiter::Writer(event.clone()));

            std::mem::drop(inner);

            current_task.block_until(guard, zx::MonotonicInstant::INFINITE).map_err(|e| {
                self.cancel_write(&event);
                e
            })?;
        }
        Ok(RwQueueWriteGuard { queue: self, _token: token })
    }
}

/// The queue is ready for any operation.
const READY: usize = 0;

/// The queue has exactly one writer.
const WRITER: usize = 0b01;

/// Each reader in the queue increments the state by this amount.
const READER: usize = 0b10;

/// A writer is currently running.
fn has_writer(state: usize) -> bool {
    state & WRITER != 0
}

/// At least one reader is currently running.
fn has_reader(state: usize) -> bool {
    state >= READER
}

fn debug_assert_consistent(state: usize) {
    debug_assert!(!has_writer(state) || !has_reader(state));
}

#[derive(Debug, Clone)]
enum Waiter {
    Reader(Arc<InterruptibleEvent>),
    Writer(Arc<InterruptibleEvent>),
}

#[derive(Debug, Default)]
struct RwQueueInner {
    /// What operations are currently ongoing.
    ///
    /// See READY, READER, WRITER above for what these bits mean.
    state: usize,

    /// The operations that are waiting for the ongoing operations to complete.
    waiters: VecDeque<Waiter>,
}

impl RwQueueInner {
    fn has_waiters(&self) -> bool {
        !self.waiters.is_empty()
    }

    fn try_read(&mut self) -> bool {
        debug_assert_consistent(self.state);
        if !has_writer(self.state) && !self.has_waiters() {
            if let Some(new_state) = self.state.checked_add(READER) {
                self.state = new_state;
                return true;
            }
        }
        false
    }

    fn try_write(&mut self) -> bool {
        debug_assert_consistent(self.state);
        if self.state == READY && !self.has_waiters() {
            self.state += WRITER;
            true
        } else {
            false
        }
    }

    fn unlock_read(&mut self) {
        debug_assert!(has_reader(self.state) && !has_writer(self.state));
        self.state -= READER;

        if !has_reader(self.state) && self.has_waiters() {
            self.notify_next();
        }
    }

    fn unlock_write(&mut self) {
        debug_assert!(has_writer(self.state) && !has_reader(self.state));
        self.state -= WRITER;

        if self.has_waiters() {
            self.notify_next();
        }
    }

    fn notify_next(&mut self) {
        while let Some(waiter) = self.waiters.front() {
            match waiter {
                Waiter::Reader(reader) => {
                    if has_writer(self.state) {
                        return;
                    }
                    // We need to use `checked_add` to ensure we do not
                    // overflow the number of readers. If that happens, we just
                    // need to wait for the enormous number of readers to finish.
                    let Some(new_state) = self.state.checked_add(READER) else {
                        return;
                    };
                    self.state = new_state;
                    reader.notify();
                }
                Waiter::Writer(writer) => {
                    if has_reader(self.state) || has_writer(self.state) {
                        return;
                    }
                    // We can never overflow writers because we only let one
                    // through at a time.
                    self.state += WRITER;
                    writer.notify();
                }
            }
            self.waiters.pop_front();
        }
        debug_assert_consistent(self.state);
    }

    fn remove_waiter(&mut self, event: &Arc<InterruptibleEvent>) -> bool {
        let len_before = self.waiters.len();
        self.waiters.retain(|waiter| {
            let (Waiter::Reader(other) | Waiter::Writer(other)) = waiter;
            !Arc::ptr_eq(event, other)
        });
        self.waiters.len() != len_before
    }

    fn cancel_read(&mut self, event: &Arc<InterruptibleEvent>) {
        if !self.remove_waiter(event) {
            // The event was already removed from the queue, so we're already holding a read lock.
            // We should unlock to avoid leaving the queue locked.
            self.unlock_read();
        }
    }

    fn cancel_write(&mut self, event: &Arc<InterruptibleEvent>) {
        if !self.remove_waiter(event) {
            // The event was already removed from the queue, so we're already holding a write lock.
            // We should unlock to avoid leaving the queue locked.
            self.unlock_write();
        } else if has_reader(self.state) {
            // Readers may be blocked in the queue behind the writer we just removed. We should
            // wake them up.
            self.notify_next();
        }
    }
}

pub struct RwQueueReadGuard<'a> {
    queue: &'a RwQueue,
    _token: LockLevelToken,
}

impl<'a> Drop for RwQueueReadGuard<'a> {
    fn drop(&mut self) {
        self.queue.unlock_read();
    }
}

pub struct RwQueueWriteGuard<'a> {
    queue: &'a RwQueue,
    _token: LockLevelToken,
}

impl<'a> Drop for RwQueueWriteGuard<'a> {
    fn drop(&mut self) {
        self.queue.unlock_write();
    }
}

#[cfg(any(test, debug_assertions))]
mod tracer {

    #[derive(Debug, Default)]
    pub struct FakeRwLock {}

    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    unsafe impl lock_api::RawRwLock for FakeRwLock {
        const INIT: Self = Self {};

        type GuardMarker = lock_api::GuardNoSend;

        fn lock_shared(&self) {}
        fn try_lock_shared(&self) -> bool {
            false
        }
        unsafe fn unlock_shared(&self) {}

        fn lock_exclusive(&self) {}
        fn try_lock_exclusive(&self) -> bool {
            false
        }
        unsafe fn unlock_exclusive(&self) {}

        fn is_locked(&self) -> bool {
            false
        }
    }

    // We should replace this type with tracing_mutex::MutexId once that type is public.
    pub type MutexTracer = tracing_mutex::lockapi::TracingWrapper<FakeRwLock>;
}

// We use tracing_mutex in tests and debug assertions, but we don't want to pull it in for
// production.
#[cfg(not(any(test, debug_assertions)))]
use tracing_mutex as _;

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::task::{Kernel, TaskFlags};
    use crate::testing::*;
    use futures::executor::block_on;
    use futures::future::join_all;
    use starnix_sync::lock_ordering;
    use starnix_uapi::errno;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[::fuchsia::test]
    fn test_remove_from_queue() {
        let mut inner = RwQueueInner::default();
        let event1 = InterruptibleEvent::new();
        let event2 = InterruptibleEvent::new();
        let event3 = InterruptibleEvent::new();
        inner.waiters.push_back(Waiter::Writer(event1.clone()));
        inner.waiters.push_back(Waiter::Writer(event2.clone()));
        inner.waiters.push_back(Waiter::Writer(event3.clone()));

        inner.remove_waiter(&event2);

        let waiter = inner.waiters.pop_front().expect("should have a waiter");
        let Waiter::Writer(event) = waiter else {
            unreachable!();
        };
        assert!(Arc::ptr_eq(&event1, &event));

        let waiter = inner.waiters.pop_front().expect("should have a waiter");
        let Waiter::Writer(event) = waiter else {
            unreachable!();
        };
        assert!(Arc::ptr_eq(&event3, &event));

        assert!(inner.waiters.is_empty());
    }

    #[::fuchsia::test]
    async fn test_write_and_read() {
        lock_ordering! {
            Unlocked => TestLevel
        }

        spawn_kernel_and_run(async |current_task| {
            let queue = RwQueue::new::<TestLevel>();
            let read_guard1 = queue.read(current_task).expect("shouldn't be interrupted");
            std::mem::drop(read_guard1);

            let write_guard = queue.write(current_task).expect("shouldn't be interrupted");
            std::mem::drop(write_guard);

            let read_guard2 = queue.read(current_task).expect("shouldn't be interrupted");
            std::mem::drop(read_guard2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_read_in_parallel() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            lock_ordering! {
                Unlocked => TestLevel
            }
            struct Info {
                barrier: Barrier,
                queue: RwQueue,
            }

            let info =
                Arc::new(Info { barrier: Barrier::new(2), queue: RwQueue::new::<TestLevel>() });

            let info1 = Arc::clone(&info);
            let closure1 = move |current_task: &CurrentTask| {
                let guard = info1.queue.read(current_task).expect("shouldn't be interrupted");
                info1.barrier.wait();
                std::mem::drop(guard);
            };
            let (thread1, req) =
                SpawnRequestBuilder::new().with_sync_closure(closure1).build_with_async_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let info2 = Arc::clone(&info);
            let closure2 = move |current_task: &CurrentTask| {
                let guard = info2.queue.read(current_task).expect("shouldn't be interrupted");
                info2.barrier.wait();
                std::mem::drop(guard);
            };
            let (thread2, req) =
                SpawnRequestBuilder::new().with_sync_closure(closure2).build_with_async_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            block_on(async {
                thread1.await.expect("failed to join thread");
                thread2.await.expect("failed to join thread");
            });
        })
        .await;
    }

    lock_ordering! {
        Unlocked => A
    }
    struct State {
        queue: RwQueue,
        gate: Barrier,
        writer_count: AtomicUsize,
        reader_count: AtomicUsize,
    }

    impl State {
        fn new(n: usize) -> State {
            State {
                queue: RwQueue::new::<A>(),
                gate: Barrier::new(n),
                writer_count: Default::default(),
                reader_count: Default::default(),
            }
        }

        fn spawn_writer(
            state: Arc<Self>,
            kernel: Arc<Kernel>,
            count: usize,
        ) -> Pin<Box<dyn Future<Output = Result<(), Errno>> + Send>> {
            let closure = move |current_task: &CurrentTask| {
                state.gate.wait();
                for _ in 0..count {
                    let guard = state.queue.write(current_task).expect("shouldn't be interrupted");
                    let writer_count = state.writer_count.fetch_add(1, Ordering::Acquire) + 1;
                    let reader_count = state.reader_count.load(Ordering::Acquire);
                    state.writer_count.fetch_sub(1, Ordering::Release);
                    std::mem::drop(guard);
                    assert_eq!(writer_count, 1, "More than one writer held the lock at once.");
                    assert_eq!(
                        reader_count, 0,
                        "A reader and writer held the lock at the same time."
                    );
                }
            };
            let (result, req) =
                SpawnRequestBuilder::new().with_sync_closure(closure).build_with_async_result();
            kernel.kthreads.spawner().spawn_from_request(req);
            Box::pin(result)
        }

        fn spawn_reader(
            state: Arc<Self>,
            kernel: Arc<Kernel>,
            count: usize,
        ) -> Pin<Box<dyn Future<Output = Result<(), Errno>> + Send>> {
            let closure = move |current_task: &CurrentTask| {
                state.gate.wait();
                for _ in 0..count {
                    let guard = state.queue.read(current_task).expect("shouldn't be interrupted");
                    let reader_count = state.reader_count.fetch_add(1, Ordering::Acquire) + 1;
                    let writer_count = state.writer_count.load(Ordering::Acquire);
                    state.reader_count.fetch_sub(1, Ordering::Release);
                    std::mem::drop(guard);
                    assert_eq!(
                        writer_count, 0,
                        "A reader and writer held the lock at the same time."
                    );
                    assert!(reader_count > 0, "A reader held the lock without being counted.");
                }
            };
            let (result, req) =
                SpawnRequestBuilder::new().with_sync_closure(closure).build_with_async_result();
            kernel.kthreads.spawner().spawn_from_request(req);
            Box::pin(result)
        }
    }

    #[::fuchsia::test]
    async fn test_thundering_reads_and_writes() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            const THREAD_PAIRS: usize = 10;

            let state = Arc::new(State::new(THREAD_PAIRS * 2));
            let mut threads = vec![];
            for _ in 0..THREAD_PAIRS {
                threads.push(State::spawn_writer(Arc::clone(&state), kernel.clone(), 100));
                threads.push(State::spawn_reader(Arc::clone(&state), kernel.clone(), 100));
            }

            block_on(join_all(threads)).into_iter().for_each(|r| r.expect("failed to join thread"));
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_cancel_read() {
        let mut inner = RwQueueInner::default();
        assert!(inner.try_write());

        let reader1 = InterruptibleEvent::new();
        let reader2 = InterruptibleEvent::new();
        let writer2 = InterruptibleEvent::new();
        inner.waiters.push_back(Waiter::Reader(reader1.clone()));
        inner.waiters.push_back(Waiter::Reader(reader2.clone()));
        inner.waiters.push_back(Waiter::Writer(writer2));

        // Cancelling a reader while still in the queue simply removes it.
        inner.cancel_read(&reader1);
        assert_eq!(inner.state, WRITER);
        assert_eq!(inner.waiters.len(), 2);

        // Unlocking pops `reader2` and transfers a read lock to it.
        inner.unlock_write();
        assert_eq!(inner.state, READER);
        assert_eq!(inner.waiters.len(), 1);

        // Cancelling `reader2` after `notify_next()` must release its read lock and wake
        // `writer2`.
        inner.cancel_read(&reader2);
        assert_eq!(inner.state, WRITER);
        assert!(inner.waiters.is_empty());
    }

    #[::fuchsia::test]
    fn test_cancel_write() {
        let mut inner = RwQueueInner::default();
        assert!(inner.try_read());

        let writer1 = InterruptibleEvent::new();
        let reader2 = InterruptibleEvent::new();
        inner.waiters.push_back(Waiter::Writer(writer1.clone()));
        inner.waiters.push_back(Waiter::Reader(reader2));

        // Cancelling a queued writer while a reader is active wakes readers queued behind it.
        inner.cancel_write(&writer1);
        assert_eq!(inner.state, 2 * READER);
        assert!(inner.waiters.is_empty());

        inner.unlock_read();
        inner.unlock_read();
        assert_eq!(inner.state, READY);

        // While a writer holds the lock, cancelling a second queued writer must not wake a reader
        // queued behind it until the active writer unlocks.
        assert!(inner.try_write());
        let writer2 = InterruptibleEvent::new();
        let reader3 = InterruptibleEvent::new();
        inner.waiters.push_back(Waiter::Writer(writer2.clone()));
        inner.waiters.push_back(Waiter::Reader(reader3));

        inner.cancel_write(&writer2);
        assert_eq!(inner.state, WRITER);
        assert_eq!(inner.waiters.len(), 1);

        inner.unlock_write();
        assert_eq!(inner.state, READER);
        assert!(inner.waiters.is_empty());

        // Cancelling a writer after `notify_next()` popped it must release its write lock and
        // wake the next waiter.
        let writer3 = InterruptibleEvent::new();
        let reader4 = InterruptibleEvent::new();
        inner.waiters.push_back(Waiter::Writer(writer3.clone()));
        inner.waiters.push_back(Waiter::Reader(reader4));

        inner.unlock_read();
        assert_eq!(inner.state, WRITER);
        assert_eq!(inner.waiters.len(), 1);

        inner.cancel_write(&writer3);
        assert_eq!(inner.state, READER);
        assert!(inner.waiters.is_empty());
    }

    #[::fuchsia::test]
    async fn test_interrupted_read_and_write() {
        lock_ordering! {
            Unlocked => TestLevel
        }

        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let queue = Arc::new(RwQueue::new::<TestLevel>());
            let barrier = Arc::new(Barrier::new(2));

            let write_guard = queue.write(current_task).expect("shouldn't be interrupted");

            let queue_clone = Arc::clone(&queue);
            let barrier_clone = Arc::clone(&barrier);
            let (thread, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |task: &CurrentTask| {
                    task.write().set_flags(TaskFlags::WORK_CANCELLED, true);
                    assert_eq!(queue_clone.read(task).err(), Some(errno!(EINTR)));
                    assert_eq!(queue_clone.write(task).err(), Some(errno!(EINTR)));
                    task.write().set_flags(TaskFlags::WORK_CANCELLED, false);

                    barrier_clone.wait();
                    barrier_clone.wait();

                    // Re-acquiring on the same thread verifies `tracer` was unlocked on `EINTR`.
                    let read_guard = queue_clone.read(task).expect("should acquire read");
                    std::mem::drop(read_guard);
                    let write_guard = queue_clone.write(task).expect("should acquire write");
                    std::mem::drop(write_guard);
                })
                .build_with_async_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            barrier.wait();
            std::mem::drop(write_guard);
            barrier.wait();
            block_on(thread).expect("failed to join thread");
        })
        .await;
    }
}
