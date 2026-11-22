// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, bail};
use fuchsia_sync::Mutex;
use mapping::{DELIVERY_VMO_SIZE, DeliveryCommand, RawDeliveryCommand};
use std::collections::HashMap;
use std::sync::Arc;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
use storage_units::page_size;
use zx;

fn is_payload_in_bounds(payload_region_offset: usize, offset: u32, length: u32) -> bool {
    payload_region_offset
        .checked_add(offset as usize)
        .and_then(|start| start.checked_add(length as usize))
        .is_some_and(|end| end <= DELIVERY_VMO_SIZE as usize)
}

// The sender is expected to push non-empty, page-aligned payloads. If a payload is the final chunk
// of a blob and does not end on a page boundary, the sender extends `length` to the next page
// boundary and zero-pads the trailing bytes, as required by `fuchsia-merkle`'s `verify_aligned`.
fn is_valid_payload_chunk(
    payload_region_offset: usize,
    blob_offset: u64,
    offset: u32,
    length: u32,
) -> bool {
    let page_size = page_size();
    length > 0
        && page_size.is_aligned(blob_offset | (offset as u64) | (length as u64))
        && is_payload_in_bounds(payload_region_offset, offset, length)
}

#[derive(Copy, Clone)]
struct PayloadTail {
    payload_region_offset: usize,
    blob_offset: u64,
    offset: u32,
    total_length: u32,
}

impl PayloadTail {
    fn is_valid_continuation(
        &self,
        next_blob_offset: u64,
        next_offset: u32,
        next_length: u32,
    ) -> bool {
        self.blob_offset.checked_add(self.total_length as u64) == Some(next_blob_offset)
            && self.offset.checked_add(self.total_length) == Some(next_offset)
            && self.total_length.checked_add(next_length).is_some()
            && is_valid_payload_chunk(
                self.payload_region_offset,
                next_blob_offset,
                next_offset,
                next_length,
            )
    }
}

// Owns the initial FIFO `Message` (and any contiguous continuation slots claimed via
// `Message::try_claim_next`) and yields unverified sub-slices while accumulating the total
// coalesced range for a single `zx::Pager::supply_pages` call. Dropping `CoalescedPayload` retires
// all claimed FIFO slots together.
struct CoalescedPayload<'a> {
    raw_msg: vmo_fifo::Message<'a, RawDeliveryCommand>,
    blob_offset: u64,
    offset: u32,
    verified_length: u32,
    total_length: u32,
}

impl<'a> CoalescedPayload<'a> {
    fn new(
        raw_msg: vmo_fifo::Message<'a, RawDeliveryCommand>,
        blob_offset: u64,
        offset: u32,
        length: u32,
    ) -> Self {
        // `CoalescedPayload` is constructed and extended while `deliver_pages` or
        // `deliver_child_pages` is the active duration slice, which is the final async stage of
        // servicing a page fault.
        let trace_flow_id = raw_msg.trace_flow_id;
        if trace_flow_id != 0 {
            storage_trace::flow_end!("storage", "page_fault", trace_flow_id.into());
        }
        Self { raw_msg, blob_offset, offset, verified_length: 0, total_length: length }
    }

    fn validate(&self) -> Result<(), Error> {
        if !is_valid_payload_chunk(
            self.raw_msg.payload_region_offset(),
            self.blob_offset,
            self.offset,
            self.total_length,
        ) {
            bail!(
                "Invalid delivery payload chunk: blob_offset={}, offset={}, length={}",
                self.blob_offset,
                self.offset,
                self.total_length,
            );
        }
        Ok(())
    }

    fn delivery_offset(&self) -> u64 {
        self.raw_msg.payload_region_offset() as u64 + self.offset as u64
    }

    fn try_claim_continuation<R>(
        &mut self,
        match_next: impl FnOnce(PayloadTail, &RawDeliveryCommand) -> Option<(u32, R)>,
    ) -> Option<R> {
        let tail = PayloadTail {
            payload_region_offset: self.raw_msg.payload_region_offset(),
            blob_offset: self.blob_offset,
            offset: self.offset,
            total_length: self.total_length,
        };
        let mut trace_flow_id = 0;
        let (len, extra) = self.raw_msg.try_claim_next(|next_raw| {
            let res = match_next(tail, next_raw)?;
            trace_flow_id = next_raw.trace_flow_id;
            Some(res)
        })?;

        // The driver flushes intermediate 32 KiB chunks with `trace_flow_id == 0` and attaches the
        // page fault's `trace_flow_id` only to the final chunk of a request, so a claimed
        // continuation slot (or a coalesced adjacent fault) may carry its own distinct flow ID that
        // terminates in this `deliver_pages` / `deliver_child_pages` slice.
        if trace_flow_id != 0 {
            storage_trace::flow_end!("storage", "page_fault", trace_flow_id.into());
        }
        self.total_length += len;
        Some(extra)
    }

    fn take_unverified_chunk(&mut self) -> Option<(u64, PtrByteSlice<'_>)> {
        let len = self.total_length - self.verified_length;
        if len == 0 {
            return None;
        }
        let chunk_blob_offset = self.blob_offset + self.verified_length as u64;
        let chunk_offset = self.offset + self.verified_length;
        self.verified_length = self.total_length;
        Some((chunk_blob_offset, self.raw_msg.payload_slice(chunk_offset, len)))
    }
}

/// Unverified data pages delivered from the driver to be verified, supporting coalescing of
/// contiguous continuation chunks across a single [`zx::Pager::supply_pages`] call.
pub struct UnverifiedPages<'a> {
    payload: CoalescedPayload<'a>,
    key: u64,
}

impl<'a> UnverifiedPages<'a> {
    pub fn new(
        raw_msg: vmo_fifo::Message<'a, RawDeliveryCommand>,
        key: u64,
        target_offset: u64,
        offset: u32,
        length: u32,
    ) -> Result<Self, Error> {
        let payload = CoalescedPayload::new(raw_msg, target_offset, offset, length);
        payload.validate()?;
        Ok(Self { payload, key })
    }

    pub fn key(&self) -> u64 {
        self.key
    }

    pub fn target_offset(&self) -> u64 {
        self.payload.blob_offset
    }

    pub fn len_in_bytes(&self) -> usize {
        self.payload.total_length as usize
    }

    pub fn delivery_offset(&self) -> u64 {
        self.payload.delivery_offset()
    }

    // Claims all currently queued contiguous `Data` continuations for `self.key` and returns
    // `(target_offset, slice)` for the unverified portion, or `None` once caught up.
    pub fn next_chunk(&mut self) -> Option<(u64, PtrByteSlice<'_>)> {
        let expected_key = self.key;
        while self
            .payload
            .try_claim_continuation(|tail, next_raw| {
                let Ok(DeliveryCommand::Data { key, target_offset, length, offset }) =
                    DeliveryCommand::try_from(*next_raw)
                else {
                    return None;
                };
                (key == expected_key && tail.is_valid_continuation(target_offset, offset, length))
                    .then_some((length, ()))
            })
            .is_some()
        {}
        self.payload.take_unverified_chunk()
    }
}

/// Parameters for delivering verified parent blob data (or sparse zeroes) into a child VMO.
pub struct ChildData<'a> {
    payload: CoalescedPayload<'a>,
    pub child_key: u64,
    pub child_target_offset: u64,
    pub in_chunk_offset: u32,
    pub child_length: u32,
}

impl<'a> ChildData<'a> {
    pub fn new(
        raw_msg: vmo_fifo::Message<'a, RawDeliveryCommand>,
        parent_offset: u64,
        offset: u32,
        length: u32,
        child_key: u64,
        child_target_offset: u64,
        in_chunk_offset: u32,
        child_length: u32,
    ) -> Self {
        Self {
            payload: CoalescedPayload::new(raw_msg, parent_offset, offset, length),
            child_key,
            child_target_offset,
            in_chunk_offset,
            child_length,
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        self.payload.validate()
    }

    pub fn length(&self) -> u32 {
        self.payload.total_length
    }

    pub fn delivery_offset(&self) -> u64 {
        self.payload.delivery_offset()
    }

    pub fn payload_mut_slice(&mut self) -> MutPtrByteSlice<'_> {
        self.payload.raw_msg.payload_mut_slice(self.payload.offset, self.payload.total_length)
    }

    // Claims all currently queued contiguous `ChildData` continuations for `self.child_key` and
    // returns `(parent_offset, slice)` for the unverified parent portion, or `None` once caught up.
    pub fn next_chunk(&mut self) -> Option<(u64, PtrByteSlice<'_>)> {
        let expected_child_key = self.child_key;
        let page_size = page_size();
        // To coalesce two `ChildData` chunks into a single `supply_pages` call on `delivery_vmo`,
        // the child slice of the previous chunk(s) must reach the exact end of the parent payload
        // (`in_chunk_offset + child_length == total_length`), and the next chunk's child slice
        // must start at offset 0 of the next parent chunk (`next_in_chunk_offset == 0`).
        while self.in_chunk_offset.checked_add(self.child_length) == Some(self.payload.total_length)
        {
            let Some(expected_child_target) =
                self.child_target_offset.checked_add(self.child_length as u64)
            else {
                break;
            };
            let total_child_length = self.child_length;
            let Some(next_child_len) = self.payload.try_claim_continuation(|tail, next_raw| {
                let Ok(DeliveryCommand::ChildData {
                    parent_offset,
                    offset,
                    length,
                    child_key,
                    child_target_offset,
                    in_chunk_offset,
                    child_length,
                }) = DeliveryCommand::try_from(*next_raw)
                else {
                    return None;
                };
                (child_key == expected_child_key
                    && tail.is_valid_continuation(parent_offset, offset, length)
                    && child_target_offset == expected_child_target
                    && in_chunk_offset == 0
                    && child_length > 0
                    && child_length <= length
                    && page_size.is_aligned(child_length as u64)
                    && total_child_length.checked_add(child_length).is_some())
                .then_some((length, child_length))
            }) else {
                break;
            };
            self.child_length += next_child_len;
        }
        self.payload.take_unverified_chunk()
    }
}

/// Parameters for delivering an EROFS inline tail directly from the parent blob's VMO in
/// `pkg-cache`.
#[derive(Copy, Clone, Debug)]
pub struct ChildInlineTail {
    pub parent_byte_offset: u64,
    pub child_key: u64,
    pub child_target_offset: u64,
    pub tail_length: u32,
    pub trace_flow_id: u64,
}

/// Trait providing data delivery and blob metadata registration for incoming delivery commands.
pub trait DeliveryQueueProvider: Send + Sync + 'static {
    /// Delivers data directly from the delivery VMO into the target blob.
    fn deliver_pages(&self, unverified_pages: UnverifiedPages<'_>) -> Result<(), Error>;

    /// Handles a RegisterBlob command using a pointer slice to shared memory.
    fn register_blob(
        &self,
        key: u64,
        leaf_data: PtrByteSlice<'_>,
        read_alignment: usize,
    ) -> Result<(), Error> {
        let _ = (key, leaf_data, read_alignment);
        Ok(())
    }

    /// Verifies a parent blob chunk (when `data.length > 0`), applies file-tail zeroing in-place
    /// in the delivery queue VMO, and supplies the resulting pages to the child VMO.
    fn deliver_child_pages(&self, _data: ChildData<'_>) -> Result<(), Error> {
        unreachable!()
    }

    /// Supplies an EROFS inline tail page by reading directly from the parent blob's VMO.
    fn deliver_child_inline_tail(
        &self,
        _data: ChildInlineTail,
        _receiver: &vmo_fifo::Receiver<RawDeliveryCommand>,
    ) -> Result<(), Error> {
        unreachable!()
    }
}

/// A simple in-memory `DeliveryQueueProvider` for testing delivery without Merkle verification.
pub struct TestVmoProvider {
    pager: Arc<zx::Pager>,
    delivery_vmo: zx::Vmo,
    vmos: Mutex<HashMap<u64, zx::Vmo>>,
}

impl TestVmoProvider {
    pub fn new(pager: Arc<zx::Pager>, delivery_vmo: zx::Vmo) -> Self {
        Self { pager, delivery_vmo, vmos: Mutex::new(HashMap::new()) }
    }

    pub fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    pub fn register_vmo(&self, key: u64, vmo: zx::Vmo) {
        self.vmos.lock().insert(key, vmo);
    }

    pub fn unregister_vmo(&self, key: u64) {
        self.vmos.lock().remove(&key);
    }
}

impl DeliveryQueueProvider for TestVmoProvider {
    fn deliver_pages(&self, mut unverified_pages: UnverifiedPages<'_>) -> Result<(), Error> {
        while unverified_pages.next_chunk().is_some() {}
        let key = unverified_pages.key();
        let target_offset = unverified_pages.target_offset();
        let vmos = self.vmos.lock();
        if let Some(target_vmo) = vmos.get(&key) {
            let length = unverified_pages.len_in_bytes() as u64;
            self.pager.supply_pages(
                target_vmo,
                target_offset..target_offset + length,
                &self.delivery_vmo,
                unverified_pages.delivery_offset(),
            )?;
            Ok(())
        } else {
            bail!("Unknown or expired key {key} in deliver_pages");
        }
    }
}

const NUM_DELIVERY_THREADS: usize = 2;
const DELIVERY_THREAD_ROLE: &str = "fuchsia.storage.blob.delivery";

pub struct DeliveryQueueProcessor {
    delivery_vmo: zx::Vmo,
    delivery_threads: Vec<std::thread::JoinHandle<()>>,
}

impl DeliveryQueueProcessor {
    pub fn spawn(
        receiver: vmo_fifo::Receiver<RawDeliveryCommand>,
        provider: Arc<dyn DeliveryQueueProvider>,
        delivery_vmo: zx::Vmo,
    ) -> Result<Self, Error> {
        let receiver = Arc::new(receiver);
        let mut delivery_threads = Vec::with_capacity(NUM_DELIVERY_THREADS);

        for _ in 0..NUM_DELIVERY_THREADS {
            let receiver = receiver.clone();
            let provider = provider.clone();
            let thread =
                std::thread::Builder::new().name("blob-delivery".to_string()).spawn(move || {
                    Self::worker_loop(receiver, provider);
                })?;
            delivery_threads.push(thread);
        }

        Ok(Self { delivery_vmo, delivery_threads })
    }

    pub fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    fn worker_loop(
        receiver: Arc<vmo_fifo::Receiver<RawDeliveryCommand>>,
        provider: Arc<dyn DeliveryQueueProvider>,
    ) {
        if let Err(error) = fuchsia_scheduler::set_role_for_this_thread(DELIVERY_THREAD_ROLE) {
            log::warn!(error:?; "Failed to set delivery thread role");
        }
        while let Ok((msg, maybe_cmd)) = receiver.claim_with(|msg| {
            match DeliveryCommand::try_from(**msg) {
                // Process `RegisterBlob` before releasing the claim lock so that any subsequent
                // `Data` commands for this blob claimed by other worker threads observe the
                // initialized Merkle verifier.
                Ok(DeliveryCommand::RegisterBlob { key, offset, length, read_alignment }) => {
                    if !is_payload_in_bounds(msg.payload_region_offset(), offset, length) {
                        log::warn!("RegisterBlob payload out of bounds");
                    } else if let Err(error) = provider.register_blob(
                        key,
                        msg.payload_slice(offset, length),
                        read_alignment as usize,
                    ) {
                        log::warn!(error:?; "Failed to handle RegisterBlob command");
                    }
                    None
                }
                Ok(cmd) => Some(cmd),
                Err(error) => {
                    log::warn!(error:?; "Invalid delivery command opcode");
                    None
                }
            }
        }) {
            if let Some(cmd) = maybe_cmd
                && let Err(error) = Self::handle_delivery(cmd, msg, &receiver, &*provider)
            {
                log::warn!(error:?; "Failed to handle delivery command");
            }
        }
    }

    fn handle_delivery(
        cmd: DeliveryCommand,
        raw_msg: vmo_fifo::Message<'_, RawDeliveryCommand>,
        receiver: &vmo_fifo::Receiver<RawDeliveryCommand>,
        provider: &dyn DeliveryQueueProvider,
    ) -> Result<(), Error> {
        match cmd {
            DeliveryCommand::Data { key, target_offset, length, offset } => {
                storage_trace::duration!(
                    "storage",
                    "deliver_pages",
                    "key" => key,
                    "offset" => target_offset,
                    "len" => length
                );
                provider.deliver_pages(UnverifiedPages::new(
                    raw_msg,
                    key,
                    target_offset,
                    offset,
                    length,
                )?)
            }
            DeliveryCommand::RegisterBlob { .. } => {
                unreachable!("RegisterBlob is handled inside claim_with")
            }
            DeliveryCommand::ChildData {
                parent_offset,
                offset,
                length,
                child_key,
                child_target_offset,
                in_chunk_offset,
                child_length,
            } => {
                storage_trace::duration!(
                    "storage",
                    "deliver_child_pages",
                    "key" => child_key,
                    "offset" => child_target_offset,
                    "len" => child_length
                );
                provider.deliver_child_pages(ChildData::new(
                    raw_msg,
                    parent_offset,
                    offset,
                    length,
                    child_key,
                    child_target_offset,
                    in_chunk_offset,
                    child_length,
                ))
            }
            DeliveryCommand::ChildInlineTail {
                parent_byte_offset,
                child_key,
                child_target_offset,
                tail_length,
            } => {
                let trace_flow_id = raw_msg.trace_flow_id;

                // Pop the header-only command from the FIFO before reading from the parent blob's
                // VMO so `read_head` can advance even if the parent VMO read faults on an evicted
                // or not-yet-supplied parent page.
                drop(raw_msg);
                provider.deliver_child_inline_tail(
                    ChildInlineTail {
                        parent_byte_offset,
                        child_key,
                        child_target_offset,
                        tail_length,
                        trace_flow_id,
                    },
                    receiver,
                )
            }
        }
    }
}

impl Drop for DeliveryQueueProcessor {
    fn drop(&mut self) {
        let _ = self.delivery_vmo.signal(zx::Signals::empty(), vmo_fifo::SIG_SHUTDOWN);
        for thread in self.delivery_threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmo_fifo::SyncSender;

    #[fuchsia::test]
    fn test_coalesced_continuations() {
        let chunk_size = 32 * 1024usize;
        let vmo = zx::Vmo::create(512 * 1024).unwrap();
        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            page_size().into(),
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();
        let receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            vmo,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        // Queue two contiguous `Data` chunks up front, then claim two more (one contiguous, one
        // non-contiguous) mid-flight.
        let mut buf = sender.reserve_payload(4 * chunk_size).unwrap();
        let off0 = buf.offset();
        for i in 0..2 {
            let off = buf.offset();
            buf.commit_prefix(
                chunk_size,
                DeliveryCommand::Data {
                    key: 1,
                    target_offset: (i * chunk_size) as u64,
                    length: chunk_size as u32,
                    offset: off,
                }
                .into(),
            )
            .unwrap();
        }

        let mut pages =
            UnverifiedPages::new(receiver.claim().unwrap(), 1, 0, off0, chunk_size as u32).unwrap();
        let (chunk_off, slice) = pages.next_chunk().unwrap();
        assert_eq!((chunk_off, slice.len()), (0, 2 * chunk_size));

        for target_chunk in [2usize, 4] {
            let off = buf.offset();
            buf.commit_prefix(
                chunk_size,
                DeliveryCommand::Data {
                    key: 1,
                    target_offset: (target_chunk * chunk_size) as u64,
                    length: chunk_size as u32,
                    offset: off,
                }
                .into(),
            )
            .unwrap();
        }
        drop(buf);

        let (chunk_off, slice) = pages.next_chunk().unwrap();
        assert_eq!((chunk_off, slice.len()), ((2 * chunk_size) as u64, chunk_size));
        assert!(pages.next_chunk().is_none());
        assert_eq!(pages.len_in_bytes(), 3 * chunk_size);
        drop(pages);
        // Pop the uncoalesced non-contiguous fourth message.
        receiver.claim().unwrap().pop().unwrap();

        // Verify `ChildData::next_chunk` coalesces contiguous child slices and updates
        // `child_length`.
        let mut cbuf = sender.reserve_payload(2 * chunk_size).unwrap();
        let coff0 = cbuf.offset();
        cbuf.commit_prefix(
            chunk_size,
            DeliveryCommand::ChildData {
                parent_offset: 0,
                offset: coff0,
                length: chunk_size as u32,
                child_key: 7,
                child_target_offset: 0,
                in_chunk_offset: 4096,
                child_length: (chunk_size - 4096) as u32,
            }
            .into(),
        )
        .unwrap();
        let coff1 = cbuf.offset();
        cbuf.commit_prefix(
            chunk_size,
            DeliveryCommand::ChildData {
                parent_offset: chunk_size as u64,
                offset: coff1,
                length: chunk_size as u32,
                child_key: 7,
                child_target_offset: (chunk_size - 4096) as u64,
                in_chunk_offset: 0,
                child_length: chunk_size as u32,
            }
            .into(),
        )
        .unwrap();

        let mut child = ChildData::new(
            receiver.claim().unwrap(),
            0,
            coff0,
            chunk_size as u32,
            7,
            0,
            4096,
            (chunk_size - 4096) as u32,
        );
        child.validate().unwrap();
        let (parent_off, slice) = child.next_chunk().unwrap();
        assert_eq!((parent_off, slice.len()), (0, 2 * chunk_size));
        assert_eq!(child.child_length as usize, 2 * chunk_size - 4096);
        assert!(child.next_chunk().is_none());
    }
}
