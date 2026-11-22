// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use async_utils::hanging_get::client::HangingGetStream;
use fidl_fuchsia_bluetooth_hfp as hfp;
use fuchsia_async::Task;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_sync::Mutex;
use futures::stream::FuturesUnordered;
use futures::{StreamExt, select};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::ops::RangeFrom;
use std::sync::Arc;

use crate::fidl::call::{Call, LocalCallId};

pub type LocalPeerId = u64;

#[derive(Clone)]
pub struct Peer {
    pub local_id: LocalPeerId,
    canonical_id: PeerId,
    pub proxy: hfp::PeerHandlerProxy,
}

struct PeerHandlerProxyTask {
    local_id: LocalPeerId,
    canonical_id: PeerId,

    next_local_call_id: Arc<Mutex<RangeFrom<LocalCallId>>>,
    calls: Arc<Mutex<HashMap<LocalCallId, Call>>>,
    call_ids: BTreeSet<LocalCallId>,
    call_tasks: FuturesUnordered<Task<LocalCallId>>,

    proxy: hfp::PeerHandlerProxy,
}

impl fmt::Debug for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer {}: [ peer id: {} ]", self.local_id, self.canonical_id)
    }
}

impl fmt::Debug for PeerHandlerProxyTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer {}: [ peer id: {} ]", self.local_id, self.canonical_id)
    }
}

impl Peer {
    pub fn new_peer_and_task(
        local_id: LocalPeerId,
        canonical_id: PeerId,
        next_local_call_id: Arc<Mutex<RangeFrom<LocalCallId>>>,
        calls: Arc<Mutex<HashMap<LocalCallId, Call>>>,
        proxy: hfp::PeerHandlerProxy,
    ) -> (Peer, Task<LocalPeerId>) {
        let call_ids = BTreeSet::new();
        let call_tasks = FuturesUnordered::new();

        let peer_task = PeerHandlerProxyTask {
            local_id,
            canonical_id,
            next_local_call_id,
            calls,
            call_ids,
            call_tasks,
            proxy: proxy.clone(),
        };
        let peer_fut = peer_task.run();
        let task = Task::local(peer_fut);

        let peer = Peer { local_id, canonical_id, proxy };
        (peer, task)
    }
}

impl PeerHandlerProxyTask {
    async fn run(mut self) -> LocalPeerId {
        let result = self.run_inner().await;
        if let Err(err) = result {
            println!("Error running peer task for peer {self:?}: {err:?}")
        }

        self.call_tasks.clear();
        let remaining_call_ids = std::mem::take(&mut self.call_ids);
        for call_id in remaining_call_ids {
            self.handle_finished_call(call_id);
        }

        self.local_id
    }

    async fn run_inner(&mut self) -> Result<()> {
        let mut next_call_stream =
            HangingGetStream::new(self.proxy.clone(), hfp::PeerHandlerProxy::watch_next_call);

        loop {
            // If the collection is empty, `poll_next` may return `Ready(None)`.  However, we
            // don't want to exit in that case as it may have more calls in the future.
            let mut finished_call_fut = self.call_tasks.select_next_some();
            let mut next_call_fut = next_call_stream.next();

            select! {
                finished_call = finished_call_fut => {
                    self.handle_finished_call(finished_call);
                }
                next_call = next_call_fut => {
                    let next_call = match next_call {
                        Some(Ok(call)) => call,
                        Some(Err(fidl::Error::ClientChannelClosed { .. })) | None => return Ok(()),
                        Some(Err(err)) => Err(format_err!("FIDL error: {err}"))?,
                    };
                    self.handle_next_call(next_call)?;
                }
            }
        }
    }

    fn handle_finished_call(&mut self, local_call_id: LocalCallId) {
        let _ = self.call_ids.remove(&local_call_id);
        let mut calls = self.calls.lock();
        if let Some(removed_call) = calls.remove(&local_call_id) {
            println!("Call {local_call_id} ended: {removed_call:?}")
        } else {
            println!("BUG: Unknown call {} removed.", local_call_id)
        }
    }

    fn handle_next_call(&mut self, next_call: hfp::NextCall) -> Result<()> {
        let next_call_debug = format!("{:?}", next_call);

        let client_end = next_call.call.ok_or_else(|| {
            format_err!("Missing Call client end on received call {}", next_call_debug)
        })?;
        let proxy = client_end.into_proxy();

        let number = next_call
            .remote
            .ok_or_else(|| format_err!("Missing number on received call {}", next_call_debug))?;
        let direction = next_call
            .direction
            .ok_or_else(|| format_err!("Missing direction on received call {}", next_call_debug))?;
        let state = next_call
            .state
            .ok_or_else(|| format_err!("Missing state on received call {}", next_call_debug))?;

        let local_id =
            self.next_local_call_id.lock().next().expect("Couldn't get next local call id.");

        let (call, task) =
            Call::new_call_with_task(local_id, number, direction, state, self.calls.clone(), proxy);
        println!("New call: {call:?}");

        let mut calls = self.calls.lock();
        let no_previous_call = calls.insert(local_id, call);
        // This should be impossible as we increment the call id every time.
        assert!(no_previous_call.is_none(), "Reused local call ID.");

        let newly_inserted = self.call_ids.insert(local_id);
        assert!(newly_inserted, "Reused local call ID in peer task.");
        self.call_tasks.push(task);

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fuchsia_async as fasync;
    use std::future::Future;
    use std::task::Poll;

    const LOCAL_PEER_ID: LocalPeerId = 7;
    const PEER_ID: PeerId = PeerId(1);
    const TEST_NUMBER_1: &str = "+15551234567";
    const TEST_NUMBER_2: &str = "+15557654321";

    /// Returns the responder of the pending `WatchNextCall` request on `peer_stream`.
    fn expect_watch_next_call(
        exec: &mut fasync::TestExecutor,
        peer_stream: &mut hfp::PeerHandlerRequestStream,
    ) -> hfp::PeerHandlerWatchNextCallResponder {
        let mut watch_call_fut = peer_stream.next();
        let Poll::Ready(Some(Ok(hfp::PeerHandlerRequest::WatchNextCall { responder }))) =
            exec.run_until_stalled(&mut watch_call_fut)
        else {
            panic!("Expected WatchNextCall request");
        };
        responder
    }

    /// Returns the responder of the pending `WatchState` request on `call_stream`.
    pub(crate) fn expect_watch_state(
        exec: &mut fasync::TestExecutor,
        call_stream: &mut hfp::CallRequestStream,
    ) -> hfp::CallWatchStateResponder {
        let mut watch_state_fut = call_stream.next();
        let Poll::Ready(Some(Ok(hfp::CallRequest::WatchState { responder }))) =
            exec.run_until_stalled(&mut watch_state_fut)
        else {
            panic!("Expected WatchState request");
        };
        responder
    }

    /// Answers the pending `WatchNextCall` request on `peer_stream` with a new active call to
    /// `number` and returns the server end of that call's `Call` channel.  `task` is the future
    /// that drives the peer and is expected to still be running afterwards.
    pub(crate) fn start_call<F: Future + Unpin>(
        exec: &mut fasync::TestExecutor,
        task: &mut F,
        peer_stream: &mut hfp::PeerHandlerRequestStream,
        number: &str,
        direction: hfp::CallDirection,
    ) -> hfp::CallRequestStream {
        let responder = expect_watch_next_call(exec, peer_stream);
        let (call_client_end, call_stream) =
            fidl::endpoints::create_request_stream::<hfp::CallMarker>();
        let next_call = hfp::NextCall {
            call: Some(call_client_end),
            remote: Some(number.to_string()),
            state: Some(hfp::CallState::OngoingActive),
            direction: Some(direction),
            ..Default::default()
        };
        responder.send(next_call).expect("Send next call");
        assert!(exec.run_until_stalled(task).is_pending());
        call_stream
    }

    fn new_peer_task()
    -> (Arc<Mutex<HashMap<LocalCallId, Call>>>, Task<LocalPeerId>, hfp::PeerHandlerRequestStream)
    {
        let (proxy, peer_stream) =
            fidl::endpoints::create_proxy_and_stream::<hfp::PeerHandlerMarker>();
        let calls = Arc::new(Mutex::new(HashMap::new()));
        let next_local_call_id = Arc::new(Mutex::new(RangeFrom { start: 1 }));
        let (_peer, task) = Peer::new_peer_and_task(
            LOCAL_PEER_ID,
            PEER_ID,
            next_local_call_id,
            calls.clone(),
            proxy,
        );
        (calls, task, peer_stream)
    }

    #[track_caller]
    fn expect_stream_closed(
        exec: &mut fasync::TestExecutor,
        call_stream: &mut hfp::CallRequestStream,
    ) {
        let mut next_fut = call_stream.next();
        let Poll::Ready(None) = exec.run_until_stalled(&mut next_fut) else {
            panic!("Expected Call channel to be closed");
        };
    }

    #[fuchsia::test]
    fn remaining_calls_are_cleaned_up_when_peer_with_multiple_calls_exits() {
        let mut exec = fasync::TestExecutor::new();
        let (calls, mut task, mut peer_stream) = new_peer_task();
        assert!(exec.run_until_stalled(&mut task).is_pending());

        let mut call_1_stream = start_call(
            &mut exec,
            &mut task,
            &mut peer_stream,
            TEST_NUMBER_1,
            hfp::CallDirection::MobileOriginated,
        );
        let mut call_2_stream = start_call(
            &mut exec,
            &mut task,
            &mut peer_stream,
            TEST_NUMBER_2,
            hfp::CallDirection::MobileTerminated,
        );
        assert_eq!(calls.lock().len(), 2);

        // Hold on to the responder for call 2 so that only the tool can close the channel.
        let _call_2_responder = expect_watch_state(&mut exec, &mut call_2_stream);

        // Terminate call 1.  Only call 2 remains.
        let call_1_responder = expect_watch_state(&mut exec, &mut call_1_stream);
        call_1_responder.send(hfp::CallState::Terminated).expect("Send terminated state");
        assert!(exec.run_until_stalled(&mut task).is_pending());
        assert_eq!(calls.lock().len(), 1);
        assert!(calls.lock().contains_key(&2));

        // Disconnect the peer while call 2 is still active.
        drop(peer_stream);
        assert_eq!(exec.run_until_stalled(&mut task), Poll::Ready(LOCAL_PEER_ID));
        assert!(calls.lock().is_empty());
        expect_stream_closed(&mut exec, &mut call_2_stream);
    }

    #[fuchsia::test]
    fn active_call_is_cleaned_up_when_peer_task_exits_with_error() {
        let mut exec = fasync::TestExecutor::new();
        let (calls, mut task, mut peer_stream) = new_peer_task();
        assert!(exec.run_until_stalled(&mut task).is_pending());

        let mut call_1_stream = start_call(
            &mut exec,
            &mut task,
            &mut peer_stream,
            TEST_NUMBER_1,
            hfp::CallDirection::MobileOriginated,
        );
        assert_eq!(calls.lock().len(), 1);

        // Hold on to the responder for call 1 so that only the tool can close the channel.
        let _call_1_responder = expect_watch_state(&mut exec, &mut call_1_stream);

        // A call without a remote number is an error that stops the peer task.
        let responder = expect_watch_next_call(&mut exec, &mut peer_stream);
        let (call_2_client_end, _call_2_stream) =
            fidl::endpoints::create_request_stream::<hfp::CallMarker>();
        let next_call = hfp::NextCall {
            call: Some(call_2_client_end),
            remote: None,
            state: Some(hfp::CallState::OngoingActive),
            direction: Some(hfp::CallDirection::MobileTerminated),
            ..Default::default()
        };
        responder.send(next_call).expect("Send next call");

        // The task exits even though the PeerHandler channel is still open, and hangs up the
        // active call.
        assert_eq!(exec.run_until_stalled(&mut task), Poll::Ready(LOCAL_PEER_ID));
        assert!(calls.lock().is_empty());
        expect_stream_closed(&mut exec, &mut call_1_stream);
    }
}
