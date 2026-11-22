// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One Thread node: a netemul realm running `lowpan-ot-driver` on top of `ot-rcp-fuchsia`, and the
//! handle tests use to control it.

use crate::medium::MediumConnector;
use crate::test_services::{self, TestServices};
use anyhow::Context as _;
use fidl::endpoints::{ClientEnd, DiscoverableProtocolMarker};
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_device as fdevice;
use fidl_fuchsia_hardware_network as fhwnet;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_lowpan as flowpan;
use fidl_fuchsia_lowpan_device as flowpan_device;
use fidl_fuchsia_netemul as fnetemul;
use fidl_fuchsia_netemul_network as fnetemul_network;
use fidl_test_lowpan_rfsim as rfsim;

/// Name of the LoWPAN interface `lowpan-ot-driver` registers (its default).
pub const LOWPAN_INTERFACE_NAME: &str = "lowpan0";

/// Name of the WLAN client interface installed in a Thread Border Router's netstack.
pub const WLAN_INTERFACE_NAME: &str = "wlan0";

/// Child names inside a node realm.
mod child {
    pub(super) const NETSTACK: &str = "netstack";
    pub(super) const MDNS: &str = "mdns";
    pub(super) const LOWPANSERVICE: &str = "lowpanservice";
    pub(super) const OT_RCP: &str = "ot-rcp";
    pub(super) const LOWPAN_OT_DRIVER: &str = "lowpan-ot-driver";
}

/// Subpackage component URLs, relative to the test package.
mod url {
    pub(super) const NETSTACK: &str = "netstack-debug#meta/netstack3-debug.cm";
    pub(super) const MDNS: &str = "mdns#meta/mdns.cm";
    pub(super) const LOWPANSERVICE: &str = "lowpanservice#meta/lowpanservice.cm";
    pub(super) const OT_RCP: &str = "ot-rcp-fuchsia#meta/ot-rcp-fuchsia.cm";
    pub(super) const LOWPAN_OT_DRIVER: &str = "lowpan-ot-driver#meta/lowpan-ot-driver.cm";
}

/// What netstack exposes to the realm. A subset of what it serves: the protocols the node
/// components and the fixture use.
const NETSTACK_PROTOCOLS: &[&str] = &[
    "fuchsia.net.interfaces.State",
    "fuchsia.net.interfaces.admin.Installer",
    "fuchsia.net.multicast.admin.Ipv6RoutingTableController",
    "fuchsia.net.routes.State",
    "fuchsia.net.routes.admin.RouteTableV4",
    "fuchsia.net.routes.admin.RouteTableV6",
    "fuchsia.posix.socket.Provider",
    "fuchsia.posix.socket.raw.Provider",
];

/// What lowpanservice exposes to the realm, for the driver and for tests.
const LOWPANSERVICE_PROTOCOLS: &[&str] = &[
    "fuchsia.factory.lowpan.FactoryLookup",
    "fuchsia.factory.lowpan.FactoryRegister",
    "fuchsia.lowpan.DeviceWatcher",
    "fuchsia.lowpan.device.CountersConnector",
    "fuchsia.lowpan.device.DeviceConnector",
    "fuchsia.lowpan.device.DeviceExtraConnector",
    "fuchsia.lowpan.device.EnergyScanConnector",
    "fuchsia.lowpan.driver.Register",
    "fuchsia.lowpan.experimental.DeviceConnector",
    "fuchsia.lowpan.experimental.DeviceExtraConnector",
    "fuchsia.lowpan.experimental.DeviceRouteConnector",
    "fuchsia.lowpan.experimental.DeviceRouteExtraConnector",
    "fuchsia.lowpan.experimental.LegacyJoiningConnector",
    "fuchsia.lowpan.experimental.TelemetryProviderConnector",
    "fuchsia.lowpan.test.DeviceTestConnector",
    "fuchsia.lowpan.thread.CapabilitiesConnector",
    "fuchsia.lowpan.thread.DatasetConnector",
    "fuchsia.lowpan.thread.EpskcConnector",
    "fuchsia.lowpan.thread.FeatureConnector",
    "fuchsia.lowpan.thread.MeshcopConnector",
];

/// The capabilities `lowpan-ot-driver`'s manifest uses that the realm provides, by provider.
///
/// The rest are left unrouted, so connecting to them fails on first use. `lowpan-ot-driver` only
/// logs a warning for each of them:
///  * fuchsia.buildinfo.Provider, fuchsia.hwinfo.Product: default vendor/version are used.
///  * fuchsia.location.namedplace.RegulatoryRegionWatcher: the region is not updated.
///  * fuchsia.feedback.CrashReporter: only used when the DHCPv6-PD stream ends.
///  * fuchsia.net.name.DnsServerWatcher, fuchsia.net.name.Lookup: no upstream DNS.
///  * config-data: `lowpan-ot-driver` uses its default config.
///
/// If a missing capability ever makes the driver fail, serve a fake from `test-services` instead.
const DRIVER_PROTOCOLS_FROM_NETSTACK: &[&str] = &[
    "fuchsia.net.interfaces.State",
    "fuchsia.net.interfaces.admin.Installer",
    "fuchsia.net.multicast.admin.Ipv6RoutingTableController",
    "fuchsia.net.routes.admin.RouteTableV4",
    "fuchsia.net.routes.admin.RouteTableV6",
    "fuchsia.posix.socket.Provider",
    "fuchsia.posix.socket.raw.Provider",
];
const DRIVER_PROTOCOLS_FROM_LOWPANSERVICE: &[&str] = &[
    "fuchsia.factory.lowpan.FactoryRegister",
    "fuchsia.lowpan.driver.Register",
    "fuchsia.lowpan.experimental.DeviceRouteConnector",
    "fuchsia.lowpan.experimental.DeviceRouteExtraConnector",
];
const DRIVER_PROTOCOLS_FROM_MDNS: &[&str] = &[
    "fuchsia.net.mdns.HostNameResolver",
    "fuchsia.net.mdns.HostNameSubscriber",
    "fuchsia.net.mdns.ProxyHostPublisher",
    "fuchsia.net.mdns.Publisher",
    "fuchsia.net.mdns.Resolver",
    "fuchsia.net.mdns.ServiceInstancePublisher",
    "fuchsia.net.mdns.ServiceSubscriber2",
];
const DRIVER_PROTOCOLS_FROM_TEST_SERVICES: &[&str] =
    &["fuchsia.net.dhcpv6.PrefixProvider", "fuchsia.net.masquerade.Factory"];

/// The service `ot-rcp-fuchsia` exposes and `lowpan-ot-driver` uses.
const SPINEL_SERVICE: &str = "fuchsia.lowpan.spinel.Service";

/// `ot-rcp-fuchsia`'s node id config capability.
const OT_RCP_NODE_ID_CONFIG: &str = "fuchsia.openthread.rcp.NodeId";

/// The kind of a node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    /// Thread Border Router: its netstack has a WLAN client interface on the network's LAN, which
    /// `lowpan-ot-driver` picks as the backbone interface.
    BorderRouter,
    /// Thread-only node: no interface besides the LoWPAN one, so no backbone.
    ThreadOnly,
}

fn protocol_dep(child: &str, protocol: &str) -> fnetemul::Capability {
    fnetemul::Capability::ChildDep(fnetemul::ChildDep {
        name: Some(child.to_string()),
        capability: Some(fnetemul::ExposedCapability::Protocol(protocol.to_string())),
        ..Default::default()
    })
}

fn protocol_deps<'a>(
    child: &'a str,
    protocols: &'a [&'a str],
) -> impl Iterator<Item = fnetemul::Capability> + 'a {
    protocols.iter().map(move |protocol| protocol_dep(child, protocol))
}

fn log_sink() -> fnetemul::Capability {
    fnetemul::Capability::LogSink(fnetemul::Empty {})
}

fn data_storage() -> fnetemul::Capability {
    fnetemul::Capability::StorageDep(fnetemul::StorageDep {
        variant: Some(fnetemul::StorageVariant::Data),
        path: Some("/data".to_string()),
        ..Default::default()
    })
}

fn to_strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// The children of a node realm.
fn realm_children(
    node_id: rfsim::NodeId,
    test_services_dir: ClientEnd<fio::DirectoryMarker>,
) -> Vec<fnetemul::ChildDef> {
    vec![
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Mock(test_services_dir)),
            name: Some(test_services::COMPONENT_NAME.to_string()),
            exposes: Some(to_strings(test_services::PROTOCOLS)),
            ..Default::default()
        },
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Component(url::NETSTACK.to_string())),
            name: Some(child::NETSTACK.to_string()),
            exposes: Some(to_strings(NETSTACK_PROTOCOLS)),
            uses: Some(fnetemul::ChildUses::Capabilities(vec![
                log_sink(),
                fnetemul::Capability::TracingProvider(fnetemul::Empty),
                data_storage(),
            ])),
            ..Default::default()
        },
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Component(url::MDNS.to_string())),
            name: Some(child::MDNS.to_string()),
            uses: Some(fnetemul::ChildUses::Capabilities(
                [
                    log_sink(),
                    protocol_dep(child::NETSTACK, "fuchsia.net.interfaces.State"),
                    protocol_dep(child::NETSTACK, "fuchsia.posix.socket.Provider"),
                    protocol_dep(
                        test_services::COMPONENT_NAME,
                        fdevice::NameProviderMarker::PROTOCOL_NAME,
                    ),
                ]
                .into(),
            )),
            ..Default::default()
        },
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Component(url::LOWPANSERVICE.to_string())),
            name: Some(child::LOWPANSERVICE.to_string()),
            exposes: Some(to_strings(LOWPANSERVICE_PROTOCOLS)),
            uses: Some(fnetemul::ChildUses::Capabilities(vec![log_sink()])),
            ..Default::default()
        },
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Component(url::OT_RCP.to_string())),
            name: Some(child::OT_RCP.to_string()),
            uses: Some(fnetemul::ChildUses::Capabilities(vec![
                log_sink(),
                protocol_dep(test_services::COMPONENT_NAME, rfsim::MediumMarker::PROTOCOL_NAME),
            ])),
            config_values: Some(vec![fnetemul::ChildConfigValue {
                capability: OT_RCP_NODE_ID_CONFIG.to_string(),
                value: fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Uint32(node_id)),
            }]),
            ..Default::default()
        },
        fnetemul::ChildDef {
            source: Some(fnetemul::ChildSource::Component(url::LOWPAN_OT_DRIVER.to_string())),
            name: Some(child::LOWPAN_OT_DRIVER.to_string()),
            uses: Some(fnetemul::ChildUses::Capabilities(
                [
                    log_sink(),
                    data_storage(),
                    // The sandbox's network-tun, shared by every node.
                    fnetemul::Capability::NetemulNetworkTun(fnetemul::Empty),
                ]
                .into_iter()
                .chain(std::iter::once(fnetemul::Capability::ChildDep(fnetemul::ChildDep {
                    name: Some(child::OT_RCP.to_string()),
                    capability: Some(fnetemul::ExposedCapability::Service(
                        SPINEL_SERVICE.to_string(),
                    )),
                    ..Default::default()
                })))
                .chain(protocol_deps(child::NETSTACK, DRIVER_PROTOCOLS_FROM_NETSTACK))
                .chain(protocol_deps(child::LOWPANSERVICE, DRIVER_PROTOCOLS_FROM_LOWPANSERVICE))
                .chain(protocol_deps(child::MDNS, DRIVER_PROTOCOLS_FROM_MDNS))
                .chain(protocol_deps(
                    test_services::COMPONENT_NAME,
                    DRIVER_PROTOCOLS_FROM_TEST_SERVICES,
                ))
                .collect(),
            )),
            // Started by the fixture once the node's interfaces are in place, see `Node::new`.
            eager: Some(false),
            ..Default::default()
        },
    ]
}

/// One Thread node of a [`crate::ThreadTestNetwork`].
pub struct Node<'a> {
    name: String,
    kind: NodeKind,
    node_id: rfsim::NodeId,
    // Dropped first: the interface holds a clone of `realm`.
    wlan_interface: Option<netemul::TestInterface<'a>>,
    realm: netemul::TestRealm<'a>,
    // Dropped after `realm`, so the realm's mock child is served until the realm is shut down.
    test_services: TestServices,
}

impl<'a> Node<'a> {
    /// Creates the node's realm and starts `lowpan-ot-driver` in it.
    pub(crate) async fn new(
        sandbox: &'a netemul::TestSandbox,
        wlan: &netemul::TestNetwork<'a>,
        medium: MediumConnector,
        name: &str,
        kind: NodeKind,
        node_id: rfsim::NodeId,
    ) -> Result<Node<'a>, anyhow::Error> {
        let (test_services, test_services_dir) =
            TestServices::serve(name.to_string(), medium).context("serving test-services")?;
        let realm = sandbox
            .create_realm(name.to_string(), realm_children(node_id, test_services_dir))
            .context("creating realm")?;

        // `lowpan-ot-driver` picks the backbone interface when it starts, and restarts itself if a
        // WLAN client interface shows up later. Installing the interface first keeps startup
        // deterministic.
        let wlan_interface = match kind {
            NodeKind::BorderRouter => Some(
                realm
                    .join_network_with(
                        wlan,
                        format!("{name}-wlan"),
                        fnetemul_network::EndpointConfig {
                            port_class: fhwnet::PortClass::WlanClient,
                            ..netemul::new_endpoint_config(netemul::DEFAULT_MTU, None)
                        },
                        netemul::InterfaceConfig {
                            name: Some(WLAN_INTERFACE_NAME.into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .context("joining the WLAN network")?,
            ),
            NodeKind::ThreadOnly => None,
        };

        realm
            .start_child_component(child::LOWPAN_OT_DRIVER)
            .await
            .context("starting lowpan-ot-driver")?;

        Ok(Node { name: name.to_string(), kind, node_id, wlan_interface, realm, test_services })
    }

    /// The node's name, also the name of its realm.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the node is a Thread Border Router or a Thread-only node.
    pub fn kind(&self) -> NodeKind {
        self.kind
    }

    /// The node's id on the medium, also its RCP's EUI-64 seed.
    pub fn node_id(&self) -> rfsim::NodeId {
        self.node_id
    }

    /// The node's realm.
    pub fn realm(&self) -> &netemul::TestRealm<'a> {
        &self.realm
    }

    /// The WLAN client interface of a Thread Border Router, `None` for a Thread-only node.
    pub fn wlan_interface(&self) -> Option<&netemul::TestInterface<'a>> {
        self.wlan_interface.as_ref()
    }

    /// The capabilities the test serves into the node's realm, to observe what the node asked of
    /// them.
    pub fn test_services(&self) -> &TestServices {
        &self.test_services
    }

    /// Connects to a protocol lowpanservice exposes.
    #[track_caller]
    pub fn connect_to_lowpanservice<P: DiscoverableProtocolMarker>(&self) -> P::Proxy {
        self.realm.connect_to_protocol_from_child::<P>(child::LOWPANSERVICE).unwrap_or_else(|e| {
            panic!("connecting to {} on node {:?}: {e:?}", P::PROTOCOL_NAME, self.name)
        })
    }

    /// Waits until `lowpan-ot-driver` has registered the node's LoWPAN interface.
    ///
    /// Only waits for the registration: if `lowpan-ot-driver` restarts afterwards, the interface
    /// goes away and comes back, and connections made in between fail.
    pub async fn wait_for_lowpan_device(&self) {
        let watcher = self.connect_to_lowpanservice::<flowpan::DeviceWatcherMarker>();
        loop {
            let (added, _removed) = watcher
                .watch_devices()
                .await
                .unwrap_or_else(|e| panic!("DeviceWatcher.WatchDevices on {:?}: {e:?}", self.name));
            if added.iter().any(|name| name == LOWPAN_INTERFACE_NAME) {
                return;
            }
        }
    }

    /// Stops the node's `lowpan-ot-driver`.
    pub(crate) async fn stop_driver(&self) -> Result<(), anyhow::Error> {
        self.realm
            .stop_child_component(child::LOWPAN_OT_DRIVER)
            .await
            .context("stopping lowpan-ot-driver")
    }

    /// Connects to `fuchsia.lowpan.device.Device` on the node's LoWPAN interface.
    #[track_caller]
    pub fn connect_lowpan_device(&self) -> flowpan_device::DeviceProxy {
        let connector = self.connect_to_lowpanservice::<flowpan_device::DeviceConnectorMarker>();
        let (proxy, server) = fidl::endpoints::create_proxy::<flowpan_device::DeviceMarker>();
        connector
            .connect(LOWPAN_INTERFACE_NAME, server)
            .unwrap_or_else(|e| panic!("DeviceConnector.Connect on {:?}: {e:?}", self.name));
        proxy
    }

    /// Provisions the node for a new Thread network described by `params` and makes it active.
    /// The node becomes leader of that network once it finds no other node of it.
    pub async fn form_network(&self, params: &flowpan_device::ProvisioningParams) {
        let device = self.connect_lowpan_device();
        device
            .provision_network(params)
            .await
            .unwrap_or_else(|e| panic!("Device.ProvisionNetwork on {:?}: {e:?}", self.name));
        device
            .set_active(true)
            .await
            .unwrap_or_else(|e| panic!("Device.SetActive on {:?}: {e:?}", self.name));
    }

    /// Waits until the node's role on its Thread network is `role`.
    pub async fn wait_for_role(&self, role: flowpan_device::Role) {
        let device = self.connect_lowpan_device();
        loop {
            let flowpan_device::DeviceState { role: current, .. } = device
                .watch_device_state()
                .await
                .unwrap_or_else(|e| panic!("Device.WatchDeviceState on {:?}: {e:?}", self.name));
            if current == Some(role) {
                return;
            }
        }
    }
}

impl std::fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { name, kind, node_id, wlan_interface: _, realm: _, test_services: _ } = self;
        f.debug_struct("Node")
            .field("name", name)
            .field("kind", kind)
            .field("node_id", node_id)
            .finish_non_exhaustive()
    }
}
