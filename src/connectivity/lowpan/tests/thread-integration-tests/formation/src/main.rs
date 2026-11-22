// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_lowpan_device as flowpan_device;
use futures::StreamExt as _;
use log::info;
use std::pin::pin;
use test_case::test_case;
use thread_testing_common::{NodeKind, ThreadTestNetwork, utilities};

/// A single Thread node boots and forms a Thread network over FIDL, whether it is a Thread Border
/// Router (with a backbone interface) or a Thread-only node (without one).
#[test_case(NodeKind::BorderRouter; "border_router")]
#[test_case(NodeKind::ThreadOnly; "thread_node")]
#[fuchsia::test]
async fn forms_network(kind: NodeKind) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let network = ThreadTestNetwork::builder().add_node("node-1", kind).build(&sandbox).await;
    let node = network.node("node-1");

    info!("waiting for lowpan device registration on {:?}", node.name());
    node.wait_for_lowpan_device().await;

    // `ot-rcp-fuchsia` serves `fuchsia.lowpan.spinel.Service` only once `Medium.Attach` has
    // replied, and `lowpan-ot-driver` awaits the two-way `DeviceSetup.SetChannel` on that service
    // before registering with `lowpanservice`.
    assert_eq!(network.attached_node_ids().await, vec![node.node_id()]);

    // Capture what the node transmits from here on.
    let mut transmissions = pin!(network.watch_transmissions().await);

    info!("forming network on {:?}", node.name());
    node.form_network(&utilities::provisioning_params()).await;
    info!("waiting for {:?} to become leader", node.name());
    node.wait_for_role(flowpan_device::Role::Leader).await;

    // Forming went through the RCP: the medium saw the node transmit on the network's channel.
    info!("waiting for a transmission on channel {}", utilities::CHANNEL);
    loop {
        let transmission = transmissions.next().await.expect("transmission stream ended");
        assert_eq!(transmission.node_id, node.node_id(), "{transmission:?}");
        if transmission.frame.channel == utilities::CHANNEL {
            break;
        }
    }

    network.shutdown().await;
}
