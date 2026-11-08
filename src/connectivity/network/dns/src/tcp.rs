// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FuchsiaTime;
use async_trait::async_trait;
use fuchsia_async::net::TcpStream;
use futures::io::{AsyncRead, AsyncWrite};
use futures::task::{Context, Poll};
use hickory_net::runtime;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use trust_dns_proto::tcp::Connect;

/// A Fuchsia-compatible implementation of `hickory_net::runtime::DnsTcpStream` which allows
/// creating a `TcpStream` to a particular destination.
pub struct DnsTcpStream(TcpStream);

impl DnsTcpStream {
    /// Connects a `DnsTcpStream` to `addr`.
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        TcpStream::connect(addr)?.await.map(Self)
    }
}

impl trust_dns_proto::tcp::DnsTcpStream for DnsTcpStream {
    type Time = FuchsiaTime;
}

impl runtime::DnsTcpStream for DnsTcpStream {
    type Time = FuchsiaTime;
}

#[async_trait]
impl Connect for DnsTcpStream {
    async fn connect(addr: SocketAddr) -> io::Result<Self> {
        Self::connect(addr).await
    }

    async fn connect_with_bind(
        addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
    ) -> io::Result<Self> {
        match bind_addr {
            Some(bind_addr) => {
                unimplemented!(
                    "https://fxbug.dev/42180092: cannot bind to {:?}; `connect_with_bind` is \
                    unimplemented",
                    bind_addr,
                )
            }
            None => Self::connect(addr).await,
        }
    }
}

impl AsyncRead for DnsTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for DnsTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net::IpAddr;

    use fuchsia_async::net::TcpListener;
    use futures::{AsyncReadExt as _, AsyncWriteExt as _, Stream, StreamExt as _};
    use hickory_net::BufDnsStreamHandle;
    use hickory_net::tcp::{self as hickory_tcp, TcpClientStream};
    use hickory_net::xfer::dns_handle::DnsStreamHandle as _;
    use hickory_proto::op::SerialMessage;
    use net_declare::std::ip;

    use crate::FuchsiaExec;

    const TEST_BYTES: &[u8] = b"DEADBEEF";
    const SEND_RECV_TIMES: usize = 4;

    async fn mock_tcp_server(listener: TcpListener) {
        let (_listener, mut stream, _src_addr) =
            listener.accept().await.expect("accept incoming TCP connection");
        // Read the two-octet length field, which tells us the length of the
        // following DNS message, in network (big-endian) order.
        let mut len_buf = [0u8; 2];
        for _ in 0..SEND_RECV_TIMES {
            stream.read_exact(&mut len_buf).await.expect("read length field");
            let len = usize::from(u16::from_be_bytes(len_buf));
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.expect("read DNS message");
            assert_eq!(buf.as_slice(), TEST_BYTES);

            stream.write_all(&len_buf).await.expect("send length field");
            stream.write_all(&buf).await.expect("send DNS message");
        }
    }

    async fn check_round_trip<S, E>(
        mut stream: S,
        mut sender: BufDnsStreamHandle,
        server_addr: SocketAddr,
    ) where
        S: Stream<Item = Result<SerialMessage, E>> + Unpin,
        E: std::fmt::Debug,
    {
        for _ in 0..SEND_RECV_TIMES {
            sender
                .send(SerialMessage::new(TEST_BYTES.to_vec(), server_addr))
                .expect("send message");
            let message =
                stream.next().await.expect("stream ended unexpectedly").expect("receive message");
            assert_eq!(message.bytes(), TEST_BYTES);
            assert_eq!(message.addr(), server_addr);
        }
    }

    async fn tcp_stream_test(server_addr: IpAddr) {
        let listener =
            TcpListener::bind(&SocketAddr::new(server_addr, 0)).expect("bind TCP listener");
        let server_addr = listener.local_addr().expect("get local addr");

        let client_fut = async move {
            let tcp = TcpStream::connect(server_addr)
                .expect("create connector")
                .await
                .map(DnsTcpStream)
                .expect("connect failed");
            let (stream, sender) = hickory_tcp::TcpStream::from_stream(tcp, server_addr);
            check_round_trip(stream, sender, server_addr).await;
        };

        let ((), ()) = futures::future::join(mock_tcp_server(listener), client_fut).await;
    }

    async fn tcp_client_stream_test(server_addr: IpAddr) {
        let listener =
            TcpListener::bind(&SocketAddr::new(server_addr, 0)).expect("bind TCP listener");
        let server_addr = listener.local_addr().expect("get local addr");

        let client_fut = async move {
            let tcp = TcpStream::connect(server_addr)
                .expect("create connector")
                .await
                .map(DnsTcpStream)
                .expect("connect failed");
            let (tcp_stream, sender) = hickory_tcp::TcpStream::from_stream(tcp, server_addr);
            let stream = TcpClientStream::from_stream(tcp_stream);
            check_round_trip(stream, sender, server_addr).await;
        };

        let ((), ()) = futures::future::join(mock_tcp_server(listener), client_fut).await;
    }

    #[test]
    fn test_tcp_stream_ipv4() {
        use trust_dns_proto::tests::tcp_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        tcp_stream_test::<DnsTcpStream, FuchsiaExec, FuchsiaTime>(ip!("127.0.0.1"), exec)
    }

    #[test]
    fn test_tcp_stream_ipv6() {
        use trust_dns_proto::tests::tcp_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        tcp_stream_test::<DnsTcpStream, FuchsiaExec, FuchsiaTime>(ip!("::1"), exec)
    }

    #[test]
    fn test_tcp_client_stream_ipv4() {
        use trust_dns_proto::tests::tcp_client_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        tcp_client_stream_test::<DnsTcpStream, FuchsiaExec, FuchsiaTime>(ip!("127.0.0.1"), exec)
    }

    #[test]
    fn test_tcp_client_stream_ipv6() {
        use trust_dns_proto::tests::tcp_client_stream_test;
        let exec = FuchsiaExec::new().expect("failed to create fuchsia executor");
        tcp_client_stream_test::<DnsTcpStream, FuchsiaExec, FuchsiaTime>(ip!("::1"), exec)
    }

    #[fuchsia::test]
    async fn test_tcp_stream_ipv4_hickory() {
        tcp_stream_test(ip!("127.0.0.1")).await
    }

    #[fuchsia::test]
    async fn test_tcp_stream_ipv6_hickory() {
        tcp_stream_test(ip!("::1")).await
    }

    #[fuchsia::test]
    async fn test_tcp_client_stream_ipv4_hickory() {
        tcp_client_stream_test(ip!("127.0.0.1")).await
    }

    #[fuchsia::test]
    async fn test_tcp_client_stream_ipv6_hickory() {
        tcp_client_stream_test(ip!("::1")).await
    }
}
