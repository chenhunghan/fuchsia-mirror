// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Asynchronous stream adapter implementations for the UART driver daemon.

use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncBufRead, AsyncWrite};

/// Implements a packetized transport for the FDomain client library over Tokio readers/writers.
///
/// Frames discrete messages using a 4-byte little-endian length prefix.
pub struct FDomainTransport {
    input: Pin<Box<dyn AsyncBufRead + Unpin + Send>>,
    output: Pin<Box<dyn AsyncWrite + Unpin + Send>>,
    write_buf: Vec<u8>,
    in_buf: Vec<u8>,
}

impl FDomainTransport {
    /// Constructs a new [`FDomainTransport`] wrapping the given asynchronous reader and writer.
    pub fn new(
        input: Box<dyn AsyncBufRead + Unpin + Send>,
        output: Box<dyn AsyncWrite + Unpin + Send>,
    ) -> Self {
        FDomainTransport {
            input: Pin::new(input),
            output: Pin::new(output),
            write_buf: Vec::new(),
            in_buf: Vec::new(),
        }
    }
}

impl fdomain_client::FDomainTransport for FDomainTransport {
    fn poll_send_message(
        mut self: Pin<&mut Self>,
        msg: &[u8],
        ctx: &mut Context<'_>,
    ) -> Poll<Result<(), Option<std::io::Error>>> {
        if self.write_buf.is_empty() {
            let size: u32 = msg
                .len()
                .try_into()
                .map_err(|_| std::io::Error::other("Message size exceeded u32 capacity"))?;
            self.write_buf.extend_from_slice(&size.to_le_bytes());
            self.write_buf.extend_from_slice(msg);
        }

        let this = &mut *self;
        while !this.write_buf.is_empty() {
            let got = ready!(this.output.as_mut().poll_write(ctx, &this.write_buf))?;
            if got == 0 {
                return Poll::Ready(Err(None));
            }
            this.write_buf.drain(..got);
        }
        Poll::Ready(Ok(()))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FDomainTransport")
    }

    fn has_debug_fmt(&self) -> bool {
        true
    }
}

const LENGTH_PREFIX_SIZE: usize = std::mem::size_of::<u32>();

impl futures::Stream for FDomainTransport {
    type Item = std::io::Result<Box<[u8]>>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.in_buf.len() >= LENGTH_PREFIX_SIZE {
                let len: usize =
                    u32::from_le_bytes(self.in_buf[..LENGTH_PREFIX_SIZE].try_into().unwrap())
                        .try_into()
                        .unwrap();
                if self.in_buf.len() >= len + LENGTH_PREFIX_SIZE {
                    let payload: Box<[u8]> =
                        self.in_buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].into();
                    self.in_buf.drain(..LENGTH_PREFIX_SIZE + len);
                    return Poll::Ready(Some(Ok(payload)));
                }
            }

            let this = &mut *self;
            let buf = ready!(this.input.as_mut().poll_fill_buf(ctx))?;
            if buf.is_empty() {
                return Poll::Ready(None);
            }
            this.in_buf.extend_from_slice(buf);
            let len = buf.len();
            this.input.as_mut().consume(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdomain_client::FDomainTransport as _;
    use futures::StreamExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[fuchsia::test]
    async fn test_fdomain_transport_send_and_receive() {
        let (client_in, mut server_out) = tokio::io::duplex(1024);
        let (mut server_in, client_out) = tokio::io::duplex(1024);
        let mut transport = FDomainTransport::new(
            Box::new(tokio::io::BufReader::new(client_in)),
            Box::new(client_out),
        );

        let msg = b"test fdomain packet";
        futures::future::poll_fn(|cx| Pin::new(&mut transport).poll_send_message(msg, cx))
            .await
            .unwrap();

        let mut len_bytes = [0u8; LENGTH_PREFIX_SIZE];
        server_in.read_exact(&mut len_bytes).await.unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut body = vec![0u8; len];
        server_in.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, msg);

        let reply = b"fdomain reply";
        let mut reply_bytes = (reply.len() as u32).to_le_bytes().to_vec();
        reply_bytes.extend_from_slice(reply);
        server_out.write_all(&reply_bytes).await.unwrap();

        let received = transport.next().await.unwrap().unwrap();
        assert_eq!(&*received, reply);
    }
}
