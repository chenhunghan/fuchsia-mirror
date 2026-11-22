// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::fxblob::blob::FxBlob;
use crate::fuchsia::fxblob::directory::BlobDirectory;
use crate::fuchsia::node::{FxNode, OpenedNode};
use anyhow::{Error, ensure};
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_merkle::Hash;
use fuchsia_sync::Mutex;
use futures::TryStreamExt;
use futures::future::join_all;
use futures::lock::Mutex as AsyncMutex;
use fxfs::errors::FxfsError;
use log::{error, warn};
use mapping::{
    Extents, MAPPING_VMO_SIZE, MappingCommand, PENDING_COMMANDS_CAPACITY, RawMappingCommand,
};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use vmo_fifo::{Alignment, AsyncSender};

/// `BlobMappingProvider` services requests to open mapping sessions over a `BlobDirectory` and
/// tracks active sessions so they can be notified on blob overwrite.
pub struct BlobMappingProvider {
    sessions: Mutex<Vec<Weak<BlobMappingSession>>>,
}

impl BlobMappingProvider {
    pub fn new() -> Self {
        Self { sessions: Mutex::new(Vec::new()) }
    }

    fn register_session(&self, session: Weak<BlobMappingSession>) {
        let mut sessions = self.sessions.lock();
        sessions.retain(|s| s.strong_count() > 0);
        sessions.push(session);
    }

    pub(crate) async fn on_blob_overwritten(&self, hash: Hash) {
        let mut active_sessions = Vec::new();
        self.sessions.lock().retain(|s| match s.upgrade() {
            Some(session) => {
                active_sessions.push(session);
                true
            }
            None => false,
        });
        join_all(active_sessions.into_iter().map(|session| session.on_blob_overwritten(hash)))
            .await;
    }

    pub async fn handle_mapping_provider_requests(
        blob_directory: Arc<BlobDirectory>,
        mut stream: fmapping::MappingProviderRequestStream,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fmapping::MappingProviderRequest::OpenSession { session, responder } => {
                    let vmo = match zx::Vmo::create(MAPPING_VMO_SIZE) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    let sender = match AsyncSender::<RawMappingCommand>::new(
                        vmo,
                        Alignment::SIZE_8B,
                        PENDING_COMMANDS_CAPACITY,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    let vmo_clone = match sender.vmo().duplicate_handle(zx::Rights::SAME_RIGHTS) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    if let Err(error) = responder.send(Ok(vmo_clone)) {
                        error!(error:?; "Failed to send open session response");
                    } else {
                        let mapping_session =
                            BlobMappingSession::new(blob_directory.clone(), sender);
                        blob_directory.volume().scope().spawn(async move {
                            mapping_session
                                .handle_mapping_session_requests(session.into_stream())
                                .await;
                        });
                    }
                }
                fmapping::MappingProviderRequest::_UnknownMethod { ordinal, .. } => {
                    warn!(ordinal; "Unknown MappingProvider method");
                }
            }
        }
    }
}

/// Server for `MappingSession` FIDL requests on a `BlobDirectory`.
///
/// `MappingSession` lets a client (such as `BlobPagerAndVerifier`) register blobs by Merkle root
/// so the block driver's `Mapper` can read their on-disk extents directly without routing reads
/// through Fxfs. When a client opens a blob under a session-unique `key`, `BlobMappingSession`
/// acquires an `OpenedNode<FxBlob>` (preventing Fxfs from purging its blocks until dropped),
/// loads its data and merkle extents, and forwards them to `Mapper` over a VMO-backed FIFO.
/// Subsequent `Close` requests and FIFO commands reference the blob by `key`.
///
/// Each session (`Arc<Self>`) is shared between its sequential FIDL request loop and
/// `BlobMappingProvider::on_blob_overwritten` callbacks:
/// - **Normal path**: `handle_mapping_session_requests` processes `Open` and `Close` requests
///   sequentially, so `self.inner` is uncontended unless an open blob is overwritten.
/// - **Overwrite path**: `on_blob_overwritten` holds `self.inner` while opening the replacement
///   blob, sending updated `Mappings` commands, and swapping `opened_blobs`, then releases
///   `self.inner` (and drops `Arc<Self>`) before awaiting `wait_until_popped` to drop the old
///   `OpenedNode<FxBlob>` once `Mapper` has retired the FIFO slot.
struct BlobMappingSession {
    blob_directory: Arc<BlobDirectory>,
    // Uses an async mutex because the lock is held across `open_blob`, `get_mapping_info`, and
    // `AsyncSender` FIFO operations to serialise FIFO commands with `opened_blobs` updates.
    inner: AsyncMutex<SessionInner>,
}

struct SessionInner {
    sender: AsyncSender<RawMappingCommand>,
    opened_blobs: HashMap<u64, (Hash, OpenedNode<FxBlob>)>,
}

impl BlobMappingSession {
    fn new(
        blob_directory: Arc<BlobDirectory>,
        sender: AsyncSender<RawMappingCommand>,
    ) -> Arc<Self> {
        let session = Arc::new(Self {
            blob_directory: blob_directory.clone(),
            inner: AsyncMutex::new(SessionInner { sender, opened_blobs: HashMap::new() }),
        });
        blob_directory.mapping_provider().register_session(Arc::downgrade(&session));
        session
    }

    async fn send_mappings(
        sender: &mut AsyncSender<RawMappingCommand>,
        key: u64,
        node: &FxBlob,
        hash: Hash,
    ) -> Result<(u64, Option<u64>), Error> {
        let info = node.get_mapping_info(hash).await?;
        ensure!(info.uncompressed_size == 0 || !info.data.is_empty(), FxfsError::Inconsistent);
        if info.uncompressed_size == 0 {
            return Ok((0, None));
        }
        let data_extents = Extents::try_new(&info.data, 0)?;
        let merkle_extents = Extents::try_new(&info.merkle, 0)?;
        let extent_count = data_extents.len() as u32;
        let metadata_count = merkle_extents.len() as u32;
        let allocation_size = (extent_count + metadata_count) as usize * std::mem::size_of::<u64>();

        let mut payload = sender.reserve_payload(allocation_size).await?;
        let offset_in_vmo = payload.offset();

        for (mut chunk, val_res) in payload.data().chunks_mut(std::mem::size_of::<u64>()).zip(
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&merkle_extents)),
        ) {
            chunk.copy_from_slice(&val_res.to_le_bytes());
        }

        let command = MappingCommand::Mappings {
            key,
            offset: offset_in_vmo as u32,
            stored_size: info.stored_size,
            device_offset: 0,
            metadata_count,
            extent_count,
            encrypted: false,
        };

        let slot = payload.commit(command.into()).await?;
        Ok((info.uncompressed_size, Some(slot)))
    }

    /// Retrieves the extent mappings for the blob and registers the blob in the mapping session.
    /// Returns the uncompressed byte size of the blob.
    async fn open_blob(&self, key: u64, hash: Hash) -> Result<u64, Error> {
        let mut inner = self.inner.lock().await;
        ensure!(!inner.opened_blobs.contains_key(&key), FxfsError::AlreadyExists);
        let node = self.blob_directory.open_blob(hash).await?.ok_or(FxfsError::NotFound)?;
        let (uncompressed_size, _) =
            Self::send_mappings(&mut inner.sender, key, &node, hash).await?;
        inner.opened_blobs.insert(key, (hash, node));
        Ok(uncompressed_size)
    }

    /// Updates any open mappings for `hash`, keeping each replaced blob handle alive until the
    /// block driver pops the updated mappings command.
    async fn on_blob_overwritten(self: Arc<Self>, hash: Hash) {
        let pending_retirements = {
            let mut inner = self.inner.lock().await;
            let keys: Vec<u64> = inner
                .opened_blobs
                .iter()
                .filter_map(|(&k, (h, _))| (*h == hash).then_some(k))
                .collect();
            if keys.is_empty() {
                return;
            }
            // Re-open `hash` from `blob_directory` so we pin the new blob object (entries in
            // `opened_blobs` still hold the old object) and pick up the latest version if another
            // overwrite or unlink raced.
            let new_node = match self.blob_directory.open_blob(hash).await {
                Ok(Some(blob)) => blob,
                Ok(None) => return,
                Err(error) => {
                    warn!(error:?, hash:%; "Failed to open new blob after overwrite");
                    return;
                }
            };
            let mut pending_retirements = Vec::with_capacity(keys.len());
            for key in keys {
                if inner
                    .opened_blobs
                    .get(&key)
                    .is_some_and(|(_, node)| node.object_id() == new_node.object_id())
                {
                    continue;
                }
                match Self::send_mappings(&mut inner.sender, key, &new_node, hash).await {
                    Ok((_, slot)) => {
                        let popped = slot.map(|s| inner.sender.wait_until_popped(s));
                        if let Some((_, old_node)) =
                            inner.opened_blobs.insert(key, (hash, new_node.dup()))
                        {
                            pending_retirements.push((popped, old_node));
                        }
                    }
                    Err(error) => {
                        warn!(
                            error:?,
                            key,
                            hash:%;
                            "Failed to send updated mappings for overwritten blob"
                        );
                    }
                }
            }
            pending_retirements
        };

        // Drop `self` before awaiting `popped` so we do not hold a strong reference to the
        // session while waiting for the client to pop the FIFO command.
        drop(self);

        for (maybe_popped, old_node) in pending_retirements {
            if let Some(popped) = maybe_popped {
                let _ = popped.await;
            }
            drop(old_node);
        }
    }

    /// Unregisters the blob mapping and signals the block driver to terminate tracking.
    ///
    /// This is only called by clients (e.g. `BlobPagerAndVerifier`) once all client VMO handles
    /// are closed (`ZX_VMO_ZERO_CHILDREN`), so no reads are in flight and the `OpenedNode` can be
    /// dropped immediately without waiting for the driver to pop `CloseBlob`.
    async fn close_blob(&self, key: u64) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;
        if inner.opened_blobs.remove(&key).is_none() {
            return Ok(());
        }
        inner.sender.push(MappingCommand::CloseBlob { key }.into()).await?;
        Ok(())
    }

    async fn handle_mapping_session_requests(
        self: Arc<Self>,
        mut stream: fmapping::MappingSessionRequestStream,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fmapping::MappingSessionRequest::Open { key, identifier, responder } => {
                    // We expect the identifier to be a Merkle Root Hash for the blob.
                    if identifier.len() != fuchsia_hash::HASH_SIZE {
                        responder.send(Err(zx::Status::INVALID_ARGS.into_raw())).unwrap_or_else(
                            |error| warn!(error:?; "Failed to send mapping session response"),
                        );
                        continue;
                    }
                    let hash = Hash::from(<[u8; 32]>::try_from(identifier.as_slice()).unwrap());
                    match self.open_blob(key, hash).await {
                        Ok(size) => {
                            responder.send(Ok(size)).unwrap_or_else(
                                |error| warn!(error:?; "Failed to send mapping session response"),
                            );
                        }
                        Err(error) => {
                            error!(error:?; "Failed to open blob");
                            responder.send(Err(map_to_status(error).into_raw())).unwrap_or_else(
                                |error| warn!(error:?; "Failed to send mapping session response"),
                            );
                        }
                    }
                }
                fmapping::MappingSessionRequest::Close { key, responder } => {
                    let result = match self.close_blob(key).await {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            error!(error:?; "Failed to close blob");
                            Err(map_to_status(error).into_raw())
                        }
                    };
                    responder.send(result).unwrap_or_else(
                        |error| warn!(error:?; "Failed to send mapping session response"),
                    );
                }
                fmapping::MappingSessionRequest::GetProfile { name, responder } => {
                    let result = match self.blob_directory.volume().get_profile_vmo(&name).await {
                        Ok(vmo) => Ok(vmo),
                        Err(error) => Err(map_to_status(error).into_raw()),
                    };
                    responder.send(result).unwrap_or_else(
                        |error| warn!(error:?; "Failed to send mapping session response"),
                    );
                }
                fmapping::MappingSessionRequest::WriteProfile { name, profile_vmo, responder } => {
                    let result = match self
                        .blob_directory
                        .volume()
                        .write_profile_vmo(&name, profile_vmo)
                        .await
                    {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            error!(error:?, name = name.as_str(); "Failed to write profile VMO");
                            Err(map_to_status(error).into_raw())
                        }
                    };
                    responder.send(result).unwrap_or_else(
                        |error| warn!(error:?; "Failed to send mapping session response"),
                    );
                }
                fmapping::MappingSessionRequest::_UnknownMethod { ordinal, .. } => {
                    warn!(ordinal; "Unknown MappingSession method");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuchsia::fxblob::testing::{BlobFixture, new_blob_fixture, open_blob_fixture};
    use crate::fuchsia::node::FxNode;
    use crate::fuchsia::testing::TestFixture;
    use blob_writer::BlobWriter;
    use block_server::DeviceBlockService;
    use delivery_blob::{CompressionMode, Type1Blob};
    use fidl_fuchsia_io::UnlinkOptions;
    use fuchsia_async as fasync;
    use futures::channel::oneshot;
    use fxfs::lsm_tree::types::Item;
    use fxfs::object_handle::ObjectHandle;
    use fxfs::object_store::{
        AttributeId, ExtentValue, HandleOptions, ObjectKey, ObjectStore, ObjectValue,
    };
    use std::pin::pin;
    use vmo_fifo::Receiver;

    #[fuchsia::test]
    async fn test_blob_mapping_provider() {
        let fixture = new_blob_fixture().await;
        // Test with a large amount of non-compressible data to generate many extents
        let data = vec![42; 300_000];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let node = blob_dir
            .open_blob(hash)
            .await
            .expect("Failed to open blob in Fxfs")
            .expect("open_blob returned None instead of node");
        let info = node.get_mapping_info(hash).await.expect("Failed to retrieve mapping info");
        let data_extents = info.data;
        let merkle_extents = info.merkle;

        // Un-dropped nodes pin the Blob as actively opened. The unmount routine in fixture.close()
        // will wait forever for this blob to be fully closed, causing a test timeout.
        drop(node);

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let client_mapping = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .unwrap();

        let session = BlobMappingSession::new(blob_dir, sender);

        let receiver_task = fasync::unblock(move || {
            let mut receiver = Receiver::<RawMappingCommand>::new(client_mapping, 256)
                .expect("Failed to create the Receiver wrapper");

            // First Open Command
            let cmd1_raw = receiver.peek().expect("peek failed");
            let cmd1 = MappingCommand::try_from(*cmd1_raw).expect("try_from failed");

            let data_extents_container = Extents::try_new(&data_extents, 0).unwrap();
            let merkle_extents_container = Extents::try_new(&merkle_extents, 0).unwrap();

            let (cmd1_offset, cmd1_extent_count, cmd1_metadata_count) = match cmd1 {
                MappingCommand::Mappings {
                    key,
                    offset,
                    stored_size: _,
                    device_offset: _,
                    metadata_count,
                    extent_count,
                    encrypted,
                } => {
                    assert_eq!(key, 1);
                    assert_eq!(extent_count, data_extents_container.len() as u32);
                    assert_eq!(metadata_count, merkle_extents_container.len() as u32);
                    assert!(!encrypted);
                    (offset, extent_count, metadata_count)
                }
                _ => panic!("Expected Mappings command"),
            };

            // Verify payload
            let total_extents = cmd1_extent_count + cmd1_metadata_count;
            let buffer = cmd1_raw.payload_slice(cmd1_offset, total_extents * 8).to_vec();

            let mut expected_payload = Vec::new();
            for val in Extents::encode_extents(&data_extents_container)
                .chain(Extents::encode_extents(&merkle_extents_container))
            {
                expected_payload.extend_from_slice(&val.to_le_bytes());
            }
            assert_eq!(buffer, expected_payload);
            cmd1_raw.pop().expect("Failed pop_commit");

            let cmd2_raw = receiver.peek().expect("peek failed");
            let cmd2 = MappingCommand::try_from(*cmd2_raw).expect("try_from failed");
            match cmd2 {
                MappingCommand::CloseBlob { key } => assert_eq!(key, 1),
                _ => panic!("Expected CloseBlob command"),
            };
            cmd2_raw.pop().expect("Failed pop_commit");
        });

        let server_task = async move {
            let key = 1;
            let size = session.open_blob(key, hash).await.expect("open_blob failed");
            assert_eq!(size, data.len() as u64);

            session.close_blob(key).await.expect("close_blob failed on existing key");

            std::mem::drop(session);
        };

        futures::join!(receiver_task, server_task);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_missing_blob() {
        let fixture = new_blob_fixture().await;
        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .unwrap();
        let session = BlobMappingSession::new(blob_dir, sender);
        let hash = Hash::from([1u8; 32]);
        session
            .open_blob(1, hash)
            .await
            .expect_err("open_blob should fail with blob that doesn't exist");

        std::mem::drop(session);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_invalid_key() {
        let fixture = new_blob_fixture().await;
        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .unwrap();
        let session = BlobMappingSession::new(blob_dir, sender);
        session.close_blob(42).await.expect("close_blob should return Ok with invalid key");
        std::mem::drop(session);

        fixture.close().await;
    }

    // Verifies that unlinking an open blob does not deallocate its extents until all references
    // are closed.
    #[fuchsia::test]
    async fn test_unlink_open_blob() {
        let fixture = new_blob_fixture().await;
        let data = vec![77u8; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let hash_string = format!("{}", hash);

        let object_id = fixture.get_blob_handle(&hash_string).await.object_id();

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .unwrap();
        let session = BlobMappingSession::new(blob_dir, sender);

        let key = 1;
        session.open_blob(key, hash).await.expect("open_blob failed");

        // Unlink the blob from the root directory.
        fixture
            .root()
            .unlink(&hash_string, &UnlinkOptions::default())
            .await
            .expect("FIDL failed")
            .expect("unlink failed");

        // Flush the graveyard to trigger purging if the object has no open references.
        fixture.fs().graveyard().flush().await;

        // Since the blob is currently open in the mapping session, it must not be purged.
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                object_id,
                HandleOptions::default(),
                None
            )
            .await
            .is_ok(),
            "Blob was purged from store while still open in BlobMappingSession"
        );

        // Close the blob in the mapping session.
        session.close_blob(key).await.expect("close_blob failed");

        // Flush the graveyard again now that the session has released the blob.
        fixture.fs().graveyard().flush().await;

        // Now the blob must be tombstoned and purged from the object store.
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                object_id,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "Blob should have been purged from store after close_blob"
        );

        drop(session);
        fixture.close().await;
    }

    // Verifies that overwriting an open blob sends an updated `MappingCommand::Mappings` with the
    // new blob's extents and keeps the old blob's extents alive until the receiver pops the
    // updated mapping command.
    #[fuchsia::test]
    async fn test_overwrite_open_blob() {
        let fixture = new_blob_fixture().await;
        let data = vec![88u8; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let hash_string = format!("{hash}");

        let old_object_id = fixture.get_blob_handle(&hash_string).await.object_id();

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).expect("create vmo failed");
        let client_vmo =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate vmo failed");
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .expect("create sender failed");
        let mut receiver =
            Receiver::<RawMappingCommand>::new(client_vmo, PENDING_COMMANDS_CAPACITY)
                .expect("create receiver failed");
        let session = BlobMappingSession::new(blob_dir, sender);

        let key = 7;
        session.open_blob(key, hash).await.expect("open_blob failed");

        let initial_offset = {
            let msg = receiver.peek().expect("peek initial Mappings failed");
            let cmd = MappingCommand::try_from(*msg).expect("try_from failed");
            let offset = match cmd {
                MappingCommand::Mappings { key: k, offset, extent_count, .. } => {
                    assert_eq!(k, key);
                    assert_eq!(extent_count, 1);
                    offset
                }
                _ => panic!("Expected Mappings command"),
            };
            let raw_extent = u64::from_le_bytes(
                msg.payload_slice(offset, 8).to_vec().as_slice().try_into().expect("8-byte slice"),
            );
            msg.pop().expect("pop initial Mappings failed");
            raw_extent
        };

        let delivery_data = Type1Blob::generate(&data, CompressionMode::Always);
        let writer =
            fixture.create_blob(&hash.into(), true).await.expect("create_blob overwrite failed");
        let mut blob_writer = BlobWriter::create(writer, delivery_data.len() as u64)
            .await
            .expect("BlobWriter::create failed");

        let write_fut = blob_writer.write(&delivery_data);
        let inspect_and_pop = async {
            let (mut receiver, new_raw_extent) = fasync::unblock(move || {
                let msg = receiver.peek().expect("peek overwrite Mappings failed");
                let cmd = MappingCommand::try_from(*msg).expect("try_from failed");
                let offset = match cmd {
                    MappingCommand::Mappings { key: k, offset, extent_count, .. } => {
                        assert_eq!(k, key);
                        assert_eq!(extent_count, 1);
                        offset
                    }
                    _ => panic!("Expected updated Mappings command on overwrite"),
                };
                let raw_extent = u64::from_le_bytes(
                    msg.payload_slice(offset, 8)
                        .to_vec()
                        .as_slice()
                        .try_into()
                        .expect("8-byte slice"),
                );
                drop(msg);
                (receiver, raw_extent)
            })
            .await;
            assert_ne!(
                new_raw_extent, initial_offset,
                "Overwritten blob should have different disk extents"
            );

            // While the updated Mappings command is still unpopped in the FIFO, the old blob must
            // not be purged even if the graveyard is flushed.
            fixture.fs().graveyard().flush().await;
            assert!(
                ObjectStore::open_object(
                    fixture.volume().volume(),
                    old_object_id,
                    HandleOptions::default(),
                    None
                )
                .await
                .is_ok(),
                "Old blob was purged before receiver popped updated Mappings command"
            );

            // Pop the updated Mappings command so `on_blob_overwritten` drops `old_node`.
            fasync::unblock(move || {
                receiver
                    .peek()
                    .expect("peek overwrite Mappings failed")
                    .pop()
                    .expect("pop overwrite Mappings failed");
            })
            .await;
        };
        let (write_res, ()) = futures::join!(write_fut, inspect_and_pop);
        write_res.expect("blob_writer.write failed");
        drop(blob_writer);

        let new_object_id = fixture.get_blob_handle(&hash_string).await.object_id();
        assert_ne!(new_object_id, old_object_id);

        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                old_object_id,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "Old blob should have been purged after updated Mappings command was popped"
        );
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                new_object_id,
                HandleOptions::default(),
                None
            )
            .await
            .is_ok(),
            "New blob should remain in store while open in session"
        );

        session.close_blob(key).await.expect("close_blob failed");
        drop(session);
        fixture.close().await;
    }

    // Verifies that back-to-back overwrites while `session.inner` is busy pick up the latest
    // committed blob object and purge unreferenced intermediate versions immediately.
    #[fuchsia::test]
    async fn test_back_to_back_overwrite_open_blob() {
        let fixture = new_blob_fixture().await;
        let data = vec![0x55u8; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let hash_string = format!("{hash}");
        let obj_id_1 = fixture.get_blob_handle(&hash_string).await.object_id();

        let blocker1 = fixture.write_blob(&vec![0xA1u8; 4096], CompressionMode::Never).await;
        let blocker2 = fixture.write_blob(&vec![0xA2u8; 4096], CompressionMode::Never).await;
        let blocker3 = fixture.write_blob(&vec![0xA3u8; 4096], CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("downcast to BlobDirectory failed");

        // Use a 2-slot FIFO so `open_blob(12)` holds `session.inner` while two overwrites commit.
        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).expect("create vmo failed");
        let client_vmo =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate vmo failed");
        let sender = AsyncSender::<RawMappingCommand>::new(vmo, Alignment::SIZE_8B, 2)
            .expect("sender new failed");
        let mut receiver =
            Receiver::<RawMappingCommand>::new(client_vmo, 2).expect("receiver new failed");
        let session = BlobMappingSession::new(blob_dir, sender);

        session.open_blob(1, hash).await.expect("open_blob 1 failed");
        receiver.peek().expect("peek open 1 failed").pop().expect("pop open 1 failed");

        session.open_blob(10, blocker1).await.expect("open_blob 10 failed");
        session.open_blob(11, blocker2).await.expect("open_blob 11 failed");
        let mut open_12_fut = Box::pin(session.open_blob(12, blocker3));
        for _ in 0..5 {
            assert!(futures::poll!(&mut open_12_fut).is_pending());
            fasync::Task::local(async {}).await;
        }

        let delivery_2 = Type1Blob::generate(&data, CompressionMode::Always);
        let writer1_proxy =
            fixture.create_blob(&hash.into(), true).await.expect("create_blob 1 failed");
        let mut blob_writer1 = BlobWriter::create(writer1_proxy, delivery_2.len() as u64)
            .await
            .expect("BlobWriter::create 1 failed");
        let mut write1_fut = pin!(blob_writer1.write(&delivery_2));

        let obj_id_2 = loop {
            assert!(futures::poll!(&mut write1_fut).is_pending());
            let current_id = fixture.get_blob_handle(&hash_string).await.object_id();
            if current_id != obj_id_1 {
                break current_id;
            }
            fasync::Task::local(async {}).await;
        };

        let delivery_3 = Type1Blob::generate(&data, CompressionMode::Never);
        let writer2_proxy =
            fixture.create_blob(&hash.into(), true).await.expect("create_blob 2 failed");
        let mut blob_writer2 = BlobWriter::create(writer2_proxy, delivery_3.len() as u64)
            .await
            .expect("BlobWriter::create 2 failed");
        blob_writer2.write(&delivery_3).await.expect("blob_writer2.write failed");
        drop(blob_writer2);

        let obj_id_3 = fixture.get_blob_handle(&hash_string).await.object_id();
        assert_ne!(obj_id_3, obj_id_2);

        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_2,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "Unopened intermediate blob obj_id_2 should already be purged"
        );

        let expected_extent_3 = {
            let info = fixture
                .get_opened_blob(hash)
                .await
                .expect("get_opened_blob 3 failed")
                .get_mapping_info(hash)
                .await
                .expect("get_mapping_info 3 failed");
            let extents = Extents::try_new(&info.data, 0).expect("Extents::try_new failed");
            Extents::encode_extents(&extents).next().expect("missing encoded extent")
        };

        let drain_fut = fasync::unblock(move || {
            let mut latest_key1_extent = None;
            for _ in 0..4 {
                let msg = receiver.peek().expect("peek failed");
                let cmd = MappingCommand::try_from(*msg).expect("decode failed");
                if let MappingCommand::Mappings { key: 1, offset, extent_count, .. } = cmd {
                    assert_eq!(extent_count, 1);
                    let raw = u64::from_le_bytes(
                        msg.payload_slice(offset, 8)
                            .to_vec()
                            .as_slice()
                            .try_into()
                            .expect("8-byte extent"),
                    );
                    latest_key1_extent = Some(raw);
                }
                msg.pop().expect("pop failed");
            }
            (receiver, latest_key1_extent.expect("missing updated Mappings for key 1"))
        });

        let (open_12_res, write1_res, (mut receiver, mapped_extent)) =
            futures::join!(open_12_fut, write1_fut, drain_fut);
        open_12_res.expect("open_blob 12 failed");
        write1_res.expect("blob_writer1.write failed");
        assert_eq!(mapped_extent, expected_extent_3);

        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_1,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "Original blob obj_id_1 should be purged after updated Mappings was popped"
        );
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_3,
                HandleOptions::default(),
                None
            )
            .await
            .is_ok(),
            "Latest blob obj_id_3 must remain pinned while key 1 is open"
        );

        for k in [1, 10, 11, 12] {
            let close_fut = session.close_blob(k);
            let pop_fut = fasync::unblock(move || {
                receiver.peek().expect("peek close failed").pop().expect("pop close failed");
                receiver
            });
            let (res, recv) = futures::join!(close_fut, pop_fut);
            res.expect("close_blob failed");
            receiver = recv;
        }
        drop(session);
        fixture.close().await;
    }

    // Verifies that if a blob is overwritten and then unlinked before `on_blob_overwritten` opens
    // the replacement, `on_blob_overwritten` exits cleanly and keeps `obj_id_1` pinned until
    // `close_blob`.
    #[fuchsia::test]
    async fn test_overwrite_followed_by_unlink_open_blob() {
        let fixture = new_blob_fixture().await;
        let data = vec![0x66u8; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let hash_string = format!("{hash}");
        let obj_id_1 = fixture.get_blob_handle(&hash_string).await.object_id();

        let blocker1 = fixture.write_blob(&vec![0xB1u8; 4096], CompressionMode::Never).await;
        let blocker2 = fixture.write_blob(&vec![0xB2u8; 4096], CompressionMode::Never).await;
        let blocker3 = fixture.write_blob(&vec![0xB3u8; 4096], CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("downcast to BlobDirectory failed");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).expect("create vmo failed");
        let client_vmo =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate vmo failed");
        let sender = AsyncSender::<RawMappingCommand>::new(vmo, Alignment::SIZE_8B, 2)
            .expect("sender new failed");
        let mut receiver =
            Receiver::<RawMappingCommand>::new(client_vmo, 2).expect("receiver new failed");
        let session = BlobMappingSession::new(blob_dir, sender);

        session.open_blob(1, hash).await.expect("open_blob 1 failed");
        receiver.peek().expect("peek open 1 failed").pop().expect("pop open 1 failed");

        session.open_blob(10, blocker1).await.expect("open_blob 10 failed");
        session.open_blob(11, blocker2).await.expect("open_blob 11 failed");
        let mut open_12_fut = Box::pin(session.open_blob(12, blocker3));
        for _ in 0..5 {
            assert!(futures::poll!(&mut open_12_fut).is_pending());
            fasync::Task::local(async {}).await;
        }

        let delivery_2 = Type1Blob::generate(&data, CompressionMode::Always);
        let writer_proxy =
            fixture.create_blob(&hash.into(), true).await.expect("create_blob failed");
        let mut blob_writer = BlobWriter::create(writer_proxy, delivery_2.len() as u64)
            .await
            .expect("BlobWriter::create failed");
        let mut write_fut = pin!(blob_writer.write(&delivery_2));

        let obj_id_2 = loop {
            assert!(futures::poll!(&mut write_fut).is_pending());
            let current_id = fixture.get_blob_handle(&hash_string).await.object_id();
            if current_id != obj_id_1 {
                break current_id;
            }
            fasync::Task::local(async {}).await;
        };

        fixture
            .root()
            .unlink(&hash_string, &UnlinkOptions::default())
            .await
            .expect("unlink wire call failed")
            .expect("unlink failed");
        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_2,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "Unlinked replacement obj_id_2 should be purged immediately"
        );

        let drain_blockers = fasync::unblock(move || {
            for _ in 0..3 {
                receiver.peek().expect("peek blocker failed").pop().expect("pop blocker failed");
            }
            receiver
        });
        let (open_12_res, write_res, mut receiver) =
            futures::join!(open_12_fut, write_fut, drain_blockers);
        open_12_res.expect("open_blob 12 failed");
        write_res.expect("blob_writer.write failed");

        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_1,
                HandleOptions::default(),
                None
            )
            .await
            .is_ok(),
            "obj_id_1 must remain alive while key 1 is still open in session"
        );

        for k in [1, 10, 11, 12] {
            let close_fut = session.close_blob(k);
            let pop_fut = fasync::unblock(move || {
                receiver.peek().expect("peek close failed").pop().expect("pop close failed");
                receiver
            });
            let (res, recv) = futures::join!(close_fut, pop_fut);
            res.expect("close_blob failed");
            receiver = recv;
        }
        fixture.fs().graveyard().flush().await;
        assert!(
            ObjectStore::open_object(
                fixture.volume().volume(),
                obj_id_1,
                HandleOptions::default(),
                None
            )
            .await
            .is_err(),
            "obj_id_1 must be purged after key 1 is closed"
        );

        drop(session);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_mapping_provider_and_mapping_session() {
        let fixture = new_blob_fixture().await;
        let data = vec![42; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let scope = blob_dir.volume().scope().clone();

        // Spawn the mapping provider stream.
        let (provider_proxy, provider_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingProviderMarker>();
        scope.spawn(async move {
            BlobMappingProvider::handle_mapping_provider_requests(
                blob_dir,
                provider_server_end.into_stream(),
            )
            .await;
        });

        // Open a mapping session from the mapping provider
        let (session_proxy, session_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingSessionMarker>();
        let _shared_vmo = provider_proxy
            .open_session(session_server_end)
            .await
            .expect("open_session failed")
            .expect("vmo returned an error");

        let not_found_profile_err = session_proxy
            .get_profile(".boot")
            .await
            .expect("get_profile wire call failed")
            .expect_err("expected get_profile(.boot) to return NOT_FOUND");
        assert_eq!(not_found_profile_err, zx::Status::NOT_FOUND.into_raw());

        let profile_data = vec![0xABu8; 4096];
        let write_vmo = zx::Vmo::create(profile_data.len() as u64).expect("create vmo failed");
        write_vmo.write(&profile_data, 0).expect("vmo write failed");
        session_proxy
            .write_profile(".boot", write_vmo)
            .await
            .expect("write_profile wire call failed")
            .expect("write_profile returned an error");

        let read_vmo = session_proxy
            .get_profile(".boot")
            .await
            .expect("get_profile wire call failed")
            .expect("get_profile returned an error");
        assert_eq!(
            read_vmo.read_to_vec::<u8>(0, profile_data.len() as u64).expect("vmo read failed"),
            profile_data
        );

        let id: [u8; 32] = hash.into();
        let key = 1;
        let size = session_proxy
            .open(key, &id)
            .await
            .expect("open failed")
            .expect("open explicitly returned an error");

        assert_eq!(size, 8192);

        session_proxy
            .close(key)
            .await
            .expect("close failed")
            .expect("close explicitly returned an error");

        // Test some failures

        // Sending an invalid length hash to open should return INVALID_ARGS
        let bad_length_id = vec![1u8, 2, 3];
        let invalid_args_err = session_proxy
            .open(key, &bad_length_id)
            .await
            .expect("open wire call failed")
            .unwrap_err();
        assert_eq!(invalid_args_err, zx::Status::INVALID_ARGS.into_raw());

        // Sending a valid length hash that does not exist should return NOT_FOUND
        let not_found_id = [0u8; 32];
        let not_found_err = session_proxy
            .open(key, &not_found_id)
            .await
            .expect("open wire call failed")
            .unwrap_err();
        assert_eq!(not_found_err, zx::Status::NOT_FOUND.into_raw());

        // Trying to close an invalid blob key currently always succeeds. The BlobMappingServer
        // unconditionally passes the command down the FIFO queue to the block driver and doesn't
        // explicitly track active connections.
        session_proxy.close(123).await.expect("close wire call failed").expect("close failed");

        fixture.close().await;
    }

    async fn run_blob_mapping_test(
        fixture: &TestFixture,
        test_data: &[u8],
        mode: CompressionMode,
        min_data_extents: usize,
        min_merkle_extents: usize,
    ) {
        let hash = fixture.write_blob(test_data, mode).await;
        run_blob_mapping_test_with_hash(
            fixture,
            hash,
            test_data,
            min_data_extents,
            min_merkle_extents,
        )
        .await;
    }

    async fn write_blob_chunked(fx: &TestFixture, data: &[u8], mode: CompressionMode) -> Hash {
        let hash = fuchsia_merkle::root_from_slice(data);
        let compressed_data = Type1Blob::generate(data, mode);
        let writer = fx.create_blob(&hash.into(), false).await.expect("create blob failed");
        let vmo = writer
            .get_vmo(compressed_data.len() as u64)
            .await
            .expect("transport error on get_vmo")
            .expect("failed to get vmo");
        let vmo_size = vmo.get_size().expect("failed to get vmo size");

        // Write in small chunks (512 B) to allocate extents across transactions.
        let chunk_size = 512;
        let mut write_offset = 0u64;
        let mut bytes_left = compressed_data.len() as u64;
        while bytes_left > 0 {
            let chunk_len = std::cmp::min(bytes_left, chunk_size);
            vmo.write(
                &compressed_data[write_offset as usize..(write_offset + chunk_len) as usize],
                write_offset % vmo_size,
            )
            .expect("failed to write to vmo");
            let _ = writer
                .bytes_ready(chunk_len)
                .await
                .expect("transport error on bytes_ready")
                .expect("failed to write data to vmo");
            write_offset += chunk_len;
            bytes_left -= chunk_len;
        }
        hash
    }

    async fn run_blob_mapping_test_with_hash(
        fixture: &TestFixture,
        hash: Hash,
        uncompressed_data: &[u8],
        min_data_extents: usize,
        min_merkle_extents: usize,
    ) {
        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let scope = blob_dir.volume().scope().clone();

        let (provider_proxy, provider_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingProviderMarker>();
        scope.spawn(async move {
            BlobMappingProvider::handle_mapping_provider_requests(
                blob_dir,
                provider_server_end.into_stream(),
            )
            .await;
        });

        let (session_proxy, session_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingSessionMarker>();
        let mapping_vmo = provider_proxy
            .open_session(session_server_end)
            .await
            .expect("open_session failed")
            .expect("vmo returned an error");

        let receiver = Arc::new(
            vmo_fifo::Receiver::<RawMappingCommand>::new(mapping_vmo, PENDING_COMMANDS_CAPACITY)
                .expect("Failed to create receiver"),
        );

        let device = fixture.fs().device().clone();
        let service: Arc<dyn mapping::reader::BlockService> =
            Arc::new(DeviceBlockService::new(device));

        let port = zx::Port::create();
        let delivery_queue = zx::Vmo::create(mapping::DELIVERY_VMO_SIZE)
            .expect("Failed to create delivery_queue VMO");
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let vmo_provider = Arc::new(blob_pager_and_verifier::TestVmoProvider::new(
            pager.clone(),
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        let delivery_receiver = vmo_fifo::Receiver::<mapping::RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();
        let _delivery_processor = blob_pager_and_verifier::DeliveryQueueProcessor::spawn(
            delivery_receiver,
            vmo_provider.clone(),
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();

        let verifier = block_server::verifier::Verifier::new(delivery_queue).unwrap();
        let files = Arc::new(mapping::Files::new(
            service,
            verifier,
            port.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));

        let id: [u8; 32] = hash.into();
        let key = 1;
        let blob_size = session_proxy
            .open(key, &id)
            .await
            .expect("open failed")
            .expect("open explicitly returned an error");

        assert_eq!(blob_size as usize, uncompressed_data.len());

        let msg = receiver.claim().expect("Failed to claim message");
        if let Ok(MappingCommand::Mappings { extent_count, metadata_count, .. }) =
            MappingCommand::try_from(*msg)
        {
            if (extent_count as usize) < min_data_extents
                || (metadata_count as usize) < min_merkle_extents
            {
                panic!(
                    "EXTENTS MISMATCH: extent_count = {}, metadata_count = {}, \
                     required min_data = {}, min_merkle = {}",
                    extent_count, metadata_count, min_data_extents, min_merkle_extents
                );
            }
        }
        mapping::process_mapping_command(msg, &receiver, &files)
            .expect("process_mapping_command failed");

        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, blob_size).unwrap();
        vmo_provider
            .register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

        let _pager_thread = files.spawn_pager_thread("blob-pager");

        let (tx, rx) = oneshot::channel();
        let len = uncompressed_data.len();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; len];
            paged_vmo.read(&mut buf, 0).expect("paged vmo read failed");
            let _ = tx.send(buf);
        });

        let read_bytes = rx.await.unwrap();
        assert_eq!(&read_bytes[..], &uncompressed_data[..]);

        session_proxy.close(key).await.expect("close failed").expect("close error");
        let msg = receiver.claim().expect("Failed to claim close message");
        mapping::process_mapping_command(msg, &receiver, &files)
            .expect("process_mapping_command failed");
    }

    /// Size of the blobs `fragment_free_space` uses to consume contiguous free space.  This is
    /// the blob writer's flush size, so each one is written with a single allocation request.
    const ANCHOR_BLOB_SIZE: usize = 131_072;

    /// Fills the volume with `block_size` blobs and unlinks every second one, then writes anchor
    /// blobs until the only free space left is the scattered `block_size` holes.
    async fn fragment_free_space(fixture: TestFixture, block_size: usize) -> TestFixture {
        let mut data = vec![0u8; block_size];
        let mut hashes = Vec::new();
        let mut i = 0usize;
        loop {
            rand::fill(&mut data[..1024]);
            data[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let hash = fuchsia_merkle::root_from_slice(&data);
            let delivery_data = Type1Blob::generate(&data, CompressionMode::Never);
            let writer = match fixture.create_blob(&hash.into(), false).await {
                Ok(w) => w,
                Err(_) => break,
            };
            if let Ok(mut blob_writer) =
                BlobWriter::create(writer, delivery_data.len() as u64).await
            {
                if blob_writer.write(&delivery_data).await.is_err() {
                    break;
                }
            } else {
                break;
            }
            hashes.push(hash);
            i += 1;
        }

        // Unlink every second small blob to create scattered free space holes.
        let root = fixture.root();
        for ix in (0..hashes.len()).step_by(2) {
            let _ = root.unlink(&format!("{}", hashes[ix]), &UnlinkOptions::default()).await;
        }

        // Remount fixture so unlinked blobs are purged and blocks are freed to allocator.
        let device = fixture.close().await;
        let fixture = open_blob_fixture(device).await;

        // Filling the volume stops when the remaining space is reserved, which can leave a large
        // contiguous region that was never allocated.  How large depends on how much metadata the
        // writes above produced, so rather than guessing its size, write anchor blobs (each one is
        // a single allocation request) until one has to be split across the holes.
        {
            let blob_dir = fixture
                .volume()
                .root()
                .clone()
                .as_node()
                .into_any()
                .downcast::<BlobDirectory>()
                .expect("Failed to downcast root directory to BlobDirectory");
            let mut anchor_data = vec![0u8; ANCHOR_BLOB_SIZE];
            loop {
                rand::fill(&mut anchor_data[..1024]);
                let hash = fixture.write_blob(&anchor_data, CompressionMode::Never).await;
                let node = blob_dir
                    .open_blob(hash)
                    .await
                    .expect("Failed to open anchor blob")
                    .expect("Anchor blob not found");
                let info = node.get_mapping_info(hash).await.expect("get_mapping_info failed");
                if info.data.len() > 1 {
                    break;
                }
            }
        }

        fixture
    }

    #[fuchsia::test]
    async fn test_fxfs_blob_mapping_uncompressed() {
        let fixture = new_blob_fixture().await;
        let test_data = vec![123u8; 8192];
        run_blob_mapping_test(&fixture, &test_data, CompressionMode::Never, 1, 0).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_fxfs_blob_mapping_compressed() {
        let fixture = new_blob_fixture().await;
        // Generate pseudo-random compressible test data.
        let mut test_data = vec![0u8; 16384];
        for i in 0..test_data.len() {
            test_data[i] = ((i / 64) % 256) as u8;
        }
        run_blob_mapping_test(&fixture, &test_data, CompressionMode::Always, 1, 0).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_fxfs_blob_mapping_fragmented_data() {
        let fixture = new_blob_fixture().await;
        let fixture = fragment_free_space(fixture, 32768).await;

        let mut uncompressed_data = vec![0u8; 262_144];
        rand::fill(&mut uncompressed_data[..]);
        let hash1 = write_blob_chunked(&fixture, &uncompressed_data, CompressionMode::Never).await;
        run_blob_mapping_test_with_hash(&fixture, hash1, &uncompressed_data, 2, 0).await;
        fixture
            .root()
            .unlink(&format!("{}", hash1), &UnlinkOptions::default())
            .await
            .unwrap()
            .unwrap();

        let mut compressed_data = vec![0u8; 262_144];
        rand::fill(&mut compressed_data[..]);
        let hash2 = write_blob_chunked(&fixture, &compressed_data, CompressionMode::Always).await;
        run_blob_mapping_test_with_hash(&fixture, hash2, &compressed_data, 2, 0).await;

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_blob_mapping_provider_large_extent() {
        let fixture = new_blob_fixture().await;
        let data = vec![42; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let node = blob_dir
            .open_blob(hash)
            .await
            .expect("Failed to open blob in Fxfs")
            .expect("open_blob returned None instead of node");
        let object_id = node.object_id();
        let original_info =
            node.get_mapping_info(hash).await.expect("Failed to retrieve mapping info");
        assert_eq!(original_info.data.len(), 1);
        let start_dev = original_info.data[0].device_offset().unwrap();
        drop(node);

        // Replace the blob's single 8 KiB data extent in the in-memory LSM tree with a single
        // extent larger than MAX_REGULAR_EXTENT_BLOCKS (0x3fff_ffff blocks) so Extents::try_new
        // splits it into 2 encoded entries.
        let large_len = (0x3fff_ffffu64 + 5) * mapping::BLOCK_SIZE;
        let key = ObjectKey::extent(object_id, AttributeId::DATA, 0..large_len);
        let lower_bound = key.key_for_merge_into();
        fixture.volume().volume().store().tree().merge_into(
            Item::new(key, ObjectValue::Extent(ExtentValue::new_raw(start_dev, 0))),
            &lower_bound,
        );

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let client_mapping = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let sender = AsyncSender::<RawMappingCommand>::new(
            vmo,
            Alignment::SIZE_8B,
            PENDING_COMMANDS_CAPACITY,
        )
        .unwrap();
        let session = BlobMappingSession::new(blob_dir, sender);

        let merkle_extents = original_info.merkle;
        let receiver_task = fasync::unblock(move || {
            let mut receiver = Receiver::<RawMappingCommand>::new(client_mapping, 256)
                .expect("Failed to create the Receiver wrapper");

            let cmd1_raw = receiver.peek().expect("peek failed");
            let cmd1 = MappingCommand::try_from(*cmd1_raw).expect("try_from failed");

            let (cmd1_offset, cmd1_extent_count, cmd1_metadata_count) = match cmd1 {
                MappingCommand::Mappings {
                    key,
                    offset,
                    stored_size: _,
                    device_offset: _,
                    metadata_count,
                    extent_count,
                    encrypted,
                } => {
                    assert_eq!(key, 1);
                    assert_eq!(extent_count, 2);
                    assert_eq!(metadata_count, merkle_extents.len() as u32);
                    assert!(!encrypted);
                    (offset, extent_count, metadata_count)
                }
                _ => panic!("Expected Mappings command"),
            };

            let total_extents = cmd1_extent_count + cmd1_metadata_count;
            let buffer = cmd1_raw.payload_slice(cmd1_offset, total_extents * 8).to_vec();

            let expected_data_extents =
                Extents::try_new([mapping::Extent::new(0..large_len, Some(start_dev))], 0).unwrap();
            let expected_merkle_extents = Extents::try_new(&merkle_extents, 0).unwrap();
            let mut expected_payload = Vec::new();
            for val in Extents::encode_extents(&expected_data_extents)
                .chain(Extents::encode_extents(&expected_merkle_extents))
            {
                expected_payload.extend_from_slice(&val.to_le_bytes());
            }
            assert_eq!(buffer, expected_payload);
            cmd1_raw.pop().expect("Failed pop_commit");

            let cmd2_raw = receiver.peek().expect("peek failed");
            let cmd2 = MappingCommand::try_from(*cmd2_raw).expect("try_from failed");
            match cmd2 {
                MappingCommand::CloseBlob { key } => assert_eq!(key, 1),
                _ => panic!("Expected CloseBlob command"),
            };
            cmd2_raw.pop().expect("Failed pop_commit");
        });

        let server_task = async move {
            let key = 1;
            let size = session.open_blob(key, hash).await.expect("open_blob failed");
            assert_eq!(size, data.len() as u64);
            session.close_blob(key).await.expect("close_blob failed");
            std::mem::drop(session);
        };

        futures::join!(receiver_task, server_task);

        fixture.close().await;
    }
}
