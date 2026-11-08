// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(non_upper_case_globals)]

use crate::objects::{
    BinderObject, BinderObjectFlags, BinderObjectRef, Handle, LocalBinderObject, RefCountActions,
    StrongRefGuard,
};
use crate::resource_accessor::{
    RemoteMemoryAccessor, RemoteResourceAccessor, ResourceAccessor, get_resource_accessor,
};
use crate::shared_memory::SharedMemory;
use crate::thread::{
    BinderThread, BinderThreadState, Command, QueuedCommand, RegistrationState,
    generate_dead_replies,
};
use starnix_core::mm::MemoryAccessor;
use starnix_core::mm::memory::MemoryObject;
use starnix_core::mutable_state::Guard;
use starnix_core::task::{Kernel, Pid, Task};
use starnix_core::vfs::FdNumber;
use starnix_logging::{log_trace, log_warn, track_stub};
use starnix_sync::{
    BinderAvailableThreadsLock, BinderFreezeLevel, BinderProcessSharedMemoryLevel,
    BinderProcessStateLevel, BinderThreadPoolLevel, LockDepGuard, LockDepMutex,
};
use starnix_types::ownership::{
    DropGuard, OwnedRef, Releasable, ReleaseGuard, Share, TempRef, WeakRef, release_after,
};
use starnix_uapi::arc_key::ArcKey;
use starnix_uapi::auth::Credentials;
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    binder_driver_command_protocol, binder_driver_command_protocol_BC_ACQUIRE,
    binder_driver_command_protocol_BC_ACQUIRE_DONE, binder_driver_command_protocol_BC_DECREFS,
    binder_driver_command_protocol_BC_INCREFS, binder_driver_command_protocol_BC_INCREFS_DONE,
    binder_driver_command_protocol_BC_RELEASE, binder_frozen_state_info, binder_uintptr_t, errno,
    error,
};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::vec::Vec;

#[derive(Debug, Default)]
pub struct BinderProcessState {
    /// Maximum number of thread to spawn.
    pub max_thread_count: usize,
    /// The number of threads in the pool with registration state `Auxiliary`.
    pub auxiliary_threads_count: usize,
    /// Whether a new thread has been requested, but not registered yet.
    pub thread_requested: bool,
    /// Binder objects hosted by the process shared with other processes.
    pub objects: BTreeMap<UserAddress, Arc<BinderObject>>,
    /// Handle table of remote binder objects.
    pub handles: ReleaseGuard<HandleTable>,
    /// State associated with active transactions, keyed by the userspace addresses of the buffers
    /// allocated to them. When the process frees a transaction buffer with `BC_FREE_BUFFER`, the
    /// state is dropped, releasing temporary strong references and the memory allocated to the
    /// transaction.
    pub active_transactions: BTreeMap<UserAddress, ReleaseGuard<ActiveTransaction>>,
    /// The list of processes that should be notified if this process dies.
    pub death_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// The death notifications requested by this process, keyed by their cookie.
    pub death_notifications: BTreeMap<binder_uintptr_t, DeathNotificationState>,
    /// Whether the binder connection for this process is closed. Once closed, any blocking
    /// operation will be aborted and return an EBADF error.
    pub closed: bool,
    /// Pending commands.
    pub command_queue: VecDeque<QueuedCommand>,
    /// Freeze notifications that have been dispatched to userspace (via BR_FROZEN_BINDER)
    /// but not yet acknowledged via BC_FREEZE_NOTIFICATION_DONE.
    pub in_flight_freeze_notifications: BTreeSet<binder_uintptr_t>,
    /// Clear freeze notification requests that arrived while a BR_FROZEN_BINDER for the
    /// same cookie was in-flight, waiting for BC_FREEZE_NOTIFICATION_DONE before sending
    /// BR_CLEAR_FREEZE_NOTIFICATION_DONE.
    pub pending_clear_freeze_notifications: BTreeSet<binder_uintptr_t>,
    /// Number of threads currently being unregistered and released that had pending transactions
    /// when removed from `thread_pool`.
    pub releasing_threads_with_pending_transactions: usize,
}

/// The state of the death notifications that a process requested for a given cookie.
///
/// This mirrors the lifecycle of `binder_ref_death` in Linux. Userspace may release the object
/// identified by the cookie as soon as it receives `BR_CLEAR_DEATH_NOTIFICATION_DONE` (libbinder
/// drops the weak reference that it took for the notification), so that command must never be
/// delivered while a `BR_DEAD_BINDER` for the same cookie is queued or still being processed.
#[derive(Debug, Default)]
pub struct DeathNotificationState {
    /// Whether the notification is registered with the owner of the binder object, which will
    /// queue a `BR_DEAD_BINDER` when it dies.
    pub registered: bool,
    /// The number of `BR_DEAD_BINDER` queued for, or delivered to, userspace that have not been
    /// acknowledged with `BC_DEAD_BINDER_DONE` yet.
    pub pending_dead_binders: usize,
    /// The number of `BR_CLEAR_DEATH_NOTIFICATION_DONE` deferred until a `BC_DEAD_BINDER_DONE`,
    /// because their `BC_CLEAR_DEATH_NOTIFICATION` arrived while a `BR_DEAD_BINDER` was pending.
    /// This is the equivalent of `BINDER_WORK_DEAD_BINDER_AND_CLEAR` in Linux.
    pub deferred_clears: usize,
}

impl DeathNotificationState {
    /// Whether a death notification is currently active (not yet cleared by
    /// `BC_CLEAR_DEATH_NOTIFICATION`). Like `ref->death != NULL` in Linux, this remains true after
    /// `BR_DEAD_BINDER` is queued until `BC_CLEAR_DEATH_NOTIFICATION` arrives.
    fn is_set(&self) -> bool {
        self.registered || self.pending_dead_binders > self.deferred_clears
    }

    /// Whether the state no longer tracks anything, and can be dropped.
    fn is_empty(&self) -> bool {
        !self.registered && self.pending_dead_binders == 0 && self.deferred_clears == 0
    }
}

#[derive(Debug, Default)]
pub struct BinderFreezeState {
    /// The list of processes that should be notified if this process is frozen.
    pub freeze_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// Status of the binder freeze.
    pub freeze_status: FreezeStatus,
    /// Number of in-flight `BINDER_FREEZE(enable=1)` operations waiting for transactions to drain
    /// in the current `thaw_generation`.
    pub freezing_count: usize,
    /// Monotonic counter incremented on every `thaw()` to invalidate any in-flight
    /// `BINDER_FREEZE(enable=1)` waiters that started before the thaw.
    pub thaw_generation: u64,
    /// Whether a `BINDER_FREEZE(enable=1)` operation has completed without a subsequent `thaw()`.
    pub committed_frozen: bool,
}

impl BinderFreezeState {
    pub fn begin_freeze(&mut self) -> u64 {
        self.freezing_count += 1;
        self.freeze_status.frozen = true;
        self.freeze_status.has_async_recv = false;
        self.freeze_status.has_sync_recv = false;
        self.thaw_generation
    }

    pub fn cancel_freeze(
        &mut self,
        expected_generation: u64,
        state: &mut BinderProcessState,
    ) -> bool {
        if self.thaw_generation != expected_generation {
            return false;
        }
        self.freezing_count = self.freezing_count.saturating_sub(1);
        if self.freezing_count == 0 && !self.committed_frozen {
            self.freeze_status.frozen = false;
            return state.unfreeze_oneway_transactions();
        }
        false
    }

    pub fn notify_frozen(&mut self) -> Vec<(OwnedRef<BinderProcess>, Command)> {
        self.freezing_count = self.freezing_count.saturating_sub(1);
        self.committed_frozen = true;
        self.freeze_status.frozen = true;
        let mut pending_notifications = Vec::new();
        self.freeze_subscribers.retain(|(proc, cookie)| {
            if let Some(p) = proc.re_own() {
                pending_notifications.push((
                    p,
                    Command::FrozenBinder(binder_frozen_state_info {
                        cookie: *cookie,
                        is_frozen: 1,
                        reserved: 0,
                    }),
                ));
                true
            } else {
                false
            }
        });
        pending_notifications
    }

    #[cfg(test)]
    pub fn freeze(&mut self) -> Vec<(OwnedRef<BinderProcess>, Command)> {
        self.begin_freeze();
        self.notify_frozen()
    }

    pub fn thaw(
        &mut self,
        state: &mut BinderProcessState,
    ) -> (Vec<(OwnedRef<BinderProcess>, Command)>, bool) {
        self.thaw_generation += 1;
        self.freezing_count = 0;
        self.committed_frozen = false;
        self.freeze_status.frozen = false;
        self.freeze_status.has_async_recv = false;
        self.freeze_status.has_sync_recv = false;
        let should_wake = state.unfreeze_oneway_transactions();
        let mut pending_notifications = Vec::new();
        self.freeze_subscribers.retain(|(proc, cookie)| {
            if let Some(p) = proc.re_own() {
                pending_notifications.push((
                    p,
                    Command::FrozenBinder(binder_frozen_state_info {
                        cookie: *cookie,
                        is_frozen: 0,
                        reserved: 0,
                    }),
                ));
                true
            } else {
                false
            }
        });
        (pending_notifications, should_wake)
    }
}

impl BinderProcessState {
    pub fn unfreeze_oneway_transactions(&mut self) -> bool {
        let mut had_frozen_oneway = false;
        for transaction in self.active_transactions.values_mut() {
            if matches!(transaction.request_type, RequestType::Oneway { .. })
                && !transaction.is_pending
            {
                transaction.is_pending = true;
                had_frozen_oneway = true;
            }
        }
        had_frozen_oneway && !self.command_queue.is_empty()
    }

    pub fn is_command_runnable(&self, command: &Command) -> bool {
        if let Command::OnewayTransaction(data) = command {
            self.active_transactions.get(&data.buffers.data.address).is_none_or(|t| t.is_pending)
        } else {
            true
        }
    }

    pub fn has_runnable_commands(&self) -> bool {
        self.command_queue.iter().any(|queued| self.is_command_runnable(&queued.command))
    }

    pub fn enqueue_command(&mut self, command: QueuedCommand) -> bool {
        let is_runnable = self.is_command_runnable(&command.command);
        crate::trace::on_command_enqueued(&command.command, command.trace_id);
        self.command_queue.push_back(command);
        is_runnable
    }
}

#[derive(Debug, Default, Clone)]

pub struct FreezeStatus {
    pub frozen: bool,
    /// Indicates whether the process has received any sync calls since last
    /// freeze (cleared at freeze/unfreeze)
    pub has_sync_recv: bool,
    /// Indicates whether the process has received any async calls since last
    /// freeze (cleared at freeze/unfreeze)
    pub has_async_recv: bool,
}

/// An active binder transaction.
#[derive(Debug)]
pub struct ActiveTransaction {
    /// The transaction's request type.
    pub request_type: RequestType,
    /// Whether this transaction is still pending for `BINDER_FREEZE` and `BINDER_GET_FROZEN_INFO`.
    pub is_pending: bool,
    /// The state associated with the transaction.
    ///
    /// Invariant: `state.proc` always points to the receiving [`BinderProcess`] whose
    /// `active_transactions` map owns this [`ActiveTransaction`].
    pub state: ReleaseGuard<TransactionState>,
}

impl Releasable for ActiveTransaction {
    type Context<'a> = &'a mut RefCountActions;

    fn release<'a>(self, context: Self::Context<'a>) {
        self.state.release(context);
    }
}

/// State held for the duration of a transaction. When a transaction completes (or fails), this
/// state is dropped, decrementing temporary strong references to binder objects.
#[derive(Debug)]
pub struct TransactionState {
    /// The process whose handle table `handles` belong to. When stored in
    /// `BinderProcessState::active_transactions`, this always refers to that owning process.
    pub proc: WeakRef<BinderProcess>,
    /// The target process.
    pub key: Pid,
    /// The remote resource accessor of the target process. This is None when the receiving process
    /// is a local process.
    pub remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,
    /// The objects to strongly owned for the duration of the transaction.
    pub guards: smallvec::SmallVec<[StrongRefGuard; 2]>,
    /// The handles to decrement their strong reference count.
    pub handles: smallvec::SmallVec<[Handle; 2]>,
    /// The FDs of the target process that the kernel is responsible for closing, because they were
    /// sent with BINDER_TYPE_FDA.
    pub owned_fds: smallvec::SmallVec<[FdNumber; 2]>,
}

impl TransactionState {
    /// Decrements temporary strong references for `self.handles` in `proc_state`, recording any
    /// deferred refcount actions in `actions`.
    fn release_handles(
        &mut self,
        proc_state: &mut BinderProcessGuard<'_>,
        actions: &mut RefCountActions,
    ) {
        debug_assert!(WeakRef::ptr_eq(&self.proc, &proc_state.base.weak_self));
        for handle in self.handles.drain(..) {
            if let Err(error) = proc_state.handles.dec_strong(handle.object_index(), actions) {
                // Ignore the error because there is little we can do about it.
                // Panicking would be wrong, in case the client issued an extra strong decrement.
                log_warn!(
                    "Error when dropping transaction state for process {}: {:?}",
                    proc_state.base.key,
                    error
                );
            }
        }
    }
}

impl Releasable for TransactionState {
    type Context<'a> = &'a mut RefCountActions;

    fn release<'a>(mut self, drop_actions: Self::Context<'a>) {
        log_trace!("Releasing binder TransactionState");
        // Release the owned objects unconditionally.
        for guard in self.guards.drain(..) {
            guard.release(drop_actions);
        }
        if !self.handles.is_empty() {
            if let Some(proc) = self.proc.upgrade().map(TempRef::into_static) {
                // Release handles only if the owning process is still alive.
                let mut proc_state = proc.lock();
                self.release_handles(&mut proc_state, drop_actions);
            }
        }

        // Close the owned fd.
        if !self.owned_fds.is_empty() {
            if let Some(task) = get_task_for_thread_group(&self.key) {
                let resource_accessor =
                    get_resource_accessor(task.deref(), &self.remote_resource_accessor);
                if let Err(error) = resource_accessor.close_files(self.owned_fds.into_vec()) {
                    log_warn!(
                        "Error when dropping transaction state while closing fd for task {}: {:?}",
                        task.tid,
                        error
                    );
                }
            }
        }
    }
}

/// Transaction state held during the processing and dispatching of a transaction. In the event of
/// an error while dispatching a transaction, this object is meant to cleanup any temporary
/// resources that were allocated. Once a transaction has been dispatched successfully, this object
/// can be converted into a [`TransactionState`] to be held for the lifetime of the transaction.
pub struct TransientTransactionState<'a> {
    /// The part of the transient state that will live for the lifetime of the transaction.
    pub state: Option<ReleaseGuard<TransactionState>>,
    /// The task to which the transient file descriptors belong.
    pub accessor: &'a dyn ResourceAccessor,
    /// The file descriptors to close in case of an error.
    pub transient_fds: smallvec::SmallVec<[FdNumber; 2]>,
    /// A guard that will ensure a panic on drop if `state` has not been released.
    pub drop_guard: DropGuard,
}

impl<'a> Releasable for TransientTransactionState<'a> {
    type Context<'b> = ();
    fn release<'b>(self, _: ()) {
        if !self.transient_fds.is_empty() {
            let _ = self.accessor.close_files(self.transient_fds.into_vec());
        }
        let mut drop_actions = RefCountActions::default();
        self.state.release(&mut drop_actions);
        drop_actions.release(());
        self.drop_guard.disarm();
    }
}

impl<'a> std::fmt::Debug for TransientTransactionState<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransientTransactionState")
            .field("state", &self.state)
            .field("accessor", &self.accessor)
            .field("transient_fds", &self.transient_fds)
            .finish()
    }
}

impl<'a> TransientTransactionState<'a> {
    /// Creates a new [`TransientTransactionState`], whose resources will belong to `accessor` and
    /// `target_proc` for FDs and binder handles respectively.
    pub fn new(accessor: &'a dyn ResourceAccessor, target_proc: &BinderProcess) -> Self {
        TransientTransactionState {
            state: Some(
                TransactionState {
                    proc: target_proc.weak_self.clone(),
                    key: target_proc.key.clone(),
                    remote_resource_accessor: target_proc.remote_resource_accessor.clone(),
                    guards: smallvec::SmallVec::new(),
                    handles: smallvec::SmallVec::new(),
                    owned_fds: smallvec::SmallVec::new(),
                }
                .into(),
            ),
            accessor,
            transient_fds: smallvec::SmallVec::new(),
            drop_guard: DropGuard::default(),
        }
        .into()
    }

    /// Schedule `handle` to have its strong reference count decremented when the transaction ends
    /// (both in case of success or failure).
    pub fn push_handle(&mut self, handle: Handle) {
        self.state.as_mut().unwrap().handles.push(handle)
    }

    /// Schedule `guard` to be released when the transaction ends (both in case of success or
    /// failure).
    pub fn push_guard(&mut self, guard: StrongRefGuard) {
        self.state.as_mut().unwrap().guards.push(guard);
    }

    /// Schedule `fd` to be removed from the file descriptor table when the transaction ends (both
    /// in case of success or failure).
    pub fn push_owned_fd(&mut self, fd: FdNumber) {
        self.state.as_mut().unwrap().owned_fds.push(fd)
    }

    /// Schedule `fd` to be removed from the file descriptor table if the transaction fails.
    pub fn push_transient_fd(&mut self, fd: FdNumber) {
        self.transient_fds.push(fd)
    }

    pub fn into_state(mut self) -> ReleaseGuard<TransactionState> {
        // Clear the transient FD list, so that these FDs no longer get closed.
        self.transient_fds.clear();
        let result = self.state.take().unwrap();
        self.release(());
        result
    }
}

/// The request type of a transaction.
#[derive(Debug)]
pub enum RequestType {
    /// A fire-and-forget request, which has special ordering guarantees.
    Oneway {
        /// The recipient of the transaction. Oneway transactions are ordered for a given binder
        /// object.
        object: Arc<BinderObject>,
    },
    /// A request/response type.
    RequestResponse,
}

/// A thread-safe stack of available threads for a [`BinderProcess`].
///
/// Threads register themselves here when they become available to process commands. Work is handed
/// to the most recently available thread first, as Linux does (a thread is added at the head of
/// `proc->waiting_threads` when it starts waiting, and `binder_select_thread_ilocked` picks the
/// head), so that a few threads with warm caches handle most of the work instead of rotating it
/// through the whole thread pool.
///
/// This stack is backed by [`LockDepMutex`] with [`BinderAvailableThreadsLock`], a terminal
/// lock level acquired after [`BinderProcessStateLevel`] and `BinderThreadStateLock`;
/// nothing may be acquired while it is held, and it must not be taken while holding another
/// terminal lock.
///
/// Using a priority-inheriting mutex with no userspace spinning avoids the `sched_yield`
/// spinning / starvation livelock seen with `SegQueue` under deadline scheduler roles
/// (see https://fxbug.dev/577150201).
#[derive(Debug)]
pub struct AvailableThreads {
    inner: LockDepMutex<Vec<WeakRef<BinderThread>>, BinderAvailableThreadsLock>,
}

impl Default for AvailableThreads {
    fn default() -> Self {
        Self::new()
    }
}

impl AvailableThreads {
    /// Creates a new empty `AvailableThreads` stack.
    pub fn new() -> Self {
        Self { inner: LockDepMutex::new(Vec::new()) }
    }

    /// Pops the most recently pushed thread, releasing the lock immediately before returning.
    ///
    /// May briefly block while acquiring the inner lock.
    /// Returns `None` if the stack is empty.
    #[inline]
    pub fn pop(&self) -> Option<WeakRef<BinderThread>> {
        self.inner.lock().pop()
    }

    /// Pushes `thread` as the most recently available thread.
    ///
    /// A thread can stop waiting without being popped, e.g. when it is kicked, which leaves its
    /// entry behind until it waits again. Any entry for `thread` is removed first, so that each
    /// thread is queued at most once, at the top of the stack.
    ///
    /// May briefly block while acquiring the inner lock.
    #[inline]
    pub fn push(&self, thread: WeakRef<BinderThread>) {
        let mut inner = self.inner.lock();
        inner.retain(|queued| !WeakRef::ptr_eq(queued, &thread));
        inner.push(thread);
    }

    /// Removes the entry of `thread`, if any.
    ///
    /// May briefly block while acquiring the inner lock.
    #[inline]
    pub fn remove(&self, thread: &WeakRef<BinderThread>) {
        self.inner.lock().retain(|queued| !WeakRef::ptr_eq(queued, thread));
    }

    /// Returns `true` if the stack is empty.
    ///
    /// May briefly block while acquiring the inner lock.
    ///
    /// # Concurrency Notes
    /// - In [`BinderProcess::enqueue_command`], this is called while holding
    ///   [`BinderProcess::state`] (where popping and locking a [`BinderThread`] is not allowed) to
    ///   check whether a thread became available after
    ///   [`BinderProcess::try_enqueue_on_available_thread`] returned `Err`. Because
    ///   `BinderDriver::handle_thread_read` drops `thread_state` (which calls [`push`](Self::push))
    ///   before dropping `proc_state`, any `push` ordered before `BinderProcess::state` is
    ///   acquired is guaranteed to be visible to this check.
    /// - A `false` return indicates entries are present in the stack, but queued entries may be
    ///   dead [`WeakRef`]s (if the target thread has exited) or live threads that are no longer
    ///   available.
    /// - Entries at the bottom of the stack are only popped after all the newer entries, so a
    ///   thread removes its entry when its wait fails (e.g. when it is interrupted, see
    ///   `BinderDriver::handle_thread_read`) or when it exits (see
    ///   [`BinderProcess::unregister_thread`]). Otherwise, its stale entry could stay queued below
    ///   the threads that keep becoming available, and make
    ///   [`BinderProcessGuard::should_request_thread_locked`] assume that the process has an idle
    ///   thread when all its threads are busy.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

#[derive(Debug)]
pub struct BinderProcess {
    /// Weak reference to self.
    pub weak_self: WeakRef<BinderProcess>,

    /// The identifier of the process associated with this binder process.
    pub key: Pid,

    /// Resource accessor to access remote resource in case of a remote binder process. None in
    /// case of a local process.
    pub remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,

    /// The credentials of the process when it opened the binder device.
    pub creds: Arc<Credentials>,

    // The mutable state of `BinderProcess` is protected by 4 locks: `shared_memory`,
    // `freeze_state`, `state`, and `thread_pool`. For ordering purposes, locks must be taken in
    // the order they are defined in this struct, even across `BinderProcess` instances.
    // Moreover, any `BinderThread` lock must be ordered after `state` and `thread_pool` from a
    // `BinderProcess`. `available_threads` has its own terminal lock. It may be taken while
    // holding any of the above or a `BinderThread` lock, and no lock may be taken while holding
    // it.
    /// The [`SharedMemory`] region mapped in both the driver and the binder process. Allows for
    /// transactions to copy data once from the sender process into the receiver process.
    pub shared_memory: LockDepMutex<Option<SharedMemory>, BinderProcessSharedMemoryLevel>,

    /// The freeze state of the `BinderProcess`.
    pub freeze_state: LockDepMutex<BinderFreezeState, BinderFreezeLevel>,

    /// The main mutable state of the `BinderProcess`.
    pub state: LockDepMutex<BinderProcessState, BinderProcessStateLevel>,

    /// A WaitQueue used solely to notify process-level waiters (like `epoll` waiters) about
    /// FD events when commands are available.
    pub process_waiters: starnix_core::task::WaitQueue,

    /// A WaitQueue used to notify waiters waiting for pending transactions to drain during
    /// `BINDER_FREEZE`.
    pub freeze_waiters: starnix_core::task::WaitQueue,

    /// A queue of threads that are available to handle commands.
    /// A thread adds itself to this queue when it transitions to being available.
    pub available_threads: Arc<AvailableThreads>,

    /// The set of threads that are interacting with the binder driver.
    /// Protected by a dedicated mutex to allow thread lookup on the ioctl hot path without
    /// acquiring the contended process `state` mutex.
    pub thread_pool: LockDepMutex<BTreeMap<Pid, OwnedRef<BinderThread>>, BinderThreadPoolLevel>,
}

pub struct BinderProcessGuard<'a>(Guard<'a, BinderProcess, LockDepGuard<'a, BinderProcessState>>);

impl<'a> Deref for BinderProcessGuard<'a> {
    type Target = Guard<'a, BinderProcess, LockDepGuard<'a, BinderProcessState>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for BinderProcessGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl BinderProcess {
    #[allow(clippy::let_and_return)]
    pub fn new(
        key: Pid,
        remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,
        creds: Arc<Credentials>,
    ) -> OwnedRef<BinderProcess> {
        let result = OwnedRef::new_cyclic(|weak_self| Self {
            weak_self,
            key,
            remote_resource_accessor,
            creds,
            shared_memory: Default::default(),
            freeze_state: Default::default(),
            state: Default::default(),
            process_waiters: Default::default(),
            freeze_waiters: Default::default(),
            available_threads: Default::default(),
            thread_pool: Default::default(),
        });
        log_trace!("new BinderProcess id={}", result.identifier());
        result
    }

    /// A global identifier at the driver level for this binder process.
    pub fn identifier(&self) -> u64 {
        self.weak_self.as_ptr() as u64
    }

    pub fn lock<'a>(&'a self) -> BinderProcessGuard<'a> {
        BinderProcessGuard(Guard::new(self, self.state.lock()))
    }

    pub fn has_pending_transactions(&self, state: &BinderProcessState) -> bool {
        !state.closed
            && (state.releasing_threads_with_pending_transactions > 0
                || state.active_transactions.values().any(|t| t.is_pending)
                || self
                    .thread_pool
                    .lock()
                    .values()
                    .any(|t| t.state.lock().has_pending_transactions()))
    }

    /// Return the `BinderThread` with the given `task.tid`, creating it if it doesn't exist.
    pub fn find_or_register_thread(&self, task: &Task) -> Result<OwnedRef<BinderThread>, Errno> {
        let mut thread_pool = self.thread_pool.lock();
        if let Some(thread) = thread_pool.get(&task.tid) {
            return Ok(OwnedRef::share(thread));
        }
        let running_state = task.running_state()?;
        let handle = running_state
            .thread
            .get()
            .map_or_else(|| Arc::new(zx::Thread::invalid()), |t| t.thread.clone());
        let thread = BinderThread::new(self, task.tid.clone(), handle);
        thread_pool.insert(task.tid.clone(), OwnedRef::share(&thread));
        Ok(thread)
    }

    /// Unregister the `BinderThread` with the given `tid`.
    pub fn unregister_thread(&self, kernel: &Kernel, tid: &Pid) {
        let removed = {
            let mut state = self.lock();
            let removed = self.thread_pool.lock().remove(tid);
            if let Some(thread) = &removed {
                if thread.registration.load(Ordering::Acquire)
                    == RegistrationState::Auxiliary.to_u8()
                {
                    state.auxiliary_threads_count = state
                        .auxiliary_threads_count
                        .checked_sub(1)
                        .expect("Attempt to decrement auxiliary_threads_count below 0");
                }
                let mut thread_state = thread.state.lock();
                if thread_state.has_pending_transactions() {
                    state.releasing_threads_with_pending_transactions += 1;
                    thread_state.releasing_with_pending_transactions = true;
                }
                // The thread may still be queued in `available_threads` if it stopped waiting
                // without being popped. Remove its entry, which would otherwise stay queued as a
                // dead `WeakRef` once the thread is released (see `AvailableThreads::is_empty`).
                self.available_threads.remove(&thread.weak_self);
            }
            removed
        };
        if let Some(thread) = removed {
            thread.release(kernel);
        }
    }

    pub fn close(&self) {
        log_trace!("closing BinderProcess id={}", self.identifier());
        let mut state = self.lock();
        if !state.closed {
            state.closed = true;
            for thread in self.thread_pool.lock().values() {
                thread.command_event.notify();
                thread.command_queue_poll_waiters.notify_all();
            }
            self.process_waiters.notify_all();
            self.freeze_waiters.notify_all();
        }
    }

    /// Make all blocked threads stop waiting and return nothing. This is used by flush to
    /// make userspace recheck whether binder is being shut down.
    pub fn kick_all_threads(&self) {
        log_trace!("kicking threads for BinderProcess id={}", self.identifier());
        for thread in self.thread_pool.lock().values() {
            thread.lock().request_kick = true;
            thread.command_event.notify();
            thread.command_queue_poll_waiters.notify_all();
        }
    }

    /// Return the `ResourceAccessor` to use to access the resources of this process.
    pub fn get_resource_accessor<'a>(
        &'a self,
        task: &'a dyn ResourceAccessor,
    ) -> &'a dyn ResourceAccessor {
        get_resource_accessor(task, &self.remote_resource_accessor)
    }

    pub fn get_memory_accessor<'a>(
        &'a self,
        task: &'a dyn ResourceAccessor,
        remote_memory_accessor: Option<&'a RemoteMemoryAccessor<'_>>,
    ) -> &'a dyn MemoryAccessor {
        if let Some(memory_accessor) = self.get_resource_accessor(task).as_memory_accessor() {
            memory_accessor
        } else {
            remote_memory_accessor.expect("A RemoteMemoryAccessor should have been provided")
        }
    }

    /// Attempts to enqueue a command on an available thread.
    /// Pops threads from `available_threads` until one is found that is truly available,
    /// and enqueues the command on it.
    ///
    /// Returns `Ok(thread)` if it found a thread to schedule one.
    /// `Err(queued)` if no available threads are found.
    #[expect(clippy::result_large_err)]
    pub fn try_enqueue_on_available_thread(
        &self,
        command: QueuedCommand,
    ) -> Result<TempRef<'static, BinderThread>, QueuedCommand> {
        while let Some(weak_thread) = self.available_threads.pop() {
            if let Some(thread) = weak_thread.upgrade() {
                let mut thread_guard = thread.lock();
                if thread_guard.is_available() {
                    thread_guard.enqueue_command(command);
                    drop(thread_guard);
                    return Ok(TempRef::into_static(thread));
                }
            }
        }
        Err(command)
    }

    /// Enqueues `command` for the process and wakes up any thread that is waiting for commands.
    ///
    /// Returns Some(thread) if we successfully enqueue the command on a thread, or None if it was
    /// enqueued in the process's queue.
    pub fn enqueue_command(
        &self,
        command: QueuedCommand,
    ) -> Option<TempRef<'static, BinderThread>> {
        log_trace!(
            "BinderProcess id={} enqueuing command {:?}",
            self.identifier(),
            command.command
        );
        // Handle oneway transactions explicitly. They should always target the process queue to
        // avoid accidentally handling them during an ongoing transaction.
        if matches!(command.command, Command::OnewayTransaction(_)) {
            let should_wake = self.lock().enqueue_command(command);

            // Since OnewayTransactions are routed to the process queue, we must explicitly
            // wake waiters and available threads so it can grab the command, rather than letting it stall.
            if should_wake {
                self.wake_process_and_available_thread();
            }
            return None;
        }

        let mut queued = command;
        loop {
            match self.try_enqueue_on_available_thread(queued) {
                Ok(thread) => return Some(thread),
                Err(returned_queued) => {
                    {
                        let mut state = self.lock();
                        // We must not call try_enqueue_on_available_thread while holding
                        // the state lock, because that function acquires thread locks,
                        // which causes a lock inversion against thread_read.
                        if !self.available_threads.is_empty() {
                            queued = returned_queued;
                            continue;
                        }
                        crate::trace::on_command_enqueued(
                            &returned_queued.command,
                            returned_queued.trace_id,
                        );
                        state.command_queue.push_back(returned_queued);
                    }
                    self.wake_process_and_available_thread();
                    return None;
                }
            }
        }
    }

    /// Wakes up process waiters and an available thread if any exists in `available_threads`.
    /// Returns true if an available thread was woken up.
    pub fn wake_process_and_available_thread(&self) -> bool {
        self.process_waiters.notify_fd_events(starnix_uapi::vfs::FdEvents::POLLIN);
        while let Some(weak_thread) = self.available_threads.pop() {
            if let Some(thread) = weak_thread.upgrade() {
                let thread_guard = thread.lock();
                if thread_guard.is_available() {
                    thread.command_event.notify();
                    return true;
                }
            }
        }
        false
    }

    /// A binder thread is done reading a buffer allocated to a transaction. The binder
    /// driver can reclaim this buffer.
    pub fn handle_free_buffer(&self, buffer_ptr: UserAddress) -> Result<(), Errno> {
        log_trace!("BinderProcess id={} freeing buffer {:?}", self.identifier(), buffer_ptr);
        // Drop the state associated with the now completed transaction, releasing any temporary
        // strong handle references under the same process lock acquisition.
        let mut drop_actions = RefCountActions::default();
        let notify_freeze_waiters = release_after!(drop_actions, (), {
            let (active_transaction, should_wake, notify_freeze_waiters) = {
                let mut proc_state = self.lock();
                let mut active_transaction = proc_state.active_transactions.remove(&buffer_ptr);
                let mut should_wake = false;
                if let Some(active_transaction) = active_transaction.as_mut() {
                    active_transaction.state.release_handles(&mut proc_state, &mut drop_actions);
                    // Check if this was a oneway transaction and schedule the next oneway if this
                    // is the case.
                    if let RequestType::Oneway { object } = &active_transaction.request_type {
                        let mut object_state = object.lock();
                        assert!(
                            object_state.handling_oneway_transaction,
                            "freeing a oneway buffer implies that a oneway transaction was being handled"
                        );
                        if let Some((transaction, trace)) =
                            object_state.oneway_transactions.pop_front()
                        {
                            drop(object_state);

                            let queued =
                                QueuedCommand::new(Command::OnewayTransaction(transaction), trace);
                            should_wake = proc_state.enqueue_command(queued);
                        } else {
                            // No more oneway transactions queued, mark the queue handling as done.
                            object_state.handling_oneway_transaction = false;
                        }
                    }
                }
                let was_pending = active_transaction.as_ref().is_some_and(|t| t.is_pending);
                (
                    active_transaction,
                    should_wake,
                    was_pending && !self.has_pending_transactions(&proc_state),
                )
            };
            release_after!(active_transaction, &mut drop_actions, {
                if should_wake {
                    self.wake_process_and_available_thread();
                }
            });
            notify_freeze_waiters
        });

        // Reclaim the memory.
        let res = self
            .shared_memory
            .lock()
            .as_mut()
            .ok_or_else(|| errno!(ENOMEM))
            .and_then(|shared_memory| shared_memory.free_buffer(buffer_ptr));
        if notify_freeze_waiters {
            self.freeze_waiters.notify_all();
        }
        res
    }

    /// Handle a binder thread's request to increment/decrement a strong/weak reference to a remote
    /// binder object.
    pub fn handle_refcount_operation(
        &self,
        command: binder_driver_command_protocol,
        handle: Handle,
    ) -> Result<(), Errno> {
        let mut actions = RefCountActions::default();
        release_after!(actions, (), {
            self.lock().handle_refcount_operation(command, handle, &mut actions)
        })
    }

    /// Handle a binder thread's notification that it successfully incremented a strong/weak
    /// reference to a local (in-process) binder object. This is in response to a
    /// `BR_ACQUIRE`/`BR_INCREFS` command.
    pub fn handle_refcount_operation_done(
        &self,
        command: binder_driver_command_protocol,
        object: LocalBinderObject,
    ) -> Result<(), Errno> {
        let mut actions = RefCountActions::default();
        release_after!(actions, (), {
            self.lock().handle_refcount_operation_done(command, object, &mut actions)
        })
    }

    /// Subscribe a process to the death of the owner of `handle`.
    pub fn handle_request_death_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let proxy = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873363"),
                    "binder death notification for service manager"
                );
                return Ok(());
            }
            Handle::Object { index } => {
                let (proxy, guard) =
                    self.lock().handles.get(index).ok_or_else(|| errno!(ENOENT))?;

                // The object must have strong reference when used to find its owner process,
                // but there is no need to keep it alive afterwards.
                let mut actions = RefCountActions::default();
                guard.release(&mut actions);
                actions.release(());

                proxy
            }
        };
        // Upgrade the owner before locking `self`: holding `Some(owner)` (`TempRef`) prevents
        // `BinderProcess::release` from running until `owner` is dropped after subscribing, while
        // `None` records the pending `BR_DEAD_BINDER` directly without exposing `registered`.
        let owner = proxy.owner.upgrade();
        {
            let mut state = self.lock();
            let notification = state.death_notifications.entry(cookie).or_default();
            if notification.is_set() {
                // Like Linux, ignore a request for a death notification that is already set.
                log_trace!(
                    "BinderProcess id={} death notification already set for cookie {:#x}",
                    self.identifier(),
                    cookie
                );
                return Ok(());
            }
            if owner.is_some() {
                notification.registered = true;
            } else {
                notification.pending_dead_binders += 1;
            }
        }
        if let Some(owner) = owner {
            owner.lock().death_subscribers.push((self.weak_self.clone(), cookie));
        } else {
            // The object is already dead. Notify immediately. To be noted: the requesting thread
            // cannot handle the notification, in case it is holding some mutex while processing a
            // oneway transaction (where its transaction stack will be empty). It is currently not
            // a problem, because enqueue_command never schedule on the current thread.
            self.enqueue_command(Command::DeadBinder(cookie).into());
        }
        Ok(())
    }

    /// Remove a previously subscribed death notification.
    pub fn handle_clear_death_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let owner = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873735"),
                    "binder clear death notification for service manager"
                );
                self.enqueue_command(Command::ClearDeathNotificationDone(cookie).into());
                return Ok(());
            }
            Handle::Object { index } => {
                let mut state = self.lock();
                let owner = state.handles.get_owner(index).ok_or_else(|| errno!(ENOENT))?;
                match state.death_notifications.entry(cookie) {
                    Entry::Occupied(mut entry) if entry.get().registered => {
                        // No BR_DEAD_BINDER has been queued for the notification, and none will
                        // be: acknowledge the clear right away.
                        let notification = entry.get_mut();
                        notification.registered = false;
                        if notification.is_empty() {
                            entry.remove();
                        }
                    }
                    Entry::Occupied(mut entry)
                        if entry.get().pending_dead_binders > entry.get().deferred_clears =>
                    {
                        // A BR_DEAD_BINDER for this cookie is queued or being processed by
                        // userspace, e.g. this is the BC_CLEAR_DEATH_NOTIFICATION that libbinder's
                        // `BpBinder::sendObituary()` sends before calling the death recipients.
                        // Like Linux, defer BR_CLEAR_DEATH_NOTIFICATION_DONE until userspace
                        // acknowledges the BR_DEAD_BINDER with BC_DEAD_BINDER_DONE.
                        entry.get_mut().deferred_clears += 1;
                        return Ok(());
                    }
                    _ => {
                        // The death notification is not set, or each pending BR_DEAD_BINDER
                        // already has a deferred clear. Like Linux, ignore the clear.
                        log_trace!(
                            "BinderProcess id={} death notification not set for cookie {:#x}",
                            self.identifier(),
                            cookie
                        );
                        return Ok(());
                    }
                }
                owner
            }
        };
        if let Some(owner) = owner.upgrade() {
            let mut owner = owner.lock();
            if let Some((idx, _)) =
                owner.death_subscribers.iter().enumerate().find(|(_idx, (proc, c))| {
                    proc.as_ptr() == self.weak_self.as_ptr() && *c == cookie
                })
            {
                owner.death_subscribers.swap_remove(idx);
            }
        }
        self.enqueue_command(Command::ClearDeathNotificationDone(cookie).into());
        Ok(())
    }

    /// Acknowledge that userspace finished processing a death notification (`BR_DEAD_BINDER`).
    pub fn handle_dead_binder_done(&self, cookie: binder_uintptr_t) {
        let clear_done = {
            let mut state = self.lock();
            match state.death_notifications.entry(cookie) {
                Entry::Occupied(mut entry) if entry.get().pending_dead_binders > 0 => {
                    let notification = entry.get_mut();
                    notification.pending_dead_binders -= 1;
                    let clear_done = notification.deferred_clears > 0;
                    if clear_done {
                        notification.deferred_clears -= 1;
                    }
                    if notification.is_empty() {
                        entry.remove();
                    }
                    clear_done
                }
                _ => {
                    // Like Linux, ignore the acknowledgement of a death notification that was not
                    // sent.
                    log_trace!(
                        "BinderProcess id={} no pending death notification for cookie {:#x}",
                        self.identifier(),
                        cookie
                    );
                    false
                }
            }
        };
        if clear_done {
            self.enqueue_command(Command::ClearDeathNotificationDone(cookie).into());
        }
    }

    /// Queues a `BR_DEAD_BINDER` for the death notification `cookie`, as the owner of the binder
    /// object died, unless the notification has been cleared in the meantime.
    pub(crate) fn notify_dead_binder(&self, cookie: binder_uintptr_t) {
        {
            let mut state = self.lock();
            match state.death_notifications.get_mut(&cookie) {
                Some(notification) if notification.registered => {
                    notification.registered = false;
                    notification.pending_dead_binders += 1;
                }
                // The notification has been cleared, and its BR_CLEAR_DEATH_NOTIFICATION_DONE may
                // already have been delivered: userspace must not receive a BR_DEAD_BINDER after
                // it.
                _ => return,
            }
        }
        self.enqueue_command(Command::DeadBinder(cookie).into());
    }

    /// Subscribe a process to the freeze of the owner of `handle`.
    pub fn handle_request_freeze_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let proxy = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/402188420"),
                    "binder freeze notification for service manager"
                );
                let info = binder_frozen_state_info { cookie, ..Default::default() };
                self.enqueue_command(Command::FrozenBinder(info).into());
                return Ok(());
            }
            Handle::Object { index } => {
                let (proxy, guard) =
                    self.lock().handles.get(index).ok_or_else(|| errno!(ENOENT))?;

                // The object must have strong reference when used to find its owner process,
                // but there is no need to keep it alive afterwards.
                let mut actions = RefCountActions::default();
                guard.release(&mut actions);
                actions.release(());

                proxy
            }
        };
        let owner = proxy.owner.upgrade().ok_or_else(|| errno!(ENOENT))?;
        let mut owner_freeze_state = owner.freeze_state.lock();
        // Check if the subscriber already exists
        if owner_freeze_state
            .freeze_subscribers
            .iter()
            .find(|(bp, c)| bp.as_ptr() == self.weak_self.as_ptr() && *c == cookie)
            .is_some()
        {
            return error!(EINVAL);
        }
        owner_freeze_state.freeze_subscribers.push((self.weak_self.clone(), cookie));
        let info = binder_frozen_state_info {
            cookie,
            is_frozen: if owner_freeze_state.committed_frozen { 1 } else { 0 },
            reserved: 0,
        };
        self.enqueue_command(Command::FrozenBinder(info).into());
        Ok(())
    }

    /// Remove a previously subscribed freeze notification.
    pub fn handle_clear_freeze_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let owner = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/402191387"),
                    "binder clear freeze notification for service manager"
                );
                None
            }
            Handle::Object { index } => {
                Some(self.lock().handles.get_owner(index).ok_or_else(|| errno!(ENOENT))?)
            }
        };
        if let Some(owner) = owner.as_ref().and_then(|o| o.upgrade()) {
            let mut owner_freeze_state = owner.freeze_state.lock();
            if let Some((idx, _)) =
                owner_freeze_state.freeze_subscribers.iter().enumerate().find(
                    |(_idx, (proc, c))| proc.as_ptr() == self.weak_self.as_ptr() && *c == cookie,
                )
            {
                owner_freeze_state.freeze_subscribers.swap_remove(idx);
            }
        }

        let mut state = self.lock();
        // Purge any pending FrozenBinder commands matching this cookie from the command queues.
        state.command_queue.retain(
            |queued| !matches!(queued.command, Command::FrozenBinder(info) if info.cookie == cookie),
        );
        for thread in self.thread_pool.lock().values() {
            thread.lock().command_queue.retain(
                |queued| !matches!(queued.command, Command::FrozenBinder(info) if info.cookie == cookie),
            );
        }

        // If the notification is currently in flight to userspace, hold off sending
        // ClearFreezeNotificationDone until BC_FREEZE_NOTIFICATION_DONE arrives.
        if state.in_flight_freeze_notifications.contains(&cookie) {
            state.pending_clear_freeze_notifications.insert(cookie);
            return Ok(());
        }
        drop(state);

        self.enqueue_command(Command::ClearFreezeNotificationDone(cookie).into());
        Ok(())
    }

    /// Acknowledge that userspace finished processing a freeze notification.
    pub fn handle_freeze_notification_done(&self, cookie: binder_uintptr_t) {
        let mut state = self.lock();
        state.in_flight_freeze_notifications.remove(&cookie);
        if state.pending_clear_freeze_notifications.remove(&cookie) {
            drop(state);
            self.enqueue_command(Command::ClearFreezeNotificationDone(cookie).into());
        }
    }

    /// Map the external vmo into the driver address space, recording the userspace address.
    pub fn map_external_vmo(&self, vmo: fidl::Vmo, mapped_address: u64) -> Result<(), Errno> {
        let mut shared_memory = self.shared_memory.lock();
        // Do not support mapping shared memory more than once.
        if shared_memory.is_some() {
            return error!(EINVAL);
        }
        let memory = MemoryObject::from(vmo);
        let size = memory.get_size();
        *shared_memory = Some(SharedMemory::map(&memory, mapped_address.into(), size as usize)?);
        Ok(())
    }

    /// Returns a task in the process
    pub fn get_task(&self) -> Option<Arc<Task>> {
        get_task_for_thread_group(&self.key)
    }
}

impl<'a> BinderProcessGuard<'a> {
    /// Inserts a reference to a binder object, returning a handle that represents it.
    /// The handle may be an existing handle if the object was already present in the table.
    /// The object must have at least a strong reference guarded by `guard` to ensure it is kept
    /// alive until it is added to the transaction.
    /// Returns the handle representing the object.
    pub fn insert_for_transaction(
        &mut self,
        guard: StrongRefGuard,
        actions: &mut RefCountActions,
    ) -> Handle {
        self.handles.insert_for_transaction(guard, actions)
    }

    /// Handle a binder thread's request to increment/decrement a strong/weak reference to a remote
    /// binder object.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn handle_refcount_operation(
        &mut self,
        command: binder_driver_command_protocol,
        handle: Handle,
        actions: &mut RefCountActions,
    ) -> Result<(), Errno> {
        let idx = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873629"),
                    "binder acquire/release refs for context manager object"
                );
                return Ok(());
            }
            Handle::Object { index } => index,
        };

        match command {
            binder_driver_command_protocol_BC_ACQUIRE => {
                log_trace!("Strong increment on handle {}", idx);
                self.handles.inc_strong(idx, actions)
            }
            binder_driver_command_protocol_BC_RELEASE => {
                log_trace!("Strong decrement on handle {}", idx);
                self.handles.dec_strong(idx, actions)
            }
            binder_driver_command_protocol_BC_INCREFS => {
                log_trace!("Weak increment on handle {}", idx);
                self.handles.inc_weak(idx, actions)
            }
            binder_driver_command_protocol_BC_DECREFS => {
                log_trace!("Weak decrement on handle {}", idx);
                self.handles.dec_weak(idx, actions)
            }
            _ => unreachable!(),
        }
    }

    /// Handle a binder thread's notification that it successfully incremented a strong/weak
    /// reference to a local (in-process) binder object. This is in response to a
    /// `BR_ACQUIRE`/`BR_INCREFS` command.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn handle_refcount_operation_done(
        &self,
        command: binder_driver_command_protocol,
        local: LocalBinderObject,
        actions: &mut RefCountActions,
    ) -> Result<(), Errno> {
        let object = self.find_object(&local).ok_or_else(|| errno!(EINVAL))?;
        match command {
            binder_driver_command_protocol_BC_INCREFS_DONE => {
                log_trace!("Acknowledging increment reference for {:?}", object);
                object.ack_incref(actions)
            }
            binder_driver_command_protocol_BC_ACQUIRE_DONE => {
                log_trace!("Acknowleding acquire done for {:?}", object);
                object.ack_acquire(actions)
            }
            _ => unreachable!(),
        }
    }

    /// Finds the binder object that corresponds to the process-local addresses `local`.
    pub fn find_object(&self, local: &LocalBinderObject) -> Option<&Arc<BinderObject>> {
        self.objects.get(&local.weak_ref_addr)
    }

    /// Finds the binder object that corresponds to the process-local addresses `local`, or creates
    /// a new [`BinderObject`] to represent the one in the process.
    pub fn find_or_register_object(
        &mut self,
        binder_thread: &BinderThread,
        local: LocalBinderObject,
        flags: BinderObjectFlags,
    ) -> Result<StrongRefGuard, Errno> {
        if let Some(object) = self.find_object(&local) {
            if object.local.strong_ref_addr != local.strong_ref_addr {
                return error!(EINVAL);
            }
            // The ref count can grow back from 0 in this instance because the object is being
            // registered again by its owner.
            Ok(object.inc_strong_unchecked(binder_thread))
        } else {
            let (object, guard) = BinderObject::new(self.base, local, flags);

            // Tell the owning process that a remote process now has a strong reference to
            // this object.
            binder_thread.lock().enqueue_command(Command::AcquireRef(object.local).into());

            self.objects.insert(object.local.weak_ref_addr, object);

            Ok(guard)
        }
    }

    /// Whether the driver should request that the client starts a new thread.
    #[cfg(test)]
    pub fn should_request_thread(&self, thread: &BinderThread) -> bool {
        self.should_request_thread_locked(&thread.lock())
    }

    /// Whether the driver should request that the client starts a new thread,
    /// using an already-acquired `BinderThreadState` lock guard.
    #[inline]
    pub fn should_request_thread_locked(&self, thread_state: &BinderThreadState) -> bool {
        !self.thread_requested
            && self.auxiliary_threads_count < self.max_thread_count
            && thread_state.is_main_or_registered()
            && self.base.available_threads.is_empty()
    }

    /// Called back when the driver successfully asked the client to start a new thread.
    pub fn did_request_thread(&mut self) {
        self.thread_requested = true;
    }
}

impl Releasable for BinderProcess {
    type Context<'a> = &'a Kernel;

    fn release<'a>(self, context: Self::Context<'a>) {
        let identifier = self.identifier();
        log_trace!("Releasing BinderProcess id={}", identifier);
        let state = self.state.into_inner();
        // Notify any subscribers that the objects this process owned are now dead.
        for (proc, cookie) in state.death_subscribers {
            if let Some(target_proc) = proc.upgrade() {
                target_proc.notify_dead_binder(cookie);
            }
        }

        // Generate dead replies for transactions currently in the command queue of this process.
        // Transactions that have been scheduled with a specific thread will generate dead replies
        // when the threads are released below.
        generate_dead_replies(state.command_queue, identifier, None);

        let mut drop_actions = RefCountActions::default();
        for transaction in state.active_transactions.into_values() {
            transaction.release(&mut drop_actions);
        }
        drop_actions.release(());

        state.handles.release(());

        for thread in self.thread_pool.into_inner().into_values() {
            thread.release(context);
        }

        self.freeze_waiters.notify_all();
    }
}

/// Table containing handles to remote binder objects.
///
/// Maintains a bidirectional mapping between handle indices and remote [`BinderObject`]s.
#[derive(Debug, Default)]
pub struct HandleTable {
    /// Slab containing the references indexed by handle index.
    table: slab::Slab<BinderObjectRef>,
    /// Reverse index for O(log N) lookup from a BinderObject to its handle index in `table`.
    objects: BTreeMap<ArcKey<BinderObject>, usize>,
}

/// The HandleTable is released at the time the BinderProcess is released. At this moment, any
/// reference to object owned by another BinderProcess need to be clean.
impl Releasable for HandleTable {
    type Context<'a> = ();
    fn release<'a>(self, _: ()) {
        for (_, r) in self.table.into_iter() {
            let mut actions = RefCountActions::default();
            r.clean_refs(&mut actions);
            actions.release(());
        }
    }
}

impl HandleTable {
    /// Inserts a reference to a binder object, returning a handle that represents it.
    /// The handle may be an existing handle if the object was already present in the table.
    /// Transfer the reference represented by `guard` to the handle.
    /// A new handle will have a single strong reference, an existing handle will have its strong
    /// reference count incremented by one.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess` and the handle representing the object.
    pub fn insert_for_transaction(
        &mut self,
        guard: StrongRefGuard,
        actions: &mut RefCountActions,
    ) -> Handle {
        let key = ArcKey(guard.binder_object.clone());
        let index = if let Some(&existing_idx) = self.objects.get(&key) {
            let object_ref =
                self.table.get_mut(existing_idx).expect("Handle table index invariant violated");
            // Increment the number of reference to the handle as expected by the caller.
            object_ref.inc_strong_with_guard(guard, actions);
            existing_idx
        } else {
            // The new handle will be created with a strong reference as expected by the
            // caller.
            let idx = self.table.insert(BinderObjectRef::new(guard));
            self.objects.insert(key, idx);
            idx
        };
        Handle::Object { index }
    }

    /// Retrieves a reference to a binder object at index `idx`. Returns None if the index doesn't
    /// exist or the object has no strong reference, otherwise returns the object and a guard to
    /// ensure it is kept alive while used.
    pub fn get(&self, idx: usize) -> Option<(Arc<BinderObject>, StrongRefGuard)> {
        let object_ref = self.table.get(idx)?;
        match object_ref.binder_object.inc_strong_checked() {
            Ok(guard) => Some((object_ref.binder_object.clone(), guard)),
            _ => None,
        }
    }

    /// Retrieves the owner of the binder object at index `idx`, whether the object has strong
    /// references or not.
    pub fn get_owner(&self, idx: usize) -> Option<WeakRef<BinderProcess>> {
        self.table.get(idx).map(|r| r.binder_object.owner.clone())
    }

    /// Increments the strong reference count of the binder object reference at index `idx`,
    /// failing if the object no longer exists.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn inc_strong(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        Ok(self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?.inc_strong(actions)?)
    }

    /// Increments the weak reference count of the binder object reference at index `idx`, failing
    /// if the object does not exist.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn inc_weak(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        Ok(self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?.inc_weak(actions))
    }

    /// Decrements the strong reference count of the binder object reference at index `idx`, failing
    /// if the object no longer exists.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn dec_strong(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        let object_ref = self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?;
        object_ref.dec_strong(actions)?;
        if !object_ref.has_ref() {
            let key = ArcKey(object_ref.binder_object.clone());
            self.objects.remove(&key);
            self.table.remove(idx);
        }
        Ok(())
    }

    /// Decrements the weak reference count of the binder object reference at index `idx`, failing
    /// if the object does not exist.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn dec_weak(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        let object_ref = self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?;
        object_ref.dec_weak(actions)?;
        if !object_ref.has_ref() {
            let key = ArcKey(object_ref.binder_object.clone());
            self.objects.remove(&key);
            self.table.remove(idx);
        }
        Ok(())
    }
}

/// Returns a task in the process keyed by `key`.
fn get_task_for_thread_group(key: &Pid) -> Option<Arc<Task>> {
    key.get_thread_group().ok().and_then(|tg| tg.read().get_running_task_with_files().ok())
}
