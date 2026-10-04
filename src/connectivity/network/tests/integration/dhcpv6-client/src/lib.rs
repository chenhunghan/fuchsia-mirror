// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use assert_matches::assert_matches;
use fidl::endpoints;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_dhcpv6 as fnet_dhcpv6;
use fidl_fuchsia_net_dhcpv6_ext::{
    AddressConfig, ClientConfig, InformationConfig, NewClientParams,
};
use fidl_fuchsia_net_interfaces as _;
use fidl_fuchsia_posix_socket as fposix_socket;
use fuchsia_async::{self as fasync, TimeoutExt as _};
use futures::FutureExt as _;
use net_declare::{fidl_ip_v6, fidl_ip_v6_with_prefix, fidl_mac, fidl_subnet, std_ip_v6};
use netstack_testing_common::realms::{KnownServiceProvider, Netstack3, TestSandboxExt as _};
use netstack_testing_common::{
    ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT, ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT,
};
use test_case::{test_case, test_matrix};

#[fuchsia::test]
async fn dhcpv6_client_bind_to_other_interface_address() {
    let sandbox = netemul::TestSandbox::new().unwrap();
    let network = sandbox.create_network("net").await.unwrap();

    let realm = sandbox
        .create_netstack_realm_with::<Netstack3, _, _>(
            "dhcpv6_client_bind_to_other_interface_address",
            &[KnownServiceProvider::Dhcpv6Client],
        )
        .unwrap();

    let iface_a = realm.join_network(&network, "iface_a").await.unwrap();
    let iface_b = realm.join_network(&network, "iface_b").await.unwrap();

    let iface_b_addr = fidl_ip_v6!("2001:db8::2");
    iface_b
        .add_address_and_subnet_route(fnet::Subnet {
            addr: fnet::IpAddress::Ipv6(iface_b_addr),
            prefix_len: 64,
        })
        .await
        .expect("add address to iface_b");

    let client_provider = realm
        .connect_to_protocol::<fnet_dhcpv6::ClientProviderMarker>()
        .expect("connect to ClientProvider should succeed");

    let (client_proxy, server_end) = endpoints::create_proxy::<fnet_dhcpv6::ClientMarker>();

    // Create a DHCPv6 client for `iface_a` that is bound to an address on
    // `iface_b` and expect that the client fails to start.

    let params = fnet_dhcpv6::NewClientParams {
        interface_id: Some(iface_a.id()),
        address: Some(fnet::Ipv6SocketAddress {
            address: iface_b_addr,
            port: fnet_dhcpv6::DEFAULT_CLIENT_PORT,
            zone_index: 0,
        }),
        config: Some(fnet_dhcpv6::ClientConfig {
            information_config: Some(fnet_dhcpv6::InformationConfig {
                dns_servers: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    client_provider.new_client(&params, server_end).expect("new_client call should succeed");

    assert_matches!(
        client_proxy
            .watch_address()
            .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || panic!(
                "expected peer to close the channel"
            ))
            .await,
        Err(fidl::Error::ClientChannelClosed { epitaph: fidl::Epitaph::PeerClosed, .. })
    );
}

const IFACE_NAME: &str = "iface";
const CLIENT_SUBNET: fnet::Subnet = fidl_subnet!("2001:db8::1/64");
const CLIENT_ADDR: fnet::Ipv6Address = fidl_ip_v6!("2001:db8::1");

const STATELESS_CLIENT_CONFIG: ClientConfig = ClientConfig {
    information_config: InformationConfig { dns_servers: true },
    non_temporary_address_config: AddressConfig { address_count: 0, preferred_addresses: None },
    prefix_delegation_config: None,
};

const TEST_DUID: fnet_dhcpv6::Duid = fnet_dhcpv6::Duid::LinkLayerAddress(
    fnet_dhcpv6::LinkLayerAddress::Ethernet(fidl_mac!("00:11:22:33:44:55")),
);

/// Creates a Netstack3 realm with a DHCPv6 client provider and an interface
/// named `IFACE_NAME` that has `CLIENT_SUBNET` assigned.
async fn setup<'a>(
    sandbox: &'a netemul::TestSandbox,
    name: &'a str,
) -> (
    netemul::TestNetwork<'a>,
    netemul::TestRealm<'a>,
    netemul::TestInterface<'a>,
    fnet_dhcpv6::ClientProviderProxy,
) {
    let network = sandbox.create_network("net").await.expect("create network");
    let realm = sandbox
        .create_netstack_realm_with::<Netstack3, _, _>(name, &[KnownServiceProvider::Dhcpv6Client])
        .expect("create realm");
    let iface = realm
        .join_network_with_if_config(
            &network,
            IFACE_NAME,
            netemul::InterfaceConfig { name: Some(IFACE_NAME.into()), ..Default::default() },
        )
        .await
        .expect("join network");
    iface.add_address_and_subnet_route(CLIENT_SUBNET).await.expect("add address");
    let client_provider = realm
        .connect_to_protocol::<fnet_dhcpv6::ClientProviderMarker>()
        .expect("connect to ClientProvider");
    (network, realm, iface, client_provider)
}

/// The FIDL socket address that clients under test are bound to.
fn client_socket_addr() -> fnet::Ipv6SocketAddress {
    fnet::Ipv6SocketAddress {
        address: CLIENT_ADDR,
        port: fnet_dhcpv6::DEFAULT_CLIENT_PORT,
        zone_index: 0,
    }
}

/// The std socket address that clients under test are bound to.
fn client_std_socket_addr() -> SocketAddr {
    SocketAddr::new(
        IpAddr::V6(std::net::Ipv6Addr::from(CLIENT_ADDR.addr)),
        fnet_dhcpv6::DEFAULT_CLIENT_PORT,
    )
}

fn new_client(
    client_provider: &fnet_dhcpv6::ClientProviderProxy,
    params: NewClientParams,
) -> fnet_dhcpv6::ClientProxy {
    let (client_proxy, server_end) = endpoints::create_proxy::<fnet_dhcpv6::ClientMarker>();
    client_provider.new_client(&params.into(), server_end).expect("new_client call should succeed");
    client_proxy
}

/// Attempts to bind a UDP socket in `realm` to the same address and interface
/// as the client under test, which conflicts with the client's socket.
async fn bind_conflicting_socket(
    realm: &netemul::TestRealm<'_>,
) -> std::io::Result<socket2::Socket> {
    let socket = realm
        .datagram_socket(fposix_socket::Domain::Ipv6, fposix_socket::DatagramSocketProtocol::Udp)
        .await
        .expect("create conflicting socket");
    socket.bind_device(Some(IFACE_NAME.as_bytes())).expect("bind conflicting socket to device");
    socket.bind(&client_std_socket_addr().into()).map(|()| socket)
}

#[fuchsia::test]
async fn dhcpv6_client_stops_on_channel_close() {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let (_network, realm, iface, client_provider) =
        setup(&sandbox, "dhcpv6_client_stops_on_channel_close").await;

    // Listen for the client's Information-Request, which tells us that the
    // client has started and bound its socket.
    let server_socket = realm
        .datagram_socket(fposix_socket::Domain::Ipv6, fposix_socket::DatagramSocketProtocol::Udp)
        .await
        .expect("create server socket");
    server_socket
        .bind(
            &SocketAddr::new(std_ip_v6!("::").into(), fnet_dhcpv6::RELAY_AGENT_AND_SERVER_PORT)
                .into(),
        )
        .expect("bind server socket");
    server_socket
        .join_multicast_v6(&std_ip_v6!("ff02::1:2"), iface.id().try_into().unwrap())
        .expect("join DHCPv6 servers multicast group");
    let server_socket = fasync::net::UdpSocket::from_socket(server_socket.into())
        .expect("create async server socket");

    let client_proxy = new_client(
        &client_provider,
        NewClientParams {
            interface_id: iface.id(),
            address: client_socket_addr(),
            config: STATELESS_CLIENT_CONFIG,
            duid: None,
        },
    );

    let client_addr = client_std_socket_addr();
    let mut buf = [0u8; 1500];
    let (_, from): (usize, SocketAddr) = server_socket
        .recv_from(&mut buf)
        .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || {
            panic!("timed out waiting for message from DHCPv6 client")
        })
        .await
        .expect("receive message from DHCPv6 client");
    assert_eq!((from.ip(), from.port()), (client_addr.ip(), client_addr.port()));

    // The client holds the address, so binding to it must fail.
    assert_matches!(
        bind_conflicting_socket(&realm).await,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
    );

    // Closing the channel stops the client, which releases its socket.
    drop(client_proxy);
    let _: socket2::Socket = async {
        loop {
            match bind_conflicting_socket(&realm).await {
                Ok(socket) => break socket,
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
                    fasync::Timer::new(Duration::from_millis(100)).await;
                }
            }
        }
    }
    .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || {
        panic!("timed out waiting for DHCPv6 client to release its socket")
    })
    .await;
}

#[derive(Clone, Copy, Debug)]
enum Watch {
    Servers,
    Address,
    Prefixes,
}

async fn watch(client_proxy: &fnet_dhcpv6::ClientProxy, watch: Watch) -> Result<(), fidl::Error> {
    match watch {
        Watch::Servers => client_proxy.watch_servers().await.map(|_| ()),
        Watch::Address => client_proxy.watch_address().await.map(|_| ()),
        Watch::Prefixes => client_proxy.watch_prefixes().await.map(|_| ()),
    }
}

#[test_case(Watch::Servers; "watch_servers")]
#[test_case(Watch::Address; "watch_address")]
#[test_case(Watch::Prefixes; "watch_prefixes")]
#[fuchsia::test]
async fn dhcpv6_client_closes_channel_on_double_watch(w: Watch) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let (_network, _realm, iface, client_provider) =
        setup(&sandbox, "dhcpv6_client_closes_channel_on_double_watch").await;

    let client_proxy = new_client(
        &client_provider,
        NewClientParams {
            interface_id: iface.id(),
            address: client_socket_addr(),
            config: STATELESS_CLIENT_CONFIG,
            duid: None,
        },
    );

    let (res1, res2) = futures::future::join(watch(&client_proxy, w), watch(&client_proxy, w))
        .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || {
            panic!("expected peer to close the channel")
        })
        .await;
    assert_matches!(
        res1,
        Err(fidl::Error::ClientChannelClosed { epitaph: fidl::Epitaph::PeerClosed, .. })
    );
    assert_matches!(
        res2,
        Err(fidl::Error::ClientChannelClosed { epitaph: fidl::Epitaph::PeerClosed, .. })
    );
}

#[derive(Debug)]
enum AddressConfigCase {
    NoAddresses,
    OneAddress,
    OneAddressEmptyPreferred,
    OneAddressOnePreferred,
    TwoAddressesOnePreferred,
}

impl From<AddressConfigCase> for AddressConfig {
    fn from(case: AddressConfigCase) -> Self {
        match case {
            AddressConfigCase::NoAddresses => AddressConfig::default(),
            AddressConfigCase::OneAddress => {
                AddressConfig { address_count: 1, preferred_addresses: None }
            }
            AddressConfigCase::OneAddressEmptyPreferred => {
                AddressConfig { address_count: 1, preferred_addresses: Some(Vec::new()) }
            }
            AddressConfigCase::OneAddressOnePreferred => AddressConfig {
                address_count: 1,
                preferred_addresses: Some(vec![fidl_ip_v6!("a::1")]),
            },
            AddressConfigCase::TwoAddressesOnePreferred => AddressConfig {
                address_count: 2,
                preferred_addresses: Some(vec![fidl_ip_v6!("a::2")]),
            },
        }
    }
}

#[derive(Debug)]
enum PrefixDelegationConfigCase {
    Empty,
    PrefixLength1,
    PrefixLength127,
    Prefix,
}

impl From<PrefixDelegationConfigCase> for fnet_dhcpv6::PrefixDelegationConfig {
    fn from(case: PrefixDelegationConfigCase) -> Self {
        match case {
            PrefixDelegationConfigCase::Empty => {
                fnet_dhcpv6::PrefixDelegationConfig::Empty(fnet_dhcpv6::Empty {})
            }
            PrefixDelegationConfigCase::PrefixLength1 => {
                fnet_dhcpv6::PrefixDelegationConfig::PrefixLength(1)
            }
            PrefixDelegationConfigCase::PrefixLength127 => {
                fnet_dhcpv6::PrefixDelegationConfig::PrefixLength(127)
            }
            PrefixDelegationConfigCase::Prefix => {
                fnet_dhcpv6::PrefixDelegationConfig::Prefix(fidl_ip_v6_with_prefix!("a::/64"))
            }
        }
    }
}

#[derive(Debug)]
enum InformationConfigCase {
    NoDnsServers,
    DnsServers,
}

impl From<InformationConfigCase> for InformationConfig {
    fn from(case: InformationConfigCase) -> Self {
        match case {
            InformationConfigCase::NoDnsServers => InformationConfig { dns_servers: false },
            InformationConfigCase::DnsServers => InformationConfig { dns_servers: true },
        }
    }
}

#[test_matrix(
    [InformationConfigCase::NoDnsServers, InformationConfigCase::DnsServers],
    [
        AddressConfigCase::NoAddresses,
        AddressConfigCase::OneAddress,
        AddressConfigCase::OneAddressEmptyPreferred,
        AddressConfigCase::OneAddressOnePreferred,
        AddressConfigCase::TwoAddressesOnePreferred
    ],
    [
        PrefixDelegationConfigCase::Empty,
        PrefixDelegationConfigCase::PrefixLength1,
        PrefixDelegationConfigCase::PrefixLength127,
        PrefixDelegationConfigCase::Prefix
    ]
)]
#[fuchsia::test]
async fn dhcpv6_client_starts_with_valid_args(
    information_config: InformationConfigCase,
    address_config: AddressConfigCase,
    prefix_delegation_config: PrefixDelegationConfigCase,
) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let (_network, _realm, iface, client_provider) =
        setup(&sandbox, "dhcpv6_client_starts_with_valid_args").await;

    let config = ClientConfig {
        information_config: information_config.into(),
        non_temporary_address_config: address_config.into(),
        prefix_delegation_config: Some(prefix_delegation_config.into()),
    };
    // A DUID is required when running in stateful mode.
    let duid = (config.non_temporary_address_config.address_count != 0
        || config.prefix_delegation_config.is_some())
    .then_some(TEST_DUID);
    let client_proxy = new_client(
        &client_provider,
        NewClientParams { interface_id: iface.id(), address: client_socket_addr(), config, duid },
    );

    // A client that failed to start would close its channel.
    assert_matches!(
        client_proxy
            .watch_servers()
            .map(Some)
            .on_timeout(ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT, || None)
            .await,
        None
    );
}

#[derive(Debug)]
enum InvalidArgs {
    LinkLocalZoneMismatch,
    MulticastAddress,
    StatelessWithDuid,
    StatefulWithoutDuid,
}

impl InvalidArgs {
    /// Returns client parameters for the interface `iface_id` that are invalid
    /// in the manner described by `self`.
    fn into_client_params(self, iface_id: u64) -> NewClientParams {
        match self {
            InvalidArgs::LinkLocalZoneMismatch => NewClientParams {
                interface_id: iface_id,
                address: fnet::Ipv6SocketAddress {
                    // The zone index is only validated for link-local addresses.
                    address: fidl_ip_v6!("fe80::1"),
                    port: fnet_dhcpv6::DEFAULT_CLIENT_PORT,
                    zone_index: iface_id + 1,
                },
                config: STATELESS_CLIENT_CONFIG,
                duid: None,
            },
            InvalidArgs::MulticastAddress => NewClientParams {
                interface_id: iface_id,
                address: fnet::Ipv6SocketAddress {
                    address: fidl_ip_v6!("ff01::1"),
                    port: fnet_dhcpv6::DEFAULT_CLIENT_PORT,
                    zone_index: iface_id,
                },
                config: STATELESS_CLIENT_CONFIG,
                duid: None,
            },
            InvalidArgs::StatelessWithDuid => NewClientParams {
                interface_id: iface_id,
                address: client_socket_addr(),
                config: STATELESS_CLIENT_CONFIG,
                duid: Some(TEST_DUID),
            },
            InvalidArgs::StatefulWithoutDuid => NewClientParams {
                interface_id: iface_id,
                address: client_socket_addr(),
                config: ClientConfig {
                    information_config: InformationConfig { dns_servers: true },
                    non_temporary_address_config: AddressConfig {
                        address_count: 1,
                        preferred_addresses: None,
                    },
                    prefix_delegation_config: None,
                },
                duid: None,
            },
        }
    }
}

#[test_case(InvalidArgs::LinkLocalZoneMismatch; "link_local_zone_mismatch")]
#[test_case(InvalidArgs::MulticastAddress; "multicast_address")]
#[test_case(InvalidArgs::StatelessWithDuid; "stateless_with_duid")]
#[test_case(InvalidArgs::StatefulWithoutDuid; "stateful_without_duid")]
#[fuchsia::test]
async fn dhcpv6_client_fails_to_start_with_invalid_args(invalid_args: InvalidArgs) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let (_network, _realm, iface, client_provider) =
        setup(&sandbox, "dhcpv6_client_fails_to_start_with_invalid_args").await;

    let params = invalid_args.into_client_params(iface.id());

    let client_proxy = new_client(&client_provider, params);
    assert_matches!(
        client_proxy
            .watch_servers()
            .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || panic!(
                "expected peer to close the channel"
            ))
            .await,
        Err(fidl::Error::ClientChannelClosed { epitaph, .. })
            if epitaph == zx::Status::INVALID_ARGS
    );
}
