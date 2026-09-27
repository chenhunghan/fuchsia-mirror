// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdomain_container::FDomain;
use fdomain_container::wire::FDomainCodec;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use log::{debug, error};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::pin;
use std::rc::Weak;
use std::task::{Context, Poll};

/// The maximum chunk size to read from the socket in a single async poll.
const SOCKET_READ_CHUNK_SIZE: usize = 4096;

/// Returns a sender that you can send fio::Directory server ends to, and they
/// will have the toolbox namespace served on them.
fn serve_toolboxes(
    rcs: Weak<remote_control::RemoteControlService>,
) -> (
    UnboundedSender<fidl::endpoints::ServerEnd<fidl_fuchsia_io::DirectoryMarker>>,
    impl Future<Output = ()>,
) {
    let (toolbox_server_sender, mut toolbox_servers) = unbounded::<fidl::endpoints::ServerEnd<_>>();
    let toolbox_task = async move {
        while let Some(server_end) = toolbox_servers.next().await {
            let Some(rcs) = rcs.upgrade() else {
                error!("RCS disappeared with pending toolbox request");
                break;
            };

            if let Err(err) = rcs.open_toolbox(server_end.into_channel()).await {
                error!(err:?; "Could not open toolbox for client");
            }
        }

        // No more incoming connections, so go to sleep. We'll wait for the
        // socket to close before we stop serving requests.
        futures::future::pending::<()>().await;
    };
    (toolbox_server_sender, toolbox_task)
}

/// Maximum number of bytes to buffer in the outgoing packet queue before applying
/// backpressure to FDomain.
const MAX_OUT_QUEUE_BYTES: usize = 256 * 1024;

/// Represents an outgoing packet waiting to be written to the socket.
///
/// Stores the 4-byte little-endian length prefix and the payload `data` without
/// copying or zeroing memory into an intermediate ring buffer.
struct OutPacket {
    header: [u8; 4],
    data: Box<[u8]>,
    offset: usize,
}

impl OutPacket {
    fn is_empty(&self) -> bool {
        self.offset >= 4 + self.data.len()
    }

    fn current_slice(&self) -> &[u8] {
        if self.offset < 4 { &self.header[self.offset..] } else { &self.data[self.offset - 4..] }
    }
}

/// Given an async socket and outgoing packets waiting to be written, try to
/// asynchronously write them to the socket. Only returns Ready when the socket is closed.
fn poll_process_out_queue(
    socket: &fuchsia_async::Socket,
    out_queue: &mut VecDeque<OutPacket>,
    out_queue_bytes: &mut usize,
    ctx: &mut Context<'_>,
) -> Poll<()> {
    while let Some(mut packet) = out_queue.pop_front() {
        loop {
            if packet.is_empty() {
                break;
            }
            match socket.poll_write_ref(ctx, packet.current_slice()) {
                Poll::Pending => {
                    out_queue.push_front(packet);
                    return Poll::Pending;
                }
                Poll::Ready(Ok(0)) => {
                    debug!("FDomain connection closed");
                    return Poll::Ready(());
                }
                Poll::Ready(Ok(size)) => {
                    packet.offset += size;
                    *out_queue_bytes = out_queue_bytes.saturating_sub(size);
                }
                Poll::Ready(Err(err)) => {
                    if err != fidl::Status::PEER_CLOSED {
                        error!(error:? = err; "FDomain socket write error");
                    } else {
                        debug!("FDomain connection closed");
                    }
                    return Poll::Ready(());
                }
            }
        }
    }
    Poll::Pending
}

/// Serves an FDomain connection over the given socket. The FDomain served has
/// the toolbox as its namespace.
pub async fn serve_fdomain_connection(
    rcs: Weak<remote_control::RemoteControlService>,
    socket: fuchsia_async::Socket,
) {
    log::debug!("Spawned new FDomain connection");

    let (toolbox_server_sender, toolbox_task) = serve_toolboxes(rcs);
    let mut toolbox_task = pin!(toolbox_task);

    let fdomain = FDomain::new(move || {
        let (client_end, server_end) = fidl::endpoints::create_endpoints();
        let _ = toolbox_server_sender.unbounded_send(server_end);

        Ok(client_end)
    });

    let mut codec = FDomainCodec::new(fdomain);

    let mut out_queue: VecDeque<OutPacket> = VecDeque::new();
    let mut out_queue_bytes: usize = 0;
    let mut in_queue: Vec<u8> = Vec::new();

    futures::future::poll_fn(move |ctx| {
        let Poll::Pending = toolbox_task.as_mut().poll(ctx) else {
            unreachable!("Toolbox task should sleep forever but it returned!");
        };

        // 1. Process incoming data from socket into FDomain.
        let mut read_buf = [0u8; SOCKET_READ_CHUNK_SIZE];
        loop {
            match socket.poll_read_ref(ctx, &mut read_buf) {
                Poll::Pending => break,
                Poll::Ready(Ok(0)) => {
                    debug!("FDomain connection closed");
                    return Poll::Ready(());
                }
                Poll::Ready(Ok(got)) => {
                    in_queue.extend_from_slice(&read_buf[..got]);
                }
                Poll::Ready(Err(err)) => {
                    if err != fidl::Status::PEER_CLOSED {
                        error!(error:? = err; "FDomain socket read error");
                    } else {
                        debug!("FDomain connection closed");
                    }
                    return Poll::Ready(());
                }
            }
        }

        let mut processed_any = false;
        let mut offset = 0;
        while in_queue.len() - offset >= 4 {
            let payload_len = u32::from_le_bytes(in_queue[offset..offset+4].try_into().unwrap()) as usize;
            let packet_len = payload_len + 4;
            if in_queue.len() - offset < packet_len {
                break;
            }

            if let Err(err) = codec.message(&in_queue[offset + 4..offset + packet_len]) {
                error!(err:?; "FDomain could not interpret an incoming message");
                return Poll::Ready(());
            }

            offset += packet_len;
            processed_any = true;
        }
        if offset > 0 {
            in_queue.drain(..offset);
        }

        // 2. Pull outgoing data from FDomain up to backpressure limit.
        while out_queue_bytes < MAX_OUT_QUEUE_BYTES {
            match codec.poll_next_unpin(ctx) {
                Poll::Ready(Some(Ok(outgoing))) => {
                    let Ok(size): Result<u32, _> = outgoing.len().try_into() else {
                        error!("Tried to send too-large packet (size {})", outgoing.len());
                        return Poll::Ready(());
                    };
                    out_queue_bytes += outgoing.len() + 4;
                    out_queue.push_back(OutPacket {
                        header: size.to_le_bytes(),
                        data: outgoing,
                        offset: 0,
                    });
                }
                Poll::Ready(Some(Err(err))) => {
                    error!(err:?; "FDomain encountered an internal error");
                    return Poll::Ready(());
                }
                Poll::Ready(None) => {
                    error!(
                        "FDomain hung up its outgoing message stream. \
                            This is not how we expect the library to behave!"
                    );
                    return Poll::Ready(());
                }
                Poll::Pending => break,
            }
        }

        // 3. Flush outgoing packets to the socket.
        let prev_out_bytes = out_queue_bytes;
        if poll_process_out_queue(&socket, &mut out_queue, &mut out_queue_bytes, ctx).is_ready() {
            return Poll::Ready(());
        }

        if prev_out_bytes >= MAX_OUT_QUEUE_BYTES && out_queue_bytes < MAX_OUT_QUEUE_BYTES {
            ctx.waker().wake_by_ref();
        }

        if processed_any {
            ctx.waker().wake_by_ref();
        }

        Poll::Pending
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;
    use futures::AsyncReadExt;

    #[fasync::run_singlethreaded(test)]
    async fn test_out_packet_writing() {
        let (local_sock, remote_sock) = fidl::Socket::create_stream();
        let local_sock = fasync::Socket::from_socket(local_sock);
        let mut remote_sock = fasync::Socket::from_socket(remote_sock);

        let mut out_queue = VecDeque::new();
        let mut out_queue_bytes = 0;

        let payload1 = vec![1u8, 2, 3, 4, 5];
        let payload2 = vec![6u8; 100_000]; // Test packet > 64KB
        let payload3 = vec![7u8; 500_000]; // Test packet > MAX_OUT_QUEUE_BYTES (256KB)

        let size1: u32 = payload1.len().try_into().unwrap();
        out_queue_bytes += payload1.len() + 4;
        out_queue.push_back(OutPacket {
            header: size1.to_le_bytes(),
            data: payload1.into_boxed_slice(),
            offset: 0,
        });

        let size2: u32 = payload2.len().try_into().unwrap();
        out_queue_bytes += payload2.len() + 4;
        out_queue.push_back(OutPacket {
            header: size2.to_le_bytes(),
            data: payload2.into_boxed_slice(),
            offset: 0,
        });

        let size3: u32 = payload3.len().try_into().unwrap();
        out_queue_bytes += payload3.len() + 4;
        out_queue.push_back(OutPacket {
            header: size3.to_le_bytes(),
            data: payload3.into_boxed_slice(),
            offset: 0,
        });

        let write_fut = futures::future::poll_fn(|ctx| {
            let _ = poll_process_out_queue(&local_sock, &mut out_queue, &mut out_queue_bytes, ctx);
            if out_queue.is_empty() { Poll::Ready(()) } else { Poll::Pending }
        });

        let read_fut = async {
            let total_len = (4 + 5) + (4 + 100_000) + (4 + 500_000);
            let mut read_data = vec![0u8; total_len];
            let mut read_offset = 0;
            while read_offset < read_data.len() {
                let got = remote_sock.read(&mut read_data[read_offset..]).await.unwrap();
                assert!(got > 0);
                read_offset += got;
            }
            read_data
        };

        let ((), read_data) = futures::join!(write_fut, read_fut);
        assert_eq!(out_queue_bytes, 0);

        // Verify packet 1
        let p1_len = u32::from_le_bytes(read_data[..4].try_into().unwrap()) as usize;
        assert_eq!(p1_len, 5);
        assert_eq!(&read_data[4..9], &[1, 2, 3, 4, 5]);

        // Verify packet 2
        let p2_len = u32::from_le_bytes(read_data[9..13].try_into().unwrap()) as usize;
        assert_eq!(p2_len, 100_000);
        assert_eq!(&read_data[13..13 + 100_000], &vec![6u8; 100_000][..]);

        // Verify packet 3
        let p3_offset = 13 + 100_000;
        let p3_len =
            u32::from_le_bytes(read_data[p3_offset..p3_offset + 4].try_into().unwrap()) as usize;
        assert_eq!(p3_len, 500_000);
        assert_eq!(&read_data[p3_offset + 4..p3_offset + 4 + 500_000], &vec![7u8; 500_000][..]);
    }
}
