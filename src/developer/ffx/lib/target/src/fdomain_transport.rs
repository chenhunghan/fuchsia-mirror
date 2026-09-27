// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncBufRead, AsyncWrite};

/// Implements a transport for the FDomain client library on top of some async IO readers.
pub struct FDomainTransport {
    input: Pin<Box<dyn AsyncBufRead + Unpin + Send>>,
    output: Pin<Box<dyn AsyncWrite + Unpin + Send>>,
    write_progress: usize,
    in_buf: Vec<u8>,
    in_buf_offset: usize,
}

impl FDomainTransport {
    /// construct a new `FDomainTransport`.
    pub fn new(
        input: Box<dyn AsyncBufRead + Unpin + Send>,
        output: Box<dyn AsyncWrite + Unpin + Send>,
    ) -> Self {
        FDomainTransport {
            input: Pin::new(input),
            output: Pin::new(output),
            write_progress: 0,
            in_buf: Vec::new(),
            in_buf_offset: 0,
        }
    }
}

impl Drop for FDomainTransport {
    fn drop(&mut self) {
        log::debug!("FDomain connection being dropped");
    }
}

impl fdomain_client::FDomainTransport for FDomainTransport {
    fn poll_send_message(
        mut self: Pin<&mut Self>,
        msg: &[u8],
        ctx: &mut Context<'_>,
    ) -> Poll<Result<(), Option<std::io::Error>>> {
        let size: u32 = msg
            .len()
            .try_into()
            .map_err(|_| std::io::Error::other("Message size exceeded u32 capacity"))?;
        let out_buf = size.to_le_bytes();

        if self.write_progress == 0 {
            let slices = [std::io::IoSlice::new(&out_buf), std::io::IoSlice::new(msg)];
            match self.output.as_mut().poll_write_vectored(ctx, &slices) {
                Poll::Ready(Ok(n)) if n == 4 + msg.len() => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Ok(n)) => {
                    self.write_progress = n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(Some(e))),
                Poll::Pending => return Poll::Pending,
            }
        }

        if self.write_progress < 4 {
            while self.write_progress < 4 {
                let offset = self.write_progress;
                let got = ready!(self.output.as_mut().poll_write(ctx, &out_buf[offset..]))?;
                if got == 0 {
                    if self.write_progress != 0 {
                        log::warn!(
                            "FDomain transport closed while sending message ({} of {} bytes sent)",
                            self.write_progress,
                            out_buf.len() + msg.len()
                        );
                    }
                    return Poll::Ready(Err(None));
                }
                self.write_progress += got;
            }
        }

        while self.write_progress - 4 < msg.len() {
            let offset = self.write_progress - 4;
            let got = ready!(self.output.as_mut().poll_write(ctx, &msg[offset..]))?;
            if got == 0 {
                log::warn!(
                    "FDomain transport closed while sending message ({} of {} bytes sent)",
                    self.write_progress,
                    4 + msg.len()
                );
                return Poll::Ready(Err(None));
            }
            self.write_progress += got;
        }

        self.write_progress = 0;
        Poll::Ready(Ok(()))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FDomainTransport")
    }

    fn has_debug_fmt(&self) -> bool {
        true
    }
}

impl futures::Stream for FDomainTransport {
    type Item = std::io::Result<Box<[u8]>>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let unread = self.in_buf.len() - self.in_buf_offset;
            if unread >= 4 {
                let len: usize = u32::from_le_bytes(
                    self.in_buf[self.in_buf_offset..self.in_buf_offset + 4].try_into().unwrap(),
                )
                .try_into()
                .unwrap();

                if unread >= len + 4 {
                    let packet_start = self.in_buf_offset + 4;
                    let packet_end = packet_start + len;
                    let packet = self.in_buf[packet_start..packet_end].to_vec().into_boxed_slice();
                    self.in_buf_offset = packet_end;
                    if self.in_buf_offset == self.in_buf.len() {
                        self.in_buf.clear();
                        self.in_buf_offset = 0;
                    } else if self.in_buf_offset > 65536 {
                        let offset = self.in_buf_offset;
                        self.in_buf.drain(..offset);
                        self.in_buf_offset = 0;
                    }
                    return Poll::Ready(Some(Ok(packet)));
                }
            }

            let this = &mut *self;

            let buf = ready!(this.input.as_mut().poll_fill_buf(ctx))?;

            if buf.is_empty() {
                match this.in_buf.len() - this.in_buf_offset {
                    0 => log::debug!("FDomain transport closed, ending stream"),
                    n => log::warn!(
                        "FDomain transport closed with incomplete packet of length {n}, ending stream"
                    ),
                };
                return Poll::Ready(None);
            }

            this.in_buf.extend_from_slice(buf);
            let len = buf.len();
            this.input.as_mut().consume(len);
        }
    }
}
