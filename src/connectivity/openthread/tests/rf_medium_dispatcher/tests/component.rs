// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests that the rf-medium-dispatcher component serves `test.lowpan.rfsim.Medium` and
//! `test.lowpan.rfsim.Monitor` from one shared medium, as routed by its manifest.

use assert_matches::assert_matches;
use fidl::endpoints::create_proxy;
use fidl_test_lowpan_rfsim as rfsim;
use fuchsia_component::client::connect_to_protocol;

const NODE_ID: rfsim::NodeId = 7;

#[fuchsia::test]
async fn monitor_observes_node_attached_through_medium() {
    let medium = connect_to_protocol::<rfsim::MediumMarker>().expect("connect to Medium");
    let monitor = connect_to_protocol::<rfsim::MonitorMarker>().expect("connect to Monitor");
    let (watcher, server_end) = create_proxy::<rfsim::NodeEventWatcherMarker>();
    monitor.get_node_event_watcher(server_end).expect("Monitor.GetNodeEventWatcher");

    // No node is attached yet. The reply also proves that the watcher is installed before the
    // node attaches below.
    assert_eq!(
        watcher.watch().await.expect("NodeEventWatcher.Watch"),
        [rfsim::NodeEvent::Idle(rfsim::Idle)]
    );

    let (radio, server_end) = create_proxy::<rfsim::RadioMarker>();
    medium
        .attach(NODE_ID, server_end)
        .await
        .expect("Medium.Attach")
        .map_err(zx::Status::err_from_raw)
        .expect("attach node");
    assert_matches!(
        watcher.watch().await.expect("NodeEventWatcher.Watch").as_slice(),
        [rfsim::NodeEvent::Added(rfsim::Node { node_id: NODE_ID, timestamp: _ })]
    );

    // Closing the radio detaches the node.
    drop(radio);
    assert_matches!(
        watcher.watch().await.expect("NodeEventWatcher.Watch").as_slice(),
        [rfsim::NodeEvent::Removed(rfsim::Node { node_id: NODE_ID, timestamp: _ })]
    );
}
