// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
#[cfg(test)]
use mapping::BlobRegistration as _;
use mapping::{
    DeliveryCommand, DeliveryHandler, PENDING_DELIVERY_COMMANDS_CAPACITY, PagePrecommit,
    PageRequest, RawDeliveryCommand,
};
use std::num::NonZero;
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;
use storage_units::page_size;
use vmo_fifo::{OwnedPayloadBuffer, SyncSender};

// Leave 512 KiB of payload headroom on full-sized delivery VMOs (>= 1 MiB payload capacity) so
// blob metadata registrations (`prepare_register_blob`) do not have to wait behind in-flight
// page-in reservations.
const REGISTER_BLOB_HEADROOM_BYTES: usize = 512 * 1024;
const MIN_CAPACITY_FOR_HEADROOM_BYTES: usize = 1024 * 1024;
// When incremental flushing is enabled (`flush_alignment == Some(align)`), flush decompressed data
// to the verifier queue every 32 KiB (rounded up to a multiple of `align`) so Merkle verification
// overlaps with decompressing the remainder of a 128 KiB readahead window, while keeping per-chunk
// FIFO overhead low and allowing the verifier to coalesce contiguous chunks into a single
// `zx_pager_supply_pages` syscall.
const VERIFICATION_FLUSH_THRESHOLD: usize = 32 * 1024;

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
            page_size().into(),
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

    /// Registers a blob's Merkle tree leaf hashes with the verifier via the delivery queue.
    #[cfg(test)]
    pub fn register_blob(
        &self,
        key: u64,
        merkle_leaves: &[[u8; 32]],
        read_alignment: usize,
    ) -> Result<(), anyhow::Error> {
        self.prepare_register_blob(key, merkle_leaves.as_flattened().len())?
            .commit(merkle_leaves, read_alignment)
    }
}

/// A pre-allocated reservation in the delivery queue VMO for a `RegisterBlob` command.
#[derive(Debug)]
pub struct BlobRegistration {
    key: u64,
    payload: OwnedPayloadBuffer<RawDeliveryCommand>,
}

impl mapping::BlobRegistration for BlobRegistration {
    fn commit(
        mut self,
        merkle_leaves: &[[u8; 32]],
        read_alignment: usize,
    ) -> Result<(), anyhow::Error> {
        let leaf_bytes: &[u8] = merkle_leaves.as_flattened();
        anyhow::ensure!(
            leaf_bytes.len() <= self.payload.remaining_len(),
            "Merkle leaves length ({}) exceeds reserved payload capacity ({})",
            leaf_bytes.len(),
            self.payload.remaining_len(),
        );
        let payload_offset = self.payload.offset();
        self.payload.data_mut().subslice_mut(0..leaf_bytes.len()).copy_from_slice(leaf_bytes);
        let cmd = DeliveryCommand::RegisterBlob {
            key: self.key,
            offset: payload_offset,
            length: leaf_bytes.len() as u32,
            read_alignment: read_alignment as u32,
        };
        self.payload.commit(cmd.into())?;
        Ok(())
    }
}

impl DeliveryHandler for Verifier {
    type Request = Buffer;
    type BlobRegistration = BlobRegistration;

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
            trace_flow_id: 0,
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
            trace_flow_id: 0,
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
        trace_flow_id: u64,
    ) {
        let cmd = DeliveryCommand::ChildInlineTail {
            parent_byte_offset,
            child_key,
            child_target_offset,
            tail_length,
        };
        let _ = self.sender.push(RawDeliveryCommand::from(cmd).with_trace_flow_id(trace_flow_id));
    }

    /// Pushes a [`DeliveryCommand::ChildData`] command with `length == 0` onto the delivery queue
    /// to supply a zero-filled sparse range to `child_key`.
    fn deliver_child_sparse(
        &self,
        child_key: u64,
        child_target_offset: u64,
        child_length: u64,
        trace_flow_id: u64,
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
        self.sender.push(RawDeliveryCommand::from(cmd).with_trace_flow_id(trace_flow_id))?;
        Ok(())
    }

    /// Reserves space in the delivery queue VMO to register a blob (`key`) with at most
    /// `max_leaf_bytes` of Merkle tree leaf hashes.
    fn prepare_register_blob(
        &self,
        key: u64,
        max_leaf_bytes: usize,
    ) -> Result<BlobRegistration, anyhow::Error> {
        let payload = self.sender.reserve_owned_payload(max_leaf_bytes, 0)?;
        Ok(BlobRegistration { key, payload })
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
    // Alignment boundary at which intermediate chunks are flushed to the verifier queue (`0` when
    // intermediate flushing is disabled).
    read_alignment: usize,
    // Precomputed byte threshold at which intermediate `read_alignment` chunks are flushed to the
    // verifier queue (`usize::MAX` if intermediate flushing is disabled or `read_alignment` is not
    // a multiple of `page_size`).
    flush_threshold: usize,
    // Bytes committed into `self.payload` so far via calls to [`DataBuffer::commit`].
    committed_len: usize,
    // Bytes delivered to the verifier queue so far.
    delivered_len: usize,
    payload: Option<OwnedPayloadBuffer<RawDeliveryCommand>>,
    child_target: Option<ChildTarget>,
    trace_flow_id: u64,
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

    fn deliver_chunk(
        &mut self,
        size: usize,
        trace_flow_id: u64,
    ) -> Result<(), ChunkedArchiveError> {
        let offset = self.read_range.start + self.delivered_len as u64;
        let aligned_size = page_size().align_up(size as u64).unwrap() as usize;
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
            .commit_prefix(
                aligned_size,
                RawDeliveryCommand::from(cmd).with_trace_flow_id(trace_flow_id),
            )
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
            // Once at least `self.flush_threshold` (32 KiB, rounded to a multiple of
            // `read_alignment`) has been committed, deliver complete `read_alignment` chunks so
            // verification overlaps with decompressing the remainder of the readahead window.
            if undelivered >= self.flush_threshold {
                let to_deliver = (undelivered / self.read_alignment) * self.read_alignment;
                self.deliver_chunk(to_deliver, 0)?;
            }
        } else if undelivered > 0 {
            // Deliver any remaining bytes once the entire prepared range has been committed,
            // attaching the page-fault flow ID to the final chunk.
            let trace_flow_id = std::mem::take(&mut self.trace_flow_id);
            self.deliver_chunk(undelivered, trace_flow_id)?;
        }

        Ok(())
    }
}

impl PageRequest for Buffer {
    fn prepare(
        &mut self,
        read_range: Range<u64>,
        flush_alignment: Option<NonZero<usize>>,
    ) -> Result<(), ChunkedArchiveError> {
        assert!(self.payload.is_none(), "prepare must only be called once");
        self.read_range = read_range;
        let total_len = (self.read_range.end - self.read_range.start) as usize;
        self.total_len = total_len;
        // Intermediate prefix flushes via `OwnedPayloadBuffer::commit_prefix` and
        // `DeliveryCommand::Data` must be page-aligned; only the final chunk of a request may have
        // an unaligned tail that gets zero-padded to a page boundary.
        if let Some(align) = flush_alignment.map(NonZero::get)
            && page_size().is_aligned(align as u64)
        {
            self.read_alignment = align;
            self.flush_threshold = VERIFICATION_FLUSH_THRESHOLD.next_multiple_of(align);
        } else {
            self.read_alignment = 0;
            self.flush_threshold = usize::MAX;
        }
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

    fn with_trace_flow_id(mut self, trace_flow_id: u64) -> Self {
        self.trace_flow_id = trace_flow_id;
        self
    }

    fn trace_flow_id(&self) -> u64 {
        self.trace_flow_id
    }

    fn forwards_trace_flow(&self) -> bool {
        true
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
            flush_threshold: usize::MAX,
            total_len: 0,
            committed_len: 0,
            delivered_len: 0,
            payload: None,
            child_target: head_child_target,
            trace_flow_id: 0,
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
        request.prepare(0..8192, None).expect("prepare");

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
        request.prepare(0..5000, None).expect("prepare");

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
        request.prepare(0..total_size as u64, NonZero::new(read_alignment)).expect("prepare");

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
        request.prepare(0..4096, None).expect("first prepare");
        let _ = request.prepare(0..8192, None);
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
        request.prepare(0..8192, None).expect("prepare");

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
    fn test_verifier_buffer_32k_overlap() {
        let total_size = 128 * 1024;
        let sub_chunk_size = 16 * 1024;
        let flush_size = 32 * 1024;
        let delivery_queue = zx::Vmo::create(total_size as u64 * 2 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 45u64;

        let mut request = verifier.get_page_request(key, 0..total_size as u64);
        // With `flush_alignment == NonZero::new(16 KiB)`, `flush_threshold` rounds up to 32 KiB
        // (`VERIFICATION_FLUSH_THRESHOLD`).
        request.prepare(0..total_size as u64, NonZero::new(sub_chunk_size)).expect("prepare");

        let sub_chunk = vec![0x55u8; sub_chunk_size];
        let mut base_payload_offset = None;

        for i in 0..4 {
            // First 16 KiB commit is below the 32 KiB flush threshold, so nothing is flushed yet.
            request.mut_ptr_slice().subslice_mut(0..sub_chunk_size).copy_from_slice(&sub_chunk);
            request.commit(sub_chunk_size).expect("commit first 16 KiB");
            assert!(receiver.is_empty());

            // Second 16 KiB commit reaches the 32 KiB flush threshold and flushes a 32 KiB chunk.
            request.mut_ptr_slice().subslice_mut(0..sub_chunk_size).copy_from_slice(&sub_chunk);
            request.commit(sub_chunk_size).expect("commit second 16 KiB");

            let msg = receiver.peek().expect("msg");
            let base = *base_payload_offset.get_or_insert(msg.offset);
            assert_eq!(msg.offset, base + (i * flush_size) as u32);
            assert_eq!(msg.target_offset, (i * flush_size) as u64);
            assert_eq!(msg.length, flush_size as u32);
            msg.pop().expect("pop msg");
        }
    }

    #[fuchsia::test]
    fn test_verifier_buffer_no_incremental_flush_when_disabled() {
        let total_size = TARGET_READ_AHEAD_SIZE as usize;
        let chunk_size = 32 * 1024;
        let delivery_queue = zx::Vmo::create(total_size as u64 * 2 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());
        let key = 47u64;

        let mut request =
            verifier.get_page_request(key, 0..total_size as u64).with_trace_flow_id(999);
        request.prepare(0..total_size as u64, None).expect("prepare");

        // Committing the first three 32 KiB chunks must not wake the verifier early when
        // `flush_alignment` is `None`.
        for i in 0..3 {
            request.mut_ptr_slice().subslice_mut(0..chunk_size).fill(i as u8 + 1);
            request.commit(chunk_size).expect("commit intermediate chunk");
            assert!(receiver.is_empty());
        }

        // Committing the final 32 KiB chunk delivers the full 128 KiB window in a single message.
        request.mut_ptr_slice().subslice_mut(0..chunk_size).fill(4);
        request.commit(chunk_size).expect("commit final chunk");

        let msg = receiver.peek().expect("msg");
        assert_eq!(msg.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(msg.key, key);
        assert_eq!(msg.target_offset, 0);
        assert_eq!(msg.length, total_size as u32);
        assert_eq!(msg.trace_flow_id, 999);
        msg.pop().expect("pop msg");
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
        req_a.prepare(0..(3 * page) as u64, NonZero::new(3 * page)).expect("prepare req_a");
        req_b.prepare(0..(3 * page) as u64, NonZero::new(3 * page)).expect("prepare req_b");
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
        req_c.prepare(0..(3 * page) as u64, NonZero::new(3 * page)).expect("prepare req_c");
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
        req.prepare(0..len as u64, None).expect("prepare 1 MiB request");
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
        verifier.deliver_child_sparse(child_key, 4096, 8192, 1001).expect("deliver_child_sparse");
        let msg_sparse = receiver.peek().expect("peek sparse");
        assert_eq!(msg_sparse.trace_flow_id, 1001);
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
        verifier.deliver_child_inline_tail(16384, child_key, 12288, 512, 1002);
        let msg_tail = receiver.peek().expect("peek inline tail");
        assert_eq!(msg_tail.trace_flow_id, 1002);
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
        // alignment): intermediate 32 KiB flush carries `trace_flow_id == 0`, and the final chunk
        // carries the page-fault flow ID (`1003`).
        let mut child_req =
            verifier.get_child_page_request(child_key, 4096, 8192..45056).with_trace_flow_id(1003);
        child_req.prepare(0..65536, NonZero::new(32768)).expect("prepare child_req");
        child_req.mut_ptr_slice().subslice_mut(0..32768).fill(0xEE);
        child_req.commit(32768).expect("commit child_req chunk 0");
        child_req.mut_ptr_slice().fill(0xEE);
        child_req.commit(32768).expect("commit child_req chunk 1");

        let msg_child_0 = receiver.peek().expect("peek child data chunk 0");
        assert_eq!(msg_child_0.trace_flow_id, 0);
        match DeliveryCommand::try_from(*msg_child_0).unwrap() {
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
                assert_eq!(length, 32768);
                assert_eq!(k, child_key);
                assert_eq!(child_target_offset, 4096);
                assert_eq!(in_chunk_offset, 8192);
                assert_eq!(child_length, 32768 - 8192);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        msg_child_0.pop().expect("pop child data chunk 0");

        let msg_child_1 = receiver.peek().expect("peek child data chunk 1");
        assert_eq!(msg_child_1.trace_flow_id, 1003);
        match DeliveryCommand::try_from(*msg_child_1).unwrap() {
            DeliveryCommand::ChildData {
                parent_offset,
                length,
                child_key: k,
                child_target_offset,
                in_chunk_offset,
                child_length,
                ..
            } => {
                assert_eq!(parent_offset, 32768);
                assert_eq!(length, 32768);
                assert_eq!(k, child_key);
                assert_eq!(child_target_offset, 4096 + (32768 - 8192));
                assert_eq!(in_chunk_offset, 0);
                assert_eq!(child_length, 45056 - 32768);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        msg_child_1.pop().expect("pop child data chunk 1");
    }

    #[fuchsia::test]
    fn test_verifier_new_invalid_vmo_returns_error() {
        assert_eq!(
            Verifier::new(zx::Vmo::create(0).unwrap()).unwrap_err(),
            zx::Status::INVALID_ARGS
        );
    }

    #[fuchsia::test]
    fn test_verifier_prepare_register_blob_upper_bound_and_drop() {
        let page = zx::system_get_page_size() as usize;
        // 13 pages header/slots (53,248 bytes) + 1 page payload (one 1-page buddy block).
        let delivery_queue = zx::Vmo::create((14 * page) as u64).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue).unwrap());

        // Reserve the single 1-page buddy block using the metadata storage upper bound (`page`),
        // then drop the reservation uncommitted (simulating a failed metadata block read).
        let dropped_reg = verifier.prepare_register_blob(800, page).expect("prepare dropped_reg");
        drop(dropped_reg);
        assert!(receiver.is_empty());

        // The freed buddy block must be immediately reusable for the next registration, which
        // commits a smaller actual leaf slice (64 bytes <= 4096 bytes reserved).
        let reg = verifier.prepare_register_blob(801, page).expect("prepare reg");
        let leaves = [[0x11u8; 32], [0x22u8; 32]];
        reg.commit(&leaves, 32 * 1024).expect("commit reg");

        let msg = receiver.peek().expect("peek msg");
        assert_eq!(msg.opcode, mapping::DELIVERY_REGISTER_BLOB_COMMAND);
        assert_eq!(msg.key, 801);
        assert_eq!(msg.length, 64);
        assert_eq!(msg.read_alignment, 32 * 1024);
        let mut buf = vec![0u8; 64];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..32], &[0x11u8; 32]);
        assert_eq!(&buf[32..], &[0x22u8; 32]);
        msg.pop().expect("pop msg");
    }

    #[fuchsia::test]
    fn test_verifier_concurrent_page_ins_and_metadata_registrations_under_contention() {
        use mapping::reader::BlockService;
        use mapping::{
            BLOCK_SIZE, Extent, Extents, Files, MAPPINGS_COMMAND, RawMappingCommand,
            TARGET_READ_AHEAD_SIZE, process_mapping_command,
        };
        use storage_device::buffer::OwnedBuffer;
        use storage_device::buffer_allocator::{BufferAllocator, BufferSource};

        struct ContendedBlockService {
            allocator: Arc<BufferAllocator>,
            device_data: Vec<u8>,
            tx: std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>,
        }

        impl BlockService for ContendedBlockService {
            fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
                let len = std::cmp::min(max_len, BLOCK_SIZE as usize);
                self.allocator.allocate_buffer_sync_owned(len)
            }

            fn read_blocks(
                &self,
                device_offset: u64,
                mut dest_buffer: OwnedBuffer,
                on_complete: Box<dyn FnOnce(Result<OwnedBuffer, anyhow::Error>) + Send>,
            ) -> Result<(), anyhow::Error> {
                let start = device_offset as usize;
                let end = start + dest_buffer.len();
                dest_buffer.as_mut_ptr_slice().copy_from_slice(&self.device_data[start..end]);
                let _ = self.tx.send(Box::new(move || {
                    on_complete(Ok(dest_buffer));
                }));
                Ok(())
            }
        }

        let page = zx::system_get_page_size() as usize;
        // 13 pages header/slots + 32 pages (128 KiB) payload capacity (< 1 MiB, so headroom == 0).
        // A single 128 KiB readahead window on `PagerThread` consumes 100% of `delivery_vmo`
        // payload capacity, forcing `prepare_register_blob` and `PagerThread` to contend directly
        // on both `delivery_vmo` and the 2-block (8 KiB) `BufferAllocator`.
        let delivery_queue = zx::Vmo::create((45 * page) as u64).unwrap();
        let mut delivery_receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();
        let verifier = Verifier::new(delivery_queue).unwrap();
        assert_eq!(verifier.headroom, 0);

        // Device layout:
        // - Blocks 0..32 (128 KiB): uncompressed data for blob 1.
        // - Block 32 (4 KiB): serialized v64 `BlobMetadata` with 1 Merkle leaf (`[0xAB; 32]`,
        //   serialized via `zerocopy_serialization` with byte length prefix `32`) and
        //   `BlobFormat::Uncompressed` (`0`).
        let data_size = TARGET_READ_AHEAD_SIZE as usize;
        let mut device_data = vec![0x5Au8; data_size + BLOCK_SIZE as usize];
        let mut encoded_meta = vec![64u8, 0, 0, 0, 32];
        encoded_meta.extend_from_slice(&[0xABu8; 32]);
        encoded_meta.push(0);
        device_data[data_size..data_size + encoded_meta.len()].copy_from_slice(&encoded_meta);

        // Single-threaded block completion worker and a tight 2-block (8 KiB) `BufferAllocator`.
        let (completion_tx, completion_rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let completion_thread = std::thread::spawn(move || {
            while let Ok(cb) = completion_rx.recv() {
                cb();
            }
        });
        let allocator = Arc::new(BufferAllocator::new(
            BLOCK_SIZE as usize,
            BufferSource::new(2 * BLOCK_SIZE as usize),
        ));
        let service = Arc::new(ContendedBlockService { allocator, device_data, tx: completion_tx });

        let port = zx::Port::create();
        let files = Arc::new(Files::new(
            service.clone(),
            verifier,
            port.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        let _pager_thread = files.spawn_pager_thread("contended-pager");

        // Map blob 1 via `process_mapping_command` with 0 metadata extents first so page-ins can
        // immediately reserve the full 128 KiB `delivery_vmo` window while
        // `process_mapping_command` registers new blobs.
        let mapping_vmo = zx::Vmo::create(65536).unwrap();
        let mut mapping_sender = vmo_fifo::SyncSender::<RawMappingCommand>::new(
            mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            32,
        )
        .unwrap();
        let mapping_receiver =
            Arc::new(vmo_fifo::Receiver::<RawMappingCommand>::new(mapping_vmo, 32).unwrap());

        let data_extents =
            Extents::try_new([Extent::new(0..TARGET_READ_AHEAD_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(TARGET_READ_AHEAD_SIZE))], 0)
                .unwrap();
        let mut data_only_payload = Vec::new();
        for w in Extents::encode_extents(&data_extents) {
            data_only_payload.extend_from_slice(&w.to_le_bytes());
        }
        let mut with_meta_payload = data_only_payload.clone();
        for w in Extents::encode_extents(&meta_extents) {
            with_meta_payload.extend_from_slice(&w.to_le_bytes());
        }

        // Map key 1 (no metadata on disk) so it's immediately loaded for page faults.
        {
            let mut buf = mapping_sender.reserve_payload(data_only_payload.len()).unwrap();
            let offset = buf.offset();
            buf.data().copy_from_slice(&data_only_payload);
            buf.commit(RawMappingCommand {
                opcode: MAPPINGS_COMMAND,
                offset,
                key: 1,
                stored_size: TARGET_READ_AHEAD_SIZE,
                device_offset: 0,
                metadata_count: 0,
                extent_count: 1,
            })
            .unwrap();
            let msg = mapping_receiver.claim().unwrap();
            process_mapping_command(msg, &mapping_receiver, &files).unwrap();
            // Pop key 1's initial empty RegisterBlob from the delivery queue.
            let reg1 = delivery_receiver.peek().unwrap();
            assert_eq!(reg1.opcode, mapping::DELIVERY_REGISTER_BLOB_COMMAND);
            assert_eq!(reg1.key, 1);
            reg1.pop().unwrap();
        }

        const PAGE_IN_ITERATIONS: usize = 6;
        const METADATA_BLOBS: u64 = 12;

        // Consumer thread draining `delivery_vmo` (both `DELIVERY_DATA_COMMAND` chunks from
        // `PagerThread` and `DELIVERY_REGISTER_BLOB_COMMAND` from `process_mapping_command`).
        let consumer_thread = std::thread::spawn(move || {
            let mut data_bytes_delivered = 0usize;
            let mut registered_blobs = 0u64;
            while data_bytes_delivered < PAGE_IN_ITERATIONS * data_size
                || registered_blobs < METADATA_BLOBS
            {
                let msg = delivery_receiver.peek().expect("delivery_receiver peek");
                match msg.opcode {
                    DELIVERY_DATA_COMMAND => {
                        data_bytes_delivered += msg.length as usize;
                    }
                    mapping::DELIVERY_REGISTER_BLOB_COMMAND => {
                        assert_eq!(msg.length, 32);
                        registered_blobs += 1;
                    }
                    other => panic!("Unexpected delivery opcode: {other}"),
                }
                msg.pop().expect("delivery_receiver pop");
            }
        });

        // Queue `PAGE_IN_ITERATIONS` user pager read packets for key 1 onto `port` while
        // concurrently running `process_mapping_command` for `METADATA_BLOBS` blobs with on-disk
        // metadata.
        let pager = zx::Pager::create(zx::PagerOptions::empty()).unwrap();
        let vmos: Vec<zx::Vmo> = (0..PAGE_IN_ITERATIONS)
            .map(|_| {
                pager.create_vmo(zx::VmoOptions::empty(), &port, 1, TARGET_READ_AHEAD_SIZE).unwrap()
            })
            .collect();
        let fault_threads: Vec<_> = vmos
            .iter()
            .map(|vmo| {
                let vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                std::thread::spawn(move || {
                    let mut byte = [0u8; 1];
                    let _ = vmo.read(&mut byte, 0);
                })
            })
            .collect();

        for key in 100..(100 + METADATA_BLOBS) {
            let mut buf = mapping_sender.reserve_payload(with_meta_payload.len()).unwrap();
            let offset = buf.offset();
            buf.data().copy_from_slice(&with_meta_payload);
            buf.commit(RawMappingCommand {
                opcode: MAPPINGS_COMMAND,
                offset,
                key,
                stored_size: TARGET_READ_AHEAD_SIZE,
                device_offset: 0,
                metadata_count: 1,
                extent_count: 1,
            })
            .unwrap();
            let msg = mapping_receiver.claim().unwrap();
            process_mapping_command(msg, &mapping_receiver, &files).unwrap();
        }

        // Wait for all 6 x 128 KiB page-ins and all 12 metadata registrations to complete through
        // `delivery_vmo` without deadlocking!
        consumer_thread.join().unwrap();
        for key in 100..(100 + METADATA_BLOBS) {
            while files.get_file(key).is_none() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        // Detach the test VMOs so the faulting threads return cleanly.
        for vmo in &vmos {
            let _ = pager.detach_vmo(vmo);
        }
        for t in fault_threads {
            let _ = t.join();
        }
        drop(_pager_thread);
        drop(files);
        drop(service);
        completion_thread.join().unwrap();
    }
}
