// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains the [`BlobDirectory`] node type used to represent a directory of immutable
//! content-addressable blobs.

use crate::fuchsia::component::map_to_raw_status;
use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::fxblob::blob::FxBlob;
use crate::fuchsia::fxblob::mapping_provider::BlobMappingProvider;
use crate::fuchsia::fxblob::writer::{BlobStorageFormat, DeliveryBlobWriter};
use crate::fuchsia::node::{FxNode, GetResult, OpenedNode};
use crate::fuchsia::volume::{FxVolume, RootDir};
use anyhow::{Context as _, Error, anyhow};
use fidl::endpoints::{ClientEnd, DiscoverableProtocolMarker, ServerEnd, create_request_stream};
use fidl_fuchsia_fxfs::{
    BlobCreatorMarker, BlobCreatorRequest, BlobCreatorRequestStream, BlobWriterMarker,
    CreateBlobError,
};
use fidl_fuchsia_io::{self as fio, FilesystemInfo, NodeMarker, WatchMask};
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping::MappingProviderMarker;
use fuchsia_hash::Hash;
use futures::TryStreamExt;
use fxfs::errors::FxfsError;
use fxfs::object_store::transaction::{LockKey, lock_keys};
use fxfs::object_store::{self, HandleOptions, ObjectDescriptor, ObjectStore, StoreObjectHandle};
use fxfs_macros::ToWeakNode;
use fxfs_trace::{TraceFutureExt, trace_future_args};
use std::sync::Arc;
use vfs::directory::dirents_sink;
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::directory::entry_container::{
    Directory as VfsDirectory, DirectoryWatcher, MutableDirectory,
};
use vfs::directory::helper::DirectlyMutable;
use vfs::directory::mutable::connection::MutableConnection;
use vfs::directory::simple::Simple;
use vfs::directory::traversal_position::TraversalPosition;
use vfs::execution_scope::ExecutionScope;
use vfs::path::Path;
use vfs::{ObjectRequestRef, ProtocolsExt, ToObjectRequest};
use zx::Status;

/// A flat directory containing content-addressable blobs (names are their hashes).
/// It is not possible to create sub-directories.
/// It is not possible to write to an existing blob.
/// It is not possible to open or read a blob until it is written and verified.
#[derive(ToWeakNode)]
pub struct BlobDirectory {
    directory: Arc<FxDirectory>,
    mapping_provider: BlobMappingProvider,
}

impl RootDir for BlobDirectory {
    fn as_directory_entry(self: Arc<Self>) -> Arc<dyn DirectoryEntry> {
        self
    }

    fn serve(self: Arc<Self>, flags: fio::Flags, server_end: ServerEnd<fio::DirectoryMarker>) {
        let scope = self.volume().scope().clone();
        vfs::directory::serve_on(self, flags, scope, server_end);
    }

    fn as_node(self: Arc<Self>) -> Arc<dyn FxNode> {
        self as Arc<dyn FxNode>
    }

    fn register_additional_volume_services(self: Arc<Self>, svc_dir: &Simple) -> Result<(), Error> {
        let this = self.clone();
        svc_dir.add_entry(
            BlobCreatorMarker::PROTOCOL_NAME,
            vfs::service::host(move |r| this.clone().handle_blob_creator_requests(r)),
        )?;

        let this = self.clone();
        svc_dir.add_entry(
            MappingProviderMarker::PROTOCOL_NAME,
            vfs::service::host(move |r| {
                BlobMappingProvider::handle_mapping_provider_requests(this.clone(), r)
            }),
        )?;

        svc_dir.add_entry(
            fblock::MapperMarker::PROTOCOL_NAME,
            vfs::service::endpoint(move |scope, channel| {
                let device = self.store().device().clone();
                scope.spawn(async move {
                    if let Err(status) =
                        device.connect_mapper(channel.into_zx_channel().into()).await
                    {
                        log::warn!(status:?; "Failed to connect to Mapper");
                    }
                });
            }),
        )?;

        Ok(())
    }
}

impl BlobDirectory {
    fn new(directory: FxDirectory) -> Self {
        Self { directory: Arc::new(directory), mapping_provider: BlobMappingProvider::new() }
    }

    pub fn mapping_provider(&self) -> &BlobMappingProvider {
        &self.mapping_provider
    }

    pub fn directory(&self) -> &Arc<FxDirectory> {
        &self.directory
    }

    pub fn volume(&self) -> &Arc<FxVolume> {
        self.directory.volume()
    }

    fn store(&self) -> &ObjectStore {
        self.directory.store()
    }

    /// Wraps [`Self::open_blob_locked`] while taking the locks.
    pub async fn open_blob(
        self: &Arc<Self>,
        hash: Hash,
    ) -> Result<Option<OpenedNode<FxBlob>>, Error> {
        let store = self.store();
        let fs = store.filesystem();
        let keys = lock_keys![LockKey::object(store.store_object_id(), self.directory.object_id())];

        // A lock needs to be held over searching the directory and incrementing the open count.
        let _guard = fs.lock_manager().read_lock(keys).await;
        self.open_blob_locked(hash).await
    }

    /// Attempt to open and cache the blob with `hash` in this directory. Returns `Ok(None)` if no
    /// blob matching `hash` was found. Requires holding locks for at least the object store and
    /// directory object.
    async fn open_blob_locked(
        self: &Arc<Self>,
        hash: Hash,
    ) -> Result<Option<OpenedNode<FxBlob>>, Error> {
        let name = hash.to_string();
        let Some((object_id, object_descriptor, _)) =
            self.directory.directory().lookup(&name).await?
        else {
            return Ok(None);
        };
        if object_descriptor != ObjectDescriptor::File {
            return Err(FxfsError::Inconsistent)
                .with_context(|| format!("Blob {name} has invalid object descriptor!"));
        }
        let node = self.get_or_load_node(object_id).await?;
        Ok(Some(OpenedNode::new(node)))
    }

    // Attempts to get a node from the node cache. If the node wasn't present in the cache,
    // installs a new FxBlob into the cache and returns it.
    async fn get_or_load_node(self: &Arc<Self>, object_id: u64) -> Result<Arc<FxBlob>, Error> {
        let volume = self.volume();
        match volume.cache().get_or_reserve(object_id).await {
            GetResult::Node(node) => node.into_any().downcast::<FxBlob>().map_err(|_| {
                anyhow!(FxfsError::Inconsistent).context("Loaded non-blob from cache")
            }),
            GetResult::Placeholder(placeholder) => {
                let handle = StoreObjectHandle::new(
                    volume.clone(),
                    object_id,
                    /*permanent_keys=*/ false,
                    HandleOptions::default(),
                    /*trace=*/ false,
                );
                let node = FxBlob::new(handle);
                placeholder.commit(&(node.clone() as Arc<dyn FxNode>));
                Ok(node)
            }
        }
    }

    /// Creates a [`ClientEnd<BlobWriterMarker>`] to write the delivery blob identified by `hash`.
    /// It is safe to create multiple writers for a given `hash`, however only one will succeed.
    /// Requests are handled asynchronously on this volume's execution scope.
    async fn create_blob_writer(
        self: &Arc<Self>,
        hash: Hash,
        allow_existing: bool,
    ) -> Result<ClientEnd<BlobWriterMarker>, CreateBlobError> {
        if !allow_existing {
            let blob_exists = self
                .directory
                .directory()
                .lookup(&hash.to_string())
                .await
                .map_err(|e| {
                    log::error!("Failed to lookup blob: {:?}", e);
                    CreateBlobError::Internal
                })?
                .is_some();
            if blob_exists {
                return Err(CreateBlobError::AlreadyExists);
            }
        }
        let (client_end, request_stream) = create_request_stream::<BlobWriterMarker>();
        let writer = DeliveryBlobWriter::new(self, hash, BlobStorageFormat::DeliveryBlob)
            .await
            .map_err(|e| {
                log::error!("Failed to create blob writer: {:?}", e);
                if FxfsError::NoSpace.matches(&e) {
                    CreateBlobError::OutOfSpace
                } else {
                    CreateBlobError::Internal
                }
            })?;
        self.volume().scope().spawn(async move {
            if let Err(e) = writer.handle_requests(request_stream).await {
                log::error!("Failed to handle BlobWriter requests: {}", e);
            }
        });
        return Ok(client_end);
    }

    async fn needs_overwrite(&self, blob_hash: Hash) -> Result<bool, Error> {
        // We don't take a lock here because this will only look up existence for now. If we
        // actually start fetching the blob or info about it after looking it up this will need to
        // take a reader lock on the directory and maybe also the object.
        match self.directory.directory().lookup(&blob_hash.to_string()).await? {
            Some(_) => Ok(false),
            None => Err(FxfsError::NotFound.into()),
        }
    }

    async fn handle_blob_creator_requests(self: Arc<Self>, mut requests: BlobCreatorRequestStream) {
        while let Ok(Some(request)) = requests.try_next().await {
            match request {
                BlobCreatorRequest::Create { responder, hash, allow_existing } => {
                    async {
                        responder
                            .send(self.create_blob_writer(Hash::from(hash), allow_existing).await)
                            .unwrap_or_else(|error| {
                                log::error!(error:?; "failed to send Create response");
                            });
                    }
                    .trace(trace_future_args!("BlobCreator::Create"))
                    .await;
                }
                BlobCreatorRequest::NeedsOverwrite { blob_hash, responder } => {
                    async {
                        let _ = responder.send(
                            self.needs_overwrite(Hash::from(blob_hash))
                                .await
                                .map_err(map_to_raw_status),
                        );
                    }
                    .trace(trace_future_args!("BlobCreator::NeedsOverwrite"))
                    .await;
                }
            }
        }
    }

    async fn open_impl(
        self: Arc<Self>,
        scope: ExecutionScope,
        path: Path,
        flags: impl ProtocolsExt,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), Status> {
        if path.is_empty() {
            object_request
                .create_connection::<MutableConnection<_>, _>(
                    scope,
                    OpenedNode::new(self).take(),
                    flags,
                )
                .await
        } else {
            Err(Status::NOT_SUPPORTED)
        }
    }
}

impl FxNode for BlobDirectory {
    fn object_id(&self) -> u64 {
        self.directory.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        self.directory.parent()
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // This directory can't be renamed.
        unreachable!();
    }

    fn open_count_add_one(&self) {}
    fn open_count_sub_one(self: Arc<Self>) {}

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::Directory
    }
}

impl MutableDirectory for BlobDirectory {
    async fn unlink(self: Arc<Self>, name: &str, must_be_directory: bool) -> Result<(), Status> {
        if must_be_directory {
            return Err(Status::INVALID_ARGS);
        }
        self.directory.clone().unlink(name, must_be_directory).await
    }

    async fn update_attributes(
        &self,
        attributes: fio::MutableNodeAttributes,
    ) -> Result<(), Status> {
        self.directory.update_attributes(attributes).await
    }

    async fn sync(&self) -> Result<(), Status> {
        self.directory.sync().await
    }
}

/// Implementation of VFS pseudo-directory for blobs. Forks a task per connection.
impl DirectoryEntry for BlobDirectory {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), Status> {
        request.open_dir(self)
    }

    fn scope(&self) -> Option<ExecutionScope> {
        Some(self.volume().scope().clone())
    }
}

impl GetEntryInfo for BlobDirectory {
    fn entry_info(&self) -> EntryInfo {
        self.directory.entry_info()
    }
}

impl vfs::node::Node for BlobDirectory {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Status> {
        self.directory.get_attributes(requested_attributes).await
    }

    fn query_filesystem(&self) -> Result<FilesystemInfo, Status> {
        self.directory.query_filesystem()
    }
}

/// Implements VFS entry container trait for directories, allowing manipulation of their contents.
impl VfsDirectory for BlobDirectory {
    fn deprecated_open(
        self: Arc<Self>,
        scope: ExecutionScope,
        flags: fio::OpenFlags,
        path: Path,
        server_end: ServerEnd<NodeMarker>,
    ) {
        scope.clone().spawn(flags.to_object_request(server_end).handle_async(
            async move |object_request| self.open_impl(scope, path, flags, object_request).await,
        ));
    }

    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        path: Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), Status> {
        scope.clone().spawn(object_request.take().handle_async(async move |object_request| {
            self.open_impl(scope, path, flags, object_request).await
        }));
        Ok(())
    }

    async fn open_async(
        self: Arc<Self>,
        scope: ExecutionScope,
        path: Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), Status> {
        self.open_impl(scope, path, flags, object_request).await
    }

    async fn read_dirents(
        &self,
        pos: &TraversalPosition,
        sink: Box<dyn dirents_sink::Sink>,
    ) -> Result<(TraversalPosition, Box<dyn dirents_sink::Sealed>), Status> {
        self.directory.read_dirents(pos, sink).await
    }

    fn register_watcher(
        self: Arc<Self>,
        scope: ExecutionScope,
        mask: WatchMask,
        watcher: DirectoryWatcher,
    ) -> Result<(), Status> {
        self.directory.clone().register_watcher(scope, mask, watcher)
    }

    fn unregister_watcher(self: Arc<Self>, key: usize) {
        self.directory.clone().unregister_watcher(key)
    }
}

impl From<object_store::Directory<FxVolume>> for BlobDirectory {
    fn from(dir: object_store::Directory<FxVolume>) -> Self {
        Self::new(dir.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuchsia::fxblob::testing::{BlobFixture, new_blob_fixture, open_blob_fixture};
    use crate::fuchsia::testing::{TestFixture, TestFixtureOptions};
    use assert_matches::assert_matches;
    use blob_writer::BlobWriter;
    use delivery_blob::{CompressionMode, Type1Blob};
    use fidl_fuchsia_storage_mapping::MappingProviderMarker;
    use fuchsia_async::{DurationExt as _, TimeoutExt as _};
    use fuchsia_component_client::connect_to_protocol_at_dir_svc;
    use fuchsia_fs::directory::{
        DirEntry, DirentKind, WatchEvent, WatchMessage, Watcher, open_directory_async,
        readdir_inclusive,
    };
    use futures::StreamExt as _;
    use std::path::PathBuf;
    use std::str::FromStr;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    #[fuchsia::test(threads = 10)]
    async fn test_unlink() {
        let fixture = new_blob_fixture().await;

        let data = [1; 1000];

        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture
            .root()
            .unlink(&format!("{}", hash), &fio::UnlinkOptions::default())
            .await
            .expect("FIDL failed")
            .expect("unlink failed");

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_readdir() {
        let fixture = new_blob_fixture().await;

        let data = [0xab; 2];
        let hash;
        {
            hash = fuchsia_merkle::root_from_slice(&data);
            let compressed_data: Vec<u8> = Type1Blob::generate(&data, CompressionMode::Always);

            let blob_proxy =
                connect_to_protocol_at_dir_svc::<fidl_fuchsia_fxfs::BlobCreatorMarker>(
                    fixture.volume_out_dir(),
                )
                .expect("failed to connect to the Blob service");

            let blob_writer_client_end = blob_proxy
                .create(&hash.into(), false)
                .await
                .expect("transport error on create")
                .expect("failed to create blob");

            let writer = blob_writer_client_end.into_proxy();
            let mut blob_writer = BlobWriter::create(writer, compressed_data.len() as u64)
                .await
                .expect("failed to create BlobWriter");
            blob_writer.write(&compressed_data[..1]).await.unwrap();

            // Before the blob is finished writing, it shouldn't appear in the directory.
            assert_eq!(
                readdir_inclusive(fixture.root()).await.ok(),
                Some(vec![DirEntry { name: ".".to_string(), kind: DirentKind::Directory }])
            );

            blob_writer.write(&compressed_data[1..]).await.unwrap();
        }

        assert_eq!(
            readdir_inclusive(fixture.root()).await.ok(),
            Some(vec![
                DirEntry { name: ".".to_string(), kind: DirentKind::Directory },
                DirEntry { name: format! {"{}", hash}, kind: DirentKind::File },
            ])
        );

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_watchers() {
        let fixture = new_blob_fixture().await;

        let mut watcher = Watcher::new(fixture.root()).await.unwrap();
        assert_eq!(
            watcher.next().await,
            Some(Ok(WatchMessage { event: WatchEvent::EXISTING, filename: PathBuf::from(".") }))
        );
        assert_matches!(
            watcher.next().await,
            Some(Ok(WatchMessage { event: WatchEvent::IDLE, .. }))
        );

        let data = vec![vec![0xab; 2], vec![0xcd; 65_536]];
        let mut hashes = vec![];
        let mut filenames = vec![];
        for datum in data {
            let hash = fuchsia_merkle::root_from_slice(&datum);
            let filename = PathBuf::from(format!("{}", hash));
            hashes.push(hash.clone());
            filenames.push(filename.clone());

            let compressed_data: Vec<u8> = Type1Blob::generate(&datum, CompressionMode::Always);

            let blob_proxy =
                connect_to_protocol_at_dir_svc::<fidl_fuchsia_fxfs::BlobCreatorMarker>(
                    fixture.volume_out_dir(),
                )
                .expect("failed to connect to the Blob service");

            let blob_writer_client_end = blob_proxy
                .create(&hash.into(), false)
                .await
                .expect("transport error on create")
                .expect("failed to create blob");

            let writer = blob_writer_client_end.into_proxy();
            let mut blob_writer = BlobWriter::create(writer, compressed_data.len() as u64)
                .await
                .expect("failed to create BlobWriter");
            blob_writer.write(&compressed_data[..compressed_data.len() - 1]).await.unwrap();

            // Before the blob is finished writing, we shouldn't see any watch events for it.
            assert_matches!(
                watcher
                    .next()
                    .on_timeout(zx::MonotonicDuration::from_millis(500).after_now(), || None)
                    .await,
                None
            );

            blob_writer.write(&compressed_data[compressed_data.len() - 1..]).await.unwrap();

            assert_eq!(
                watcher.next().await,
                Some(Ok(WatchMessage { event: WatchEvent::ADD_FILE, filename }))
            );
        }

        for (hash, filename) in hashes.iter().zip(filenames) {
            fixture
                .root()
                .unlink(&format!("{}", hash), &fio::UnlinkOptions::default())
                .await
                .expect("FIDL call failed")
                .expect("unlink failed");
            assert_eq!(
                watcher.next().await,
                Some(Ok(WatchMessage { event: WatchEvent::REMOVE_FILE, filename }))
            );
        }

        std::mem::drop(watcher);
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_rename_fails() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let (status, token) = fixture.root().deprecated_get_token().await.expect("FIDL failed");
        Status::ok(status).unwrap();
        fixture
            .root()
            .rename(&format!("{}", hash), token.unwrap().into(), "foo")
            .await
            .expect("FIDL failed")
            .expect_err("rename should fail");

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_link_fails() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let (status, token) = fixture.root().deprecated_get_token().await.expect("FIDL failed");
        Status::ok(status).unwrap();
        let status = fixture
            .root()
            .link(&format!("{}", hash), token.unwrap().into(), "foo")
            .await
            .expect("FIDL failed");
        assert_eq!(Status::ok(status), Err(Status::NOT_SUPPORTED));

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_verify_cached_hash_node() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let evil_hash =
            Hash::from_str("2222222222222222222222222222222222222222222222222222222222222222")
                .unwrap();

        // Create a malicious link to the existing blob. This shouldn't be possible without special
        // access either via internal apis or modifying the disk image.
        {
            let root = fixture
                .volume()
                .root()
                .clone()
                .as_node()
                .into_any()
                .downcast::<BlobDirectory>()
                .unwrap()
                .directory()
                .clone();
            root.clone()
                .link(evil_hash.to_string(), root, &hash.to_string())
                .await
                .expect("Linking file");
        }
        let device = fixture.close().await;

        let fixture = open_blob_fixture(device).await;
        {
            // Hold open a ref to keep it in the node cache.
            let _vmo = fixture.get_blob_vmo(hash).await;

            // Open the malicious link
            fixture
                .blob_pager()
                .unwrap()
                .create_vmo(&evil_hash.into())
                .await
                .expect_err("Hashes should mismatch");
        }
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_blob_needs_overwrite_verifies_existence() {
        let fixture = new_blob_fixture().await;
        let data = vec![42u8; 32];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_creator =
            connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(fixture.volume_out_dir())
                .expect("failed to connect to the BlobCreator service");
        assert!(
            !blob_creator
                .needs_overwrite(&hash.into())
                .await
                .expect("fidl transport")
                .expect("Find new blob")
        );

        // Fail to find one that is missing.
        blob_creator
            .needs_overwrite(&[1u8; 32].into())
            .await
            .expect("fidl transport")
            .expect_err("Blob should not exist");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_mapping_provider_exposure() {
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { encrypted: false, as_blob: true, ..Default::default() },
        )
        .await;

        let svc_dir = open_directory_async(fixture.volume_out_dir(), "svc", fio::PERM_READABLE)
            .expect("failed to open svc dir");

        let entries = readdir_inclusive(&svc_dir).await.expect("readdir");
        let is_exposed = entries.iter().any(|e| e.name == MappingProviderMarker::PROTOCOL_NAME);
        assert!(is_exposed, "MappingProvider not exposed");
        let mapper_exposed = entries.iter().any(|e| e.name == fblock::MapperMarker::PROTOCOL_NAME);
        assert!(mapper_exposed, "Mapper not exposed");

        let mapper =
            connect_to_protocol_at_dir_svc::<fblock::MapperMarker>(fixture.volume_out_dir())
                .expect("failed to connect to the Mapper service");
        assert_matches::assert_matches!(
            mapper.take_event_stream().try_next().await,
            Err(fidl::Error::ClientChannelClosed {
                epitaph: fidl::Epitaph::Explicit(Err(Status::NOT_SUPPORTED)),
                ..
            })
        );

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_create_blob_out_of_space() {
        let fixture = new_blob_fixture().await;
        let reservation = fixture.fs().allocator().reserve_with(None, |limit| limit);

        assert_matches!(
            fixture.create_blob(&[1u8; 32], false).await,
            Err(CreateBlobError::OutOfSpace)
        );

        drop(reservation);
        fixture.close().await;
    }
}
