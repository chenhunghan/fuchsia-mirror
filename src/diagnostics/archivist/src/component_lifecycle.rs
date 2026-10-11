// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::RequestStream;
use fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream};
use fuchsia_async as fasync;
use fuchsia_runtime::{HandleInfo, HandleType, take_startup_handle};
use futures::{Future, StreamExt};
use log::{debug, warn};

/// Takes the startup handle for LIFECYCLE and returns a stream listening for Lifecycle FIDL
/// requests on it.
pub fn take_lifecycle_request_stream() -> LifecycleRequestStream {
    let lifecycle_handle_info = HandleInfo::new(HandleType::Lifecycle, 0);
    let lifecycle_handle = take_startup_handle(lifecycle_handle_info)
        .expect("must have been provided a lifecycle channel in procargs");
    let async_chan = fasync::Channel::from_channel(lifecycle_handle.into());
    LifecycleRequestStream::from_channel(async_chan)
}

/// Serves the Lifecycle protocol from the component runtime used for controlled shutdown of the
/// archivist. When Stop is requrested executes the given callback.
pub async fn on_stop_request<F, Fut>(mut request_stream: LifecycleRequestStream, cb: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    match request_stream.next().await {
        None => {
            warn!("Lifecycle closed");
        }
        Some(Err(err)) => {
            warn!(err:?; "Lifecycle error");
        }
        Some(Ok(LifecycleRequest::Stop { .. })) => {
            debug!("Initiating shutdown.");
            cb().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{Proxy, create_proxy_and_stream};
    use fidl_fuchsia_process_lifecycle::LifecycleMarker;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[fuchsia::test]
    async fn on_stop_request_invokes_callback() {
        let (proxy, stream) = create_proxy_and_stream::<LifecycleMarker>();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        let stop_fut = on_stop_request(stream, move || async move {
            called_clone.store(true, Ordering::Relaxed);
        });

        proxy.stop().unwrap();
        stop_fut.await;

        assert!(called.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn on_stop_request_channel_closed() {
        let (proxy, stream) = create_proxy_and_stream::<LifecycleMarker>();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        let stop_fut = on_stop_request(stream, move || async move {
            called_clone.store(true, Ordering::Relaxed);
        });

        drop(proxy);
        stop_fut.await;

        assert!(!called.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn on_stop_request_stream_error() {
        let (proxy, stream) = create_proxy_and_stream::<LifecycleMarker>();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        // Send invalid FIDL message
        proxy.as_channel().write(&[0xff; 16], &mut []).expect("write invalid bytes");

        on_stop_request(stream, move || async move {
            called_clone.store(true, Ordering::Relaxed);
        })
        .await;

        assert!(!called.load(Ordering::Relaxed));
    }
}
