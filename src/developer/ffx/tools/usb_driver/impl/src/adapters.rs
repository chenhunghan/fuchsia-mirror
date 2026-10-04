// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_metrics::{UsbDriverConnectionEvent, UsbProtocolCounts};
use futures::channel::mpsc;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Instant;
use tokio::io::ReadBuf;
use tokio::net::UnixStream;

/// Lock-free atomic counters for USB driver FIDL protocol methods and events.
#[derive(Debug, Default)]
pub struct AtomicUsbProtocolCounts {
    initialize_control: AtomicU64,
    initialize_list_devices: AtomicU64,
    initialize_connect_to: AtomicU64,
    initialize_accept: AtomicU64,
    listen: AtomicU64,
    stop_listen: AtomicU64,
    reject: AtomicU64,
    on_incoming: AtomicU64,
    on_device_appeared: AtomicU64,
    on_device_disappeared: AtomicU64,
}

impl AtomicUsbProtocolCounts {
    pub fn record_initialize_control(&self) {
        self.initialize_control.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_initialize_list_devices(&self) {
        self.initialize_list_devices.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_initialize_connect_to(&self) {
        self.initialize_connect_to.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_initialize_accept(&self) {
        self.initialize_accept.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_listen(&self) {
        self.listen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stop_listen(&self) {
        self.stop_listen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_reject(&self) {
        self.reject.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_on_incoming(&self) {
        self.on_incoming.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_on_device_appeared(&self) {
        self.on_device_appeared.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_on_device_disappeared(&self) {
        self.on_device_disappeared.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> UsbProtocolCounts {
        UsbProtocolCounts {
            initialize_control: self.initialize_control.load(Ordering::Relaxed),
            initialize_list_devices: self.initialize_list_devices.load(Ordering::Relaxed),
            initialize_connect_to: self.initialize_connect_to.load(Ordering::Relaxed),
            initialize_accept: self.initialize_accept.load(Ordering::Relaxed),
            listen: self.listen.load(Ordering::Relaxed),
            stop_listen: self.stop_listen.load(Ordering::Relaxed),
            reject: self.reject.load(Ordering::Relaxed),
            on_incoming: self.on_incoming.load(Ordering::Relaxed),
            on_device_appeared: self.on_device_appeared.load(Ordering::Relaxed),
            on_device_disappeared: self.on_device_disappeared.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct ConnectionTelemetryTracker {
    connected_at: Instant,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    protocol_counts: Arc<AtomicUsbProtocolCounts>,
    on_close: mpsc::UnboundedSender<UsbDriverConnectionEvent>,
}

/// Adapter to cope with the difference between Tokio's `AsyncRead/Write` and
/// the `futures` crate's `AsyncRead/Write`, while tracking full-lifetime
/// socket connection duration, transferred bytes, and FIDL protocol counts.
#[derive(Debug)]
pub struct WrapStream {
    stream: UnixStream,
    tracker: Option<ConnectionTelemetryTracker>,
}

impl WrapStream {
    /// Creates a tracked [`WrapStream`] for an accepted client socket connection.
    pub fn new_tracked(
        stream: UnixStream,
        on_close: mpsc::UnboundedSender<UsbDriverConnectionEvent>,
    ) -> Self {
        log::debug!("Recorded new USB driver socket connection");
        Self {
            stream,
            tracker: Some(ConnectionTelemetryTracker {
                connected_at: Instant::now(),
                rx_bytes: AtomicU64::new(0),
                tx_bytes: AtomicU64::new(0),
                protocol_counts: Arc::new(AtomicUsbProtocolCounts::default()),
                on_close,
            }),
        }
    }

    /// Returns a shared handle to the per-connection protocol invocation counters,
    /// if this stream is tracked.
    pub fn protocol_counts(&self) -> Option<Arc<AtomicUsbProtocolCounts>> {
        self.tracker.as_ref().map(|t| Arc::clone(&t.protocol_counts))
    }

    fn record_rx_bytes(&self, bytes: usize) {
        if let Some(tracker) = &self.tracker {
            tracker.rx_bytes.fetch_add(u64::try_from(bytes).unwrap_or(0), Ordering::Relaxed);
        }
    }

    fn record_tx_bytes(&self, bytes: usize) {
        if let Some(tracker) = &self.tracker {
            tracker.tx_bytes.fetch_add(u64::try_from(bytes).unwrap_or(0), Ordering::Relaxed);
        }
    }
}

impl From<UnixStream> for WrapStream {
    fn from(stream: UnixStream) -> Self {
        Self { stream, tracker: None }
    }
}

impl Drop for WrapStream {
    fn drop(&mut self) {
        if let Some(tracker) = self.tracker.take() {
            let duration_ms =
                u64::try_from(tracker.connected_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let event = UsbDriverConnectionEvent {
                duration_ms,
                rx_bytes: tracker.rx_bytes.load(Ordering::Relaxed),
                tx_bytes: tracker.tx_bytes.load(Ordering::Relaxed),
                protocol_counts: tracker.protocol_counts.snapshot(),
            };
            let _ = tracker.on_close.unbounded_send(event);
        }
    }
}

impl tokio::io::AsyncRead for WrapStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.stream).poll_read(cx, buf))?;
        let read_len = buf.filled().len().saturating_sub(before);
        self.record_rx_bytes(read_len);
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for WrapStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let written = ready!(Pin::new(&mut self.stream).poll_write(cx, buf))?;
        self.record_tx_bytes(written);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl futures::AsyncRead for WrapStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        ready!(tokio::io::AsyncRead::poll_read(self, cx, &mut read_buf))?;
        Poll::Ready(Ok(read_buf.filled().len()))
    }
}

impl futures::AsyncWrite for WrapStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(self, cx)
    }
}
