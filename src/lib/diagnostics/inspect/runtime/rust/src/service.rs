// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the `fuchsia.inspect.Tree` protocol server.

use crate::TreeServerSendPreference;
use anyhow::Error;
use fidl_fuchsia_inspect::{
    TreeContent, TreeMarker, TreeNameIteratorRequest, TreeNameIteratorRequestStream, TreeRequest,
    TreeRequestStream,
};
use fidl_fuchsia_mem::Buffer;
use fuchsia_async as fasync;
use fuchsia_inspect::Inspector;
use fuchsia_inspect::reader::ReadableTree;
use futures::{FutureExt, TryFutureExt, TryStreamExt};
use log::warn;
use zx::sys::ZX_CHANNEL_MAX_MSG_BYTES;

/// Runs a server for the `fuchsia.inspect.Tree` protocol. This protocol returns the VMO
/// associated with the given tree on `get_content` and allows to open linked trees (lazy nodes).
#[allow(clippy::manual_async_fn)] // required because of recursion
pub fn handle_request_stream(
    inspector: Inspector,
    settings: TreeServerSendPreference,
    mut stream: TreeRequestStream,
    scope: fasync::Scope,
) -> impl futures::Future<Output = Result<(), Error>> + Send {
    async move {
        while let Some(request) = stream.try_next().await? {
            match request {
                TreeRequest::GetContent { responder } => {
                    // If freezing fails, full snapshot algo needed on live duplicate
                    let vmo = match settings {
                        TreeServerSendPreference::DeepCopy => inspector.copy_vmo(),
                        TreeServerSendPreference::Live => inspector.duplicate_vmo(),
                        TreeServerSendPreference::Frozen { ref on_failure } => {
                            match inspector.frozen_vmo_copy() {
                                Ok(vmo) => Some(vmo),
                                Err(_) => match **on_failure {
                                    TreeServerSendPreference::DeepCopy => inspector.copy_vmo(),
                                    TreeServerSendPreference::Live => inspector.duplicate_vmo(),
                                    _ => None,
                                },
                            }
                        }
                    };

                    let buffer_data =
                        vmo.and_then(|vmo| vmo.get_size().ok().map(|size| (vmo, size)));
                    let content = TreeContent {
                        buffer: buffer_data.map(|data| Buffer { vmo: data.0, size: data.1 }),
                        ..Default::default()
                    };
                    responder.send(content)?;
                }
                TreeRequest::ListChildNames { tree_iterator, .. } => {
                    let request_stream = tree_iterator.into_stream();
                    let inspector = inspector.clone();
                    scope.spawn(async move {
                        inspector
                            .tree_names()
                            .map_err(|e| anyhow::anyhow!("{e:?}"))
                            .and_then(|values| {
                                run_tree_name_iterator_server(values, request_stream)
                            })
                            .map_err(|err| {
                                warn!(err:?; "failed to run tree name iterator server");
                            })
                            .map(|_| {})
                            .await
                    });
                }
                TreeRequest::OpenChild { child_name, tree, .. } => {
                    let nested = scope.new_child_with_name("nested_tree_server");
                    let inspector = inspector.clone();
                    let settings = settings.clone();
                    let stream = tree.into_stream();
                    scope.spawn(async move {
                        inspector
                            .read_tree(&child_name)
                            .map_err(|e| anyhow::anyhow!("{e:?}"))
                            .and_then(|inspector| {
                                handle_request_stream(inspector, settings, stream, nested)
                            })
                            .map_err(|err| {
                                warn!(err:?; "failed to run `fuchsia.inspect.Tree` server");
                            })
                            .map(|_| {})
                            .await
                    });
                }
                #[cfg(fuchsia_api_level_at_least = "HEAD")]
                TreeRequest::ListBlobChildNames { tree_iterator, .. } => {
                    let request_stream = tree_iterator.into_stream();
                    let inspector = inspector.clone();
                    scope.spawn(async move {
                        match inspector.blob_names().await {
                            Ok(values) => {
                                if let Err(err) =
                                    run_tree_name_iterator_server(values, request_stream).await
                                {
                                    warn!(err:?; "failed to run blob tree name iterator server");
                                }
                            }
                            Err(err) => {
                                warn!(err:?; "failed to read blob names");
                            }
                        }
                    });
                }
                #[cfg(fuchsia_api_level_at_least = "HEAD")]
                TreeRequest::OpenBlobChild { child_name, responder } => {
                    let vmo_result = match settings {
                        TreeServerSendPreference::DeepCopy => inspector.copy_blob_vmo(&child_name),
                        TreeServerSendPreference::Live => inspector.duplicate_blob_vmo(&child_name),
                        TreeServerSendPreference::Frozen { ref on_failure } => {
                            match inspector.frozen_blob_vmo_copy(&child_name) {
                                Ok(vmo) => Ok(vmo),
                                Err(err @ fuchsia_inspect::Error::VmoMissing) => Err(err),
                                Err(err) => match **on_failure {
                                    TreeServerSendPreference::DeepCopy => {
                                        inspector.copy_blob_vmo(&child_name)
                                    }
                                    TreeServerSendPreference::Live => {
                                        inspector.duplicate_blob_vmo(&child_name)
                                    }
                                    _ => Err(err),
                                },
                            }
                        }
                    };
                    let response = vmo_result.map_err(|err| match err {
                        fuchsia_inspect::Error::VmoMissing => zx::Status::NOT_FOUND.into_raw(),
                        _ => zx::Status::UNAVAILABLE.into_raw(),
                    });
                    responder.send(response)?;
                }
                TreeRequest::_UnknownMethod { ordinal, method_type, .. } => {
                    warn!(ordinal, method_type:?; "Unknown request");
                }
            }
        }

        scope.join().await;

        Ok(())
    }
}

/// Spawns a server for the `fuchsia.inspect.Tree` protocol. This protocol returns the VMO
/// associated with the given tree on `get_content` and allows to open linked trees (lazy nodes).
///
/// This version of the function accepts a `TreeRequestStream`, making it suitable for the
/// recursive calls performed by the `OpenChild` method on `fuchsia.inspect.Tree`.
/// `spawn_tree_server` is a more ergonomic option for spawning the root tree.
pub fn spawn_tree_server_with_stream(
    inspector: Inspector,
    settings: TreeServerSendPreference,
    stream: TreeRequestStream,
    scope: &fasync::ScopeHandle,
) {
    scope.spawn(
        handle_request_stream(
            inspector,
            settings,
            stream,
            scope.new_child_with_name("tree_server"),
        )
        .map(|e| {
            e.unwrap_or_else(
                |err: Error| warn!(err:?; "failed to run `fuchsia.inspect.Tree` server"),
            );
        }),
    );
}

/// Spawns a `fuchsia.inspect.Tree` server and returns the task handling
/// `fuchsia.inspect.Tree requests and a `ClientEnd` handle to the tree.
pub fn spawn_tree_server(
    inspector: Inspector,
    settings: TreeServerSendPreference,
    scope: &fasync::ScopeHandle,
) -> fidl::endpoints::ClientEnd<TreeMarker> {
    let (tree, server_end) = fidl::endpoints::create_endpoints::<TreeMarker>();
    spawn_tree_server_with_stream(inspector, settings, server_end.into_stream(), scope);
    tree
}

/// Runs a server for the `fuchsia.inspect.TreeNameIterator` protocol. This protocol returns the
/// given list of values by chunks.
async fn run_tree_name_iterator_server(
    values: Vec<String>,
    mut stream: TreeNameIteratorRequestStream,
) -> Result<(), anyhow::Error> {
    let mut values_iter = values.into_iter().peekable();
    while let Some(request) = stream.try_next().await? {
        match request {
            TreeNameIteratorRequest::GetNext { responder } => {
                let mut bytes_used: usize = 32; // Page overhead of message header + vector
                let mut result = vec![];
                loop {
                    match values_iter.peek() {
                        None => break,
                        Some(value) => {
                            let next_bytes = bytes_used
                                + 16 // String overhead
                                + fidl::encoding::round_up_to_align(value.len(), 8);
                            if next_bytes > ZX_CHANNEL_MAX_MSG_BYTES as usize {
                                if result.is_empty() {
                                    warn!("Dropping tree name that exceeds channel message size");
                                    values_iter.next();
                                    continue;
                                }
                                break;
                            }
                            bytes_used = next_bytes;
                            result.push(values_iter.next().unwrap());
                        }
                    }
                }
                if result.is_empty() {
                    responder.send(&[])?;
                    return Ok(());
                }
                responder.send(&result)?;
            }
            TreeNameIteratorRequest::_UnknownMethod { ordinal, method_type, .. } => {
                warn!(ordinal, method_type:?; "Unknown request");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    use assert_matches::assert_matches;
    use diagnostics_assertions::{assert_data_tree, assert_json_diff};
    use fidl_fuchsia_inspect::{TreeNameIteratorMarker, TreeNameIteratorProxy, TreeProxy};
    use fuchsia_async::DurationExt;
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    use fuchsia_inspect::reader::ReaderError;
    use fuchsia_inspect::reader::{DiagnosticsHierarchy, PartialNodeHierarchy, read_with_timeout};
    use std::sync::Arc;

    use futures::FutureExt;
    use std::task::Poll;
    use std::time::Duration;

    /// Spawns a `fuchsia.inspect.Tree` server and returns the task handling
    /// `fuchsia.inspect.Tree` requests and a `TreeProxy` handle to the tree.
    pub fn spawn_server_proxy(
        inspector: Inspector,
        settings: TreeServerSendPreference,
    ) -> (Arc<fasync::Scope>, TreeProxy) {
        let scope = Arc::new(fasync::Scope::new());
        (scope.clone(), spawn_tree_server(inspector, settings, scope.as_handle()).into_proxy())
    }

    #[fuchsia::test]
    async fn get_contents() -> Result<(), Error> {
        let (_server, tree) =
            spawn_server_proxy(test_inspector(), TreeServerSendPreference::default());
        let tree_content = tree.get_content().await?;
        let hierarchy = parse_content(tree_content)?;
        assert_data_tree!(hierarchy, root: {
            a: 1i64,
        });
        Ok(())
    }

    #[fuchsia::test]
    async fn list_child_names() -> Result<(), Error> {
        let (_server, tree) =
            spawn_server_proxy(test_inspector(), TreeServerSendPreference::default());
        let (name_iterator, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        tree.list_child_names(server_end)?;
        verify_iterator(name_iterator, vec!["lazy-0".to_string()]).await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn oversized_tree_name_is_dropped() -> Result<(), Error> {
        let (name_iterator, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        let scope = fasync::Scope::new();
        scope.spawn(async move {
            let oversized = "a".repeat(ZX_CHANNEL_MAX_MSG_BYTES as usize);
            run_tree_name_iterator_server(
                vec![oversized, "normal".to_string()],
                server_end.into_stream(),
            )
            .await
            .unwrap();
        });
        verify_iterator(name_iterator, vec!["normal".to_string()]).await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn open_children() -> Result<(), Error> {
        let (_server, tree) =
            spawn_server_proxy(test_inspector(), TreeServerSendPreference::default());
        let (child_tree, server_end) = fidl::endpoints::create_proxy::<TreeMarker>();
        tree.open_child("lazy-0", server_end)?;
        let tree_content = child_tree.get_content().await?;
        let hierarchy = parse_content(tree_content)?;
        assert_data_tree!(hierarchy, root: {
            b: 2u64,
        });
        let (name_iterator, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        child_tree.list_child_names(server_end)?;
        verify_iterator(name_iterator, vec!["lazy-vals-0".to_string()]).await?;

        let (child_tree_2, server_end) = fidl::endpoints::create_proxy::<TreeMarker>();
        child_tree.open_child("lazy-vals-0", server_end)?;
        let tree_content = child_tree_2.get_content().await?;
        let hierarchy = parse_content(tree_content)?;
        assert_data_tree!(hierarchy, root: {
            c: 3.0,
        });
        let (name_iterator, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        child_tree_2.list_child_names(server_end)?;
        verify_iterator(name_iterator, vec![]).await?;

        Ok(())
    }

    #[fuchsia::test]
    async fn default_snapshots_are_private_on_success() -> Result<(), Error> {
        let inspector = test_inspector();
        let (_server, tree_copy) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::default());
        let tree_content_copy = tree_copy.get_content().await?;

        inspector.root().record_int("new", 6);

        // A tree that copies the vmo doesn't see the new int
        let hierarchy = parse_content(tree_content_copy)?;
        assert_data_tree!(hierarchy, root: {
            a: 1i64,
        });
        Ok(())
    }

    #[fuchsia::test]
    async fn force_live_snapshot() -> Result<(), Error> {
        let inspector = test_inspector();
        let (_server1, tree_cow) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::default());
        let (_server2, tree_live) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::Live);
        let tree_content_live = tree_live.get_content().await?;
        let tree_content_cow = tree_cow.get_content().await?;

        inspector.root().record_int("new", 6);

        // A tree that cow's the vmo doesn't see the new int
        let hierarchy = parse_content(tree_content_cow)?;
        assert_data_tree!(hierarchy, root: {
            a: 1i64,
        });

        // A tree that live-duplicates the vmo sees the new int
        let hierarchy = parse_content(tree_content_live)?;
        assert_data_tree!(hierarchy, root: {
            a: 1i64,
            new: 6i64,
        });
        Ok(())
    }

    #[fuchsia::test]
    async fn read_hanging_lazy_node() -> Result<(), Error> {
        let inspector = Inspector::default();
        let root = inspector.root();
        root.record_string("child", "value");

        root.record_lazy_values("lazy-node-always-hangs", || {
            async move {
                fuchsia_async::Timer::new(zx::MonotonicDuration::from_minutes(30).after_now())
                    .await;
                Ok(Inspector::default())
            }
            .boxed()
        });

        root.record_lazy_values("lazy-node-doesnt-hang", || {
            async move {
                let inspector = Inspector::default();
                inspector.root().record_string("from", "non-hanging");
                Ok(inspector)
            }
            .boxed()
        });

        root.record_int("int", 3);

        let instrumentation = Inspector::default();
        let counter = instrumentation.root().create_uint("counter", 0);

        let (_server, proxy) = spawn_server_proxy(inspector, TreeServerSendPreference::default());
        let result = read_with_timeout(&proxy, Duration::from_secs(5), &counter).await?;

        assert_json_diff!(instrumentation, root: { counter: 1 });

        assert_json_diff!(result, root: {
            child: "value",
            int: 3i64,
            from: "non-hanging",
        });

        Ok(())
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn multiple_hanging_lazy_nodes_time_out_concurrently() {
        let inspector = Inspector::default();
        let root = inspector.root();
        root.record_int("int", 3);

        for i in 0..3 {
            root.record_lazy_values(format!("lazy-node-always-hangs-{i}"), || {
                async move {
                    futures::future::pending::<()>().await;
                    Ok(Inspector::default())
                }
                .boxed()
            });
        }

        root.record_lazy_values("lazy-node-doesnt-hang", || {
            async move {
                let inspector = Inspector::default();
                inspector.root().record_string("from", "non-hanging");
                Ok(inspector)
            }
            .boxed()
        });

        root.record_lazy_child("healthy", || {
            async move {
                let inspector = Inspector::default();
                inspector.root().record_string("status", "ok");
                inspector.root().record_lazy_values("nested-lazy-node-always-hangs", || {
                    async move {
                        futures::future::pending::<()>().await;
                        Ok(Inspector::default())
                    }
                    .boxed()
                });
                Ok(inspector)
            }
            .boxed()
        });

        let instrumentation = Inspector::default();
        let counter = instrumentation.root().create_uint("counter", 0);

        let (_server, proxy) = spawn_server_proxy(inspector, TreeServerSendPreference::default());
        let mut read = Box::pin(read_with_timeout(&proxy, Duration::from_secs(5), &counter));

        // Without fake time advancing, the hanging lazy nodes keep the read pending.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut read).await.is_pending());
        assert_json_diff!(instrumentation, root: { counter: 0 });

        // A single timeout period is enough for every hanging lazy node to time out, including
        // the nested one, since they are all read concurrently.
        fasync::TestExecutor::advance_to(fasync::MonotonicInstant::after(
            zx::MonotonicDuration::from_seconds(5),
        ))
        .await;
        let result = match fasync::TestExecutor::poll_until_stalled(&mut read).await {
            Poll::Ready(result) => result.expect("read hierarchy"),
            Poll::Pending => panic!("read should complete after a single timeout period"),
        };

        assert_json_diff!(instrumentation, root: { counter: 4 });
        assert_json_diff!(result, root: {
            int: 3i64,
            from: "non-hanging",
            healthy: {
                status: "ok",
            },
        });
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fuchsia::test]
    async fn list_blob_child_names() -> Result<(), Error> {
        let inspector = test_inspector();
        inspector.root().record_blob_property("events", 16, |blob| {
            blob.write_at(0, b"hello").unwrap();
        });

        let (_server, tree) = spawn_server_proxy(inspector, TreeServerSendPreference::default());

        // Lazy children and blob children are listed in their respective iterators.
        let (child_iter, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        tree.list_child_names(server_end)?;
        verify_iterator(child_iter, vec!["lazy-0".to_string()]).await?;

        let (blob_iter, server_end) = fidl::endpoints::create_proxy::<TreeNameIteratorMarker>();
        tree.list_blob_child_names(server_end)?;
        verify_iterator(blob_iter, vec!["events-1".to_string()]).await?;

        assert_eq!(tree.blob_names().await?, vec!["events-1".to_string()]);
        Ok(())
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fuchsia::test]
    async fn open_blob_child_send_preferences() -> Result<(), Error> {
        let inspector = Inspector::default();
        let blob = inspector.root().create_blob_property("events", 16);
        blob.write_at(0, b"initial_payload!").unwrap();

        let (_s1, tree_cow) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::default());
        let (_s2, tree_copy) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::DeepCopy);
        let (_s3, tree_live) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::Live);

        let cow_vmo = tree_cow.read_blob("events-0").await?;
        let copy_vmo = tree_copy.open_blob_child("events-0").await?.expect("open_blob_child copy");
        let live_vmo = tree_live.open_blob_child("events-0").await?.expect("open_blob_child live");

        for vmo in [&cow_vmo, &copy_vmo, &live_vmo] {
            assert!(!vmo.basic_info()?.rights.contains(zx::Rights::WRITE));
        }

        // Mutate the blob after opening the VMO handles.
        blob.write_at(0, b"updated_payload!").unwrap();

        let mut payload = [0u8; 16];
        cow_vmo.read(&mut payload, 32)?;
        assert_eq!(&payload, b"initial_payload!");

        copy_vmo.read(&mut payload, 32)?;
        assert_eq!(&payload, b"initial_payload!");

        live_vmo.read(&mut payload, 32)?;
        assert_eq!(&payload, b"updated_payload!");

        Ok(())
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fuchsia::test]
    async fn open_blob_child_not_found() -> Result<(), Error> {
        let inspector = Inspector::default();
        let (_s1, tree_cow) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::default());
        let (_s2, tree_copy) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::DeepCopy);
        let (_s3, tree_live) = spawn_server_proxy(inspector, TreeServerSendPreference::Live);

        for tree in [&tree_cow, &tree_copy, &tree_live] {
            assert_eq!(
                tree.open_blob_child("missing-0").await?,
                Err(zx::Status::NOT_FOUND.into_raw())
            );
        }

        assert_matches!(
            tree_cow.read_blob("missing-0").await,
            Err(ReaderError::FailedToLoadBlob(name)) if name == "missing-0"
        );

        Ok(())
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fuchsia::test]
    async fn open_blob_child_unavailable_when_contended() -> Result<(), Error> {
        let inspector = Inspector::default();
        let blob = inspector.root().create_blob_property("events", 16);

        let (_s1, tree_cow) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::default());
        let (_s2, tree_copy) =
            spawn_server_proxy(inspector.clone(), TreeServerSendPreference::DeepCopy);
        let (_s3, tree_live) = spawn_server_proxy(inspector, TreeServerSendPreference::Live);

        let (locked_tx, locked_rx) = futures::channel::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer_thread = std::thread::spawn(move || {
            let _ = blob.atomic_write(|_| {
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            blob
        });
        locked_rx.await?;

        for tree in [&tree_cow, &tree_copy, &tree_live] {
            assert_eq!(
                tree.open_blob_child("events-0").await?,
                Err(zx::Status::UNAVAILABLE.into_raw())
            );
        }

        assert_matches!(
            tree_cow.read_blob("events-0").await,
            Err(ReaderError::Vmo(zx::Status::UNAVAILABLE))
        );

        release_tx.send(())?;
        let _blob = writer_thread.join().expect("writer thread panicked");

        Ok(())
    }

    async fn verify_iterator(
        name_iterator: TreeNameIteratorProxy,
        values: Vec<String>,
    ) -> Result<(), Error> {
        if !values.is_empty() {
            assert_eq!(name_iterator.get_next().await?, values);
        }
        assert!(name_iterator.get_next().await?.is_empty());
        assert!(name_iterator.get_next().await.is_err());
        Ok(())
    }

    fn parse_content(tree_content: TreeContent) -> Result<DiagnosticsHierarchy, Error> {
        let buffer = tree_content.buffer.unwrap();
        Ok(PartialNodeHierarchy::try_from(&buffer.vmo)?.into())
    }

    fn test_inspector() -> Inspector {
        let inspector = Inspector::default();
        inspector.root().record_int("a", 1);
        inspector.root().record_lazy_child("lazy", || {
            async move {
                let inspector = Inspector::default();
                inspector.root().record_uint("b", 2);
                inspector.root().record_lazy_values("lazy-vals", || {
                    async move {
                        let inspector = Inspector::default();
                        inspector.root().record_double("c", 3.0);
                        Ok(inspector)
                    }
                    .boxed()
                });
                Ok(inspector)
            }
            .boxed()
        });
        inspector
    }
}
