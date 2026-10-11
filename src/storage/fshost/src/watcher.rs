// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::{Device, Parent, VolumeServiceDevice};
use anyhow::{Context as _, Error};
use async_trait::async_trait;
use fidl_fuchsia_io as fio;
use fuchsia_async as fasync;
use fuchsia_fs::directory::{WatchEvent, WatchMessage};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt, stream};
use std::future::ready;
use std::sync::Arc;

/// A provider for a stream of block devices.
#[async_trait]
pub trait WatchSource: Send + Sync + 'static {
    /// Generates a new stream of block devices.
    async fn as_stream(&mut self) -> Result<stream::BoxStream<'static, Box<dyn Device>>, Error>;
}

fn common_filters(watcher: fuchsia_fs::directory::Watcher) -> stream::BoxStream<'static, String> {
    Box::pin(watcher.filter_map(|result| {
        ready(match result {
            Ok(WatchMessage { event: WatchEvent::ADD_FILE | WatchEvent::EXISTING, filename })
                if filename.as_os_str() != "." =>
            {
                Some(filename.to_str().unwrap().to_owned())
            }
            Err(error) => {
                // TODO(https://fxbug.dev/422230360): This is probably worth using error for, but
                // sometimes in tests the gpt2 watcher stream gets closed while we are still trying
                // to read watch messages, which was causing flakes. Upgrade again once the flake
                // is addressed.
                log::warn!(error:?; "fshost block watcher stream error");
                None
            }
            _ => None,
        })
    }))
}

/// An implementation of `WatchSource` based on a DirectoryProxy.  The source is expected to be
/// a directory containing a "volume" node which implements fuchsia.storage.block.Block.
#[derive(Clone, Debug)]
pub struct DirSource {
    dir: fio::DirectoryProxy,
    // The name of the source of these devices, for example the moniker of a component providing
    // them. Used for logging and debugging.
    source: String,
    // The parent to set for these devices.
    parent: Parent,
}

impl DirSource {
    /// Creates a `DirSource` that connects a `VolumeProtocolDevice` to each entry in `dir`.
    pub fn new(dir: fio::DirectoryProxy, source: impl ToString, parent: Parent) -> Self {
        Self { dir, source: source.to_string(), parent }
    }
}

#[async_trait]
impl WatchSource for DirSource {
    async fn as_stream(&mut self) -> Result<stream::BoxStream<'static, Box<dyn Device>>, Error> {
        let watcher = fuchsia_fs::directory::Watcher::new(&self.dir)
            .await
            .with_context(|| format!("Failed to watch dir with source: {}", self.source))?;
        let dir = Arc::new(fuchsia_fs::directory::clone(&self.dir)?);
        let source = self.source.clone();
        let parent = self.parent;
        Ok(Box::pin(common_filters(watcher).filter_map(move |filename| {
            let dir = dir.clone();
            let source = source.clone();
            async move {
                VolumeServiceDevice::new(&dir, filename, source, parent)
                    .await
                    .map(|d| Box::new(d) as Box<dyn Device>)
                    .map_err(|err| {
                        log::warn!(err:?; "Failed to create device (maybe it went away?)");
                        err
                    })
                    .ok()
            }
        })))
    }
}

/// Watcher generates new [`Device`]s for fshost to process.
pub struct Watcher {
    device_tx: mpsc::UnboundedSender<Box<dyn Device>>,
    // Each source has its own Task, and they all feed into _device_tx.
    tasks: Vec<fasync::Task<()>>,
}

impl Watcher {
    /// Create a new Watcher and Device stream. The watcher will start watching `sources`
    /// initially, populating the stream with any entries which are already there, then sending new
    /// items on the stream as they are added to the directory.
    pub async fn new(
        sources: Vec<Box<dyn WatchSource>>,
    ) -> Result<(Self, impl futures::Stream<Item = Box<dyn Device>>), Error> {
        let (device_tx, device_rx) = mpsc::unbounded();

        let mut this = Watcher { device_tx, tasks: vec![] };
        for source in sources.into_iter() {
            this.add_source(source).await?;
        }

        Ok((this, device_rx))
    }

    pub async fn add_source(&mut self, mut source: Box<dyn WatchSource>) -> Result<(), Error> {
        self.tasks.push(fasync::Task::spawn(Self::process_one_stream(
            source.as_stream().await?,
            self.device_tx.clone(),
        )));
        Ok(())
    }

    async fn process_one_stream(
        mut device_stream: stream::BoxStream<'static, Box<dyn Device>>,
        mut device_tx: mpsc::UnboundedSender<Box<dyn Device>>,
    ) {
        while let Some(device) = device_stream.next().await {
            if let Err(error) = device_tx.send(device).await {
                log::warn!(error:?; "Failed to send device");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DirSource, Watcher};
    use crate::device::Parent;
    use fidl_fuchsia_storage_block::BlockRequestStream;
    use futures::StreamExt;
    use std::sync::Arc;
    use vfs::directory::helper::DirectlyMutable;
    use vfs::execution_scope::ExecutionScope;
    use vfs::service;

    pub fn block_protocol() -> Arc<service::Service> {
        service::host(move |mut stream: BlockRequestStream| async move {
            if let Some(request) = stream.next().await {
                // The service never actually gets used in the tests.
                panic!("Unexpected request {request:?}");
            }
        })
    }

    #[fuchsia::test]
    async fn watcher_populates_device_stream() {
        let partitions_dir = vfs::pseudo_directory! {
            "000" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
            "001" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
        };

        let client = vfs::directory::serve_read_only(partitions_dir.clone(), ExecutionScope::new());
        let (_watcher, mut device_stream) =
            Watcher::new(vec![Box::new(DirSource::new(client, "test-dir-source", Parent::Dev))])
                .await
                .expect("failed to make watcher");

        let expected_devices = std::collections::HashSet::from([
            "test-dir-source/000".to_string(),
            "test-dir-source/001".to_string(),
        ]);
        let mut devices = std::collections::HashSet::new();
        for _ in 0..expected_devices.len() {
            devices.insert(device_stream.next().await.unwrap().path().to_string());
        }
        assert_eq!(devices, expected_devices);

        // Removing an entry for a device already taken off the stream doesn't do anything.
        assert!(
            partitions_dir
                .remove_entry("001", false)
                .expect("failed to remove dir entry 001")
                .is_some()
        );

        // Adding an entry generates a new block device.
        partitions_dir
            .add_entry(
                "002",
                vfs::pseudo_directory! {
                    "volume" => block_protocol(),
                },
            )
            .expect("failed to add dir entry 002");

        assert_eq!(device_stream.next().await.unwrap().path(), "test-dir-source/002");
    }

    #[fuchsia::test]
    async fn add_stream() {
        let dir1 = vfs::pseudo_directory! {
            "000" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
            "001" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
        };

        let dir2 = vfs::pseudo_directory! {
            "000" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
            "001" => vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
        };

        let client1 = vfs::directory::serve_read_only(dir1.clone(), ExecutionScope::new());
        let client2 = vfs::directory::serve_read_only(dir2.clone(), ExecutionScope::new());

        let (mut watcher, mut device_stream) =
            Watcher::new(vec![Box::new(DirSource::new(client1, "dir1", Parent::Dev))])
                .await
                .expect("failed to make watcher");

        let mut devices = std::collections::HashSet::from(["dir1/000", "dir1/001"]);

        // There are two devices that were added before we started watching.
        assert!(devices.remove(device_stream.next().await.unwrap().path()));
        assert!(devices.remove(device_stream.next().await.unwrap().path()));
        assert!(devices.is_empty());

        // Existing entries in the new source are yielded immediately
        watcher
            .add_source(Box::new(DirSource::new(client2, "dir2", Parent::SystemPartitionTable)))
            .await
            .expect("failed to add_source");

        let mut devices = std::collections::HashSet::from(["dir2/000", "dir2/001"]);
        assert!(devices.remove(device_stream.next().await.unwrap().path()));
        assert!(devices.remove(device_stream.next().await.unwrap().path()));
        assert!(devices.is_empty());

        // And now the directories are both watched as expected
        dir2.add_entry(
            "002",
            vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
        )
        .expect("failed to add dir entry 002");

        assert_eq!(device_stream.next().await.unwrap().path(), "dir2/002");

        dir1.add_entry(
            "002",
            vfs::pseudo_directory! {
                "volume" => block_protocol(),
            },
        )
        .expect("failed to add dir entry 002");

        assert_eq!(device_stream.next().await.unwrap().path(), "dir1/002");
    }
}
