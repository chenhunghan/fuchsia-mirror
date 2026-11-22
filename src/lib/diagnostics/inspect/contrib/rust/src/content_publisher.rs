// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_inspect::{
    InspectSinkMarker, InspectSinkPublishRequest, TreeContent, TreeGetContentResponder, TreeMarker,
    TreeRequest, TreeRequestStream,
};
use fuchsia_inspect::Inspector;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

/// Configuration options for the content publisher.
pub struct PublishOptions {
    /// Channel over which the InspectSink protocol will be used.
    pub inspect_sink_client: ClientEnd<InspectSinkMarker>,
}

/// A responder for a GetContent request.
pub struct ContentResponder {
    responder: TreeGetContentResponder,
}

impl ContentResponder {
    /// Responds to the GetContent request with the VMO from the provided Inspector.
    /// The Inspector is dropped after sending, freeing its resources.
    pub fn send(self, inspector: Inspector) -> Result<(), Error> {
        let vmo = inspector.frozen_vmo_copy().context("failed to copy vmo")?;
        let size = vmo.get_size().context("failed to get vmo size")?;
        let content = TreeContent {
            buffer: Some(fidl_fuchsia_mem::Buffer { vmo, size }),
            ..Default::default()
        };

        self.responder.send(content).context("failed to send response")?;
        Ok(())
    }
}

/// A stream of incoming GetContent requests.
pub struct ContentPublisher {
    stream: TreeRequestStream,
}

impl Stream for ContentPublisher {
    type Item = ContentResponder;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.stream.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(TreeRequest::GetContent { responder }))) => {
                    return Poll::Ready(Some(ContentResponder { responder }));
                }
                #[cfg(fuchsia_api_level_at_least = "HEAD")]
                Poll::Ready(Some(Ok(TreeRequest::OpenBlobChild { responder, .. }))) => {
                    // `ContentPublisher` does not support `BlobProperty`; reply `NOT_FOUND` rather
                    // than dropping `responder` (whose `Drop` impl shuts down the `Tree` channel).
                    let _ = responder.send(Err(zx::Status::NOT_FOUND.into_raw()));
                    continue;
                }
                Poll::Ready(Some(Ok(_))) => {
                    // Ignore other requests (e.g. ListChildNames, OpenChild, ListBlobChildNames).
                    // This is a minimal server that only supports GetContent.
                    continue;
                }
                Poll::Ready(Some(Err(_))) => {
                    // Stream error, terminate.
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Publishes a minimal Inspect Tree server that yields responders for data requests.
///
/// This can be thought of like a lazy node, but for the root.
///
/// - Lazy nodes and `BlobProperty` values are _not supported_ in Inspectors exposed this way.
///
/// Usage:
///
/// ```rust
/// let mut publisher = content_publisher(options).expect("failed to create publisher");
/// let mut val = 0;
/// while let Some(responder) = publisher.next().await {
///     // Generate a fresh inspector for each request
///     let inspector = Inspector::default();
///     inspector.root().record_int("dynamic_data", val);
///     inspector.root().record_string("status", "ok");
///
///     // Send the generated VMO
///     responder.send(inspector).expect("failed to send inspector");
///     val += 1
/// }
/// ```
pub fn content_publisher(options: PublishOptions) -> Result<ContentPublisher, Error> {
    let (tree_client, tree_stream) = fidl::endpoints::create_request_stream::<TreeMarker>();

    let inspect_sink = options.inspect_sink_client.into_proxy();

    inspect_sink
        .publish(InspectSinkPublishRequest { tree: Some(tree_client), ..Default::default() })
        .context("failed to publish tree")?;

    Ok(ContentPublisher { stream: tree_stream })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fidl_fuchsia_inspect::InspectSinkRequest;
    use fuchsia_async as fasync;
    use fuchsia_inspect::reader::read;
    use futures::StreamExt;

    #[fuchsia::test]
    async fn test_content_publisher_usage() {
        let (sink_client, mut sink_stream) =
            fidl::endpoints::create_request_stream::<InspectSinkMarker>();

        let options = PublishOptions { inspect_sink_client: sink_client };

        let mut publisher = content_publisher(options).expect("failed to create publisher");

        let tree_proxy = match sink_stream.next().await {
            Some(Ok(InspectSinkRequest::Publish { payload, .. })) => {
                payload.tree.expect("tree channel missing").into_proxy()
            }
            _ => panic!("Expected Publish request"),
        };

        // Start a background task representing the user's workload that generates inspect data
        // on demand.
        let _task = fasync::Task::spawn(async move {
            let mut val = 0;
            while let Some(responder) = publisher.next().await {
                // Generate a fresh inspector for each request
                let inspector = Inspector::default();
                inspector.root().record_int("dynamic_data", val);
                inspector.root().record_string("status", "ok");

                // Send the generated VMO
                responder.send(inspector).expect("failed to send inspector");
                val += 1
            }
        });

        // Read the tree, simulating a snapshot by an Archivist
        let hierarchy = read(&tree_proxy).await.expect("failed to read from tree proxy");

        // Verify the dynamic data was successfully retrieved
        assert_data_tree!(hierarchy, root: {
            dynamic_data: 0i64,
            status: "ok",
        });

        // Read the tree, simulating a snapshot by an Archivist
        let hierarchy = read(&tree_proxy).await.expect("failed to read from tree proxy");

        // Verify the dynamic data was successfully retrieved
        assert_data_tree!(hierarchy, root: {
            dynamic_data: 1i64,
            status: "ok",
        });
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fuchsia::test]
    async fn test_content_publisher_open_blob_child_not_found() {
        let (sink_client, mut sink_stream) =
            fidl::endpoints::create_request_stream::<InspectSinkMarker>();

        let mut publisher =
            content_publisher(PublishOptions { inspect_sink_client: sink_client }).unwrap();

        let tree_proxy = match sink_stream.next().await {
            Some(Ok(InspectSinkRequest::Publish { payload, .. })) => {
                payload.tree.expect("tree channel missing").into_proxy()
            }
            _ => panic!("Expected Publish request"),
        };

        let _task = fasync::Task::spawn(async move {
            while let Some(responder) = publisher.next().await {
                let inspector = Inspector::default();
                inspector.root().record_string("status", "ok");
                responder.send(inspector).expect("failed to send inspector");
            }
        });

        // `OpenBlobChild` replies with `NOT_FOUND` rather than dropping the responder and closing
        // the `Tree` channel, so subsequent `GetContent` reads continue to succeed.
        assert_eq!(
            tree_proxy.open_blob_child("events-0").await.expect("open_blob_child FIDL call"),
            Err(zx::Status::NOT_FOUND.into_raw())
        );

        let hierarchy = read(&tree_proxy).await.expect("tree channel remained open");
        assert_data_tree!(hierarchy, root: {
            status: "ok",
        });
    }
}
