// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test harness that mirrors how the crate is used in production: Fxfs runs on a dedicated executor
//! thread, while tests call `SyncIo` methods from a thread with no executor.

use crate::Volume;
use fidl_fuchsia_fxfs as ffxfs;
use fidl_fuchsia_io as fio;
use fuchsia_async as fasync;
use futures::channel::oneshot;
use fxfs::filesystem::FxFilesystemBuilder;
use fxfs::object_handle::{ObjectHandle, WriteObjectHandle};
use fxfs::object_store::transaction::{LockKey, Options, lock_keys};
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{
    Directory, HandleOptions, MAX_INLINE_SYMLINK_SIZE, MAX_INLINE_XATTR_SIZE, NewChildStoreOptions,
    ObjectStore, SetExtendedAttributeMode, StoreObjectHandle, StoreOptions,
};
use fxfs_crypto::Crypt;
use fxfs_platform::fuchsia::volume::MemoryPressureConfig;
use fxfs_platform::fuchsia::volumes_directory::VolumesDirectory;
use refaults_vmo::PageFaultCounter;
use std::future::Future;
use std::sync::{Arc, Weak, mpsc};
use std::thread::JoinHandle;
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

pub const FILE_PATH: &str = "file";
pub const FILE_CONTENTS: &[u8] = b"hello, remote volume!";
pub const FILE_MODE: u32 = 0o100644;
pub const FILE_UID: u32 = 1000;
pub const FILE_GID: u32 = 2000;
pub const FILE_RDEV: u64 = 42;
pub const FILE_SELINUX_CONTEXT: &[u8] = b"u:object_r:file_test:s0";
pub const FILE_XATTR_NAME: &[u8] = b"user.file";
pub const FILE_XATTR_VALUE: &[u8] = b"file xattr value";
/// An xattr on [`FILE_PATH`] that is too large to be stored inline.
pub const LARGE_XATTR_NAME: &[u8] = b"user.large";
pub const LARGE_XATTR_VALUE: &[u8] = &[0xab; MAX_INLINE_XATTR_SIZE + 1];
pub const EMPTY_FILE_PATH: &str = "empty";
pub const SUBDIR_PATH: &str = "dir";
pub const SUBDIR_MODE: u32 = 0o040750;
pub const SUBDIR_UID: u32 = 1002;
pub const SUBDIR_GID: u32 = 1003;
pub const SUBDIR_SELINUX_CONTEXT: &[u8] = b"u:object_r:dir_test:s0";
pub const SUBDIR_XATTR_NAME: &[u8] = b"user.dir";
pub const SUBDIR_XATTR_VALUE: &[u8] = b"dir xattr value";
pub const NESTED_FILE_PATH: &str = "dir/nested";
pub const NESTED_FILE_CONTENTS: &[u8] = b"nested contents";
pub const SYMLINK_PATH: &str = "symlink";
pub const SYMLINK_TARGET: &[u8] = FILE_PATH.as_bytes();
pub const SYMLINK_MODE: u32 = 0o120777;
pub const SYMLINK_UID: u32 = 1004;
pub const SYMLINK_GID: u32 = 1005;
pub const SYMLINK_SELINUX_CONTEXT: &[u8] = b"u:object_r:symlink_test:s0";
pub const SYMLINK_XATTR_NAME: &[u8] = b"user.symlink";
pub const SYMLINK_XATTR_VALUE: &[u8] = b"symlink xattr value";
/// A symlink whose target is too long to be stored inline.
pub const LONG_SYMLINK_PATH: &str = "long_symlink";
pub const LONG_SYMLINK_TARGET: &[u8] = &[b'a'; MAX_INLINE_SYMLINK_SIZE + 1];
pub const VERITY_FILE_PATH: &str = "verity";
pub const VERITY_FILE_CONTENTS: &[u8] = b"verified contents";

struct FixtureIds {
    root_directory_object_id: u64,
    file_object_id: u64,
    empty_file_object_id: u64,
    subdir_object_id: u64,
    nested_file_object_id: u64,
    verity_file_object_id: u64,
    symlink_object_id: u64,
    long_symlink_object_id: u64,
}

async fn populate_test_store(store: &Arc<ObjectStore>) -> FixtureIds {
    let root_directory_object_id = store.root_directory_object_id();
    let root_dir =
        Directory::open(store, root_directory_object_id).await.expect("open root directory failed");

    let mut transaction = store
        .filesystem()
        .new_transaction(
            lock_keys![LockKey::object(store.store_object_id(), root_directory_object_id)],
            Options::default(),
        )
        .await
        .expect("new_transaction failed");
    let file =
        root_dir.create_child_file(&mut transaction, FILE_PATH).await.expect("create file failed");
    file.update_attributes(
        &mut transaction,
        Some(&fio::MutableNodeAttributes {
            mode: Some(FILE_MODE),
            uid: Some(FILE_UID),
            gid: Some(FILE_GID),
            rdev: Some(FILE_RDEV),
            ..Default::default()
        }),
        None,
    )
    .await
    .expect("update_attributes failed");
    let empty_file = root_dir
        .create_child_file(&mut transaction, EMPTY_FILE_PATH)
        .await
        .expect("create empty file failed");
    let subdir =
        root_dir.create_child_dir(&mut transaction, SUBDIR_PATH).await.expect("create dir failed");
    store
        .update_attributes(
            &mut transaction,
            subdir.object_id(),
            Some(&fio::MutableNodeAttributes {
                mode: Some(SUBDIR_MODE),
                uid: Some(SUBDIR_UID),
                gid: Some(SUBDIR_GID),
                ..Default::default()
            }),
            None,
        )
        .await
        .expect("update_attributes on dir failed");
    let symlink_object_id = root_dir
        .create_symlink(&mut transaction, SYMLINK_TARGET, SYMLINK_PATH)
        .await
        .expect("create symlink failed");
    store
        .update_attributes(
            &mut transaction,
            symlink_object_id,
            Some(&fio::MutableNodeAttributes {
                mode: Some(SYMLINK_MODE),
                uid: Some(SYMLINK_UID),
                gid: Some(SYMLINK_GID),
                ..Default::default()
            }),
            None,
        )
        .await
        .expect("update_attributes on symlink failed");
    let long_symlink_object_id = root_dir
        .create_symlink(&mut transaction, LONG_SYMLINK_TARGET, LONG_SYMLINK_PATH)
        .await
        .expect("create long symlink failed");
    let verity_file = root_dir
        .create_child_file(&mut transaction, VERITY_FILE_PATH)
        .await
        .expect("create verity file failed");
    transaction.commit().await.expect("commit failed");

    let mut buf = file.allocate_buffer(FILE_CONTENTS.len()).await;
    buf.as_mut_ptr_slice().copy_from_slice(FILE_CONTENTS);
    file.write_or_append(Some(0), buf.as_ref()).await.expect("write file failed");
    for (name, value) in [
        (fio::SELINUX_CONTEXT_NAME.as_bytes(), FILE_SELINUX_CONTEXT),
        (FILE_XATTR_NAME, FILE_XATTR_VALUE),
        (LARGE_XATTR_NAME, LARGE_XATTR_VALUE),
    ] {
        file.set_extended_attribute(name.to_vec(), value.to_vec(), SetExtendedAttributeMode::Set)
            .await
            .expect("set file xattr failed");
    }

    let mut buf = verity_file.allocate_buffer(VERITY_FILE_CONTENTS.len()).await;
    buf.as_mut_ptr_slice().copy_from_slice(VERITY_FILE_CONTENTS);
    verity_file.write_or_append(Some(0), buf.as_ref()).await.expect("write verity file failed");
    verity_file
        .enable_verity(fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![]),
            ..Default::default()
        })
        .await
        .expect("enable_verity failed");

    subdir.set_casefold(true).await.expect("set_casefold failed");
    for (name, value) in [
        (fio::SELINUX_CONTEXT_NAME.as_bytes(), SUBDIR_SELINUX_CONTEXT),
        (SUBDIR_XATTR_NAME, SUBDIR_XATTR_VALUE),
    ] {
        subdir
            .set_extended_attribute(name.to_vec(), value.to_vec(), SetExtendedAttributeMode::Set)
            .await
            .expect("set dir xattr failed");
    }
    let symlink = StoreObjectHandle::new(
        store.clone(),
        symlink_object_id,
        /* permanent_keys: */ false,
        HandleOptions::default(),
        /* trace: */ false,
    );
    for (name, value) in [
        (fio::SELINUX_CONTEXT_NAME.as_bytes(), SYMLINK_SELINUX_CONTEXT),
        (SYMLINK_XATTR_NAME, SYMLINK_XATTR_VALUE),
    ] {
        symlink
            .set_extended_attribute(name.to_vec(), value.to_vec(), SetExtendedAttributeMode::Set)
            .await
            .expect("set symlink xattr failed");
    }
    let mut transaction = store
        .filesystem()
        .new_transaction(
            lock_keys![LockKey::object(store.store_object_id(), subdir.object_id())],
            Options::default(),
        )
        .await
        .expect("new_transaction failed");
    let nested_file = subdir
        .create_child_file(&mut transaction, "nested")
        .await
        .expect("create nested file failed");
    transaction.commit().await.expect("commit failed");

    let mut buf = nested_file.allocate_buffer(NESTED_FILE_CONTENTS.len()).await;
    buf.as_mut_ptr_slice().copy_from_slice(NESTED_FILE_CONTENTS);
    nested_file.write_or_append(Some(0), buf.as_ref()).await.expect("write nested file failed");

    // Flush the store so all records live in persistent layers when the volume is mounted remotely.
    store.flush().await.expect("flush store failed");

    FixtureIds {
        root_directory_object_id,
        file_object_id: file.object_id(),
        empty_file_object_id: empty_file.object_id(),
        subdir_object_id: subdir.object_id(),
        nested_file_object_id: nested_file.object_id(),
        verity_file_object_id: verity_file.object_id(),
        symlink_object_id,
        long_symlink_object_id,
    }
}

/// A freshly formatted Fxfs volume served from a background executor thread.
///
/// Dropping a `MountedVolume` shuts down the filesystem and joins the background thread,
/// propagating any panic from it.
pub struct MountedVolume {
    volume: Option<Volume>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MountedVolume {
    /// Formats a filesystem, creates a volume (encrypted if `crypt` is `Some`), runs `populate` on
    /// the volume's `ObjectStore`, and mounts it remotely.
    pub fn start<T, F, Fut>(crypt: Option<Arc<dyn Crypt>>, populate: F) -> (Self, T)
    where
        T: Send + 'static,
        F: FnOnce(Arc<ObjectStore>) -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        let (init_tx, init_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

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
                    let vol_store = root_volume
                        .new_volume(
                            "vol",
                            NewChildStoreOptions {
                                options: StoreOptions {
                                    crypt: crypt.clone(),
                                    ..StoreOptions::default()
                                },
                                ..Default::default()
                            },
                        )
                        .await
                        .expect("new_volume failed");
                    let output = populate(vol_store.clone()).await;
                    if crypt.is_some() {
                        vol_store.lock().await.expect("lock store failed");
                    }
                    drop(vol_store);

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
                    let (client_end, server_end) =
                        fidl::endpoints::create_endpoints::<ffxfs::RemoteVolumeMarker>();
                    let (info, initial_layers, _guid) = volumes_directory
                        .mount_remote_volume("vol", crypt, server_end)
                        .await
                        .expect("mount_remote_volume failed");

                    init_tx.send((info, initial_layers, client_end, output)).unwrap();

                    let _ = shutdown_rx.await;

                    volumes_directory.terminate().await;
                    drop(volumes_directory);
                    filesystem.close().await.expect("close filesystem failed");
                })
            })
            .expect("Failed to spawn executor thread");

        let (info, initial_layers, client_end, output) = match init_rx.recv() {
            Ok(init) => init,
            // The executor thread exited before initialization finished, so it must have panicked.
            Err(_) => match thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("Executor thread exited without creating a volume"),
            },
        };
        // Construct `Volume` on the test thread so that any pager-backed VMO reads during layer
        // opening are serviced by the running executor thread.
        let volume = Volume::new(info, initial_layers, client_end).expect("Volume::new failed");
        (Self { volume: Some(volume), shutdown: Some(shutdown_tx), thread: Some(thread) }, output)
    }

    pub fn volume(&self) -> &Volume {
        self.volume.as_ref().unwrap()
    }
}

impl Drop for MountedVolume {
    fn drop(&mut self) {
        // Drop the volume on the client thread first so its `Close` FIDL call completes while the
        // executor thread is still running.
        drop(self.volume.take());
        if let Some(shutdown) = self.shutdown.take() {
            // This only fails if the executor thread already exited, in which case the join below
            // reports why.
            let _ = shutdown.send(());
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

/// A freshly formatted, unencrypted Fxfs volume served from a background executor thread.
///
/// Dropping a `TestVolume` shuts down the filesystem and joins the background thread, propagating
/// any panic from it.
pub struct TestVolume {
    mounted: MountedVolume,
    ids: FixtureIds,
}

impl TestVolume {
    pub fn new() -> Self {
        let (mounted, ids) =
            MountedVolume::start(None, |store| async move { populate_test_store(&store).await });
        Self { mounted, ids }
    }

    pub fn volume(&self) -> &Volume {
        self.mounted.volume()
    }

    /// The object ID of the volume's root directory.
    pub fn root_directory_object_id(&self) -> u64 {
        self.ids.root_directory_object_id
    }

    /// The object ID of [`FILE_PATH`].
    pub fn file_object_id(&self) -> u64 {
        self.ids.file_object_id
    }

    /// The object ID of [`EMPTY_FILE_PATH`].
    pub fn empty_file_object_id(&self) -> u64 {
        self.ids.empty_file_object_id
    }

    /// The object ID of [`SUBDIR_PATH`].
    pub fn subdir_object_id(&self) -> u64 {
        self.ids.subdir_object_id
    }

    /// The object ID of [`NESTED_FILE_PATH`].
    pub fn nested_file_object_id(&self) -> u64 {
        self.ids.nested_file_object_id
    }

    /// The object ID of [`VERITY_FILE_PATH`].
    pub fn verity_file_object_id(&self) -> u64 {
        self.ids.verity_file_object_id
    }

    /// The object ID of [`SYMLINK_PATH`].
    pub fn symlink_object_id(&self) -> u64 {
        self.ids.symlink_object_id
    }

    /// The object ID of [`LONG_SYMLINK_PATH`].
    pub fn long_symlink_object_id(&self) -> u64 {
        self.ids.long_symlink_object_id
    }
}
