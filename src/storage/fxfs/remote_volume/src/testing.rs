// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test harness that mirrors how the crate is used in production: Fxfs runs on a dedicated executor
//! thread, while tests call `SyncIo` methods from a thread with no executor.

use crate::Volume;
use fuchsia_async as fasync;
use futures::channel::oneshot;
use fxfs::filesystem::FxFilesystemBuilder;
use fxfs::object_store::volume::root_volume;
use fxfs_platform::fuchsia::volume::MemoryPressureConfig;
use fxfs_platform::fuchsia::volumes_directory::VolumesDirectory;
use refaults_vmo::PageFaultCounter;
use std::sync::{Arc, Weak, mpsc};
use std::thread::JoinHandle;
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

/// A freshly formatted, unencrypted Fxfs volume served from a background executor thread.
///
/// Dropping a `TestVolume` shuts down the filesystem and joins the background thread, propagating
/// any panic from it.
pub struct TestVolume {
    volume: Option<Volume>,
    root_directory_object_id: u64,
    // The volume is handed back to the executor thread on shutdown so that it's dropped there.
    shutdown: Option<oneshot::Sender<Volume>>,
    thread: Option<JoinHandle<()>>,
}

impl TestVolume {
    pub fn new() -> Self {
        let (init_tx, init_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<Volume>();

        let thread = std::thread::Builder::new()
            .name("remote-volume-test-executor".to_string())
            .spawn(move || {
                fasync::LocalExecutor::default().run_singlethreaded(async move {
                    let filesystem = FxFilesystemBuilder::new()
                        .format(DeviceHolder::new(FakeDevice::new(16384, 512)))
                        .await
                        .expect("format filesystem failed");
                    let root_volume =
                        root_volume(filesystem.clone()).await.expect("root_volume failed");
                    let page_fault_counter = Arc::new(
                        PageFaultCounter::new().expect("Failed to create PageFaultCounter"),
                    );
                    let volumes_directory = VolumesDirectory::new(
                        root_volume,
                        Weak::new(),
                        None,
                        page_fault_counter,
                        MemoryPressureConfig::default(),
                    )
                    .await
                    .expect("VolumesDirectory::new failed");
                    let volume_and_root = volumes_directory
                        .create_and_mount_volume("vol", None, false, Default::default())
                        .await
                        .expect("create_and_mount_volume failed");
                    let root_directory_object_id =
                        volume_and_root.volume().store().root_directory_object_id();
                    let volume = Volume::new(volume_and_root).expect("Volume::new failed");

                    init_tx.send((volume, root_directory_object_id)).unwrap();

                    // Wait for `TestVolume::drop` to hand the volume back, and drop it here.
                    drop(shutdown_rx.await);

                    volumes_directory.terminate().await;
                    drop(volumes_directory);
                    filesystem.close().await.expect("close filesystem failed");
                })
            })
            .expect("Failed to spawn executor thread");

        match init_rx.recv() {
            Ok((volume, root_directory_object_id)) => Self {
                volume: Some(volume),
                root_directory_object_id,
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            },
            // The executor thread exited before initialization finished, so it must have panicked.
            Err(_) => match thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("Executor thread exited without creating a volume"),
            },
        }
    }

    pub fn volume(&self) -> &Volume {
        self.volume.as_ref().unwrap()
    }

    /// The object ID of the volume's root directory.
    pub fn root_directory_object_id(&self) -> u64 {
        self.root_directory_object_id
    }
}

impl Drop for TestVolume {
    fn drop(&mut self) {
        if let (Some(shutdown), Some(volume)) = (self.shutdown.take(), self.volume.take()) {
            // This only fails if the executor thread already exited, in which case the join below
            // reports why.
            let _ = shutdown.send(volume);
        }
        if let Some(thread) = self.thread.take() {
            if let Err(panic) = thread.join() {
                if !std::thread::panicking() {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }
}
