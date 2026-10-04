// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::channel::mpsc;
use futures::sink::Sink;
use futures::stream::Stream;
use futures::{AsyncRead, AsyncWrite};
use std::io::{self, Cursor, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

/// A bidirectional in-memory byte transport backed by `futures::channel::mpsc` channels.
#[derive(Debug)]
pub struct MpscTransport {
    sender: mpsc::Sender<Vec<u8>>,
    receiver: mpsc::Receiver<Vec<u8>>,
    current_read_buf: Option<Cursor<Vec<u8>>>,
}

impl MpscTransport {
    /// Creates a pair of connected [`MpscTransport`] endpoints with a default capacity of 128 chunks.
    pub fn pair() -> (Self, Self) {
        Self::pair_with_capacity(128)
    }

    /// Creates a pair of connected [`MpscTransport`] endpoints with the given channel capacity.
    pub fn pair_with_capacity(capacity: usize) -> (Self, Self) {
        let (tx_a, rx_b) = mpsc::channel(capacity);
        let (tx_b, rx_a) = mpsc::channel(capacity);
        (
            Self { sender: tx_a, receiver: rx_a, current_read_buf: None },
            Self { sender: tx_b, receiver: rx_b, current_read_buf: None },
        )
    }

    /// Creates a new [`MpscTransport`] from a sender and receiver.
    pub fn new(sender: mpsc::Sender<Vec<u8>>, receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { sender, receiver, current_read_buf: None }
    }
}

impl AsyncRead for MpscTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = &mut *self;
        loop {
            if let Some(cursor) = &mut this.current_read_buf {
                let n = match cursor.read(buf) {
                    Ok(n) => n,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                if n > 0 {
                    if cursor.position() >= cursor.get_ref().len() as u64 {
                        this.current_read_buf = None;
                    }
                    return Poll::Ready(Ok(n));
                }
                this.current_read_buf = None;
            }

            match Pin::new(&mut this.receiver).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    this.current_read_buf = Some(Cursor::new(chunk));
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Ok(0));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

impl AsyncWrite for MpscTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = &mut *self;
        match Pin::new(&mut this.sender).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                let len = buf.len();
                match Pin::new(&mut this.sender).start_send(buf.to_vec()) {
                    Ok(()) => Poll::Ready(Ok(len)),
                    Err(e) if e.is_disconnected() => {
                        Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
                    }
                    Err(e) => Poll::Ready(Err(io::Error::other(e))),
                }
            }
            Poll::Ready(Err(e)) => {
                if e.is_disconnected() {
                    Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
                } else {
                    Poll::Ready(Err(io::Error::other(e)))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        match Pin::new(&mut this.sender).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                if e.is_disconnected() {
                    Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
                } else {
                    Poll::Ready(Err(io::Error::other(e)))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        match Pin::new(&mut this.sender).poll_close(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                if e.is_disconnected() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(io::Error::other(e)))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{AsyncReadExt, AsyncWriteExt};

    #[fuchsia::test]
    async fn test_basic_transfer() {
        let (mut a, mut b) = MpscTransport::pair();
        a.write_all(b"hello world").await.unwrap();

        let mut buf = [0u8; 11];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[fuchsia::test]
    async fn test_fragmented_read() {
        let (mut a, mut b) = MpscTransport::pair();
        a.write_all(b"1234567890").await.unwrap();

        let mut buf1 = [0u8; 4];
        let mut buf2 = [0u8; 6];
        b.read_exact(&mut buf1).await.unwrap();
        b.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf1, b"1234");
        assert_eq!(&buf2, b"567890");
    }

    #[fuchsia::test]
    async fn test_eof_on_drop() {
        let (a, mut b) = MpscTransport::pair();
        drop(a);

        let mut buf = [0u8; 10];
        let n = b.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
    }

    #[fuchsia::test]
    async fn test_broken_pipe_on_write() {
        let (mut a, b) = MpscTransport::pair();
        drop(b);

        let err = a.write_all(b"fail").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
