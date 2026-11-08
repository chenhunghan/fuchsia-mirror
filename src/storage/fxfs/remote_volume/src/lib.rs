// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes an Fxfs volume in-process through the [`SyncIo`] and [`SyncIoDirectory`] traits.
//!
//! `SyncIo` methods may be called from threads that don't run a `fuchsia_async` executor, so they
//! drive Fxfs futures inline with `futures::executor::block_on`. Code reachable from `SyncIo`
//! therefore must not rely on an implicit executor (`fasync::Task::spawn`, `EHandle::local()`,
//! timers, ...).

use anyhow::{Error, anyhow};
use fidl_fuchsia_io as fio;
use futures::executor::block_on;
use fxfs_platform::fuchsia::directory::FxDirectory;
use fxfs_platform::fuchsia::node::FxNode as _;
use fxfs_platform::fuchsia::volume::FxVolumeAndRoot;
use std::ops::ControlFlow;
use std::sync::Arc;
use sync_io_client::{
    AllocateMode, Factory, SyncIo, SyncIoDirectory, XattrSetMode, zxio_fsverity_descriptor_t,
};
use vfs::node::Node as _;

#[cfg(test)]
mod testing;

/// An Fxfs volume mounted in-process, exposing nodes via `SyncIo`.
// TODO(https://fxbug.dev/570053552): `Volume` does not own the volume's lifecycle yet; the local
// Fxfs instance that created the `FxVolumeAndRoot` does. Unmount/shutdown will come with
// `MountRemote`.
pub struct Volume {
    root: Arc<FxDirectory>,
}

impl Volume {
    /// Creates a new `Volume`.
    ///
    /// # Errors
    ///
    /// Returns an error if the volume's root is not an `FxDirectory` (e.g. a blob volume).
    // TODO(https://fxbug.dev/570053552): Construct from the outputs of
    // `fuchsia.fs.startup.Volume/MountRemote` (client-side ObjectStore backed by layer VMOs)
    // instead of a local volume handle.
    pub fn new(volume_and_root: FxVolumeAndRoot) -> Result<Self, Error> {
        let root = volume_and_root
            .root()
            .clone()
            .into_any()
            .downcast::<FxDirectory>()
            .map_err(|_| anyhow!("volume root is not an FxDirectory"))?;
        Ok(Self { root })
    }

    /// Returns a `SyncIo` handle to the root directory.
    pub fn root(&self) -> LocalFxIo {
        LocalFxIo { kind: NodeKind::Directory(self.root.clone()) }
    }
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("name", &self.root.volume().name())
            .field("root_object_id", &self.root.object_id())
            .finish()
    }
}

enum NodeKind {
    Directory(Arc<FxDirectory>),
    // TODO(https://fxbug.dev/570053552): Add files and symlinks.
}

impl std::fmt::Debug for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeKind::Directory(dir) => f.debug_tuple("Directory").field(&dir.object_id()).finish(),
        }
    }
}

/// A handle to a node in a `Volume`. Implements `SyncIo`.
#[derive(Debug)]
pub struct LocalFxIo {
    kind: NodeKind,
}

impl SyncIo for LocalFxIo {
    type Directory = LocalFxIoDirectory;

    fn attr_get(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<(fio::MutableNodeAttributes, fio::ImmutableNodeAttributes), zx::Status> {
        let attributes = match &self.kind {
            NodeKind::Directory(dir) => block_on(dir.get_attributes(query))?,
        };
        Ok((attributes.mutable_attributes, attributes.immutable_attributes))
    }

    fn attr_set(&self, _attributes: fio::MutableNodeAttributes) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement attr_set.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn query_filesystem(&self) -> Result<fio::FilesystemInfo, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Fxfs implements this with
        // `FxVolume::filesystem_info_for_volume()`, which calls `ensure_local()`. That will need to
        // change for remote filesystems.
        match &self.kind {
            NodeKind::Directory(dir) => dir.query_filesystem(),
        }
    }

    fn open<F: Factory<Self>>(
        &self,
        _path: &str,
        _flags: fio::Flags,
        _create_attributes: Option<fio::MutableNodeAttributes>,
        _query: fio::NodeAttributesQuery,
        _factory: F,
    ) -> Result<F::Result, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement open.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn open_pipelined<F: Factory<Self>>(
        &self,
        _paths: &[&str],
        _flags: fio::Flags,
        _query: fio::NodeAttributesQuery,
        _factory_fn: impl FnMut() -> F,
    ) -> impl Iterator<Item = Result<F::Result, zx::Status>> {
        // TODO(https://fxbug.dev/570053552): Implement open_pipelined.
        std::iter::once(Err(zx::Status::NOT_SUPPORTED))
    }

    fn link_into(&self, _target_dir: &Self, _name: &str) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement link_into.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn unlink(&self, _name: &str, _flags: fio::UnlinkFlags) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement unlink.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn rename(
        &self,
        _old_path: &str,
        _new_directory: &Self,
        _new_path: &str,
    ) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement rename.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn create_symlink(&self, _name: &str, _target: &[u8]) -> Result<Self, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement create_symlink.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn read_partial(&self, _offset: u64, _max: usize) -> Result<(Vec<u8>, bool), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement read_partial.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn write(&self, _offset: u64, _data: &[u8]) -> Result<usize, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement write.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn supports_vectored(&self) -> bool {
        // TODO(https://fxbug.dev/570053552): Support vectored I/O.
        false
    }

    unsafe fn readv(
        &self,
        _offset: u64,
        _iovecs: &mut [zx::sys::zx_iovec_t],
    ) -> Result<usize, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement readv.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn writev(&self, _offset: u64, _iovecs: &[zx::sys::zx_iovec_t]) -> Result<usize, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement writev.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn truncate(&self, _length: u64) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement truncate.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn vmo_get(&self, _flags: zx::VmarFlags) -> Result<zx::Vmo, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement vmo_get.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn sync(&self) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement sync.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn enable_verity(&self, _descriptor: &zxio_fsverity_descriptor_t) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement enable_verity.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn allocate(&self, _offset: u64, _len: u64, _mode: AllocateMode) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement allocate.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn xattr_get(&self, _name: &[u8]) -> Result<Vec<u8>, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement xattr_get.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn xattr_set(
        &self,
        _name: &[u8],
        _value: &[u8],
        _mode: XattrSetMode,
    ) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement xattr_set.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn xattr_remove(&self, _name: &[u8]) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement xattr_remove.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn xattr_list(&self) -> Result<Vec<Vec<u8>>, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement xattr_list.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn open_directory(&self) -> Result<Self::Directory, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement open_directory.
        Err(zx::Status::NOT_SUPPORTED)
    }
}

/// A stateful directory iterator. Implements `SyncIoDirectory`.
// TODO(https://fxbug.dev/570053552): Hold the directory and iteration state once readdir is
// implemented.
#[derive(Debug)]
pub struct LocalFxIoDirectory;

impl SyncIoDirectory for LocalFxIoDirectory {
    fn seek(&self, _new_index: u64) -> Result<u64, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement seek.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn readdir<B, S: FnMut(u64, fio::DirentType, &[u8]) -> ControlFlow<B, ()>>(
        &self,
        _sink: S,
    ) -> Result<Option<B>, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement readdir.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn sync(&self) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement sync.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn clone_proxy(&self) -> Result<Option<fio::DirectorySynchronousProxy>, zx::Status> {
        // This directory is not backed by a FIDL connection.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::TestVolume;
    use fidl_fuchsia_io as fio;
    use sync_io_client::SyncIo;

    #[fuchsia::test]
    fn create_and_teardown() {
        let test_volume = TestVolume::new();
        let _root = test_volume.volume().root();
    }

    #[fuchsia::test]
    fn root_attr_get() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let (_mutable, immutable) = root
            .attr_get(
                fio::NodeAttributesQuery::PROTOCOLS
                    | fio::NodeAttributesQuery::ID
                    | fio::NodeAttributesQuery::LINK_COUNT,
            )
            .expect("attr_get failed");
        assert_eq!(immutable.protocols, Some(fio::NodeProtocolKinds::DIRECTORY));
        assert_eq!(immutable.id, Some(test_volume.root_directory_object_id()));
        // Fxfs reports refs + 1 (for ".") + sub_dirs; a fresh root has one ref and no subdirs.
        assert_eq!(immutable.link_count, Some(2));
        // Attributes that weren't requested shouldn't be returned.
        assert_eq!(immutable.content_size, None);
    }

    #[fuchsia::test]
    fn root_query_filesystem() {
        let test_volume = TestVolume::new();
        let info = test_volume.volume().root().query_filesystem().expect("query_filesystem failed");
        assert_ne!(info.block_size, 0);
        assert_ne!(info.total_bytes, 0);
        assert_eq!(info.fs_type, fidl_fuchsia_fs::VfsType::Fxfs.into_primitive());
    }

    #[fuchsia::test]
    fn unimplemented_op_returns_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        assert_eq!(root.open_directory().err(), Some(zx::Status::NOT_SUPPORTED));
    }
}
