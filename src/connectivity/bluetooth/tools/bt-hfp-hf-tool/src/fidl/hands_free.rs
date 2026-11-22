// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use async_utils::hanging_get::client::HangingGetStream;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_bluetooth as bt;
use fidl_fuchsia_bluetooth_hfp as hfp;
use fuchsia_async::Task;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_sync::Mutex;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt, select};
use std::collections::HashMap;
use std::future::Future;
use std::ops::RangeFrom;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::fidl::call::{Call, LocalCallId};
use crate::fidl::peer::{LocalPeerId, Peer};

pub struct HandsFree {
    pub peers: Arc<Mutex<HashMap<LocalPeerId, Peer>>>,
    pub calls: Arc<Mutex<HashMap<LocalCallId, Call>>>,

    _proxy: hfp::HandsFreeProxy,
    task: Task<Result<()>>,
}

struct HandsFreeProxyTask {
    next_local_peer_id: RangeFrom<LocalPeerId>,
    next_local_call_id: Arc<Mutex<RangeFrom<LocalCallId>>>,

    peers: Arc<Mutex<HashMap<LocalPeerId, Peer>>>,
    peer_tasks: FuturesUnordered<Task<LocalPeerId>>,

    calls: Arc<Mutex<HashMap<LocalCallId, Call>>>,

    proxy: hfp::HandsFreeProxy,
}

impl Future for HandsFree {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.task.poll_unpin(context)
    }
}

impl HandsFree {
    pub fn new(proxy: hfp::HandsFreeProxy) -> HandsFree {
        let peers = Arc::new(Mutex::new(HashMap::new()));
        let calls = Arc::new(Mutex::new(HashMap::new()));

        let peer_tasks = FuturesUnordered::new();

        let next_local_peer_id = RangeFrom { start: 1 };
        let next_local_call_id = Arc::new(Mutex::new(RangeFrom { start: 1 }));

        let call_task = HandsFreeProxyTask {
            next_local_peer_id,
            next_local_call_id,
            peers: peers.clone(),
            peer_tasks,
            calls: calls.clone(),
            proxy: proxy.clone(),
        };
        let hands_free_fut = call_task.run();
        let task = Task::local(hands_free_fut);

        Self { peers, calls, _proxy: proxy, task }
    }
}

impl HandsFreeProxyTask {
    pub async fn run(mut self) -> Result<()> {
        let mut new_peer_stream =
            HangingGetStream::new(self.proxy.clone(), hfp::HandsFreeProxy::watch_peer_connected);

        loop {
            // If the collection is empty, `poll_next` may return `Ready(None)`.  However, we
            // don't want to exit in that case as it may have more peers in the future.
            let mut finished_peer_fut = self.peer_tasks.select_next_some();
            let mut new_peer_fut = new_peer_stream.next();

            select! {
                finished_peer = finished_peer_fut => {
                    self.handle_finished_peer(finished_peer);
                }
                new_peer = new_peer_fut => {
                    let (new_peer_id, new_peer_client_end) =
                        new_peer
                            .ok_or_else(|| format_err!("HandsFree stream closed."))?
                            .map_err(|e| format_err!("FIDL error: {e}"))?
                            .map_err(|e| format_err!("HandsFree error: {e}"))?;
                    self.handle_new_peer(new_peer_id, new_peer_client_end)?;
                }
            }
        }
    }

    fn handle_finished_peer(&mut self, local_peer_id: LocalPeerId) {
        let mut peers = self.peers.lock();
        if let Some(removed_peer) = peers.remove(&local_peer_id) {
            println!("Peer {local_peer_id} disconnected: {removed_peer:?}")
        } else {
            println!("BUG: Unknown peer {local_peer_id} removed.")
        }
    }

    fn handle_new_peer(
        &mut self,
        canonical_id: bt::PeerId,
        client_end: ClientEnd<hfp::PeerHandlerMarker>,
    ) -> Result<()> {
        let local_id = self.next_local_peer_id.next().expect("Couldn't get next local peer id.");

        let next_local_call_id = self.next_local_call_id.clone();
        let canonical_id: PeerId = canonical_id.into();
        let proxy: hfp::PeerHandlerProxy = client_end.into_proxy();

        let (peer, task) = Peer::new_peer_and_task(
            local_id,
            canonical_id,
            next_local_call_id,
            self.calls.clone(),
            proxy,
        );
        println!("New peer connected: {peer:?}");

        let no_previous_peer = self.peers.lock().insert(local_id, peer);
        // This should be impossible as we increment the peer id every time.
        assert!(no_previous_peer.is_none(), "Reused local peer ID.");

        self.peer_tasks.push(task);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fidl::peer::tests::{expect_watch_state, start_call};
    use fuchsia_async as fasync;
    use std::pin::pin;

    const PEER_ID_1: bt::PeerId = bt::PeerId { value: 1 };
    const PEER_ID_2: bt::PeerId = bt::PeerId { value: 2 };
    const TEST_NUMBER_1: &str = "+15551234567";
    const TEST_NUMBER_2: &str = "+15557654321";

    /// Answers the pending `WatchPeerConnected` request with a new peer `peer_id` and returns
    /// the server end of that peer's `PeerHandler` channel.
    fn connect_peer(
        exec: &mut fasync::TestExecutor,
        hands_free: &mut Pin<&mut HandsFree>,
        hands_free_stream: &mut hfp::HandsFreeRequestStream,
        peer_id: bt::PeerId,
    ) -> hfp::PeerHandlerRequestStream {
        let mut watch_peer_fut = hands_free_stream.next();
        let Poll::Ready(Some(Ok(hfp::HandsFreeRequest::WatchPeerConnected { responder }))) =
            exec.run_until_stalled(&mut watch_peer_fut)
        else {
            panic!("Expected WatchPeerConnected request");
        };

        let (peer_client_end, peer_stream) =
            fidl::endpoints::create_request_stream::<hfp::PeerHandlerMarker>();
        responder.send(Ok((&peer_id, peer_client_end))).expect("Send peer connected");
        assert!(exec.run_until_stalled(hands_free).is_pending());
        peer_stream
    }

    #[fuchsia::test]
    fn terminated_call_and_disconnected_peer_are_cleaned_up() {
        let mut exec = fasync::TestExecutor::new();
        let (hands_free_proxy, mut hands_free_stream) =
            fidl::endpoints::create_proxy_and_stream::<hfp::HandsFreeMarker>();
        let hands_free = HandsFree::new(hands_free_proxy);
        let peers = hands_free.peers.clone();
        let calls = hands_free.calls.clone();
        let mut hands_free = pin!(hands_free);

        // With no peers connected, polling HandsFree should stall cleanly rather than spinning.
        assert!(exec.run_until_stalled(&mut hands_free).is_pending());

        let mut peer_stream =
            connect_peer(&mut exec, &mut hands_free, &mut hands_free_stream, PEER_ID_1);
        assert_eq!(peers.lock().len(), 1);
        assert!(peers.lock().contains_key(&1));

        let mut call_stream = start_call(
            &mut exec,
            &mut hands_free,
            &mut peer_stream,
            TEST_NUMBER_1,
            hfp::CallDirection::MobileOriginated,
        );
        assert_eq!(calls.lock().len(), 1);
        assert!(calls.lock().contains_key(&1));

        let responder = expect_watch_state(&mut exec, &mut call_stream);
        responder.send(hfp::CallState::Terminated).expect("Send terminated state");

        assert!(exec.run_until_stalled(&mut hands_free).is_pending());
        assert!(calls.lock().is_empty());
        assert_eq!(peers.lock().len(), 1);

        drop(peer_stream);
        assert!(exec.run_until_stalled(&mut hands_free).is_pending());
        assert!(peers.lock().is_empty());
    }

    #[fuchsia::test]
    fn active_calls_are_cleaned_up_when_peer_disconnects() {
        let mut exec = fasync::TestExecutor::new();
        let (hands_free_proxy, mut hands_free_stream) =
            fidl::endpoints::create_proxy_and_stream::<hfp::HandsFreeMarker>();
        let hands_free = HandsFree::new(hands_free_proxy);
        let peers = hands_free.peers.clone();
        let calls = hands_free.calls.clone();
        let mut hands_free = pin!(hands_free);

        assert!(exec.run_until_stalled(&mut hands_free).is_pending());

        let mut peer_1_stream =
            connect_peer(&mut exec, &mut hands_free, &mut hands_free_stream, PEER_ID_1);
        let mut peer_2_stream =
            connect_peer(&mut exec, &mut hands_free, &mut hands_free_stream, PEER_ID_2);
        assert_eq!(peers.lock().len(), 2);

        // Start an active call on each peer.
        let _call_1_stream = start_call(
            &mut exec,
            &mut hands_free,
            &mut peer_1_stream,
            TEST_NUMBER_1,
            hfp::CallDirection::MobileOriginated,
        );
        let _call_2_stream = start_call(
            &mut exec,
            &mut hands_free,
            &mut peer_2_stream,
            TEST_NUMBER_2,
            hfp::CallDirection::MobileTerminated,
        );
        assert_eq!(calls.lock().len(), 2);

        // Disconnect peer 1 while its call stream is still open.
        drop(peer_1_stream);
        assert!(exec.run_until_stalled(&mut hands_free).is_pending());

        // Peer 1 and its call (call 1) should be removed; peer 2 and call 2 remain.
        assert_eq!(peers.lock().len(), 1);
        assert!(!peers.lock().contains_key(&1));
        assert!(peers.lock().contains_key(&2));
        assert_eq!(calls.lock().len(), 1);
        assert!(!calls.lock().contains_key(&1));
        assert!(calls.lock().contains_key(&2));

        // Disconnect peer 2 as well.
        drop(peer_2_stream);
        assert!(exec.run_until_stalled(&mut hands_free).is_pending());
        assert!(peers.lock().is_empty());
        assert!(calls.lock().is_empty());
    }
}
