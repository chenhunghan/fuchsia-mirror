// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FuchsiaTime;
use crate::tcp::DnsTcpStream;
use crate::udp::DnsUdpSocket;
use fuchsia_async as fasync;
use futures::{Future, FutureExt};
use hickory_net::NetError;
use hickory_net::runtime::{RuntimeProvider as HickoryRuntimeProvider, Spawn as HickorySpawn};
use hickory_net::tcp::TcpClientStream;
use hickory_net::udp::UdpClientStream;
use hickory_net::xfer::DnsExchange;
use hickory_resolver::config::{ConnectionConfig, ProtocolConfig};
use hickory_resolver::{PoolContext, Resolver as HickoryResolver};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::Pin;
use trust_dns_proto::error::ProtoError;
use trust_dns_resolver::AsyncResolver;
use trust_dns_resolver::name_server::{
    GenericConnection as Connection, GenericConnectionProvider, RuntimeProvider, Spawn,
};

/// Implement the `Spawn` trait in terms of `fasync::Task`.
#[derive(Clone)]
pub struct Spawner;

impl Spawn for Spawner {
    fn spawn_bg<F>(&mut self, future: F)
    where
        F: Future<Output = Result<(), ProtoError>> + Send + 'static,
    {
        fasync::Task::spawn(future.map(|_| ())).detach()
    }
}

impl HickorySpawn for Spawner {
    fn spawn_bg(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        fasync::Task::spawn(future).detach()
    }
}

/// A Fuchsia Runtime type which uses `Handle`, `DnsTcpStream`, `FuchsiaTime`, and `DnsUdpSocket`
/// defined in this crate.
#[derive(Clone)]
pub struct FuchsiaRuntime;

impl RuntimeProvider for FuchsiaRuntime {
    type Handle = Spawner;
    type Tcp = DnsTcpStream;
    type Timer = FuchsiaTime;
    type Udp = DnsUdpSocket;
}

impl HickoryRuntimeProvider for FuchsiaRuntime {
    type Handle = Spawner;
    type Tcp = DnsTcpStream;
    type Timer = FuchsiaTime;
    type Udp = DnsUdpSocket;

    fn create_handle(&self) -> Self::Handle {
        Spawner
    }

    fn connect_tcp(
        &self,
        server_addr: std::net::SocketAddr,
        bind_addr: Option<std::net::SocketAddr>,
        timeout: Option<std::time::Duration>,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Self::Tcp>> + Send + 'static>> {
        if let Some(bind_addr) = bind_addr {
            unimplemented!(
                "https://fxbug.dev/42180092: cannot bind to {:?}; `connect_tcp` with `bind_addr` \
                is unimplemented",
                bind_addr,
            );
        }
        Box::pin(async move {
            let connect_fut = DnsTcpStream::connect(server_addr);
            match timeout {
                Some(timeout) => {
                    <FuchsiaTime as hickory_net::runtime::Time>::timeout(timeout, connect_fut)
                        .await?
                }
                None => connect_fut.await,
            }
        })
    }

    fn bind_udp(
        &self,
        local_addr: std::net::SocketAddr,
        _server_addr: std::net::SocketAddr,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Self::Udp>> + Send + 'static>> {
        Box::pin(futures::future::ready(DnsUdpSocket::bind(local_addr)))
    }
}

/// ConnectionProvider that creates new connections to DNS servers on Fuchsia.
pub type ConnectionProvider = GenericConnectionProvider<FuchsiaRuntime>;

/// Resolver that resolves DNS requests on Fuchsia.
pub type Resolver = AsyncResolver<Connection, ConnectionProvider>;

// TODO(https://fxbug.dev/584512770): Remove this workaround once upstream
// `hickory-dns` (https://github.com/hickory-dns/hickory-dns/issues/3713)
// natively preserves IPv6 `scope_id` in `NameServerConfig`. Currently
// `NameServerConfig` stores only an `IpAddr` for the server address, which
// drops the IPv6 `scope_id` needed to reach link-local IPv6 DNS servers (e.g.
// discovered via NDP RDNSS or DHCPv6).
/// Configures `conn`'s port and encodes any non-zero IPv6 `scope_id` (zone
/// index) from `server_addr` into `conn.bind_addr`.
///
/// Because Fuchsia does not configure a local `bind_addr` on DNS connections
/// (and [`FuchsiaRuntime::connect_tcp`] does not support `bind_addr`, see
/// https://fxbug.dev/42180092), `conn.bind_addr` is always `None` prior to
/// calling this function and can be safely repurposed to carry `scope_id`.
/// [`HickoryConnectionProvider`] extracts the `scope_id` from `conn.bind_addr`,
/// restores it on the remote [`SocketAddr`], and resets `bind_addr` to `None`
/// when establishing connections.
///
/// # Panics
///
/// Panics if `conn.bind_addr` is already `Some`.
pub fn set_connection_addr(conn: &mut ConnectionConfig, server_addr: SocketAddr) {
    assert!(
        conn.bind_addr.is_none(),
        "https://fxbug.dev/42180092, https://fxbug.dev/584512770: `conn.bind_addr` ({:?}) must \
        be `None` before calling `set_connection_addr`",
        conn.bind_addr,
    );
    conn.port = server_addr.port();
    if let SocketAddr::V6(v6) = server_addr {
        if v6.scope_id() != 0 {
            conn.bind_addr =
                Some(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, v6.scope_id())));
        }
    }
}

// TODO(https://fxbug.dev/584512770): Switch back to
// `GenericConnector<FuchsiaRuntime>` once upstream `hickory-dns`
// (https://github.com/hickory-dns/hickory-dns/issues/3713) natively preserves
// IPv6 `scope_id` in `NameServerConfig`.
/// [`hickory_resolver::ConnectionProvider`] that creates new connections to DNS
/// servers on Fuchsia, restoring any IPv6 `scope_id` encoded in
/// [`ConnectionConfig::bind_addr`] by [`set_connection_addr`].
#[derive(Clone, Default)]
pub struct HickoryConnectionProvider;

impl hickory_resolver::ConnectionProvider for HickoryConnectionProvider {
    type Conn = DnsExchange<FuchsiaRuntime>;
    type FutureConn = Pin<Box<dyn Future<Output = Result<Self::Conn, NetError>> + Send + 'static>>;
    type RuntimeProvider = FuchsiaRuntime;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        let (remote_addr, bind_addr) = match (ip, config.bind_addr) {
            (IpAddr::V6(v6), Some(SocketAddr::V6(bind_v6)))
                if bind_v6.ip().is_unspecified()
                    && bind_v6.port() == 0
                    && bind_v6.scope_id() != 0 =>
            {
                (SocketAddr::V6(SocketAddrV6::new(v6, config.port, 0, bind_v6.scope_id())), None)
            }
            _ => (SocketAddr::new(ip, config.port), config.bind_addr),
        };

        match &config.protocol {
            ProtocolConfig::Udp => {
                let timeout = cx.options.timeout;
                let os_port_selection = cx.options.os_port_selection;
                let avoid_local_udp_ports = cx.options.avoid_local_udp_ports.clone();
                Ok(Box::pin(async move {
                    Ok(UdpClientStream::builder(remote_addr, FuchsiaRuntime)
                        .with_timeout(Some(timeout))
                        .with_os_port_selection(os_port_selection)
                        .avoid_local_ports(avoid_local_udp_ports)
                        .with_bind_addr(bind_addr)
                        .exchange())
                }))
            }
            ProtocolConfig::Tcp => Ok(Box::pin(TcpClientStream::exchange(
                remote_addr,
                bind_addr,
                cx.options.timeout,
                Some(cx.options.max_active_requests),
                FuchsiaRuntime,
            ))),
        }
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &FuchsiaRuntime
    }
}

/// Hickory-based resolver that resolves DNS requests on Fuchsia.
pub type HickoryDnsResolver = HickoryResolver<HickoryConnectionProvider>;

#[cfg(test)]
mod tests {
    use super::*;

    use fidl_fuchsia_net as fnet;
    use fidl_fuchsia_net_interfaces as fnet_interfaces;
    use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
    use fidl_fuchsia_net_root as fnet_root;
    use futures::{AsyncReadExt as _, AsyncWriteExt as _};
    use hickory_proto::op::Message;
    use hickory_proto::rr::rdata::{A, AAAA};
    use hickory_proto::rr::{RData, Record};
    use hickory_resolver::config::{
        LookupIpStrategy, NameServerConfig, ResolverConfig as HickoryResolverConfig,
    };
    use net_declare::std::{ip, ip_v6, socket_addr};
    use trust_dns_resolver::config::ResolverConfig;

    use crate::FuchsiaExec;

    const MAX_DNS_UDP_MESSAGE_LEN: usize = 512;
    const LOOPBACK_IF_ID: u32 = 1;

    async fn add_loopback_ip(addr: Ipv6Addr) -> fnet_interfaces_admin::AddressStateProviderProxy {
        let root_interfaces =
            fuchsia_component::client::connect_to_protocol::<fnet_root::InterfacesMarker>()
                .expect("connect to fuchsia.net.root/Interfaces");
        let (control, control_server_end) =
            fidl::endpoints::create_proxy::<fnet_interfaces_admin::ControlMarker>();
        root_interfaces
            .get_admin(u64::from(LOOPBACK_IF_ID), control_server_end)
            .expect("get admin for loopback");
        let (asp, asp_server_end) =
            fidl::endpoints::create_proxy::<fnet_interfaces_admin::AddressStateProviderMarker>();
        control
            .add_address(
                &fnet::Subnet {
                    addr: fidl_fuchsia_net_ext::IpAddress(IpAddr::V6(addr)).into(),
                    prefix_len: 64,
                },
                &fnet_interfaces_admin::AddressParameters {
                    add_subnet_route: Some(true),
                    ..Default::default()
                },
                asp_server_end,
            )
            .expect("add address to loopback");
        let state =
            asp.watch_address_assignment_state().await.expect("watch address assignment state");
        assert_eq!(state, fnet_interfaces::AddressAssignmentState::Assigned);
        asp
    }

    fn build_ip_response(query_bytes: &[u8], expected_ip: IpAddr) -> Vec<u8> {
        let query = Message::from_vec(query_bytes).expect("deserialize DNS query");
        let query_name = query.queries[0].name().clone();
        let rdata = match expected_ip {
            IpAddr::V4(addr) => RData::A(A(addr)),
            IpAddr::V6(addr) => RData::AAAA(AAAA(addr)),
        };
        let mut response = query.into_response();
        let _: &mut Message = response.add_answer(Record::from_rdata(query_name, 60, rdata));
        response.to_vec().expect("serialize DNS response")
    }

    fn build_resolver(
        server_addr: SocketAddr,
        mut conn: ConnectionConfig,
        expected_ip: IpAddr,
    ) -> HickoryDnsResolver {
        set_connection_addr(&mut conn, server_addr);
        let name_server = NameServerConfig::new(server_addr.ip(), true, vec![conn]);
        let config = HickoryResolverConfig::from_parts(None, vec![], vec![name_server]);
        let mut builder =
            HickoryDnsResolver::builder_with_config(config, HickoryConnectionProvider);
        // Use a generous timeout to avoid flakes.
        builder.options_mut().timeout = std::time::Duration::from_secs(60);
        builder.options_mut().ip_strategy = match expected_ip {
            IpAddr::V4(_) => LookupIpStrategy::Ipv4Only,
            IpAddr::V6(_) => LookupIpStrategy::Ipv6Only,
        };
        builder.build().expect("build resolver")
    }

    async fn run_resolve_client(
        server_addr: SocketAddr,
        conn: ConnectionConfig,
        expected_ip: IpAddr,
    ) {
        if let SocketAddr::V6(mut bad_addr) = server_addr {
            if bad_addr.scope_id() != 0 {
                // Verify that both an omitted scope_id (0, rejected with EINVAL
                // for link-local addresses) and a bogus scope_id (999, rejected
                // with ENODEV) fail with NetError::NoConnections.
                for bad_scope_id in [0, 999] {
                    bad_addr.set_scope_id(bad_scope_id);
                    let bad_resolver =
                        build_resolver(SocketAddr::V6(bad_addr), conn.clone(), expected_ip);
                    assert!(matches!(
                        bad_resolver.lookup_ip("example.com.").await,
                        Err(NetError::NoConnections)
                    ));
                }
            }
        }
        let resolver = build_resolver(server_addr, conn, expected_ip);
        let response = resolver.lookup_ip("example.com.").await.expect("lookup_ip should succeed");
        assert_eq!(response.iter().collect::<Vec<_>>(), vec![expected_ip]);
    }

    async fn resolve_udp_test(bind_addr: SocketAddr, expected_ip: IpAddr) {
        let server = fasync::net::UdpSocket::bind(&bind_addr).expect("bind UDP server");
        let server_addr = server.local_addr().expect("get local UDP addr");

        let server_fut = async move {
            let mut buf = [0u8; MAX_DNS_UDP_MESSAGE_LEN];
            loop {
                let (read, src_addr) = server.recv_from(&mut buf).await.expect("receive UDP query");
                let response = build_ip_response(&buf[..read], expected_ip);
                let written = server.send_to(&response, src_addr).await.expect("send UDP response");
                assert_eq!(written, response.len());
            }
        };

        let client_fut = run_resolve_client(server_addr, ConnectionConfig::udp(), expected_ip);
        futures::select! {
            () = server_fut.fuse() => unreachable!("server_fut never returns"),
            () = client_fut.fuse() => {}
        }
    }

    async fn resolve_tcp_test(bind_addr: SocketAddr, expected_ip: IpAddr) {
        let listener = fasync::net::TcpListener::bind(&bind_addr).expect("bind TCP listener");
        let server_addr = listener.local_addr().expect("get local TCP addr");

        let server_fut = async move {
            let (_listener, mut stream, _src_addr) =
                listener.accept().await.expect("accept incoming TCP connection");
            let mut len_buf = [0u8; 2];
            stream.read_exact(&mut len_buf).await.expect("read TCP length field");
            let len = usize::from(u16::from_be_bytes(len_buf));
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.expect("read TCP DNS query");

            let response = build_ip_response(&buf, expected_ip);
            let response_len =
                u16::try_from(response.len()).expect("response fits in u16").to_be_bytes();
            stream.write_all(&response_len).await.expect("write TCP length field");
            stream.write_all(&response).await.expect("write TCP DNS response");
        };

        let client_fut = run_resolve_client(server_addr, ConnectionConfig::tcp(), expected_ip);
        let ((), ()) = futures::future::join(server_fut, client_fut).await;
    }

    #[test]
    fn test_ip_lookup() {
        use trust_dns_resolver::testing::ip_lookup_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        ip_lookup_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner)
    }

    #[test]
    fn test_ip_lookup_across_threads() {
        use trust_dns_resolver::testing::ip_lookup_across_threads_test;
        ip_lookup_across_threads_test::<FuchsiaExec, FuchsiaRuntime>(Spawner)
    }

    #[test]
    fn test_localhost_ipv4() {
        use trust_dns_resolver::testing::localhost_ipv4_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        localhost_ipv4_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    #[test]
    fn test_localhost_ipv6() {
        use trust_dns_resolver::testing::localhost_ipv6_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        localhost_ipv6_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    #[test]
    fn test_search_ipv4_large_ndots() {
        use trust_dns_resolver::testing::search_ipv4_large_ndots_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        search_ipv4_large_ndots_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    #[test]
    fn test_search_ipv6_large_ndots() {
        use trust_dns_resolver::testing::search_ipv6_large_ndots_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        search_ipv6_large_ndots_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    #[test]
    fn test_search_ipv6_name_parse_fails() {
        use trust_dns_resolver::testing::search_ipv6_name_parse_fails_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        search_ipv6_name_parse_fails_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_lookup_google() {
        use trust_dns_resolver::testing::lookup_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        lookup_test::<FuchsiaExec, FuchsiaRuntime>(ResolverConfig::google(), exec, Spawner)
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_lookup_cloudflare() {
        use trust_dns_resolver::testing::lookup_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        lookup_test::<FuchsiaExec, FuchsiaRuntime>(ResolverConfig::cloudflare(), exec, Spawner)
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore] // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    fn test_lookup_quad9() {
        use trust_dns_resolver::testing::lookup_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        lookup_test::<FuchsiaExec, FuchsiaRuntime>(ResolverConfig::quad9(), exec, Spawner)
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_fqdn() {
        use trust_dns_resolver::testing::fqdn_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        fqdn_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_ndots() {
        use trust_dns_resolver::testing::ndots_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        ndots_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_large_ndots() {
        use trust_dns_resolver::testing::large_ndots_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        large_ndots_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_domain_search() {
        use trust_dns_resolver::testing::domain_search_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        domain_search_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_search_list() {
        use trust_dns_resolver::testing::search_list_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        search_list_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    // TODO(chunyingw): Make it as a manual test.
    // The test requires Internet connection for testing.
    #[test]
    #[ignore]
    fn test_idna() {
        use trust_dns_resolver::testing::idna_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        idna_test::<FuchsiaExec, FuchsiaRuntime>(exec, Spawner);
    }

    #[fuchsia::test]
    async fn test_resolve_udp_ipv4_hickory() {
        resolve_udp_test(socket_addr!("127.0.0.1:0"), ip!("192.0.2.1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_udp_ipv6_hickory() {
        resolve_udp_test(socket_addr!("[::1]:0"), ip!("2001:db8::1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_udp_ipv6_with_scope_id_hickory() {
        let addr = ip_v6!("fe80::1");
        let _asp = add_loopback_ip(addr).await;
        resolve_udp_test(
            SocketAddr::V6(SocketAddrV6::new(addr, 0, 0, LOOPBACK_IF_ID)),
            ip!("2001:db8::1"),
        )
        .await
    }

    #[fuchsia::test]
    async fn test_resolve_tcp_ipv4_hickory() {
        resolve_tcp_test(socket_addr!("127.0.0.1:0"), ip!("192.0.2.1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_tcp_ipv6_hickory() {
        resolve_tcp_test(socket_addr!("[::1]:0"), ip!("2001:db8::1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_tcp_ipv6_with_scope_id_hickory() {
        let addr = ip_v6!("fe80::2");
        let _asp = add_loopback_ip(addr).await;
        resolve_tcp_test(
            SocketAddr::V6(SocketAddrV6::new(addr, 0, 0, LOOPBACK_IF_ID)),
            ip!("2001:db8::1"),
        )
        .await
    }
}
