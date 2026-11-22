// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A hermetic Thread network for integration tests.
//!
//! [`ThreadTestNetwork`] runs any number of Thread nodes, each `lowpan-ot-driver` on top of
//! `ot-rcp-fuchsia` in its own netemul realm (with its own netstack, mdns and lowpanservice):
//!
//! * Thread Border Routers ([`NodeKind::BorderRouter`]) also have a WLAN client interface. All of
//!   them are on one netemul network, standing for the home WLAN.
//! * Thread-only nodes ([`NodeKind::ThreadOnly`]) have no interface besides the LoWPAN one.
//!
//! All the Thread nodes of a network join the same `rf-medium-dispatcher` (the simulated IEEE
//! 802.15.4 medium, in a netemul realm of its own), which determines which nodes can hear each
//! other. Each [`ThreadTestNetwork`] spawns its own medium.
//!
//! Must be used on a local (single-threaded) executor, which is what `#[fuchsia::test]` provides by
//! default. Packages of tests using the fixture must list `thread_test_subpackages` from
//! `thread_test_package.gni` as their `subpackages`; `expect_includes` automatically adds
//! `client.shard.cml` to generated test manifests (`fuchsia_unittest_package`).
//! Tests end with [`ThreadTestNetwork::shutdown`]; dropping the network without it panics.

mod medium;
mod node;
mod test_services;
pub mod utilities;

pub use node::{LOWPAN_INTERFACE_NAME, Node, NodeKind, WLAN_INTERFACE_NAME};
pub use test_services::{MasqueradeInstance, TestServices};

use fidl_test_lowpan_rfsim as rfsim;
use futures::{Stream, StreamExt as _, stream};
use medium::MediumRealm;

/// Name of the netemul network the Thread Border Routers' WLAN client interfaces are on.
///
/// A sandbox holds at most one [`ThreadTestNetwork`], since network names are unique in a sandbox.
pub const WLAN_NETWORK_NAME: &str = "wlan";

/// Describes a [`ThreadTestNetwork`] to build. Derive the `node_id`s from their indexes.
#[derive(Debug, Default)]
pub struct ThreadTestNetworkBuilder {
    nodes: Vec<(String, NodeKind)>,
}

impl ThreadTestNetworkBuilder {
    /// Adds a Thread Border Router named `name`.
    pub fn add_border_router_node(self, name: impl Into<String>) -> Self {
        self.add_node(name, NodeKind::BorderRouter)
    }

    /// Adds a Thread-only node named `name`.
    pub fn add_thread_node(self, name: impl Into<String>) -> Self {
        self.add_node(name, NodeKind::ThreadOnly)
    }

    /// Adds a node of `kind` named `name`. Names must be unique.
    pub fn add_node(mut self, name: impl Into<String>, kind: NodeKind) -> Self {
        self.nodes.push((name.into(), kind));
        self
    }

    /// Spawns the network's `rf-medium-dispatcher`, creates the network and its nodes in `sandbox`,
    /// and starts every node's `lowpan-ot-driver`.
    ///
    /// Panics if `sandbox` already has a `ThreadTestNetwork`, on duplicate node names, or with more
    /// than `test.lowpan.rfsim/MAX_NODES` nodes. Does not wait for the drivers to be ready: see
    /// [`Node::wait_for_lowpan_device`].
    pub async fn build<'a>(self, sandbox: &'a netemul::TestSandbox) -> ThreadTestNetwork<'a> {
        let Self { nodes: specs } = self;
        let max_nodes = usize::try_from(rfsim::MAX_NODES).expect("MAX_NODES fits in usize");
        assert!(
            specs.len() <= max_nodes,
            "{} nodes, but the medium holds at most {max_nodes}",
            specs.len()
        );
        let medium = MediumRealm::new().expect("spawning rf-medium-dispatcher");
        let wlan =
            sandbox.create_network(WLAN_NETWORK_NAME).await.expect("creating the WLAN network");
        let mut nodes: Vec<Node<'a>> = Vec::with_capacity(specs.len());
        for (index, (name, kind)) in specs.into_iter().enumerate() {
            assert!(!nodes.iter().any(|node| node.name() == name), "duplicate node name {name:?}");
            // Ids start at 1, unique on the medium.
            let node_id = rfsim::NodeId::try_from(index + 1)
                .expect("node count is checked against MAX_NODES");
            let node = Node::new(sandbox, &wlan, medium.connector(), &name, kind, node_id)
                .await
                .unwrap_or_else(|e| panic!("creating node {name:?}: {e:?}"));
            nodes.push(node);
        }
        ThreadTestNetwork { nodes, wlan, medium, shutdown_called: false }
    }
}

/// A running hermetic Thread network. See the crate documentation.
///
/// Must be ended with [`ThreadTestNetwork::shutdown`]; dropping it otherwise panics.
pub struct ThreadTestNetwork<'a> {
    // Dropped first: shutting down the node realms while the medium they use is still running.
    nodes: Vec<Node<'a>>,
    wlan: netemul::TestNetwork<'a>,
    medium: MediumRealm,
    // Set by `shutdown`, checked on drop.
    shutdown_called: bool,
}

impl<'a> ThreadTestNetwork<'a> {
    /// Starts describing a network.
    pub fn builder() -> ThreadTestNetworkBuilder {
        ThreadTestNetworkBuilder::default()
    }

    /// Available nodes, in the order they were added.
    pub fn nodes(&self) -> &[Node<'a>] {
        &self.nodes
    }

    /// The node named `name`.
    #[track_caller]
    pub fn node(&self, name: &str) -> &Node<'a> {
        self.nodes
            .iter()
            .find(|node| node.name() == name)
            .unwrap_or_else(|| panic!("no node named {name:?}"))
    }

    /// The netemul network the Thread Border Routers' WLAN client interfaces are on.
    pub fn wlan_network(&self) -> &netemul::TestNetwork<'a> {
        &self.wlan
    }

    /// Stops every node's `lowpan-ot-driver`, then tears the network down.
    ///
    /// Tests call this at the end; dropping the network without it panics. A plain drop would drop
    /// the Thread Border Routers' `netemul::TestInterface`s before their realms shut down (each
    /// `TestInterface` holds a clone of its `TestRealm`), removing the backbone interface while
    /// `lowpan-ot-driver` is still running. `lowpan-ot-driver` treats backbone interface removal as
    /// an unexpected shutdown and logs at `ERROR`, which fails the test.
    pub async fn shutdown(mut self) {
        for node in &self.nodes {
            node.stop_driver()
                .await
                .unwrap_or_else(|e| panic!("stopping node {:?}: {e:?}", node.name()));
        }
        self.shutdown_called = true;
    }

    /// Connects to the medium's `test.lowpan.rfsim.Monitor`.
    #[track_caller]
    pub fn monitor(&self) -> rfsim::MonitorProxy {
        self.medium.monitor().expect("connecting to test.lowpan.rfsim.Monitor")
    }

    /// Installs a `test.lowpan.rfsim.TransmissionWatcher` on the medium and returns the stream of
    /// transmissions captured from that point on.
    ///
    /// Panics if the medium drops any transmission because the stream was not polled fast enough.
    pub async fn watch_transmissions(&self) -> impl Stream<Item = rfsim::Transmission> + use<> {
        let monitor = self.monitor();
        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<rfsim::TransmissionWatcherMarker>();
        monitor.get_transmission_watcher(server_end).await.expect("Monitor.GetTransmissionWatcher");
        stream::unfold(watcher, |watcher| async move {
            let (transmissions, dropped) =
                watcher.watch().await.expect("TransmissionWatcher.Watch");
            assert_eq!(dropped, 0, "TransmissionWatcher dropped {dropped} transmissions");
            Some((stream::iter(transmissions), watcher))
        })
        .flatten()
    }

    /// Ids of the nodes attached to the medium right now, in attachment order.
    pub async fn attached_node_ids(&self) -> Vec<rfsim::NodeId> {
        let monitor = self.monitor();
        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<rfsim::NodeEventWatcherMarker>();
        monitor.get_node_event_watcher(server_end).expect("Monitor.GetNodeEventWatcher");
        // The watcher reports the attached nodes as `existing`, then `idle`.
        let mut node_ids = Vec::new();
        loop {
            for event in watcher.watch().await.expect("NodeEventWatcher.Watch") {
                match event {
                    rfsim::NodeEvent::Existing(rfsim::Node { node_id, .. }) => {
                        node_ids.push(node_id)
                    }
                    rfsim::NodeEvent::Idle(_) => return node_ids,
                    event @ (rfsim::NodeEvent::Added(_) | rfsim::NodeEvent::Removed(_)) => {
                        panic!("unexpected event before idle: {event:?}");
                    }
                }
            }
        }
    }
}

impl Drop for ThreadTestNetwork<'_> {
    fn drop(&mut self) {
        assert!(self.shutdown_called, "ThreadTestNetwork dropped without calling shutdown()");
    }
}

impl std::fmt::Debug for ThreadTestNetwork<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { nodes, wlan, medium: _, shutdown_called: _ } = self;
        f.debug_struct("ThreadTestNetwork")
            .field("nodes", nodes)
            .field("wlan", wlan)
            .finish_non_exhaustive()
    }
}
