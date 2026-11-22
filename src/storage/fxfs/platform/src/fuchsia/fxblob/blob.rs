// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains the [`FxBlob`] node type used to represent an immutable blob persisted to
//! disk which can be read back.

use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::node::FxNode;
use crate::fuchsia::volume::FxVolume;
use anyhow::{Error, anyhow, ensure};
use fuchsia_merkle::Hash;
use futures::{StreamExt, TryStreamExt};
use fxfs::blob_metadata::{BlobMetadata, FxfsBlobMetadataExt};
use fxfs::errors::FxfsError;
use fxfs::object_store::{AttributeId, ObjectDescriptor, StoreObjectHandle};
use fxfs_macros::ToWeakNode;
use mapping::Extent as MappingExtent;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The size and extent mappings for a blob's data and merkle attributes.
///
/// Each `MappingExtent` maps a logical byte range to a physical device offset.
/// Extents are sorted by logical offset and non-overlapping.
pub struct BlobMappingInfo {
    pub uncompressed_size: u64,
    pub stored_size: u64,
    pub data: Vec<MappingExtent>,
    pub merkle: Vec<MappingExtent>,
}

// When the top bit of the open count is set, it means the file has been deleted and when the count
// drops to zero, it will be tombstoned.  Once it has dropped to zero, it cannot be opened again
// (assertions will fire).
const PURGED: usize = 1 << (usize::BITS - 1);

const EMPTY_BLOB_HASH: Hash = Hash::from_array([
    0x15, 0xec, 0x7b, 0xf0, 0xb5, 0x07, 0x32, 0xb4, 0x9f, 0x82, 0x28, 0xe0, 0x7d, 0x24, 0x36, 0x53,
    0x38, 0xf9, 0xe3, 0xab, 0x99, 0x4b, 0x00, 0xaf, 0x08, 0xe5, 0xa3, 0xbf, 0xfe, 0x55, 0xfd, 0x8b,
]);

/// Represents an immutable blob stored on Fxfs.
#[derive(ToWeakNode)]
pub struct FxBlob {
    handle: StoreObjectHandle<FxVolume>,
    open_count: AtomicUsize,
}

// Fuchsia can have many open blobs at once. The size of FxBlob is important.
static_assertions::const_assert!(size_of::<FxBlob>() <= 32);

impl FxBlob {
    pub fn new(handle: StoreObjectHandle<FxVolume>) -> Arc<Self> {
        Arc::new(Self { handle, open_count: AtomicUsize::new(0) })
    }

    /// Validates the blob's metadata and returns its uncompressed size, stored size, and the
    /// extents mapping logical offsets to device offsets for both data and merkle attributes.
    pub async fn get_mapping_info(&self, merkle_root: Hash) -> Result<BlobMappingInfo, Error> {
        let stored_size = self
            .handle
            .store()
            .get_attribute_size(self.handle.object_id(), AttributeId::DATA)
            .await?;
        let metadata = BlobMetadata::read_from(&self.handle).await?;
        let uncompressed_size = metadata.format.uncompressed_size().unwrap_or(stored_size);

        // Check the uncompressed size of the blob against the number of merkle leaves. This only
        // ensures that the uncompressed size lands within the same merkle block as the true
        // uncompressed size. To fully validate the uncompressed size requires reading in the last
        // merkle block and verifying it.
        let expect_merkle_hashes = if uncompressed_size <= fuchsia_merkle::BLOCK_SIZE as u64 {
            0
        } else {
            uncompressed_size.div_ceil(fuchsia_merkle::BLOCK_SIZE as u64)
        };
        ensure!(
            metadata.merkle_leaves.len() as u64 == expect_merkle_hashes,
            FxfsError::IntegrityError
        );

        // Fully validate the empty blob and its size.
        ensure!(
            (uncompressed_size == 0) == (merkle_root == EMPTY_BLOB_HASH),
            FxfsError::IntegrityError
        );

        let tree = &self.handle.store().tree();
        let layer_set = tree.layer_set();

        // Query data extents
        let mut merger = layer_set.merger();
        let stream = self.handle.extent_stream(&mut merger, AttributeId::DATA).await?;

        // TODO(https://fxbug.dev/535489428): Remove use of vec here.
        let data = stream
            .map(|res| {
                let extent = res?;
                MappingExtent::try_new(extent.logical_range(), Some(extent.device_range().start))
                    .map_err(|_| anyhow!(FxfsError::Inconsistent))
            })
            .try_collect()
            .await?;

        // Query Merkle extents
        let mut merger = layer_set.merger();
        let stream = self.handle.extent_stream(&mut merger, AttributeId::BLOB_METADATA).await?;
        let merkle = stream
            .map(|res| {
                let extent = res?;
                MappingExtent::try_new(extent.logical_range(), Some(extent.device_range().start))
                    .map_err(|_| anyhow!(FxfsError::Inconsistent))
            })
            .try_collect()
            .await?;

        Ok(BlobMappingInfo { uncompressed_size, stored_size, data, merkle })
    }
}

impl Drop for FxBlob {
    fn drop(&mut self) {
        let volume = self.handle.owner();
        volume.cache().remove(self);
    }
}

impl FxNode for FxBlob {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!(); // Add a parent back-reference if needed.
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // NOP
    }

    fn open_count_add_one(&self) {
        let old = self.open_count.fetch_add(1, Ordering::Relaxed);
        assert!(old != PURGED && old != PURGED - 1);
    }

    fn open_count_sub_one(self: Arc<Self>) {
        let old = self.open_count.fetch_sub(1, Ordering::Relaxed);
        assert!(old & !PURGED > 0);
        if old == PURGED + 1 {
            let store = self.handle.store();
            store
                .filesystem()
                .ensure_local()
                .graveyard()
                .queue_tombstone_object(store.store_object_id(), self.object_id());
        }
    }

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::File
    }

    fn terminate(&self) {}

    fn mark_to_be_purged(self: Arc<Self>) {
        let old = self.open_count.fetch_or(PURGED, Ordering::Relaxed);
        assert!(old & PURGED == 0);
        if old == 0 {
            let store = self.handle.store();
            store
                .filesystem()
                .ensure_local()
                .graveyard()
                .queue_tombstone_object(store.store_object_id(), self.object_id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuchsia::fxblob::testing::{BlobFixture, new_blob_fixture, open_blob_fixture};
    use crate::fuchsia::volume::READ_AHEAD_SIZE;
    use assert_matches::assert_matches;
    use blob_writer::BlobWriter;
    use delivery_blob::compression::CompressionAlgorithm;
    use delivery_blob::{CompressionMode, Type1Blob};
    use fxfs::blob_metadata::BlobFormat;
    use fxfs::object_handle::{ObjectHandle, WriteObjectHandle};
    use fxfs::object_store::{HandleOptions, ObjectStore};
    use fxfs_make_blob_image::FxBlobBuilder;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;
    use storage_units::page_size;

    const BLOCK_SIZE: u64 = fuchsia_merkle::BLOCK_SIZE as u64;
    const CHUNK_SIZE: usize = 32 * 1024;

    #[fuchsia::test(threads = 10)]
    async fn test_empty_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_large_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_multiple_mapping_extents() {
        let fixture = new_blob_fixture().await;

        {
            let data = vec![0xff; 3_000_000];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let name = format!("{}", hash);

            let handle = fixture.get_blob_handle(&name).await;
            let mut transaction =
                handle.new_transaction().await.expect("failed to create transaction");
            let mut buf = handle.allocate_buffer(8192).await;
            buf.as_mut_ptr_slice().fill(0xaa);
            handle
                .txn_write(&mut transaction, 0, buf.as_ref())
                .await
                .expect("txn_write at offset 0 failed");
            handle
                .txn_write(&mut transaction, 16384, buf.as_ref())
                .await
                .expect("txn_write at offset 16384 failed");
            transaction.commit().await.expect("failed to commit transaction");

            let blob = fixture.get_blob(hash).await.expect("getting blob failed");
            let info = blob.get_mapping_info(hash).await.expect("get_mapping_info failed");

            assert!(!info.merkle.is_empty(), "Expected at least one merkle extent");
            assert!(info.data.len() > 1, "Expected multiple data extents, got {}", info.data.len());

            // The overwritten extents should each be exactly 8192 bytes.
            let overwrite_extent = info
                .data
                .iter()
                .find(|e| e.logical_range().start == 0)
                .expect("extent at offset 0");
            assert_eq!(
                overwrite_extent.logical_range().end - overwrite_extent.logical_range().start,
                8192
            );
            let overwrite_extent = info
                .data
                .iter()
                .find(|e| e.logical_range().start == 16384)
                .expect("extent at offset 16384");
            assert_eq!(
                overwrite_extent.logical_range().end - overwrite_extent.logical_range().start,
                8192
            );
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_large_compressed_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Always).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_non_page_aligned_blob() {
        let fixture = new_blob_fixture().await;

        let page_size = page_size().get() as usize;
        let data = vec![0xffu8; page_size - 1];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        assert_eq!(fixture.read_blob(hash).await, data);

        {
            let vmo = fixture.get_blob_vmo(hash).await;
            let mut buf = vec![0x11u8; page_size];
            vmo.read(&mut buf[..], 0).expect("vmo read failed");
            assert_eq!(data, buf[..data.len()]);
            // Ensure the tail is zeroed
            assert_eq!(buf[data.len()], 0);
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_blob_invalid_contents() {
        let fixture = new_blob_fixture().await;

        let data = vec![0xffu8; (READ_AHEAD_SIZE + BLOCK_SIZE) as usize];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let name = format!("{}", hash);

        {
            // Overwrite the second read-ahead window.  The first window should successfully verify.
            let handle = fixture.get_blob_handle(&name).await;
            let mut transaction =
                handle.new_transaction().await.expect("failed to create transaction");
            let mut buf = handle.allocate_buffer(BLOCK_SIZE as usize).await;
            buf.fill(0);
            handle
                .txn_write(&mut transaction, READ_AHEAD_SIZE, buf.as_ref())
                .await
                .expect("txn_write failed");
            transaction.commit().await.expect("failed to commit transaction");
        }

        {
            let blob_vmo = fixture.get_blob_vmo(hash).await;
            let mut buf = vec![0; BLOCK_SIZE as usize];
            assert_matches!(blob_vmo.read(&mut buf[..], 0), Ok(_));
            assert_matches!(
                blob_vmo.read(&mut buf[..], READ_AHEAD_SIZE),
                Err(zx::Status::IO_DATA_INTEGRITY)
            );
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_blob_invalid_uncompressed_size() {
        let fixture = new_blob_fixture().await;

        // 1. A single-block blob (uncompressed_size <= BLOCK_SIZE) should have 0 merkle leaves in
        // metadata. If metadata contains 1 leaf (even if it equals the root hash), opening fails.
        {
            let data = vec![0xaa; BLOCK_SIZE as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            BlobMetadata { merkle_leaves: vec![hash.into()], format: BlobFormat::Uncompressed }
                .write_to(&handle)
                .await
                .expect("write_to failed");
            let blob = fixture.get_blob(hash).await.expect("get_blob failed");
            let err =
                blob.get_mapping_info(hash).await.err().expect("get_mapping_info should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 2. A multi-block compressed blob (uncompressed_size > BLOCK_SIZE) with 0 merkle leaves in
        // metadata must fail to open with IntegrityError.
        {
            let data = vec![0xbb; (BLOCK_SIZE * 2) as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Always).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            let mut metadata = BlobMetadata::read_from(&handle).await.expect("read_from failed");
            metadata.merkle_leaves.clear();
            metadata.write_to(&handle).await.expect("write_to failed");
            let blob = fixture.get_blob(hash).await.expect("get_blob failed");
            let err =
                blob.get_mapping_info(hash).await.err().expect("get_mapping_info should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 3. A multi-block blob whose uncompressed size disagrees with the number of merkle leaves
        // (even when merkle_leaves validly hashes to the merkle root) must fail with
        // IntegrityError.
        {
            let data = vec![0xcc; (BLOCK_SIZE * 2) as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(BLOCK_SIZE * 3).await.expect("truncate failed");
            let blob = fixture.get_blob(hash).await.expect("get_blob failed");
            let err =
                blob.get_mapping_info(hash).await.err().expect("get_mapping_info should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 4. A non-empty single-block blob truncated to 0 bytes must fail to open with
        // IntegrityError.
        {
            let data = vec![0xdd; 100];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(0).await.expect("truncate failed");
            let blob = fixture.get_blob(hash).await.expect("get_blob failed");
            let err =
                blob.get_mapping_info(hash).await.err().expect("get_mapping_info should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 5. An empty blob whose uncompressed size is non-zero must fail to open with
        // IntegrityError.
        {
            let hash = fixture.write_blob(&[], CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(100).await.expect("truncate failed");
            let blob = fixture.get_blob(hash).await.expect("get_blob failed");
            let err =
                blob.get_mapping_info(hash).await.err().expect("get_mapping_info should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_lz4_blob() {
        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        // Only the first chunk can be compressed, so the rest are stored raw.
        let mut blob_data = vec![0xAA; READ_AHEAD_SIZE as usize * 2 + 5000];
        rand::fill(&mut blob_data[CHUNK_SIZE..]);
        let fxblob_builder = FxBlobBuilder::new(device).await.unwrap();
        let blob = fxblob_builder
            .generate_blob(blob_data.clone(), Some(CompressionAlgorithm::Lz4))
            .unwrap();
        let blob_hash = blob.hash();
        fxblob_builder.install_blob(&blob).await.unwrap();
        let device = fxblob_builder.finalize().await.unwrap().0;
        device.reopen(/*read_only=*/ false);
        let fixture = open_blob_fixture(device).await;

        assert_eq!(fixture.read_blob(blob_hash).await, blob_data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_blob_vmos_are_immutable() {
        let fixture = new_blob_fixture().await;

        let data = vec![0xffu8; 500];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let blob_vmo = fixture.get_blob_vmo(hash).await;

        // The VMO shouldn't be resizable.
        assert_matches!(blob_vmo.set_size(20), Err(_));

        // The VMO shouldn't be writable.
        assert_matches!(blob_vmo.write(b"overwrite", 0), Err(_));

        // The VMO's content size shouldn't be modifiable.
        assert_matches!(blob_vmo.set_stream_size(20), Err(_));

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_overwrite_open_blob() {
        let fixture = new_blob_fixture().await;

        {
            let data = vec![0xabu8; 65_536];
            let hash = fixture.write_blob(&data, CompressionMode::Always).await;

            let opened_blob = fixture.get_opened_blob(hash).await.expect("get_opened_blob failed");
            let old_object_id = opened_blob.object_id();
            let info = opened_blob.get_mapping_info(hash).await.expect("get_mapping_info failed");
            assert_eq!(info.uncompressed_size, data.len() as u64);
            assert!(info.stored_size > 0);
            assert!(!info.data.is_empty());
            assert!(!info.merkle.is_empty());

            // Overwriting the open blob must keep the old object alive until `opened_blob` is
            // dropped.
            let delivery_data = Type1Blob::generate(&data, CompressionMode::Never);
            let writer = fixture.create_blob(&hash.into(), true).await.expect("create_blob failed");
            let mut blob_writer = BlobWriter::create(writer, delivery_data.len() as u64)
                .await
                .expect("BlobWriter::create failed");
            blob_writer.write(&delivery_data).await.expect("blob_writer.write failed");

            fixture.fs().graveyard().flush().await;
            assert!(
                ObjectStore::open_object(
                    fixture.volume().volume(),
                    old_object_id,
                    HandleOptions::default(),
                    None,
                )
                .await
                .is_ok(),
                "Overwritten blob object should remain alive while OpenedNode is held"
            );

            drop(opened_blob);
            fixture.fs().graveyard().flush().await;
            assert_matches!(
                ObjectStore::open_object(
                    fixture.volume().volume(),
                    old_object_id,
                    HandleOptions::default(),
                    None,
                )
                .await
                .err()
                .expect("Overwritten blob object should be purged after OpenedNode is dropped")
                .downcast_ref::<FxfsError>(),
                Some(FxfsError::NotFound)
            );

            let overwritten_blob =
                fixture.get_opened_blob(hash).await.expect("get_opened_blob after overwrite");
            let info =
                overwritten_blob.get_mapping_info(hash).await.expect("get_mapping_info failed");
            assert_eq!(info.uncompressed_size, data.len() as u64);
            assert_eq!(info.stored_size, data.len() as u64);
            drop(overwritten_blob);
        }

        fixture.close().await;
    }
}
