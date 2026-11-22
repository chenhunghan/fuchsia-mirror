// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::prelude::*;
use futures::ready;
use futures::task::{Context, Poll};
use netext::TokioAsyncReadExt;
use std::fmt;
use std::io::{Error, ErrorKind, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;
use timeout::timeout;
use tokio::net::TcpStream;

const FB_HANDSHAKE: [u8; 4] = *b"FB01";

/// Size of the header of a Fastboot over TCP message: the length of the rest of the message, as
/// a big-endian u64.
const HEADER_SIZE: usize = 8;

/// Messages up to this size are read in full before `poll_read` returns any of their data, so
/// that a read returns a whole message, e.g. a reply, even if TCP splits it. Fastboot replies are
/// at most 256 bytes.
const MAX_BUFFERED_MESSAGE_SIZE: usize = 4096;

///////////////////////////////////////////////////////////////////////////////
// TcpNetworkInterface
//

/// A Fastboot over TCP connection.
///
/// In TCP mode the Fastboot Protocol expects the first 8 bytes (sizeof u64) of each message to
/// indicate the size in bytes of the rest of the message. `poll_write` sends each buffer it is
/// given as one message, and `poll_read` returns the data of the received messages.
pub struct TcpNetworkInterface<T> {
    stream: T,
    read_state: ReadState,
    /// Holds a message of up to `MAX_BUFFERED_MESSAGE_SIZE` bytes while it is read.
    read_buffer: Box<[u8; MAX_BUFFERED_MESSAGE_SIZE]>,
    /// The message being written, if a write is in progress.
    write_state: Option<WriteState>,
}

/// Progress of reading a message.
#[derive(Debug)]
enum ReadState {
    /// Reading the header of the next message, of which `pos` bytes are read.
    Header { header: [u8; HEADER_SIZE], pos: usize },
    /// Reading a message of `len` bytes into `read_buffer`. `filled` bytes are read and `consumed`
    /// bytes of those are returned.
    Buffered { len: usize, filled: usize, consumed: usize },
    /// Reading a larger message directly into the reader's buffers. `remaining` bytes are left.
    Streamed { remaining: u64 },
}

impl Default for ReadState {
    fn default() -> Self {
        Self::Header { header: [0; HEADER_SIZE], pos: 0 }
    }
}

/// Progress of writing a message.
#[derive(Debug)]
struct WriteState {
    header: [u8; HEADER_SIZE],
    /// The length of the message's data, without the header.
    len: usize,
    /// The number of bytes of the header and data written so far.
    written: usize,
}

impl WriteState {
    fn new(len: usize) -> Self {
        // usize is at most 64 bits wide on all supported hosts.
        Self { header: (len as u64).to_be_bytes(), len, written: 0 }
    }
}

impl<T> TcpNetworkInterface<T> {
    pub fn new(stream: T) -> Self {
        Self {
            stream,
            read_state: ReadState::default(),
            read_buffer: Box::new([0; MAX_BUFFERED_MESSAGE_SIZE]),
            write_state: None,
        }
    }
}

impl<T> fmt::Debug for TcpNetworkInterface<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpNetworkInterface")
            .field("read_state", &self.read_state)
            .field("write_state", &self.write_state)
            .finish()
    }
}

fn unexpected_eof(what: &str) -> Error {
    Error::new(ErrorKind::UnexpectedEof, format!("connection closed in the middle of a {what}"))
}

impl<T> AsyncRead for TcpNetworkInterface<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // All reads from `stream` either return `Pending`, which registers `cx` for a wake-up
        // when more data arrives, or make progress.
        loop {
            match &mut this.read_state {
                ReadState::Header { header, pos } => {
                    while *pos < HEADER_SIZE {
                        let n =
                            ready!(Pin::new(&mut this.stream).poll_read(cx, &mut header[*pos..]))?;
                        if n == 0 {
                            // The peer may only close the connection between messages.
                            return Poll::Ready(match *pos {
                                0 => Ok(0),
                                _ => Err(unexpected_eof("message header")),
                            });
                        }
                        *pos += n;
                    }
                    let len = u64::from_be_bytes(*header);
                    this.read_state = match usize::try_from(len) {
                        // An empty message carries nothing to return, and returning no data would
                        // signal the end of the stream.
                        Ok(0) => ReadState::default(),
                        Ok(len) if len <= MAX_BUFFERED_MESSAGE_SIZE => {
                            ReadState::Buffered { len, filled: 0, consumed: 0 }
                        }
                        _ => ReadState::Streamed { remaining: len },
                    };
                }
                ReadState::Buffered { len, filled, consumed } => {
                    while *filled < *len {
                        let n = ready!(
                            Pin::new(&mut this.stream)
                                .poll_read(cx, &mut this.read_buffer[*filled..*len])
                        )?;
                        if n == 0 {
                            return Poll::Ready(Err(unexpected_eof("message")));
                        }
                        *filled += n;
                    }
                    let n = std::cmp::min(buf.len(), *len - *consumed);
                    buf[..n].copy_from_slice(&this.read_buffer[*consumed..][..n]);
                    *consumed += n;
                    if *consumed == *len {
                        this.read_state = ReadState::default();
                    }
                    return Poll::Ready(Ok(n));
                }
                ReadState::Streamed { remaining } => {
                    let max = usize::try_from(*remaining).map_or(buf.len(), |v| v.min(buf.len()));
                    let n = ready!(Pin::new(&mut this.stream).poll_read(cx, &mut buf[..max]))?;
                    if n == 0 {
                        return Poll::Ready(Err(unexpected_eof("message")));
                    }
                    // `n` is at most `remaining`.
                    *remaining -= n as u64;
                    if *remaining == 0 {
                        this.read_state = ReadState::default();
                    }
                    return Poll::Ready(Ok(n));
                }
            }
        }
    }
}

impl<T> AsyncWrite for TcpNetworkInterface<T>
where
    T: AsyncWrite + Unpin,
{
    /// Sends `buf` as one message.
    ///
    /// Reports the length of `buf` as written, without the header, once the whole message is
    /// written, as `AsyncWrite` operations like `write_all` expect. For example, writing a `16`
    /// byte message sends `24` bytes but returns `16`. If this returns `Pending`, it must be
    /// called again with the same `buf`.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let state = this.write_state.get_or_insert_with(|| WriteState::new(buf.len()));
        if state.len != buf.len() {
            if state.written > 0 {
                return Poll::Ready(Err(Error::new(
                    ErrorKind::InvalidInput,
                    "a write of a different buffer interrupted a partially written message",
                )));
            }
            // A previous write was abandoned before anything was sent: start over with `buf`.
            *state = WriteState::new(buf.len());
        }
        while state.written < HEADER_SIZE + state.len {
            // Returning `Pending` from the stream registers `cx` for a wake-up once the stream
            // can accept more data.
            let n = if state.written < HEADER_SIZE {
                // Sends the header and the data in one write if possible, so that they can share
                // a segment instead of the data waiting for the peer to acknowledge the header.
                let bufs = [IoSlice::new(&state.header[state.written..]), IoSlice::new(buf)];
                ready!(Pin::new(&mut this.stream).poll_write_vectored(cx, &bufs))?
            } else {
                ready!(
                    Pin::new(&mut this.stream).poll_write(cx, &buf[state.written - HEADER_SIZE..])
                )?
            };
            if n == 0 {
                return Poll::Ready(Err(ErrorKind::WriteZero.into()));
            }
            state.written += n;
        }
        this.write_state = None;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_close(cx)
    }
}

async fn handshake<T: AsyncWrite + AsyncRead + std::marker::Unpin>(
    stream: &mut T,
) -> Result<(), crate::FastbootTransportError> {
    stream.write_all(&FB_HANDSHAKE).await.map_err(crate::FastbootTransportError::SendError)?;
    let mut response = [0; 4];
    stream.read_exact(&mut response).await.map_err(crate::FastbootTransportError::RecvError)?;
    if response != FB_HANDSHAKE {
        return Err(crate::FastbootTransportError::InvalidHandshake);
    }
    Ok(())
}

/// Timeout in seconds waiting for a valid fastboot TCP handshake.
pub const HANDSHAKE_TIMEOUT: &str = "fastboot.tcp.handshake.timeout";

pub const RETRY_WAIT_SECONDS: u64 = 5;
const FASTBOOT_PORT: u16 = 5554;
pub const HANDSHAKE_TIMEOUT_MILLIS: u64 = 1000;

pub async fn open_once(
    target: &SocketAddr,
    handshake_timeout: Duration,
) -> Result<
    TcpNetworkInterface<netext::MultithreadedTokioAsyncWrapper<TcpStream>>,
    crate::FastbootTransportError,
> {
    let mut addr: SocketAddr = target.clone();
    if addr.port() == 0 {
        log::debug!("Address does not have port set ({addr:?}. Using default:  {FASTBOOT_PORT}");
        addr.set_port(FASTBOOT_PORT);
    }

    log::debug!("Trying to establish TCP Connection to address: {addr:?}");
    timeout(handshake_timeout, async {
        let stream =
            TcpStream::connect(addr).await.map_err(|e| crate::FastbootTransportError::Io(e))?;
        // Fastboot is request/response, so don't let Nagle's algorithm hold a message until the
        // device acknowledges the previous one, which devices may delay.
        if let Err(e) = stream.set_nodelay(true) {
            log::warn!("Could not set TCP_NODELAY on the connection to {addr:?}: {e}");
        }
        let mut stream = stream.into_multithreaded_futures_stream();
        handshake(&mut stream).await?;
        Ok(TcpNetworkInterface::new(stream))
    })
    .await
    .map_err(|_| crate::FastbootTransportError::Timeout)?
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use futures::task::noop_waker_ref;
    use pretty_assertions::assert_eq;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Wake, Waker};

    #[derive(Clone)]
    struct TestAsyncIo {
        data: Box<[u8]>,
    }

    impl AsyncRead for TestAsyncIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            let len = std::cmp::min(buf.len(), self.data.len());
            buf[..len].copy_from_slice(&self.data[..len]);
            Poll::Ready(Ok(len))
        }
    }

    impl AsyncWrite for TestAsyncIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let len = std::cmp::min(buf.len(), self.data.len());
            self.data[..len].copy_from_slice(&buf[..len]);
            Poll::Ready(Ok(len))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }
    }

    #[derive(Clone)]
    struct TestInnerWriter {
        inner: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for TestInnerWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.inner.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }
    }

    impl AsyncRead for TestInnerWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            unimplemented!();
        }
    }

    /// Accepts at most `max_write` bytes per write, including vectored writes, and records the
    /// size of each write.
    #[derive(Clone)]
    struct TestChunkedWriter {
        inner: Arc<Mutex<Vec<u8>>>,
        writes: Arc<Mutex<Vec<usize>>>,
        max_write: usize,
    }

    impl AsyncWrite for TestChunkedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.poll_write_vectored(cx, &[IoSlice::new(buf)])
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            let mut inner = self.inner.lock().unwrap();
            let mut len = 0;
            for buf in bufs {
                let n = std::cmp::min(buf.len(), self.max_write - len);
                inner.extend_from_slice(&buf[..n]);
                len += n;
            }
            self.writes.lock().unwrap().push(len);
            Poll::Ready(Ok(len))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!();
        }
    }

    /// Returns `Pending` for the first `pending` writes without arranging a wake-up, then accepts
    /// everything.
    struct TestPendingWriter {
        pending: usize,
        inner: Vec<u8>,
    }

    impl AsyncWrite for TestPendingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.pending > 0 {
                self.pending -= 1;
                return Poll::Pending;
            }
            self.inner.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Counts its wake-ups.
    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns `data` at most `max_read` bytes per read, then the end of the stream.
    struct TestReader {
        data: Vec<u8>,
        pos: usize,
        max_read: usize,
    }

    impl TestReader {
        fn new(data: Vec<u8>, max_read: usize) -> Self {
            Self { data, pos: 0, max_read }
        }
    }

    impl AsyncRead for TestReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            let len = buf.len().min(self.max_read).min(self.data.len() - self.pos);
            buf[..len].copy_from_slice(&self.data[self.pos..][..len]);
            self.pos += len;
            Poll::Ready(Ok(len))
        }
    }

    /// Returns `data` framed as a Fastboot over TCP message.
    fn message(data: &[u8]) -> Vec<u8> {
        [&(data.len() as u64).to_be_bytes()[..], data].concat()
    }

    #[fuchsia::test]
    async fn test_async_write_sends_header_with_data() -> std::io::Result<()> {
        let inner = Arc::new(Mutex::new(vec![]));
        let writes = Arc::new(Mutex::new(vec![]));
        let stream =
            TestChunkedWriter { inner: inner.clone(), writes: writes.clone(), max_write: 64 };
        let mut interface = TcpNetworkInterface::new(stream);

        assert_eq!(interface.write(&[0, 1, 2]).await?, 3, "reported length");
        assert_eq!(*writes.lock().unwrap(), vec![11], "one write for header and data");
        assert_eq!(
            *inner.lock().unwrap(),
            vec![0, 0, 0, 0, 0, 0, 0, 3, 0, 1, 2],
            "stream contents"
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_async_write_partial_writes() -> std::io::Result<()> {
        let inner = Arc::new(Mutex::new(vec![]));
        let writes = Arc::new(Mutex::new(vec![]));
        let stream =
            TestChunkedWriter { inner: inner.clone(), writes: writes.clone(), max_write: 4 };
        let mut interface = TcpNetworkInterface::new(stream);

        assert_eq!(interface.write(&[0, 1, 2]).await?, 3, "reported length");
        assert_eq!(*writes.lock().unwrap(), vec![4, 4, 3], "writes");
        assert_eq!(
            *inner.lock().unwrap(),
            vec![0, 0, 0, 0, 0, 0, 0, 3, 0, 1, 2],
            "stream contents"
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_async_write_includes_header() -> Result<()> {
        let inner = Arc::new(Mutex::new(vec![]));
        let stream = TestInnerWriter { inner: inner.clone() };
        let mut interface = TcpNetworkInterface::new(stream);

        let bytes = vec![0, 1, 2];
        interface.write_all(&bytes).await?;
        assert_eq!(
            *inner.lock().unwrap(),
            vec![0, 0, 0, 0, 0, 0, 0, 3, 0, 1, 2],
            "stream contents"
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_async_write_consecutive_messages() -> Result<()> {
        let inner = Arc::new(Mutex::new(vec![]));
        let writes = Arc::new(Mutex::new(vec![]));
        let stream =
            TestChunkedWriter { inner: inner.clone(), writes: writes.clone(), max_write: 5 };
        let mut interface = TcpNetworkInterface::new(stream);

        interface.write_all(b"OKAY").await?;
        interface.write_all(b"DATA").await?;
        assert_eq!(*inner.lock().unwrap(), [message(b"OKAY"), message(b"DATA")].concat());
        Ok(())
    }

    #[fuchsia::test]
    fn test_async_write_empty_buffer_writes_nothing() {
        let inner = Arc::new(Mutex::new(vec![]));
        let mut interface = TcpNetworkInterface::new(TestInnerWriter { inner: inner.clone() });
        let mut cx = Context::from_waker(noop_waker_ref());
        let res = Pin::new(&mut interface).poll_write(&mut cx, &[]);
        assert!(matches!(res, Poll::Ready(Ok(0))));
        assert!(inner.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn test_async_write_pending_does_not_wake_itself() {
        let mut interface =
            TcpNetworkInterface::new(TestPendingWriter { pending: 1, inner: vec![] });
        let counter = Arc::new(CountingWaker::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        // A full stream is the stream's to wake up for: waking right away would busy-wait.
        assert!(Pin::new(&mut interface).poll_write(&mut cx, &[1, 2]).is_pending());
        assert_eq!(counter.0.load(Ordering::Relaxed), 0, "wake-ups");

        let res = Pin::new(&mut interface).poll_write(&mut cx, &[1, 2]);
        assert!(matches!(res, Poll::Ready(Ok(2))));
        assert_eq!(interface.stream.inner, message(&[1, 2]));
    }

    #[fuchsia::test]
    fn test_async_write_restarts_abandoned_message() {
        let mut interface =
            TcpNetworkInterface::new(TestPendingWriter { pending: 1, inner: vec![] });
        let mut cx = Context::from_waker(noop_waker_ref());

        // Nothing of the first message was sent, so a write of another buffer replaces it.
        assert!(Pin::new(&mut interface).poll_write(&mut cx, &[1, 2]).is_pending());
        let res = Pin::new(&mut interface).poll_write(&mut cx, &[3, 4, 5]);
        assert!(matches!(res, Poll::Ready(Ok(3))));
        assert_eq!(interface.stream.inner, message(&[3, 4, 5]));
    }

    #[fuchsia::test]
    async fn test_read_wrapper() -> Result<()> {
        let msg = "O, that this too too solid flesh would melt";
        let mut data = (msg.as_bytes().len() as u64).to_be_bytes().to_vec();
        data.extend(msg.as_bytes());
        let data = data.into_boxed_slice();
        let stream = TestAsyncIo { data };
        let mut interface = TcpNetworkInterface::new(stream);

        let mut buf = [0u8; 64];
        let len = interface.read(&mut buf).await?;
        assert_eq!(interface.stream.data[0..8], (len as u64).to_be_bytes());
        assert_eq!(msg.as_bytes().len(), len);
        assert_eq!(&interface.stream.data[..len], &buf[..len]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read_returns_whole_messages_split_by_the_stream() -> Result<()> {
        let data = [message(b"OKAY"), message(b"INFOhello")].concat();
        let mut interface = TcpNetworkInterface::new(TestReader::new(data, 3));

        let mut buf = [0u8; 64];
        let len = interface.read(&mut buf).await?;
        assert_eq!(&buf[..len], b"OKAY");
        let len = interface.read(&mut buf).await?;
        assert_eq!(&buf[..len], b"INFOhello");
        assert_eq!(interface.read(&mut buf).await?, 0, "end of stream");
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read_message_larger_than_buffer() -> Result<()> {
        let data = [message(b"0123456789"), message(b"OKAY")].concat();
        let mut interface = TcpNetworkInterface::new(TestReader::new(data, 64));

        let mut buf = [0u8; 4];
        let mut reads = vec![];
        for _ in 0..4 {
            let len = interface.read(&mut buf).await?;
            reads.push(buf[..len].to_vec());
        }
        assert_eq!(reads, [&b"0123"[..], b"4567", b"89", b"OKAY"]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read_streams_large_message() -> Result<()> {
        let payload: Vec<u8> = (0..3 * MAX_BUFFERED_MESSAGE_SIZE).map(|v| v as u8).collect();
        let data = [message(&payload), message(b"OKAY")].concat();
        let mut interface = TcpNetworkInterface::new(TestReader::new(data, 1000));

        let mut received = vec![];
        let mut buf = vec![0u8; 1500];
        while received.len() < payload.len() {
            let len = interface.read(&mut buf).await?;
            assert!(len > 0);
            received.extend_from_slice(&buf[..len]);
        }
        assert_eq!(received, payload);
        let len = interface.read(&mut buf).await?;
        assert_eq!(&buf[..len], b"OKAY");
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read_skips_empty_messages() -> Result<()> {
        let data = [message(b""), message(b"OKAY")].concat();
        let mut interface = TcpNetworkInterface::new(TestReader::new(data, 64));

        let mut buf = [0u8; 64];
        let len = interface.read(&mut buf).await?;
        assert_eq!(&buf[..len], b"OKAY");
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read_eof_inside_message_header() {
        let mut interface = TcpNetworkInterface::new(TestReader::new(vec![0, 0, 0], 64));

        let mut buf = [0u8; 64];
        let err = interface.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[fuchsia::test]
    async fn test_read_eof_inside_message() {
        let mut data = message(b"OKAY");
        data.truncate(10);
        let mut interface = TcpNetworkInterface::new(TestReader::new(data, 64));

        let mut buf = [0u8; 64];
        let err = interface.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }
}
