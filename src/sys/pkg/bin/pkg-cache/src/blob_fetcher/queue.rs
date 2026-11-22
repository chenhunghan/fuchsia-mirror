// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A bounded-concurrency work queue with priority ordering, key deduplication, and RAII
//! cancellation.
//!
//! A *request* is one call to [`WorkQueue::push`]. Requests wait in `(Priority, push order)`
//! order. Whenever fewer than `max_concurrency` tasks are running, a single driver task ([`run`])
//! starts the first waiting request whose key is idle.
//!
//! * **One task per key**: A request whose key is busy waits for that task to finish, without
//!   holding up requests for other keys.
//! * **Deduplication**: Requests with an equal key `K` and context `C` share one task and its
//!   output `O`, whether they were waiting when the task started or were pushed while it ran.
//! * **RAII cancellation**: Dropping a [`WorkFuture`] removes its request. When a running task
//!   loses its last request its [`CancelSignal`] resolves, and the key stays busy until the task
//!   returns.
//! * **Shutdown**: Dropping every [`WorkQueue`] handle drops all in-flight work and then resolves
//!   every outstanding [`WorkFuture`] to `Err(Cancelled)`.
//!
//! `K`, `C`, `M` and `O` values can be dropped, and wakers woken, while the queue's lock is held,
//! so none of them may call back into the queue.
//!
//! # Structure
//!
//! ```text
//!   WorkQueue --------------+                             +------ WorkFuture, one per request:
//!   (clonable handle)       |                             |       holds its (K, OrderKey) and
//!         |                 v                             v       receives its output O
//!         |           +--------------------------------------------+
//!         |           | Mutex<State>                               |
//!         | owns      |   runnable: OrderKey -> K                  |
//!         |           |   keys:     K -> KeyEntry                  |
//!         |           |               waiting: OrderKey -> Request |
//!         |           |               running: Option<Running>     |
//!         |           +--------------------------------------------+
//!         v                                  ^
//!   driver task (`run`) ---------------------+
//!   polls each running work_fn(K, C, M, CancelSignal)
//! ```
//!
//! # Lifecycle of a request
//!
//! ```text
//!               WorkQueue::push(Priority, K, C, M) -> WorkFuture
//!                                      |
//!             +------------------------+------------------------+
//!             | otherwise                                       | if the key's task is
//!             v                                                 | Running::Active with an
//!   +---------------------------------------+                   | equal context
//!   | Waiting                               |                   |
//!   |   a Request in KeyEntry::waiting,     |                   |
//!   |   ordered by OrderKey; removed if its |                   |
//!   |   WorkFuture is dropped               |                   |
//!   +---------------------------------------+                   |
//!             | the driver starts a task from the key's first   |
//!             | Request and attaches every waiting Request      |
//!             | for the key that has an equal context           |
//!             v                                                 v
//!   +--------------------------------------------------------------------------+
//!   | Attached                                                                 |
//!   |   a waiter in Running::Active { context, waiters, .. } while             |
//!   |   work_fn(K, C, M, CancelSignal) runs                                    |
//!   +--------------------------------------------------------------------------+
//!             |                                       |
//!             | work_fn returns Ok(O)                 | the WorkFuture is dropped
//!             v                                       v
//!   the WorkFuture resolves to Ok(O)      the waiter is removed; after the last one the task is
//!                                         Running::Cancelling: its CancelSignal resolves, and
//!                                         the key stays busy until work_fn returns
//! ```

use fuchsia_async as fasync;
use fuchsia_inspect as finspect;
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use futures::future::{BoxFuture, FutureExt as _};
use futures::stream::{FuturesUnordered, StreamExt as _};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::{Future, poll_fn};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};

/// Clonable handle to a bounded-concurrency work queue parameterized by key `K`, coalescing
/// context `C`, per-request metadata `M`, and output `O`.
pub(crate) struct WorkQueue<K, C, M, O> {
    state: Arc<Mutex<State<K, C, M, O>>>,
    /// Owns the task running [`run`], which is aborted when the last handle is dropped.
    _driver: Arc<fasync::Scope>,
}

impl<K, C, M, O> std::fmt::Debug for WorkQueue<K, C, M, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkQueue").finish_non_exhaustive()
    }
}

impl<K, C, M, O> Clone for WorkQueue<K, C, M, O> {
    fn clone(&self) -> Self {
        Self { state: Arc::clone(&self.state), _driver: Arc::clone(&self._driver) }
    }
}

impl<K, C, M, O> WorkQueue<K, C, M, O> {
    /// Creates a queue that runs at most `max_concurrency` tasks at a time.
    ///
    /// `work_fn` starts the task for a key. It is passed the context and metadata of the request
    /// the task is started for, and a [`CancelSignal`] that resolves once every request attached
    /// to the task has been dropped.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an executor, because the queue's driver task is
    /// spawned on [`fasync::Scope::global`].
    pub(crate) fn new<W, F>(max_concurrency: NonZeroUsize, work_fn: W) -> Self
    where
        W: Fn(K, C, M, CancelSignal) -> F + Send + 'static,
        F: Future<Output = Result<O, Cancelled>> + Send + 'static,
        K: Eq + Hash + Clone + Send + 'static,
        C: Eq + Hash + Clone + Send + 'static,
        M: Send + 'static,
        O: Clone + Send + 'static,
    {
        let state = Arc::new(Mutex::new(State::new()));
        // Tie the driver's lifetime to the `WorkQueue` handles rather than the caller's scope:
        // `Scope::new()` (a child of `Scope::current()`) would abort the driver if the creating
        // scope is cancelled or hang `join()` on its infinite loop, while `new_child()` on
        // `Scope::global()` still aborts the driver when the last `WorkQueue` handle is dropped.
        let scope = fasync::Scope::global().new_child();
        let _: fasync::JoinHandle<()> =
            scope.spawn(run(CloseOnDrop(Arc::clone(&state)), max_concurrency.get(), work_fn));
        Self { state, _driver: Arc::new(scope) }
    }

    /// Allocates a new [`Priority`] with lower priority than any previously allocated [`Priority`].
    pub(crate) fn new_priority(&self) -> Priority {
        let mut state = self.state.lock();
        let priority = Priority(state.next_priority);
        state.next_priority += 1;
        priority
    }

    /// Queues a request for `key`. Requests with an equal `key` and `context` share one task, and
    /// `meta` is passed to `work_fn` only if this is the request the task is started for.
    ///
    /// The returned future resolves to the task's output, or to `Err(Cancelled)` if the queue is
    /// dropped first. Dropping the future removes the request.
    pub(crate) fn push(
        &self,
        priority: Priority,
        key: K,
        context: C,
        meta: M,
    ) -> WorkFuture<K, C, M, O>
    where
        K: Eq + Hash + Clone,
        C: Eq + Hash + Clone,
    {
        let (order, rx, waker) = {
            let mut state = self.state.lock();
            let (order, rx) = state.enqueue(priority, key.clone(), context, meta);
            (order, rx, state.waker.take())
        };
        // Wake the driver after releasing the lock so that it does not immediately block on it.
        if let Some(waker) = waker {
            waker.wake();
        }
        WorkFuture { rx, key, order, state: Arc::downgrade(&self.state) }
    }

    /// Queues all `requests` at `priority` under a single lock acquisition, waking the driver at
    /// most once.
    ///
    /// Dropping the returned iterator or any unresolved [`WorkFuture`] removes the corresponding
    /// request.
    pub(crate) fn push_all(
        &self,
        priority: Priority,
        requests: impl IntoIterator<Item = (K, C, M)>,
    ) -> std::vec::IntoIter<WorkFuture<K, C, M, O>>
    where
        K: Eq + Hash + Clone,
        C: Eq + Hash + Clone,
    {
        let requests = requests.into_iter();
        let (enqueued, waker) = {
            let mut state = self.state.lock();
            let mut enqueued = Vec::with_capacity(requests.size_hint().0);
            for (key, context, meta) in requests {
                let (order, rx) = state.enqueue(priority, key.clone(), context, meta);
                enqueued.push((key, order, rx));
            }
            let waker = if enqueued.is_empty() { None } else { state.waker.take() };
            (enqueued, waker)
        };
        // Wake the driver after releasing the lock so that it does not immediately block on it.
        if let Some(waker) = waker {
            waker.wake();
        }
        let weak_state = Arc::downgrade(&self.state);
        enqueued
            .into_iter()
            .map(|(key, order, rx)| WorkFuture { rx, key, order, state: Weak::clone(&weak_state) })
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Returns a lazy Inspect callback recording the number of keys with live (running + waiting)
    /// requests.
    pub(crate) fn record_lazy_inspect(
        &self,
    ) -> impl Fn() -> BoxFuture<'static, Result<finspect::Inspector, anyhow::Error>>
    + Send
    + Sync
    + 'static
    where
        K: Send + 'static,
        C: Send + 'static,
        M: Send + 'static,
        O: Send + 'static,
    {
        let state = Arc::downgrade(&self.state);
        move || {
            let state = Weak::clone(&state);
            async move {
                let inspector = finspect::Inspector::default();
                if let Some(state) = state.upgrade() {
                    // Release the lock before the inspect operations in case they are slow.
                    let length =
                        state.lock().keys.values().filter(|entry| entry.requests() > 0).count();
                    inspector.root().record_uint("length", length as u64);
                }
                Ok(inspector)
            }
            .boxed()
        }
    }
}

/// Priority identifier for queued work items. Lower internal values have higher priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Priority(u64);

/// `(Priority, push order)`: priority ordering across priorities, FIFO within one.
type OrderKey = (Priority, u64);

struct State<K, C, M, O> {
    next_priority: u64,
    next_seq: u64,
    /// Set once the driver is gone. Requests pushed afterwards fail immediately.
    closed: bool,
    /// Wakes the driver, if it is parked waiting for a request it can start.
    waker: Option<Waker>,
    /// The first waiting request of every idle key, in scheduling order.
    runnable: BTreeMap<OrderKey, K>,
    /// Invariant: every entry has a running task or at least one waiting request.
    keys: HashMap<K, KeyEntry<C, M, O>>,
}

struct KeyEntry<C, M, O> {
    waiting: BTreeMap<OrderKey, Request<C, M, O>>,
    /// The waiting requests with each context, so a starting task can find the ones to coalesce.
    waiting_by_context: HashMap<C, BTreeSet<OrderKey>>,
    running: Option<Running<C, O>>,
    /// This key's entry in [`State::runnable`], if it has one.
    runnable: Option<OrderKey>,
}

impl<C, M, O> KeyEntry<C, M, O> {
    fn requests(&self) -> usize {
        let attached = match &self.running {
            Some(Running::Active { waiters, .. }) => waiters.len(),
            Some(Running::Cancelling) | None => 0,
        };
        attached + self.waiting.len()
    }
}

struct Request<C, M, O> {
    context: C,
    meta: M,
    result_tx: oneshot::Sender<O>,
}

/// The task running for a key.
enum Running<C, O> {
    Active {
        context: C,
        /// The requests that will receive the task's output. Never empty.
        waiters: BTreeMap<OrderKey, oneshot::Sender<O>>,
        /// Dropped to tell the task to stop.
        _cancel_tx: oneshot::Sender<!>,
    },
    /// Every waiter was dropped and the task was told to stop. The key stays busy until it does.
    Cancelling,
}

impl<K, C, M, O> State<K, C, M, O> {
    fn new() -> Self {
        Self {
            next_priority: 0,
            next_seq: 0,
            closed: false,
            waker: None,
            runnable: BTreeMap::new(),
            keys: HashMap::new(),
        }
    }

    /// Fails every outstanding and future request.
    fn close(&mut self) {
        self.closed = true;
        self.waker = None;
        self.runnable.clear();
        self.keys.clear();
    }
}

impl<K: Eq + Hash + Clone, C: Eq + Hash, M, O> State<K, C, M, O> {
    /// Restores the invariants for `key` after its waiting requests or its task changed: updates
    /// its entry in `runnable`, and forgets the key once it has neither requests nor a task.
    fn refresh(&mut self, key: &K) {
        let Some(entry) = self.keys.get_mut(key) else {
            return;
        };
        let runnable = match entry.running {
            None => entry.waiting.keys().next().copied(),
            Some(_) => None,
        };
        if entry.runnable != runnable {
            if let Some(old) = entry.runnable {
                self.runnable.remove(&old);
            }
            if let Some(new) = runnable {
                self.runnable.insert(new, key.clone());
            }
            entry.runnable = runnable;
        }
        if entry.running.is_none() && entry.waiting.is_empty() {
            self.keys.remove(key);
        }
    }

    fn enqueue(
        &mut self,
        priority: Priority,
        key: K,
        context: C,
        meta: M,
    ) -> (OrderKey, oneshot::Receiver<O>)
    where
        C: Clone,
    {
        let order = (priority, self.next_seq);
        self.next_seq += 1;
        let (result_tx, rx) = oneshot::channel();
        if self.closed {
            return (order, rx);
        }
        let entry = self.keys.entry(key.clone()).or_insert_with(|| KeyEntry {
            waiting: BTreeMap::new(),
            waiting_by_context: HashMap::new(),
            running: None,
            runnable: None,
        });
        match &mut entry.running {
            Some(Running::Active { context: running, waiters, .. }) if *running == context => {
                waiters.insert(order, result_tx);
            }
            Some(Running::Active { .. } | Running::Cancelling) | None => {
                entry.waiting_by_context.entry(context.clone()).or_default().insert(order);
                entry.waiting.insert(order, Request { context, meta, result_tx });
                self.refresh(&key);
            }
        }
        (order, rx)
    }

    /// Removes the request of a dropped [`WorkFuture`], if it is still outstanding.
    fn detach(&mut self, key: &K, order: OrderKey) {
        let Some(entry) = self.keys.get_mut(key) else {
            return;
        };
        if let Some(request) = entry.waiting.remove(&order) {
            let orders = entry
                .waiting_by_context
                .get_mut(&request.context)
                .expect("waiting request is indexed by context");
            orders.remove(&order);
            if orders.is_empty() {
                entry.waiting_by_context.remove(&request.context);
            }
            self.refresh(key);
        } else if let Some(Running::Active { waiters, .. }) = &mut entry.running
            && waiters.remove(&order).is_some()
            && waiters.is_empty()
        {
            // Drops `_cancel_tx`, which tells the task to stop.
            entry.running = Some(Running::Cancelling);
        }
    }

    /// Starts the first waiting request whose key is idle, attaching every waiting request with
    /// the same key and context to the new task.
    fn start_next(&mut self) -> Option<(K, C, M, CancelSignal)>
    where
        C: Clone,
    {
        let (&order, key) = self.runnable.first_key_value()?;
        let key = key.clone();
        let entry = self.keys.get_mut(&key).expect("runnable key has an entry");
        let head = entry.waiting.get(&order).expect("runnable request is waiting");
        let orders = entry
            .waiting_by_context
            .remove(&head.context)
            .expect("waiting request is indexed by context");
        // The key's first waiting request is also the first with its context.
        let mut requests = orders.into_iter().map(|order| {
            (order, entry.waiting.remove(&order).expect("indexed request is waiting"))
        });
        let (order, Request { context, meta, result_tx }) =
            requests.next().expect("indexed context has a request");
        let mut waiters = BTreeMap::from([(order, result_tx)]);
        waiters.extend(requests.map(|(order, request)| (order, request.result_tx)));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        entry.running =
            Some(Running::Active { context: context.clone(), waiters, _cancel_tx: cancel_tx });
        self.refresh(&key);
        Some((key, context, meta, CancelSignal::new(cancel_rx)))
    }

    /// Marks `key`'s task as finished. Returns its output and the requests still attached to it,
    /// if any, for the caller to deliver after releasing the lock.
    fn finish(
        &mut self,
        key: &K,
        output: Result<O, Cancelled>,
    ) -> Option<(O, BTreeMap<OrderKey, oneshot::Sender<O>>)> {
        let entry = self.keys.get_mut(key).expect("finished key has an entry");
        let running = entry.running.take().expect("finished key was running");
        self.refresh(key);
        match (running, output) {
            (Running::Active { waiters, .. }, Ok(output)) => Some((output, waiters)),
            (Running::Active { .. }, Err(Cancelled(()))) => {
                panic!("work_fn returned Err(Cancelled) while requests were still attached");
            }
            (Running::Cancelling, Ok(_) | Err(Cancelled(()))) => None,
        }
    }
}

/// Closes the queue when the driver goes away, whether or not it was ever polled.
struct CloseOnDrop<K, C, M, O>(Arc<Mutex<State<K, C, M, O>>>);

impl<K, C, M, O> Drop for CloseOnDrop<K, C, M, O> {
    fn drop(&mut self) {
        self.0.lock().close();
    }
}

/// Starts requests in scheduling order, at most `max_concurrency` at a time, until dropped.
///
/// `state` is an argument so that it is dropped after `running`: in-flight work is dropped before
/// the requests waiting on it are failed.
async fn run<W, F, K, C, M, O>(state: CloseOnDrop<K, C, M, O>, max_concurrency: usize, work_fn: W)
where
    W: Fn(K, C, M, CancelSignal) -> F,
    F: Future<Output = Result<O, Cancelled>>,
    K: Eq + Hash + Clone,
    C: Eq + Hash + Clone,
    O: Clone,
{
    let state = &state.0;
    let mut running = FuturesUnordered::new();
    loop {
        let has_free_slot = running.len() < max_concurrency;
        let mut next_task = poll_fn(|cx| {
            if !has_free_slot {
                return Poll::Pending;
            }
            let mut state = state.lock();
            match state.start_next() {
                Some(task) => Poll::Ready(task),
                None => {
                    state.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .fuse();
        futures::select_biased! {
            (key, output) = running.select_next_some() => {
                let delivery = state.lock().finish(&key, output);
                // Deliver after releasing the lock so that woken requesters do not block on it.
                if let Some((output, waiters)) = delivery {
                    for result_tx in waiters.into_values() {
                        let _: Result<(), O> = result_tx.send(output.clone());
                    }
                }
            }
            (key, context, meta, cancel) = next_task => {
                let work = work_fn(key.clone(), context, meta, cancel);
                running.push(work.map(move |output| (key, output)));
            }
        }
    }
}

/// Handle to a request's output. Dropping it before completion removes the request, cancelling
/// its task if no other requests are attached.
#[must_use = "futures do nothing unless you `.await` or poll them; dropping cancels the work item"]
#[derive(Debug)]
pub(crate) struct WorkFuture<K: Eq + Hash + Clone, C: Eq + Hash, M, O> {
    rx: oneshot::Receiver<O>,
    key: K,
    order: OrderKey,
    state: Weak<Mutex<State<K, C, M, O>>>,
}

impl<K: Eq + Hash + Clone + Unpin, C: Eq + Hash, M, O> Future for WorkFuture<K, C, M, O> {
    type Output = Result<O, Cancelled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.rx.poll_unpin(cx).map_err(|oneshot::Canceled| Cancelled(()))
    }
}

impl<K: Eq + Hash + Clone, C: Eq + Hash, M, O> Drop for WorkFuture<K, C, M, O> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock().detach(&self.key, self.order);
        }
    }
}

/// Signal passed to `work_fn` that resolves once all requests for the running task have been
/// dropped.
#[derive(Debug)]
pub(crate) struct CancelSignal(oneshot::Receiver<!>);

impl CancelSignal {
    pub(crate) fn new(rx: oneshot::Receiver<!>) -> Self {
        Self(rx)
    }

    /// Returns `Some(Cancelled)` if cancellation has already been signaled, without blocking.
    pub(crate) fn check(&mut self) -> Option<Cancelled> {
        match self.0.try_recv() {
            Ok(None) => None,
            Err(oneshot::Canceled) => Some(Cancelled(())),
        }
    }

    /// Resolves once cancellation has been requested. Safe to call repeatedly.
    pub(crate) async fn wait(&mut self) -> Cancelled {
        let Err(oneshot::Canceled) = (&mut self.0).await;
        Cancelled(())
    }
}

/// Error indicating that a work item was cancelled or its [`WorkQueue`] was dropped.
///
/// The private field prevents `work_fn` implementations outside this module from constructing
/// `Err(Cancelled)` unless cancellation was actually observed via [`CancelSignal`].
#[derive(thiserror::Error, Clone, Copy, Debug, PartialEq, Eq)]
#[error("work item was cancelled or queue was dropped")]
pub(crate) struct Cancelled(());

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Wake;

    /// Handle for driving an in-flight test task's completion, cancellation check, and teardown.
    struct StartedTask<O> {
        key: &'static str,
        context: u32,
        meta: &'static str,
        finish_tx: oneshot::Sender<O>,
        cancelled_rx: mpsc::UnboundedReceiver<()>,
        finish_teardown_tx: oneshot::Sender<()>,
    }

    impl<O: std::fmt::Debug> StartedTask<O> {
        fn complete(self, output: O) {
            self.finish_tx.send(output).expect("task still waiting for output");
        }

        async fn wait_cancelled(&mut self) {
            assert_eq!(self.cancelled_rx.next().await, Some(()));
        }

        fn finish_teardown(self) {
            self.finish_teardown_tx.send(()).expect("task still waiting for teardown");
        }
    }

    /// Test fixture pairing a [`WorkQueue`] with a channel of [`StartedTask`] handles.
    struct TestHarness<O> {
        queue: WorkQueue<&'static str, u32, &'static str, O>,
        started_rx: mpsc::UnboundedReceiver<StartedTask<O>>,
    }

    impl<O: Clone + Send + 'static> TestHarness<O> {
        fn new(max_concurrency: usize) -> Self {
            let (started_tx, started_rx) = mpsc::unbounded();
            let queue = WorkQueue::new(
                NonZeroUsize::new(max_concurrency).expect("max_concurrency must be nonzero"),
                move |key: &'static str,
                      context: u32,
                      meta: &'static str,
                      mut cancel: CancelSignal| {
                    let started_tx = started_tx.clone();
                    async move {
                        let (finish_tx, finish_rx) = oneshot::channel();
                        let (cancelled_tx, cancelled_rx) = mpsc::unbounded();
                        let (finish_teardown_tx, finish_teardown_rx) = oneshot::channel();
                        let _ = started_tx.unbounded_send(StartedTask {
                            key,
                            context,
                            meta,
                            finish_tx,
                            cancelled_rx,
                            finish_teardown_tx,
                        });
                        let mut cancel_fut = pin!(cancel.wait().fuse());
                        let mut work_fut = pin!(finish_rx.fuse());
                        futures::select_biased! {
                            cancelled = cancel_fut => {
                                let _ = cancelled_tx.unbounded_send(());
                                let _ = finish_teardown_rx.await;
                                Err(cancelled)
                            }
                            res = work_fut => Ok(
                                res.expect("StartedTask dropped without completing")
                            ),
                        }
                    }
                },
            );
            Self { queue, started_rx }
        }

        async fn next_started(&mut self) -> StartedTask<O> {
            self.started_rx.next().await.expect("queue still open")
        }

        fn assert_none_started(&mut self) {
            assert!(self.started_rx.try_recv().is_err());
        }
    }

    #[fuchsia::test]
    async fn executes_in_priority_order_and_fifo_within_priority() {
        let mut harness = TestHarness::new(1);
        let p0 = harness.queue.new_priority();
        let p1 = harness.queue.new_priority();
        let p2 = harness.queue.new_priority();

        // Occupy the single concurrency slot with p0.
        let fut0 = harness.queue.push(p0, "k0", 0, "p0");
        let task0 = harness.next_started().await;
        assert_eq!((task0.key, task0.meta), ("k0", "p0"));

        // Queue p2 first, then two items in p1 (higher priority than p2).
        let fut2 = harness.queue.push(p2, "k2", 0, "p2");
        let fut1_a = harness.queue.push(p1, "k1_a", 0, "p1_a");
        let fut1_b = harness.queue.push(p1, "k1_b", 0, "p1_b");

        // Complete k0 -> p1 runs in FIFO order (k1_a then k1_b).
        task0.complete("p0");
        assert_eq!(fut0.await, Ok("p0"));

        let task1_a = harness.next_started().await;
        assert_eq!((task1_a.key, task1_a.meta), ("k1_a", "p1_a"));
        task1_a.complete("p1_a");
        assert_eq!(fut1_a.await, Ok("p1_a"));

        let task1_b = harness.next_started().await;
        assert_eq!((task1_b.key, task1_b.meta), ("k1_b", "p1_b"));

        // Pushing another item into p1 after earlier p1 items drained still runs ahead of p2.
        let fut1_c = harness.queue.push(p1, "k1_c", 0, "p1_c");
        task1_b.complete("p1_b");
        assert_eq!(fut1_b.await, Ok("p1_b"));

        let task1_c = harness.next_started().await;
        assert_eq!((task1_c.key, task1_c.meta), ("k1_c", "p1_c"));
        task1_c.complete("p1_c");
        assert_eq!(fut1_c.await, Ok("p1_c"));

        let task2 = harness.next_started().await;
        assert_eq!((task2.key, task2.meta), ("k2", "p2"));
        task2.complete("p2");
        assert_eq!(fut2.await, Ok("p2"));
    }

    #[fuchsia::test]
    async fn serializes_same_key_while_allowing_different_keys_concurrently() {
        let mut harness = TestHarness::new(2);

        let p0 = harness.queue.new_priority();
        let fut1 = harness.queue.push(p0, "dup", 1, "first");
        let task1 = harness.next_started().await;
        assert_eq!((task1.key, task1.context), ("dup", 1));

        // Enqueue "dup" with a different context (blocked on task1) and "unique" (runs in slot 2).
        let p1 = harness.queue.new_priority();
        let fut2 = harness.queue.push(p1, "dup", 2, "second");
        let fut3 = harness.queue.push(p1, "unique", 2, "third");

        let task3 = harness.next_started().await;
        assert_eq!((task3.key, task3.meta), ("unique", "third"));

        // Completing "unique" leaves "dup" (context 2) waiting on task1.
        task3.complete(30);
        assert_eq!(fut3.await, Ok(30));
        fasync::yield_now().await;
        harness.assert_none_started();

        // Completing task1 unblocks "dup" (context 2).
        task1.complete(10);
        assert_eq!(fut1.await, Ok(10));
        let task2 = harness.next_started().await;
        assert_eq!((task2.key, task2.context, task2.meta), ("dup", 2, "second"));
        task2.complete(20);
        assert_eq!(fut2.await, Ok(20));
    }

    #[fuchsia::test]
    async fn coalesces_matching_pending_and_active_items_sharing_outcome() {
        let mut harness = TestHarness::<Result<u32, &'static str>>::new(1);

        // Start "k0" (context 1) in the single concurrency slot.
        let fut0 = harness.queue.push(harness.queue.new_priority(), "k0", 1, "m0");
        let task0 = harness.next_started().await;
        assert_eq!((task0.key, task0.meta), ("k0", "m0"));

        // Queue "k1" at higher priority than a subsequent matching "k0" arrival, plus a
        // non-matching "k0" (context 2).
        let fut_k1_a = harness.queue.push(harness.queue.new_priority(), "k1", 1, "k1_first");
        let fut0_late = harness.queue.push(harness.queue.new_priority(), "k0", 1, "m0_late");
        let fut0_other_ctx = harness.queue.push(harness.queue.new_priority(), "k0", 2, "m0_ctx2");
        let fut_k1_b = harness.queue.push(harness.queue.new_priority(), "k1", 1, "k1_second");

        // Completing "k0" immediately resolves `fut0_late` ahead of pending "k1".
        task0.complete(Ok(10));
        assert_eq!(fut0.await, Ok(Ok(10)));
        assert_eq!(fut0_late.await, Ok(Ok(10)));

        // Next, both pending "k1" items coalesce into a single run and share its `Err` outcome,
        // along with a waiter attaching while "k1" is in flight.
        let task_k1 = harness.next_started().await;
        assert_eq!((task_k1.key, task_k1.meta), ("k1", "k1_first"));
        let fut_k1_active = harness.queue.push(harness.queue.new_priority(), "k1", 1, "k1_active");
        task_k1.complete(Err("failed"));
        assert_eq!(fut_k1_a.await, Ok(Err("failed")));
        assert_eq!(fut_k1_b.await, Ok(Err("failed")));
        assert_eq!(fut_k1_active.await, Ok(Err("failed")));

        // Finally, "k0" with non-matching context 2 runs on its own.
        let task0_ctx2 = harness.next_started().await;
        assert_eq!((task0_ctx2.key, task0_ctx2.context), ("k0", 2));
        task0_ctx2.complete(Ok(40));
        assert_eq!(fut0_other_ctx.await, Ok(Ok(40)));
    }

    #[fuchsia::test]
    async fn higher_priority_coalescing_promotes_pending_item() {
        let mut harness = TestHarness::new(1);
        let p_high = harness.queue.new_priority();
        let p_mid = harness.queue.new_priority();
        let p_low = harness.queue.new_priority();

        // Occupy the single concurrency slot so subsequent pushes remain pending.
        let fut_blocker = harness.queue.push(p_high, "blocker", 1, "blocker_meta");
        let task_blocker = harness.next_started().await;
        assert_eq!((task_blocker.key, task_blocker.meta), ("blocker", "blocker_meta"));

        // Enqueue "shared" at `p_low` behind "mid" at `p_mid` (plus a non-matching context on
        // "shared" at `p_low`), then enqueue "shared" with the matching context at the previously
        // allocated `p_high`.
        let fut_shared_low = harness.queue.push(p_low, "shared", 1, "shared_low");
        let fut_mid = harness.queue.push(p_mid, "mid", 1, "mid_meta");
        let fut_shared_other_ctx = harness.queue.push(p_low, "shared", 2, "shared_ctx2");
        let fut_shared_high = harness.queue.push(p_high, "shared", 1, "shared_high");

        // Completing "blocker" must schedule "shared" (context 1) ahead of "mid" and coalesce
        // `fut_shared_low` onto the single `p_high` run.
        task_blocker.complete("done_blocker");
        assert_eq!(fut_blocker.await, Ok("done_blocker"));

        let task_shared = harness.next_started().await;
        assert_eq!(
            (task_shared.key, task_shared.context, task_shared.meta),
            ("shared", 1, "shared_high")
        );
        task_shared.complete("done_shared");
        assert_eq!(fut_shared_high.await, Ok("done_shared"));
        assert_eq!(fut_shared_low.await, Ok("done_shared"));

        // "mid" (`p_mid`) still runs ahead of the non-matching "shared" (context 2) at `p_low`.
        let task_mid = harness.next_started().await;
        assert_eq!((task_mid.key, task_mid.meta), ("mid", "mid_meta"));
        task_mid.complete("done_mid");
        assert_eq!(fut_mid.await, Ok("done_mid"));

        let task_shared_ctx2 = harness.next_started().await;
        assert_eq!(
            (task_shared_ctx2.key, task_shared_ctx2.context, task_shared_ctx2.meta),
            ("shared", 2, "shared_ctx2")
        );
        task_shared_ctx2.complete("done_ctx2");
        assert_eq!(fut_shared_other_ctx.await, Ok("done_ctx2"));
        harness.assert_none_started();
    }

    #[fuchsia::test]
    async fn dropping_promoted_pending_waiter_demotes_key_priority() {
        let mut harness = TestHarness::new(1);
        let p_high = harness.queue.new_priority();
        let p_mid = harness.queue.new_priority();
        let p_low = harness.queue.new_priority();

        let fut_blocker = harness.queue.push(p_high, "blocker", 1, "blocker_meta");
        let task_blocker = harness.next_started().await;

        // Promote runnable "shared" from `p_low` to `p_high` ahead of "mid", then drop the
        // `p_high` waiter before a slot opens so "shared" demotes back behind "mid".
        let fut_shared_low = harness.queue.push(p_low, "shared", 1, "shared_low");
        let fut_mid = harness.queue.push(p_mid, "mid", 1, "mid_meta");
        let fut_shared_high = harness.queue.push(p_high, "shared", 1, "shared_high");
        drop(fut_shared_high);

        // Also queue blocked items on "blocker" (context 2) at `p_low` and `p_high` and drop the
        // `p_high` waiter while "blocker" is still running.
        let fut_blocker_ctx2_low = harness.queue.push(p_low, "blocker", 2, "blocker_ctx2_low");
        let fut_blocker_ctx2_high = harness.queue.push(p_high, "blocker", 2, "blocker_ctx2_high");
        drop(fut_blocker_ctx2_high);

        task_blocker.complete("done_blocker");
        assert_eq!(fut_blocker.await, Ok("done_blocker"));

        // "mid" (`p_mid`) must run before both demoted `p_low` items.
        let task_mid = harness.next_started().await;
        assert_eq!((task_mid.key, task_mid.meta), ("mid", "mid_meta"));
        task_mid.complete("done_mid");
        assert_eq!(fut_mid.await, Ok("done_mid"));

        let task_shared = harness.next_started().await;
        assert_eq!((task_shared.key, task_shared.meta), ("shared", "shared_low"));
        task_shared.complete("done_shared");
        assert_eq!(fut_shared_low.await, Ok("done_shared"));

        let task_blocker_ctx2 = harness.next_started().await;
        assert_eq!(
            (task_blocker_ctx2.key, task_blocker_ctx2.context, task_blocker_ctx2.meta),
            ("blocker", 2, "blocker_ctx2_low")
        );
        task_blocker_ctx2.complete("done_blocker_ctx2");
        assert_eq!(fut_blocker_ctx2_low.await, Ok("done_blocker_ctx2"));
        harness.assert_none_started();
    }

    #[fuchsia::test]
    async fn dropping_future_cancels_pending_and_unshared_running_with_teardown_barrier() {
        // Use two concurrency slots so that one stays free throughout: only the busy key, not the
        // concurrency limit, can then hold back the requests queued behind task 1.
        let mut harness = TestHarness::new(2);

        // Start task 1 ("shared_key", context 1).
        let fut1 = harness.queue.push(harness.queue.new_priority(), "shared_key", 1, "run1");
        let mut task1 = harness.next_started().await;
        assert_eq!((task1.key, task1.meta), ("shared_key", "run1"));

        // Queue and immediately drop a pending item so it never starts.
        let fut_dropped_pending =
            harness.queue.push(harness.queue.new_priority(), "dropped_pending", 1, "never");
        drop(fut_dropped_pending);

        // Queue non-matching "shared_key" (context 2) behind task 1.
        let fut2 = harness.queue.push(harness.queue.new_priority(), "shared_key", 2, "run2");

        // Dropping `fut1` triggers task 1's `CancelSignal`, keeping "shared_key" serialized until
        // async teardown finishes.
        drop(fut1);
        task1.wait_cancelled().await;
        harness.assert_none_started();

        // A matching context pushed while task 1 is tearing down must NOT attach to the dying task;
        // it queues behind teardown and runs afterward.
        let fut3 = harness.queue.push(harness.queue.new_priority(), "shared_key", 1, "run3");
        // Yield to the driver, which the push woke, so that a task wrongly started in the free
        // slot would be observed.
        fasync::yield_now().await;
        harness.assert_none_started();

        // Finishing task 1's teardown starts `fut2` (skipping `dropped_pending`), then `fut3`.
        task1.finish_teardown();
        let task2 = harness.next_started().await;
        assert_eq!((task2.key, task2.meta), ("shared_key", "run2"));
        task2.complete(2);
        assert_eq!(fut2.await, Ok(2));

        let task3 = harness.next_started().await;
        assert_eq!((task3.key, task3.meta), ("shared_key", "run3"));
        task3.complete(3);
        assert_eq!(fut3.await, Ok(3));
        harness.assert_none_started();
    }

    #[fuchsia::test]
    async fn dropping_one_waiter_does_not_cancel_shared_task() {
        let mut harness = TestHarness::new(1);

        // Case 1: second waiter attaches while task is actively running.
        let fut_a = harness.queue.push(harness.queue.new_priority(), "shared", 1, "a");
        let mut task_shared = harness.next_started().await;
        let fut_b = harness.queue.push(harness.queue.new_priority(), "shared", 1, "b");

        drop(fut_a);
        // The task reports a cancellation only when the driver polls it, so yield to the driver.
        fasync::yield_now().await;
        assert!(task_shared.cancelled_rx.try_recv().is_err());
        task_shared.complete("done_1");
        assert_eq!(fut_b.await, Ok("done_1"));

        // Case 2: two waiters coalesce while pending behind a blocker.
        let fut_block = harness.queue.push(harness.queue.new_priority(), "blocker", 1, "block");
        let task_block = harness.next_started().await;
        let fut_c = harness.queue.push(harness.queue.new_priority(), "shared_pending", 1, "c");
        let fut_d = harness.queue.push(harness.queue.new_priority(), "shared_pending", 1, "d");

        task_block.complete("unblocked");
        assert_eq!(fut_block.await, Ok("unblocked"));
        let mut task_coalesced = harness.next_started().await;
        assert_eq!(task_coalesced.key, "shared_pending");

        drop(fut_c);
        // As above, yield to the driver before checking for a cancellation.
        fasync::yield_now().await;
        assert!(task_coalesced.cancelled_rx.try_recv().is_err());
        task_coalesced.complete("done_2");
        assert_eq!(fut_d.await, Ok("done_2"));
        harness.assert_none_started();
    }

    #[fuchsia::test]
    async fn dropping_queue_cancels_pending_and_running_after_dropping_in_flight_futures() {
        struct WakeObserver {
            work_dropped: Arc<AtomicBool>,
            dropped_before_wake: AtomicBool,
            inner: Mutex<Waker>,
        }

        impl Wake for WakeObserver {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.dropped_before_wake
                    .store(self.work_dropped.load(Ordering::SeqCst), Ordering::SeqCst);
                self.inner.lock().wake_by_ref();
            }
        }

        let work_dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = mpsc::unbounded();

        let flag = Arc::clone(&work_dropped);
        let queue = WorkQueue::<_, _, _, ()>::new(
            NonZeroUsize::MIN,
            move |key: &'static str, _ctx: u32, _meta: (), mut cancel: CancelSignal| {
                let started_tx = started_tx.clone();
                let drop_guard =
                    scopeguard::guard(Arc::clone(&flag), |flag| flag.store(true, Ordering::SeqCst));
                async move {
                    let _drop_guard = drop_guard;
                    let _ = started_tx.unbounded_send(key);
                    Err(cancel.wait().await)
                }
            },
        );
        let p0 = queue.new_priority();

        let await_cancelled_after_work_dropped = |mut fut: WorkFuture<_, _, _, ()>| {
            let observer = Arc::new(WakeObserver {
                work_dropped: Arc::clone(&work_dropped),
                dropped_before_wake: AtomicBool::new(false),
                inner: Mutex::new(Waker::noop().clone()),
            });
            // Register `observer` before `drop(queue)` so `oneshot::Sender::drop` wakes it
            // synchronously inside `CloseOnDrop::drop` regardless of executor scheduling.
            assert_eq!(
                fut.poll_unpin(&mut Context::from_waker(&Waker::from(Arc::clone(&observer)))),
                Poll::Pending,
            );
            async move {
                let res = poll_fn(|cx| {
                    *observer.inner.lock() = cx.waker().clone();
                    fut.poll_unpin(&mut Context::from_waker(&Waker::from(Arc::clone(&observer))))
                })
                .await;
                assert!(observer.dropped_before_wake.load(Ordering::SeqCst));
                res
            }
        };

        // Leave one task running and one request waiting behind `max_concurrency = 1`.
        let fut_running = queue.push(p0, "running", 1, ());
        assert_eq!(started_rx.next().await, Some("running"));
        let fut_waiting = queue.push(p0, "waiting", 1, ());

        let check_running = await_cancelled_after_work_dropped(fut_running);
        let check_waiting = await_cancelled_after_work_dropped(fut_waiting);

        drop(queue);

        // In-flight work is dropped before any request is failed.
        assert_eq!(
            futures::future::join(check_running, check_waiting).await,
            (Err(Cancelled(())), Err(Cancelled(()))),
        );
    }

    #[fuchsia::test]
    async fn queue_outlives_enclosing_child_scope() {
        let child_scope = fasync::Scope::new();
        let mut harness = child_scope.compute(async { TestHarness::new(1) }).await;
        child_scope.cancel().await;

        let fut = harness.queue.push(harness.queue.new_priority(), "k", 1, "m");
        let task = harness.next_started().await;
        task.complete("ok");
        assert_eq!(fut.await, Ok("ok"));
    }

    #[fuchsia::test]
    async fn cancelled_scope_cleans_up_running_and_pending() {
        // Cancelling the internal scope while `WorkQueue` handles remain alive resolves running,
        // blocked, pending, and subsequent pushes to `Err(Cancelled)` without hanging.
        let mut harness = TestHarness::<&'static str>::new(1);
        let p0 = harness.queue.new_priority();
        let fut_running = harness.queue.push(p0, "running", 1, "r1");
        let _task = harness.next_started().await;
        let fut_blocked = harness.queue.push(p0, "running", 2, "r2");
        let fut_pending = harness.queue.push(p0, "pending", 1, "p");

        harness.queue._driver.to_handle().cancel().await;

        assert_eq!(fut_running.await, Err(Cancelled(())));
        assert_eq!(fut_blocked.await, Err(Cancelled(())));
        assert_eq!(fut_pending.await, Err(Cancelled(())));

        let fut_after = harness.queue.push(p0, "after", 1, "a");
        assert_eq!(fut_after.await, Err(Cancelled(())));
        harness.assert_none_started();
    }

    #[fuchsia::test]
    async fn cancel_signal_can_be_observed_repeatedly() {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut cancel = CancelSignal::new(cancel_rx);
        assert_eq!(cancel.check(), None);

        {
            let mut wait_fut = pin!(cancel.wait());
            assert_eq!(futures::poll!(&mut wait_fut), Poll::Pending);
            drop(cancel_tx);
            assert_eq!(wait_fut.await, Cancelled(()));
        }

        assert_eq!(cancel.check(), Some(Cancelled(())));
        assert_eq!(cancel.wait().await, Cancelled(()));
        assert_eq!(cancel.check(), Some(Cancelled(())));
    }

    #[fuchsia::test]
    async fn push_all_enqueues_batch_and_dropping_iterator_cancels_unconsumed() {
        let mut harness = TestHarness::new(1);
        let p0 = harness.queue.new_priority();

        // Park the driver, then verify an empty `push_all` preserves the parked driver waker.
        fasync::yield_now().await;
        assert_eq!(harness.queue.push_all(p0, []).count(), 0);

        // Take only the first `WorkFuture` and drop the iterator; unconsumed requests must detach.
        let mut iter = harness
            .queue
            .push_all(p0, [("first", 1, "m1"), ("second", 1, "m2"), ("third", 1, "m3")]);
        let fut1 = iter.next().unwrap();
        drop(iter);

        let task1 = harness.next_started().await;
        assert_eq!((task1.key, task1.meta), ("first", "m1"));
        task1.complete("done1");
        assert_eq!(fut1.await, Ok("done1"));
        fasync::yield_now().await;
        harness.assert_none_started();

        // Batch pushes preserve FIFO order and coalesce duplicates within the batch.
        let fut_block = harness.queue.push(p0, "block", 1, "block");
        let task_block = harness.next_started().await;
        let futs: Vec<_> =
            harness.queue.push_all(p0, [("a", 1, "a1"), ("b", 1, "b1"), ("a", 1, "a2")]).collect();
        let [fut_a1, fut_b, fut_a2]: [_; 3] = futs.try_into().unwrap();

        task_block.complete("unblocked");
        assert_eq!(fut_block.await, Ok("unblocked"));

        let task_a = harness.next_started().await;
        assert_eq!((task_a.key, task_a.meta), ("a", "a1"));
        task_a.complete("done_a");
        assert_eq!(fut_a1.await, Ok("done_a"));
        assert_eq!(fut_a2.await, Ok("done_a"));

        let task_b = harness.next_started().await;
        assert_eq!((task_b.key, task_b.meta), ("b", "b1"));
        task_b.complete("done_b");
        assert_eq!(fut_b.await, Ok("done_b"));
        harness.assert_none_started();
    }
}
