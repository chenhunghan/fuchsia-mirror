// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes an Fxfs volume in-process through the [`SyncIo`] and [`SyncIoDirectory`] traits.
//!
//! `SyncIo` methods may be called from threads that don't run a `fuchsia_async` executor, so they
//! drive Fxfs futures inline with `futures::executor::block_on`. Code reachable from `SyncIo`
//! therefore must not rely on an implicit executor (`fasync::Task::spawn`, `EHandle::local()`,
//! timers, ...).

// Needed for tests calling `VolumesDirectory::mount_remote_volume` under optimization.
#![recursion_limit = "256"]

use anyhow::{Error, anyhow};
use async_trait::async_trait;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_fxfs as ffxfs;
use fidl_fuchsia_io as fio;
use futures::executor::block_on;
use fxfs::errors::FxfsError;
use fxfs::filesystem::{self, Filesystem, SyncOptions, TruncateGuard};
use fxfs::lsm_tree::LSMTree;
use fxfs::lsm_tree::persistent_layer::SyncPersistentLayer;
use fxfs::lsm_tree::types::Layer;
use fxfs::object_store::allocator::Reservation;
use fxfs::object_store::transaction::{
    self, LockKeys, LockManager, Transaction, TransactionConcurrencyLimiter, TransactionSink,
};
use fxfs::object_store::{
    ExtendedAttributeValue, HandleOwner, ObjectKey, ObjectStoreTrait, ObjectValue,
    StoreObjectHandle,
};
use fxfs_crypto::CipherHolder;
use fxfs_platform::fuchsia::layer_pager::VmoLayerObject;
use std::ops::ControlFlow;
use std::sync::Arc;
use storage_units::BlockSize;
use sync_io_client::{
    AllocateMode, Factory, SyncIo, SyncIoDirectory, XattrSetMode, zxio_fsverity_descriptor_t,
};
use vfs::path::Path;
use vfs::{CreationMode, ProtocolsExt};

mod directory;
mod file;
mod symlink;
#[cfg(test)]
mod testing;

use crate::directory::RemoteDirectory;
use crate::file::RemoteFile;
use crate::symlink::RemoteSymlink;

/// `open` flags that create or truncate a node, or that ask for the right to modify it. They all
/// return `zx::Status::NOT_SUPPORTED` because remote volumes are read-only for now.
const MUTATING_OPEN_FLAGS: fio::Flags = fio::Flags::FLAG_MAYBE_CREATE
    .union(fio::Flags::FLAG_MUST_CREATE)
    .union(fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY)
    .union(fio::Flags::FILE_TRUNCATE)
    .union(fio::Flags::PERM_WRITE_BYTES)
    .union(fio::Flags::PERM_MODIFY_DIRECTORY)
    .union(fio::Flags::PERM_UPDATE_ATTRIBUTES);

/// Converts an error from Fxfs into the status that `SyncIo` returns, like the platform's
/// `map_to_status`.
fn map_to_status(err: Error) -> zx::Status {
    let root_cause = err.root_cause();
    if let Some(status) = root_cause.downcast_ref::<zx::Status>() {
        *status
    } else if let Some(fxfs_error) = root_cause.downcast_ref::<FxfsError>() {
        fxfs_error.clone().into()
    } else {
        log::warn!("Internal error: {err:?}");
        zx::Status::INTERNAL
    }
}

/// Returns the node's inline SELinux context, or `FxfsError::NotSupported` if the context is stored
/// out of line in an extended attribute.
fn get_inline_selinux_context(
    handle: &StoreObjectHandle<RemoteObjectStore>,
) -> Result<Option<fio::SelinuxContext>, Error> {
    match block_on(handle.get_inline_selinux_context())? {
        Some(fio::SelinuxContext::UseExtendedAttributes(_)) => {
            // TODO(https://fxbug.dev/570053552): Return `UseExtendedAttributes` once `xattr_get`
            // has a FIDL fallback for out-of-line extended attributes. Returning it while
            // `xattr_get` fails with `NOT_SUPPORTED` (`EOPNOTSUPP`) causes Starnix to treat the
            // node as unlabeled (`ENODATA | EOPNOTSUPP => None`).
            Err(FxfsError::NotSupported.into())
        }
        context => Ok(context),
    }
}

/// An Fxfs volume mounted in-process, exposing nodes via `SyncIo`.
pub struct Volume {
    store: Arc<RemoteObjectStore>,
    root_directory: Arc<RemoteDirectory>,
}

impl Volume {
    /// Creates a new `Volume` from the outputs of `fuchsia.fs.startup.Volume/MountRemote`.
    ///
    /// # Errors
    ///
    /// Returns an error if `info.block_size` is invalid, if any of the layer VMOs in
    /// `initial_layers` cannot be mapped and opened, or if the root directory cannot be opened.
    pub fn new(
        info: ffxfs::VolumeInfo,
        initial_layers: ffxfs::LayerSet,
        remote_volume: ClientEnd<ffxfs::RemoteVolumeMarker>,
    ) -> Result<Self, Error> {
        let block_size = BlockSize::new(info.block_size)
            .ok_or_else(|| anyhow!("invalid block size: {}", info.block_size))?;
        let filesystem = RemoteFilesystem::new(block_size);
        let store = RemoteObjectStore::new(
            filesystem,
            info.store_object_id,
            initial_layers,
            remote_volume,
        )?;
        let root_directory = RemoteDirectory::open(&store, info.root_directory_object_id)?;
        Ok(Self { store, root_directory })
    }

    /// Returns a `SyncIo` handle to the root directory.
    pub fn root(&self) -> LocalFxIo {
        LocalFxIo { kind: NodeKind::Directory(self.root_directory.clone()) }
    }
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("root_directory", &self.root_directory)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Client-side [`Filesystem`] implementation for a remotely mounted volume.
struct RemoteFilesystem {
    block_size: BlockSize,
    options: filesystem::Options,
    lock_manager: LockManager,
}

impl RemoteFilesystem {
    fn new(block_size: BlockSize) -> Arc<Self> {
        Arc::new(Self {
            block_size,
            options: filesystem::Options::default(),
            lock_manager: LockManager::default(),
        })
    }
}

impl std::fmt::Debug for RemoteFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFilesystem").field("block_size", &self.block_size).finish()
    }
}

#[async_trait]
impl TransactionSink for RemoteFilesystem {
    async fn new_transaction<'a>(
        self: Arc<Self>,
        _txn_locks: LockKeys,
        _options: transaction::Options<'a>,
    ) -> Result<Transaction<'a>, Error> {
        // TODO(https://fxbug.dev/570053552): Implement new_transaction.
        Err(FxfsError::NotSupported.into())
    }

    async fn check_space(&self) -> Result<(), Error> {
        // TODO(https://fxbug.dev/570053552): Implement check_space.
        Err(FxfsError::NotSupported.into())
    }

    fn reserve(&self, _owner_object_id: Option<u64>, _amount: u64) -> Option<Reservation> {
        // TODO(https://fxbug.dev/570053552): Implement reserve.
        None
    }

    async fn commit(&self, _transaction: &mut Transaction<'_>) -> Result<u64, Error> {
        // TODO(https://fxbug.dev/570053552): Implement commit.
        Err(FxfsError::NotSupported.into())
    }

    async fn commit_with_callback<'a>(
        &self,
        _transaction: &mut Transaction<'_>,
        _callback: Box<dyn FnOnce(u64) + Send + 'a>,
    ) -> Result<u64, Error> {
        // TODO(https://fxbug.dev/570053552): Implement commit_with_callback.
        Err(FxfsError::NotSupported.into())
    }

    fn downgrade_locks(&self, lock_keys: &LockKeys) {
        self.lock_manager.downgrade_locks(lock_keys);
    }

    fn drop_transaction(&self, transaction: &mut Transaction<'_>) {
        self.lock_manager.drop_transaction(transaction);
    }

    fn lock_manager(&self) -> &LockManager {
        &self.lock_manager
    }

    fn transaction_limiter(&self) -> &TransactionConcurrencyLimiter {
        // TODO(https://fxbug.dev/570053552): Implement transaction_limiter.
        todo!("https://fxbug.dev/570053552: Implement transaction_limiter")
    }
}

#[async_trait]
impl Filesystem for RemoteFilesystem {
    fn queue_tombstone_object(&self, _store_object_id: u64, _object_id: u64) {
        // TODO(https://fxbug.dev/570053552): Implement queue_tombstone_object.
        todo!("https://fxbug.dev/570053552: Implement queue_tombstone_object")
    }

    async fn tombstone_object(
        &self,
        _store_object_id: u64,
        _object_id: u64,
        _truncate_guard: Option<&TruncateGuard<'_>>,
    ) -> Result<(), Error> {
        // TODO(https://fxbug.dev/570053552): Implement tombstone_object.
        Err(FxfsError::NotSupported.into())
    }

    fn options(&self) -> &filesystem::Options {
        &self.options
    }

    fn block_size(&self) -> BlockSize {
        self.block_size
    }

    fn get_info(&self) -> filesystem::Info {
        // TODO(https://fxbug.dev/570053552): Implement get_info.
        todo!("https://fxbug.dev/570053552: Implement get_info")
    }

    async fn sync(&self, _options: SyncOptions<'_>) -> Result<(), Error> {
        // TODO(https://fxbug.dev/570053552): Implement sync.
        Err(FxfsError::NotSupported.into())
    }
}

/// Client-side [`ObjectStoreTrait`] implementation backed by read-only layer VMOs.
struct RemoteObjectStore {
    filesystem: Arc<RemoteFilesystem>,
    store_object_id: u64,
    tree: LSMTree<ObjectKey, ObjectValue>,
    // Keeps the server-side mount alive for as long as the store or any node handle exists.
    _remote_volume: ClientEnd<ffxfs::RemoteVolumeMarker>,
}

impl RemoteObjectStore {
    fn new(
        filesystem: Arc<RemoteFilesystem>,
        store_object_id: u64,
        initial_layers: ffxfs::LayerSet,
        remote_volume: ClientEnd<ffxfs::RemoteVolumeMarker>,
    ) -> Result<Arc<Self>, Error> {
        let block_size = filesystem.block_size();
        let mut layers: Vec<Arc<dyn Layer<ObjectKey, ObjectValue>>> =
            Vec::with_capacity(initial_layers.layers.len());
        for layer_file in initial_layers.layers {
            let vmo_object =
                Arc::new(VmoLayerObject::new(layer_file.vmo, layer_file.object_id, block_size)?);
            let layer = SyncPersistentLayer::<ObjectKey, ObjectValue>::open(vmo_object)?;
            layers.push(layer);
        }
        let tree = LSMTree::new(fxfs::object_store::merge::merge, None);
        tree.set_layers(layers);
        Ok(Arc::new(Self { filesystem, store_object_id, tree, _remote_volume: remote_volume }))
    }

    /// Looks up an extended attribute on `handle`. Inline values are returned from the local tree;
    /// out-of-line values stored in an attribute currently return `FxfsError::NotSupported`.
    fn get_extended_attribute(
        handle: &StoreObjectHandle<Self>,
        name: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        // TODO(https://fxbug.dev/570053552): Take a read lock on the object via `LockManager` to
        // prevent concurrent modification to the attribute.
        match block_on(handle.lookup_extended_attribute(name))?.ok_or(FxfsError::NotFound)? {
            ExtendedAttributeValue::Inline(value) => Ok(value),
            ExtendedAttributeValue::AttributeId(_) => {
                // TODO(https://fxbug.dev/570053552): This requires disk access, so we will need a
                // fallback mechanism to get the data from fxfs.
                Err(FxfsError::NotSupported.into())
            }
        }
    }
}

impl std::fmt::Debug for RemoteObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteObjectStore")
            .field("store_object_id", &self.store_object_id)
            .field("filesystem", &self.filesystem)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ObjectStoreTrait for RemoteObjectStore {
    fn store_object_id(&self) -> u64 {
        self.store_object_id
    }

    fn is_encrypted(&self) -> bool {
        // TODO(https://fxbug.dev/570053552): Support encrypted remote volumes.
        false
    }

    async fn get_fscrypt_key(&self, _object_id: u64) -> Result<CipherHolder, Error> {
        // TODO(https://fxbug.dev/570053552): Support encrypted remote volumes.
        Err(FxfsError::NotSupported.into())
    }

    async fn purge(
        &self,
        object_id: u64,
        truncate_guard: Option<&TruncateGuard<'_>>,
    ) -> Result<(), Error> {
        self.filesystem.tombstone_object(self.store_object_id, object_id, truncate_guard).await
    }

    fn purge_async(&self, object_id: u64) {
        self.filesystem.queue_tombstone_object(self.store_object_id, object_id);
    }

    fn tree(&self) -> &LSMTree<ObjectKey, ObjectValue> {
        &self.tree
    }

    fn filesystem(&self) -> Arc<dyn Filesystem> {
        self.filesystem.clone()
    }
}

impl HandleOwner for RemoteObjectStore {
    type Store = Self;

    fn store(&self) -> &Self::Store {
        self
    }
}

#[derive(Clone, Debug)]
enum NodeKind {
    Directory(Arc<RemoteDirectory>),
    File(Arc<RemoteFile>),
    Symlink(Arc<RemoteSymlink>),
}

impl NodeKind {
    fn get_attributes(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Error> {
        if query.is_empty() {
            return Ok(fio::NodeAttributes2 {
                mutable_attributes: fio::MutableNodeAttributes::default(),
                immutable_attributes: fio::ImmutableNodeAttributes::default(),
            });
        }
        match self {
            Self::Directory(dir) => dir.get_attributes(query),
            Self::File(file) => file.get_attributes(query),
            Self::Symlink(symlink) => symlink.get_attributes(query),
        }
    }

    fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Error> {
        match self {
            Self::Directory(dir) => dir.get_extended_attribute(name),
            Self::File(file) => file.get_extended_attribute(name),
            Self::Symlink(symlink) => symlink.get_extended_attribute(name),
        }
    }

    fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Error> {
        match self {
            Self::Directory(dir) => dir.list_extended_attributes(),
            Self::File(file) => file.list_extended_attributes(),
            Self::Symlink(symlink) => symlink.list_extended_attributes(),
        }
    }

    fn dirent_type(&self) -> fio::DirentType {
        match self {
            Self::Directory(_) => fio::DirentType::Directory,
            Self::File(_) => fio::DirentType::File,
            Self::Symlink(_) => fio::DirentType::Symlink,
        }
    }
}

/// Opens `path` relative to `directory`, making the checks fuchsia.io's `Directory.Open` makes.
fn open_node(directory: &NodeKind, path: &str, flags: fio::Flags) -> Result<NodeKind, zx::Status> {
    if flags.intersects(MUTATING_OPEN_FLAGS) {
        return Err(zx::Status::NOT_SUPPORTED);
    }
    let path = Path::validate_and_split(path)?;
    let NodeKind::Directory(directory) = directory else {
        return Err(zx::Status::NOT_DIR);
    };
    let must_be_directory = path.is_dir();
    let kind = directory.walk(path).map_err(map_to_status)?;
    if must_be_directory && !matches!(kind, NodeKind::Directory(_)) {
        return Err(zx::Status::NOT_DIR);
    }
    let protocols: &dyn ProtocolsExt = &flags;
    if protocols.is_node() {
        protocols.to_node_options(kind.dirent_type())?;
    } else {
        match &kind {
            NodeKind::Directory(_) => {
                protocols.to_directory_options()?;
            }
            NodeKind::File(_) => {
                if !protocols.is_file_allowed() && protocols.is_dir_allowed() {
                    return Err(zx::Status::NOT_DIR);
                }
                protocols.to_file_options()?;
            }
            NodeKind::Symlink(_) => {
                protocols.to_symlink_options()?;
            }
        }
    }
    Ok(kind)
}

fn optional_attributes(
    kind: &NodeKind,
    query: fio::NodeAttributesQuery,
) -> Result<Option<fio::NodeAttributes2>, zx::Status> {
    if query.is_empty() {
        Ok(None)
    } else {
        kind.get_attributes(query).map(Some).map_err(map_to_status)
    }
}

/// Fetches any requested attributes (and the target for non-node symlink opens) and hands `kind`
/// to `factory` as the kind of object it is (or as a node reference when `flags.is_node()` is set).
fn create_with_factory<F: Factory<LocalFxIo>>(
    factory: F,
    kind: NodeKind,
    flags: fio::Flags,
    query: fio::NodeAttributesQuery,
) -> Result<F::Result, zx::Status> {
    if flags.is_node() {
        let attributes = optional_attributes(&kind, query)?;
        return Ok(factory
            .create_node(LocalFxIo { kind }, fio::NodeInfo { attributes, ..Default::default() }));
    }
    Ok(match &kind {
        NodeKind::Directory(_) => {
            let attributes = optional_attributes(&kind, query)?;
            factory.create_directory(
                LocalFxIo { kind },
                fio::DirectoryInfo { attributes, ..Default::default() },
            )
        }
        NodeKind::File(_) => {
            let attributes = optional_attributes(&kind, query)?;
            factory
                .create_file(LocalFxIo { kind }, fio::FileInfo { attributes, ..Default::default() })
        }
        NodeKind::Symlink(symlink) => {
            let (target, attributes) =
                symlink.read_target_and_attributes(query).map_err(map_to_status)?;
            factory.create_symlink(
                LocalFxIo { kind },
                fio::SymlinkInfo { target: Some(target), attributes, ..Default::default() },
            )
        }
    })
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
        let attributes = self.kind.get_attributes(query).map_err(map_to_status)?;
        Ok((attributes.mutable_attributes, attributes.immutable_attributes))
    }

    fn attr_set(&self, _attributes: fio::MutableNodeAttributes) -> Result<(), zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement attr_set.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn query_filesystem(&self) -> Result<fio::FilesystemInfo, zx::Status> {
        // TODO(https://fxbug.dev/570053552): Implement query_filesystem for remote volumes.
        Err(zx::Status::NOT_SUPPORTED)
    }

    fn open<F: Factory<Self>>(
        &self,
        path: &str,
        flags: fio::Flags,
        create_attributes: Option<fio::MutableNodeAttributes>,
        query: fio::NodeAttributesQuery,
        factory: F,
    ) -> Result<F::Result, zx::Status> {
        if create_attributes.is_some() && flags.creation_mode() == CreationMode::Never {
            return Err(zx::Status::INVALID_ARGS);
        }
        let kind = open_node(&self.kind, path, flags)?;
        create_with_factory(factory, kind, flags, query)
    }

    fn open_pipelined<F: Factory<Self>>(
        &self,
        paths: &[&str],
        flags: fio::Flags,
        query: fio::NodeAttributesQuery,
        mut factory_fn: impl FnMut() -> F,
    ) -> impl Iterator<Item = Result<F::Result, zx::Status>> {
        if paths.is_empty() {
            return Vec::new().into_iter();
        }
        if !fio::MASK_KNOWN_PERMISSIONS.contains(flags) {
            return vec![Err(zx::Status::INVALID_ARGS)].into_iter();
        }
        let mut results = Vec::with_capacity(paths.len());
        let mut kind = self.kind.clone();
        for (index, path) in paths.iter().enumerate() {
            let mut flags = flags;
            if index + 1 < paths.len() {
                flags |= fio::Flags::PROTOCOL_DIRECTORY | fio::Flags::PROTOCOL_SYMLINK;
            }
            let opened = open_node(&kind, path, flags).and_then(|child| {
                let result = create_with_factory(factory_fn(), child.clone(), flags, query)?;
                Ok((child, result))
            });
            match opened {
                Ok((child, result)) => {
                    results.push(Ok(result));
                    kind = child;
                }
                Err(status) => {
                    results.push(Err(status));
                    break;
                }
            }
        }
        results.into_iter()
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

    fn xattr_get(&self, name: &[u8]) -> Result<Vec<u8>, zx::Status> {
        self.kind.get_extended_attribute(name.to_vec()).map_err(map_to_status)
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
        self.kind.list_extended_attributes().map_err(map_to_status)
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
    use super::LocalFxIo;
    use crate::testing::{
        EMPTY_FILE_PATH, FILE_CONTENTS, FILE_GID, FILE_MODE, FILE_PATH, FILE_RDEV,
        FILE_SELINUX_CONTEXT, FILE_UID, FILE_XATTR_NAME, FILE_XATTR_VALUE, LARGE_XATTR_NAME,
        LONG_SYMLINK_PATH, LONG_SYMLINK_TARGET, MountedVolume, NESTED_FILE_PATH, SUBDIR_GID,
        SUBDIR_MODE, SUBDIR_PATH, SUBDIR_SELINUX_CONTEXT, SUBDIR_UID, SUBDIR_XATTR_NAME,
        SUBDIR_XATTR_VALUE, SYMLINK_GID, SYMLINK_MODE, SYMLINK_PATH, SYMLINK_SELINUX_CONTEXT,
        SYMLINK_TARGET, SYMLINK_UID, SYMLINK_XATTR_NAME, SYMLINK_XATTR_VALUE, TestVolume,
        VERITY_FILE_PATH,
    };
    use fidl_fuchsia_io as fio;
    use futures::executor::block_on;
    use fxfs::errors::FxfsError;
    use fxfs::filesystem::{Filesystem, SyncOptions};
    use fxfs::object_store::transaction::{
        self, LockKey, LockKeys, Options, TransactionSink, lock_keys,
    };
    use fxfs::object_store::{
        AttributeId, AttributeKey, Directory, FSCRYPT_KEY_ID, FscryptDirInfo, FscryptPolicyFlags,
        HandleOptions, MAX_INLINE_XATTR_SIZE, Mutation, ObjectKey, ObjectKind, ObjectStoreTrait,
        ObjectValue, PosixAttributes, SetExtendedAttributeMode, StoreObjectHandle,
        VOLUME_DATA_KEY_ID,
    };
    use fxfs_crypto::Crypt;
    use fxfs_insecure_crypto::new_insecure_crypt;
    use std::sync::Arc;
    use sync_io_client::{AllocateMode, Factory, SyncIo, XattrSetMode, zxio_fsverity_descriptor_t};

    const BASIC_ATTRIBUTES: fio::NodeAttributesQuery = fio::NodeAttributesQuery::ID
        .union(fio::NodeAttributesQuery::PROTOCOLS)
        .union(fio::NodeAttributesQuery::ABILITIES)
        .union(fio::NodeAttributesQuery::MODE)
        .union(fio::NodeAttributesQuery::UID)
        .union(fio::NodeAttributesQuery::GID)
        .union(fio::NodeAttributesQuery::CONTENT_SIZE)
        .union(fio::NodeAttributesQuery::LINK_COUNT);

    /// Every attribute a query can ask for that isn't in `BASIC_ATTRIBUTES`. This includes
    /// `PENDING_ACCESS_TIME_UPDATE`, which is ignored because the volume is read-only.
    const OTHER_ATTRIBUTES: fio::NodeAttributesQuery =
        fio::NodeAttributesQuery::all().difference(BASIC_ATTRIBUTES);

    #[derive(Debug)]
    enum Opened {
        Node(LocalFxIo, Option<fio::NodeAttributes2>),
        Directory(LocalFxIo, Option<fio::NodeAttributes2>),
        File(LocalFxIo, Option<fio::NodeAttributes2>),
        Symlink(LocalFxIo, fio::SymlinkInfo),
    }

    impl Opened {
        fn io(&self) -> &LocalFxIo {
            match self {
                Self::Node(io, _)
                | Self::Directory(io, _)
                | Self::File(io, _)
                | Self::Symlink(io, _) => io,
            }
        }

        fn attributes(&self) -> Option<&fio::NodeAttributes2> {
            match self {
                Self::Node(_, attributes)
                | Self::Directory(_, attributes)
                | Self::File(_, attributes) => attributes.as_ref(),
                Self::Symlink(_, info) => info.attributes.as_ref(),
            }
        }

        fn expect_node(self) -> Self {
            assert!(matches!(self, Self::Node(..)), "expected a node, got {self:?}");
            self
        }

        fn expect_directory(self) -> Self {
            assert!(matches!(self, Self::Directory(..)), "expected a directory, got {self:?}");
            self
        }

        fn expect_file(self) -> Self {
            assert!(matches!(self, Self::File(..)), "expected a file, got {self:?}");
            self
        }

        fn expect_symlink(self) -> Self {
            assert!(matches!(self, Self::Symlink(..)), "expected a symlink, got {self:?}");
            self
        }

        /// The target, if this is a symlink.
        fn target(&self) -> Option<&[u8]> {
            match self {
                Self::Symlink(_, info) => info.target.as_deref(),
                _ => None,
            }
        }
    }

    struct TestFactory;

    impl Factory<LocalFxIo> for TestFactory {
        type Result = Opened;

        fn create_node(self, io: LocalFxIo, info: fio::NodeInfo) -> Opened {
            Opened::Node(io, info.attributes)
        }

        fn create_directory(self, io: LocalFxIo, info: fio::DirectoryInfo) -> Opened {
            Opened::Directory(io, info.attributes)
        }

        fn create_file(self, io: LocalFxIo, info: fio::FileInfo) -> Opened {
            Opened::File(io, info.attributes)
        }

        fn create_symlink(self, io: LocalFxIo, info: fio::SymlinkInfo) -> Opened {
            Opened::Symlink(io, info)
        }
    }

    fn open(io: &LocalFxIo, path: &str) -> Result<Opened, zx::Status> {
        open_with_flags(io, path, fio::PERM_READABLE)
    }

    fn open_with_flags(
        io: &LocalFxIo,
        path: &str,
        flags: fio::Flags,
    ) -> Result<Opened, zx::Status> {
        io.open(path, flags, None, BASIC_ATTRIBUTES, TestFactory)
    }

    fn file_attributes(id: u64, posix_attributes: Option<PosixAttributes>) -> fio::NodeAttributes2 {
        fio::NodeAttributes2 {
            mutable_attributes: fio::MutableNodeAttributes {
                mode: posix_attributes.map(|a| a.mode),
                uid: posix_attributes.map(|a| a.uid),
                gid: posix_attributes.map(|a| a.gid),
                ..Default::default()
            },
            immutable_attributes: fio::ImmutableNodeAttributes {
                protocols: Some(fio::NodeProtocolKinds::FILE),
                abilities: Some(fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES),
                content_size: Some(0),
                link_count: Some(1),
                id: Some(id),
                ..Default::default()
            },
        }
    }

    fn directory_attributes(
        id: u64,
        link_count: u64,
        posix_attributes: Option<PosixAttributes>,
    ) -> fio::NodeAttributes2 {
        fio::NodeAttributes2 {
            mutable_attributes: fio::MutableNodeAttributes {
                mode: posix_attributes.map(|a| a.mode),
                uid: posix_attributes.map(|a| a.uid),
                gid: posix_attributes.map(|a| a.gid),
                ..Default::default()
            },
            immutable_attributes: fio::ImmutableNodeAttributes {
                protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                abilities: Some(
                    fio::Operations::GET_ATTRIBUTES
                        | fio::Operations::ENUMERATE
                        | fio::Operations::TRAVERSE,
                ),
                content_size: Some(0),
                link_count: Some(link_count),
                id: Some(id),
                ..Default::default()
            },
        }
    }

    fn symlink_attributes(
        id: u64,
        target_len: usize,
        posix_attributes: Option<PosixAttributes>,
    ) -> fio::NodeAttributes2 {
        fio::NodeAttributes2 {
            mutable_attributes: fio::MutableNodeAttributes {
                mode: posix_attributes.map(|a| a.mode),
                uid: posix_attributes.map(|a| a.uid),
                gid: posix_attributes.map(|a| a.gid),
                ..Default::default()
            },
            immutable_attributes: fio::ImmutableNodeAttributes {
                protocols: Some(fio::NodeProtocolKinds::SYMLINK),
                abilities: Some(fio::Operations::GET_ATTRIBUTES),
                content_size: Some(target_len as u64),
                link_count: Some(1),
                id: Some(id),
                ..Default::default()
            },
        }
    }

    /// The attributes of [`SYMLINK_PATH`].
    fn short_symlink_attributes(test_volume: &TestVolume) -> fio::NodeAttributes2 {
        symlink_attributes(
            test_volume.symlink_object_id(),
            SYMLINK_TARGET.len(),
            Some(PosixAttributes {
                mode: SYMLINK_MODE,
                uid: SYMLINK_UID,
                gid: SYMLINK_GID,
                rdev: 0,
            }),
        )
    }

    /// The attributes of [`LONG_SYMLINK_PATH`].
    fn long_symlink_attributes(test_volume: &TestVolume) -> fio::NodeAttributes2 {
        symlink_attributes(test_volume.long_symlink_object_id(), LONG_SYMLINK_TARGET.len(), None)
    }

    #[fuchsia::test]
    fn create_and_teardown() {
        let test_volume = TestVolume::new();
        let _root = test_volume.volume().root();
    }

    #[fuchsia::test]
    fn remote_filesystem() {
        let test_volume = TestVolume::new();
        let fs = &test_volume.volume().store.filesystem;
        assert!(fs.is_remote());
        assert_ne!(fs.block_size().get(), 0);
        assert!(fs.layer_pager().is_none());
        assert!(!fs.options().read_only);
        assert!(!fs.image_builder_mode());
        assert!(fs.metadata_reservation().is_none());
        assert!(!fs.is_system_store(1));
        assert!(fs.reserve(None, 0).is_none());

        let err = block_on(
            fs.clone().new_transaction(LockKeys::default(), transaction::Options::default()),
        )
        .expect_err("new_transaction should fail");
        assert!(FxfsError::NotSupported.matches(&err));

        let err = block_on(fs.check_space()).expect_err("check_space should fail");
        assert!(FxfsError::NotSupported.matches(&err));

        let err =
            block_on(fs.tombstone_object(1, 2, None)).expect_err("tombstone_object should fail");
        assert!(FxfsError::NotSupported.matches(&err));

        let err = block_on(fs.sync(SyncOptions::default())).expect_err("sync should fail");
        assert!(FxfsError::NotSupported.matches(&err));
    }

    #[fuchsia::test]
    fn remote_object_store() {
        let test_volume = TestVolume::new();
        let store: &dyn ObjectStoreTrait = &*test_volume.volume().store;
        assert!(store.filesystem().is_remote());

        let root_id = test_volume.root_directory_object_id();
        let item = block_on(store.tree().find(&ObjectKey::object(root_id)))
            .expect("find root directory failed")
            .expect("root directory should exist in layer set");
        assert_eq!(item.key, ObjectKey::object(root_id));
        assert!(
            matches!(item.value, ObjectValue::Object { kind: ObjectKind::Directory { .. }, .. }),
            "unexpected root directory value: {:?}",
            item.value,
        );

        let err = block_on(store.purge(root_id, None)).expect_err("purge should fail");
        assert!(FxfsError::NotSupported.matches(&err));

        assert!(!store.is_encrypted());
        let err =
            block_on(store.get_fscrypt_key(root_id)).err().expect("get_fscrypt_key should fail");
        assert!(FxfsError::NotSupported.matches(&err));
    }

    #[fuchsia::test]
    fn unimplemented_op_returns_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        assert_eq!(root.query_filesystem().err(), Some(zx::Status::NOT_SUPPORTED));
        assert_eq!(root.open_directory().err(), Some(zx::Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    fn mutating_ops_return_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let file = open(&root, FILE_PATH).expect("open failed");
        let file = file.io();
        assert_eq!(
            root.attr_set(fio::MutableNodeAttributes::default()),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(file.link_into(&root, "link"), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(
            root.unlink(FILE_PATH, fio::UnlinkFlags::empty()),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(root.rename(FILE_PATH, &root, "renamed"), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(root.create_symlink("link", b"target").err(), Some(zx::Status::NOT_SUPPORTED));
        assert_eq!(file.write(0, b"data"), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(file.writev(0, &[]), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(file.truncate(0), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(
            file.enable_verity(&zxio_fsverity_descriptor_t::default()),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(file.allocate(0, 1, AllocateMode::empty()), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(
            file.xattr_set(b"name", b"value", XattrSetMode::Set),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(file.xattr_remove(b"name"), Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(
            root.xattr_set(b"name", b"value", XattrSetMode::Set),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(root.xattr_remove(b"name"), Err(zx::Status::NOT_SUPPORTED));
        let symlink = open(&root, SYMLINK_PATH).expect("open failed");
        assert_eq!(
            symlink.io().xattr_set(b"name", b"value", XattrSetMode::Set),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(symlink.io().xattr_remove(b"name"), Err(zx::Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    fn open_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let opened = open(&root, FILE_PATH).expect("open failed").expect_file();
        assert_eq!(
            opened.attributes(),
            Some(&file_attributes(
                test_volume.file_object_id(),
                Some(PosixAttributes {
                    mode: FILE_MODE,
                    uid: FILE_UID,
                    gid: FILE_GID,
                    rdev: FILE_RDEV,
                }),
            ))
        );
    }

    #[fuchsia::test]
    fn open_file_without_posix_attributes() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let opened = open(&root, EMPTY_FILE_PATH).expect("open failed").expect_file();
        assert_eq!(
            opened.attributes(),
            Some(&file_attributes(test_volume.empty_file_object_id(), None))
        );
    }

    #[fuchsia::test]
    fn open_directory() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let opened = open(&root, SUBDIR_PATH).expect("open failed").expect_directory();
        // The directory has no subdirectories, so its links are its entry and its own `.`.
        assert_eq!(
            opened.attributes(),
            Some(&directory_attributes(
                test_volume.subdir_object_id(),
                2,
                Some(PosixAttributes {
                    mode: SUBDIR_MODE,
                    uid: SUBDIR_UID,
                    gid: SUBDIR_GID,
                    rdev: 0,
                }),
            ))
        );
    }

    #[fuchsia::test]
    fn open_nested_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let expected = file_attributes(test_volume.nested_file_object_id(), None);
        for path in
            [NESTED_FILE_PATH.to_string(), format!("/{NESTED_FILE_PATH}"), "dir/NESTED".to_string()]
        {
            let opened = open(&root, &path).expect("open failed").expect_file();
            assert_eq!(opened.attributes(), Some(&expected), "{path}");
        }

        let directory = open(&root, SUBDIR_PATH).expect("open failed");
        let nested_name = NESTED_FILE_PATH.strip_prefix("dir/").expect("nested file path");
        for name in [nested_name, &nested_name.to_uppercase()] {
            let opened = open(directory.io(), name).expect("open failed").expect_file();
            assert_eq!(opened.attributes(), Some(&expected), "{name}");
        }
    }

    #[fuchsia::test]
    fn open_dot_returns_directory_itself() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let flags = fio::Flags::PROTOCOL_DIRECTORY
            | fio::PERM_READABLE
            | fio::Flags::PERM_INHERIT_WRITE
            | fio::Flags::PERM_INHERIT_EXECUTE;
        // The root's links are its entry, its own `.`, and the `..` of its one subdirectory.
        let expected = directory_attributes(test_volume.root_directory_object_id(), 3, None);
        for path in [".", "/"] {
            let opened = open_with_flags(&root, path, flags).expect("open failed");
            assert_eq!(opened.expect_directory().attributes(), Some(&expected), "{path}");
        }

        let directory = open(&root, SUBDIR_PATH).expect("open failed");
        let opened = open(directory.io(), ".").expect("open failed").expect_directory();
        assert_eq!(
            opened.attributes().and_then(|a| a.immutable_attributes.id),
            Some(test_volume.subdir_object_id())
        );
    }

    #[fuchsia::test]
    fn open_with_trailing_slash_requires_directory() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        open(&root, &format!("{SUBDIR_PATH}/")).expect("open failed").expect_directory();
        assert_eq!(open(&root, &format!("{FILE_PATH}/")).err(), Some(zx::Status::NOT_DIR));
    }

    #[fuchsia::test]
    fn open_missing_returns_not_found() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        for path in ["missing", "dir/missing", "missing/file"] {
            assert_eq!(open(&root, path).err(), Some(zx::Status::NOT_FOUND), "{path}");
        }
    }

    #[fuchsia::test]
    fn open_through_file_returns_not_dir() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        assert_eq!(open(&root, &format!("{FILE_PATH}/x")).err(), Some(zx::Status::NOT_DIR));
        let file = open(&root, FILE_PATH).expect("open failed");
        for path in [".", "x"] {
            assert_eq!(open(file.io(), path).err(), Some(zx::Status::NOT_DIR), "{path}");
        }
    }

    #[fuchsia::test]
    fn open_invalid_path() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        for path in ["", "//", "dir//nested", "./file", "dir/.", "..", "dir/..", "dir\0"] {
            assert_eq!(open(&root, path).err(), Some(zx::Status::INVALID_ARGS), "{path:?}");
        }
        let long_name = "a".repeat(fio::MAX_NAME_LENGTH as usize + 1);
        assert_eq!(open(&root, &long_name).err(), Some(zx::Status::BAD_PATH));
        let long_path = "a/".repeat(fio::MAX_PATH_LENGTH as usize / 2 + 1);
        assert_eq!(open(&root, &long_path).err(), Some(zx::Status::BAD_PATH));
    }

    #[fuchsia::test]
    fn open_checks_protocol() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let open_file = |flags| open_with_flags(&root, FILE_PATH, fio::PERM_READABLE | flags);
        let open_dir = |flags| open_with_flags(&root, SUBDIR_PATH, fio::PERM_READABLE | flags);

        open_file(fio::Flags::PROTOCOL_FILE).expect("open failed").expect_file();
        open_file(fio::Flags::FILE_APPEND).expect("open failed").expect_file();
        open_dir(fio::Flags::PROTOCOL_DIRECTORY).expect("open failed").expect_directory();
        open_dir(fio::Flags::PROTOCOL_FILE | fio::Flags::PROTOCOL_DIRECTORY)
            .expect("open failed")
            .expect_directory();

        assert_eq!(open_file(fio::Flags::PROTOCOL_DIRECTORY).err(), Some(zx::Status::NOT_DIR));
        assert_eq!(open_dir(fio::Flags::PROTOCOL_FILE).err(), Some(zx::Status::NOT_FILE));
        assert_eq!(
            open_file(fio::Flags::PROTOCOL_DIRECTORY | fio::Flags::PROTOCOL_SYMLINK).err(),
            Some(zx::Status::NOT_DIR)
        );
        assert_eq!(open_file(fio::Flags::PROTOCOL_SYMLINK).err(), Some(zx::Status::WRONG_TYPE));
        assert_eq!(open_dir(fio::Flags::PROTOCOL_SERVICE).err(), Some(zx::Status::WRONG_TYPE));
        assert_eq!(open_dir(fio::Flags::FILE_APPEND).err(), Some(zx::Status::INVALID_ARGS));

        open_with_flags(&root, FILE_PATH, fio::Flags::PROTOCOL_NODE)
            .expect("open failed")
            .expect_node();
        open_with_flags(&root, SUBDIR_PATH, fio::Flags::PROTOCOL_NODE)
            .expect("open failed")
            .expect_node();
        assert_eq!(
            open_with_flags(
                &root,
                FILE_PATH,
                fio::Flags::PROTOCOL_NODE | fio::Flags::PROTOCOL_DIRECTORY,
            )
            .err(),
            Some(zx::Status::NOT_DIR)
        );
        assert_eq!(
            open_with_flags(
                &root,
                SUBDIR_PATH,
                fio::Flags::PROTOCOL_NODE | fio::Flags::PROTOCOL_FILE,
            )
            .err(),
            Some(zx::Status::NOT_FILE)
        );
    }

    #[fuchsia::test]
    fn open_mutating_flags_return_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        for flags in [
            fio::Flags::FLAG_MAYBE_CREATE,
            fio::Flags::FLAG_MUST_CREATE,
            fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY,
            fio::Flags::FILE_TRUNCATE,
            fio::PERM_WRITABLE,
            fio::Flags::PERM_WRITE_BYTES,
            fio::Flags::PERM_MODIFY_DIRECTORY,
            fio::Flags::PERM_UPDATE_ATTRIBUTES,
        ] {
            for path in [FILE_PATH, "missing", "."] {
                assert_eq!(
                    open_with_flags(&root, path, fio::PERM_READABLE | flags).err(),
                    Some(zx::Status::NOT_SUPPORTED),
                    "{path}: {flags:?}"
                );
            }
        }

        let create_attributes =
            fio::MutableNodeAttributes { mode: Some(0o100644), ..Default::default() };
        assert_eq!(
            root.open(
                "missing",
                fio::PERM_READABLE | fio::Flags::FLAG_MUST_CREATE,
                Some(create_attributes.clone()),
                BASIC_ATTRIBUTES,
                TestFactory,
            )
            .err(),
            Some(zx::Status::NOT_SUPPORTED)
        );
        // Without a create flag there is nothing to apply the attributes to.
        assert_eq!(
            root.open(
                FILE_PATH,
                fio::PERM_READABLE,
                Some(create_attributes),
                BASIC_ATTRIBUTES,
                TestFactory,
            )
            .err(),
            Some(zx::Status::INVALID_ARGS)
        );
    }

    #[fuchsia::test]
    fn open_symlink() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let expected = short_symlink_attributes(&test_volume);
        for flags in [fio::PERM_READABLE, fio::PERM_READABLE | fio::Flags::PROTOCOL_SYMLINK] {
            let opened =
                open_with_flags(&root, SYMLINK_PATH, flags).expect("open failed").expect_symlink();
            assert_eq!(opened.target(), Some(SYMLINK_TARGET), "{flags:?}");
            assert_eq!(opened.attributes(), Some(&expected), "{flags:?}");
        }
        let opened = root
            .open(
                SYMLINK_PATH,
                fio::PERM_READABLE,
                None,
                fio::NodeAttributesQuery::empty(),
                TestFactory,
            )
            .expect("open without query failed")
            .expect_symlink();
        assert_eq!(opened.target(), Some(SYMLINK_TARGET));
        assert_eq!(opened.attributes(), None);
    }

    // TODO(https://fxbug.dev/570053552): Flip this once the FIDL fallback exists.
    #[fuchsia::test]
    fn open_long_symlink_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        assert_eq!(open(&root, LONG_SYMLINK_PATH).err(), Some(zx::Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    fn open_symlink_as_node() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        // Opening a node doesn't read the target, so it works for long symlinks too.
        for (path, expected) in [
            (SYMLINK_PATH, short_symlink_attributes(&test_volume)),
            (LONG_SYMLINK_PATH, long_symlink_attributes(&test_volume)),
        ] {
            let opened = open_with_flags(&root, path, fio::Flags::PROTOCOL_NODE)
                .expect("open failed")
                .expect_node();
            assert_eq!(opened.attributes(), Some(&expected), "{path}");
        }
    }

    #[fuchsia::test]
    fn open_symlink_checks_protocol() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let open_symlink = |flags| open_with_flags(&root, SYMLINK_PATH, flags);

        open_symlink(
            fio::PERM_READABLE | fio::Flags::PROTOCOL_SYMLINK | fio::Flags::PROTOCOL_DIRECTORY,
        )
        .expect("open failed")
        .expect_symlink();

        // Like `FxDirectory`, a symlink that doesn't match the protocol fails with WRONG_TYPE, even
        // when a directory or file is asked for.
        for flags in [
            fio::Flags::PROTOCOL_DIRECTORY,
            fio::Flags::PROTOCOL_FILE,
            fio::Flags::PROTOCOL_DIRECTORY | fio::Flags::PROTOCOL_FILE,
            fio::Flags::PROTOCOL_SERVICE,
        ] {
            assert_eq!(
                open_symlink(fio::PERM_READABLE | flags).err(),
                Some(zx::Status::WRONG_TYPE),
                "{flags:?}"
            );
        }
        // Symlink connections need the right to get attributes.
        assert_eq!(
            open_symlink(fio::Flags::PROTOCOL_SYMLINK).err(),
            Some(zx::Status::INVALID_ARGS)
        );
    }

    #[fuchsia::test]
    fn open_through_symlink_returns_not_dir() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        // fuchsia.io doesn't follow symlinks, so a symlink in a path is like any other
        // non-directory, whether or not its target can be read.
        for path in [SYMLINK_PATH, LONG_SYMLINK_PATH] {
            assert_eq!(
                open(&root, &format!("{path}/x")).err(),
                Some(zx::Status::NOT_DIR),
                "{path}"
            );
        }
        let symlink = open(&root, SYMLINK_PATH).expect("open failed");
        for path in [".", "x"] {
            assert_eq!(open(symlink.io(), path).err(), Some(zx::Status::NOT_DIR), "{path}");
        }
    }

    #[fuchsia::test]
    fn open_returns_requested_attributes_only() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let opened = root
            .open(
                FILE_PATH,
                fio::PERM_READABLE,
                None,
                fio::NodeAttributesQuery::empty(),
                TestFactory,
            )
            .expect("open failed");
        assert_eq!(opened.attributes(), None);

        let opened = root
            .open(
                FILE_PATH,
                fio::PERM_READABLE,
                None,
                fio::NodeAttributesQuery::ID | fio::NodeAttributesQuery::LINK_COUNT,
                TestFactory,
            )
            .expect("open failed");
        assert_eq!(
            opened.attributes(),
            Some(&fio::NodeAttributes2 {
                mutable_attributes: Default::default(),
                immutable_attributes: fio::ImmutableNodeAttributes {
                    id: Some(test_volume.file_object_id()),
                    link_count: Some(1),
                    ..Default::default()
                },
            })
        );
    }

    #[fuchsia::test]
    fn attr_get_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let file = open(&root, FILE_PATH).expect("open failed");
        assert_eq!(
            file.io().attr_get(fio::NodeAttributesQuery::empty()),
            Ok((fio::MutableNodeAttributes::default(), fio::ImmutableNodeAttributes::default()))
        );
        let expected = file_attributes(
            test_volume.file_object_id(),
            Some(PosixAttributes {
                mode: FILE_MODE,
                uid: FILE_UID,
                gid: FILE_GID,
                rdev: FILE_RDEV,
            }),
        );
        assert_eq!(
            file.io().attr_get(BASIC_ATTRIBUTES),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );
        let (mutable, immutable) = file.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert_eq!(mutable.rdev, Some(FILE_RDEV));
        assert!(immutable.storage_size.unwrap() >= FILE_CONTENTS.len() as u64);
        assert!(mutable.creation_time.unwrap() > 0);
        assert!(mutable.modification_time.unwrap() > 0);
        assert!(mutable.access_time.unwrap() > 0);
        assert!(immutable.change_time.unwrap() > 0);
        // Files don't report casefold.
        assert_eq!(mutable.casefold, None);
        assert_eq!(mutable.encryption_policy, None);
        assert_eq!(
            mutable.selinux_context,
            Some(fio::SelinuxContext::Data(FILE_SELINUX_CONTEXT.to_vec()))
        );
        assert_eq!(immutable.verity_enabled, Some(false));
        assert_eq!(immutable.options, None);
        assert_eq!(immutable.root_hash, None);
    }

    #[fuchsia::test]
    fn attr_get_verity_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let query = BASIC_ATTRIBUTES | fio::NodeAttributesQuery::VERITY_ENABLED;
        let file = root
            .open(VERITY_FILE_PATH, fio::PERM_READABLE, None, query, TestFactory)
            .expect("open failed")
            .expect_file();
        let mut expected = file_attributes(test_volume.verity_file_object_id(), None);
        expected.immutable_attributes.verity_enabled = Some(true);
        assert_eq!(file.attributes(), Some(&expected));
        assert_eq!(
            file.io().attr_get(query),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );

        // TODO(https://fxbug.dev/570053552): Flip these once the FIDL fallback exists.
        for query in [fio::NodeAttributesQuery::OPTIONS, fio::NodeAttributesQuery::ROOT_HASH] {
            assert_eq!(
                file.io().attr_get(query).err(),
                Some(zx::Status::NOT_SUPPORTED),
                "{query:?}"
            );
        }
    }

    #[fuchsia::test]
    fn attr_get_out_of_line_selinux_context() {
        const LARGE_FILE_PATH: &str = "large_selinux_file";
        const REUSED_FILE_PATH: &str = "reused_selinux_file";
        const LARGE_DIR_PATH: &str = "large_selinux_dir";
        const LARGE_SYMLINK_PATH: &str = "large_selinux_symlink";
        const LARGE_CONTEXT: &[u8] = &[b'a'; MAX_INLINE_XATTR_SIZE + 1];
        const SHORT_CONTEXT: &[u8] = b"u:object_r:short_test:s0";

        let (mounted, ()) = MountedVolume::start(None, |store| async move {
            let root_id = store.root_directory_object_id();
            let root_dir = Directory::open(&store, root_id).await.expect("open root failed");
            let mut transaction = store
                .filesystem()
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_id)],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let large_file = root_dir
                .create_child_file(&mut transaction, LARGE_FILE_PATH)
                .await
                .expect("create large file failed");
            let reused_file = root_dir
                .create_child_file(&mut transaction, REUSED_FILE_PATH)
                .await
                .expect("create reused file failed");
            let large_dir = root_dir
                .create_child_dir(&mut transaction, LARGE_DIR_PATH)
                .await
                .expect("create large dir failed");
            let large_symlink_id = root_dir
                .create_symlink(&mut transaction, LARGE_FILE_PATH.as_bytes(), LARGE_SYMLINK_PATH)
                .await
                .expect("create large symlink failed");
            transaction.commit().await.expect("commit failed");

            large_file
                .set_extended_attribute(
                    fio::SELINUX_CONTEXT_NAME.into(),
                    LARGE_CONTEXT.to_vec(),
                    SetExtendedAttributeMode::Set,
                )
                .await
                .expect("set large file selinux context failed");

            // Overwriting a >256-byte xattr with a short value keeps it stored out of line in the
            // previously allocated attribute.
            reused_file
                .set_extended_attribute(
                    fio::SELINUX_CONTEXT_NAME.into(),
                    LARGE_CONTEXT.to_vec(),
                    SetExtendedAttributeMode::Set,
                )
                .await
                .expect("set reused file large selinux context failed");
            reused_file
                .set_extended_attribute(
                    fio::SELINUX_CONTEXT_NAME.into(),
                    SHORT_CONTEXT.to_vec(),
                    SetExtendedAttributeMode::Set,
                )
                .await
                .expect("overwrite reused file selinux context failed");

            large_dir
                .set_extended_attribute(
                    fio::SELINUX_CONTEXT_NAME.into(),
                    LARGE_CONTEXT.to_vec(),
                    SetExtendedAttributeMode::Set,
                )
                .await
                .expect("set large dir selinux context failed");

            let large_symlink = StoreObjectHandle::new(
                store.clone(),
                large_symlink_id,
                /* permanent_keys: */ false,
                HandleOptions::default(),
                /* trace: */ false,
            );
            large_symlink
                .set_extended_attribute(
                    fio::SELINUX_CONTEXT_NAME.into(),
                    LARGE_CONTEXT.to_vec(),
                    SetExtendedAttributeMode::Set,
                )
                .await
                .expect("set large symlink selinux context failed");
        });

        let root = mounted.volume().root();
        // TODO(https://fxbug.dev/570053552): Flip these to expect `UseExtendedAttributes` once
        // `xattr_get` has a FIDL fallback for out-of-line extended attributes.
        for path in [LARGE_FILE_PATH, REUSED_FILE_PATH, LARGE_DIR_PATH, LARGE_SYMLINK_PATH] {
            assert_eq!(
                root.open(
                    path,
                    fio::PERM_READABLE,
                    None,
                    fio::NodeAttributesQuery::SELINUX_CONTEXT,
                    TestFactory,
                )
                .err(),
                Some(zx::Status::NOT_SUPPORTED),
                "{path}"
            );

            // Queries that do not ask for SELINUX_CONTEXT still succeed.
            let opened = open(&root, path).expect("open without SELINUX_CONTEXT failed");
            assert!(opened.io().attr_get(BASIC_ATTRIBUTES).is_ok(), "{path}");
            for query in [fio::NodeAttributesQuery::SELINUX_CONTEXT, OTHER_ATTRIBUTES] {
                assert_eq!(
                    opened.io().attr_get(query).err(),
                    Some(zx::Status::NOT_SUPPORTED),
                    "{path}: {query:?}"
                );
            }
            assert_eq!(
                opened.io().xattr_get(fio::SELINUX_CONTEXT_NAME.as_bytes()),
                Err(zx::Status::NOT_SUPPORTED),
                "{path}"
            );
        }
    }

    #[fuchsia::test]
    fn attr_get_encrypted_directory() {
        const ENCRYPTED_DIR_PATH: &str = "encrypted";
        const ENCRYPTED_CASEFOLD_DIR_PATH: &str = "encrypted_casefold";
        const WRAPPING_KEY_ID: [u8; 16] = u128::to_le_bytes(2);
        const CASEFOLD_WRAPPING_KEY_ID: [u8; 16] = u128::to_le_bytes(3);
        const WRAPPING_KEY_BYTES: [u8; 32] = [0x11; 32];
        const CASEFOLD_WRAPPING_KEY_BYTES: [u8; 32] = [0x22; 32];
        const CASEFOLD_FLAGS: fio::FscryptPolicyFlags =
            fio::FscryptPolicyFlags::PAD_16.union(fio::FscryptPolicyFlags::IV_INO_LBLK_32);

        let crypt = Arc::new(new_insecure_crypt());
        crypt
            .add_wrapping_key(WRAPPING_KEY_ID, WRAPPING_KEY_BYTES)
            .expect("add_wrapping_key failed");
        crypt
            .add_wrapping_key(CASEFOLD_WRAPPING_KEY_ID, CASEFOLD_WRAPPING_KEY_BYTES)
            .expect("add_wrapping_key failed");

        let (mounted, ()) =
            MountedVolume::start(Some(crypt as Arc<dyn Crypt>), |store| async move {
                let root_id = store.root_directory_object_id();
                let root_dir = Directory::open(&store, root_id).await.expect("open root failed");
                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), root_id)],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let encrypted_dir = root_dir
                    .create_child_dir(&mut transaction, ENCRYPTED_DIR_PATH)
                    .await
                    .expect("create encrypted dir failed");
                let encrypted_casefold_dir = root_dir
                    .create_child_dir(&mut transaction, ENCRYPTED_CASEFOLD_DIR_PATH)
                    .await
                    .expect("create encrypted casefold dir failed");
                transaction.commit().await.expect("commit failed");

                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            encrypted_dir.object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                encrypted_dir
                    .set_wrapping_key(&mut transaction, WRAPPING_KEY_ID)
                    .await
                    .expect("set_wrapping_key failed");
                transaction.commit().await.expect("commit failed");

                encrypted_casefold_dir.set_casefold(true).await.expect("set_casefold failed");
                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            encrypted_casefold_dir.object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                encrypted_casefold_dir
                    .set_wrapping_key(
                        &mut transaction,
                        FscryptDirInfo::new(
                            CASEFOLD_WRAPPING_KEY_ID,
                            FscryptPolicyFlags::from(CASEFOLD_FLAGS),
                        ),
                    )
                    .await
                    .expect("set_wrapping_key on casefold dir failed");
                transaction.commit().await.expect("commit failed");
            });

        let root = mounted.volume().root();
        let (root_mutable, _) =
            root.attr_get(fio::NodeAttributesQuery::ENCRYPTION_POLICY).expect("attr_get failed");
        assert_eq!(root_mutable.encryption_policy, None);

        let query =
            fio::NodeAttributesQuery::ENCRYPTION_POLICY | fio::NodeAttributesQuery::CASEFOLD;
        let expected_encrypted = fio::FscryptPolicy {
            key_identifier: WRAPPING_KEY_ID,
            flags: fio::FscryptPolicyFlags::PAD_16,
        };
        let dir = root
            .open(ENCRYPTED_DIR_PATH, fio::PERM_READABLE, None, query, TestFactory)
            .expect("open encrypted dir failed")
            .expect_directory();
        assert_eq!(
            dir.attributes().and_then(|a| a.mutable_attributes.encryption_policy),
            Some(expected_encrypted)
        );
        assert_eq!(dir.attributes().and_then(|a| a.mutable_attributes.casefold), Some(false));
        let (mutable, _) = dir.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert_eq!(mutable.encryption_policy, Some(expected_encrypted));
        assert_eq!(mutable.casefold, Some(false));

        let expected_casefold =
            fio::FscryptPolicy { key_identifier: CASEFOLD_WRAPPING_KEY_ID, flags: CASEFOLD_FLAGS };
        let casefold_dir = root
            .open(ENCRYPTED_CASEFOLD_DIR_PATH, fio::PERM_READABLE, None, query, TestFactory)
            .expect("open encrypted casefold dir failed")
            .expect_directory();
        assert_eq!(
            casefold_dir.attributes().and_then(|a| a.mutable_attributes.encryption_policy),
            Some(expected_casefold)
        );
        assert_eq!(
            casefold_dir.attributes().and_then(|a| a.mutable_attributes.casefold),
            Some(true)
        );
        let (mutable, _) = casefold_dir.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert_eq!(mutable.encryption_policy, Some(expected_casefold));
        assert_eq!(mutable.casefold, Some(true));
    }

    #[fuchsia::test]
    fn attr_get_directory() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        assert_eq!(
            root.attr_get(fio::NodeAttributesQuery::empty()),
            Ok((fio::MutableNodeAttributes::default(), fio::ImmutableNodeAttributes::default()))
        );
        let expected = directory_attributes(test_volume.root_directory_object_id(), 3, None);
        assert_eq!(
            root.attr_get(BASIC_ATTRIBUTES),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );
        let (root_mutable, _) = root
            .attr_get(
                fio::NodeAttributesQuery::CASEFOLD | fio::NodeAttributesQuery::SELINUX_CONTEXT,
            )
            .unwrap();
        assert_eq!(root_mutable.casefold, Some(false));
        // The root has no SELinux context.
        assert_eq!(root_mutable.selinux_context, None);

        let directory = open(&root, SUBDIR_PATH).expect("open failed");
        let expected = directory_attributes(
            test_volume.subdir_object_id(),
            2,
            Some(PosixAttributes { mode: SUBDIR_MODE, uid: SUBDIR_UID, gid: SUBDIR_GID, rdev: 0 }),
        );
        assert_eq!(
            directory.io().attr_get(BASIC_ATTRIBUTES),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );
        let (mutable, immutable) =
            directory.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert_eq!(mutable.casefold, Some(true));
        assert_eq!(mutable.rdev, Some(0));
        assert_eq!(immutable.storage_size, Some(0));
        assert!(mutable.creation_time.unwrap() > 0);
        assert!(mutable.modification_time.unwrap() > 0);
        assert!(mutable.access_time.unwrap() > 0);
        assert!(immutable.change_time.unwrap() > 0);
        assert_eq!(mutable.encryption_policy, None);
        assert_eq!(
            mutable.selinux_context,
            Some(fio::SelinuxContext::Data(SUBDIR_SELINUX_CONTEXT.to_vec()))
        );
        assert_eq!(immutable.verity_enabled, Some(false));
        assert_eq!(immutable.options, None);
        assert_eq!(immutable.root_hash, None);
    }

    #[fuchsia::test]
    fn attr_get_symlink() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let symlink = open(&root, SYMLINK_PATH).expect("open failed");
        let expected = short_symlink_attributes(&test_volume);
        assert_eq!(
            symlink.io().attr_get(BASIC_ATTRIBUTES),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );
        let (mutable, immutable) =
            symlink.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert_eq!(mutable.rdev, Some(0));
        // The target is in the object record, so nothing is allocated.
        assert_eq!(immutable.storage_size, Some(0));
        assert!(mutable.creation_time.unwrap() > 0);
        assert!(mutable.modification_time.unwrap() > 0);
        // Fxfs doesn't set the access or change time when it creates a symlink.
        assert_eq!(mutable.access_time, Some(0));
        assert_eq!(immutable.change_time, Some(0));
        // Symlinks don't report casefold.
        assert_eq!(mutable.casefold, None);
        assert_eq!(mutable.encryption_policy, None);
        assert_eq!(
            mutable.selinux_context,
            Some(fio::SelinuxContext::Data(SYMLINK_SELINUX_CONTEXT.to_vec()))
        );
        assert_eq!(immutable.verity_enabled, Some(false));
        assert_eq!(immutable.options, None);
        assert_eq!(immutable.root_hash, None);

        // A long target's length is the size of the data attribute, which is in the tree.
        let long_symlink = open_with_flags(&root, LONG_SYMLINK_PATH, fio::Flags::PROTOCOL_NODE)
            .expect("open failed");
        let expected = long_symlink_attributes(&test_volume);
        assert_eq!(
            long_symlink.io().attr_get(BASIC_ATTRIBUTES),
            Ok((expected.mutable_attributes, expected.immutable_attributes))
        );
        let (_, immutable) = long_symlink.io().attr_get(OTHER_ATTRIBUTES).expect("attr_get failed");
        assert!(immutable.storage_size.unwrap() >= LONG_SYMLINK_TARGET.len() as u64);
    }

    #[fuchsia::test]
    fn attr_get_non_inline_symlink_missing_data_returns_inconsistent() {
        const MISSING_DATA_SYMLINK_PATH: &str = "missing_data_symlink";

        let (mounted, ()) = MountedVolume::start(None, |store| async move {
            let root_id = store.root_directory_object_id();
            let root_dir = Directory::open(&store, root_id).await.expect("open root failed");
            let mut transaction = store
                .filesystem()
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_id)],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let symlink_id = root_dir
                .create_symlink(&mut transaction, LONG_SYMLINK_TARGET, MISSING_DATA_SYMLINK_PATH)
                .await
                .expect("create symlink failed");
            transaction.commit().await.expect("commit failed");

            let mut transaction = store
                .filesystem()
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), symlink_id)],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            transaction.add(
                store.store_object_id(),
                Mutation::replace_or_insert_object(
                    ObjectKey::attribute(symlink_id, AttributeId::DATA, AttributeKey::Attribute),
                    ObjectValue::None,
                ),
            );
            transaction.commit().await.expect("commit failed");
        });

        let root = mounted.volume().root();
        // Opening as a symlink fails fast with NOT_SUPPORTED before querying AttributeId::DATA.
        assert_eq!(
            root.open(
                MISSING_DATA_SYMLINK_PATH,
                fio::PERM_READABLE,
                None,
                fio::NodeAttributesQuery::CONTENT_SIZE,
                TestFactory,
            )
            .err(),
            Some(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            root.open(
                MISSING_DATA_SYMLINK_PATH,
                fio::Flags::PROTOCOL_NODE,
                None,
                fio::NodeAttributesQuery::CONTENT_SIZE,
                TestFactory,
            )
            .err(),
            Some(zx::Status::IO_DATA_INTEGRITY)
        );
        let opened = root
            .open(
                MISSING_DATA_SYMLINK_PATH,
                fio::Flags::PROTOCOL_NODE,
                None,
                fio::NodeAttributesQuery::empty(),
                TestFactory,
            )
            .expect("open node without query failed");
        assert_eq!(
            opened.io().attr_get(fio::NodeAttributesQuery::CONTENT_SIZE).err(),
            Some(zx::Status::IO_DATA_INTEGRITY)
        );
    }

    #[fuchsia::test]
    fn attr_get_encrypted_symlinks() {
        const ROOT_LONG_SYMLINK_PATH: &str = "root_long_symlink";
        const ENCRYPTED_DIR_PATH: &str = "encrypted";
        const WRAPPING_KEY_ID: [u8; 16] = u128::to_le_bytes(2);
        const WRAPPING_KEY_BYTES: [u8; 32] = [0x11; 32];

        let crypt = Arc::new(new_insecure_crypt());
        crypt
            .add_wrapping_key(WRAPPING_KEY_ID, WRAPPING_KEY_BYTES)
            .expect("add_wrapping_key failed");

        let (mounted, (root_long_id, fscrypt_short_id, fscrypt_long_id)) =
            MountedVolume::start(Some(crypt as Arc<dyn Crypt>), |store| async move {
                let root_id = store.root_directory_object_id();
                let root_dir = Directory::open(&store, root_id).await.expect("open root failed");
                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), root_id)],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let root_long_id = root_dir
                    .create_symlink(&mut transaction, LONG_SYMLINK_TARGET, ROOT_LONG_SYMLINK_PATH)
                    .await
                    .expect("create root long symlink failed");
                let encrypted_dir = root_dir
                    .create_child_dir(&mut transaction, ENCRYPTED_DIR_PATH)
                    .await
                    .expect("create encrypted dir failed");
                transaction.commit().await.expect("commit failed");

                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            encrypted_dir.object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                encrypted_dir
                    .set_wrapping_key(&mut transaction, WRAPPING_KEY_ID)
                    .await
                    .expect("set_wrapping_key failed");
                transaction.commit().await.expect("commit failed");

                // Reopen so the handle sees the updated `DirType::Encrypted` from the store.
                let encrypted_dir = Directory::open(&store, encrypted_dir.object_id())
                    .await
                    .expect("reopen encrypted dir failed");
                let mut transaction = store
                    .filesystem()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            encrypted_dir.object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let fscrypt_short_id = encrypted_dir
                    .create_symlink(&mut transaction, SYMLINK_TARGET, "short")
                    .await
                    .expect("create fscrypt short symlink failed");
                let fscrypt_long_id = encrypted_dir
                    .create_symlink(&mut transaction, LONG_SYMLINK_TARGET, "long")
                    .await
                    .expect("create fscrypt long symlink failed");
                transaction.commit().await.expect("commit failed");

                (root_long_id, fscrypt_short_id, fscrypt_long_id)
            });

        // A long symlink in the unencrypted root directory has an `ObjectKey::keys` record with
        // `VOLUME_DATA_KEY_ID` (and not `FSCRYPT_KEY_ID`), so `content_size` reports its exact
        // plaintext target length.
        let store = &mounted.volume().store;
        match block_on(store.tree().find_value(&ObjectKey::keys(root_long_id)))
            .expect("find keys failed")
        {
            Some(ObjectValue::Keys(keys)) => {
                assert!(keys.get(VOLUME_DATA_KEY_ID).is_some());
                assert!(keys.get(FSCRYPT_KEY_ID).is_none());
            }
            other => panic!("unexpected keys value: {other:?}"),
        }
        let root = mounted.volume().root();
        assert_eq!(open(&root, ROOT_LONG_SYMLINK_PATH).err(), Some(zx::Status::NOT_SUPPORTED));
        let root_long = open_with_flags(&root, ROOT_LONG_SYMLINK_PATH, fio::Flags::PROTOCOL_NODE)
            .expect("open node failed")
            .expect_node();
        assert_eq!(
            root_long.attributes(),
            Some(&symlink_attributes(root_long_id, LONG_SYMLINK_TARGET.len(), None))
        );

        // Short (`EncryptedSymlink`) and long (`NonInlineSymlink` with `FSCRYPT_KEY_ID`) symlinks
        // in an fscrypt directory report `NotSupported` for target and `CONTENT_SIZE`.
        for id in [fscrypt_short_id, fscrypt_long_id] {
            match block_on(store.tree().find_value(&ObjectKey::keys(id))).expect("find keys failed")
            {
                Some(ObjectValue::Keys(keys)) => {
                    assert!(keys.get(FSCRYPT_KEY_ID).is_some(), "{id}");
                }
                other => panic!("unexpected keys value for {id}: {other:?}"),
            }
            let symlink = super::RemoteSymlink::new(store.clone(), id);
            let err = symlink.target().expect_err("fscrypt symlink target should fail");
            assert!(FxfsError::NotSupported.matches(&err), "{id}: {err:?}");
            let err = symlink
                .read_target_and_attributes(fio::NodeAttributesQuery::CONTENT_SIZE)
                .expect_err("fscrypt symlink read_target_and_attributes should fail");
            assert!(FxfsError::NotSupported.matches(&err), "{id}: {err:?}");
            let err = symlink
                .get_attributes(fio::NodeAttributesQuery::CONTENT_SIZE)
                .expect_err("fscrypt symlink content_size should fail");
            assert!(FxfsError::NotSupported.matches(&err), "{id}: {err:?}");
            let attrs = symlink
                .get_attributes(fio::NodeAttributesQuery::ID | fio::NodeAttributesQuery::PROTOCOLS)
                .expect("get_attributes failed");
            assert_eq!(attrs.immutable_attributes.id, Some(id));
            assert_eq!(attrs.immutable_attributes.protocols, Some(fio::NodeProtocolKinds::SYMLINK));
        }
    }

    /// Returns the node's xattr names, sorted so tests don't depend on the order of the records.
    fn xattr_names(io: &LocalFxIo) -> Vec<Vec<u8>> {
        let mut names = io.xattr_list().expect("xattr_list failed");
        names.sort();
        names
    }

    #[fuchsia::test]
    fn xattr_get_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let file = open(&root, FILE_PATH).expect("open failed");
        assert_eq!(file.io().xattr_get(FILE_XATTR_NAME), Ok(FILE_XATTR_VALUE.to_vec()));
        assert_eq!(
            file.io().xattr_get(fio::SELINUX_CONTEXT_NAME.as_bytes()),
            Ok(FILE_SELINUX_CONTEXT.to_vec())
        );
        for name in [b"user.missing".as_slice(), SUBDIR_XATTR_NAME] {
            assert_eq!(file.io().xattr_get(name), Err(zx::Status::NOT_FOUND), "{name:?}");
        }
    }

    // TODO(https://fxbug.dev/570053552): Flip this once the FIDL fallback exists.
    #[fuchsia::test]
    fn xattr_get_out_of_line_not_supported() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let file = open(&root, FILE_PATH).expect("open failed");
        assert_eq!(file.io().xattr_get(LARGE_XATTR_NAME), Err(zx::Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    fn xattr_get_directory() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let directory = open(&root, SUBDIR_PATH).expect("open failed");
        assert_eq!(directory.io().xattr_get(SUBDIR_XATTR_NAME), Ok(SUBDIR_XATTR_VALUE.to_vec()));
        assert_eq!(
            directory.io().xattr_get(fio::SELINUX_CONTEXT_NAME.as_bytes()),
            Ok(SUBDIR_SELINUX_CONTEXT.to_vec())
        );
        for name in [b"user.missing".as_slice(), FILE_XATTR_NAME] {
            assert_eq!(directory.io().xattr_get(name), Err(zx::Status::NOT_FOUND), "{name:?}");
        }
        // The root has no xattrs.
        assert_eq!(root.xattr_get(SUBDIR_XATTR_NAME), Err(zx::Status::NOT_FOUND));
    }

    #[fuchsia::test]
    fn xattr_list_file() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let file = open(&root, FILE_PATH).expect("open failed");
        // Out-of-line xattrs are listed even though their values can't be read yet.
        let mut expected = vec![
            fio::SELINUX_CONTEXT_NAME.as_bytes().to_vec(),
            FILE_XATTR_NAME.to_vec(),
            LARGE_XATTR_NAME.to_vec(),
        ];
        expected.sort();
        assert_eq!(xattr_names(file.io()), expected);

        let empty_file = open(&root, EMPTY_FILE_PATH).expect("open failed");
        assert_eq!(xattr_names(empty_file.io()), Vec::<Vec<u8>>::new());
    }

    #[fuchsia::test]
    fn xattr_list_directory() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let directory = open(&root, SUBDIR_PATH).expect("open failed");
        let mut expected =
            vec![fio::SELINUX_CONTEXT_NAME.as_bytes().to_vec(), SUBDIR_XATTR_NAME.to_vec()];
        expected.sort();
        assert_eq!(xattr_names(directory.io()), expected);
        assert_eq!(xattr_names(&root), Vec::<Vec<u8>>::new());
    }

    #[fuchsia::test]
    fn xattr_get_symlink() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let symlink = open(&root, SYMLINK_PATH).expect("open failed");
        assert_eq!(symlink.io().xattr_get(SYMLINK_XATTR_NAME), Ok(SYMLINK_XATTR_VALUE.to_vec()));
        assert_eq!(
            symlink.io().xattr_get(fio::SELINUX_CONTEXT_NAME.as_bytes()),
            Ok(SYMLINK_SELINUX_CONTEXT.to_vec())
        );
        for name in [b"user.missing".as_slice(), FILE_XATTR_NAME] {
            assert_eq!(symlink.io().xattr_get(name), Err(zx::Status::NOT_FOUND), "{name:?}");
        }
    }

    #[fuchsia::test]
    fn xattr_list_symlink() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let symlink = open(&root, SYMLINK_PATH).expect("open failed");
        let mut expected =
            vec![fio::SELINUX_CONTEXT_NAME.as_bytes().to_vec(), SYMLINK_XATTR_NAME.to_vec()];
        expected.sort();
        assert_eq!(xattr_names(symlink.io()), expected);

        let long_symlink = open_with_flags(&root, LONG_SYMLINK_PATH, fio::Flags::PROTOCOL_NODE)
            .expect("open failed");
        assert_eq!(xattr_names(long_symlink.io()), Vec::<Vec<u8>>::new());
    }

    #[fuchsia::test]
    fn open_pipelined() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let open_pipelined_with_flags =
            |paths: &[&str], flags: fio::Flags| -> Vec<Result<Opened, zx::Status>> {
                root.open_pipelined(paths, flags, BASIC_ATTRIBUTES, || TestFactory).collect()
            };
        let open_pipelined = |paths: &[&str]| -> Vec<Result<Opened, zx::Status>> {
            open_pipelined_with_flags(paths, fio::PERM_READABLE)
        };

        let (dir_name, nested_name) = NESTED_FILE_PATH.split_once('/').expect("nested file path");
        for flags in [
            fio::Flags::empty(),
            fio::PERM_READABLE,
            fio::PERM_READABLE | fio::Flags::PERM_INHERIT_WRITE | fio::Flags::PERM_INHERIT_EXECUTE,
        ] {
            let results = open_pipelined_with_flags(&[dir_name, nested_name], flags);
            let [Ok(Opened::Directory(dir_io, directory)), Ok(Opened::File(file_io, file))] =
                &results[..]
            else {
                panic!("unexpected results for {flags:?}: {results:?}");
            };
            assert_eq!(
                directory.as_ref().and_then(|a| a.immutable_attributes.id),
                Some(test_volume.subdir_object_id())
            );
            assert_eq!(
                dir_io.attr_get(fio::NodeAttributesQuery::ID).unwrap().1.id,
                Some(test_volume.subdir_object_id())
            );
            assert_eq!(
                file.as_ref().and_then(|a| a.immutable_attributes.id),
                Some(test_volume.nested_file_object_id())
            );
            assert_eq!(
                file_io.attr_get(fio::NodeAttributesQuery::ID).unwrap().1.id,
                Some(test_volume.nested_file_object_id())
            );
        }

        // Empty and single-component paths.
        assert!(open_pipelined(&[]).is_empty());
        let results = open_pipelined(&[FILE_PATH]);
        assert!(matches!(&results[..], [Ok(Opened::File(..))]), "unexpected results: {results:?}");
        let results = open_pipelined(&[SUBDIR_PATH]);
        assert!(
            matches!(&results[..], [Ok(Opened::Directory(..))]),
            "unexpected results: {results:?}"
        );

        // A file in the middle fails with NOT_DIR and stops before subsequent components.
        for paths in [&[FILE_PATH, "x", "y"][..], &[NESTED_FILE_PATH, "x"][..]] {
            let results = open_pipelined(paths);
            assert!(
                matches!(&results[..], [Err(zx::Status::NOT_DIR)]),
                "unexpected results for {paths:?}: {results:?}"
            );
        }

        let results = open_pipelined(&[dir_name, "missing", "x"]);
        assert!(
            matches!(&results[..], [Ok(Opened::Directory(..)), Err(zx::Status::NOT_FOUND)]),
            "unexpected results: {results:?}"
        );

        let results = open_pipelined(&[dir_name, "", "x"]);
        assert!(
            matches!(&results[..], [Ok(Opened::Directory(..)), Err(zx::Status::INVALID_ARGS)]),
            "unexpected results: {results:?}"
        );

        // A symlink in the middle is opened as a symlink, so the caller can follow it, and nothing
        // after it is opened.
        let results = open_pipelined(&[SYMLINK_PATH, "x"]);
        let [Ok(symlink), Err(zx::Status::NOT_DIR)] = &results[..] else {
            panic!("unexpected results: {results:?}");
        };
        assert_eq!(symlink.target(), Some(SYMLINK_TARGET));

        // TODO(https://fxbug.dev/570053552): Flip this once the FIDL fallback exists.
        let results = open_pipelined(&[LONG_SYMLINK_PATH, "x"]);
        assert!(
            matches!(&results[..], [Err(zx::Status::NOT_SUPPORTED)]),
            "unexpected results: {results:?}"
        );

        // Calling open_pipelined on a file handle fails with NOT_DIR.
        let file = open(&root, FILE_PATH).expect("open file failed");
        let results: Vec<_> = file
            .io()
            .open_pipelined(&["x"], fio::PERM_READABLE, BASIC_ATTRIBUTES, || TestFactory)
            .collect();
        assert!(
            matches!(&results[..], [Err(zx::Status::NOT_DIR)]),
            "unexpected results: {results:?}"
        );
    }

    #[fuchsia::test]
    fn open_pipelined_invalid_flags() {
        let test_volume = TestVolume::new();
        let root = test_volume.volume().root();
        let (dir_name, nested_name) = NESTED_FILE_PATH.split_once('/').expect("nested file path");

        // Any flag outside MASK_KNOWN_PERMISSIONS is rejected upfront.
        for bad_flag in [
            fio::Flags::PROTOCOL_NODE,
            fio::Flags::PROTOCOL_FILE,
            fio::Flags::PROTOCOL_DIRECTORY,
            fio::Flags::PROTOCOL_SYMLINK,
            fio::Flags::PROTOCOL_SERVICE,
            fio::Flags::PROTOCOL_NODE | fio::Flags::PROTOCOL_DIRECTORY,
            fio::Flags::PROTOCOL_NODE | fio::Flags::PROTOCOL_FILE,
            fio::Flags::FILE_APPEND,
            fio::Flags::FILE_TRUNCATE,
            fio::Flags::FLAG_MAYBE_CREATE,
            fio::Flags::FLAG_MUST_CREATE,
            fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY,
            fio::Flags::FLAG_SEND_REPRESENTATION,
            fio::Flags::from_bits_retain(1 << 63),
        ] {
            for flags in [bad_flag, fio::PERM_READABLE | bad_flag] {
                for paths in [&[dir_name, nested_name][..], &[FILE_PATH][..]] {
                    let results: Vec<_> = root
                        .open_pipelined(paths, flags, BASIC_ATTRIBUTES, || TestFactory)
                        .collect();
                    assert!(
                        matches!(&results[..], [Err(zx::Status::INVALID_ARGS)]),
                        "expected INVALID_ARGS for {flags:?} on {paths:?}, got {results:?}"
                    );
                }
            }
        }

        // Mutating permission flags pass validation and fail with NOT_SUPPORTED (read-only).
        for flags in [
            fio::PERM_WRITABLE,
            fio::Flags::PERM_WRITE_BYTES,
            fio::Flags::PERM_MODIFY_DIRECTORY,
            fio::Flags::PERM_UPDATE_ATTRIBUTES,
        ] {
            let results: Vec<_> = root
                .open_pipelined(&[dir_name, nested_name], flags, BASIC_ATTRIBUTES, || TestFactory)
                .collect();
            assert!(
                matches!(&results[..], [Err(zx::Status::NOT_SUPPORTED)]),
                "expected NOT_SUPPORTED for {flags:?}, got {results:?}"
            );
        }
    }
}
