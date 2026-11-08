// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use mapping::{
    DeliveryCommand, DeliveryHandler, PENDING_DELIVERY_COMMANDS_CAPACITY, PagePrecommit,
    PageRequest, RawDeliveryCommand,
};
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;
use vmo_fifo::{OwnedPayloadBuffer, SyncSender};

// Leave 512 KiB of payload headroom on full-sized delivery VMOs (>= 1 MiB payload capacity, i.e.
// the 2 MiB production delivery VMO) so metadata registrations (`register_blob`) running on block
// completion threads never block behind in-flight page-in reservations.
const REGISTER_BLOB_HEADROOM_BYTES: usize = 512 * 1024;
const MIN_CAPACITY_FOR_HEADROOM_BYTES: usize = 1024 * 1024;
const VERIFICATION_FLUSH_THRESHOLD: usize = 96 * 1024;

/// Manages data verification and page supply via the delivery queue VMO.
#[derive(Debug)]
pub struct Verifier {
    sender: Arc<SyncSender<RawDeliveryCommand>>,
    headroom: usize,
}

impl Verifier {
    pub fn new(delivery_queue: zx::Vmo) -> Result<Self, zx::Status> {
        // Payloads in the delivery queue are transferred to the kernel pager via
        // `zx_pager_supply_pages`, which requires the source offset to be page-aligned. Set
        // the alignment of payload offsets to be page aligned.
        let sender = Arc::new(SyncSender::<RawDeliveryCommand>::new(
            delivery_queue,
            zx::system_get_page_size() as usize,
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )?);
        let headroom = if sender.payload_capacity() >= MIN_CAPACITY_FOR_HEADROOM_BYTES {
            REGISTER_BLOB_HEADROOM_BYTES
        } else {
            0
        };
        Ok(Self { sender, headroom })
    }

    fn reserve_payload(
        &self,
        len: usize,
    ) -> Result<OwnedPayloadBuffer<RawDeliveryCommand>, ChunkedArchiveError> {
        self.sender.reserve_owned_payload(len, self.headroom).map_err(|error| {
            log::error!(error:?; "Verifier::reserve_payload failed");
            ChunkedArchiveError::IntegrityError
        })
    }
}

impl DeliveryHandler for Verifier {
    type Request = Buffer;

    /// Returns a [`PageRequest`] implementation for delivering page-in data.
    fn get_page_request(self: &Arc<Self>, key: u64, original_range: Range<u64>) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key,
            read_range: original_range,
            total_len: 0,
            read_alignment: 0,
            flush_threshold: usize::MAX,
            committed_len: 0,
            delivered_len: 0,
            payload: None,
            child_target: None,
        }
    }

    /// Returns a [`Buffer`] configured to deliver parent blob chunks for a child file extent via
    /// [`DeliveryCommand::ChildData`].
    fn get_child_page_request(
        self: &Arc<Self>,
        child_key: u64,
        child_target_offset: u64,
        parent_byte_range: Range<u64>,
    ) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key: child_key,
            read_range: parent_byte_range.clone(),
            total_len: 0,
            read_alignment: 0,
            flush_threshold: usize::MAX,
            committed_len: 0,
            delivered_len: 0,
            payload: None,
            child_target: Some(ChildTarget { child_target_offset, parent_byte_range }),
        }
    }

    /// Pushes a header-only [`DeliveryCommand::ChildInlineTail`] command onto the delivery queue
    /// so the verifier can supply the child tail page from the verified parent VMO.
    fn deliver_child_inline_tail(
        self: &Arc<Self>,
        parent_byte_offset: u64,
        child_key: u64,
        child_target_offset: u64,
        tail_length: u32,
    ) {
        let cmd = DeliveryCommand::ChildInlineTail {
            parent_byte_offset,
            child_key,
            child_target_offset,
            tail_length,
        };
        let _ = self.sender.push(cmd.into());
    }

    /// Pushes a [`DeliveryCommand::ChildData`] command with `length == 0` onto the delivery queue
    /// to supply a zero-filled sparse range to `child_key`.
    fn deliver_child_sparse(
        &self,
        child_key: u64,
        child_target_offset: u64,
        child_length: u64,
    ) -> Result<(), anyhow::Error> {
        let cmd = DeliveryCommand::ChildData {
            parent_offset: 0,
            offset: 0,
            length: 0,
            child_key,
            child_target_offset,
            in_chunk_offset: 0,
            child_length: child_length as u32,
        };
        self.sender.push(cmd.into())?;
        Ok(())
    }

    /// Registers a blob's Merkle tree leaf hashes with the verifier via the delivery queue.
    fn register_blob(
        &self,
        key: u64,
        merkle_leaves: &[[u8; 32]],
        read_alignment: usize,
    ) -> Result<(), anyhow::Error> {
        let leaf_bytes: &[u8] = merkle_leaves.as_flattened();
        let mut payload = self.sender.reserve_owned_payload(leaf_bytes.len(), 0)?;
        let payload_offset = payload.offset();
        payload.data_mut().subslice_mut(0..leaf_bytes.len()).copy_from_slice(leaf_bytes);
        let cmd = DeliveryCommand::RegisterBlob {
            key,
            offset: payload_offset,
            length: leaf_bytes.len() as u32,
            read_alignment: read_alignment as u32,
        };
        payload.commit(cmd.into())?;
        Ok(())
    }
}

/// Target metadata for a [`Buffer`] servicing a page request on a child file mapped over a
/// parent blob.
#[derive(Debug)]
struct ChildTarget {
    /// Starting page-aligned logical byte offset in the child VMO for this request.
    child_target_offset: u64,
    /// Byte range in the parent blob corresponding to this child extent slice.
    parent_byte_range: Range<u64>,
}

/// Implementation of [`PageRequest`] for verifier deliveries that writes directly into reserved
/// `delivery_vmo` pages and forwards chunks via the delivery queue without bounce-buffering.
#[derive(Debug)]
pub struct Buffer {
    verifier: Arc<Verifier>,
    key: u64,
    read_range: Range<u64>,
    total_len: usize,
    // Alignment boundary (matching the blob's `read_alignment`) at which data is flushed to the
    // verifier queue.
    read_alignment: usize,
    // Precomputed byte threshold at which intermediate `read_alignment` chunks are flushed to the
    // verifier queue (`usize::MAX` if `read_alignment` is not a multiple of `page_size`).
    flush_threshold: usize,
    // Bytes committed into `self.payload` so far via calls to [`DataBuffer::commit`].
    committed_len: usize,
    // Bytes delivered to the verifier queue so far.
    delivered_len: usize,
    payload: Option<OwnedPayloadBuffer<RawDeliveryCommand>>,
    child_target: Option<ChildTarget>,
}

impl Buffer {
    fn make_delivery_command(
        child_target: Option<&ChildTarget>,
        key: u64,
        chunk_offset: u64,
        aligned_size: usize,
        payload_offset: u32,
    ) -> DeliveryCommand {
        if let Some(target) = child_target {
            let chunk_end = chunk_offset + aligned_size as u64;
            let slice_start = std::cmp::max(chunk_offset, target.parent_byte_range.start);
            let slice_end = std::cmp::min(chunk_end, target.parent_byte_range.end);
            let in_chunk_offset = (slice_start - chunk_offset) as u32;
            let child_target_offset =
                target.child_target_offset + (slice_start - target.parent_byte_range.start);
            let child_length = (slice_end.saturating_sub(slice_start)) as u32;
            DeliveryCommand::ChildData {
                parent_offset: chunk_offset,
                offset: payload_offset,
                length: aligned_size as u32,
                child_key: key,
                child_target_offset,
                in_chunk_offset,
                child_length,
            }
        } else {
            DeliveryCommand::Data {
                key,
                target_offset: chunk_offset,
                length: aligned_size as u32,
                offset: payload_offset,
            }
        }
    }

    fn deliver_chunk(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        let offset = self.read_range.start + self.delivered_len as u64;
        let page_size = zx::system_get_page_size() as usize;
        let aligned_size = size.next_multiple_of(page_size);
        let payload = self.payload.as_mut().expect("prepare must be called before commit");
        if aligned_size > size {
            payload.data_mut().subslice_mut(size..aligned_size).fill(0);
        }
        let payload_offset = payload.offset();
        let cmd = Self::make_delivery_command(
            self.child_target.as_ref(),
            self.key,
            offset,
            aligned_size,
            payload_offset,
        );
        payload
            .commit_prefix(aligned_size, cmd.into())
            .map_err(|_| ChunkedArchiveError::IntegrityError)?;
        self.delivered_len += size;
        Ok(())
    }
}

impl DataBuffer for Buffer {
    fn range(&self) -> Range<u64> {
        self.read_range.clone()
    }

    /// Returns a mutable slice into the mapped transfer buffer.
    ///
    /// # Panics
    ///
    /// Panics if `prepare()` has not been called prior to accessing this method.
    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        let payload =
            self.payload.as_mut().expect("prepare must be called before accessing mut_ptr_slice");
        // `payload.commit_prefix()` in `deliver_chunk()` already advances `payload` past
        // `self.delivered_len`, so slice relative to the remaining uncommitted payload.
        let start = self.committed_len - self.delivered_len;
        let remaining = self.total_len - self.committed_len;
        payload.data_mut().subslice_mut(start..start + remaining)
    }

    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        debug_assert!(self.committed_len + size <= self.total_len);
        self.committed_len += size;

        let undelivered = self.committed_len - self.delivered_len;
        if self.committed_len < self.total_len {
            // Once at least `self.flush_threshold` (96 KiB, rounded to a multiple of
            // `read_alignment`) has been committed, deliver complete `read_alignment` chunks so
            // verification overlaps with decompressing the remainder of the readahead window.
            if undelivered >= self.flush_threshold {
                let to_deliver = (undelivered / self.read_alignment) * self.read_alignment;
                self.deliver_chunk(to_deliver)?;
            }
        } else if undelivered > 0 {
            // Deliver any remaining bytes once the entire prepared range has been committed.
            self.deliver_chunk(undelivered)?;
        }

        Ok(())
    }
}

impl PageRequest for Buffer {
    fn prepare(
        &mut self,
        read_range: Range<u64>,
        read_alignment: usize,
    ) -> Result<(), ChunkedArchiveError> {
        assert!(self.payload.is_none(), "prepare must only be called once");
        assert!(read_alignment > 0, "read_alignment must be non-zero");
        self.read_range = read_range;
        self.read_alignment = read_alignment;
        let total_len = (self.read_range.end - self.read_range.start) as usize;
        self.total_len = total_len;
        let page_size = zx::system_get_page_size() as usize;
        // Intermediate prefix flushes via `OwnedPayloadBuffer::commit_prefix` and
        // `DeliveryCommand::Data` must be page-aligned; only the final chunk of a request may have
        // an unaligned tail that gets zero-padded to a page boundary.
        self.flush_threshold = if read_alignment.is_multiple_of(page_size) {
            (VERIFICATION_FLUSH_THRESHOLD / read_alignment).max(1) * read_alignment
        } else {
            usize::MAX
        };
        self.payload = Some(self.verifier.reserve_payload(total_len)?);
        Ok(())
    }

    fn precommit(&self) -> PagePrecommit {
        match &self.payload {
            Some(payload) => PagePrecommit::new(
                payload.vaddr(),
                payload.remaining_len(),
                Arc::clone(payload.sender()) as Arc<dyn Send + Sync>,
            ),
            None => PagePrecommit::default(),
        }
    }

    fn split_at(&mut self, mid: u64) -> Self {
        assert!(self.payload.is_none(), "split_at must be called before prepare");
        assert!(mid > self.read_range.start && mid < self.read_range.end);
        let head_range = self.read_range.start..mid;
        let head_child_target = self.child_target.as_mut().map(|target| {
            let head_target = ChildTarget {
                child_target_offset: target.child_target_offset,
                parent_byte_range: head_range.clone(),
            };
            target.child_target_offset += mid - head_range.start;
            target.parent_byte_range.start = mid;
            head_target
        });
        self.read_range.start = mid;
        Buffer {
            verifier: Arc::clone(&self.verifier),
            key: self.key,
            read_range: head_range,
            read_alignment: 0,
            flush_threshold: 0,
            total_len: 0,
            committed_len: 0,
            delivered_len: 0,
            payload: None,
            child_target: head_child_target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mapping::{DELIVERY_DATA_COMMAND, TARGET_READ_AHEAD_SIZE};
    use test_case::test_case;

    #[fuchsia::test]
    fn test_verifier_buffer_incremental_commit() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 42u64;

        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

        // Fill first page (4096 bytes) with 0xAA
        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        // Verify mut_ptr_slice advanced to second page
        assert_eq!(request.mut_ptr_slice().len(), 4096);
        // Not all data has been committed yet, so no command should be delivered yet.
        assert!(receiver.is_empty());

        // Fill second page (4096 bytes) with 0xBB
        let page2_data = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        request.commit(4096).expect("commit page 2");

        // Verify remaining space is 0
        assert_eq!(request.mut_ptr_slice().len(), 0);

        // Check single consolidated message delivered on queue
        let msg = receiver.peek().expect("msg");
        assert_eq!(msg.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(msg.key, key);
        assert_eq!(msg.length, 8192);
        assert_eq!(msg.target_offset, 0);
        let mut buf = vec![0u8; 8192];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..4096], &page1_data[..]);
        assert_eq!(&buf[4096..], &page2_data[..]);
        msg.pop().expect("pop msg");
    }

    #[fuchsia::test]
    fn test_verifier_buffer_page_aligned_commits() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 43u64;

        let mut request = verifier.get_page_request(key, 0..5000);
        request.prepare(0..5000, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

        let data = vec![0xCCu8; 5000];

        // Commit first 4096 bytes
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&data[..4096]);
        request.commit(4096).expect("commit 4096 bytes");
        assert!(receiver.is_empty());

        // Commit remaining 904 bytes (completes the prepared range)
        request.mut_ptr_slice().subslice_mut(0..904).copy_from_slice(&data[4096..5000]);
        request.commit(904).expect("commit 904 bytes");

        // Check single message delivered, page-aligned to 8192
        let msg = receiver.peek().expect("msg");
        assert_eq!(msg.target_offset, 0);
        assert_eq!(msg.length, 8192);
        let mut buf = vec![0u8; 8192];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..5000], &data[..]);
        assert_eq!(&buf[5000..], &[0u8; 3192]);
        msg.pop().expect("pop msg");
    }

    #[test_case(32 * 1024, 128 * 1024; "zstd_32k_read_128k")]
    #[test_case(48 * 1024, 96 * 1024; "zstd_48k_read_96k")]
    #[test_case(152 * 1024, 152 * 1024; "zstd_152k_read_152k")]
    #[fuchsia::test]
    fn test_verifier_buffer_delivery_data_size_chunking(
        zstd_chunk_size: usize,
        read_alignment: usize,
    ) {
        let total_size = read_alignment * 2;
        let delivery_queue =
            zx::Vmo::create(total_size.next_power_of_two() as u64 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 44u64;

        let mut request = verifier.get_page_request(key, 0..total_size as u64);
        request.prepare(0..total_size as u64, read_alignment).expect("prepare");

        let chunk = vec![0x55u8; zstd_chunk_size];
        let chunks_per_read = read_alignment / zstd_chunk_size;

        // Push `chunks_per_read - 1` chunks - less than `read_alignment`
        for _ in 0..(chunks_per_read - 1) {
            request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
            request.commit(zstd_chunk_size).expect("commit");
        }
        assert!(receiver.is_empty());

        // Push the final chunk of the first `read_alignment` window
        request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
        request.commit(zstd_chunk_size).expect("commit");

        // 1st `read_alignment` message delivered
        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.target_offset, 0);
        assert_eq!(msg1.length, read_alignment as u32);
        msg1.pop().expect("pop msg1");

        // Push remaining `chunks_per_read` chunks (second `read_alignment` window)
        for _ in 0..chunks_per_read {
            request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
            request.commit(zstd_chunk_size).expect("commit");
        }

        // 2nd `read_alignment` message delivered
        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.target_offset, read_alignment as u64);
        assert_eq!(msg2.length, read_alignment as u32);
        msg2.pop().expect("pop msg2");
    }

    #[fuchsia::test]
    #[should_panic(expected = "prepare must only be called once")]
    fn test_verifier_buffer_prepare_multiple_times_panics() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());

        let key = 46u64;
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..4096, TARGET_READ_AHEAD_SIZE as usize).expect("first prepare");
        let _ = request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize);
    }

    #[fuchsia::test]
    fn test_verifier_buffer_supplies_pages_via_delivery_receiver() {
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let port = zx::Port::create();
        let key = 44u64;
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();

        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let vmo_provider = Arc::new(blob_pager_and_verifier::TestVmoProvider::new(
            pager.clone(),
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        vmo_provider
            .register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());
        let receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();
        let _processor = blob_pager_and_verifier::DeliveryQueueProcessor::spawn(
            receiver,
            vmo_provider,
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        let page2_data = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        request.commit(4096).expect("commit page 2");

        let mut read_buf = vec![0u8; 8192];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
        assert_eq!(&read_buf[..4096], &page1_data[..]);
        assert_eq!(&read_buf[4096..8192], &page2_data[..]);
    }

    #[fuchsia::test]
    fn test_verifier_register_blob() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 42u64;
        let leaves = [[0xABu8; 32], [0xCDu8; 32]];

        verifier.register_blob(key, &leaves, 96 * 1024).expect("register_blob failed");

        let msg = receiver.peek().expect("peek msg");
        assert_eq!(msg.opcode, mapping::DELIVERY_REGISTER_BLOB_COMMAND);
        assert_eq!(msg.key, key);
        assert_eq!(msg.length, 64);
        assert_eq!(msg.read_alignment, 96 * 1024);
        let mut buf = vec![0u8; 64];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..32], &[0xABu8; 32]);
        assert_eq!(&buf[32..], &[0xCDu8; 32]);
        msg.pop().expect("pop msg failed");
    }

    #[fuchsia::test]
    fn test_verifier_buffer_96k_overlap() {
        let total_size = 128 * 1024;
        let chunk_size = 32 * 1024;
        let delivery_queue = zx::Vmo::create(total_size as u64 * 2 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 45u64;

        let mut request = verifier.get_page_request(key, 0..total_size as u64);
        request.prepare(0..total_size as u64, chunk_size).expect("prepare");

        let chunk = vec![0x55u8; chunk_size];

        // First two 32 KiB chunks (64 KiB total) should not flush yet.
        for _ in 0..2 {
            request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
            request.commit(chunk_size).expect("commit");
        }
        assert!(receiver.is_empty());

        // Third 32 KiB chunk reaches 96 KiB and flushes 96 KiB so verification overlaps with
        // decompressing the final 32 KiB.
        request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
        request.commit(chunk_size).expect("commit");

        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.target_offset, 0);
        assert_eq!(msg1.length, 96 * 1024);
        msg1.pop().expect("pop msg1");

        // Fourth 32 KiB chunk completes the 128 KiB window and flushes the remaining 32 KiB.
        request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
        request.commit(chunk_size).expect("commit");

        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.target_offset, 96 * 1024);
        assert_eq!(msg2.length, 32 * 1024);
        msg2.pop().expect("pop msg2");
    }

    #[fuchsia::test]
    fn test_verifier_concurrent_out_of_order_buffers_and_precommit() {
        let page = zx::system_get_page_size() as usize;
        // 13 pages header/slots (53,248 bytes) + 8 pages payload (two 4-page buddy blocks).
        let delivery_queue = zx::Vmo::create((21 * page) as u64).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());

        // Prepare two concurrent 3-page buffers (consuming both 4-page buddy blocks) and precommit
        // both.
        let mut req_a = verifier.get_page_request(100, 0..(3 * page) as u64);
        let mut req_b = verifier.get_page_request(200, 0..(3 * page) as u64);
        req_a.prepare(0..(3 * page) as u64, 3 * page).expect("prepare req_a");
        req_b.prepare(0..(3 * page) as u64, 3 * page).expect("prepare req_b");
        req_a.precommit().run();
        req_b.precommit().run();

        // Complete and retire `req_b` BEFORE `req_a` even writes its data.
        req_b.mut_ptr_slice().fill(0xBB);
        req_b.commit(3 * page).expect("commit req_b");
        let msg_b = receiver.peek().expect("peek msg_b");
        assert_eq!(msg_b.key, 200);
        assert_eq!(msg_b.length, (3 * page) as u32);
        msg_b.pop().expect("pop msg_b");

        // Prepare `req_c` in the freed 3 pages, write to it, then drop `req_c` uncommitted.
        let mut req_c = verifier.get_page_request(300, 0..(3 * page) as u64);
        req_c.prepare(0..(3 * page) as u64, 3 * page).expect("prepare req_c");
        req_c.mut_ptr_slice().fill(0xCC);
        drop(req_c);

        // `req_a`'s reservation must remain untouched; write and commit `req_a`.
        req_a.mut_ptr_slice().fill(0xAA);
        req_a.commit(3 * page).expect("commit req_a");

        let msg_a = receiver.peek().expect("peek msg_a");
        assert_eq!(msg_a.key, 100);
        assert_eq!(msg_a.length, (3 * page) as u32);
        let mut buf_a = vec![0u8; 3 * page];
        msg_a.payload_slice(msg_a.offset, msg_a.length).copy_to_slice(&mut buf_a);
        assert_eq!(buf_a, vec![0xAA; 3 * page]);
        msg_a.pop().expect("pop msg_a");
    }

    #[fuchsia::test]
    fn test_verifier_headroom_reserves_space_for_register_blob() {
        let delivery_queue = zx::Vmo::create(mapping::DELIVERY_VMO_SIZE).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        assert_eq!(verifier.headroom, REGISTER_BLOB_HEADROOM_BYTES);

        // Reserve a full 1 MiB page request while holding the reservation open, and verify that
        // `register_blob` still succeeds using the reserved 512 KiB headroom.
        let len = 1024 * 1024usize;
        let read_alignment = 128 * 1024usize;
        let mut req = verifier.get_page_request(601, 0..len as u64);
        req.prepare(0..len as u64, read_alignment).expect("prepare 1 MiB request");
        req.precommit().run();

        let leaves = [[0xABu8; 32]; 4];
        verifier.register_blob(602, &leaves, read_alignment).expect("register_blob with headroom");
        let reg_msg = receiver.peek().expect("peek register_blob");
        assert_eq!(reg_msg.opcode, mapping::DELIVERY_REGISTER_BLOB_COMMAND);
        assert_eq!(reg_msg.key, 602);
        reg_msg.pop().expect("pop register_blob");

        req.mut_ptr_slice().fill(0x7A);
        req.commit(len).expect("commit 1 MiB request");
        let data_msg = receiver.peek().expect("peek 1 MiB data");
        assert_eq!(data_msg.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(data_msg.length, len as u32);
        data_msg.pop().expect("pop 1 MiB data");
    }

    #[fuchsia::test]
    fn test_verifier_child_page_request_sparse_and_inline_tail() {
        let delivery_queue = zx::Vmo::create(128 * 1024).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let child_key = 700u64;

        // 1. Sparse child delivery (`length == 0`).
        verifier.deliver_child_sparse(child_key, 4096, 8192).expect("deliver_child_sparse");
        let msg_sparse = receiver.peek().expect("peek sparse");
        assert_eq!(
            DeliveryCommand::try_from(*msg_sparse).unwrap(),
            DeliveryCommand::ChildData {
                parent_offset: 0,
                offset: 0,
                length: 0,
                child_key,
                child_target_offset: 4096,
                in_chunk_offset: 0,
                child_length: 8192,
            }
        );
        msg_sparse.pop().expect("pop sparse");

        // 2. Child inline tail delivery.
        verifier.deliver_child_inline_tail(16384, child_key, 12288, 512);
        let msg_tail = receiver.peek().expect("peek inline tail");
        assert_eq!(
            DeliveryCommand::try_from(*msg_tail).unwrap(),
            DeliveryCommand::ChildInlineTail {
                parent_byte_offset: 16384,
                child_key,
                child_target_offset: 12288,
                tail_length: 512,
            }
        );
        msg_tail.pop().expect("pop inline tail");

        // 3. Child page request spanning 8192..45056 in parent prepared over 0..65536 (32 KiB
        // alignment).
        let mut child_req = verifier.get_child_page_request(child_key, 4096, 8192..45056);
        child_req.prepare(0..65536, 32768).expect("prepare child_req");
        child_req.mut_ptr_slice().fill(0xEE);
        child_req.commit(65536).expect("commit child_req");

        let msg_child = receiver.peek().expect("peek child data");
        match DeliveryCommand::try_from(*msg_child).unwrap() {
            DeliveryCommand::ChildData {
                parent_offset,
                length,
                child_key: k,
                child_target_offset,
                in_chunk_offset,
                child_length,
                ..
            } => {
                assert_eq!(parent_offset, 0);
                assert_eq!(length, 65536);
                assert_eq!(k, child_key);
                assert_eq!(child_target_offset, 4096);
                assert_eq!(in_chunk_offset, 8192);
                assert_eq!(child_length, 45056 - 8192);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        msg_child.pop().expect("pop child data");
    }

    #[fuchsia::test]
    fn test_verifier_new_invalid_vmo_returns_error() {
        assert_eq!(
            Verifier::new(zx::Vmo::create(0).unwrap()).unwrap_err(),
            zx::Status::INVALID_ARGS
        );
    }
}
