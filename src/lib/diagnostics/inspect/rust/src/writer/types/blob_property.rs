// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::blob_buffer::BlobBuffer;
use crate::writer::{BlobReaderWriter, Error, Inner, InnerData, InnerType, InspectType, State};
use fuchsia_sync::{Mutex, MutexGuard};
use inspect_format::BlockIndex;
use std::sync::Arc;

/// RAII guard used by [`BlobProperty::atomic_write`] to hold the [`BlobBuffer`] mutex and its
/// 32-byte `"BLOB"` header seqlock across a write transaction.
///
/// Because [`fuchsia_sync::Mutex`] does not poison on panic, an unwinding `update_fn` (e.g. in
/// host tests or `catch_unwind`) would release the mutex without running cleanup code that
/// follows the closure call. Unlocking the `"BLOB"` header in [`Drop`] guarantees the header's
/// generation counter is always restored to an even (unlocked) value before the mutex is
/// released, preventing the blob VMO from being left permanently locked.
struct LockedBlobGuard<'a>(MutexGuard<'a, BlobBuffer>);

impl<'a> LockedBlobGuard<'a> {
    fn new(blob: &'a Mutex<BlobBuffer>) -> Self {
        let mut guard = blob.lock();
        guard.header_mut().lock();
        Self(guard)
    }

    fn reader_writer(&mut self) -> BlobReaderWriter<'_> {
        BlobReaderWriter::new(self.0.payload_slice_mut())
    }
}

impl Drop for LockedBlobGuard<'_> {
    fn drop(&mut self) {
        self.0.header_mut().unlock();
    }
}

/// Inspect Blob Property data type, backed by a dedicated external VMO with a 32-byte
/// `"BLOB"` seqlock header at bytes `0..32` followed by `payload_size` bytes of contiguous
/// payload (`32..32 + payload_size`).
///
/// Even when the underlying VMO is padded up to the next 4 KiB page boundary, all reads,
/// writes, and snapshots strictly enforce the requested `payload_size`.
///
/// NOTE: do not rely on PartialEq implementation for true comparison.
/// Instead leverage the reader.
///
/// NOTE: Operations on a Default value are no-ops.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct BlobProperty {
    inner: Inner<InnerBlobPropertyType>,
}

impl InspectType for BlobProperty {
    fn into_recorded(self) -> crate::writer::types::RecordedInspectType {
        crate::writer::types::RecordedInspectType::BlobProperty(self)
    }
}

crate::impl_inspect_type_internal!(BlobProperty, custom_data);

impl BlobProperty {
    pub(crate) fn new_with_buffer(
        state: State,
        block_index: BlockIndex,
        buffer: Arc<Mutex<BlobBuffer>>,
    ) -> Self {
        Self { inner: Inner::new_with_data(state, block_index, BlobPropertyData::new(buffer)) }
    }

    /// Returns the requested payload capacity in bytes (excluding the 32-byte `"BLOB"` header
    /// and any trailing page-alignment padding), or `0` if this `BlobProperty` is a no-op.
    pub fn payload_capacity(&self) -> usize {
        self.inner.inner_ref().map(|inner_ref| inner_ref.data.payload_size_bytes()).unwrap_or(0)
    }

    /// Executes `update_fn` within a single seqlock write transaction on the backing Blob VMO.
    ///
    /// Returns [`Error::NoOp`] without invoking `update_fn` if this `BlobProperty` is a no-op
    /// (for example, created from a no-op [`Inspector`][crate::Inspector], [`BlobProperty::default()`],
    /// or a failed allocation).
    ///
    /// Before `update_fn` runs, the 32-byte `"BLOB"` header's generation counter is locked
    /// (incremented to an odd number), and unlocked (incremented to an even number) once
    /// `update_fn` returns.
    ///
    /// The [`BlobReaderWriter`] passed to `update_fn` exposes only the payload section of the VMO,
    /// so offset `0` refers to the first byte immediately after the 32-byte `"BLOB"` header.
    pub fn atomic_write<F, R>(&self, update_fn: F) -> Result<R, Error>
    where
        F: FnOnce(&mut BlobReaderWriter<'_>) -> R,
    {
        let inner_ref = self.inner.inner_ref().ok_or(Error::NoOp("BlobProperty"))?;
        let mut locked = LockedBlobGuard::new(inner_ref.data.buffer());
        Ok(update_fn(&mut locked.reader_writer()))
    }

    /// Writes `bytes` into the payload section starting at `offset` (where `0` is the first
    /// byte after the 32-byte `"BLOB"` header) within a seqlock write transaction.
    ///
    /// Returns [`Error::NoOp`] if this `BlobProperty` is a no-op, or [`Error::Bounds`] if
    /// `offset + bytes.len()` exceeds [`payload_capacity`][Self::payload_capacity].
    pub fn write_at(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        // The outer `?` unwraps `Result<_, Error>` from `atomic_write` (e.g. `Error::NoOp`),
        // and the inner `?` converts `Result<(), BoundsError>` from `writer.write_at` into
        // `Error::Bounds` via `From`.
        Ok(self.atomic_write(|writer| writer.write_at(offset, bytes))??)
    }
}

#[derive(Default, Debug)]
pub(crate) struct InnerBlobPropertyType;

#[derive(Debug)]
pub(crate) struct BlobPropertyData {
    buffer: Arc<Mutex<BlobBuffer>>,
    payload_size_bytes: usize,
}

impl BlobPropertyData {
    fn new(buffer: Arc<Mutex<BlobBuffer>>) -> Self {
        let payload_size_bytes = buffer.lock().payload_size_bytes();
        Self { buffer, payload_size_bytes }
    }

    fn buffer(&self) -> &Arc<Mutex<BlobBuffer>> {
        &self.buffer
    }

    fn payload_size_bytes(&self) -> usize {
        self.payload_size_bytes
    }
}

impl InnerData for BlobPropertyData {
    fn is_valid(&self) -> bool {
        true
    }
}

impl InnerType for InnerBlobPropertyType {
    type Data = BlobPropertyData;

    fn free(state: &State, _: &Self::Data, block_index: BlockIndex) -> Result<(), Error> {
        let mut state_lock = state.try_lock()?;
        state_lock
            .free_blob_property(block_index)
            .map_err(|err| Error::free("blob property", block_index, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::BoundsError;
    use crate::writer::blob_buffer::BLOB_HEADER_SIZE_BYTES;
    use crate::writer::testing_utils::GetBlockExt;
    use crate::writer::types::{Inspector, InspectorConfig};
    use inspect_format::{Block, BlockType, Link, LinkNodeDisposition, constants};

    #[cfg(target_os = "fuchsia")]
    use crate::reader::snapshot::Snapshot;
    #[cfg(target_os = "fuchsia")]
    use inspect_format::Header;

    #[fuchsia::test]
    fn blob_property_creates_link_and_blob_header() {
        let inspector = Inspector::default();
        let node = inspector.root().create_child("node");
        {
            let blob = node.create_blob_property("events", 128);
            assert_eq!(blob.payload_capacity(), 128);

            // Verify LINK_VALUE block in parent VMO has LinkNodeDisposition::Blob.
            blob.get_block::<_, Link>(|link_block| {
                assert_eq!(link_block.block_type(), Some(BlockType::LinkValue));
                assert_eq!(link_block.link_node_disposition(), Some(LinkNodeDisposition::Blob));
            });
            node.get_block::<_, inspect_format::Node>(|node_block| {
                assert_eq!(node_block.child_count(), 1);
            });

            // Verify InnerState stores the blob in `blobs` and NOT in `callbacks`.
            let state = inspector.state().unwrap();
            let guard = state.try_lock().unwrap();
            assert!(guard.callbacks().is_empty());
            assert_eq!(guard.blobs().len(), 1);
            assert!(guard.blobs().contains_key("events-0"));
        }

        // Dropping BlobProperty frees the link block and removes the entry from `blobs`.
        node.get_block::<_, inspect_format::Node>(|node_block| {
            assert_eq!(node_block.child_count(), 0);
        });
        let state = inspector.state().unwrap();
        let guard = state.try_lock().unwrap();
        assert!(guard.blobs().is_empty());
    }

    #[fuchsia::test]
    fn blob_property_arbitrary_payload_sizes_and_page_padding() {
        let inspector = Inspector::default();
        for (i, payload_size) in [0, 1, 64, 4064, 4096, 5000].into_iter().enumerate() {
            let name = format!("blob_{payload_size}");
            let link_key = format!("{name}-{i}");
            let blob = inspector.root().create_blob_property(&name, payload_size);
            assert_eq!(blob.payload_capacity(), payload_size);

            let raw_bytes = inspector.copy_blob_vmo_data(&link_key).unwrap();
            assert_eq!(raw_bytes.len(), BLOB_HEADER_SIZE_BYTES + payload_size);

            let header_slice: [u8; BLOB_HEADER_SIZE_BYTES] =
                raw_bytes[..BLOB_HEADER_SIZE_BYTES].try_into().unwrap();
            let header = Block::parse_blob_header(&header_slice).expect("valid BLOB header");
            assert_eq!(
                header.vmo_size().unwrap().unwrap() as usize,
                BLOB_HEADER_SIZE_BYTES + payload_size
            );
        }
    }

    #[fuchsia::test]
    fn no_op_blob_property() {
        let inspector = Inspector::new(InspectorConfig::default().no_op());
        let blob = inspector.root().create_blob_property("noop", 128);
        assert_eq!(blob.payload_capacity(), 0);
        assert_eq!(
            blob.atomic_write(|_| panic!("update_fn must not be invoked on a no-op BlobProperty")),
            Err(Error::NoOp("BlobProperty"))
        );
        assert_eq!(blob.write_at(0, &[1, 2, 3]), Err(Error::NoOp("BlobProperty")));
        assert_eq!(blob.write_at(0, &[]), Err(Error::NoOp("BlobProperty")));
    }

    #[fuchsia::test]
    fn blob_property_write_at_offset_zero_is_after_blob_header() {
        let inspector = Inspector::default();
        let payload_size = 64;
        let blob = inspector.root().create_blob_property("events", payload_size);

        // Write at offset 0 of BlobProperty: this must land at byte 32 (immediately after the
        // 32-byte "BLOB" header) in the underlying VMO, leaving bytes 0..32 intact.
        blob.write_at(0, b"hello blob").unwrap();

        let raw_bytes = inspector.copy_blob_vmo_data("events-0").unwrap();
        assert_eq!(raw_bytes.len(), BLOB_HEADER_SIZE_BYTES + payload_size);

        // Bytes 0..32 must still be a valid "BLOB" header (and rejected as an "INSP" header).
        let header_slice: [u8; BLOB_HEADER_SIZE_BYTES] =
            raw_bytes[..BLOB_HEADER_SIZE_BYTES].try_into().unwrap();
        let header = Block::parse_blob_header(&header_slice).expect("valid BLOB header");
        assert!(Block::parse_inspect_header(&header_slice).is_none());
        assert_eq!(header.magic_number(), constants::BLOB_HEADER_MAGIC_NUMBER);
        assert_eq!(header.version(), constants::HEADER_VERSION_NUMBER);
        assert_eq!(header.generation_count(), 2);
        assert_eq!(
            header.vmo_size().unwrap().unwrap() as usize,
            BLOB_HEADER_SIZE_BYTES + payload_size
        );

        // Payload bytes start at BLOB_HEADER_SIZE_BYTES (32).
        assert_eq!(&raw_bytes[BLOB_HEADER_SIZE_BYTES..BLOB_HEADER_SIZE_BYTES + 10], b"hello blob");
        assert!(raw_bytes[BLOB_HEADER_SIZE_BYTES + 10..].iter().all(|&b| b == 0));
    }

    #[fuchsia::test]
    fn blob_property_atomic_write_updates_generation_and_checks_bounds() {
        let inspector = Inspector::default();
        let blob = inspector.root().create_blob_property("ring", 32);

        let state = inspector.state().unwrap();
        let blob_arc = state.try_lock().unwrap().blobs().get("ring-0").unwrap().clone();

        assert_eq!(blob_arc.lock().header().generation_count(), 0);

        blob.atomic_write(|rw| {
            assert_eq!(rw.payload_capacity(), 32);
            // Non-contiguous writes within a single seqlock transaction.
            rw.write_at(0, &[1, 2, 3, 4]).unwrap();
            rw.write_at(28, &[5, 6, 7, 8]).unwrap();

            let mut head = [0u8; 4];
            rw.read_at(0, &mut head).unwrap();
            assert_eq!(head, [1, 2, 3, 4]);
            assert_eq!(rw.slice_at(28, 4).unwrap(), &[5, 6, 7, 8]);

            // Out-of-bounds accesses beyond payload_size (32) are rejected even though the
            // underlying VMO is padded to 4096 bytes.
            assert_eq!(
                rw.write_at(29, &[1, 2, 3, 4]),
                Err(BoundsError::OutOfBounds { offset: 29, len: 4, payload_capacity: 32 })
            );
            assert_eq!(
                rw.write_at(usize::MAX, &[1]),
                Err(BoundsError::OutOfBounds { offset: usize::MAX, len: 1, payload_capacity: 32 })
            );
            assert_eq!(
                rw.read_at(30, &mut head),
                Err(BoundsError::OutOfBounds { offset: 30, len: 4, payload_capacity: 32 })
            );

            // Querying payload_capacity on the property itself is lock-free and does not
            // self-deadlock inside atomic_write.
            assert_eq!(blob.payload_capacity(), 32);

            // Concurrent copy_blob_vmo_data from another thread returns None via try_lock()
            // instead of blocking while atomic_write holds the blob lock.
            // ASAN builders panic on this if from the same thread.
            std::thread::scope(|s| {
                s.spawn(|| {
                    assert!(inspector.copy_blob_vmo_data("ring-0").is_none());
                });
            });
        })
        .unwrap();

        // One atomic_write transaction increments generation_count by 2 (lock + unlock).
        assert_eq!(blob_arc.lock().header().generation_count(), 2);

        // Out-of-bounds write_at on BlobProperty surfaces Error::Bounds(BoundsError::OutOfBounds).
        assert_eq!(
            blob.write_at(29, &[1, 2, 3, 4]),
            Err(Error::Bounds(BoundsError::OutOfBounds {
                offset: 29,
                len: 4,
                payload_capacity: 32
            }))
        );

        let raw_bytes = inspector.copy_blob_vmo_data("ring-0").unwrap();
        let payload = &raw_bytes[BLOB_HEADER_SIZE_BYTES..];
        assert_eq!(&payload[0..4], &[1, 2, 3, 4]);
        assert_eq!(&payload[4..28], &[0u8; 24]);
        assert_eq!(&payload[28..32], &[5, 6, 7, 8]);
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    fn blob_property_frozen_and_copied_vmo_snapshots() {
        let inspector = Inspector::default();
        let blob = inspector.root().create_blob_property("events", 16);
        blob.write_at(0, &[0xAA; 16]).unwrap();

        // 1. Frozen CoW child VMO has VMO_FROZEN in its header while parent is thawed back.
        let frozen_vmo = inspector.frozen_blob_vmo_copy("events-0").unwrap();
        let mut header_bytes = [0u8; BLOB_HEADER_SIZE_BYTES];
        frozen_vmo.read(&mut header_bytes, 0).unwrap();
        let frozen_header = Block::parse_blob_header(&header_bytes).unwrap();
        assert!(frozen_header.is_frozen());

        let live_vmo = inspector.duplicate_blob_vmo("events-0").unwrap();
        live_vmo.read(&mut header_bytes, 0).unwrap();
        let live_header = Block::parse_blob_header(&header_bytes).unwrap();
        assert!(!live_header.is_frozen());
        assert_eq!(live_header.generation_count(), 2);

        let snapshot = Snapshot::try_from_blob_vmo(&frozen_vmo).unwrap();
        let header = Block::<_, Header>::parse_blob_header(&header_bytes).unwrap();
        let vmo_size = header.vmo_size().unwrap().unwrap() as usize;
        assert_eq!(vmo_size, BLOB_HEADER_SIZE_BYTES + 16);
        let _ = snapshot;

        // 2. Deep-copied VMO is not frozen and snapshots via the seqlock path.
        let copied_vmo = inspector.copy_blob_vmo("events-0").unwrap();
        copied_vmo.read(&mut header_bytes, 0).unwrap();
        let copied_header = Block::parse_blob_header(&header_bytes).unwrap();
        assert!(!copied_header.is_frozen());
        assert!(Snapshot::try_from_blob_vmo(&copied_vmo).is_ok());

        // 3. Attempting to duplicate, freeze, or copy from another thread while a write
        // transaction is active fails gracefully via try_lock() without blocking.
        blob.atomic_write(|_| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    assert!(inspector.duplicate_blob_vmo("events-0").is_none());
                    assert!(matches!(
                        inspector.frozen_blob_vmo_copy("events-0"),
                        Err(Error::ExpectedLockState(false))
                    ));
                    assert!(inspector.copy_blob_vmo("events-0").is_none());
                });
            });
        })
        .unwrap();

        let state = inspector.state().unwrap();
        let blob_arc = state.try_lock().unwrap().blobs().get("events-0").unwrap().clone();
        {
            let mut guard = blob_arc.lock();
            guard.header_mut().lock();
            assert!(matches!(guard.frozen_vmo_copy(), Err(Error::ExpectedLockState(false))));
            guard.header_mut().unlock();
        }
    }
}
