// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::error::Error;
use derivative::Derivative;
use inspect_format::{
    Block, BlockAccessorExt, BlockAccessorMutExt, BlockContainer, BlockIndex, Container, ReadBytes,
    WriteBytes, constants,
};
use std::sync::Arc;

/// Size in bytes of the 32-byte `"BLOB"` header block at `BlockIndex::HEADER`.
pub(crate) const BLOB_HEADER_SIZE_BYTES: usize = 32;

/// Backing storage and header/payload accessors for a single `BlobProperty` external VMO.
///
/// The first `BLOB_HEADER_SIZE_BYTES` bytes of `container` hold the `"BLOB"`
/// `Header` block (`BlockIndex::HEADER`), and the next `payload_size_bytes` bytes
/// (`32..32 + payload_size_bytes`) hold the caller's raw payload bytes. The underlying
/// `container` allocation is padded up to the next full 4 KiB page boundary
/// (`constants::PAGE_SIZE_BYTES`), while the `"BLOB"` header's `vmo_size` and all
/// read/write/snapshot accessors strictly bound accesses to the specified
/// `payload_size_bytes`.
#[derive(Derivative)]
#[derivative(Debug)]
pub(crate) struct BlobBuffer {
    #[derivative(Debug = "ignore")]
    container: Container,
    #[allow(dead_code)] // unused in host.
    #[derivative(Debug = "ignore")]
    storage: Arc<<Container as BlockContainer>::ShareableData>,
    payload_size_bytes: usize,
}

impl BlobBuffer {
    /// Allocates a new `BlobBuffer` capable of holding `payload_size_bytes` bytes of payload
    /// after the 32-byte `"BLOB"` header.
    ///
    /// Adds `BLOB_HEADER_SIZE_BYTES` (32) to `payload_size_bytes` to compute the logical size
    /// (`32 + payload_size_bytes`) stored in the `"BLOB"` header's `vmo_size` field, and pads the
    /// underlying VMO/container allocation up to the next full page size
    /// (`constants::PAGE_SIZE_BYTES`, 4 KiB) if not already page-aligned.
    pub(crate) fn new(payload_size_bytes: usize) -> Result<Self, Error> {
        let logical_size = BLOB_HEADER_SIZE_BYTES
            .checked_add(payload_size_bytes)
            .ok_or(Error::BlockSizeTooBig(payload_size_bytes))?;

        // Even though `usize` is u64, we store `logical_size` in the "BLOB" header's u32 `vmo_size`
        // field anyway, so verify `logical_size` can be represented as a `u32`.
        let _ =
            u32::try_from(logical_size).map_err(|_| Error::BlockSizeTooBig(payload_size_bytes))?;

        // If necessary, we could add another check for maximum size of a BlobProperty VMO.
        // Currently we have decided against having a maximum size, only relying on the users
        // of this library's configuration capabilities.

        let allocated_size = logical_size
            .checked_next_multiple_of(constants::PAGE_SIZE_BYTES)
            .ok_or(Error::BlockSizeTooBig(payload_size_bytes))?;

        #[cfg(target_os = "fuchsia")]
        let (mut container, storage) = {
            let (container, vmo) =
                Container::read_and_write(allocated_size).map_err(Error::AllocateVmo)?;
            let name = zx::Name::new("InspectBlob").unwrap();
            vmo.set_name(&name).map_err(Error::AllocateVmo)?;
            (container, Arc::new(vmo))
        };

        #[cfg(not(target_os = "fuchsia"))]
        let (mut container, storage) = {
            let (container, storage) = Container::read_and_write(allocated_size).unwrap();
            (container, Arc::new(storage))
        };

        Block::free(
            &mut container,
            BlockIndex::HEADER,
            constants::HEADER_ORDER,
            BlockIndex::EMPTY,
        )?
        .become_reserved()
        .become_blob_header(logical_size)?;

        Ok(Self { container, storage, payload_size_bytes })
    }

    pub(crate) fn payload_size_bytes(&self) -> usize {
        self.payload_size_bytes
    }

    pub(crate) fn header(&self) -> Block<&Container, inspect_format::Header> {
        self.container.block_at_unchecked::<inspect_format::Header>(BlockIndex::HEADER)
    }

    pub(crate) fn header_mut(&mut self) -> Block<&mut Container, inspect_format::Header> {
        self.container.block_at_unchecked_mut::<inspect_format::Header>(BlockIndex::HEADER)
    }

    /// Returns the mutable payload slice (`32..32 + payload_size_bytes`), excluding both the
    /// 32-byte `"BLOB"` header and any trailing page-alignment padding bytes.
    pub(crate) fn payload_slice_mut(&mut self) -> &mut [u8] {
        self.container
            .get_slice_mut_at(BLOB_HEADER_SIZE_BYTES, self.payload_size_bytes)
            .expect("payload slice within container bounds")
    }

    /// Returns the logical VMO byte slice (`0..32 + payload_size_bytes`), including the 32-byte
    /// `"BLOB"` header and the requested `payload_size_bytes`, while excluding any trailing
    /// page-alignment padding bytes.
    pub(crate) fn vmo_bytes(&self) -> &[u8] {
        self.container
            .get_slice_at(0, BLOB_HEADER_SIZE_BYTES + self.payload_size_bytes)
            .expect("logical size within container bounds")
    }

    /// Returns a heap-allocated copy of [`Self::vmo_bytes`] (`0..32 + payload_size_bytes`), or
    /// `None` if the `"BLOB"` header is currently locked.
    pub(crate) fn copy_vmo_bytes(&self) -> Option<Box<[u8]>> {
        if self.header().is_locked() {
            return None;
        }
        Some(Box::from(self.vmo_bytes()))
    }

    /// Produces a read-only Copy-on-Write (`ZX_VMO_CHILD_SNAPSHOT | ZX_VMO_CHILD_NO_WRITE`)
    /// child VMO with `VMO_FROZEN` set in its `"BLOB"` header, thawing the parent header back
    /// before returning.
    ///
    /// Returns [`Error::ExpectedLockState`] if the `"BLOB"` header is currently locked.
    #[cfg(target_os = "fuchsia")]
    pub(crate) fn frozen_vmo_copy(&mut self) -> Result<zx::Vmo, Error> {
        if self.header().is_locked() {
            return Err(Error::ExpectedLockState(false));
        }

        let size = self.storage.get_size().map_err(Error::GetVmoSize)?;
        let old = self.header_mut().freeze();
        let child = self
            .storage
            .create_child(zx::VmoChildOptions::SNAPSHOT | zx::VmoChildOptions::NO_WRITE, 0, size)
            .map_err(Error::CreateChildVmo);
        self.header_mut().thaw(old);
        child
    }

    /// Returns a duplicated handle to the backing VMO with read-only rights
    /// (`zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP | zx::Rights::GET_PROPERTY`,
    /// omitting `zx::Rights::WRITE` so readers cannot mutate the producer's VMO).
    #[cfg(target_os = "fuchsia")]
    pub(crate) fn duplicate_vmo(&self) -> Option<zx::Vmo> {
        self.storage
            .duplicate_handle(
                zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP | zx::Rights::GET_PROPERTY,
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "fuchsia")]
    use crate::reader::ReaderError;
    #[cfg(target_os = "fuchsia")]
    use crate::reader::snapshot::Snapshot;
    #[cfg(target_os = "fuchsia")]
    use fuchsia_sync::Mutex;

    #[fuchsia::test]
    fn blob_buffer_new_overflow() {
        assert!(matches!(BlobBuffer::new(usize::MAX), Err(Error::BlockSizeTooBig(usize::MAX))));
        let exceeds_u32 = u32::MAX as usize;
        assert!(
            matches!(BlobBuffer::new(exceeds_u32), Err(Error::BlockSizeTooBig(v)) if v == exceeds_u32)
        );
    }

    #[fuchsia::test]
    fn blob_buffer_sizes_as_expected() {
        let buffer = BlobBuffer::new(24).unwrap();
        assert_eq!(buffer.payload_size_bytes(), 24);
        assert_eq!(buffer.vmo_bytes().len(), BLOB_HEADER_SIZE_BYTES + 24);
        assert_eq!(
            buffer.header().vmo_size().unwrap().unwrap() as usize,
            BLOB_HEADER_SIZE_BYTES + 24
        );
        assert_eq!(buffer.container.len(), constants::PAGE_SIZE_BYTES);
    }

    #[fuchsia::test]
    fn blob_buffer_payload_slice_mut_and_vmo_bytes() {
        let mut buffer = BlobBuffer::new(24).unwrap();
        buffer.header_mut().lock();
        buffer.payload_slice_mut().copy_from_slice(&[0x5A; 24]);
        buffer.header_mut().unlock();

        let bytes = buffer.vmo_bytes();
        let header_slice: [u8; BLOB_HEADER_SIZE_BYTES] =
            bytes[..BLOB_HEADER_SIZE_BYTES].try_into().unwrap();
        let header = Block::parse_blob_header(&header_slice).expect("valid BLOB header");
        assert_eq!(header.magic_number(), constants::BLOB_HEADER_MAGIC_NUMBER);
        assert_eq!(header.generation_count(), 2);
        assert_eq!(&bytes[BLOB_HEADER_SIZE_BYTES..], &[0x5A; 24]);
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    fn blob_buffer_duplicate_vmo() {
        let mut buffer = BlobBuffer::new(32).unwrap();
        buffer.payload_slice_mut().copy_from_slice(&[0x11; 32]);

        let dup_vmo = buffer.duplicate_vmo().unwrap();
        assert_eq!(dup_vmo.get_name().unwrap().as_bstr(), b"InspectBlob");
        // duplicate_vmo() duplicates the handle without zx::Rights::WRITE so external readers
        // (such as Archivist) can read/map the VMO but cannot mutate the producer's buffer.
        assert_eq!(dup_vmo.write(&[0xFF], 0), Err(zx::Status::ACCESS_DENIED));

        // Verify the initial payload written before duplicating is readable from dup_vmo.
        let mut payload = [0u8; 32];
        dup_vmo.read(&mut payload, BLOB_HEADER_SIZE_BYTES as u64).unwrap();
        assert_eq!(payload, [0x11; 32]);

        // Header lock/unlock state changes on the parent BlobBuffer are visible on dup_vmo.
        let mut header_bytes = [0u8; BLOB_HEADER_SIZE_BYTES];
        buffer.header_mut().lock();
        dup_vmo.read(&mut header_bytes, 0).unwrap();
        assert!(Block::parse_blob_header(&header_bytes).unwrap().is_locked());

        buffer.header_mut().unlock();
        dup_vmo.read(&mut header_bytes, 0).unwrap();
        assert!(!Block::parse_blob_header(&header_bytes).unwrap().is_locked());

        // Modify the parent buffer in-place and verify dup_vmo reads back the updated [0x22; 32]
        // bytes we just wrote above.
        buffer.payload_slice_mut().copy_from_slice(&[0x22; 32]);
        dup_vmo.read(&mut payload, BLOB_HEADER_SIZE_BYTES as u64).unwrap();
        assert_eq!(payload, [0x22; 32]);
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    fn blob_buffer_frozen_vmo_copy() {
        let mut buffer = BlobBuffer::new(32).unwrap();
        buffer.header_mut().lock();
        buffer.payload_slice_mut().copy_from_slice(&[0x11; 32]);
        buffer.header_mut().unlock();

        // frozen_vmo_copy sets VMO_FROZEN in the child header and thaws the parent back to 2.
        let frozen_vmo = buffer.frozen_vmo_copy().unwrap();
        let mut header_bytes = [0u8; BLOB_HEADER_SIZE_BYTES];
        frozen_vmo.read(&mut header_bytes, 0).unwrap();
        let frozen_header = Block::parse_blob_header(&header_bytes).unwrap();
        assert!(frozen_header.is_frozen());
        assert!(!buffer.header().is_frozen());
        assert_eq!(buffer.header().generation_count(), 2);

        // While locked, frozen_vmo_copy returns ExpectedLockState(false) and leaves header locked.
        buffer.header_mut().lock();
        buffer.payload_slice_mut().copy_from_slice(&[0x22; 32]);
        assert!(matches!(buffer.frozen_vmo_copy(), Err(Error::ExpectedLockState(false))));
        assert!(buffer.header().is_locked());
        buffer.header_mut().unlock();

        // The frozen CoW child is isolated from subsequent writes to the parent.
        // Check that the payload has not been modified from the original pre-copy value.
        let mut payload = [0u8; 32];
        frozen_vmo.read(&mut payload, BLOB_HEADER_SIZE_BYTES as u64).unwrap();
        assert_eq!(payload, [0x11; 32]);
    }

    #[fuchsia::test]
    fn blob_buffer_copy_vmo_bytes() {
        let mut buffer = BlobBuffer::new(32).unwrap();
        buffer.header_mut().lock();
        buffer.payload_slice_mut().copy_from_slice(&[0x11; 32]);
        // While the header is locked, copy_vmo_bytes returns None to avoid copying a mid-write buffer.
        assert!(buffer.copy_vmo_bytes().is_none());
        buffer.header_mut().unlock();

        // copy_vmo_bytes copies the current header + payload bytes into a heap slice without
        // freezing the header, so the copied header retains generation_count == 2.
        let copied_bytes = buffer.copy_vmo_bytes().unwrap();
        let header_slice: [u8; BLOB_HEADER_SIZE_BYTES] =
            copied_bytes[..BLOB_HEADER_SIZE_BYTES].try_into().unwrap();
        let copied_header = Block::parse_blob_header(&header_slice).unwrap();
        assert!(!copied_header.is_frozen());
        assert_eq!(copied_header.generation_count(), 2);

        // Subsequent writes to the parent buffer do not affect the copied bytes.
        buffer.payload_slice_mut().copy_from_slice(&[0x22; 32]);
        assert_eq!(&copied_bytes[BLOB_HEADER_SIZE_BYTES..], &[0x11; 32]);
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    fn blob_buffer_concurrent_writer_and_readers() {
        use std::sync::Barrier;

        const PAYLOAD_LEN: usize = 64;
        const ITERS: usize = 100;

        let buffer = Arc::new(Mutex::new(BlobBuffer::new(PAYLOAD_LEN).unwrap()));
        let live_vmo = buffer.lock().duplicate_vmo().unwrap();

        // Synchronize the writer thread and reader thread across 4 points in every iteration:
        // 1. `before_write_barrier`: reader finishes its pre-write read before the writer begins.
        // 2. `torn_write_barrier`: writer has locked the header and written the first half of the
        //    payload, releasing the reader to verify that a lock-free `try_once_from_blob_vmo`
        //    attempt is rejected by the odd seqlock generation counter.
        // 3. `resume_write_barrier`: reader releases the writer to complete the second half and
        //    unlock while the reader blocks on the mutex to observe the completed write.
        // 4. `after_write_barrier`: writer has unlocked and reader has completed its torn-write
        //    read, so the reader can perform its post-write read at the end of the iteration.
        let before_write_barrier = Arc::new(Barrier::new(2));
        let torn_write_barrier = Arc::new(Barrier::new(2));
        let resume_write_barrier = Arc::new(Barrier::new(2));
        let after_write_barrier = Arc::new(Barrier::new(2));

        let writer_buffer = Arc::clone(&buffer);
        let writer_before_write = Arc::clone(&before_write_barrier);
        let writer_torn_write = Arc::clone(&torn_write_barrier);
        let writer_resume_write = Arc::clone(&resume_write_barrier);
        let writer_after_write = Arc::clone(&after_write_barrier);
        let writer_thread = std::thread::spawn(move || {
            for i in 1..=ITERS {
                let byte = (i & 0xFF) as u8;

                // 1. Wait for the reader to complete its pre-write read before mutating.
                writer_before_write.wait();

                {
                    let mut guard = writer_buffer.lock();
                    guard.header_mut().lock();
                    guard.payload_slice_mut()[..PAYLOAD_LEN / 2].fill(byte);

                    // 2. Signal that the buffer is now in a torn state (first half written,
                    //    header seqlock held).
                    writer_torn_write.wait();
                    // 3. Wait for the reader to verify that a lock-free snapshot attempt is
                    //    rejected while torn, then finish the second half and unlock.
                    writer_resume_write.wait();

                    guard.payload_slice_mut()[PAYLOAD_LEN / 2..].fill(byte);
                    guard.header_mut().unlock();
                }

                // 4. Signal that the write transaction is complete so the reader can do its
                //    post-write read at the end of the iteration.
                writer_after_write.wait();
            }
        });

        let assert_reads_equal =
            |snapshot_buffer: &Mutex<BlobBuffer>, vmo: &zx::Vmo, expected: u8| {
                // Take mutex-protected CoW (`frozen_vmo_copy`) and deep-copy
                // (`copy_vmo_bytes`) snapshots. During Read 2b, `.lock()` blocks on the mutex
                // until the writer finishes writing the second half and unlocks.
                let (frozen_vmo, copied_bytes) = {
                    let mut guard = snapshot_buffer.lock();
                    (guard.frozen_vmo_copy().unwrap(), guard.copy_vmo_bytes().unwrap())
                };
                let mut payload = [0u8; PAYLOAD_LEN];
                frozen_vmo.read(&mut payload, BLOB_HEADER_SIZE_BYTES as u64).unwrap();
                assert_eq!(payload, [expected; PAYLOAD_LEN]);

                let copied_payload = &copied_bytes[BLOB_HEADER_SIZE_BYTES..];
                assert_eq!(copied_payload, &[expected; PAYLOAD_LEN]);

                // Snapshot the live duplicate VMO once the write transaction is unlocked.
                assert!(Snapshot::try_from_blob_vmo(vmo).is_ok());
            };

        let snapshot_buffer = Arc::clone(&buffer);
        let reader_before_write = Arc::clone(&before_write_barrier);
        let reader_torn_write = Arc::clone(&torn_write_barrier);
        let reader_resume_write = Arc::clone(&resume_write_barrier);
        let reader_after_write = Arc::clone(&after_write_barrier);
        let reader_thread = std::thread::spawn(move || {
            for i in 1..=ITERS {
                let prev_byte = ((i - 1) & 0xFF) as u8;
                let new_byte = (i & 0xFF) as u8;

                // Read 1: before the first write.
                assert_reads_equal(&snapshot_buffer, &live_vmo, prev_byte);
                reader_before_write.wait();

                // Read 2a: while the buffer is guaranteed to be in a torn state, a lock-free
                // snapshot attempt must be rejected by the seqlock.
                reader_torn_write.wait();
                assert!(matches!(
                    Snapshot::try_once_from_blob_vmo(&live_vmo),
                    Err(ReaderError::InconsistentSnapshot)
                ));

                // Read 2b: release the writer to finish the second half while we block on the
                // mutex in `assert_reads_equal`, verifying we observe the completed `new_byte`.
                reader_resume_write.wait();
                assert_reads_equal(&snapshot_buffer, &live_vmo, new_byte);

                // Read 3: at the end of the iteration after the write has completed.
                reader_after_write.wait();
                assert_reads_equal(&snapshot_buffer, &live_vmo, new_byte);
            }
        });

        writer_thread.join().unwrap();
        reader_thread.join().unwrap();
    }
}
