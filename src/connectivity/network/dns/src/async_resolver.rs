// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FuchsiaTime;
use crate::tcp::DnsTcpStream;
use crate::udp::DnsUdpSocket;
use fuchsia_async as fasync;
use futures::{Future, FutureExt};
use hickory_net::runtime::{RuntimeProvider as HickoryRuntimeProvider, Spawn as HickorySpawn};
use hickory_resolver::Resolver as HickoryResolver;
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

/// Hickory-based resolver that resolves DNS requests on Fuchsia.
pub type HickoryDnsResolver = HickoryResolver<FuchsiaRuntime>;

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{IpAddr, SocketAddr};

    use futures::{AsyncReadExt as _, AsyncWriteExt as _};
    use hickory_proto::op::Message;
    use hickory_proto::rr::rdata::{A, AAAA};
    use hickory_proto::rr::{RData, Record};
    use hickory_resolver::config::{
        ConnectionConfig, LookupIpStrategy, NameServerConfig,
        ResolverConfig as HickoryResolverConfig,
    };
    use net_declare::std::ip;
    use trust_dns_resolver::config::ResolverConfig;

    use crate::FuchsiaExec;

    const MAX_DNS_UDP_MESSAGE_LEN: usize = 512;

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
        conn.port = server_addr.port();
        let name_server = NameServerConfig::new(server_addr.ip(), true, vec![conn]);
        let config = HickoryResolverConfig::from_parts(None, vec![], vec![name_server]);
        let mut builder = HickoryDnsResolver::builder_with_config(config, FuchsiaRuntime);
        builder.options_mut().ip_strategy = match expected_ip {
            IpAddr::V4(_) => LookupIpStrategy::Ipv4Only,
            IpAddr::V6(_) => LookupIpStrategy::Ipv6Only,
        };
        builder.build().expect("build resolver")
    }

    async fn resolve_udp_test(server_ip: IpAddr, expected_ip: IpAddr) {
        let server =
            fasync::net::UdpSocket::bind(&SocketAddr::new(server_ip, 0)).expect("bind UDP server");
        let server_addr = server.local_addr().expect("get local UDP addr");

        let server_fut = async move {
            let mut buf = [0u8; MAX_DNS_UDP_MESSAGE_LEN];
            let (read, src_addr) = server.recv_from(&mut buf).await.expect("receive UDP query");
            let response = build_ip_response(&buf[..read], expected_ip);
            let written = server.send_to(&response, src_addr).await.expect("send UDP response");
            assert_eq!(written, response.len());
        };

        let client_fut = async move {
            let resolver = build_resolver(server_addr, ConnectionConfig::udp(), expected_ip);
            let response =
                resolver.lookup_ip("example.com.").await.expect("UDP lookup_ip should succeed");
            assert_eq!(response.iter().collect::<Vec<_>>(), vec![expected_ip]);
        };

        let ((), ()) = futures::future::join(server_fut, client_fut).await;
    }

    async fn resolve_tcp_test(server_ip: IpAddr, expected_ip: IpAddr) {
        let listener = fasync::net::TcpListener::bind(&SocketAddr::new(server_ip, 0))
            .expect("bind TCP listener");
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

        let client_fut = async move {
            let resolver = build_resolver(server_addr, ConnectionConfig::tcp(), expected_ip);
            let response =
                resolver.lookup_ip("example.com.").await.expect("TCP lookup_ip should succeed");
            assert_eq!(response.iter().collect::<Vec<_>>(), vec![expected_ip]);
        };

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
        resolve_udp_test(ip!("127.0.0.1"), ip!("192.0.2.1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_udp_ipv6_hickory() {
        resolve_udp_test(ip!("::1"), ip!("2001:db8::1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_tcp_ipv4_hickory() {
        resolve_tcp_test(ip!("127.0.0.1"), ip!("192.0.2.1")).await
    }

    #[fuchsia::test]
    async fn test_resolve_tcp_ipv6_hickory() {
        resolve_tcp_test(ip!("::1"), ip!("2001:db8::1")).await
    }
}
