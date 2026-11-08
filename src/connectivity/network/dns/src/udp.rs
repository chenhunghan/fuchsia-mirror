// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FuchsiaTime;
use async_trait::async_trait;
use fuchsia_async::net::UdpSocket;
use futures::FutureExt as _;
use hickory_net::runtime;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::pin;
use std::task::{Context, Poll};

/// A Fuchsia-compatible implementation of `hickory_net::runtime::DnsUdpSocket` which allows
/// creating a `UdpSocket` to a particular destination.
pub struct DnsUdpSocket(UdpSocket);

impl DnsUdpSocket {
    /// Binds a `DnsUdpSocket` to `addr`.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        UdpSocket::bind(&addr).map(Self)
    }
}

#[async_trait]
impl trust_dns_proto::udp::UdpSocket for DnsUdpSocket {
    type Time = FuchsiaTime;

    async fn connect_with_bind(_addr: SocketAddr, bind_addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(&bind_addr).map(Self)?;

        // TODO(https://fxbug.dev/42060217): Consider calling connect on the
        // socket. Doing so isn't strictly necessary and is disabled within the
        // provided Trust-DNS implementations. As a result, the same behavior is
        // currently implemented here. See
        // https://github.com/hickory-dns/hickory-dns/commit/e712a2c031572a128d720d6c763a83fe57399d7f

        Ok(socket)
    }

    async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let bind_addr = match addr {
            SocketAddr::V4(_addr) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_addr) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };

        <Self as trust_dns_proto::udp::UdpSocket>::connect_with_bind(addr, bind_addr).await
    }

    async fn bind(addr: SocketAddr) -> io::Result<Self> {
        UdpSocket::bind(&addr).map(Self)
    }

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let Self(socket) = self;
        let fut = socket.recv_from(buf);
        let mut fut = pin!(fut);
        fut.poll_unpin(cx)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let Self(socket) = self;
        socket.recv_from(buf).await
    }

    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let Self(socket) = self;
        let fut = socket.send_to(buf, target);
        let mut fut = pin!(fut);
        fut.poll_unpin(cx)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let Self(socket) = self;
        socket.send_to(buf, target).await
    }
}

#[async_trait]
impl runtime::DnsUdpSocket for DnsUdpSocket {
    type Time = FuchsiaTime;

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let Self(socket) = self;
        let fut = socket.recv_from(buf);
        let mut fut = pin!(fut);
        fut.poll_unpin(cx)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let Self(socket) = self;
        socket.recv_from(buf).await
    }

    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let Self(socket) = self;
        let fut = socket.send_to(buf, target);
        let mut fut = pin!(fut);
        fut.poll_unpin(cx)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let Self(socket) = self;
        socket.send_to(buf, target).await
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net::IpAddr;
    use std::pin::Pin;
    use std::str::FromStr as _;
    use std::time::Duration;

    use futures::{Future, StreamExt as _};
    use hickory_net::runtime::RuntimeProvider;
    use hickory_net::udp::{UdpClientStream, UdpStream};
    use hickory_net::xfer::dns_handle::DnsStreamHandle as _;
    use hickory_net::xfer::{DnsRequestSender as _, FirstAnswer as _};
    use hickory_proto::op::{DnsRequest, DnsRequestOptions, Message, Query, SerialMessage};
    use hickory_proto::rr::rdata::NULL;
    use hickory_proto::rr::{Name, RData, Record, RecordType};
    use net_declare::std::ip;

    use crate::FuchsiaExec;
    use crate::async_resolver::Spawner;
    use crate::tcp::DnsTcpStream;

    const TEST_BYTES: &[u8] = b"DEADBEEF";
    const MAX_DNS_UDP_MESSAGE_LEN: usize = 512;
    const SEND_RECV_TIMES: usize = 4;

    #[derive(Clone)]
    struct TestRuntime;

    impl RuntimeProvider for TestRuntime {
        type Handle = Spawner;
        type Tcp = DnsTcpStream;
        type Timer = FuchsiaTime;
        type Udp = DnsUdpSocket;

        fn create_handle(&self) -> Self::Handle {
            Spawner
        }

        fn connect_tcp(
            &self,
            _server_addr: SocketAddr,
            _bind_addr: Option<SocketAddr>,
            _timeout: Option<Duration>,
        ) -> Pin<Box<dyn Future<Output = io::Result<Self::Tcp>> + Send + 'static>> {
            unimplemented!("not used in UDP tests")
        }

        fn bind_udp(
            &self,
            local_addr: SocketAddr,
            _server_addr: SocketAddr,
        ) -> Pin<Box<dyn Future<Output = io::Result<Self::Udp>> + Send + 'static>> {
            Box::pin(futures::future::ready(UdpSocket::bind(&local_addr).map(DnsUdpSocket)))
        }
    }

    async fn udp_stream_test(server_addr: IpAddr) {
        let server = UdpSocket::bind(&SocketAddr::new(server_addr, 0)).expect("bind UDP server");
        let server_addr = server.local_addr().expect("get local addr");

        let server_fut = async move {
            let mut buf = [0u8; MAX_DNS_UDP_MESSAGE_LEN];
            for _ in 0..SEND_RECV_TIMES {
                let (read, src_addr) = server.recv_from(&mut buf).await.expect("receive UDP query");
                assert_eq!(&buf[..read], TEST_BYTES);
                let written =
                    server.send_to(&buf[..read], src_addr).await.expect("send UDP response");
                assert_eq!(written, read);
            }
        };

        let client_fut = async move {
            let bind_addr = SocketAddr::new(
                match server_addr {
                    SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                },
                0,
            );
            let socket = UdpSocket::bind(&bind_addr).map(DnsUdpSocket).expect("bind UDP client");
            let (mut stream, mut sender) =
                UdpStream::<TestRuntime>::with_bound(socket, server_addr);

            for _ in 0..SEND_RECV_TIMES {
                sender
                    .send(SerialMessage::new(TEST_BYTES.to_vec(), server_addr))
                    .expect("send message");
                let message = stream
                    .next()
                    .await
                    .expect("stream ended unexpectedly")
                    .expect("receive message");
                assert_eq!(message.bytes(), TEST_BYTES);
                assert_eq!(message.addr(), server_addr);
            }
        };

        let ((), ()) = futures::future::join(server_fut, client_fut).await;
    }

    async fn udp_client_stream_test(server_addr: IpAddr) {
        let server = UdpSocket::bind(&SocketAddr::new(server_addr, 0)).expect("bind UDP server");
        let server_addr = server.local_addr().expect("get local addr");
        let test_name = Name::from_str("dead.beef.").expect("parse domain name");

        let server_name = test_name.clone();
        let server_fut = async move {
            let mut buf = [0u8; MAX_DNS_UDP_MESSAGE_LEN];
            for _ in 0..SEND_RECV_TIMES {
                let (read, src_addr) = server.recv_from(&mut buf).await.expect("receive DNS query");
                let query = Message::from_vec(&buf[..read]).expect("deserialize DNS query");
                assert_eq!(query.queries, [Query::query(server_name.clone(), RecordType::NULL)]);

                let mut response = query.into_response();
                let _: &mut Message = response.add_answer(Record::from_rdata(
                    server_name.clone(),
                    0,
                    RData::NULL(NULL::with(TEST_BYTES.to_vec())),
                ));
                let response = response.to_vec().expect("serialize DNS response");
                let written = server.send_to(&response, src_addr).await.expect("send DNS response");
                assert_eq!(written, response.len());
            }
        };

        let client_fut = async move {
            let mut query = Message::query();
            let _: &mut Message = query.add_query(Query::query(test_name, RecordType::NULL));
            let mut stream = UdpClientStream::builder(server_addr, TestRuntime).build();

            for _ in 0..SEND_RECV_TIMES {
                let response = stream
                    .send_message(DnsRequest::new(query.clone(), DnsRequestOptions::default()))
                    .first_answer()
                    .await
                    .expect("receive DNS response");
                let response = Message::from(response);
                assert_eq!(response.answers.len(), 1);
                assert_eq!(response.answers[0].data, RData::NULL(NULL::with(TEST_BYTES.to_vec())));
            }
        };

        let ((), ()) = futures::future::join(server_fut, client_fut).await;
    }

    #[test]
    fn test_next_random_socket() {
        use trust_dns_proto::tests::next_random_socket_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        next_random_socket_test::<DnsUdpSocket, FuchsiaExec>(exec)
    }

    #[fuchsia::test]
    async fn test_udp_stream_ipv4() {
        use trust_dns_proto::tests::udp_stream_test;
        udp_stream_test::<DnsUdpSocket>(ip!("127.0.0.1")).await
    }

    #[fuchsia::test]
    async fn test_udp_stream_ipv6() {
        use trust_dns_proto::tests::udp_stream_test;
        udp_stream_test::<DnsUdpSocket>(ip!("::1")).await
    }

    #[test]
    fn test_udp_client_stream_ipv4() {
        use trust_dns_proto::tests::udp_client_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        udp_client_stream_test::<DnsUdpSocket, FuchsiaExec, FuchsiaTime>(ip!("127.0.0.1"), exec)
    }

    #[test]
    fn test_udp_client_stream_ipv6() {
        use trust_dns_proto::tests::udp_client_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        udp_client_stream_test::<DnsUdpSocket, FuchsiaExec, FuchsiaTime>(ip!("::1"), exec)
    }

    #[fuchsia::test]
    async fn test_next_random_socket_hickory() {
        let (stream, _) = UdpStream::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52),
            None,
            None,
            false,
            TestRuntime,
        );
        drop(stream.await.expect("failed to get next socket address"));
    }

    #[fuchsia::test]
    async fn test_udp_stream_ipv4_hickory() {
        udp_stream_test(ip!("127.0.0.1")).await
    }

    #[fuchsia::test]
    async fn test_udp_stream_ipv6_hickory() {
        udp_stream_test(ip!("::1")).await
    }

    #[fuchsia::test]
    async fn test_udp_client_stream_ipv4_hickory() {
        udp_client_stream_test(ip!("127.0.0.1")).await
    }

    #[fuchsia::test]
    async fn test_udp_client_stream_ipv6_hickory() {
        udp_client_stream_test(ip!("::1")).await
    }
}
