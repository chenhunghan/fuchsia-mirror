// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! State shared between the sockets in a wake group and the task serving it.

use std::sync::Arc;
use std::task::{Poll, Waker};

use assert_matches::assert_matches;
use futures::Future;
use netstack3_core::sync::Mutex;
use replace_with::replace_with_and;

/// The state of a wake group.
#[derive(Debug, Default)]
enum WakeState {
    /// The client is awake.
    #[default]
    Awake,
    /// The client is asleep.
    ///
    /// Holds a [`Waker`] for data arrival notifications. That `Waker` is `None`
    /// if the wake group task hasn't yet polled the future waiting for
    /// notifications.
    Asleep(Option<Waker>),
    /// The client is waking up.
    ///
    /// Data arrived while the client was asleep, and we are in the process of
    /// waking it.
    // TODO(https://fxbug.dev/538164589): Hold delegated wake leases from
    // netdevice until the client is awake.
    Waking,
    /// The client is disconnected.
    ///
    /// No more state transitions can happen to this wake group and all
    /// notifications are no-ops.
    Defunct,
}

/// The notifier side of the underlying data availability signal.
///
/// Notifiers can be cloned to allow for multiple current producers.
#[derive(Debug, Clone)]
pub(crate) struct DataNotifier {
    inner: Arc<Mutex<WakeState>>,
}

impl DataNotifier {
    /// Notifies the watcher that data is available if the watcher is waiting.
    pub(crate) fn notify(&self) {
        let waker = replace_with_and(&mut *self.inner.lock(), |state| match state {
            WakeState::Awake | WakeState::Waking | WakeState::Defunct => (state, None),
            // RACE: The waker here is None if the watcher has called
            // client_asleep_wait_for_data but not yet polled the future. This
            // is fine because the future will just resolve on the first poll.
            WakeState::Asleep(waker) => (WakeState::Waking, waker),
        });

        if let Some(waker) = waker {
            // This doesn't need to be under the lock. The waiting future can't
            // have woken up without this call, so there isn't a race.
            waker.wake();
        }
    }
}

/// Handle to the shared wake group state, owned by the task serving the group.
#[derive(Debug)]
pub(crate) struct WakeGroupState {
    inner: Arc<Mutex<WakeState>>,
}

impl Drop for WakeGroupState {
    fn drop(&mut self) {
        // Become defunct on teardown so any lingering DataNotifiers held by
        // sockets become no-ops, and any stored wakers or held wake leases are
        // dropped.
        *self.inner.lock() = WakeState::Defunct;
    }
}

impl WakeGroupState {
    /// Creates a new [`WakeGroupState`] and [`DataNotifier`] pair.
    pub(crate) fn new() -> (Self, DataNotifier) {
        let state = WakeGroupState { inner: Arc::new(Mutex::default()) };
        let notifier = DataNotifier { inner: Arc::clone(&state.inner) };
        (state, notifier)
    }

    /// Records that the client is awake and will no longer be notified for
    /// incoming data.
    pub(crate) fn client_awake(&self) {
        let mut state = self.inner.lock();
        assert_matches!(*state, WakeState::Waking | WakeState::Asleep(_));
        *state = WakeState::Awake;
    }

    /// Records that the client is asleep and incoming data should attempt to wake it.
    ///
    /// Returns a future that completes once data has arrived.
    pub(crate) fn client_asleep_wait_for_data(&self) -> impl Future<Output = ()> + use<'_> {
        let mut state = self.inner.lock();
        assert_matches!(*state, WakeState::Awake);
        *state = WakeState::Asleep(None);

        futures::future::poll_fn(move |cx| {
            let mut state = self.inner.lock();
            match &mut *state {
                WakeState::Waking => Poll::Ready(()),
                WakeState::Asleep(stored_waker) => {
                    *stored_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
                WakeState::Awake => unreachable!("waited for data while client is awake"),
                WakeState::Defunct => unreachable!("waited for data while defunct"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications() {
        let mut exec = fuchsia_async::TestExecutor::new();

        let (state, tcp) = WakeGroupState::new();
        let udp = tcp.clone();

        // If we notify before the watcher wait has been initialized, the watcher is not
        // notified.
        tcp.notify();
        let mut fut = state.client_asleep_wait_for_data();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // If we notify after the watcher wait has been initialized, it should wake the
        // watcher future.
        tcp.notify();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
        // If we notify again before a new watcher wait is initialized, again, the
        // notification is swallowed.
        state.client_awake();
        drop(fut);

        tcp.notify();
        let mut fut = state.client_asleep_wait_for_data();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // We can notify arbitrarily many times on the same notifier or on arbitrarily
        // many notifiers attached to the same watcher, and regardless it should result
        // in the future waking.
        tcp.notify();
        udp.notify();
        tcp.notify();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
        // But the notifications are coalesced, so a subsequent wait should not
        // complete.
        state.client_awake();
        drop(fut);

        let mut fut = state.client_asleep_wait_for_data();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);
    }
}
