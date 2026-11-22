// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{
    ClientEnd, ControlHandle as _, Responder as _, create_proxy_and_stream, create_request_stream,
};
use fidl_fuchsia_power_broker as fpb;
use fuchsia_async as fasync;
use fuchsia_async::condition::Condition;
use futures::{FutureExt, StreamExt, select_biased};
use std::collections::VecDeque;
use std::fmt;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

struct LeaseControlState {
    current_status: fpb::LeaseStatus,
    queued_statuses: VecDeque<fpb::LeaseStatus>,
    active: bool,
    channel_closed: bool,
    watcher_parked: bool,
}

impl LeaseControlState {
    fn mark_closed(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        self.watcher_parked = false;
        self.current_status = fpb::LeaseStatus::Vacated;
        if !self.queued_statuses.is_empty()
            && self.queued_statuses.back() != Some(&fpb::LeaseStatus::Vacated)
        {
            self.queued_statuses.push_back(fpb::LeaseStatus::Vacated);
        }
        true
    }
}

struct LeaseControlInner {
    scope: fasync::Scope,
    level: Option<fpb::PowerLevel>,
    state: Arc<Condition<LeaseControlState>>,
}

struct LeaseStreamGuard {
    stream: Option<fpb::LeaseControlRequestStream>,
    state: Arc<Condition<LeaseControlState>>,
}

impl Drop for LeaseStreamGuard {
    fn drop(&mut self) {
        // Drop the request stream before waking `wait_until_closed` waiters so the client channel
        // observes `PEER_CLOSED` even when the server task is aborted at an `.await` point.
        drop(self.stream.take());
        let mut state = self.state.lock();
        state.mark_closed();
        state.queued_statuses.clear();
        state.channel_closed = true;
        for waker in state.drain_wakers() {
            waker.wake();
        }
    }
}

/// Fake server for `fuchsia.power.broker/LeaseControl` with spec-compliant `WatchStatus`
/// hanging-get semantics.
///
/// For standalone instances created via [`Self::spawn`], dropping all clones of `FakeLeaseControl`
/// aborts its server task and closes the `LeaseControl` channel. Leases minted by [`FakeLessor`]
/// also retain a clone inside [`FakeLessor::leases`] until the `FakeLessor` is dropped; call
/// [`Self::close`] to close a lease from the server side while its `FakeLessor` remains alive.
#[derive(Clone)]
pub struct FakeLeaseControl {
    inner: Arc<LeaseControlInner>,
}

impl fmt::Debug for FakeLeaseControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.state.lock();
        f.debug_struct("FakeLeaseControl")
            .field("level", &self.inner.level)
            .field("status", &state.current_status)
            .field("active", &state.active)
            .finish()
    }
}

impl FakeLeaseControl {
    /// Spawns a standalone `FakeLeaseControl` returning `(Self, ClientEnd<LeaseControlMarker>)`.
    ///
    /// The server task runs on a child of [`fuchsia_async::Scope::current`]; dropping all clones of
    /// the returned `FakeLeaseControl` or cancelling that enclosing scope aborts the server task
    /// and closes the `LeaseControl` channel.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    #[must_use = "dropping the returned FakeLeaseControl aborts its server task and closes the channel"]
    pub fn spawn(initial_status: fpb::LeaseStatus) -> (Self, ClientEnd<fpb::LeaseControlMarker>) {
        Self::spawn_with_options(fasync::Scope::new(), initial_status, None)
    }

    fn spawn_with_options(
        scope: fasync::Scope,
        initial_status: fpb::LeaseStatus,
        level: Option<fpb::PowerLevel>,
    ) -> (Self, ClientEnd<fpb::LeaseControlMarker>) {
        let (client_end, stream) = create_request_stream::<fpb::LeaseControlMarker>();
        let state = Arc::new(Condition::new(LeaseControlState {
            current_status: initial_status,
            queued_statuses: VecDeque::new(),
            active: true,
            channel_closed: false,
            watcher_parked: false,
        }));

        let control =
            Self { inner: Arc::new(LeaseControlInner { scope, level, state: state.clone() }) };
        let mut guard = LeaseStreamGuard { stream: Some(stream), state };
        control.inner.scope.spawn(async move {
            loop {
                let next_request = {
                    let mut closed = pin!(
                        guard
                            .state
                            .when(|state| {
                                if !state.active && state.queued_statuses.is_empty() {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            })
                            .fuse()
                    );
                    let stream =
                        guard.stream.as_mut().expect("stream stays Some until guard drops");
                    select_biased! {
                        request = stream.next().fuse() => request,
                        _ = closed => None,
                    }
                };
                let Some(Ok(request)) = next_request else {
                    break;
                };
                match request {
                    fpb::LeaseControlRequest::WatchStatus { last_status, responder } => {
                        let mut watch = pin!(
                            guard
                                .state
                                .when(|state| {
                                    state.watcher_parked = false;
                                    if last_status == fpb::LeaseStatus::Unknown {
                                        state.queued_statuses.clear();
                                        return Poll::Ready(Some((
                                            state.current_status,
                                            !state.active,
                                        )));
                                    }
                                    while let Some(queued) = state.queued_statuses.pop_front() {
                                        if queued != last_status {
                                            let close_after =
                                                !state.active && state.queued_statuses.is_empty();
                                            return Poll::Ready(Some((queued, close_after)));
                                        }
                                    }
                                    if last_status != state.current_status {
                                        Poll::Ready(Some((state.current_status, !state.active)))
                                    } else if !state.active {
                                        Poll::Ready(None)
                                    } else {
                                        state.watcher_parked = true;
                                        Poll::Pending
                                    }
                                })
                                .fuse()
                        );
                        let outcome = select_biased! {
                            result = watch => result,
                            _ = responder.control_handle().on_closed().fuse() => None,
                        };
                        match outcome {
                            Some((status, close_after)) => {
                                let _ = responder.send(status);
                                if close_after {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    fpb::LeaseControlRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("FakeLeaseControl ignoring unknown method ordinal: {ordinal}");
                    }
                }
            }
        });

        (control, client_end)
    }

    /// Returns the power level requested when this lease was created via `FakeLessor`, if any.
    pub fn level(&self) -> Option<fpb::PowerLevel> {
        self.inner.level
    }

    /// Returns the current `LeaseStatus`.
    pub fn status(&self) -> fpb::LeaseStatus {
        self.inner.state.lock().current_status
    }

    /// Updates the current `LeaseStatus` and wakes any parked `WatchStatus` call if changed.
    ///
    /// No-op once the lease is inactive (for example, after [`Self::close`] or client channel
    /// closure).
    pub fn set_status(&self, status: fpb::LeaseStatus) {
        let mut state = self.inner.state.lock();
        if !state.active || state.current_status == status {
            return;
        }
        state.current_status = status;
        if std::mem::take(&mut state.watcher_parked) {
            state.queued_statuses.push_back(status);
        }
        for waker in state.drain_wakers() {
            waker.wake();
        }
    }

    /// Returns `true` while the lease has not been closed or dropped.
    ///
    /// Calling [`Self::close`] marks the lease inactive synchronously before the background server
    /// task drops the channel endpoint; await [`Self::wait_until_closed`] to observe actual channel
    /// closure (`PEER_CLOSED`).
    pub fn is_active(&self) -> bool {
        self.inner.state.lock().active
    }

    /// Transitions the lease to `LeaseStatus::Vacated` (waking any parked `WatchStatus` call whose
    /// `last_status` was not already `Vacated`) and closes the server end of the `LeaseControl`
    /// channel.
    ///
    /// Callers simulating an intermediate power-down phase during element removal can call
    /// `set_status(fpb::LeaseStatus::PoweringDown)` immediately before calling `close()`; when a
    /// `WatchStatus` call is parked, the intermediate transition is delivered before
    /// `LeaseStatus::Vacated` and channel closure.
    pub fn close(&self) {
        let mut state = self.inner.state.lock();
        if !state.mark_closed() {
            return;
        }
        for waker in state.drain_wakers() {
            waker.wake();
        }
    }

    /// Waits until the `LeaseControl` channel has been closed.
    pub async fn wait_until_closed(&self) {
        self.inner
            .state
            .when(|state| if state.channel_closed { Poll::Ready(()) } else { Poll::Pending })
            .await;
    }
}

struct LessorState {
    default_status: fpb::LeaseStatus,
    lease_error: Option<fpb::LeaseError>,
    valid_levels: Option<Vec<fpb::PowerLevel>>,
    leases: Vec<FakeLeaseControl>,
    next_lease_index: usize,
    next_waiter_id: u64,
    waiter_queue: VecDeque<u64>,
}

struct LessorInner {
    scope: fasync::Scope,
    lease_parent_scope: fasync::ScopeHandle,
    state: Condition<LessorState>,
}

struct NextLeaseWaiterGuard<'a> {
    inner: &'a LessorInner,
    waiter_id: Option<u64>,
}

impl Drop for NextLeaseWaiterGuard<'_> {
    fn drop(&mut self) {
        let Some(id) = self.waiter_id.take() else {
            return;
        };
        let mut state = self.inner.state.lock();
        let was_front = state.waiter_queue.front() == Some(&id);
        state.waiter_queue.retain(|&queued_id| queued_id != id);
        if was_front
            && !state.waiter_queue.is_empty()
            && state.next_lease_index < state.leases.len()
        {
            for waker in state.drain_wakers() {
                waker.wake();
            }
        }
    }
}

/// Fake server for `fuchsia.power.broker/Lessor`.
///
/// Dropping all clones of a `FakeLessor` aborts its `Lessor` server tasks and closes its `Lessor`
/// channels. Already-minted leases remain open after `FakeLessor` is dropped only while at least
/// one [`FakeLeaseControl`] clone (for example, from [`Self::next_lease`] or [`Self::leases`]) is
/// retained by the caller.
#[must_use = "dropping FakeLessor aborts its server tasks and closes Lessor channels"]
#[derive(Clone)]
pub struct FakeLessor {
    inner: Arc<LessorInner>,
}

impl fmt::Debug for FakeLessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.state.lock();
        let leased_levels: Vec<_> =
            state.leases.iter().filter_map(FakeLeaseControl::level).collect();
        f.debug_struct("FakeLessor")
            .field("default_status", &state.default_status)
            .field("lease_error", &state.lease_error)
            .field("valid_levels", &state.valid_levels)
            .field("leased_levels", &leased_levels)
            .field("lease_count", &state.leases.len())
            .finish()
    }
}

impl FakeLessor {
    /// Creates a new `FakeLessor` whose minted leases start in `initial_status`.
    ///
    /// Both this `FakeLessor`'s server tasks and any `FakeLeaseControl` tasks it mints are spawned
    /// under [`fuchsia_async::Scope::current`] captured at construction time; cancelling that
    /// scope tears down the lessor and all of its minted leases.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn new(initial_status: fpb::LeaseStatus) -> Self {
        Self::new_internal(initial_status, None)
    }

    /// Creates a `FakeLessor` that only grants leases for levels in `valid_levels`, returning
    /// `Err(fpb::LeaseError::InvalidLevel)` for any other requested `PowerLevel`.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn with_valid_levels(
        initial_status: fpb::LeaseStatus,
        valid_levels: impl Into<Vec<fpb::PowerLevel>>,
    ) -> Self {
        Self::new_internal(initial_status, Some(valid_levels.into()))
    }

    fn new_internal(
        initial_status: fpb::LeaseStatus,
        valid_levels: Option<Vec<fpb::PowerLevel>>,
    ) -> Self {
        let lease_parent_scope = fasync::Scope::current();
        Self {
            inner: Arc::new(LessorInner {
                scope: lease_parent_scope.new_child(),
                lease_parent_scope,
                state: Condition::new(LessorState {
                    default_status: initial_status,
                    lease_error: None,
                    valid_levels,
                    leases: Vec::new(),
                    next_lease_index: 0,
                    next_waiter_id: 0,
                    waiter_queue: VecDeque::new(),
                }),
            }),
        }
    }

    /// Spawns a new `FakeLessor` and returns `(Self, LessorProxy)`.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn spawn(initial_status: fpb::LeaseStatus) -> (Self, fpb::LessorProxy) {
        let lessor = Self::new(initial_status);
        let proxy = lessor.create_proxy();
        (lessor, proxy)
    }

    /// Creates a new `LessorProxy` connected to this `FakeLessor`.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn create_proxy(&self) -> fpb::LessorProxy {
        let (proxy, stream) = create_proxy_and_stream::<fpb::LessorMarker>();
        self.serve_stream(stream);
        proxy
    }

    /// Serves a `LessorRequestStream` on this `FakeLessor`'s internal `Scope`.
    pub fn serve_stream(&self, mut stream: fpb::LessorRequestStream) {
        let weak = Arc::downgrade(&self.inner);
        self.inner.scope.spawn(async move {
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fpb::LessorRequest::Lease { level, responder } => {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        let mut state = inner.state.lock();
                        if let Some(err) = state.lease_error {
                            let _ = responder.send(Err(err));
                        } else if let Some(valid_levels) = &state.valid_levels
                            && !valid_levels.contains(&level)
                        {
                            let _ = responder.send(Err(fpb::LeaseError::InvalidLevel));
                        } else {
                            let (lease_control, client_end) = FakeLeaseControl::spawn_with_options(
                                inner.lease_parent_scope.new_child(),
                                state.default_status,
                                Some(level),
                            );
                            state.leases.push(lease_control);
                            let _ = responder.send(Ok(client_end));
                            for waker in state.drain_wakers() {
                                waker.wake();
                            }
                        }
                    }
                    fpb::LessorRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("FakeLessor ignoring unknown method ordinal: {ordinal}");
                    }
                }
            }
        });
    }

    /// Configures `Lessor::Lease` to return `Err(err)` when `Some`, or succeed when `None`.
    pub fn set_lease_error(&self, error: Option<fpb::LeaseError>) {
        self.inner.state.lock().lease_error = error;
    }

    /// Restricts `Lessor::Lease` to levels in `valid_levels` (returning
    /// `Err(fpb::LeaseError::InvalidLevel)` for others), or allows any `PowerLevel` when `None`.
    pub fn set_valid_levels(&self, valid_levels: Option<Vec<fpb::PowerLevel>>) {
        self.inner.state.lock().valid_levels = valid_levels;
    }

    /// Updates the initial `LeaseStatus` assigned to subsequently created `FakeLeaseControl`s.
    pub fn set_default_status(&self, status: fpb::LeaseStatus) {
        self.inner.state.lock().default_status = status;
    }

    /// Returns the ordered list of power levels requested via successful `Lessor::Lease` calls.
    pub fn leased_levels(&self) -> Vec<fpb::PowerLevel> {
        self.inner.state.lock().leases.iter().filter_map(FakeLeaseControl::level).collect()
    }

    /// Returns the number of leases minted by this `FakeLessor` that are still active.
    pub fn active_lease_count(&self) -> usize {
        self.inner.state.lock().leases.iter().filter(|l| l.is_active()).count()
    }

    /// Returns handles to all `FakeLeaseControl` instances created by this `FakeLessor`.
    pub fn leases(&self) -> Vec<FakeLeaseControl> {
        self.inner.state.lock().leases.clone()
    }

    /// Awaits the next granted lease from `Lessor::Lease` in FIFO order.
    ///
    /// Requests rejected via [`Self::set_lease_error`] or [`Self::set_valid_levels`] do not
    /// resolve `next_lease`.
    pub async fn next_lease(&self) -> FakeLeaseControl {
        let mut guard = NextLeaseWaiterGuard { inner: &self.inner, waiter_id: None };
        self.inner
            .state
            .when(|state| {
                let id = *guard.waiter_id.get_or_insert_with(|| {
                    let id = state.next_waiter_id;
                    state.next_waiter_id += 1;
                    state.waiter_queue.push_back(id);
                    id
                });
                if state.waiter_queue.front() != Some(&id) {
                    return Poll::Pending;
                }
                let Some(lease) = state.leases.get(state.next_lease_index).cloned() else {
                    return Poll::Pending;
                };
                state.waiter_queue.pop_front();
                guard.waiter_id = None;
                state.next_lease_index += 1;
                if !state.waiter_queue.is_empty() && state.next_lease_index < state.leases.len() {
                    for waker in state.drain_wakers() {
                        waker.wake();
                    }
                }
                Poll::Ready(lease)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    // Verify FakeLeaseControl hanging-get parking, status transitions, and sequential request
    // handling on a single channel.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_hanging_get_and_sequential_requests() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Pending);
        assert_eq!(lease_ctrl.level(), None);
        let proxy = client_end.into_proxy();

        // Verify watch_status(Unknown) returns the current status immediately.
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Pending
        );

        // Verify watch_status with a non-Unknown mismatched status returns immediately.
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Satisfied).await.unwrap(),
            fpb::LeaseStatus::Pending
        );

        // Verify watch_status parks on the server when last_status matches the current status.
        let mut watch_fut = proxy.watch_status(fpb::LeaseStatus::Pending);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut watch_fut).await,
            Poll::Pending
        );

        // Verify set_status with an unchanged status is a no-op and keeps watch_status parked.
        lease_ctrl.set_status(fpb::LeaseStatus::Pending);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut watch_fut).await,
            Poll::Pending
        );

        // Verify set_status updates the status and wakes the parked watch_status call.
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);
        assert_eq!(watch_fut.await.unwrap(), fpb::LeaseStatus::Satisfied);

        // Verify a parked watch_status blocks subsequent requests on the same channel until status
        // changes, matching Power Broker's sequential LeaseControl stream handling.
        let mut first_watch = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut first_watch).await,
            Poll::Pending
        );
        let mut queued_watch = proxy.watch_status(fpb::LeaseStatus::Unknown);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut queued_watch).await,
            Poll::Pending
        );
        assert!(lease_ctrl.is_active());
        lease_ctrl.set_status(fpb::LeaseStatus::Pending);
        assert_eq!(first_watch.await.unwrap(), fpb::LeaseStatus::Pending);
        assert_eq!(queued_watch.await.unwrap(), fpb::LeaseStatus::Pending);
    }

    // Verify FakeLeaseControl::close transitions through LeaseStatus::Vacated, wakes a parked
    // WatchStatus call, and closes the channel.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_close_wakes_parked_watcher_with_vacated() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let proxy = client_end.into_proxy();
        let mut close_watch = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut close_watch).await,
            Poll::Pending
        );

        lease_ctrl.close();
        assert!(!lease_ctrl.is_active());
        assert_eq!(lease_ctrl.status(), fpb::LeaseStatus::Vacated);
        assert_eq!(close_watch.await.unwrap(), fpb::LeaseStatus::Vacated);
        lease_ctrl.wait_until_closed().await;
        assert_matches!(
            proxy.watch_status(fpb::LeaseStatus::Vacated).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify a WatchStatus request queued before the server task is polled still receives Vacated
    // when FakeLeaseControl::close is called in the same turn.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_close_replies_vacated_to_queued_unpolled_watch() {
        let (unpolled_ctrl, unpolled_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let unpolled_proxy = unpolled_end.into_proxy();
        let queued_watch = unpolled_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        unpolled_ctrl.close();
        assert_eq!(queued_watch.await.unwrap(), fpb::LeaseStatus::Vacated);
        unpolled_ctrl.wait_until_closed().await;
        assert_matches!(
            unpolled_proxy.watch_status(fpb::LeaseStatus::Vacated).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify FakeLeaseControl::close on an idle channel exits immediately when inactive and ignores
    // subsequent set_status calls.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_close_exits_idle_channel_and_ignores_set_status() {
        let (idle_ctrl, idle_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let idle_proxy = idle_end.into_proxy();
        idle_ctrl.close();
        idle_ctrl.set_status(fpb::LeaseStatus::Satisfied);
        assert_eq!(idle_ctrl.status(), fpb::LeaseStatus::Vacated);
        idle_ctrl.wait_until_closed().await;
        assert_matches!(
            idle_proxy.watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify FakeLeaseControl::close while WatchStatus is already parked on Vacated closes the
    // channel without sending a duplicate Vacated response.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_close_closes_channel_when_already_vacated() {
        let (vacated_ctrl, vacated_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Vacated);
        let vacated_proxy = vacated_end.into_proxy();
        let mut vacated_watch = vacated_proxy.watch_status(fpb::LeaseStatus::Vacated);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut vacated_watch).await,
            Poll::Pending
        );
        vacated_ctrl.close();
        assert_matches!(vacated_watch.await, Err(fidl::Error::ClientChannelClosed { .. }));
        vacated_ctrl.wait_until_closed().await;
    }

    // Verify calling set_status(PoweringDown) immediately followed by close() delivers
    // PoweringDown first and then Vacated before closing the LeaseControl channel.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_queues_intermediate_powering_down_before_close() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let proxy = client_end.into_proxy();
        let mut parked_watch = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );

        lease_ctrl.set_status(fpb::LeaseStatus::PoweringDown);
        lease_ctrl.close();
        assert!(!lease_ctrl.is_active());
        assert_eq!(lease_ctrl.status(), fpb::LeaseStatus::Vacated);

        assert_eq!(parked_watch.await.unwrap(), fpb::LeaseStatus::PoweringDown);
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::PoweringDown).await.unwrap(),
            fpb::LeaseStatus::Vacated
        );
        lease_ctrl.wait_until_closed().await;
        assert_matches!(
            proxy.watch_status(fpb::LeaseStatus::Vacated).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify unwatched set_status transitions coalesce into current_status rather than replaying
    // stale intermediate statuses on subsequent WatchStatus calls.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_unwatched_transitions_coalesce() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Pending);
        let proxy = client_end.into_proxy();

        // Verify unwatched transitions before WatchStatus(Unknown) return the latest status.
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);
        lease_ctrl.set_status(fpb::LeaseStatus::Pending);
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Pending
        );

        // Verify unwatched round-trip transitions back to last_status park WatchStatus rather than
        // replaying stale intermediate statuses.
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Pending).await.unwrap(),
            fpb::LeaseStatus::Satisfied
        );
        lease_ctrl.set_status(fpb::LeaseStatus::Pending);
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);

        let mut watch_fut = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut watch_fut).await,
            Poll::Pending
        );
    }

    // Verify synchronous back-to-back set_status calls while WatchStatus is parked queue only a
    // single waking transition and coalesce remaining updates into current_status.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_parked_watch_queues_single_waking_transition() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let proxy = client_end.into_proxy();
        let mut parked_watch = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );

        // Issue three synchronous transitions while parked; the first wakes parked_watch and the
        // remaining unwatched updates coalesce into Satisfied.
        lease_ctrl.set_status(fpb::LeaseStatus::Pending);
        lease_ctrl.set_status(fpb::LeaseStatus::PoweringDown);
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);

        assert_eq!(parked_watch.await.unwrap(), fpb::LeaseStatus::Pending);
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Pending).await.unwrap(),
            fpb::LeaseStatus::Satisfied
        );

        let mut next_watch = proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut next_watch).await,
            Poll::Pending
        );
    }

    // Verify close() and wait_until_closed() complete when set_status() is called after a client
    // stops calling WatchStatus while keeping its LeaseControl proxy open.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_close_completes_after_client_stops_watching() {
        let (lease_ctrl, client_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Pending);
        let proxy = client_end.into_proxy();
        assert_eq!(
            proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Pending
        );

        // Stop calling watch_status while keeping proxy alive across set_status and close.
        lease_ctrl.set_status(fpb::LeaseStatus::Satisfied);
        lease_ctrl.close();
        lease_ctrl.wait_until_closed().await;
        assert_matches!(
            proxy.watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify dropping one FakeLeaseControl clone keeps an idle channel open until the last clone
    // drops.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_drop_closes_idle_channel() {
        let (drop_ctrl, drop_end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let drop_ctrl_clone = drop_ctrl.clone();
        let drop_proxy = drop_end.into_proxy();
        drop(drop_ctrl);
        assert_eq!(
            drop_proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Satisfied
        );
        drop(drop_ctrl_clone);
        assert_matches!(
            drop_proxy.watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify dropping FakeLeaseControl while WatchStatus is parked aborts the server task and
    // resolves the parked WatchStatus with PEER_CLOSED.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_drop_closes_parked_watch_status() {
        let (parked_drop_ctrl, parked_drop_end) =
            FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let parked_drop_proxy = parked_drop_end.into_proxy();
        let mut parked_drop_watch = parked_drop_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_drop_watch).await,
            Poll::Pending
        );
        drop(parked_drop_ctrl);
        assert_matches!(parked_drop_watch.await, Err(fidl::Error::ClientChannelClosed { .. }));
    }

    // Verify closing the client channel while WatchStatus is parked wakes wait_until_closed and
    // marks the lease inactive.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_client_close_wakes_wait_until_closed() {
        let (client_close_ctrl, client_close_end) =
            FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
        let client_close_proxy = client_close_end.into_proxy();
        let mut parked_watch = client_close_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );
        drop(parked_watch);
        drop(client_close_proxy);
        client_close_ctrl.wait_until_closed().await;
        assert!(!client_close_ctrl.is_active());
    }

    // Verify cancelling an enclosing Scope cleans up an active idle FakeLeaseControl and wakes
    // wait_until_closed.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_scope_cancellation_when_idle() {
        let child_scope = fasync::Scope::new();
        let (scoped_ctrl, scoped_end) = child_scope
            .compute(async { FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied) })
            .await;
        let scoped_proxy = scoped_end.into_proxy();
        assert_eq!(
            scoped_proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Satisfied
        );
        let mut wait_closed_fut = pin!(scoped_ctrl.wait_until_closed());
        assert_matches!(futures::poll!(&mut wait_closed_fut), Poll::Pending);
        child_scope.cancel().await;
        wait_closed_fut.await;
        assert!(!scoped_ctrl.is_active());
        assert_matches!(
            scoped_proxy.watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify cancelling an enclosing Scope while WatchStatus is parked resolves the parked
    // WatchStatus with PEER_CLOSED, wakes wait_until_closed, and marks the lease inactive.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_scope_cancellation_while_watch_status_parked() {
        let parked_scope = fasync::Scope::new();
        let (parked_ctrl, parked_end) = parked_scope
            .compute(async { FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied) })
            .await;
        let parked_proxy = parked_end.into_proxy();
        let mut parked_watch = parked_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );
        let mut parked_wait_closed = pin!(parked_ctrl.wait_until_closed());
        assert_matches!(futures::poll!(&mut parked_wait_closed), Poll::Pending);
        parked_scope.cancel().await;
        assert_matches!(parked_watch.await, Err(fidl::Error::ClientChannelClosed { .. }));
        parked_wait_closed.await;
        assert!(!parked_ctrl.is_active());
    }

    // Verify cancelling an enclosing Scope before the spawned server task is ever polled still runs
    // LeaseStreamGuard drop.
    #[fuchsia::test(allow_stalls = false)]
    async fn lease_control_scope_cancellation_before_initial_poll() {
        let prepoll_scope = fasync::Scope::new();
        let prepoll_handle = prepoll_scope.to_handle();
        let (prepoll_ctrl, prepoll_end, cancel_join) = prepoll_handle
            .compute(async move {
                let (ctrl, end) = FakeLeaseControl::spawn(fpb::LeaseStatus::Satisfied);
                {
                    let mut wait_closed = pin!(ctrl.wait_until_closed());
                    assert_matches!(futures::poll!(&mut wait_closed), Poll::Pending);
                }
                let cancel_join = prepoll_scope.cancel();
                (ctrl, end, cancel_join)
            })
            .await;
        cancel_join.await;
        prepoll_ctrl.wait_until_closed().await;
        assert!(!prepoll_ctrl.is_active());
        assert_matches!(
            prepoll_end.into_proxy().watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify FakeLessor tracks leases, default status updates, concurrent FIFO next_lease waiters,
    // and multiple Lessor connections.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_tracks_leases_across_connections_and_waiters() {
        // Verify create_proxy and serve_stream attach multiple Lessor connections to one
        // FakeLessor.
        let lessor = FakeLessor::new(fpb::LeaseStatus::Satisfied);
        let proxy_a = lessor.create_proxy();
        let (proxy_b, stream_b) = create_proxy_and_stream::<fpb::LessorMarker>();
        lessor.serve_stream(stream_b);

        // Verify two concurrent next_lease tasks resolve one lease each in FIFO order.
        let scope = fasync::Scope::current();
        let lessor_1 = lessor.clone();
        let mut first_waiter = scope.compute(async move { lessor_1.next_lease().await });
        let lessor_2 = lessor.clone();
        let mut second_waiter = scope.compute(async move { lessor_2.next_lease().await });
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut first_waiter).await,
            Poll::Pending
        );
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut second_waiter).await,
            Poll::Pending
        );

        let client_1 = proxy_a.lease(2).await.unwrap().unwrap();
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut second_waiter).await,
            Poll::Pending
        );
        let lease_1 = first_waiter.await;
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut second_waiter).await,
            Poll::Pending
        );
        assert_eq!(lease_1.level(), Some(2));
        assert_eq!(lease_1.status(), fpb::LeaseStatus::Satisfied);
        assert_eq!(lessor.active_lease_count(), 1);

        // Verify set_default_status applies to leases minted on a second connection and resolves
        // the second waiter.
        lessor.set_default_status(fpb::LeaseStatus::Pending);
        let client_2 = proxy_b.lease(5).await.unwrap().unwrap();
        let lease_2 = second_waiter.await;
        assert_eq!(lease_2.level(), Some(5));
        assert_eq!(lease_2.status(), fpb::LeaseStatus::Pending);
        assert_eq!(lessor.leased_levels(), vec![2, 5]);
        assert_eq!(lessor.leases().len(), 2);
        assert_eq!(lessor.active_lease_count(), 2);

        let proxy_2 = client_2.into_proxy();
        assert_eq!(
            proxy_2.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Pending
        );

        // Verify dropping client endpoints closes the leases and decrements active_lease_count.
        drop(client_1);
        drop(proxy_2);
        lease_1.wait_until_closed().await;
        lease_2.wait_until_closed().await;
        assert_eq!(lessor.active_lease_count(), 0);

        // Verify dropping a parked front waiter wakes the next queued waiter when an unconsumed
        // lease is already available.
        let lessor_3 = lessor.clone();
        let mut promoted_waiter = scope.compute(async move { lessor_3.next_lease().await });
        let client_3 = {
            let mut cancelled_front = pin!(lessor.next_lease());
            assert_matches!(futures::poll!(&mut cancelled_front), Poll::Pending);
            assert_matches!(
                fasync::TestExecutor::poll_until_stalled(&mut promoted_waiter).await,
                Poll::Pending
            );
            let client_3 = proxy_a.lease(8).await.unwrap().unwrap();
            assert_matches!(
                fasync::TestExecutor::poll_until_stalled(&mut promoted_waiter).await,
                Poll::Pending
            );
            client_3
        };
        let lease_3 = promoted_waiter.await;
        assert_eq!(lease_3.level(), Some(8));
        drop(client_3);
        lease_3.wait_until_closed().await;
        assert_eq!(lessor.active_lease_count(), 0);
    }

    // Verify FakeLessor valid_levels validation and set_lease_error injection leave recorded lease
    // state unchanged on failure.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_valid_levels_and_error_injection() {
        let lessor = FakeLessor::with_valid_levels(fpb::LeaseStatus::Satisfied, [1, 3]);
        let proxy = lessor.create_proxy();

        // Verify invalid levels fail with InvalidLevel without recording a lease.
        assert_matches!(proxy.lease(0).await, Ok(Err(fpb::LeaseError::InvalidLevel)));
        assert_matches!(proxy.lease(2).await, Ok(Err(fpb::LeaseError::InvalidLevel)));
        assert!(lessor.leased_levels().is_empty());
        assert!(lessor.leases().is_empty());
        assert_eq!(lessor.active_lease_count(), 0);

        // Verify valid levels succeed on the same Lessor channel.
        let valid_client = proxy.lease(3).await.unwrap().unwrap();
        assert_eq!(lessor.next_lease().await.level(), Some(3));
        assert_eq!(lessor.leased_levels(), vec![3]);
        assert_eq!(lessor.leases().len(), 1);
        assert_eq!(lessor.active_lease_count(), 1);

        // Verify set_lease_error takes precedence over valid_levels and fails without modifying
        // lease state.
        lessor.set_lease_error(Some(fpb::LeaseError::NotAuthorized));
        assert_matches!(proxy.lease(1).await, Ok(Err(fpb::LeaseError::NotAuthorized)));
        assert_matches!(proxy.lease(2).await, Ok(Err(fpb::LeaseError::NotAuthorized)));
        assert_eq!(lessor.leased_levels(), vec![3]);
        assert_eq!(lessor.leases().len(), 1);
        assert_eq!(lessor.active_lease_count(), 1);

        // Verify clearing set_lease_error and updating set_valid_levels dynamically takes effect.
        lessor.set_lease_error(None);
        lessor.set_valid_levels(None);
        let unrestricted_client = proxy.lease(9).await.unwrap().unwrap();
        assert_eq!(lessor.next_lease().await.level(), Some(9));
        assert_eq!(lessor.leased_levels(), vec![3, 9]);
        assert_eq!(lessor.leases().len(), 2);
        assert_eq!(lessor.active_lease_count(), 2);

        lessor.set_valid_levels(Some(vec![1, 3]));
        assert_matches!(proxy.lease(9).await, Ok(Err(fpb::LeaseError::InvalidLevel)));
        assert_eq!(lessor.leased_levels(), vec![3, 9]);
        assert_eq!(lessor.leases().len(), 2);
        assert_eq!(lessor.active_lease_count(), 2);

        drop((valid_client, unrestricted_client));
    }

    // Verify dropping one FakeLessor clone keeps the server open and shares state until the last
    // clone drops, while retaining a FakeLeaseControl clone keeps the minted lease open.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_drop_keeps_retained_lease_open() {
        let (lessor, proxy) = FakeLessor::spawn(fpb::LeaseStatus::Pending);
        let lessor_clone = lessor.clone();
        drop(lessor);
        let surviving_client = proxy.lease(3).await.unwrap().unwrap();
        let surviving_lease = lessor_clone.next_lease().await;
        assert_eq!(lessor_clone.leased_levels(), vec![3]);
        let surviving_proxy = surviving_client.into_proxy();
        drop(lessor_clone);
        assert_matches!(proxy.lease(1).await, Err(fidl::Error::ClientChannelClosed { .. }));
        assert!(surviving_lease.is_active());
        assert_eq!(
            surviving_proxy.watch_status(fpb::LeaseStatus::Unknown).await.unwrap(),
            fpb::LeaseStatus::Pending
        );
        surviving_lease.set_status(fpb::LeaseStatus::Satisfied);
        assert_eq!(
            surviving_proxy.watch_status(fpb::LeaseStatus::Pending).await.unwrap(),
            fpb::LeaseStatus::Satisfied
        );
        drop(surviving_proxy);
        surviving_lease.wait_until_closed().await;
        assert!(!surviving_lease.is_active());
    }

    // Verify dropping FakeLessor when no FakeLeaseControl clone is retained aborts minted
    // LeaseControl tasks and closes their channels even while WatchStatus is parked.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_drop_closes_unretained_lease() {
        let (unretained_lessor, unretained_proxy) = FakeLessor::spawn(fpb::LeaseStatus::Satisfied);
        let unretained_client = unretained_proxy.lease(4).await.unwrap().unwrap();
        assert_eq!(unretained_lessor.next_lease().await.level(), Some(4));
        let unretained_lease_proxy = unretained_client.into_proxy();
        let mut parked_unretained_watch =
            unretained_lease_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_unretained_watch).await,
            Poll::Pending
        );
        drop(unretained_lessor);
        assert_matches!(
            unretained_proxy.lease(1).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
        assert_matches!(
            parked_unretained_watch.await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
        assert_matches!(
            unretained_lease_proxy.watch_status(fpb::LeaseStatus::Unknown).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify FakeLessor::spawn and leases() return handles to all minted leases in order.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_spawn_and_leases_preserve_mint_order() {
        let (spawned_lessor, spawned_proxy) = FakeLessor::spawn(fpb::LeaseStatus::Satisfied);
        assert!(spawned_lessor.leases().is_empty());
        let first_client = spawned_proxy.lease(4).await.unwrap().unwrap();
        let second_client = spawned_proxy.lease(7).await.unwrap().unwrap();
        spawned_lessor.next_lease().await;
        spawned_lessor.next_lease().await;
        let all_leases = spawned_lessor.leases();
        assert_eq!(all_leases.len(), 2);
        assert_eq!(all_leases[0].level(), Some(4));
        assert_eq!(all_leases[1].level(), Some(7));
        drop((first_client, second_client));
    }

    // Verify calling FakeLeaseControl::close on a FakeLessor-minted lease decrements
    // active_lease_count while retaining the closed lease handle in FakeLessor::leases.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_minted_lease_close_updates_active_count() {
        let (lessor, proxy) = FakeLessor::spawn(fpb::LeaseStatus::Satisfied);
        let lease_client = proxy.lease(4).await.unwrap().unwrap();
        let lease = lessor.next_lease().await;
        let lease_proxy = lease_client.into_proxy();
        assert_eq!(lessor.active_lease_count(), 1);
        assert_eq!(lessor.leases().len(), 1);

        // Verify server-side close() decrements active_lease_count without removing the lease from
        // lessor.leases().
        let mut parked_watch = lease_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );
        lease.close();
        assert_eq!(lessor.active_lease_count(), 0);
        assert_eq!(lessor.leases().len(), 1);
        assert!(!lessor.leases()[0].is_active());
        assert_eq!(parked_watch.await.unwrap(), fpb::LeaseStatus::Vacated);
        lease.wait_until_closed().await;
        assert_matches!(
            lease_proxy.watch_status(fpb::LeaseStatus::Vacated).await,
            Err(fidl::Error::ClientChannelClosed { .. })
        );
    }

    // Verify cancelling the enclosing Scope captured at FakeLessor::new construction tears down
    // both the Lessor channel and any minted LeaseControl channels, even when a FakeLeaseControl
    // clone is retained by the caller.
    #[fuchsia::test(allow_stalls = false)]
    async fn lessor_scope_cancellation_tears_down_retained_lease() {
        let child_scope = fasync::Scope::new();
        let (lessor, proxy) = child_scope
            .compute(async {
                let lessor = FakeLessor::new(fpb::LeaseStatus::Satisfied);
                let proxy = lessor.create_proxy();
                (lessor, proxy)
            })
            .await;

        // Mint a lease, retain a FakeLeaseControl clone, and park a WatchStatus call before
        // cancelling child_scope.
        let lease_client = proxy.lease(6).await.unwrap().unwrap();
        let retained_lease = lessor.next_lease().await;
        let lease_proxy = lease_client.into_proxy();
        let mut parked_watch = lease_proxy.watch_status(fpb::LeaseStatus::Satisfied);
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut parked_watch).await,
            Poll::Pending
        );

        // Verify cancelling child_scope closes both the Lessor proxy and the retained lease.
        child_scope.cancel().await;
        assert_matches!(proxy.lease(1).await, Err(fidl::Error::ClientChannelClosed { .. }));
        assert_matches!(parked_watch.await, Err(fidl::Error::ClientChannelClosed { .. }));
        retained_lease.wait_until_closed().await;
        assert!(!retained_lease.is_active());
        assert_eq!(lessor.active_lease_count(), 0);
    }
}
