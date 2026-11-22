// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{ClientEnd, create_request_stream};
use fidl_fuchsia_power_broker as fpb;
use fuchsia_async as fasync;
use fuchsia_async::condition::Condition;
use futures::StreamExt;
use std::fmt;
use std::sync::{Arc, Weak};
use std::task::Poll;

#[derive(Default)]
struct RunnerState {
    level_history: Vec<fpb::PowerLevel>,
    channel_closed: bool,
}

struct RunnerStreamGuard {
    stream: Option<fpb::ElementRunnerRequestStream>,
    weak: Weak<RunnerInner>,
}

impl Drop for RunnerStreamGuard {
    fn drop(&mut self) {
        // Drop the request stream before waking `wait_for_level` waiters so the client channel
        // observes `PEER_CLOSED` even when the server task is aborted at an `.await` point.
        drop(self.stream.take());
        if let Some(inner) = self.weak.upgrade() {
            let mut state = inner.state.lock();
            state.channel_closed = true;
            for waker in state.drain_wakers() {
                waker.wake();
            }
        }
    }
}

struct RunnerInner {
    scope: fasync::Scope,
    state: Condition<RunnerState>,
}

/// Minimal `fuchsia.power.broker/ElementRunner` server that acknowledges incoming `SetLevel`
/// requests and records power level transitions.
///
/// Dropping all clones of a `BasicElementRunner` aborts its internal server task and closes the
/// `ElementRunner` channel.
#[must_use = "dropping BasicElementRunner aborts its server task and closes the channel"]
#[derive(Clone)]
pub struct BasicElementRunner {
    inner: Arc<RunnerInner>,
}

impl fmt::Debug for BasicElementRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.state.lock();
        f.debug_struct("BasicElementRunner")
            .field("current_level", &state.level_history.last().copied())
            .field("level_history", &state.level_history)
            .field("channel_closed", &state.channel_closed)
            .finish()
    }
}

impl BasicElementRunner {
    /// Spawns a new `BasicElementRunner` and returns `(Self, ClientEnd<ElementRunnerMarker>)`.
    ///
    /// The server task runs on a child of [`fuchsia_async::Scope::current`]; dropping all clones of
    /// the returned `BasicElementRunner` or cancelling that enclosing scope aborts the server task
    /// and closes the `ElementRunner` channel.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn spawn() -> (Self, ClientEnd<fpb::ElementRunnerMarker>) {
        let (client_end, stream) = create_request_stream::<fpb::ElementRunnerMarker>();
        (Self::serve_stream(stream), client_end)
    }

    /// Serves an existing `ElementRunnerRequestStream` on a child of
    /// [`fuchsia_async::Scope::current`].
    ///
    /// Dropping all clones of the returned `BasicElementRunner` or cancelling that enclosing scope
    /// aborts the server task and closes the `ElementRunner` channel.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an active `fuchsia_async` executor.
    pub fn serve_stream(stream: fpb::ElementRunnerRequestStream) -> Self {
        let inner =
            Arc::new(RunnerInner { scope: fasync::Scope::new(), state: Condition::default() });
        let runner = Self { inner };
        let mut guard =
            RunnerStreamGuard { stream: Some(stream), weak: Arc::downgrade(&runner.inner) };
        runner.inner.scope.spawn(async move {
            while let Some(Ok(request)) =
                guard.stream.as_mut().expect("stream stays Some until guard drops").next().await
            {
                match request {
                    fpb::ElementRunnerRequest::SetLevel { level, responder } => {
                        let Some(inner) = guard.weak.upgrade() else {
                            break;
                        };
                        let mut state = inner.state.lock();
                        state.level_history.push(level);
                        let _ = responder.send();
                        for waker in state.drain_wakers() {
                            waker.wake();
                        }
                    }
                    fpb::ElementRunnerRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("BasicElementRunner ignoring unknown method ordinal: {ordinal}");
                    }
                }
            }
        });
        runner
    }

    /// Returns the most recent power level set via `ElementRunner::SetLevel`.
    pub fn current_level(&self) -> Option<fpb::PowerLevel> {
        self.inner.state.lock().level_history.last().copied()
    }

    /// Returns the ordered history of power levels received via `ElementRunner::SetLevel`.
    pub fn level_history(&self) -> Vec<fpb::PowerLevel> {
        self.inner.state.lock().level_history.clone()
    }

    /// Waits until `expected` is observed at or after the current level when first polled
    /// (returning immediately if `current_level() == Some(expected)`, or once a subsequent
    /// `ElementRunner::SetLevel` reaches `expected`, even across back-to-back transitions).
    ///
    /// # Panics
    ///
    /// Panics if the `ElementRunner` channel closes before `expected` is observed.
    pub async fn wait_for_level(&self, expected: fpb::PowerLevel) {
        let mut start_index = None;
        self.inner
            .state
            .when(|state| {
                let start =
                    *start_index.get_or_insert_with(|| state.level_history.len().saturating_sub(1));
                if state.level_history[start..].contains(&expected) {
                    return Poll::Ready(());
                }
                assert!(
                    !state.channel_closed,
                    "ElementRunner channel closed before reaching power level {expected} (level_history: {:?})",
                    state.level_history,
                );
                Poll::Pending
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use std::pin::pin;

    // Verify levels are correctly updated, tracked, and closed on drop in BasicElementRunner::spawn.
    #[fuchsia::test(allow_stalls = false)]
    async fn basic_element_runner_tracks_levels() {
        let (runner, client_end) = BasicElementRunner::spawn();

        // Verify initial current_level and level_history are empty.
        assert_eq!(runner.current_level(), None);
        assert!(runner.level_history().is_empty());

        let proxy = client_end.into_proxy();
        {
            // Verify wait_for_level parks until SetLevel reaches the target level.
            let mut wait_for_3 = pin!(runner.wait_for_level(3));
            assert_matches!(futures::poll!(&mut wait_for_3), Poll::Pending);

            proxy.set_level(1).await.unwrap();
            assert_matches!(futures::poll!(&mut wait_for_3), Poll::Pending);

            proxy.set_level(3).await.unwrap();
            wait_for_3.await;
        }

        // Verify wait_for_level returns immediately at the target level and records history.
        runner.wait_for_level(3).await;
        assert_eq!(runner.current_level(), Some(3));
        assert_eq!(runner.level_history(), vec![1, 3]);

        // Verify dropping one clone keeps the channel open and shares state until the last clone
        // drops.
        let runner_clone = runner.clone();
        drop(runner);
        proxy.set_level(5).await.unwrap();
        assert_eq!(runner_clone.current_level(), Some(5));
        assert_eq!(runner_clone.level_history(), vec![1, 3, 5]);
        drop(runner_clone);
        assert_matches!(proxy.set_level(1).await, Err(fidl::Error::ClientChannelClosed { .. }));
    }

    // Verify BasicElementRunner::serve_stream, concurrent waiters on distinct target levels, and
    // channel closure on drop.
    #[fuchsia::test(allow_stalls = false)]
    async fn basic_element_runner_serve_stream_concurrent_waiters_and_drop() {
        // Verify serve_stream serves an external request stream and non-matching wait_for_level
        // futures re-park when state.drain_wakers() wakes all waiters.
        let (proxy, stream) = create_proxy_and_stream::<fpb::ElementRunnerMarker>();
        let runner = BasicElementRunner::serve_stream(stream);

        {
            let mut wait_for_2 = pin!(runner.wait_for_level(2));
            let mut wait_for_4 = pin!(runner.wait_for_level(4));
            assert_matches!(futures::poll!(&mut wait_for_2), Poll::Pending);
            assert_matches!(futures::poll!(&mut wait_for_4), Poll::Pending);

            proxy.set_level(1).await.unwrap();
            assert_matches!(futures::poll!(&mut wait_for_2), Poll::Pending);
            assert_matches!(futures::poll!(&mut wait_for_4), Poll::Pending);

            proxy.set_level(2).await.unwrap();
            wait_for_2.await;
            assert_matches!(futures::poll!(&mut wait_for_4), Poll::Pending);

            proxy.set_level(4).await.unwrap();
            wait_for_4.await;
        }
        assert_eq!(runner.level_history(), vec![1, 2, 4]);

        // Verify dropping BasicElementRunner closes the ElementRunner channel.
        drop(runner);
        assert_matches!(proxy.set_level(1).await, Err(fidl::Error::ClientChannelClosed { .. }));
    }

    // Verify BasicElementRunner::wait_for_level ignores earlier historical levels, resolves across
    // back-to-back transitions, and still succeeds for already-reached levels after peer close.
    #[fuchsia::test(allow_stalls = false)]
    async fn basic_element_runner_back_to_back_transitions_and_closed_history() {
        let (runner, client_end) = BasicElementRunner::spawn();
        let proxy = client_end.into_proxy();

        // Verify levels visited prior to current_level do not satisfy a new wait_for_level call.
        proxy.set_level(3).await.unwrap();
        proxy.set_level(1).await.unwrap();
        let mut wait_for_3 = pin!(runner.wait_for_level(3));
        assert_matches!(futures::poll!(&mut wait_for_3), Poll::Pending);

        // Verify back-to-back transitions passing through 3 before wait_for_3 is polled again
        // resolve wait_for_3 even though current_level has moved on to 0.
        proxy.set_level(3).await.unwrap();
        proxy.set_level(0).await.unwrap();
        assert_eq!(runner.current_level(), Some(0));
        wait_for_3.await;
        assert_eq!(runner.level_history(), vec![3, 1, 3, 0]);

        // Verify wait_for_level(0) still returns cleanly after the client closes the channel.
        drop(proxy);
        let _: Poll<()> =
            fasync::TestExecutor::poll_until_stalled(std::future::pending::<()>()).await;
        runner.wait_for_level(0).await;
    }

    // Verify BasicElementRunner::wait_for_level panics when the ElementRunner channel closes before
    // the expected level is observed. Disable diagnostics logging so the expected panic hook does
    // not emit an ERROR log that fails the test component's max-severity check.
    #[fuchsia::test(logging = false, allow_stalls = false)]
    #[should_panic(expected = "ElementRunner channel closed before reaching power level 5")]
    async fn basic_element_runner_wait_for_level_panics_on_channel_close() {
        // Park a wait_for_level(5) future while the runner is at level 1.
        let (runner, client_end) = BasicElementRunner::spawn();
        let proxy = client_end.into_proxy();
        proxy.set_level(1).await.unwrap();

        let mut wait_for_5 = pin!(runner.wait_for_level(5));
        assert_matches!(futures::poll!(&mut wait_for_5), Poll::Pending);

        // Closing the client channel wakes wait_for_5 and panics because level 5 was never reached.
        drop(proxy);
        wait_for_5.await;
    }

    // Verify cancelling the enclosing Scope closes the ElementRunner channel even while
    // BasicElementRunner is retained, while still allowing wait_for_level on already-reached levels.
    #[fuchsia::test(allow_stalls = false)]
    async fn basic_element_runner_scope_cancellation_closes_channel() {
        let child_scope = fasync::Scope::new();
        let (runner, client_end) = child_scope.compute(async { BasicElementRunner::spawn() }).await;
        let proxy = client_end.into_proxy();
        proxy.set_level(2).await.unwrap();

        child_scope.cancel().await;
        assert_matches!(proxy.set_level(3).await, Err(fidl::Error::ClientChannelClosed { .. }));
        runner.wait_for_level(2).await;
    }

    // Verify cancelling the enclosing Scope wakes and panics a parked BasicElementRunner
    // wait_for_level call when the expected level was not reached.
    #[fuchsia::test(logging = false, allow_stalls = false)]
    #[should_panic(expected = "ElementRunner channel closed before reaching power level 5")]
    async fn basic_element_runner_scope_cancellation_panics_parked_waiter() {
        let child_scope = fasync::Scope::new();
        let (runner, client_end) = child_scope.compute(async { BasicElementRunner::spawn() }).await;
        let proxy = client_end.into_proxy();
        proxy.set_level(1).await.unwrap();

        let mut wait_for_5 = pin!(runner.wait_for_level(5));
        assert_matches!(futures::poll!(&mut wait_for_5), Poll::Pending);
        child_scope.cancel().await;
        wait_for_5.await;
    }
}
