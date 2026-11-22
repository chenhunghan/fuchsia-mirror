// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reader::{
    BlockService, GuardedBlockService, MAX_READ_BUFFER_SIZE, read_aligned_range,
    read_aligned_range_with_precommit,
};
use crate::{
    BLOCK_SIZE, ENCRYPTION_KEY_SIZE, Extents, MappingCommand, NullPageRequest, PageRequest,
    RawMappingCommand,
};
use anyhow::{Error, anyhow, bail};
use blob_metadata::{BlobMetadata, BlobMetadataV53, MerkleLeaves};
use byteorder::{LittleEndian, ReadBytesExt};
use delivery_blob::compression::{
    CompressionAlgorithm, CompressionInfo, DataBuffer, StreamingDecompressor,
};
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use fxfs_crypto::{Cipher, FxfsCipher, UnwrappedKey};
use std::borrow::Borrow;
use std::cmp::min;
use std::collections::hash_map::{Entry, HashMap};
use std::num::NonZero;
use std::ops::{ControlFlow, Range};
use std::sync::{Arc, OnceLock};
use storage_profile::{FileMessage, Message as _};
use storage_units::{BlockSize, page_size};
use vmo_fifo::{Message, Receiver, SlotGuard};
use zx::sys::zx_page_request_command_t::ZX_PAGER_VMO_READ;

/// Target readahead size (128 KiB), matching Fxfs.
///
/// Uncompressed and encrypted files always use this size. For compressed files, the effective size
/// is adjusted so that each readahead window aligns to a whole number of compression chunks.
pub const TARGET_READ_AHEAD_SIZE: u64 = 128 * 1024;

/// Default Merkle verification alignment (32 KiB) for uncompressed blobs, matching standard
/// compressed blob chunk sizes.
pub const DEFAULT_VERIFICATION_ALIGNMENT: u64 = 32 * 1024;

/// Calculates the readahead size for a given `compression_chunk_size`, rounding down
/// `target_read_ahead_size` to a multiple of `compression_chunk_size`, or returning
/// `compression_chunk_size` if it is larger than `target_read_ahead_size`.
fn read_ahead_size_for_chunk_size(compression_chunk_size: u64, target_read_ahead_size: u64) -> u64 {
    if compression_chunk_size >= target_read_ahead_size {
        compression_chunk_size
    } else {
        (target_read_ahead_size / compression_chunk_size) * compression_chunk_size
    }
}

// Begins a `page_fault` trace flow and returns its non-zero ID when the `"storage"` trace
// category is enabled, or `0` without calling `Id::new()` when tracing is disabled.
fn begin_page_fault_flow() -> u64 {
    let mut trace_flow_id = 0;
    storage_trace::flow_begin!("storage", "page_fault", {
        let id = storage_trace::Id::new();
        trace_flow_id = id.into();
        id
    });
    trace_flow_id
}

fn trace_read_callback_flow(trace_flow_id: u64, end_flow: bool) {
    if trace_flow_id != 0 {
        if end_flow {
            storage_trace::flow_end!("storage", "page_fault", trace_flow_id.into());
        } else {
            storage_trace::flow_step!("storage", "page_fault", trace_flow_id.into());
        }
    }
}

/// Transformation applied to stored data before it can be presented to the reader.
#[derive(Default)]
pub enum Transform {
    /// The file is stored uncompressed and unencrypted.
    #[default]
    None,
    /// The file is stored compressed with chunked metadata.
    Compressed(CompressionInfo),
    /// The file is stored encrypted with a cipher.
    Encrypted(Arc<dyn Cipher>),
}

impl std::fmt::Debug for Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "Transform::None"),
            Self::Compressed(_) => write!(f, "Transform::Compressed(..)"),
            Self::Encrypted(cipher) => f.debug_tuple("Transform::Encrypted").field(cipher).finish(),
        }
    }
}

impl From<Option<CompressionInfo>> for Transform {
    fn from(info: Option<CompressionInfo>) -> Self {
        match info {
            Some(info) => Self::Compressed(info),
            None => Self::None,
        }
    }
}

impl From<CompressionInfo> for Transform {
    fn from(info: CompressionInfo) -> Self {
        Self::Compressed(info)
    }
}

impl From<Arc<dyn Cipher>> for Transform {
    fn from(cipher: Arc<dyn Cipher>) -> Self {
        Self::Encrypted(cipher)
    }
}

#[derive(Clone)]
struct FileCompression(Arc<File>);

impl Borrow<CompressionInfo> for FileCompression {
    fn borrow(&self) -> &CompressionInfo {
        self.0.compression_info().expect("compression_info missing")
    }
}

/// A mapped file containing extents and an optional transformation (compression or encryption).
pub struct File {
    extents: Extents,
    uncompressed_size: u64,
    transform: Transform,
    // Holds the first FIFO `SlotGuard` that retired or began replacing this `File` so its slot
    // stays unpopped until all in-flight block reads on this `File` complete. Because a `Files`
    // session is driven by a single FIFO `Receiver` whose `read_head` advances only across
    // contiguous completed slots, keeping the first retiring slot unpopped also prevents any later
    // superseding `Mappings` or `CloseBlob` slot from retiring early.
    retired_slot_guard: OnceLock<Arc<SlotGuard<RawMappingCommand>>>,
}

impl File {
    pub fn new(extents: Extents, uncompressed_size: u64, transform: Transform) -> Self {
        Self { extents, uncompressed_size, transform, retired_slot_guard: OnceLock::new() }
    }

    fn attach_slot_guard(&self, slot_guard: Option<Arc<SlotGuard<RawMappingCommand>>>) {
        if let Some(guard) = slot_guard {
            let _ = self.retired_slot_guard.set(guard);
        }
    }

    /// Returns the extents mapping logical offsets to device offsets.
    pub fn extents(&self) -> &Extents {
        &self.extents
    }

    /// Returns the uncompressed size of the file in bytes.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns the transform applied to the file if compressed or encrypted.
    pub fn transform(&self) -> &Transform {
        &self.transform
    }

    /// Returns decompression metadata if the file is compressed.
    pub fn compression_info(&self) -> Option<&CompressionInfo> {
        match &self.transform {
            Transform::Compressed(info) => Some(info),
            _ => None,
        }
    }

    /// Returns the cipher if the file is encrypted.
    pub fn cipher(&self) -> Option<&Arc<dyn Cipher>> {
        match &self.transform {
            Transform::Encrypted(cipher) => Some(cipher),
            _ => None,
        }
    }

    /// Returns the read-ahead window size in bytes for this file.
    pub fn read_ahead_size(&self) -> u64 {
        match &self.transform {
            Transform::Compressed(info) => {
                read_ahead_size_for_chunk_size(info.chunk_size(), TARGET_READ_AHEAD_SIZE)
            }
            Transform::None | Transform::Encrypted(_) => TARGET_READ_AHEAD_SIZE,
        }
    }

    /// Returns the Merkle verification alignment in bytes for this file (`chunk_size` for
    /// compressed files, or [`DEFAULT_VERIFICATION_ALIGNMENT`] for uncompressed/encrypted files).
    pub fn verification_alignment(&self) -> u64 {
        match &self.transform {
            Transform::Compressed(info) => info.chunk_size(),
            Transform::None | Transform::Encrypted(_) => DEFAULT_VERIFICATION_ALIGNMENT,
        }
    }

    /// Returns the maximum uncompressed byte length for a single [`PageRequest`] window on a file
    /// with `window_alignment` (`MAX_READ_BUFFER_SIZE` rounded down to a multiple of
    /// `window_alignment`, or `window_alignment` if larger).
    fn max_page_request_window(window_alignment: u64) -> u64 {
        std::cmp::max(
            (MAX_READ_BUFFER_SIZE as u64 / window_alignment) * window_alignment,
            window_alignment,
        )
    }

    /// Expands `original_range` to `window_alignment` boundaries, clamped to the page-aligned end
    /// of the file, or returns `None` if `original_range` is empty, starts past the end of the
    /// file, or aligning the file size overflows.
    fn aligned_read_range(
        &self,
        original_range: Range<u64>,
        window_alignment: u64,
    ) -> Option<Range<u64>> {
        self.aligned_read_range_for_page_size(original_range, window_alignment, page_size().into())
    }

    fn aligned_read_range_for_page_size(
        &self,
        original_range: Range<u64>,
        window_alignment: u64,
        page_size: BlockSize,
    ) -> Option<Range<u64>> {
        if original_range.is_empty() {
            return None;
        }
        let page_aligned_size = page_size.align_up(self.uncompressed_size)?;
        if original_range.start >= page_aligned_size {
            return None;
        }
        Some(
            (original_range.start / window_alignment) * window_alignment
                ..min(original_range.end.next_multiple_of(window_alignment), page_aligned_size),
        )
    }

    /// Streams and decodes the uncompressed range requested by `page_request`, applying readahead.
    pub fn read_range(
        self: &Arc<Self>,
        service: &(impl BlockService + ?Sized),
        page_request: impl PageRequest,
    ) {
        self.read_range_with_recording(self.read_ahead_size(), service, page_request, |_| {});
    }

    /// Streams and decodes the uncompressed range requested by `page_request`, aligning the read
    /// window to `window_alignment` rounded up to a multiple of `self.verification_alignment()`.
    ///
    /// Unlike [`Self::read_range`] (which expands the read window to `self.read_ahead_size()`),
    /// this allows a caller (such as a child file mapping) to bound the read window to a smaller
    /// alignment (e.g. a single verification chunk of the parent file).
    pub fn read_range_with_alignment(
        self: &Arc<Self>,
        window_alignment: u64,
        service: &(impl BlockService + ?Sized),
        page_request: impl PageRequest,
    ) {
        let verification_alignment = self.verification_alignment();
        let window_alignment = std::cmp::max(window_alignment, verification_alignment)
            .next_multiple_of(verification_alignment);
        self.read_range_with_recording(window_alignment, service, page_request, |_| {});
    }

    fn read_range_with_recording(
        self: &Arc<Self>,
        window_alignment: u64,
        service: &(impl BlockService + ?Sized),
        mut page_request: impl PageRequest,
        record_offset: impl Fn(u64),
    ) {
        let max_window = Self::max_page_request_window(window_alignment);
        loop {
            let range = page_request.range();
            let max_end = (range.start / max_window + 1) * max_window;
            if range.end > max_end {
                let head = page_request.split_at(max_end);
                self.read_single_window(window_alignment, service, head, &record_offset);
            } else {
                self.read_single_window(window_alignment, service, page_request, &record_offset);
                break;
            }
        }
    }

    fn read_single_window(
        self: &Arc<Self>,
        window_alignment: u64,
        service: &(impl BlockService + ?Sized),
        page_request: impl PageRequest,
        record_offset: &impl Fn(u64),
    ) {
        self.read_single_window_for_page_size(
            window_alignment,
            page_size().into(),
            service,
            page_request,
            record_offset,
        );
    }

    fn read_single_window_for_page_size(
        self: &Arc<Self>,
        window_alignment: u64,
        page_size: BlockSize,
        service: &(impl BlockService + ?Sized),
        mut page_request: impl PageRequest,
        record_offset: &impl Fn(u64),
    ) {
        let trace_flow_id = page_request.trace_flow_id();
        let Some(read_range) = self.aligned_read_range_for_page_size(
            page_request.range(),
            window_alignment,
            page_size,
        ) else {
            return;
        };
        storage_trace::duration!(
            "storage",
            "File::read_single_window",
            "len" => read_range.end - read_range.start
        );

        enum ReadMode<'a> {
            Raw(u64),
            Decompress,
            Decrypt(&'a Arc<dyn Cipher>),
        }
        let mode = (read_range.start < self.uncompressed_size).then(|| match &self.transform {
            Transform::None => ReadMode::Raw(read_range.start),
            Transform::Compressed(info) => {
                match info.raw_offset_for_uncompressed_range(&read_range, BLOCK_SIZE) {
                    Some(offset) => ReadMode::Raw(offset),
                    None => ReadMode::Decompress,
                }
            }
            Transform::Encrypted(cipher) => ReadMode::Decrypt(cipher),
        });
        // Only flush intermediate 32 KiB chunks when decompressing LZ4, where decompressing a
        // chunk is faster than SHA-256 verification so `pkg-cache` verifies chunks in one
        // continuous wake cycle. For ZSTD (where decompression is much slower than SHA-256) and
        // raw/encrypted reads, wait until the full window is ready to avoid repeated wake/sleep
        // cycles and partial-window supplies that trigger duplicate readahead faults.
        let flush_alignment = match (&self.transform, &mode) {
            (Transform::Compressed(info), Some(ReadMode::Decompress))
                if info.compression_algorithm() == CompressionAlgorithm::Lz4 =>
            {
                NonZero::new(info.chunk_size() as usize)
            }
            _ => None,
        };
        if page_request.prepare(read_range.clone(), flush_alignment).is_err() {
            return;
        }

        for offset in (read_range.start..read_range.end).step_by(window_alignment as usize) {
            record_offset(offset);
        }

        let Some(mode) = mode else {
            let len = (read_range.end - read_range.start) as usize;
            page_request.mut_ptr_slice().subslice_mut(0..len).fill(0);
            let _ = page_request.commit(len);
            return;
        };

        // Obtain the deferred VMAR pre-commit handle before moving `page_request` into the
        // completion closure. `read_aligned_range_with_precommit` executes `precommit.run()` on
        // this thread right after the first asynchronous block read is submitted to overlap
        // `zx::VmarOp::COMMIT` with storage DMA.
        let precommit = page_request.precommit();
        let forwards_trace_flow = page_request.forwards_trace_flow();
        // Wrap `service` so every submitted `read_blocks` sub-request holds `Arc<File>` (and any
        // attached `SlotGuard`) until its physical read callback finishes, even if
        // `SplittableBuffer` or `ReadContext` aborts early on a sibling extent or chunk error.
        let guarded_service = GuardedBlockService::new(service, self.clone());
        let service = &guarded_service;

        match mode {
            ReadMode::Raw(raw_offset) => {
                let uncompressed_size = self.uncompressed_size;
                let stored_len = min(read_range.end, uncompressed_size)
                    .saturating_sub(read_range.start)
                    .next_multiple_of(BLOCK_SIZE);
                let mut current_offset = read_range.start;
                let stored_end_offset = read_range.start + stored_len;
                let stored_range = raw_offset..raw_offset + stored_len;

                read_aligned_range_with_precommit(
                    &self.extents,
                    stored_range,
                    service,
                    precommit,
                    move |res| {
                        let buffer = match res {
                            Ok(buffer) => buffer,
                            Err(error) => {
                                storage_trace::duration!("storage", "blob-read-raw", "len" => 0u64);
                                trace_read_callback_flow(trace_flow_id, true);
                                if !is_cancelled(&error) {
                                    log::error!(error:?; "Failed to read blocks for mapped file");
                                }
                                return ControlFlow::Break(());
                            }
                        };
                        storage_trace::duration!(
                            "storage",
                            "blob-read-raw",
                            "len" => buffer.len() as u64
                        );
                        let is_final_chunk =
                            current_offset + buffer.len() as u64 >= stored_end_offset;
                        trace_read_callback_flow(
                            trace_flow_id,
                            !forwards_trace_flow && is_final_chunk,
                        );
                        let valid_len = min(
                            buffer.len() as u64,
                            uncompressed_size.saturating_sub(current_offset),
                        ) as usize;
                        current_offset += buffer.len() as u64;
                        let commit_len = if current_offset == stored_end_offset
                            && current_offset < read_range.end
                        {
                            let total_len =
                                buffer.len() + (read_range.end - current_offset) as usize;
                            current_offset = read_range.end;
                            total_len
                        } else {
                            buffer.len()
                        };
                        let dest = page_request.mut_ptr_slice().subslice_mut(0..commit_len);
                        let (mut head, mut tail) = dest.split_at_mut(valid_len);
                        head.copy_from_ptr_slice(buffer.as_ptr_slice().subslice(0..valid_len));
                        drop(buffer);
                        tail.fill(0);
                        if page_request.commit(commit_len).is_err() {
                            return ControlFlow::Break(());
                        }
                        ControlFlow::Continue(())
                    },
                );
            }
            ReadMode::Decompress => {
                let Ok((mut decompressor, aligned_range)) = StreamingDecompressor::new(
                    FileCompression(self.clone()),
                    self.uncompressed_size,
                    page_request,
                ) else {
                    // The range must be out of range. This should be handled when `page_request`
                    // is dropped.
                    return;
                };
                let mut remaining_compressed = aligned_range.end - aligned_range.start;

                read_aligned_range_with_precommit(
                    &self.extents,
                    aligned_range,
                    service,
                    precommit,
                    move |res| {
                        let buffer = match res {
                            Ok(buffer) => buffer,
                            Err(error) => {
                                storage_trace::duration!(
                                    "storage",
                                    "blob-decompress",
                                    "len" => 0u64
                                );
                                trace_read_callback_flow(trace_flow_id, true);
                                if !is_cancelled(&error) {
                                    log::error!(
                                        error:?;
                                        "Failed to read blocks for compressed mapped file"
                                    );
                                }
                                return ControlFlow::Break(());
                            }
                        };
                        storage_trace::duration!(
                            "storage",
                            "blob-decompress",
                            "len" => buffer.len() as u64
                        );
                        remaining_compressed =
                            remaining_compressed.saturating_sub(buffer.len() as u64);
                        trace_read_callback_flow(
                            trace_flow_id,
                            !forwards_trace_flow && remaining_compressed == 0,
                        );
                        let res = decompressor.push(buffer.as_ptr_slice());
                        if res.is_err() {
                            return ControlFlow::Break(());
                        }
                        ControlFlow::Continue(())
                    },
                );
            }
            ReadMode::Decrypt(cipher) => {
                let mut current_offset = read_range.start;
                let end_offset = read_range.end;
                let uncompressed_size = self.uncompressed_size;
                let cipher = cipher.clone();

                read_aligned_range_with_precommit(
                    &self.extents,
                    read_range,
                    service,
                    precommit,
                    move |res| {
                        let buffer = match res {
                            Ok(buffer) => buffer,
                            Err(error) => {
                                storage_trace::duration!("storage", "blob-decrypt", "len" => 0u64);
                                trace_read_callback_flow(trace_flow_id, true);
                                if !is_cancelled(&error) {
                                    log::error!(
                                        error:?;
                                        "Failed to read blocks for encrypted mapped file"
                                    );
                                }
                                return ControlFlow::Break(());
                            }
                        };
                        let buffer_len = buffer.len();
                        storage_trace::duration!(
                            "storage",
                            "blob-decrypt",
                            "len" => buffer_len as u64
                        );
                        let is_final_chunk = current_offset + buffer_len as u64 >= end_offset;
                        trace_read_callback_flow(
                            trace_flow_id,
                            !forwards_trace_flow && is_final_chunk,
                        );
                        let mut dest = page_request.mut_ptr_slice().subslice_mut(0..buffer_len);

                        // Both `buffer` and `dest` are aligned to 64 bytes.
                        let decrypt_res = cipher.decrypt_to(
                            0,
                            0,
                            0,
                            current_offset,
                            buffer.as_ptr_slice(),
                            dest.reborrow(),
                        );
                        drop(buffer);
                        if let Err(error) = decrypt_res {
                            log::error!(error:?; "Failed to decrypt buffer");
                            return ControlFlow::Break(());
                        }
                        let valid_len = min(
                            buffer_len as u64,
                            uncompressed_size.saturating_sub(current_offset),
                        ) as usize;
                        dest.subslice_mut(valid_len..buffer_len).fill(0);
                        if page_request.commit(buffer_len).is_err() {
                            return ControlFlow::Break(());
                        }
                        current_offset += buffer_len as u64;
                        ControlFlow::Continue(())
                    },
                );
            }
        }
    }

    /// Services a page request for a child file (`self`, identified by `child_key`) whose extents
    /// map to logical byte offsets in `parent_file`.
    ///
    /// Expands `original_range` to `self.read_ahead_size()` and iterates over the overlapping
    /// child extents:
    /// - [`DeviceExtent::Sparse`]: notifies `delivery_handler` via
    ///   [`DeliveryHandler::deliver_child_sparse`] to zero-fill the child slice.
    /// - [`DeviceExtent::Block`]: reads the corresponding byte range of `parent_file` aligned to
    ///   `parent_file.verification_alignment()` so the downstream verifier can verify the parent
    ///   chunks before supplying the child VMO pages.
    /// - [`DeviceExtent::InlineTail`]: enqueues a header-only command via
    ///   [`DeliveryHandler::deliver_child_inline_tail`] so the verifier can copy the tail bytes
    ///   directly from the verified parent VMO without performing additional block I/O.
    ///
    /// Returns `true` if at least one asynchronous block read was initiated.
    pub fn read_child_range<D: DeliveryHandler>(
        self: &Arc<Self>,
        child_key: u64,
        original_range: Range<u64>,
        mut trace_flow_id: u64,
        parent_file: &Arc<File>,
        service: &(impl BlockService + ?Sized),
        delivery_handler: &Arc<D>,
    ) -> bool {
        let Some(child_read_range) =
            self.aligned_read_range(original_range, self.read_ahead_size())
        else {
            return false;
        };
        storage_trace::duration!(
            "storage",
            "File::read_child_range",
            "key" => child_key,
            "offset" => child_read_range.start,
            "len" => child_read_range.end - child_read_range.start
        );
        let parent_align = parent_file.verification_alignment();
        let has_trace_flow = trace_flow_id != 0;

        let mut started_async_io = false;
        for extent in self
            .extents
            .iter_extents(child_read_range.start)
            .take_while(|e| e.logical_range().start < child_read_range.end)
        {
            let slice_start = std::cmp::max(extent.logical_range().start, child_read_range.start);
            let slice_end = min(extent.logical_range().end, child_read_range.end);
            if slice_start >= slice_end {
                continue;
            }
            let slice_len = slice_end - slice_start;
            let extent_flow_id = if trace_flow_id != 0 {
                std::mem::take(&mut trace_flow_id)
            } else if has_trace_flow {
                begin_page_fault_flow()
            } else {
                0
            };
            match extent.device_extent() {
                crate::DeviceExtent::Sparse => {
                    let _ = delivery_handler.deliver_child_sparse(
                        child_key,
                        slice_start,
                        slice_len,
                        extent_flow_id,
                    );
                }
                crate::DeviceExtent::Block(dev_offset) => {
                    let parent_start = dev_offset + (slice_start - extent.logical_range().start);
                    let child_req = delivery_handler
                        .get_child_page_request(
                            child_key,
                            slice_start,
                            parent_start..(parent_start + slice_len),
                        )
                        .with_trace_flow_id(extent_flow_id);
                    parent_file.read_range_with_alignment(parent_align, service, child_req);
                    started_async_io = true;
                }
                crate::DeviceExtent::InlineTail(dev_offset) => {
                    let tail_end_in_file = min(extent.logical_range().end, self.uncompressed_size);
                    let tail_length = tail_end_in_file.saturating_sub(slice_start) as u32;
                    delivery_handler.deliver_child_inline_tail(
                        dev_offset,
                        child_key,
                        slice_start,
                        tail_length,
                        extent_flow_id,
                    );
                }
            }
        }
        started_async_io
    }
}

struct LoadingSlot<R> {
    requests: Vec<R>,
    waiters: Vec<oneshot::Sender<Arc<File>>>,
}

impl<R> Default for LoadingSlot<R> {
    fn default() -> Self {
        Self { requests: Vec::new(), waiters: Vec::new() }
    }
}

enum FileState<R> {
    Loading(LoadingSlot<R>),
    Loaded(Arc<File>),
}

/// A pre-allocated reservation for registering a blob's Merkle tree leaf hashes with the
/// downstream verifier.
///
/// Reserving space upfront on the submission thread before issuing metadata block reads ensures
/// that all paths acquire resources in a consistent order (`delivery_vmo` -> `BufferAllocator`)
/// and avoids blocking block-completion threads on `delivery_vmo` space.
pub trait BlobRegistration: Send + 'static {
    /// Commits the blob registration with `merkle_leaves` and `read_alignment`.
    fn commit(self, merkle_leaves: &[[u8; 32]], read_alignment: usize) -> Result<(), Error>;
}

impl BlobRegistration for () {
    fn commit(self, _merkle_leaves: &[[u8; 32]], _read_alignment: usize) -> Result<(), Error> {
        Ok(())
    }
}

/// Trait for handling deliveries (page request buffers and blob registrations) from [`Files`].
pub trait DeliveryHandler: Send + Sync + 'static {
    type Request: PageRequest;
    type BlobRegistration: BlobRegistration;

    /// Produces a [`PageRequest`] to receive decompressed or uncompressed page data.
    fn get_page_request(self: &Arc<Self>, key: u64, range: Range<u64>) -> Self::Request;

    /// Produces a [`PageRequest`] to receive parent file data for a child file mapping.
    ///
    /// `parent_byte_range` is the unaligned slice within the parent file that backs the child
    /// extent starting at page-aligned `child_target_offset` in `child_key`. The returned
    /// [`PageRequest`] will be prepared and committed with chunks aligned to the parent file's
    /// verification alignment.
    fn get_child_page_request(
        self: &Arc<Self>,
        _child_key: u64,
        _child_target_offset: u64,
        _parent_byte_range: Range<u64>,
    ) -> Self::Request {
        unreachable!()
    }

    /// Enqueues a header-only `ChildInlineTail` command instructing the downstream verifier to
    /// supply a single inline-tail page at `child_target_offset` in `child_key` by copying
    /// `tail_length` bytes directly from `parent_byte_offset` in the verified parent VMO.
    fn deliver_child_inline_tail(
        self: &Arc<Self>,
        _parent_byte_offset: u64,
        _child_key: u64,
        _child_target_offset: u64,
        _tail_length: u32,
        _trace_flow_id: u64,
    ) {
        unreachable!()
    }

    /// Delivers a sparse (zero-filled) range of `child_length` bytes at `child_target_offset` in
    /// `child_key`.
    fn deliver_child_sparse(
        &self,
        _child_key: u64,
        _child_target_offset: u64,
        _child_length: u64,
        _trace_flow_id: u64,
    ) -> Result<(), Error> {
        unreachable!()
    }

    /// Reserves space to register a blob (`key`) with at most `max_leaf_bytes` of Merkle tree leaf
    /// hashes with the downstream verifier.
    fn prepare_register_blob(
        &self,
        key: u64,
        max_leaf_bytes: usize,
    ) -> Result<Self::BlobRegistration, Error>;

    /// Unregisters a file when it is closed.
    fn unregister_file(&self, _key: u64) {}
}

/// A no-op [`DeliveryHandler`] for sessions that do not run a kernel pager or verify blobs
/// (such as intermediate partition mapping sessions).
pub struct NoopDeliveryHandler;

impl DeliveryHandler for NoopDeliveryHandler {
    type Request = NullPageRequest;
    type BlobRegistration = ();

    fn get_page_request(self: &Arc<Self>, _key: u64, _range: Range<u64>) -> NullPageRequest {
        NullPageRequest
    }

    fn prepare_register_blob(
        &self,
        _key: u64,
        _max_leaf_bytes: usize,
    ) -> Result<Self::BlobRegistration, Error> {
        Ok(())
    }
}

const PAGER_SHUTDOWN_KEY: u64 = 0;
const PAGER_WAKE_KEY: u64 = 1;
const PAGER_THREAD_ROLE: &str = "fuchsia.storage.blob.delivery";

/// An RAII guard for the background pager thread spawned by [`Files::spawn_pager_thread`].
///
/// The thread services page requests for as long as this guard is kept alive, and stops
/// when this guard is dropped.
#[must_use = "Dropping PagerThread immediately stops the background pager thread"]
pub struct PagerThread<S: ?Sized, D: DeliveryHandler> {
    files: Arc<Files<S, D>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<S: ?Sized, D: DeliveryHandler> Drop for PagerThread<S, D> {
    fn drop(&mut self) {
        let packet = zx::Packet::from_user_packet(
            PAGER_SHUTDOWN_KEY,
            0,
            zx::UserPacket::from_u8_array([0; 32]),
        );
        let _ = self.files.port.queue(&packet);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// Upper bound on the profile VMO size to prevent unbounded memory growth if a recording runs for a
// long time, since entries are appended without deduplication.
const MAX_PROFILE_VMO_SIZE: u64 = 64 * 1024 * 1024;

struct ProfileRecording {
    vmo: zx::Vmo,
    vmo_offset: u64,
    buffer: Vec<u8>,
}

impl ProfileRecording {
    fn new() -> Result<Self, zx::Status> {
        let vmo = zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 0)?;
        Ok(Self { vmo, vmo_offset: 0, buffer: Vec::with_capacity(BLOCK_SIZE as usize) })
    }

    fn record(&mut self, key: u64, offset: u64) {
        const MSG_SIZE: usize = std::mem::size_of::<FileMessage>();
        // `FileMessage::decode_from_vmo` decodes in `BLOCK_SIZE` chunks and expects messages not
        // to cross `BLOCK_SIZE` boundaries.
        const { assert!((BLOCK_SIZE as usize).is_multiple_of(MSG_SIZE)) };

        if self.vmo_offset >= MAX_PROFILE_VMO_SIZE {
            return;
        }
        let mut buf = [0u8; MSG_SIZE];
        FileMessage { id: key, offset }.encode_to((&mut buf[..]).into());
        self.buffer.extend_from_slice(&buf);
        if self.buffer.len() == BLOCK_SIZE as usize
            && let Err(error) = self.flush()
        {
            log::warn!(error:?; "Failed to flush profile buffer to VMO");
        }
    }

    fn flush(&mut self) -> Result<(), zx::Status> {
        let new_size = self.vmo_offset + BLOCK_SIZE;
        let res = self.vmo.set_size(new_size).and_then(|()| {
            self.vmo.write(&self.buffer, self.vmo_offset)?;
            self.vmo_offset = new_size;
            Ok(())
        });
        self.buffer.clear();
        res
    }

    fn finish(mut self) -> Result<zx::Vmo, zx::Status> {
        if !self.buffer.is_empty() {
            self.flush()?;
        }
        Ok(self.vmo)
    }
}

struct FileEntry<R> {
    // Sequence number of the latest `Mappings` command for this key (`0` if created by a page
    // request before the first `Mappings` command arrived).
    generation: u64,
    state: FileState<R>,
}

struct FilesState<R> {
    map: HashMap<u64, FileEntry<R>>,
    next_generation: u64,
}

/// A thread-safe registry of active [`File`] instances indexed by their Zircon pager port key.
pub struct Files<S: ?Sized, D: DeliveryHandler> {
    service: Arc<S>,
    delivery_handler: Arc<D>,
    port: zx::Port,
    // When `Some((parent_files, parent_key))`, this registry belongs to a child mapper session
    // whose files map logical offsets into the parent file registered under `parent_key` in
    // `parent_files`.
    parent: Option<(Arc<Files<S, D>>, u64)>,
    state: Mutex<FilesState<D::Request>>,
    // Page requests queued while a file's metadata was still loading, paired with
    // `(key, file, req)` to be dispatched on `PagerThread` once metadata loading completes.
    deferred_page_requests: Mutex<Vec<(u64, Arc<File>, D::Request)>>,
    // Active profile recording, if one is in progress.
    recording: Mutex<Option<ProfileRecording>>,
}

impl<S: BlockService + ?Sized + 'static, D: DeliveryHandler> Files<S, D> {
    /// Creates a new file registry with the provided block service, delivery handler, and port.
    pub fn new(service: Arc<S>, delivery_handler: D, port: zx::Port) -> Self {
        Self::new_with_shared_handler(service, Arc::new(delivery_handler), port, None)
    }

    /// Creates a new file registry sharing an existing `Arc<D>` delivery handler, optionally
    /// mapped as a child session over `(parent_files, parent_key)`.
    pub fn new_with_shared_handler(
        service: Arc<S>,
        delivery_handler: Arc<D>,
        port: zx::Port,
        parent: Option<(Arc<Files<S, D>>, u64)>,
    ) -> Self {
        Self {
            service,
            delivery_handler,
            port,
            parent,
            state: Mutex::new(FilesState { map: HashMap::new(), next_generation: 1 }),
            deferred_page_requests: Mutex::new(Vec::new()),
            recording: Mutex::new(None),
        }
    }

    /// Starts recording a profile of page requests serviced by `self`.
    ///
    /// While active, each serviced page request records a [`FileMessage`] containing the file's
    /// Zircon pager port `key` and the readahead-aligned uncompressed logical byte `offset` within
    /// that file.
    ///
    /// Returns `Err(zx::Status::BAD_STATE)` if a recording is already active.
    pub fn start_recording(&self) -> Result<(), zx::Status> {
        let mut recording = self.recording.lock();
        if recording.is_some() {
            return Err(zx::Status::BAD_STATE);
        }
        *recording = Some(ProfileRecording::new()?);
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().is_some()
    }

    /// Stops the active profile recording and returns a VMO containing the recorded
    /// [`FileMessage`] items.
    ///
    /// Returns `Err(zx::Status::BAD_STATE)` if no recording is active.
    pub fn stop_recording(&self) -> Result<zx::Vmo, zx::Status> {
        let recording = self.recording.lock().take().ok_or(zx::Status::BAD_STATE)?;
        recording.finish()
    }

    /// Aborts any active profile recording.
    pub fn abort_recording(&self) {
        *self.recording.lock() = None;
    }

    /// Returns a reference to the [`DeliveryHandler`] backing this registry.
    pub fn delivery_handler(&self) -> &Arc<D> {
        &self.delivery_handler
    }

    /// Returns `true` if this registry belongs to a child session whose files map logical byte
    /// ranges within a parent file.
    pub fn has_parent(&self) -> bool {
        self.parent.is_some()
    }

    /// Spawns a background thread with `name` to service pager requests for `self`.
    pub fn spawn_pager_thread(self: &Arc<Self>, name: &str) -> PagerThread<S, D> {
        let files = self.clone();
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                if let Err(error) = fuchsia_scheduler::set_role_for_this_thread(PAGER_THREAD_ROLE) {
                    log::warn!(error:?; "Failed to set pager thread role");
                }
                files.run_pager_loop();
            })
            .expect("failed to spawn pager thread");
        PagerThread { files: self.clone(), thread: Some(thread) }
    }

    // Dispatches a `D::Request` to either `file.read_child_range` (for child sessions) or
    // `file.read_range_with_recording` (for root sessions).
    fn dispatch_page_request(&self, key: u64, file: &Arc<File>, req: D::Request) {
        if let Some((parent_files, parent_key)) = &self.parent {
            let Some(parent_file) = parent_files.get_file(*parent_key) else {
                log::warn!(
                    key,
                    parent_key = *parent_key;
                    "Parent file is no longer mapped for child page request"
                );
                return;
            };
            let _ = file.read_child_range(
                key,
                req.range(),
                req.trace_flow_id(),
                &parent_file,
                self.service.as_ref(),
                &self.delivery_handler,
            );
        } else {
            file.read_range_with_recording(
                file.read_ahead_size(),
                self.service.as_ref(),
                req,
                |offset| {
                    if let Some(recording) = self.recording.lock().as_mut() {
                        recording.record(key, offset);
                    }
                },
            );
        }
    }

    // Runs a synchronous event loop that listens on `self.port` for pager page requests and
    // dispatches them to `Self::handle_page_request`.
    fn run_pager_loop(self: &Arc<Self>) {
        while let Ok(packet) = self.port.wait(zx::MonotonicInstant::INFINITE) {
            match packet.contents() {
                zx::PacketContents::User(_) => match packet.key() {
                    PAGER_SHUTDOWN_KEY => break,
                    PAGER_WAKE_KEY => {
                        let requests = std::mem::take(&mut *self.deferred_page_requests.lock());
                        for (key, file, req) in requests {
                            let range = req.range();
                            storage_trace::duration!(
                                "storage",
                                "Files::dispatch_deferred_page_request",
                                "key" => key,
                                "offset" => range.start,
                                "len" => range.end - range.start
                            );
                            let trace_flow_id = req.trace_flow_id();
                            if trace_flow_id != 0 {
                                storage_trace::flow_step!(
                                    "storage",
                                    "page_fault",
                                    trace_flow_id.into()
                                );
                            }
                            self.dispatch_page_request(key, &file, req);
                        }
                    }
                    _ => {}
                },
                zx::PacketContents::Pager(pager_packet)
                    if pager_packet.command() == ZX_PAGER_VMO_READ =>
                {
                    self.handle_page_request(packet.key(), pager_packet.range());
                }
                _ => {}
            }
        }
    }

    /// Returns a reference to the block service.
    pub fn service(&self) -> &Arc<S> {
        &self.service
    }

    /// Reserves space with the downstream delivery handler to register `key` with at most
    /// `max_leaf_bytes` of Merkle tree leaf hashes.
    pub fn prepare_register_blob(
        &self,
        key: u64,
        max_leaf_bytes: usize,
    ) -> Result<D::BlobRegistration, Error> {
        self.delivery_handler.prepare_register_blob(key, max_leaf_bytes)
    }

    // Handles a page request from `PagerThread`.
    //
    // If the file is loaded, reads the range into a newly allocated buffer immediately.
    // If the file is currently loading or unmapped, queues the request to be fulfilled
    // when loaded.
    fn handle_page_request(&self, key: u64, range: Range<u64>) {
        storage_trace::duration!(
            "storage",
            "Files::handle_page_request",
            "key" => key,
            "offset" => range.start,
            "len" => range.end - range.start
        );
        let trace_flow_id = begin_page_fault_flow();
        let req =
            self.delivery_handler.get_page_request(key, range).with_trace_flow_id(trace_flow_id);
        let mut state = self.state.lock();
        match state.map.entry(key) {
            Entry::Occupied(mut entry) => match &mut entry.get_mut().state {
                FileState::Loaded(file) => {
                    let file = Arc::clone(file);
                    drop(state);
                    self.dispatch_page_request(key, &file, req);
                }
                FileState::Loading(slot) => {
                    slot.requests.push(req);
                }
            },
            Entry::Vacant(entry) => {
                // Page requests can arrive before the corresponding `Mappings` command
                // is processed. We create a loading entry and queue the request so it can be
                // serviced once the mapping arrives and metadata finishes loading.
                //
                // Potential weakness: Unknown keys are unbounded. If an invalid or bogus page
                // request arrives for a key that is never mapped, this entry will remain in
                // memory until the session is dropped.
                entry.insert(FileEntry {
                    generation: 0,
                    state: FileState::Loading(LoadingSlot {
                        requests: vec![req],
                        waiters: Vec::new(),
                    }),
                });
            }
        }
    }

    #[cfg(test)]
    pub fn begin_loading(&self, key: u64) {
        let _ = self.begin_loading_with_slot_guard(key, None);
    }

    // Marks `key` as currently loading metadata (or advances its generation if already present so
    // an existing `Loaded` entry remains readable while replacement metadata loads), preserving
    // any page requests that arrived prior to the mapping command.
    fn begin_loading_with_slot_guard(
        &self,
        key: u64,
        slot_guard: Option<&Arc<SlotGuard<RawMappingCommand>>>,
    ) -> u64 {
        let mut state = self.state.lock();
        let generation = state.next_generation;
        state.next_generation += 1;
        match state.map.entry(key) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                entry.generation = generation;
                if let FileState::Loaded(old_file) = &entry.state {
                    // Attach a clone of `slot_guard` immediately so that even if this overwrite's
                    // metadata read is later superseded (by `CloseBlob` or another overwrite) and
                    // completes before `old_file`'s in-flight block reads finish, this FIFO slot
                    // will not retire until `old_file` is also dropped.
                    old_file.attach_slot_guard(slot_guard.cloned());
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(FileEntry {
                    generation,
                    state: FileState::Loading(LoadingSlot::default()),
                });
            }
        }
        generation
    }

    /// Inserts a file into the registry under `key` and dispatches any page requests that
    /// arrived while metadata was loading.
    #[cfg(test)]
    pub(crate) fn insert(&self, key: u64, file: Arc<File>) {
        self.insert_with_slot_guard(key, file, None);
    }

    fn insert_with_slot_guard(
        &self,
        key: u64,
        file: Arc<File>,
        slot_guard: Option<Arc<SlotGuard<RawMappingCommand>>>,
    ) {
        let (reqs, waiters, old_file) = {
            let mut state = self.state.lock();
            let generation = state.next_generation;
            state.next_generation += 1;
            let prev = state
                .map
                .insert(key, FileEntry { generation, state: FileState::Loaded(file.clone()) });
            match prev.map(|entry| entry.state) {
                Some(FileState::Loading(slot)) => (slot.requests, slot.waiters, None),
                Some(FileState::Loaded(old_file)) => (Vec::new(), Vec::new(), Some(old_file)),
                None => (Vec::new(), Vec::new(), None),
            }
        };
        if let Some(old_file) = old_file {
            old_file.attach_slot_guard(slot_guard);
        } else {
            drop(slot_guard);
        }
        self.finish_insert(key, file, reqs, waiters);
    }

    fn commit_loading(
        &self,
        key: u64,
        generation: u64,
        file: Arc<File>,
        registration: Option<D::BlobRegistration>,
        merkle_leaves: &[[u8; 32]],
    ) {
        // Child files do not have their own Merkle trees, and overwritten root blobs (`Loaded`)
        // keep their existing verifier initialised on initial open. Check `generation` and whether
        // initial registration is needed before committing `registration` outside the lock.
        let Some(needs_registration) = ({
            let state = self.state.lock();
            match state.map.get(&key) {
                Some(entry) if entry.generation == generation => {
                    Some(matches!(entry.state, FileState::Loading(_)) && !self.has_parent())
                }
                _ => None,
            }
        }) else {
            return;
        };
        if needs_registration
            && let Some(registration) = registration
            && let Err(error) =
                registration.commit(merkle_leaves, file.verification_alignment() as usize)
        {
            log::error!(error:?, key; "Failed to register blob");
            self.abort_loading(key, generation);
            return;
        }
        let (reqs, waiters, old_file) = {
            let mut state = self.state.lock();
            let Some(entry) = state.map.get_mut(&key) else {
                return;
            };
            if entry.generation != generation {
                return;
            }
            match std::mem::replace(&mut entry.state, FileState::Loaded(file.clone())) {
                FileState::Loading(slot) => (slot.requests, slot.waiters, None),
                // `begin_loading_with_slot_guard` already attached `slot_guard` to `old_file`.
                FileState::Loaded(old_file) => (Vec::new(), Vec::new(), Some(old_file)),
            }
        };
        drop(old_file);
        self.finish_insert(key, file, reqs, waiters);
    }

    fn abort_loading(&self, key: u64, generation: u64) {
        let removed = {
            let mut state = self.state.lock();
            match state.map.entry(key) {
                Entry::Occupied(entry) if entry.get().generation == generation => {
                    Some(entry.remove())
                }
                _ => None,
            }
        };
        if let Some(removed) = removed {
            drop(removed);
            if self.parent.is_none() {
                self.delivery_handler.unregister_file(key);
            }
        }
    }

    fn finish_insert(
        &self,
        key: u64,
        file: Arc<File>,
        reqs: Vec<D::Request>,
        waiters: Vec<oneshot::Sender<Arc<File>>>,
    ) {
        for waiter in waiters {
            let _ = waiter.send(file.clone());
        }

        if !reqs.is_empty() {
            // `insert` / `commit_loading` can be called inside a block I/O completion
            // callback. Calling `file.read_range` here would deadlock a single-threaded block
            // completion worker when a read requires multiple buffer allocations from a full pool:
            // allocating the next chunk blocks the completion thread waiting for earlier chunks to
            // complete and free their buffers, which that same thread is responsible for
            // completing. Instead, hand `reqs` to `PagerThread` so the callback can return
            // immediately.
            self.deferred_page_requests
                .lock()
                .extend(reqs.into_iter().map(|req| (key, file.clone(), req)));
            let packet = zx::Packet::from_user_packet(
                PAGER_WAKE_KEY,
                0,
                zx::UserPacket::from_u8_array([0; 32]),
            );
            let _ = self.port.queue(&packet);
        }
    }

    /// Returns the loaded [`File`] registered under `key`, if present.
    pub fn get_file(&self, key: u64) -> Option<Arc<File>> {
        let state = self.state.lock();
        match state.map.get(&key).map(|entry| &entry.state) {
            Some(FileState::Loaded(file)) => Some(file.clone()),
            _ => None,
        }
    }

    /// Returns a future that completes with the loaded [`File`] registered under `key`,
    /// waiting asynchronously if the file is currently loading or hasn't arrived yet.
    pub async fn wait_for_file(&self, key: u64) -> Result<Arc<File>, Error> {
        let receiver = {
            let mut state = self.state.lock();
            match state.map.entry(key) {
                Entry::Occupied(mut entry) => match &mut entry.get_mut().state {
                    FileState::Loaded(file) => return Ok(file.clone()),
                    FileState::Loading(slot) => {
                        let (sender, receiver) = oneshot::channel();
                        slot.waiters.push(sender);
                        receiver
                    }
                },
                Entry::Vacant(entry) => {
                    let (sender, receiver) = oneshot::channel();
                    entry.insert(FileEntry {
                        generation: 0,
                        state: FileState::Loading(LoadingSlot {
                            requests: Vec::new(),
                            waiters: vec![sender],
                        }),
                    });
                    receiver
                }
            }
        };

        receiver.await.map_err(|_| anyhow!("File loading cancelled"))
    }

    /// Removes the file registered under `key`.
    pub fn remove(&self, key: u64) {
        self.remove_with_slot_guard(key, None);
    }

    fn remove_with_slot_guard(
        &self,
        key: u64,
        slot_guard: Option<Arc<SlotGuard<RawMappingCommand>>>,
    ) {
        let removed = self.state.lock().map.remove(&key);
        if let Some(FileEntry { state: FileState::Loaded(old_file), .. }) = removed {
            old_file.attach_slot_guard(slot_guard);
        } else {
            drop(removed);
            drop(slot_guard);
        }
        if self.parent.is_none() {
            self.delivery_handler.unregister_file(key);
        }
    }

    /// Returns `true` if `key` is currently in the loading state.
    #[cfg(test)]
    pub fn is_loading(&self, key: u64) -> bool {
        matches!(self.state.lock().map.get(&key).map(|e| &e.state), Some(FileState::Loading(_)))
    }

    /// Returns `true` if `key` is currently in the loading state with at least one queued page
    /// request.
    #[cfg(test)]
    pub fn has_queued_page_requests(&self, key: u64) -> bool {
        matches!(
            self.state.lock().map.get(&key).map(|e| &e.state),
            Some(FileState::Loading(slot)) if !slot.requests.is_empty()
        )
    }

    /// Returns `true` if `key` is currently in the loaded state.
    pub(crate) fn is_loaded(&self, key: u64) -> bool {
        matches!(self.state.lock().map.get(&key).map(|e| &e.state), Some(FileState::Loaded(_)))
    }
}

impl<S: BlockService + ?Sized + 'static> Files<S, NoopDeliveryHandler> {
    /// Creates a file registry without a pager for intermediate (e.g. partition) sessions.
    pub fn new_without_pager(service: Arc<S>) -> Self {
        Self::new(service, NoopDeliveryHandler, zx::Port::create())
    }
}

const MIN_BLOB_METADATA_VERSION: u32 = 53;

/// The latest version of `BlobMetadata` in Fxfs.
/// Eventually, we'll need to integrate Fxfs's code for upgrading data structures.
pub(crate) const BLOB_METADATA_VERSION: u32 = 64;

/// Deserializes versioned `BlobMetadata` from the raw on-disk bytes.
fn deserialize_blob_metadata(mut bytes: &[u8]) -> Result<BlobMetadata, anyhow::Error> {
    use bincode::Options;
    let options = bincode::DefaultOptions::new().allow_trailing_bytes();

    // Fxfs versions are a 24-bit major version followed by an 8-bit minor version.
    let version = bytes.read_u24::<LittleEndian>()?;
    let _minor_version = bytes.read_u8()?;

    // Unlike Fxfs, newer versions aren't rejected so that this doesn't need updating every time the
    // Fxfs version changes. Fxfs won't mount a filesystem that is newer than it.
    match version {
        MIN_BLOB_METADATA_VERSION..BLOB_METADATA_VERSION => {
            options.deserialize::<BlobMetadataV53>(bytes).map(Into::into)
        }
        BLOB_METADATA_VERSION.. => options.deserialize::<BlobMetadata>(bytes),
        _ => bail!(
            "Unsupported blob metadata version: {version} (expected >= {MIN_BLOB_METADATA_VERSION})"
        ),
    }
    .map_err(|e| anyhow!("Failed to deserialize BlobMetadata: {e:?}"))
}

/// Metadata decoded from on-disk storage describing a blob's uncompressed size, compression info,
/// and Merkle leaves. Compression info will be None if blob is uncompressed.
pub struct DecodedBlobMetadata {
    pub uncompressed_size: u64,
    pub compression_info: Option<CompressionInfo>,
    pub merkle_leaves: MerkleLeaves,
}

// Returns true if `error` was caused by `zx::Status::CANCELED` (e.g. when a mapper session is
// closed while a read is pending).
fn is_cancelled(error: &Error) -> bool {
    error.root_cause().downcast_ref::<zx::Status>() == Some(&zx::Status::CANCELED)
}

/// Reads blob metadata asynchronously from `metadata_extents` using `service`.
/// Once the metadata is retrieved, deserialized, and parsed, `callback` is invoked with
/// [`DecodedBlobMetadata`].
///
/// On failure or if the operation is aborted, `callback` is dropped without being invoked.
pub fn read_blob_metadata(
    service: &(impl BlockService + ?Sized),
    metadata_extents: &Extents,
    stored_data_size: u64,
    callback: impl FnOnce(DecodedBlobMetadata) + Send + 'static,
) {
    let total_metadata_len = metadata_extents.logical_len();
    if total_metadata_len == 0 {
        callback(DecodedBlobMetadata {
            uncompressed_size: stored_data_size,
            compression_info: None,
            merkle_leaves: MerkleLeaves::new(),
        });
        return;
    }

    let mut callback = Some(callback);
    let mut metadata_bytes = Some(Vec::with_capacity(total_metadata_len as usize));
    read_aligned_range(metadata_extents, 0..total_metadata_len, service, move |res| {
        let buffer = match res {
            Ok(buffer) => buffer,
            Err(error) => {
                if !is_cancelled(&error) {
                    log::error!(error:?; "Failed to read metadata blocks");
                }
                return ControlFlow::Break(());
            }
        };
        let bytes = metadata_bytes.as_mut().unwrap();
        let copy_len = min(buffer.len(), total_metadata_len as usize - bytes.len());
        buffer.as_ptr_slice().subslice(0..copy_len).append_to(bytes);
        drop(buffer);
        if bytes.len() < total_metadata_len as usize {
            return ControlFlow::Continue(());
        }

        let metadata_bytes = metadata_bytes.take().unwrap();
        let cb = callback.take().unwrap();
        let metadata = match deserialize_blob_metadata(&metadata_bytes) {
            Ok(metadata) => metadata,
            Err(error) => {
                log::error!(error:?; "Failed to deserialize BlobMetadata");
                return ControlFlow::Break(());
            }
        };

        let compression_info = match metadata.format.compression_info(stored_data_size) {
            Ok(compression_info) => compression_info,
            Err(error) => {
                log::error!(error:?; "Failed to parse CompressionInfo");
                return ControlFlow::Break(());
            }
        };
        cb(DecodedBlobMetadata {
            uncompressed_size: metadata.format.uncompressed_size().unwrap_or(stored_data_size),
            compression_info,
            merkle_leaves: metadata.merkle_leaves,
        });
        ControlFlow::Break(())
    });
}

/// RAII guard that manages the lifecycle of a file while its metadata is being fetched
/// asynchronously from storage.
///
/// For an initial open, the file entry in [`Files`] starts in [`FileState::Loading`], accumulating
/// incoming page requests in its queue. For an overwrite of an already-loaded file, the existing
/// [`FileState::Loaded`] entry stays in place until the new metadata finishes loading.
///
/// - On success: [`LoadingFileGuard::commit`] consumes the guard, registers the blob's Merkle
///   leaves on initial load, replaces any prior entry with the newly initialised [`File`]
///   (dropping its own [`SlotGuard`] reference while any replaced [`File`] retains the clone
///   attached in `begin_loading_with_slot_guard` until its in-flight reads finish), and dispatches
///   any queued page requests to [`PagerThread`].
/// - On failure or cancellation: If dropped without committing while `key` is still at
///   `generation`, `Drop` removes `key` from [`Files`], both when `key` is in
///   [`FileState::Loading`] (dropping any queued [`PageRequest`] objects) and when `key` holds
///   [`FileState::Loaded`] during a failed overwrite (since retiring the overwrite's [`SlotGuard`]
///   allows Fxfs to free the old file's extents).
struct LoadingFileGuard<S: BlockService + ?Sized + 'static, D: DeliveryHandler> {
    files: Arc<Files<S, D>>,
    key: u64,
    generation: u64,
    committed: bool,
    slot_guard: Arc<SlotGuard<RawMappingCommand>>,
}

impl<S: BlockService + ?Sized + 'static, D: DeliveryHandler> LoadingFileGuard<S, D> {
    fn new(
        files: Arc<Files<S, D>>,
        key: u64,
        slot_guard: Arc<SlotGuard<RawMappingCommand>>,
    ) -> Self {
        let generation = files.begin_loading_with_slot_guard(key, Some(&slot_guard));
        Self { files, key, generation, committed: false, slot_guard }
    }

    /// Commits `registration` (on initial load of a root blob) and stores the loaded file in the
    /// registry if `self.generation` is still current.
    fn commit(
        mut self,
        file: Arc<File>,
        registration: Option<D::BlobRegistration>,
        merkle_leaves: &[[u8; 32]],
    ) {
        self.committed = true;
        self.files.commit_loading(self.key, self.generation, file, registration, merkle_leaves);
    }
}

impl<S: BlockService + ?Sized + 'static, D: DeliveryHandler> Drop for LoadingFileGuard<S, D> {
    fn drop(&mut self) {
        if !self.committed {
            self.files.abort_loading(self.key, self.generation);
        }
    }
}

/// Processes a claimed raw mapping command (`RawMappingCommand`), decoding extent descriptors,
/// reading blob metadata from storage, registering Merkle leaves via `files`, inserting/removing
/// the file from `files`, and holding the command's [`SlotGuard`] until any asynchronous metadata
/// read completes and any replaced or removed [`File`] (including all in-flight block reads on it)
/// has been dropped.
pub fn process_mapping_command<S: BlockService + ?Sized + 'static, D: DeliveryHandler>(
    msg: Message<'_, RawMappingCommand>,
    receiver: &Arc<Receiver<RawMappingCommand>>,
    files: &Arc<Files<S, D>>,
) -> Result<(), Error> {
    let cmd = *msg;
    match MappingCommand::try_from(cmd)? {
        MappingCommand::Mappings {
            key,
            offset,
            stored_size,
            device_offset,
            metadata_count,
            extent_count,
            encrypted,
        } => {
            if encrypted && metadata_count > 0 {
                bail!("Encrypted files cannot have blob metadata");
            }
            let extent_bytes_len = (extent_count as usize)
                .checked_mul(8)
                .ok_or_else(|| anyhow!("Overflow calculating extent byte length"))?;
            let metadata_bytes_len = (metadata_count as usize)
                .checked_mul(8)
                .ok_or_else(|| anyhow!("Overflow calculating metadata extent byte length"))?;
            let key_bytes_len = if encrypted { ENCRYPTION_KEY_SIZE } else { 0 };
            let total_bytes_len = extent_bytes_len
                .checked_add(metadata_bytes_len)
                .and_then(|len| len.checked_add(key_bytes_len))
                .ok_or_else(|| anyhow!("Overflow calculating total extent byte length"))?;
            let payload_len: u32 = total_bytes_len
                .try_into()
                .map_err(|_| anyhow!("Extent byte length exceeds u32"))?;

            // `offset` comes from the client, so validate it before decoding. Extents are read as
            // `u64`s and the payload region is page aligned, so `offset` must be 8-byte aligned.
            if !offset.is_multiple_of(std::mem::align_of::<u64>() as u32) {
                bail!("Payload offset {offset} is not 8-byte aligned");
            }
            let payload_bytes = msg
                .try_payload_slice(offset, payload_len)
                .ok_or_else(|| anyhow!("Payload slice is out of bounds"))?;
            let data_bytes = payload_bytes.subslice(0..extent_bytes_len);
            let data_extents = Extents::from_encoded(data_bytes.iter_as::<u64>(), device_offset)
                .ok_or_else(|| anyhow!("Failed to decode data extents"))?;

            if encrypted {
                let key_bytes = payload_bytes.subslice(extent_bytes_len..total_bytes_len);
                let cipher_key = UnwrappedKey::new(key_bytes.to_vec());
                let cipher = Arc::new(FxfsCipher::new(&cipher_key)) as Arc<dyn Cipher>;
                let slot_guard = Arc::new(msg.into_slot_guard(Arc::clone(receiver)));
                let file =
                    Arc::new(File::new(data_extents, stored_size, Transform::Encrypted(cipher)));
                files.insert_with_slot_guard(key, file, Some(slot_guard));
                return Ok(());
            }

            let metadata_bytes = payload_bytes.subslice(extent_bytes_len..total_bytes_len);
            let metadata_extents =
                Extents::from_encoded(metadata_bytes.iter_as::<u64>(), device_offset)
                    .ok_or_else(|| anyhow!("Failed to decode metadata extents"))?;
            let max_leaf_bytes: usize = metadata_extents
                .logical_len()
                .try_into()
                .map_err(|_| anyhow!("Metadata byte length exceeds usize"))?;

            if max_leaf_bytes == 0 {
                let file = Arc::new(File::new(data_extents, stored_size, Transform::None));
                if !files.has_parent() && !files.is_loaded(key) {
                    files
                        .prepare_register_blob(key, 0)?
                        .commit(&[], file.verification_alignment() as usize)?;
                }
                let slot_guard = Arc::new(msg.into_slot_guard(Arc::clone(receiver)));
                files.insert_with_slot_guard(key, file, Some(slot_guard));
                return Ok(());
            }

            // Child files do not have their own Merkle trees, and overwritten root blobs (`Loaded`)
            // keep their existing verifier initialised on initial open. For initial root-blob
            // opens, preallocate the delivery VMO space required to register the blob's Merkle
            // leaves before mutating `files` or issuing the metadata block read so blocking
            // reservations stay off the block-completion thread, resources are acquired in
            // `delivery_vmo` -> `BufferAllocator` order, and `files` is left unchanged if
            // preallocation fails.
            //
            // We use `metadata_extents.logical_len()` as a conservative upper bound for
            // `max_leaf_bytes`: the on-disk metadata stores the uncompressed Merkle leaves plus
            // headers and (when compressed) seek offsets.
            let registration = if !files.has_parent() && !files.is_loaded(key) {
                Some(files.prepare_register_blob(key, max_leaf_bytes)?)
            } else {
                None
            };
            let slot_guard = Arc::new(msg.into_slot_guard(Arc::clone(receiver)));
            let guard = LoadingFileGuard::new(files.clone(), key, slot_guard);

            let service = files.service().clone();
            let guarded_service =
                GuardedBlockService::new(service.as_ref(), Arc::clone(&guard.slot_guard));
            read_blob_metadata(&guarded_service, &metadata_extents, stored_size, move |metadata| {
                let file = Arc::new(File::new(
                    data_extents,
                    metadata.uncompressed_size,
                    metadata.compression_info.into(),
                ));
                guard.commit(file, registration, &metadata.merkle_leaves);
            });
            Ok(())
        }
        MappingCommand::CloseBlob { key } => {
            let slot_guard = Arc::new(msg.into_slot_guard(Arc::clone(receiver)));
            files.remove_with_slot_guard(key, Some(slot_guard));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::tests::FakeBlockService;
    use crate::reader::{MAX_READ_BUFFER_SIZE, OwnedBuffer};
    use crate::testing::{TestDeliveryHandler, TestVecBuffer};
    use crate::{BLOCK_SIZE, Extent};
    use anyhow::Error;
    use bincode::Options;
    use blob_metadata::BlobFormat;
    use byteorder::WriteBytesExt;
    use delivery_blob::compression::{ChunkedArchive, ChunkedArchiveError, ChunkedArchiveOptions};
    use fuchsia_async as fasync;
    use fxfs_crypto::{Cipher, FxfsCipher, UnwrappedKey};
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
    use storage_ptr_slice::MutPtrByteSlice;

    fn serialize_metadata(metadata: &BlobMetadata) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.write_u32::<LittleEndian>(BLOB_METADATA_VERSION).unwrap();
        bincode::DefaultOptions::new()
            .allow_trailing_bytes()
            .serialize_into(&mut bytes, metadata)
            .unwrap();
        bytes
    }

    #[test]
    fn test_read_range_uncompressed() {
        let block_count = 8;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 255) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents = Extents::try_new([Extent::new(0..(8 * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8 * BLOCK_SIZE, Transform::None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(8 * BLOCK_SIZE));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, (8 * BLOCK_SIZE) as usize)]);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_compressed_zstd() {
        let uncompressed_size = 32768 * 2 + 1024;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 7) % 255) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, compression_info.into()));

        let dest_alloc_size = uncompressed_size.next_multiple_of(chunk_size);
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(dest_alloc_size, 0);
        file.read_range(&service, page_request);

        assert_eq!(
            rx.commits(),
            vec![
                (0, chunk_size),
                (chunk_size as u64, chunk_size),
                (chunk_size as u64 * 2, chunk_size)
            ]
        );
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
    }

    #[test]
    fn test_read_range_compressed_lz4_split_across_buffers() {
        let uncompressed_size = 32768 * 2;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 13) % 255) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);

        // Force a small block allocation limit (e.g. 4096 bytes) so that read_aligned_range
        // splits the compressed chunks across multiple consecutive OwnedBuffers!
        let service = FakeBlockService::new_with_cap(device_data, Some(4096));

        let extents =
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Lz4,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, compression_info.into()));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(rx.output(), uncompressed_data);
    }

    #[test]
    fn test_read_range_invalid_range_noop() {
        let service = FakeBlockService::new(vec![0u8; 8192]);
        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8192, Transform::None));

        let (page_request, rx) = TestVecBuffer::new_with_range(4096..4096);
        // start >= end should be a no-op returning Ok(())
        file.read_range(&service, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_file_getters() {
        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let uncompressed_size = 8192u64;

        let file_uncompressed = File::new(extents, uncompressed_size, Transform::None);
        assert_eq!(file_uncompressed.uncompressed_size(), 8192);
        assert!(matches!(file_uncompressed.transform(), Transform::None));
        assert!(file_uncompressed.compression_info().is_none());
        assert!(file_uncompressed.cipher().is_none());

        let compression_info =
            CompressionInfo::new(32768, 4096, &[0], CompressionAlgorithm::Zstd).unwrap();
        let file_compressed = File::new(
            Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap(),
            uncompressed_size,
            compression_info.into(),
        );
        assert!(matches!(file_compressed.transform(), Transform::Compressed(_)));
        assert!(file_compressed.compression_info().is_some());
        assert!(file_compressed.cipher().is_none());

        let key = UnwrappedKey::new(vec![0x42u8; 32]);
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&key));
        let file_encrypted = File::new(
            Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap(),
            uncompressed_size,
            cipher.into(),
        );
        assert!(matches!(file_encrypted.transform(), Transform::Encrypted(_)));
        assert!(file_encrypted.compression_info().is_none());
        assert!(file_encrypted.cipher().is_some());
    }

    #[test]
    fn test_read_range_block_service_error_returns_err() {
        struct FailingBlockService;
        impl BlockService for FailingBlockService {
            fn allocate_buffer(&self, max_len: usize) -> storage_device::buffer::OwnedBuffer {
                FakeBlockService::new(vec![0u8; max_len]).allocate_buffer(max_len)
            }
            fn read_blocks(
                &self,
                _device_offset: u64,
                _dest_buffer: storage_device::buffer::OwnedBuffer,
                _on_complete: Box<
                    dyn FnOnce(Result<storage_device::buffer::OwnedBuffer, Error>) + Send,
                >,
            ) -> Result<(), Error> {
                Err(anyhow::anyhow!("block read failure"))
            }
        }

        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8192, Transform::None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);

        file.read_range(&FailingBlockService, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_read_range_uncompressed_multi_chunk() {
        let block_count = 4;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 11) % 255) as u8;
        }
        // Force capping to 4096 bytes per buffer allocation so read_range processes
        // 4 separate chunks.
        let service = FakeBlockService::new_with_cap(expected_data.clone(), Some(4096));

        let extents =
            Extents::try_new([Extent::new(0..(block_count * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, block_count * BLOCK_SIZE, Transform::None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(block_count * BLOCK_SIZE));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits().len(), 4);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_uncompressed_unaligned_uncompressed_size() {
        let uncompressed_size = 5000u64;
        let mut expected_data = vec![0u8; 8192];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size, Transform::None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, 8192)]);
        assert_eq!(&rx.output()[..5000], &expected_data[..5000]);
    }

    #[test]
    fn test_read_range_uncompressed_readahead() {
        let total_blocks = 64; // 256 KiB
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 17) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, total_blocks * BLOCK_SIZE, Transform::None));

        // Request 1 block at offset 4096. Readahead should expand to 0..128 KiB.
        let (page_request, rx) = TestVecBuffer::new_with_range(4096..8192);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, TARGET_READ_AHEAD_SIZE as usize)]);
        assert_eq!(
            &rx.output()[..TARGET_READ_AHEAD_SIZE as usize],
            &expected_data[..TARGET_READ_AHEAD_SIZE as usize]
        );
    }

    #[test]
    fn test_read_range_uncompressed_readahead_second_window() {
        let total_blocks = 64; // 256 KiB
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 19) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, total_blocks * BLOCK_SIZE, Transform::None));

        // Request 1 block at offset 132 KiB (135168..139264).
        // Readahead should expand to 128 KiB..256 KiB (131072..262144).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(TARGET_READ_AHEAD_SIZE, TARGET_READ_AHEAD_SIZE as usize)]);
        assert_eq!(
            &rx.output()[..TARGET_READ_AHEAD_SIZE as usize],
            &expected_data[TARGET_READ_AHEAD_SIZE as usize..2 * TARGET_READ_AHEAD_SIZE as usize]
        );
    }

    #[test]
    fn test_read_range_uncompressed_readahead_tail_capped() {
        let uncompressed_size = 140_000u64;
        let total_blocks = (uncompressed_size.next_multiple_of(BLOCK_SIZE) / BLOCK_SIZE) as u64;
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for i in 0..uncompressed_size as usize {
            expected_data[i] = ((i * 23) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size, Transform::None));

        // Request 1 block at offset 132 KiB (135168..139264).
        // Readahead window starts at 128 KiB (131072) and would normally extend to 256 KiB
        // (262144), but should be capped at page_aligned_size (143360).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        let expected_start = TARGET_READ_AHEAD_SIZE;
        let page_aligned_size = uncompressed_size.next_multiple_of(BLOCK_SIZE);
        let expected_len = (page_aligned_size - expected_start) as usize;
        assert_eq!(rx.commits(), vec![(expected_start, expected_len)]);

        let valid_len = (uncompressed_size - expected_start) as usize;
        assert_eq!(
            &rx.output()[..valid_len],
            &expected_data[expected_start as usize..uncompressed_size as usize]
        );
        // The remaining tail bytes within the last page must be zero-filled.
        assert!(rx.output()[valid_len..expected_len].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_read_range_compressed_second_readahead_window() {
        let chunk_count = 8;
        let chunk_size = 32768usize;
        let uncompressed_size = chunk_count * chunk_size;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 7) % 251) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, compression_info.into()));

        // Request 1 block in the second 128 KiB readahead window (e.g. 135168..139264).
        // Readahead should expand to 131072..262144 (chunks 4, 5, 6, 7).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        assert_eq!(
            rx.commits(),
            vec![
                (131072, chunk_size),
                (163840, chunk_size),
                (196608, chunk_size),
                (229376, chunk_size),
            ]
        );
        assert_eq!(&rx.output()[..131072], &uncompressed_data[131072..262144]);
    }

    #[test]
    fn test_read_range_compressed_partial_final_chunk_zero_tail() {
        let uncompressed_size = 32768 + 1024;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 13) % 251) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, compression_info.into()));

        // Pre-fill destination buffer with 0xFF bytes to verify tail zeroing
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(65536, 0);
        page_request.data.fill(0xFF);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
        assert_eq!(&rx.output()[uncompressed_size..65536], &[0u8; 31744]);
    }

    #[test]
    fn test_files_registry() {
        let extents = Extents::try_new([Extent::new(0..4096, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 4096, Transform::None));
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let files = Files::new(
            service,
            TestDeliveryHandler(|_key, _range| TestVecBuffer::new(4096).0),
            zx::Port::create(),
        );

        assert!(!files.is_loading(100));
        files.begin_loading(100);
        assert!(files.is_loading(100));

        files.insert(100, file.clone());
        assert!(!files.is_loading(100));
        files.remove(100);
        assert!(!files.is_loading(100));
    }

    #[test]
    fn test_files_new_without_pager() {
        let extents = Extents::try_new([Extent::new(0..4096, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 4096, Transform::None));
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let files = Files::new_without_pager(service);

        files.insert(100, file.clone());
        assert_eq!(files.get_file(100).unwrap().uncompressed_size(), 4096);
    }

    struct DelayedBlockService {
        device_data: Vec<u8>,
        pending: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
        fail_offsets: Mutex<Vec<u64>>,
        allocator: Option<Arc<BufferAllocator>>,
    }

    impl DelayedBlockService {
        fn new(device_data: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                device_data,
                pending: Mutex::new(Vec::new()),
                fail_offsets: Mutex::new(Vec::new()),
                allocator: None,
            })
        }

        fn new_with_pool_capacity(device_data: Vec<u8>, pool_capacity: usize) -> Arc<Self> {
            Arc::new(Self {
                device_data,
                pending: Mutex::new(Vec::new()),
                fail_offsets: Mutex::new(Vec::new()),
                allocator: Some(Arc::new(BufferAllocator::new(
                    BLOCK_SIZE as usize,
                    BufferSource::new(pool_capacity),
                ))),
            })
        }

        fn fail_at_offset(&self, offset: u64) {
            self.fail_offsets.lock().push(offset);
        }

        fn wait_for_pending(&self, min_count: usize) {
            while self.pending.lock().len() < min_count {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        fn wait_and_trigger_sync(&self) {
            loop {
                let callbacks = std::mem::take(&mut *self.pending.lock());
                if !callbacks.is_empty() {
                    for cb in callbacks {
                        cb();
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    impl BlockService for DelayedBlockService {
        fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
            if let Some(allocator) = &self.allocator {
                let max_len = std::cmp::min(
                    std::cmp::min(max_len, MAX_READ_BUFFER_SIZE),
                    allocator.buffer_source().size(),
                );
                allocator.allocate_buffer_sync_owned(max_len)
            } else {
                FakeBlockService::new(vec![0u8; max_len]).allocate_buffer(max_len)
            }
        }

        fn read_blocks(
            &self,
            device_offset: u64,
            mut dest_buffer: OwnedBuffer,
            on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
        ) -> Result<(), Error> {
            let should_fail = {
                let mut fail_offsets = self.fail_offsets.lock();
                if let Some(pos) = fail_offsets.iter().position(|&off| off == device_offset) {
                    fail_offsets.swap_remove(pos);
                    true
                } else {
                    false
                }
            };
            if should_fail {
                self.pending.lock().push(Box::new(move || {
                    on_complete(Err(anyhow!("injected block read failure")));
                }));
                return Ok(());
            }
            let start = device_offset as usize;
            let end = (start + dest_buffer.len()).min(self.device_data.len());
            dest_buffer
                .as_mut_ptr_slice()
                .subslice_mut(0..end - start)
                .copy_from_slice(&self.device_data[start..end]);
            self.pending.lock().push(Box::new(move || {
                on_complete(Ok(dest_buffer));
            }));
            Ok(())
        }
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_callback() {
        let uncompressed_size = 65536u64;
        let chunk_size = 32768u64;
        let compressed_offsets = vec![0u64, 1200u64];
        let metadata = BlobMetadata {
            merkle_leaves: MerkleLeaves::new(),
            format: BlobFormat::ChunkedZstd {
                uncompressed_size,
                chunk_size,
                compressed_offsets: compressed_offsets.clone(),
            },
        };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 2400, move |res| {
            let _ = tx.send(res);
        });

        // Trigger delayed storage read completion.
        service.wait_and_trigger_sync();

        let metadata = rx.recv().unwrap();
        assert_eq!(metadata.uncompressed_size, uncompressed_size);
        assert!(metadata.compression_info.is_some());
        assert_eq!(metadata.merkle_leaves, MerkleLeaves::new());
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_error_drops_callback() {
        // Corrupt CompressionInfo where compressed_offsets are invalid (descending)
        let metadata = BlobMetadata {
            merkle_leaves: MerkleLeaves::new(),
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 1200, move |res| {
            let _ = tx.send(res);
        });

        service.wait_and_trigger_sync();

        // Callback should have been dropped on error without sending a message.
        assert!(rx.recv().is_err());
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_corrupt_bincode_drops_callback() {
        // Corrupt random bytes that cannot be deserialized as BlobMetadata
        let device_data = vec![0xFFu8; BLOCK_SIZE as usize];
        let service = DelayedBlockService::new(device_data);
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 1200, move |res| {
            let _ = tx.send(res);
        });

        service.wait_and_trigger_sync();

        // Callback should have been dropped on error without sending a message.
        assert!(rx.recv().is_err());
    }

    #[fuchsia::test]
    fn test_process_mapping_command_error_cleans_up_blob() {
        let metadata = BlobMetadata {
            merkle_leaves: MerkleLeaves::new(),
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let files = Arc::new(Files::new(
            service.clone(),
            TestDeliveryHandler(|_k, _r| TestVecBuffer::new(4096).0),
            zx::Port::create(),
        ));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 99,
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        // File is initially in Loading state.
        assert!(files.is_loading(99));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Files map should have cleaned up the failed entry on drop.
        assert!(!files.is_loading(99));
    }

    #[fuchsia::test]
    fn test_process_mapping_command_registers_blob_merkle_leaves() {
        let leaf1 = [1u8; 32];
        let leaf2 = [2u8; 32];

        // Prepare serialized BlobMetadata with Merkle leaves on a mock block device.
        let metadata =
            BlobMetadata { merkle_leaves: vec![leaf1, leaf2], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        // Use DelayedBlockService to hold back the block read completion for the blob's metadata.
        let service = DelayedBlockService::new(device_data);
        let registered = Arc::new(std::sync::Mutex::new(None));
        let registered_clone = registered.clone();
        struct TestRegistration {
            key: u64,
            max_leaf_bytes: usize,
            registered: Arc<std::sync::Mutex<Option<(u64, usize, Vec<[u8; 32]>, usize)>>>,
        }
        impl BlobRegistration for TestRegistration {
            fn commit(self, leaves: &[[u8; 32]], read_alignment: usize) -> Result<(), Error> {
                *self.registered.lock().unwrap() =
                    Some((self.key, self.max_leaf_bytes, leaves.to_vec(), read_alignment));
                Ok(())
            }
        }
        struct TestRegisterHandler(
            Arc<std::sync::Mutex<Option<(u64, usize, Vec<[u8; 32]>, usize)>>>,
        );
        impl DeliveryHandler for TestRegisterHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = TestRegistration;
            fn get_page_request(self: &Arc<Self>, _key: u64, _range: Range<u64>) -> Self::Request {
                TestVecBuffer::new(4096).0
            }
            fn prepare_register_blob(
                &self,
                key: u64,
                max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                Ok(TestRegistration { key, max_leaf_bytes, registered: self.0.clone() })
            }
        }
        let files = Arc::new(Files::new(
            service.clone(),
            TestRegisterHandler(registered_clone),
            zx::Port::create(),
        ));

        // Encode extent descriptors for data (block 0) and metadata (block 1).
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let data_extent_words = Extents::encode_extents(&data_extents);
        let meta_extent_words = Extents::encode_extents(&meta_extents);
        let mut payload_bytes = Vec::new();
        for w in data_extent_words.chain(meta_extent_words) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        // Inject the Mappings command and extent payload into the VMO FIFO.
        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 42,
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        // Process the mapping command.
        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        // While the block read for metadata is pending completion, the file is in the "loading"
        // state and leaves are not yet registered.
        assert!(files.is_loading(42));
        assert!(registered.lock().unwrap().is_none());

        // Trigger completion of the pending block read.
        service.wait_and_trigger_sync();

        // Once the block read completes and metadata is decoded, verify that:
        // - The file transitioned out of "loading" and is committed into `Files`.
        // - The `BlobRegistration::commit` callback was invoked with the expected key, reserved
        //   max_leaf_bytes, Merkle leaves, and read_alignment.
        assert!(!files.is_loading(42));
        assert!(files.get_file(42).is_some());
        let (reg_key, reg_max_leaf_bytes, reg_leaves, reg_read_alignment) =
            registered.lock().unwrap().take().expect("registered callback called");
        assert_eq!(reg_key, 42);
        assert_eq!(reg_max_leaf_bytes, BLOCK_SIZE as usize);
        assert_eq!(reg_leaves, vec![leaf1, leaf2]);
        assert_eq!(reg_read_alignment, DEFAULT_VERIFICATION_ALIGNMENT as usize);
    }

    #[fuchsia::test]
    fn test_loading_page_request_buffer_dropped_on_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use storage_ptr_slice::MutPtrByteSlice;

        struct DroppingBuffer {
            data: Vec<u8>,
            range: Range<u64>,
            dropped: Arc<AtomicBool>,
        }
        impl Drop for DroppingBuffer {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::Relaxed);
            }
        }
        impl DataBuffer for DroppingBuffer {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                MutPtrByteSlice::from(&mut self.data[..])
            }
            fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }
        impl PageRequest for DroppingBuffer {
            fn prepare(
                &mut self,
                _range: Range<u64>,
                _flush_alignment: Option<NonZero<usize>>,
            ) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
            fn split_at(&mut self, _mid: u64) -> Self {
                unreachable!()
            }
        }

        let metadata = BlobMetadata {
            merkle_leaves: MerkleLeaves::new(),
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_clone = dropped.clone();
        let files = Arc::new(Files::new(
            service.clone(),
            TestDeliveryHandler(move |_k, r| DroppingBuffer {
                data: vec![0u8; 4096],
                range: r,
                dropped: dropped_clone.clone(),
            }),
            zx::Port::create(),
        ));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 99,
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        assert!(files.is_loading(99));

        // Pager request arrives while loading. Buffer should be created and held.
        files.handle_page_request(99, 0..4096);
        assert!(!dropped.load(Ordering::Relaxed));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Dropping guard cleans up LoadingSlot, which drops the queued buffer.
        assert!(dropped.load(Ordering::Relaxed));
        assert!(!files.is_loading(99));
    }

    #[fuchsia::test]
    async fn test_pager_concurrent_request_during_metadata_read() {
        use vmo_fifo::{Alignment, SyncSender};
        use zx::{Pager, PagerOptions, Port, Rights, Vmo, VmoOptions};

        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        // Metadata stored at physical block 1
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        // Data stored at physical block 0
        device_data[..4].copy_from_slice(&[10, 20, 30, 40]);

        let service = DelayedBlockService::new(device_data);
        let port = Port::create();

        let (page_request, rx) = TestVecBuffer::new(BLOCK_SIZE as usize);
        let page_request_holder = Arc::new(Mutex::new(Some(page_request)));
        let page_request_clone = page_request_holder.clone();

        let files = Arc::new(Files::new(
            service.clone(),
            TestDeliveryHandler(move |_key, _range| page_request_clone.lock().take().unwrap()),
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
        ));

        let _pager_thread = files.spawn_pager_thread("pager");

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = Vmo::create(65536).unwrap();
        let mut sender = SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 42,
            stored_size: BLOCK_SIZE,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        std::thread::scope(|s| {
            let msg = receiver.claim().unwrap();
            let receiver_for_process = receiver.clone();
            let files_for_process = files.clone();
            s.spawn(move || {
                process_mapping_command(msg, &receiver_for_process, &files_for_process).unwrap();
            });

            // Wait until metadata block read has been submitted to service
            // (file is in Loading state).
            while service.pending.lock().is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            // Trigger page fault from a detached background thread while file is loading metadata.
            std::thread::spawn(move || {
                let mut b = [0u8; 1];
                let _ = vmo_blob_clone.read(&mut b, 0);
            });

            // Wait until the page request arrives and is queued in the loading entry before
            // completing the metadata read.
            while !files.has_queued_page_requests(42) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            // Complete metadata block read; this will insert the file and immediately
            // drain queued page requests.
            service.wait_and_trigger_sync();
        });

        // Complete data block read requested by pager thread when draining queued request.
        service.wait_and_trigger_sync();

        // Check if page request made during metadata read was serviced!
        assert!(
            !rx.wait_for_commits(1).is_empty(),
            "Page request made during metadata read was dropped!"
        );
    }

    #[fuchsia::test]
    async fn test_pager_request_arrives_before_mapping_command() {
        use vmo_fifo::{Alignment, SyncSender};
        use zx::{Pager, PagerOptions, Port, Rights, Vmo, VmoOptions};

        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        // Metadata stored at physical block 1
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        // Data stored at physical block 0
        device_data[..4].copy_from_slice(&[10, 20, 30, 40]);

        let service = DelayedBlockService::new(device_data);
        let port = Port::create();

        let (page_request, rx) = TestVecBuffer::new(BLOCK_SIZE as usize);
        let page_request_holder = Arc::new(Mutex::new(Some(page_request)));
        let page_request_clone = page_request_holder.clone();

        let files = Arc::new(Files::new(
            service.clone(),
            TestDeliveryHandler(move |_key, _range| page_request_clone.lock().take().unwrap()),
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
        ));

        let _pager_thread = files.spawn_pager_thread("pager");

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = Vmo::create(65536).unwrap();
        let mut sender = SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 42,
            stored_size: BLOCK_SIZE,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        // Trigger page fault from a background thread BEFORE the mapping command is processed.
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            let _ = vmo_blob_clone.read(&mut b, 0);
        });

        // Wait until the page request arrives and is queued as pending mapping in `files`.
        while !files.has_queued_page_requests(42) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        std::thread::scope(|s| {
            let msg = receiver.claim().unwrap();
            let receiver_for_process = receiver.clone();
            let files_for_process = files.clone();
            s.spawn(move || {
                process_mapping_command(msg, &receiver_for_process, &files_for_process).unwrap();
            });

            // Trigger metadata read completion.
            service.wait_and_trigger_sync();
        });

        // Complete data block read requested by pager thread when draining queued request.
        service.wait_and_trigger_sync();

        // Check if page request made before mapping command was serviced!
        assert!(
            !rx.wait_for_commits(1).is_empty(),
            "Page request made before mapping command was dropped!"
        );
    }

    #[fuchsia::test]
    fn test_process_mapping_command_close_blob() {
        let service = Arc::new(FakeBlockService::new(vec![0u8; 8192]));
        let files = Arc::new(Files::new(
            service,
            TestDeliveryHandler(|_k, _r| TestVecBuffer::new(4096).0),
            zx::Port::create(),
        ));

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();

        // 1. Send Mappings command with 0 metadata extents (uncompressed file, loads immediately).
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in Extents::encode_extents(&data_extents) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 123,
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 0,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();

        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());
        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        assert!(files.is_loaded(123));

        // 2. Send CloseBlob command.
        sender
            .push(crate::RawMappingCommand {
                opcode: crate::CLOSE_BLOB_COMMAND,
                offset: 0,
                key: 123,
                stored_size: 0,
                device_offset: 0,
                metadata_count: 0,
                extent_count: 0,
            })
            .unwrap();

        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        assert!(!files.is_loaded(123));
    }

    #[test]
    fn test_read_ahead_size_for_chunk_size() {
        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 32 * 1024), 32 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 32 * 1024), 48 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 32 * 1024), 64 * 1024);

        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 64 * 1024), 64 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 64 * 1024), 48 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 64 * 1024), 64 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(96 * 1024, 64 * 1024), 96 * 1024);

        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 128 * 1024), 128 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 128 * 1024), 96 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 128 * 1024), 128 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(96 * 1024, 128 * 1024), 96 * 1024);
    }

    #[test]
    fn test_deserialize_blob_metadata() {
        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };

        // Valid metadata with version 64.
        let bytes = serialize_metadata(&metadata);
        let deserialized = deserialize_blob_metadata(&bytes).unwrap();
        assert_eq!(deserialized, metadata);

        // Buffer too short (< 4 bytes).
        assert!(deserialize_blob_metadata(&[1, 2, 3]).is_err());

        // Unsupported version (< 53).
        let with_version = |bytes: &[u8], version: u32| {
            let mut bytes = bytes.to_vec();
            (&mut bytes[..4]).write_u32::<LittleEndian>(version).unwrap();
            bytes
        };
        assert!(deserialize_blob_metadata(&with_version(&bytes, 52)).is_err());

        // Versions before 64 are decoded with the old format and can't contain raw chunks.
        let lz4 = |format| BlobMetadata { merkle_leaves: MerkleLeaves::new(), format };
        let old = lz4(BlobFormat::ChunkedLz4 {
            uncompressed_size: 65536,
            chunk_size: 32768,
            compressed_offsets: vec![0, 100],
        });
        assert_eq!(
            deserialize_blob_metadata(&with_version(&serialize_metadata(&old), 63)).unwrap(),
            old
        );
        let raw = lz4(BlobFormat::ChunkedLz4WithRawChunks {
            uncompressed_size: 65536,
            chunk_size: 32768,
            compressed_offsets: vec![0, 100],
        });
        let raw_bytes = serialize_metadata(&raw);
        assert_eq!(deserialize_blob_metadata(&raw_bytes).unwrap(), raw);
        assert_eq!(
            deserialize_blob_metadata(&with_version(&raw_bytes, BLOB_METADATA_VERSION | (1 << 24)))
                .unwrap(),
            raw
        );
        assert!(deserialize_blob_metadata(&with_version(&raw_bytes, 63)).is_err());
    }

    #[fuchsia::test]
    async fn test_wait_for_file_before_and_after_insert() {
        let service = Arc::new(FakeBlockService::new(vec![]));
        let files = Arc::new(Files::new_without_pager(service));

        let extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, BLOCK_SIZE, Transform::None));

        // Wait before insert.
        let files_clone = files.clone();
        let file_clone = file.clone();
        let wait_task =
            fasync::Task::spawn(async move { files_clone.wait_for_file(42).await.unwrap() });

        // Insert the file.
        files.insert(42, file_clone);
        let loaded_file = wait_task.await;
        assert_eq!(loaded_file.uncompressed_size(), BLOCK_SIZE);

        // Wait after insert.
        let loaded_file2 = files.wait_for_file(42).await.unwrap();
        assert_eq!(loaded_file2.uncompressed_size(), BLOCK_SIZE);
    }

    #[test]
    fn test_read_range_encrypted() {
        let block_count = 8;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 255) as u8;
        }

        let key = UnwrappedKey::new(vec![0x42u8; 32]);
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&key));

        let mut encrypted_data = expected_data.clone();
        cipher.encrypt(0, 0, 0, 0, MutPtrByteSlice::from(&mut encrypted_data[..])).unwrap();
        let service = FakeBlockService::new(encrypted_data);

        let extents = Extents::try_new([Extent::new(0..(8 * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8 * BLOCK_SIZE, cipher.into()));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(8 * BLOCK_SIZE));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, (8 * BLOCK_SIZE) as usize)]);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_encrypted_with_offset() {
        let total_blocks = 64; // 256 KiB
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 17) % 255) as u8;
        }

        let key = UnwrappedKey::new(vec![0x5au8; 32]);
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&key));

        let mut encrypted_data = expected_data.clone();
        cipher.encrypt(0, 0, 0, 0, MutPtrByteSlice::from(&mut encrypted_data[..])).unwrap();
        let service = FakeBlockService::new(encrypted_data);

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, total_blocks * BLOCK_SIZE, cipher.into()));

        // Request 1 block at offset 132 KiB (135168..139264).
        // Readahead window expands to 128 KiB..256 KiB (131072..262144), ensuring that
        // current_offset starts at a non-zero offset (128 KiB) with correct sector tweak.
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(TARGET_READ_AHEAD_SIZE, TARGET_READ_AHEAD_SIZE as usize)]);
        assert_eq!(
            &rx.output()[..TARGET_READ_AHEAD_SIZE as usize],
            &expected_data[TARGET_READ_AHEAD_SIZE as usize..2 * TARGET_READ_AHEAD_SIZE as usize]
        );
    }

    #[test]
    fn test_read_range_encrypted_multi_chunk() {
        let block_count = 4;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 31) % 255) as u8;
        }

        let key = UnwrappedKey::new(vec![0x7fu8; 32]);
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&key));

        let mut encrypted_data = expected_data.clone();
        cipher.encrypt(0, 0, 0, 0, MutPtrByteSlice::from(&mut encrypted_data[..])).unwrap();
        // Capping to BLOCK_SIZE forces read_aligned_range to issue separate buffer reads for each chunk.
        let service = FakeBlockService::new_with_cap(encrypted_data, Some(BLOCK_SIZE as usize));

        let extents =
            Extents::try_new([Extent::new(0..(block_count * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, block_count * BLOCK_SIZE, cipher.into()));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(block_count * BLOCK_SIZE));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits().len(), 4);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_decryption_error() {
        #[derive(Debug)]
        struct FailingCipher(FxfsCipher);
        impl Cipher for FailingCipher {
            fn encrypt(
                &self,
                ino: u64,
                attr: u64,
                dev: u64,
                file: u64,
                buf: MutPtrByteSlice<'_>,
            ) -> Result<(), Error> {
                self.0.encrypt(ino, attr, dev, file, buf)
            }
            fn decrypt(
                &self,
                _ino: u64,
                _attribute_id: u64,
                _device_offset: u64,
                _file_offset: u64,
                _buffer: MutPtrByteSlice<'_>,
            ) -> Result<(), Error> {
                bail!("decrypt failed")
            }
            fn encrypt_filename(&self, ino: u64, name: &mut Vec<u8>) -> Result<(), Error> {
                self.0.encrypt_filename(ino, name)
            }
            fn decrypt_filename(&self, ino: u64, name: &mut Vec<u8>) -> Result<(), Error> {
                self.0.decrypt_filename(ino, name)
            }
            fn hash_code(&self, name: &[u8], filename: &str) -> Option<u32> {
                self.0.hash_code(name, filename)
            }
            fn hash_code_casefold(&self, filename: &str) -> u32 {
                self.0.hash_code_casefold(filename)
            }
            fn supports_inline_encryption(&self) -> bool {
                self.0.supports_inline_encryption()
            }
            fn crypt_ctx(&self, ino: u64, attr: u64, offset: u64) -> Option<(u64, u8)> {
                self.0.crypt_ctx(ino, attr, offset)
            }
        }

        let key = UnwrappedKey::new(vec![0x42u8; 32]);
        let failing_cipher: Arc<dyn Cipher> = Arc::new(FailingCipher(FxfsCipher::new(&key)));
        let service = FakeBlockService::new(vec![0u8; 8192]);
        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8192, failing_cipher.into()));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);
        file.read_range(&service, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[fuchsia::test]
    fn test_process_mapping_command_unencrypted_no_metadata() {
        let block_count = 2;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let service = Arc::new(FakeBlockService::new(expected_data.clone()));
        let files = Arc::new(Files::new(
            service.clone(),
            TestDeliveryHandler(|_k, r| TestVecBuffer::new_with_range(r).0),
            zx::Port::create(),
        ));

        let data_extents =
            Extents::try_new([Extent::new(0..(block_count as u64 * BLOCK_SIZE), Some(0))], 0)
                .unwrap();
        let mut payload_bytes = Vec::new();
        for w in Extents::encode_extents(&data_extents) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 100,
            stored_size: block_count as u64 * BLOCK_SIZE,
            device_offset: 0,
            metadata_count: 0,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        let file = files.get_file(100).expect("File should be loaded immediately");
        let (page_request, rx) =
            TestVecBuffer::new_with_range(0..(block_count as u64 * BLOCK_SIZE));
        file.read_range(service.as_ref(), page_request);
        assert_eq!(rx.output(), expected_data);
    }

    #[fuchsia::test]
    fn test_process_mapping_command_encrypted() {
        struct NoRegisterBlobHandler;
        impl DeliveryHandler for NoRegisterBlobHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = ();
            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }
            fn prepare_register_blob(
                &self,
                _key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                panic!("prepare_register_blob should not be called for encrypted files");
            }
        }

        let block_count = 2;
        let mut plaintext = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in plaintext.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        let raw_key = [0x5au8; 32];
        let key = UnwrappedKey::new(raw_key.to_vec());
        let cipher = FxfsCipher::new(&key);

        let mut ciphertext = plaintext.clone();
        cipher.encrypt(0, 0, 0, 0, MutPtrByteSlice::from(&mut ciphertext[..])).unwrap();

        let service = Arc::new(FakeBlockService::new(ciphertext));
        let files =
            Arc::new(Files::new(service.clone(), NoRegisterBlobHandler, zx::Port::create()));

        let data_extents =
            Extents::try_new([Extent::new(0..(block_count as u64 * BLOCK_SIZE), Some(0))], 0)
                .unwrap();
        let mut payload_bytes = Vec::new();
        for w in Extents::encode_extents(&data_extents) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }
        payload_bytes.extend_from_slice(&raw_key);

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND | crate::MAPPINGS_FLAG_ENCRYPTED,
            offset: payload_buf.offset(),
            key: 101,
            stored_size: block_count as u64 * BLOCK_SIZE,
            device_offset: 0,
            metadata_count: 0,
            extent_count: 1,
        };
        payload_buf.commit(cmd).unwrap();

        let invalid_cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND | crate::MAPPINGS_FLAG_ENCRYPTED,
            offset: 0,
            key: 102,
            stored_size: block_count as u64 * BLOCK_SIZE,
            device_offset: 0,
            metadata_count: 1,
            extent_count: 1,
        };
        sender.reserve_payload(0).unwrap().commit(invalid_cmd).unwrap();

        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        let invalid_msg = receiver.claim().unwrap();
        assert!(process_mapping_command(invalid_msg, &receiver, &files).is_err());

        let file = files.get_file(101).expect("File should be loaded immediately");
        assert!(matches!(file.transform(), Transform::Encrypted(_)));

        let (page_request, rx) =
            TestVecBuffer::new_with_range(0..(block_count as u64 * BLOCK_SIZE));
        file.read_range(service.as_ref(), page_request);
        assert_eq!(rx.output(), plaintext);
    }

    #[fuchsia::test]
    fn test_queued_page_request_does_not_deadlock_single_completion_thread() {
        // 128 KiB of blob data (> 64 KiB buffer pool) + 1 block (4 KiB) of metadata.
        let data_size = TARGET_READ_AHEAD_SIZE as usize;
        let encoded_metadata = serialize_metadata(&BlobMetadata {
            merkle_leaves: MerkleLeaves::new(),
            format: BlobFormat::Uncompressed,
        });
        let mut device_data = vec![0xabu8; data_size + BLOCK_SIZE as usize];
        device_data[data_size..data_size + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        // 64 KiB buffer pool (< 128 KiB TARGET_READ_AHEAD_SIZE).
        let block_service = DelayedBlockService::new_with_pool_capacity(device_data, 64 * 1024);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..4096);
        let page_request = Mutex::new(Some(page_request));
        let files = Arc::new(Files::new(
            block_service.clone(),
            TestDeliveryHandler(move |_key, _range| page_request.lock().take().unwrap()),
            zx::Port::create(),
        ));
        let _pager_thread = files.spawn_pager_thread("pager");

        let data_extents =
            Extents::try_new([Extent::new(0..TARGET_READ_AHEAD_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(TARGET_READ_AHEAD_SIZE))], 0)
                .unwrap();

        let blob_key = 200;

        // Start metadata read (txn 0: 4 KiB). Allocates 4 KiB from the 64 KiB pool and queues txn 0
        // in `block_service.pending`.
        files.begin_loading(blob_key);
        let files_clone = files.clone();
        read_blob_metadata(
            block_service.as_ref(),
            &meta_extents,
            TARGET_READ_AHEAD_SIZE,
            move |metadata| {
                let file = Arc::new(File::new(
                    data_extents,
                    metadata.uncompressed_size,
                    metadata.compression_info.into(),
                ));
                files_clone.insert(blob_key, file);
            },
        );

        // Queue a page request while txn 0 is still pending. Because `File::read_range` expands
        // page requests to `TARGET_READ_AHEAD_SIZE` (128 KiB) and the buffer pool is 64 KiB,
        // servicing this request requires two sequential 64 KiB buffer allocations (txn 1 and
        // txn 2).
        files.handle_page_request(blob_key, 0..4096);

        // Complete txn 0 (metadata) followed by txn 1 and txn 2 (the two 64 KiB data chunks) on a
        // single completion thread. Completing txn 0 inserts the file, which must not block the
        // completion thread while servicing the queued page request.
        let block_completion_thread = std::thread::spawn({
            let block_service = block_service.clone();
            move || {
                for _ in 0..3 {
                    block_service.wait_and_trigger_sync();
                }
            }
        });

        block_completion_thread.join().unwrap();

        // Verify that the 128 KiB readahead was split into two 64 KiB block reads (matching the
        // 64 KiB pool capacity) and both chunks were committed to the pager.
        assert_eq!(rx.wait_for_commits(2), vec![(0, 65536), (65536, 65536)]);
        // Verify that the full 128 KiB of blob data was copied into the page request's buffer.
        assert_eq!(rx.output(), vec![0xabu8; data_size]);
    }

    #[fuchsia::test]
    fn test_pager_thread_lifecycle() {
        let port = zx::Port::create();
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let files = Arc::new(Files::new(
            service,
            TestDeliveryHandler(|_key, _range| TestVecBuffer::new(4096).0),
            port,
        ));
        let thread = files.spawn_pager_thread("pager");
        drop(thread);
    }

    #[fuchsia::test]
    fn test_pager_packet() {
        let port = zx::Port::create();
        let pager = zx::Pager::create(zx::PagerOptions::empty()).expect("create pager");
        let vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, 1234, 4096).expect("create vmo");

        let vmo_clone = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate vmo");
        let _reader_thread = std::thread::spawn(move || {
            let mut b = [0u8; 1];
            let _ = vmo_clone.read(&mut b, 0);
        });

        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("wait packet");
        assert_eq!(packet.key(), 1234);
        if let zx::PacketContents::Pager(pager_packet) = packet.contents() {
            assert_eq!(pager_packet.command(), ZX_PAGER_VMO_READ);
            assert_eq!(pager_packet.range(), 0..4096);
        } else {
            panic!("Expected pager packet");
        }
    }

    #[fuchsia::test]
    fn test_files_record_profile() {
        let total_size = TARGET_READ_AHEAD_SIZE * 2;
        let uncompressed_data = vec![0x55u8; total_size as usize];

        // Compress a 192 KiB file with 48 KiB chunks so its `read_ahead_size` is 96 KiB
        // (`128 KiB / 48 KiB * 48 KiB = 96 KiB`), distinct from `TARGET_READ_AHEAD_SIZE` (128 KiB).
        let compressed_uncompressed_size = 192 * 1024;
        let chunk_size = 48 * 1024;
        let options = ChunkedArchiveOptions::V2 {
            minimum_chunk_size: chunk_size,
            chunk_alignment: chunk_size,
            compression_level: 1,
        };
        let archive = delivery_blob::compression::ChunkedArchive::new(
            &uncompressed_data[..compressed_uncompressed_size],
            options,
        )
        .expect("ChunkedArchive::new failed");
        let mut compressed_offsets = vec![0];
        let mut compressed_bytes = Vec::new();
        for chunk in archive.chunks() {
            compressed_bytes.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_bytes.len() as u64);
        }
        compressed_offsets.pop();
        let compressed_stored_size = compressed_bytes.len() as u64;
        let compressed_stored_aligned = compressed_stored_size.next_multiple_of(BLOCK_SIZE);

        let mut device_data = vec![0x55u8; (total_size + compressed_stored_aligned) as usize];
        device_data[total_size as usize..total_size as usize + compressed_bytes.len()]
            .copy_from_slice(&compressed_bytes);
        let service = Arc::new(FakeBlockService::new(device_data));

        let files = Arc::new(Files::new(
            service,
            TestDeliveryHandler(|_key, range| TestVecBuffer::new_with_range(range).0),
            zx::Port::create(),
        ));

        let extents1 = Extents::try_new([Extent::new(0..total_size, Some(0))], 0)
            .expect("Extents::try_new failed");
        let file1 = Arc::new(File::new(extents1, total_size, Transform::None));
        files.insert(10, file1);

        let extents2 =
            Extents::try_new([Extent::new(0..compressed_stored_aligned, Some(total_size))], 0)
                .expect("Extents::try_new failed");
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            compressed_stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .expect("CompressionInfo::new failed");
        let file2 = Arc::new(File::new(
            extents2,
            compressed_uncompressed_size as u64,
            compression_info.into(),
        ));
        let compressed_read_ahead = file2.read_ahead_size();
        assert_eq!(compressed_read_ahead, 96 * 1024);
        files.insert(20, file2);

        // Stopping when no recording is active should return BAD_STATE.
        assert_eq!(
            files.stop_recording().expect_err("stop_recording succeeded without active recording"),
            zx::Status::BAD_STATE
        );

        // Page requests before recording starts should not be recorded.
        files.handle_page_request(10, 0..4096);

        files.start_recording().expect("start_recording failed");
        assert_eq!(
            files.start_recording().expect_err("duplicate start_recording succeeded"),
            zx::Status::BAD_STATE
        );

        // Fault in first readahead window of key 10 (offset 4096 -> window 0).
        files.handle_page_request(10, 4096..8192);
        // Fault in second 96 KiB readahead window of compressed key 20 (offset 100 KiB -> 96 KiB).
        files.handle_page_request(20, 100 * 1024..104 * 1024);
        // Fault spanning both 128 KiB readahead windows of key 10 (0..256 KiB -> 0 and 128 KiB).
        files.handle_page_request(10, 0..total_size);

        let vmo = files.stop_recording().expect("stop_recording failed");
        let entries: Vec<_> = FileMessage::decode_from_vmo(vmo, BLOCK_SIZE as usize)
            .expect("decode_from_vmo failed")
            .collect();
        assert_eq!(
            entries,
            vec![
                FileMessage { id: 10, offset: 0 },
                FileMessage { id: 20, offset: compressed_read_ahead },
                FileMessage { id: 10, offset: 0 },
                FileMessage { id: 10, offset: TARGET_READ_AHEAD_SIZE },
            ]
        );

        // Faults after stop_recording should not be recorded, and a subsequent recording starts
        // clean.
        files.handle_page_request(20, 0..4096);
        files.start_recording().expect("second start_recording failed");
        let empty_vmo = files.stop_recording().expect("second stop_recording failed");
        assert_eq!(
            FileMessage::decode_from_vmo(empty_vmo, BLOCK_SIZE as usize)
                .expect("decode_from_vmo failed")
                .collect::<Vec<_>>(),
            Vec::<FileMessage>::new()
        );
    }

    #[fuchsia::test]
    fn test_files_record_profile_deferred_loading() {
        let total_size = TARGET_READ_AHEAD_SIZE;
        let device_data = vec![0x77u8; total_size as usize];
        let service = Arc::new(FakeBlockService::new(device_data));

        let (page_request, rx) = TestVecBuffer::new_with_range(4096..8192);
        let page_request = Mutex::new(Some(page_request));
        let files = Arc::new(Files::new(
            service,
            TestDeliveryHandler(move |_key, _range| {
                page_request.lock().take().expect("page_request already taken")
            }),
            zx::Port::create(),
        ));
        let _pager_thread = files.spawn_pager_thread("pager");

        files.start_recording().expect("start_recording failed");

        // Queue a page request for key 55 before its mapping is inserted.
        files.handle_page_request(55, 4096..8192);
        assert!(files.has_queued_page_requests(55));

        let extents = Extents::try_new([Extent::new(0..total_size, Some(0))], 0)
            .expect("Extents::try_new failed");
        let file = Arc::new(File::new(extents, total_size, Transform::None));
        files.insert(55, file);

        assert_eq!(rx.wait_for_commits(1), vec![(0, total_size as usize)]);

        let vmo = files.stop_recording().expect("stop_recording failed");
        let entries: Vec<_> = FileMessage::decode_from_vmo(vmo, BLOCK_SIZE as usize)
            .expect("decode_from_vmo failed")
            .collect();
        assert_eq!(entries, vec![FileMessage { id: 55, offset: 0 }]);
    }

    #[test]
    fn test_child_range_and_inline_tail() {
        #[derive(Debug, PartialEq)]
        enum RecordedEvent {
            InlineTail {
                parent_byte_offset: u64,
                child_key: u64,
                child_target_offset: u64,
                tail_length: u32,
            },
            ChildPageRequest {
                child_key: u64,
                child_target_offset: u64,
                parent_byte_range: Range<u64>,
            },
        }

        struct RecordingChildHandler {
            events: Mutex<Vec<RecordedEvent>>,
        }
        impl DeliveryHandler for RecordingChildHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = ();
            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }
            fn prepare_register_blob(
                &self,
                _key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                Ok(())
            }
            fn get_child_page_request(
                self: &Arc<Self>,
                child_key: u64,
                child_target_offset: u64,
                parent_byte_range: Range<u64>,
            ) -> Self::Request {
                self.events.lock().push(RecordedEvent::ChildPageRequest {
                    child_key,
                    child_target_offset,
                    parent_byte_range: parent_byte_range.clone(),
                });
                TestVecBuffer::new_with_range(parent_byte_range).0
            }
            fn deliver_child_inline_tail(
                self: &Arc<Self>,
                parent_byte_offset: u64,
                child_key: u64,
                child_target_offset: u64,
                tail_length: u32,
                _trace_flow_id: u64,
            ) {
                self.events.lock().push(RecordedEvent::InlineTail {
                    parent_byte_offset,
                    child_key,
                    child_target_offset,
                    tail_length,
                });
            }
        }

        let data_size = (TARGET_READ_AHEAD_SIZE * 2) as usize;
        let mut expected_data = vec![0u8; data_size];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 31 + 7) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data);
        let parent_extents =
            Extents::try_new([Extent::new(0..(data_size as u64), Some(0))], 0).unwrap();
        let parent_file = Arc::new(File::new(parent_extents, data_size as u64, Transform::None));

        // Create a 2-page FlatInline child file whose first page (0..4096) is at parent block
        // 16384..20480 and whose inline tail (4096..8192) is at unaligned parent offset 4128.
        let child_extents = Extents::try_new(
            [
                Extent::new(0..4096, Some(16384)),
                Extent::new(4096..8192, crate::DeviceExtent::InlineTail(4128)),
            ],
            0,
        )
        .unwrap();
        let child_file = Arc::new(File::new(child_extents, 4608, Transform::None));

        let handler = Arc::new(RecordingChildHandler { events: Mutex::new(Vec::new()) });
        let started_async =
            child_file.read_child_range(10, 0..4096, 0, &parent_file, &service, &handler);
        assert!(started_async, "Expected Block extent to perform one aligned parent read");
        assert_eq!(service.read_count(), 1, "InlineTail must not perform any block I/O");

        let events = handler.events.lock();
        assert_eq!(
            &*events,
            &[
                RecordedEvent::ChildPageRequest {
                    child_key: 10,
                    child_target_offset: 0,
                    parent_byte_range: 16384..20480,
                },
                RecordedEvent::InlineTail {
                    parent_byte_offset: 4128,
                    child_key: 10,
                    child_target_offset: 4096,
                    tail_length: 512,
                },
            ]
        );
    }

    #[fuchsia::test]
    fn test_read_range_runs_precommit_only_when_io_is_in_flight() {
        use crate::PagePrecommit;

        struct VmarBackedRequest {
            range: Range<u64>,
            vaddr: usize,
            len: usize,
            keep_alive: Arc<()>,
        }

        impl DataBuffer for VmarBackedRequest {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                unsafe {
                    MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(
                        self.vaddr as *mut u8,
                        self.len,
                    ))
                }
            }
            fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
                self.vaddr += size;
                self.len -= size;
                Ok(())
            }
        }

        impl PageRequest for VmarBackedRequest {
            fn prepare(
                &mut self,
                read_range: Range<u64>,
                _flush_alignment: Option<NonZero<usize>>,
            ) -> Result<(), ChunkedArchiveError> {
                self.range = read_range;
                Ok(())
            }
            fn split_at(&mut self, _mid: u64) -> Self {
                unreachable!()
            }
            fn precommit(&self) -> PagePrecommit {
                PagePrecommit::new(self.vaddr, self.len, self.keep_alive.clone())
            }
        }

        let page_size = zx::system_get_page_size() as usize;
        let vmo = zx::Vmo::create((2 * page_size) as u64).expect("Vmo::create");
        let vaddr = fuchsia_runtime::vmar_root_self()
            .map(0, &vmo, 0, 2 * page_size, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .expect("vmar map");

        let extents = Extents::try_new([Extent::new(0..(2 * page_size as u64), Some(0))], 0)
            .expect("extents");
        let file = Arc::new(File::new(extents, (2 * page_size) as u64, Transform::None));

        // With `DelayedBlockService`, the read is in flight when `read_range` returns, so
        // `PagePrecommit::run` must have already committed both pages in `vmo` before the read
        // completes!
        let delayed = DelayedBlockService::new(vec![0x5Au8; 2 * page_size]);
        let req = VmarBackedRequest {
            range: 0..(page_size as u64),
            vaddr,
            len: 2 * page_size,
            keep_alive: Arc::new(()),
        };
        file.read_range(&*delayed, req);
        assert_eq!(
            vmo.info().expect("vmo info").committed_bytes,
            (2 * page_size) as u64,
            "PagePrecommit must commit VMO pages while async I/O is in flight"
        );
        delayed.wait_and_trigger_sync();

        unsafe {
            let _ = fuchsia_runtime::vmar_root_self().unmap(vaddr, 2 * page_size);
        }
    }

    #[fuchsia::test]
    fn test_files_splits_large_page_request_into_1mib_windows() {
        struct RecordingRequest {
            range: Range<u64>,
            buf: Vec<u8>,
            prepared_ranges: Arc<Mutex<Vec<Range<u64>>>>,
        }

        impl DataBuffer for RecordingRequest {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                MutPtrByteSlice::from(&mut self.buf[..])
            }
            fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }

        impl PageRequest for RecordingRequest {
            fn prepare(
                &mut self,
                read_range: Range<u64>,
                _flush_alignment: Option<NonZero<usize>>,
            ) -> Result<(), ChunkedArchiveError> {
                let len = (read_range.end - read_range.start) as usize;
                assert!(len <= MAX_READ_BUFFER_SIZE, "prepared range {len} > 1 MiB");
                self.buf = vec![0u8; len];
                self.prepared_ranges.lock().push(read_range);
                Ok(())
            }

            fn split_at(&mut self, mid: u64) -> Self {
                let head_range = self.range.start..mid;
                self.range.start = mid;
                Self {
                    range: head_range,
                    buf: Vec::new(),
                    prepared_ranges: self.prepared_ranges.clone(),
                }
            }
        }

        let file_size = (2 * MAX_READ_BUFFER_SIZE + 256 * 1024) as u64;
        let service = Arc::new(FakeBlockService::new(vec![0xABu8; file_size as usize]));
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let prepared_clone = prepared.clone();
        let files = Arc::new(Files::new(
            service,
            TestDeliveryHandler(move |_k, range| RecordingRequest {
                range,
                buf: Vec::new(),
                prepared_ranges: prepared_clone.clone(),
            }),
            zx::Port::create(),
        ));

        let extents = Extents::try_new([Extent::new(0..file_size, Some(0))], 0).expect("extents");
        let file = Arc::new(File::new(extents, file_size, Transform::None));
        files.insert(77, file);

        files.handle_page_request(77, 0..file_size);
        let ranges = prepared.lock().clone();
        let mib = MAX_READ_BUFFER_SIZE as u64;
        assert_eq!(ranges, vec![0..mib, mib..2 * mib, 2 * mib..file_size]);
    }

    #[fuchsia::test]
    fn test_child_files_splits_large_page_requests_and_handles_deferred_loading() {
        #[derive(Clone, Debug, PartialEq)]
        struct RecordedChildSlice {
            child_key: u64,
            child_target_offset: u64,
            parent_byte_range: Range<u64>,
            prepared_parent_range: Range<u64>,
        }

        struct ChildRecordingRequest {
            range: Range<u64>,
            buf: Vec<u8>,
            child_meta: Option<(u64, u64, Range<u64>)>,
            recorded: Arc<Mutex<Vec<RecordedChildSlice>>>,
            on_record: Arc<fuchsia_sync::Condvar>,
        }

        impl DataBuffer for ChildRecordingRequest {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                MutPtrByteSlice::from(&mut self.buf[..])
            }
            fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }

        impl PageRequest for ChildRecordingRequest {
            fn prepare(
                &mut self,
                read_range: Range<u64>,
                _flush_alignment: Option<NonZero<usize>>,
            ) -> Result<(), ChunkedArchiveError> {
                let len = (read_range.end - read_range.start) as usize;
                assert!(len <= MAX_READ_BUFFER_SIZE, "prepared child parent range {len} > 1 MiB");
                self.buf = vec![0u8; len];
                let (child_key, child_target_offset, parent_byte_range) = self
                    .child_meta
                    .take()
                    .expect("root get_page_request should not be prepared for child session");
                self.recorded.lock().push(RecordedChildSlice {
                    child_key,
                    child_target_offset,
                    parent_byte_range,
                    prepared_parent_range: read_range,
                });
                self.on_record.notify_all();
                Ok(())
            }

            fn split_at(&mut self, mid: u64) -> Self {
                let head_range = self.range.start..mid;
                let head_meta =
                    self.child_meta.as_mut().map(|(child_key, child_offset, parent_range)| {
                        let head = (*child_key, *child_offset, head_range.clone());
                        *child_offset += mid - head_range.start;
                        parent_range.start = mid;
                        head
                    });
                self.range.start = mid;
                Self {
                    range: head_range,
                    buf: Vec::new(),
                    child_meta: head_meta,
                    recorded: self.recorded.clone(),
                    on_record: self.on_record.clone(),
                }
            }
        }

        struct ChildRecordingHandler {
            recorded: Arc<Mutex<Vec<RecordedChildSlice>>>,
            on_record: Arc<fuchsia_sync::Condvar>,
        }

        impl DeliveryHandler for ChildRecordingHandler {
            type Request = ChildRecordingRequest;
            type BlobRegistration = ();

            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                ChildRecordingRequest {
                    range,
                    buf: Vec::new(),
                    child_meta: None,
                    recorded: self.recorded.clone(),
                    on_record: self.on_record.clone(),
                }
            }

            fn prepare_register_blob(
                &self,
                _key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                Ok(())
            }

            fn get_child_page_request(
                self: &Arc<Self>,
                child_key: u64,
                child_target_offset: u64,
                parent_byte_range: Range<u64>,
            ) -> Self::Request {
                ChildRecordingRequest {
                    range: parent_byte_range.clone(),
                    buf: Vec::new(),
                    child_meta: Some((child_key, child_target_offset, parent_byte_range)),
                    recorded: self.recorded.clone(),
                    on_record: self.on_record.clone(),
                }
            }
        }

        let mib = MAX_READ_BUFFER_SIZE as u64;
        let parent_size = 2 * mib + 256 * 1024;
        let child_size = 2 * mib;
        let service = Arc::new(FakeBlockService::new(vec![0xCDu8; parent_size as usize]));

        let parent_extents =
            Extents::try_new([Extent::new(0..parent_size, Some(0))], 0).expect("parent extents");
        let parent_file = Arc::new(File::new(parent_extents, parent_size, Transform::None));

        // Map the 2 MiB child file starting at an unaligned 4 KiB block offset in the parent so a
        // 2 MiB child read (`0..2 MiB` -> `4 KiB..2 MiB + 4 KiB` in parent) crosses two 1 MiB
        // parent window boundaries (`1 MiB` and `2 MiB`).
        let dev_offset = 4096u64;
        let child_extents =
            Extents::try_new([Extent::new(0..child_size, Some(dev_offset))], 0).expect("extents");
        let child_file = Arc::new(File::new(child_extents, child_size, Transform::None));

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let on_record = Arc::new(fuchsia_sync::Condvar::new());
        let handler = Arc::new(ChildRecordingHandler {
            recorded: recorded.clone(),
            on_record: on_record.clone(),
        });
        let parent_files = Arc::new(Files::new_with_shared_handler(
            service.clone(),
            handler.clone(),
            zx::Port::create(),
            None,
        ));
        parent_files.insert(1, parent_file);
        let child_files = Arc::new(Files::new_with_shared_handler(
            service,
            handler,
            zx::Port::create(),
            Some((parent_files, 1)),
        ));
        let _pager_thread = child_files.spawn_pager_thread("child-pager");

        // 1. Queue a 4 KiB child page request while the child file is not yet loaded, then insert
        // the child file so `PagerThread` dispatches it through `dispatch_page_request` ->
        // `dispatch_page_range` -> `read_child_range`.
        child_files.handle_page_request(88, 0..4096);
        assert!(child_files.has_queued_page_requests(88));
        child_files.insert(88, child_file);

        {
            let mut guard = recorded.lock();
            while guard.is_empty() {
                on_record.wait(&mut guard);
            }
            assert_eq!(
                &*guard,
                &[RecordedChildSlice {
                    child_key: 88,
                    child_target_offset: 0,
                    parent_byte_range: dev_offset..(dev_offset + TARGET_READ_AHEAD_SIZE),
                    prepared_parent_range: 0..(TARGET_READ_AHEAD_SIZE
                        + DEFAULT_VERIFICATION_ALIGNMENT),
                }]
            );
            guard.clear();
        }

        // 2. Dispatch a 2 MiB request on the loaded child file and verify it is split at the 1 MiB
        // and 2 MiB parent boundaries so each prepared parent range is <= 1 MiB.
        child_files.handle_page_request(88, 0..child_size);
        let slices = recorded.lock().clone();
        assert_eq!(
            slices,
            vec![
                RecordedChildSlice {
                    child_key: 88,
                    child_target_offset: 0,
                    parent_byte_range: dev_offset..mib,
                    prepared_parent_range: 0..mib,
                },
                RecordedChildSlice {
                    child_key: 88,
                    child_target_offset: mib - dev_offset,
                    parent_byte_range: mib..(2 * mib),
                    prepared_parent_range: mib..(2 * mib),
                },
                RecordedChildSlice {
                    child_key: 88,
                    child_target_offset: 2 * mib - dev_offset,
                    parent_byte_range: (2 * mib)..(2 * mib + dev_offset),
                    prepared_parent_range: (2 * mib)..(2 * mib + DEFAULT_VERIFICATION_ALIGNMENT),
                },
            ]
        );
    }

    #[fuchsia::test]
    fn test_lz4_raw_chunks_direct_read() {
        const CHUNK_SIZE: usize = 32768;
        const WINDOW: usize = TARGET_READ_AHEAD_SIZE as usize;
        const BLOB_LEN: usize = WINDOW * 2 + 5000;

        // Only chunk 0 is compressible, so windows 1 and 2 are made up of raw chunks.
        let mut data = vec![0xAAu8; CHUNK_SIZE];
        // Generate pseudo-random incompressible bytes with a linear congruential generator.
        let mut rng_state = 1u64;
        data.extend((CHUNK_SIZE..BLOB_LEN).map(|_| {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng_state >> 32) as u8
        }));

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let archive = ChunkedArchive::new(&data, options).unwrap();

        let mut compressed_offsets = Vec::new();
        let mut compressed_data = Vec::new();
        for (padding, chunk) in archive.chunks_with_padding() {
            compressed_offsets.push(compressed_data.len() as u64);
            compressed_data.extend_from_slice(padding);
            compressed_data.extend_from_slice(&chunk.compressed_data);
        }
        assert_eq!(compressed_offsets, archive.compressed_offsets());

        let stored_size = compressed_data.len() as u64;
        let stored_aligned = stored_size.next_multiple_of(BLOCK_SIZE);
        compressed_data.resize(stored_aligned as usize, 0);

        let extents = Extents::try_new([Extent::new(0..stored_aligned, Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new_lz4_with_raw_chunks(
            CHUNK_SIZE as u64,
            BLOB_LEN as u64,
            stored_size,
            &compressed_offsets,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, BLOB_LEN as u64, compression_info.into()));
        let service = FakeBlockService::new(compressed_data);

        let read_window = |window: usize| {
            let start = (window * WINDOW) as u64;
            let (req, rx) = TestVecBuffer::new_with_range(start..start + 4096);
            file.read_range(&service, req);
            rx
        };

        // Window 0 has a compressed chunk so it's decompressed a chunk at a time.
        let rx = read_window(0);
        assert_eq!(&rx.output()[..], &data[..WINDOW]);
        assert_eq!(rx.commits().len(), 4);

        // Window 1 only has raw chunks so it's read directly.
        let rx = read_window(1);
        assert_eq!(&rx.output()[..], &data[WINDOW..WINDOW * 2]);
        assert_eq!(rx.commits(), vec![(WINDOW as u64, WINDOW)]);

        // Window 2 is a raw partial chunk, which is read directly and zero filled to a page.
        let rx = read_window(2);
        let mut expected = data[WINDOW * 2..].to_vec();
        expected.resize(8192, 0);
        assert_eq!(rx.output(), expected);
        assert_eq!(rx.commits(), vec![((WINDOW * 2) as u64, 8192)]);

        // When page_size > BLOCK_SIZE (e.g. 16 KiB pages), read_range.end exceeds the
        // BLOCK_SIZE-padded EOF extents (8 KiB), so ReadMode::Raw clamps disk reads to the
        // block-padded extents and zero-fills the remaining tail pages in a single commit.
        let (req, rx) =
            TestVecBuffer::new_with_range((WINDOW * 2) as u64..(WINDOW * 2 + 4096) as u64);
        file.read_single_window_for_page_size(
            WINDOW as u64,
            BlockSize::SIZE_16KIB,
            &service,
            req,
            &|_| {},
        );
        let mut expected = data[WINDOW * 2..].to_vec();
        expected.resize(16384, 0);
        assert_eq!(rx.output(), expected);
        assert_eq!(rx.commits(), vec![((WINDOW * 2) as u64, 16384)]);

        // Also test ReadMode::Raw extent clamping with an uncompressed file.
        let uncompressed_len = 5000usize;
        let uncompressed_bytes: Vec<u8> = (0..uncompressed_len).map(|i| (i % 251) as u8).collect();
        let stored_blocks_len = (uncompressed_len as u64).next_multiple_of(BLOCK_SIZE);
        let mut raw_device_data = vec![0xFFu8; stored_blocks_len as usize];
        raw_device_data[..uncompressed_len].copy_from_slice(&uncompressed_bytes);
        let raw_extents =
            Extents::try_new([Extent::new(0..stored_blocks_len, Some(0))], 0).unwrap();
        let raw_file = Arc::new(File::new(raw_extents, uncompressed_len as u64, Transform::None));
        let raw_service = FakeBlockService::new(raw_device_data);
        let (req, rx) = TestVecBuffer::new_with_range(0..4096);
        raw_file.read_single_window_for_page_size(
            16384,
            BlockSize::SIZE_16KIB,
            &raw_service,
            req,
            &|_| {},
        );
        let mut expected_raw = uncompressed_bytes;
        expected_raw.resize(16384, 0);
        assert_eq!(rx.output(), expected_raw);
        assert_eq!(rx.commits(), vec![(0, 16384)]);
    }

    #[fuchsia::test]
    fn test_read_single_window_decompress_16k_page_size_with_8k_chunks() {
        const PAGE_SIZE: BlockSize = BlockSize::SIZE_16KIB;
        const CHUNK_SIZE: usize = 8192;
        const UNCOMPRESSED_LEN: usize = 12000;
        let data: Vec<u8> = (0..UNCOMPRESSED_LEN).map(|i| (i % 251) as u8).collect();
        let options = ChunkedArchiveOptions::V2 {
            minimum_chunk_size: CHUNK_SIZE,
            chunk_alignment: CHUNK_SIZE,
            compression_level: 1,
        };
        let archive = ChunkedArchive::new(&data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = Vec::new();
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let stored_size = compressed_data.len() as u64;
        let stored_aligned = stored_size.next_multiple_of(BLOCK_SIZE);
        compressed_data.resize(stored_aligned as usize, 0);

        let extents = Extents::try_new([Extent::new(0..stored_aligned, Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            CHUNK_SIZE as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, UNCOMPRESSED_LEN as u64, compression_info.into()));
        let service = FakeBlockService::new(compressed_data.clone());

        let (req, rx) = TestVecBuffer::new_with_range(0..4096);
        file.read_single_window_for_page_size(PAGE_SIZE.get(), PAGE_SIZE, &service, req, &|_| {});
        let mut expected = data;
        expected.resize(PAGE_SIZE.get() as usize, 0);
        assert_eq!(rx.output(), expected);
        assert_eq!(rx.commits(), vec![(0, CHUNK_SIZE), (CHUNK_SIZE as u64, CHUNK_SIZE)]);

        // When a window starts at or past `uncompressed_size` (e.g. a 16 KiB page tail on a
        // 24 KiB window alignment), `read_single_window_for_page_size` zero-fills and commits the
        // tail pages instead of dropping `page_request`.
        let exact_chunk_data = vec![0x42u8; CHUNK_SIZE];
        let single_archive = ChunkedArchive::new(&exact_chunk_data, options).unwrap();
        let single_compressed = single_archive.chunks()[0].compressed_data.clone();
        let single_stored_size = single_compressed.len() as u64;
        let single_stored_aligned = single_stored_size.next_multiple_of(BLOCK_SIZE);
        let mut single_device_data = single_compressed;
        single_device_data.resize(single_stored_aligned as usize, 0);

        let single_extents =
            Extents::try_new([Extent::new(0..single_stored_aligned, Some(0))], 0).unwrap();
        let single_info = CompressionInfo::new(
            CHUNK_SIZE as u64,
            single_stored_size,
            &[0],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let single_file =
            Arc::new(File::new(single_extents, CHUNK_SIZE as u64, single_info.into()));
        let single_service = FakeBlockService::new(single_device_data);
        let (req, rx) = TestVecBuffer::new_with_range(CHUNK_SIZE as u64..PAGE_SIZE.get());
        single_file.read_single_window_for_page_size(
            CHUNK_SIZE as u64,
            PAGE_SIZE,
            &single_service,
            req,
            &|_| {},
        );
        assert_eq!(rx.output(), vec![0u8; CHUNK_SIZE]);
        assert_eq!(rx.commits(), vec![(CHUNK_SIZE as u64, CHUNK_SIZE)]);
    }

    #[fuchsia::test]
    fn test_process_mapping_command_orders_registration_before_metadata_buffer_and_drops_buffer_before_commit()
     {
        #[derive(Debug, PartialEq)]
        enum Step {
            PrepareRegisterBlob { key: u64, max_leaf_bytes: usize },
            AllocateMetadataBuffer { max_len: usize },
            CommitRegisterBlob { key: u64, leaf_count: usize, buffer_freed_before_commit: bool },
        }

        const ALLOCATOR_CAPACITY: usize = 4096;

        struct OrderingBlockService {
            allocator: Arc<BufferAllocator>,
            data: Vec<u8>,
            steps: Arc<Mutex<Vec<Step>>>,
        }

        impl BlockService for OrderingBlockService {
            fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
                self.steps.lock().push(Step::AllocateMetadataBuffer { max_len });
                self.allocator.allocate_buffer_sync_owned(max_len)
            }

            fn read_blocks(
                &self,
                device_offset: u64,
                mut dest_buffer: OwnedBuffer,
                on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
            ) -> Result<(), Error> {
                let start = device_offset as usize;
                let end = start + dest_buffer.len();
                dest_buffer.as_mut_ptr_slice().copy_from_slice(&self.data[start..end]);
                on_complete(Ok(dest_buffer));
                Ok(())
            }
        }

        struct OrderingRegistration {
            key: u64,
            allocator: Arc<BufferAllocator>,
            steps: Arc<Mutex<Vec<Step>>>,
        }

        impl BlobRegistration for OrderingRegistration {
            fn commit(
                self,
                merkle_leaves: &[[u8; 32]],
                _read_alignment: usize,
            ) -> Result<(), Error> {
                // The 4096-byte pool only has room for 1 block, so `try_allocate_buffer(4096)`
                // succeeds if and only if `read_blob_metadata` already dropped its `OwnedBuffer`
                // before invoking the registration commit callback.
                let buffer_freed_before_commit =
                    self.allocator.try_allocate_buffer(ALLOCATOR_CAPACITY).is_ok();
                self.steps.lock().push(Step::CommitRegisterBlob {
                    key: self.key,
                    leaf_count: merkle_leaves.len(),
                    buffer_freed_before_commit,
                });
                Ok(())
            }
        }

        struct OrderingDeliveryHandler {
            allocator: Arc<BufferAllocator>,
            steps: Arc<Mutex<Vec<Step>>>,
        }

        impl DeliveryHandler for OrderingDeliveryHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = OrderingRegistration;

            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }

            fn prepare_register_blob(
                &self,
                key: u64,
                max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                self.steps.lock().push(Step::PrepareRegisterBlob { key, max_leaf_bytes });
                Ok(OrderingRegistration {
                    key,
                    allocator: self.allocator.clone(),
                    steps: self.steps.clone(),
                })
            }
        }

        let metadata = BlobMetadata {
            merkle_leaves: vec![[0xAAu8; 32], [0xBBu8; 32]],
            format: BlobFormat::Uncompressed,
        };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let steps = Arc::new(Mutex::new(Vec::new()));
        let allocator = Arc::new(BufferAllocator::new(
            BLOCK_SIZE as usize,
            BufferSource::new(ALLOCATOR_CAPACITY),
        ));
        let service = Arc::new(OrderingBlockService {
            allocator: allocator.clone(),
            data: device_data,
            steps: steps.clone(),
        });
        let handler = OrderingDeliveryHandler { allocator, steps: steps.clone() };
        let files = Arc::new(Files::new(service, handler, zx::Port::create()));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        let offset = payload_buf.offset();
        payload_buf.data().copy_from_slice(&payload_bytes);
        payload_buf
            .commit(crate::RawMappingCommand {
                opcode: crate::MAPPINGS_COMMAND,
                offset,
                key: 77,
                stored_size: 4096,
                device_offset: 0,
                metadata_count: 1,
                extent_count: 1,
            })
            .unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());
        let msg = receiver.claim().unwrap();
        process_mapping_command(msg, &receiver, &files).unwrap();

        assert_eq!(
            &*steps.lock(),
            &[
                Step::PrepareRegisterBlob { key: 77, max_leaf_bytes: BLOCK_SIZE as usize },
                Step::AllocateMetadataBuffer { max_len: BLOCK_SIZE as usize },
                Step::CommitRegisterBlob {
                    key: 77,
                    leaf_count: 2,
                    buffer_freed_before_commit: true,
                },
            ]
        );

        // 2. Verify `metadata_count == 0` on a root session (`!files.has_parent()`):
        // `prepare_register_blob(key, 0)` and synchronous `commit` with zero metadata buffer
        // allocations.
        steps.lock().clear();
        let data_only_words: Vec<u64> = Extents::encode_extents(&data_extents).collect();
        let mut data_only_bytes = Vec::new();
        for w in data_only_words {
            data_only_bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut payload_buf = sender.reserve_payload(data_only_bytes.len()).unwrap();
        let offset = payload_buf.offset();
        payload_buf.data().copy_from_slice(&data_only_bytes);
        payload_buf
            .commit(crate::RawMappingCommand {
                opcode: crate::MAPPINGS_COMMAND,
                offset,
                key: 78,
                stored_size: 4096,
                device_offset: 0,
                metadata_count: 0,
                extent_count: 1,
            })
            .unwrap();
        let msg_78 = receiver.claim().unwrap();
        process_mapping_command(msg_78, &receiver, &files).unwrap();
        assert!(files.is_loaded(78));
        assert_eq!(
            &*steps.lock(),
            &[
                Step::PrepareRegisterBlob { key: 78, max_leaf_bytes: 0 },
                Step::CommitRegisterBlob {
                    key: 78,
                    leaf_count: 0,
                    buffer_freed_before_commit: true,
                },
            ]
        );
    }

    #[fuchsia::test]
    fn test_process_mapping_command_failed_or_corrupt_metadata_drops_registration() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct SingleSlotRegistration {
            key: u64,
            slot_in_use: Arc<AtomicBool>,
            committed_keys: Arc<Mutex<Vec<u64>>>,
        }

        impl BlobRegistration for SingleSlotRegistration {
            fn commit(
                self,
                _merkle_leaves: &[[u8; 32]],
                _read_alignment: usize,
            ) -> Result<(), Error> {
                self.committed_keys.lock().push(self.key);
                Ok(())
            }
        }

        impl Drop for SingleSlotRegistration {
            fn drop(&mut self) {
                assert!(
                    self.slot_in_use.swap(false, Ordering::SeqCst),
                    "SingleSlotRegistration dropped when slot was not marked in use"
                );
            }
        }

        struct SingleSlotDeliveryHandler {
            slot_in_use: Arc<AtomicBool>,
            committed_keys: Arc<Mutex<Vec<u64>>>,
        }

        impl DeliveryHandler for SingleSlotDeliveryHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = SingleSlotRegistration;

            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }

            fn prepare_register_blob(
                &self,
                key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                if self.slot_in_use.swap(true, Ordering::SeqCst) {
                    bail!("delivery_vmo registration slot was not freed by previous failed read");
                }
                Ok(SingleSlotRegistration {
                    key,
                    slot_in_use: self.slot_in_use.clone(),
                    committed_keys: self.committed_keys.clone(),
                })
            }
        }

        struct ConfigurableMetadataBlockService {
            allocator: Arc<BufferAllocator>,
            corrupt_data: Vec<u8>,
            valid_data: Vec<u8>,
            mode: Mutex<u8>, // 0 = I/O error, 1 = corrupt bincode, 2 = valid metadata
            pending: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
        }

        impl BlockService for ConfigurableMetadataBlockService {
            fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
                self.allocator.allocate_buffer_sync_owned(max_len)
            }

            fn read_blocks(
                &self,
                _device_offset: u64,
                mut dest_buffer: OwnedBuffer,
                on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
            ) -> Result<(), Error> {
                let mode = *self.mode.lock();
                let len = dest_buffer.len();
                match mode {
                    0 => {
                        self.pending.lock().push(Box::new(move || {
                            drop(dest_buffer);
                            on_complete(Err(anyhow!("simulated metadata block read I/O error")));
                        }));
                    }
                    1 => {
                        dest_buffer.as_mut_ptr_slice().copy_from_slice(&self.corrupt_data[..len]);
                        self.pending.lock().push(Box::new(move || {
                            on_complete(Ok(dest_buffer));
                        }));
                    }
                    _ => {
                        dest_buffer.as_mut_ptr_slice().copy_from_slice(&self.valid_data[..len]);
                        self.pending.lock().push(Box::new(move || {
                            on_complete(Ok(dest_buffer));
                        }));
                    }
                }
                Ok(())
            }
        }

        let valid_metadata =
            BlobMetadata { merkle_leaves: vec![[0xCCu8; 32]], format: BlobFormat::Uncompressed };
        let encoded_valid = serialize_metadata(&valid_metadata);
        let mut valid_data = vec![0u8; BLOCK_SIZE as usize];
        valid_data[..encoded_valid.len()].copy_from_slice(&encoded_valid);
        let corrupt_data = vec![0xFFu8; BLOCK_SIZE as usize];

        let allocator = Arc::new(BufferAllocator::new(
            BLOCK_SIZE as usize,
            BufferSource::new(BLOCK_SIZE as usize),
        ));
        let service = Arc::new(ConfigurableMetadataBlockService {
            allocator,
            corrupt_data,
            valid_data,
            mode: Mutex::new(0),
            pending: Mutex::new(Vec::new()),
        });
        let slot_in_use = Arc::new(AtomicBool::new(false));
        let committed_keys = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Files::new(
            service.clone(),
            SingleSlotDeliveryHandler {
                slot_in_use: slot_in_use.clone(),
                committed_keys: committed_keys.clone(),
            },
            zx::Port::create(),
        ));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let send_and_process =
            |key: u64,
             sender: &mut vmo_fifo::SyncSender<crate::RawMappingCommand>,
             receiver: &Arc<vmo_fifo::Receiver<crate::RawMappingCommand>>| {
                let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
                let offset = payload_buf.offset();
                payload_buf.data().copy_from_slice(&payload_bytes);
                payload_buf
                    .commit(crate::RawMappingCommand {
                        opcode: crate::MAPPINGS_COMMAND,
                        offset,
                        key,
                        stored_size: BLOCK_SIZE,
                        device_offset: 0,
                        metadata_count: 1,
                        extent_count: 1,
                    })
                    .unwrap();
                let msg = receiver.claim().unwrap();
                process_mapping_command(msg, receiver, &files).unwrap();
            };

        // 1. Metadata I/O error: `prepare_register_blob` reserves the single slot, and when the
        // block read fails, the captured `BlobRegistration` is dropped and frees the slot.
        *service.mode.lock() = 0;
        send_and_process(80, &mut sender, &receiver);
        assert!(slot_in_use.load(Ordering::SeqCst));
        for cb in std::mem::take(&mut *service.pending.lock()) {
            cb();
        }
        assert!(!slot_in_use.load(Ordering::SeqCst));
        assert!(!files.is_loading(80));
        assert!(!files.is_loaded(80));
        assert!(committed_keys.lock().is_empty());

        // 2. Corrupt bincode deserialization error: `prepare_register_blob` reuses the freed slot,
        // and when deserialization fails, `BlobRegistration` is dropped and frees the slot again.
        *service.mode.lock() = 1;
        send_and_process(81, &mut sender, &receiver);
        assert!(slot_in_use.load(Ordering::SeqCst));
        for cb in std::mem::take(&mut *service.pending.lock()) {
            cb();
        }
        assert!(!slot_in_use.load(Ordering::SeqCst));
        assert!(!files.is_loading(81));
        assert!(!files.is_loaded(81));
        assert!(committed_keys.lock().is_empty());

        // 3. Valid metadata read succeeds using the freed slot and commits registration.
        *service.mode.lock() = 2;
        send_and_process(82, &mut sender, &receiver);
        for cb in std::mem::take(&mut *service.pending.lock()) {
            cb();
        }
        assert!(!slot_in_use.load(Ordering::SeqCst));
        assert!(files.is_loaded(82));
        assert_eq!(&*committed_keys.lock(), &[82]);
    }

    #[fuchsia::test]
    fn test_aligned_read_range_for_page_size_overflow() {
        let extents = Extents::try_new(std::iter::empty::<Extent>(), 0).unwrap();
        let file = File::new(extents, u64::MAX, Transform::None);
        const PAGE_SIZE: BlockSize = BlockSize::SIZE_4KIB;
        assert_eq!(file.aligned_read_range_for_page_size(0..4096, 4096, PAGE_SIZE), None);
    }

    #[fuchsia::test]
    fn test_close_blob_while_metadata_loading_prevents_commit_and_reinsert() {
        struct TrackingRegistration {
            key: u64,
            committed_keys: Arc<Mutex<Vec<u64>>>,
        }
        impl BlobRegistration for TrackingRegistration {
            fn commit(
                self,
                _merkle_leaves: &[[u8; 32]],
                _read_alignment: usize,
            ) -> Result<(), Error> {
                self.committed_keys.lock().push(self.key);
                Ok(())
            }
        }
        struct TrackingDeliveryHandler {
            committed_keys: Arc<Mutex<Vec<u64>>>,
        }
        impl DeliveryHandler for TrackingDeliveryHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = TrackingRegistration;
            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }
            fn prepare_register_blob(
                &self,
                key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                Ok(TrackingRegistration { key, committed_keys: self.committed_keys.clone() })
            }
        }

        let metadata =
            BlobMetadata { merkle_leaves: vec![[0x11u8; 32]], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let committed_keys = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Files::new(
            service.clone(),
            TrackingDeliveryHandler { committed_keys: committed_keys.clone() },
            zx::Port::create(),
        ));

        // Pre-populate key 20 as already loaded so we also test re-mapping an already-loaded key
        // followed by CloseBlob while the new metadata read is in flight.
        let initial_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        files.insert(20, Arc::new(File::new(initial_extents, BLOCK_SIZE, Transform::None)));
        assert!(files.is_loaded(20));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        for key in [10u64, 20u64] {
            let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
            let offset = payload_buf.offset();
            payload_buf.data().copy_from_slice(&payload_bytes);
            payload_buf
                .commit(crate::RawMappingCommand {
                    opcode: crate::MAPPINGS_COMMAND,
                    offset,
                    key,
                    stored_size: BLOCK_SIZE,
                    device_offset: 0,
                    metadata_count: 1,
                    extent_count: 1,
                })
                .unwrap();
            let msg = receiver.claim().unwrap();
            process_mapping_command(msg, &receiver, &files).unwrap();
            if key == 10 {
                assert!(files.is_loading(key));
            } else {
                assert!(files.is_loaded(key));
            }

            // Close the blob before its metadata read completes.
            sender
                .push(crate::RawMappingCommand {
                    opcode: crate::CLOSE_BLOB_COMMAND,
                    offset: 0,
                    key,
                    stored_size: 0,
                    device_offset: 0,
                    metadata_count: 0,
                    extent_count: 0,
                })
                .unwrap();
            let close_msg = receiver.claim().unwrap();
            process_mapping_command(close_msg, &receiver, &files).unwrap();
            assert!(!files.is_loading(key));
            assert!(!files.is_loaded(key));
        }

        // Complete both pending metadata reads; neither closed key may be registered or
        // re-inserted into `files`.
        service.wait_and_trigger_sync();
        assert!(committed_keys.lock().is_empty());
        assert!(!files.is_loaded(10));
        assert!(!files.is_loaded(20));
    }

    #[fuchsia::test]
    async fn test_remap_same_key_without_close_blob_and_commit_failure() {
        struct ControlledRegistration {
            key: u64,
            fail_commit: bool,
            committed: Arc<Mutex<Vec<(u64, Vec<[u8; 32]>)>>>,
        }
        impl BlobRegistration for ControlledRegistration {
            fn commit(
                self,
                merkle_leaves: &[[u8; 32]],
                _read_alignment: usize,
            ) -> Result<(), Error> {
                if self.fail_commit {
                    bail!("simulated registration commit failure for key {}", self.key);
                }
                self.committed.lock().push((self.key, merkle_leaves.to_vec()));
                Ok(())
            }
        }
        struct ControlledDeliveryHandler {
            fail_prepare: Mutex<bool>,
            fail_commit: Mutex<bool>,
            committed: Arc<Mutex<Vec<(u64, Vec<[u8; 32]>)>>>,
            unregistered: Arc<Mutex<Vec<u64>>>,
        }
        impl DeliveryHandler for ControlledDeliveryHandler {
            type Request = TestVecBuffer;
            type BlobRegistration = ControlledRegistration;
            fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
                TestVecBuffer::new_with_range(range).0
            }
            fn prepare_register_blob(
                &self,
                key: u64,
                _max_leaf_bytes: usize,
            ) -> Result<Self::BlobRegistration, Error> {
                if *self.fail_prepare.lock() {
                    bail!("simulated prepare_register_blob failure for key {key}");
                }
                Ok(ControlledRegistration {
                    key,
                    fail_commit: *self.fail_commit.lock(),
                    committed: self.committed.clone(),
                })
            }
            fn unregister_file(&self, key: u64) {
                self.unregistered.lock().push(key);
            }
        }

        struct PerReadBlockService {
            allocator: Arc<BufferAllocator>,
            data: Vec<u8>,
            pending:
                Mutex<Vec<(u64, OwnedBuffer, Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>)>>,
        }
        impl BlockService for PerReadBlockService {
            fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
                self.allocator.allocate_buffer_sync_owned(max_len)
            }
            fn read_blocks(
                &self,
                device_offset: u64,
                dest_buffer: OwnedBuffer,
                on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
            ) -> Result<(), Error> {
                self.pending.lock().push((device_offset, dest_buffer, on_complete));
                Ok(())
            }
        }
        impl PerReadBlockService {
            fn complete_at(&self, index: usize, ok: bool) {
                let (device_offset, mut buf, cb) = self.pending.lock().remove(index);
                if ok {
                    let start = device_offset as usize;
                    let len = buf.len();
                    buf.as_mut_ptr_slice().copy_from_slice(&self.data[start..start + len]);
                    cb(Ok(buf));
                } else {
                    drop(buf);
                    cb(Err(anyhow!("simulated metadata read failure")));
                }
            }
        }

        let meta_1 =
            BlobMetadata { merkle_leaves: vec![[0xAAu8; 32]], format: BlobFormat::Uncompressed };
        let meta_2 =
            BlobMetadata { merkle_leaves: vec![[0xBBu8; 32]], format: BlobFormat::Uncompressed };
        let encoded_1 = serialize_metadata(&meta_1);
        let encoded_2 = serialize_metadata(&meta_2);
        let mut device_data = vec![0u8; (4 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_1.len()]
            .copy_from_slice(&encoded_1);
        device_data[(2 * BLOCK_SIZE) as usize..(2 * BLOCK_SIZE) as usize + encoded_2.len()]
            .copy_from_slice(&encoded_2);

        let allocator = Arc::new(BufferAllocator::new(
            BLOCK_SIZE as usize,
            BufferSource::new((8 * BLOCK_SIZE) as usize),
        ));
        let service = Arc::new(PerReadBlockService {
            allocator,
            data: device_data,
            pending: Mutex::new(Vec::new()),
        });
        let committed = Arc::new(Mutex::new(Vec::new()));
        let unregistered = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Files::new(
            service.clone(),
            ControlledDeliveryHandler {
                fail_prepare: Mutex::new(false),
                fail_commit: Mutex::new(false),
                committed: committed.clone(),
                unregistered: unregistered.clone(),
            },
            zx::Port::create(),
        ));

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            vmo_fifo::Alignment::SIZE_1KIB,
            16,
        )
        .unwrap();
        let receiver =
            Arc::new(vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap());

        let send_mapping = |key: u64,
                            meta_block: u64,
                            stored_size: u64,
                            sender: &mut vmo_fifo::SyncSender<crate::RawMappingCommand>,
                            receiver: &Arc<vmo_fifo::Receiver<crate::RawMappingCommand>>|
         -> Result<(), Error> {
            let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
            let meta_extents =
                Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(meta_block * BLOCK_SIZE))], 0)
                    .unwrap();
            let mut payload_bytes = Vec::new();
            for w in
                Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
            {
                payload_bytes.extend_from_slice(&w.to_le_bytes());
            }
            let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
            let offset = payload_buf.offset();
            payload_buf.data().copy_from_slice(&payload_bytes);
            payload_buf
                .commit(crate::RawMappingCommand {
                    opcode: crate::MAPPINGS_COMMAND,
                    offset,
                    key,
                    stored_size,
                    device_offset: 0,
                    metadata_count: 1,
                    extent_count: 1,
                })
                .unwrap();
            let msg = receiver.claim().unwrap();
            process_mapping_command(msg, receiver, &files)
        };

        // 1. Re-mapping `Mappings #1 -> Mappings #2` on key 100 without `CloseBlob`, where `#1`
        // completes before `#2`: `#1` must be ignored and queued page requests + `wait_for_file`
        // waiters must remain attached until `#2` completes.
        send_mapping(100, 1, BLOCK_SIZE, &mut sender, &receiver).unwrap();
        files.handle_page_request(100, 0..BLOCK_SIZE);
        let mut waiter_100 = std::pin::pin!(files.wait_for_file(100));
        assert!(futures::poll!(&mut waiter_100).is_pending());
        send_mapping(100, 2, 2 * BLOCK_SIZE, &mut sender, &receiver).unwrap();
        assert_eq!(service.pending.lock().len(), 2);

        // Complete `#1` first; it must not commit or resolve the waiter.
        service.complete_at(0, true);
        assert!(committed.lock().is_empty());
        assert!(files.is_loading(100));
        assert!(files.has_queued_page_requests(100));
        assert!(futures::poll!(&mut waiter_100).is_pending());

        // Complete `#2`; it must commit `meta_2`, move the queued page request to
        // `deferred_page_requests`, and resolve the waiter with `2 * BLOCK_SIZE`.
        service.complete_at(0, true);
        assert!(files.is_loaded(100));
        assert_eq!(&*committed.lock(), &[(100, vec![[0xBBu8; 32]])]);
        assert_eq!(files.deferred_page_requests.lock().len(), 1);
        files.deferred_page_requests.lock().clear();
        let loaded_100 = waiter_100.await.unwrap();
        assert_eq!(loaded_100.uncompressed_size(), 2 * BLOCK_SIZE);

        // 2. Re-mapping `Mappings #1 -> Mappings #2` on key 101 without `CloseBlob`, where `#1`
        // fails while `#2` is in flight, and also where `#2` completes before `#1`:
        committed.lock().clear();
        send_mapping(101, 1, BLOCK_SIZE, &mut sender, &receiver).unwrap();
        send_mapping(101, 2, 2 * BLOCK_SIZE, &mut sender, &receiver).unwrap();
        // Fail `#1` while `#2` is still loading; `#2` must remain loading and not be unregistered.
        service.complete_at(0, false);
        assert!(files.is_loading(101));
        assert!(unregistered.lock().is_empty());
        // Complete `#2`.
        service.complete_at(0, true);
        assert!(files.is_loaded(101));
        assert_eq!(files.get_file(101).unwrap().uncompressed_size(), 2 * BLOCK_SIZE);

        // Now test `#2` completing BEFORE `#1` on key 102:
        committed.lock().clear();
        send_mapping(102, 1, BLOCK_SIZE, &mut sender, &receiver).unwrap();
        send_mapping(102, 2, 2 * BLOCK_SIZE, &mut sender, &receiver).unwrap();
        // Complete `#2` (index 1) first, then complete `#1` (index 0): `#1` must not overwrite `#2`.
        service.complete_at(1, true);
        assert_eq!(files.get_file(102).unwrap().uncompressed_size(), 2 * BLOCK_SIZE);
        service.complete_at(0, true);
        assert_eq!(files.get_file(102).unwrap().uncompressed_size(), 2 * BLOCK_SIZE);
        assert_eq!(&*committed.lock(), &[(102, vec![[0xBBu8; 32]])]);

        // 3. `prepare_register_blob` failing on an unmapped/loading key must leave `files`
        // unchanged, while overwriting already-loaded key 102 skips `prepare_register_blob`.
        *files.delivery_handler().fail_prepare.lock() = true;
        assert!(send_mapping(104, 1, BLOCK_SIZE, &mut sender, &receiver).is_err());
        assert!(!files.is_loading(104));
        assert!(!files.is_loaded(104));
        send_mapping(102, 1, BLOCK_SIZE, &mut sender, &receiver).unwrap();
        service.complete_at(0, true);
        *files.delivery_handler().fail_prepare.lock() = false;
        assert!(files.is_loaded(102));
        assert_eq!(files.get_file(102).unwrap().uncompressed_size(), BLOCK_SIZE);

        // 4. `registration.commit` failing in `commit_loading` on key 103 must remove `key` from
        // `files`, cancel `wait_for_file`, and call `unregister_file(103)`.
        *files.delivery_handler().fail_commit.lock() = true;
        send_mapping(103, 1, BLOCK_SIZE, &mut sender, &receiver).unwrap();
        let mut waiter_103 = std::pin::pin!(files.wait_for_file(103));
        assert!(futures::poll!(&mut waiter_103).is_pending());
        service.complete_at(0, true);
        *files.delivery_handler().fail_commit.lock() = false;
        assert!(!files.is_loading(103));
        assert!(!files.is_loaded(103));
        assert_eq!(&*unregistered.lock(), &[103]);
        assert!(waiter_103.await.is_err());
    }

    struct TrackingRegistration {
        registered_alignments: Arc<Mutex<Vec<usize>>>,
        fail_commit: bool,
    }

    impl BlobRegistration for TrackingRegistration {
        fn commit(self, _merkle_leaves: &[[u8; 32]], read_alignment: usize) -> Result<(), Error> {
            if self.fail_commit {
                bail!("Simulated registration commit failure");
            }
            self.registered_alignments.lock().push(read_alignment);
            Ok(())
        }
    }

    #[derive(Default)]
    struct TrackingDeliveryHandler {
        registered_alignments: Arc<Mutex<Vec<usize>>>,
        prepared_registrations: AtomicUsize,
        fail_next_registration_commit: AtomicBool,
        unregister_count: AtomicUsize,
        page_requests: Mutex<VecDeque<TestVecBuffer>>,
        child_page_requests: Mutex<VecDeque<TestVecBuffer>>,
    }

    impl DeliveryHandler for TrackingDeliveryHandler {
        type Request = TestVecBuffer;
        type BlobRegistration = TrackingRegistration;

        fn get_page_request(self: &Arc<Self>, _key: u64, range: Range<u64>) -> Self::Request {
            self.page_requests
                .lock()
                .pop_front()
                .unwrap_or_else(|| TestVecBuffer::new_with_range(range).0)
        }

        fn get_child_page_request(
            self: &Arc<Self>,
            _child_key: u64,
            _child_target_offset: u64,
            parent_byte_range: Range<u64>,
        ) -> Self::Request {
            self.child_page_requests
                .lock()
                .pop_front()
                .unwrap_or_else(|| TestVecBuffer::new_with_range(parent_byte_range).0)
        }

        fn prepare_register_blob(
            &self,
            _key: u64,
            _max_leaf_bytes: usize,
        ) -> Result<Self::BlobRegistration, Error> {
            self.prepared_registrations.fetch_add(1, Ordering::SeqCst);
            Ok(TrackingRegistration {
                registered_alignments: Arc::clone(&self.registered_alignments),
                fail_commit: self.fail_next_registration_commit.swap(false, Ordering::SeqCst),
            })
        }

        fn unregister_file(&self, _key: u64) {
            self.unregister_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct TestMappingFifo {
        sender: vmo_fifo::AsyncSender<crate::RawMappingCommand>,
        receiver: Arc<vmo_fifo::Receiver<crate::RawMappingCommand>>,
    }

    impl TestMappingFifo {
        fn new() -> Self {
            let vmo = zx::Vmo::create(65536).expect("create VMO failed");
            let sender = vmo_fifo::AsyncSender::<crate::RawMappingCommand>::new(
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate VMO failed"),
                vmo_fifo::Alignment::SIZE_1KIB,
                16,
            )
            .expect("create AsyncSender failed");
            let receiver = Arc::new(
                vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16)
                    .expect("create Receiver failed"),
            );
            Self { sender, receiver }
        }

        async fn push_extents_mapping(
            &mut self,
            key: u64,
            data_extents: &Extents,
            meta_extents: &Extents,
            stored_size: u64,
        ) -> u64 {
            let mut payload_bytes = Vec::new();
            let mut extent_count = 0u32;
            for w in Extents::encode_extents(data_extents) {
                payload_bytes.extend_from_slice(&w.to_le_bytes());
                extent_count += 1;
            }
            let mut metadata_count = 0u32;
            for w in Extents::encode_extents(meta_extents) {
                payload_bytes.extend_from_slice(&w.to_le_bytes());
                metadata_count += 1;
            }
            let mut payload_buf = self
                .sender
                .reserve_payload(payload_bytes.len())
                .await
                .expect("reserve_payload failed");
            payload_buf.data().copy_from_slice(&payload_bytes);
            let cmd = crate::RawMappingCommand {
                opcode: crate::MAPPINGS_COMMAND,
                offset: payload_buf.offset(),
                key,
                stored_size,
                device_offset: 0,
                metadata_count,
                extent_count,
            };
            payload_buf.commit(cmd).await.expect("commit failed")
        }

        async fn push_mapping(
            &mut self,
            key: u64,
            data_block: u64,
            meta_block: u64,
            stored_size: u64,
        ) -> u64 {
            let data_extents =
                Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(data_block * BLOCK_SIZE))], 0)
                    .expect("data extents");
            let meta_extents =
                Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(meta_block * BLOCK_SIZE))], 0)
                    .expect("meta extents");
            self.push_extents_mapping(key, &data_extents, &meta_extents, stored_size).await
        }

        async fn push_close(&mut self, key: u64) -> u64 {
            self.sender
                .push(crate::MappingCommand::CloseBlob { key }.into())
                .await
                .expect("push CloseBlob failed")
        }

        fn process_next<S, D>(&self, files: &Arc<Files<S, D>>)
        where
            S: BlockService + ?Sized + 'static,
            D: DeliveryHandler,
        {
            let msg = self.receiver.claim().expect("claim failed");
            process_mapping_command(msg, &self.receiver, files)
                .expect("process_mapping_command failed");
        }
    }

    #[fuchsia::test]
    async fn test_overwrite_keeps_old_file_and_retires_slot_after_in_flight_read() {
        let blob_key = 55;
        let old_data = vec![0xAAu8; BLOCK_SIZE as usize];
        let new_data = vec![0xBBu8; BLOCK_SIZE as usize];

        let metadata =
            BlobMetadata { merkle_leaves: vec![[7u8; 32]], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);

        // Layout on mock device:
        // Block 0: old data (0xAA), Block 1: old metadata
        // Block 2: new data (0xBB), Block 3: new metadata
        let mut device_data = vec![0u8; (4 * BLOCK_SIZE) as usize];
        device_data[..BLOCK_SIZE as usize].copy_from_slice(&old_data);
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(2 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize]
            .copy_from_slice(&new_data);
        device_data[(3 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let (req1, rx1) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let (req2, rx2) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let (req3, rx3) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);

        let handler = TrackingDeliveryHandler {
            page_requests: Mutex::new(VecDeque::from([req1, req2, req3])),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let _pager_thread = files.spawn_pager_thread("pager");
        let mut fifo = TestMappingFifo::new();

        // Load the initial mapping (blocks 0 and 1).
        let slot0 = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        let mut wait_slot0 = pin!(fifo.sender.wait_until_popped(slot0));
        fifo.process_next(&files);
        assert!(futures::poll!(&mut wait_slot0).is_pending());
        service.wait_and_trigger_sync();
        wait_slot0.await.expect("initial mapping slot should pop once loaded");
        assert_eq!(
            *files.delivery_handler.registered_alignments.lock(),
            vec![DEFAULT_VERIFICATION_ALIGNMENT as usize]
        );

        // Start an in-flight block read on `old_file` without completing it yet.
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 1);

        // Push and process the overwrite mapping (blocks 2 and 3) synchronously: `old_file` must
        // remain `Loaded` while the overwrite's metadata read is in flight, and `slot1` must stay
        // unpopped.
        let slot1 = fifo.push_mapping(blob_key, 2, 3, BLOCK_SIZE).await;
        let mut wait_slot1 = pin!(fifo.sender.wait_until_popped(slot1));
        fifo.process_next(&files);
        assert!(files.is_loaded(blob_key));
        assert!(futures::poll!(&mut wait_slot1).is_pending());
        assert_eq!(service.pending.lock().len(), 2);

        // A page request arriving while the overwrite's metadata read is in flight is served
        // immediately by `old_file` without blocking in a loading queue.
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 3);

        // Complete the overwrite's metadata read (index 1 in `service.pending`) while leaving both
        // data reads on `old_file` (indices 0 and 2) in flight.
        {
            let cb = service.pending.lock().remove(1);
            cb();
        }
        // `new_file` is now installed, `prepare_register_blob` / `commit` were not called a second
        // time, and `slot1` is still unpopped because `old_file` has two in-flight data reads.
        assert_eq!(files.delivery_handler.prepared_registrations.load(Ordering::SeqCst), 1);
        assert_eq!(
            *files.delivery_handler.registered_alignments.lock(),
            vec![DEFAULT_VERIFICATION_ALIGNMENT as usize]
        );
        assert_eq!(files.delivery_handler.unregister_count.load(Ordering::SeqCst), 0);
        assert!(futures::poll!(&mut wait_slot1).is_pending());

        // Complete the two in-flight data reads on `old_file`; once both finish, `old_file` drops
        // and retires `slot1`.
        service.wait_and_trigger_sync();
        assert_eq!(rx1.output(), old_data);
        assert_eq!(rx2.output(), old_data);
        wait_slot1.await.expect("overwrite slot should pop once in-flight reads on old_file end");

        // Subsequent page requests read from `new_file`.
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        service.wait_and_trigger_sync();
        assert_eq!(rx3.wait_for_commits(1), vec![(0, BLOCK_SIZE as usize)]);
        assert_eq!(rx3.output(), new_data);
    }

    #[fuchsia::test]
    async fn test_overwrite_during_in_flight_metadata_read_carries_over_requests() {
        let blob_key = 77;
        let old_data = vec![0x11u8; BLOCK_SIZE as usize];
        let new_data = vec![0x22u8; BLOCK_SIZE as usize];

        let metadata =
            BlobMetadata { merkle_leaves: vec![[0x33u8; 32]], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);

        // Block 0: old data (0x11), Block 1: old metadata
        // Block 2: new data (0x22), Block 3: new metadata
        let mut device_data = vec![0u8; (4 * BLOCK_SIZE) as usize];
        device_data[..BLOCK_SIZE as usize].copy_from_slice(&old_data);
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(2 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize]
            .copy_from_slice(&new_data);
        device_data[(3 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let (req1, rx1) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let (req2, rx2) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);

        let handler = TrackingDeliveryHandler {
            page_requests: Mutex::new(VecDeque::from([req1, req2])),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let _pager_thread = files.spawn_pager_thread("pager");
        let mut fifo = TestMappingFifo::new();

        // Start the initial mapping (blocks 0 and 1) and queue a page request while its metadata
        // read is still in flight.
        let slot0 = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        fifo.process_next(&files);
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);

        // Push and process the overwrite mapping (blocks 2 and 3) while the first metadata read is
        // still in flight, then queue a second page request.
        let slot1 = fifo.push_mapping(blob_key, 2, 3, BLOCK_SIZE).await;
        fifo.process_next(&files);
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 2);

        // Complete the first metadata read; because its generation was superseded, it must skip
        // `register_blob` and `commit` and retire `slot0`.
        {
            let cb = service.pending.lock().remove(0);
            cb();
        }
        fifo.sender.wait_until_popped(slot0).await.expect("slot0 should pop");
        assert!(files.delivery_handler.registered_alignments.lock().is_empty());

        // Complete the second metadata read (`register_blob` is now committed once for the
        // replacement file while `prepare_register_blob` ran twice across both `Loading` opens),
        // then wait for `PagerThread` to submit both data reads and complete them.
        service.wait_and_trigger_sync();
        fifo.sender.wait_until_popped(slot1).await.expect("slot1 should pop");
        assert_eq!(files.delivery_handler.prepared_registrations.load(Ordering::SeqCst), 2);
        assert_eq!(
            *files.delivery_handler.registered_alignments.lock(),
            vec![DEFAULT_VERIFICATION_ALIGNMENT as usize]
        );
        service.wait_for_pending(2);
        service.wait_and_trigger_sync();

        assert_eq!(rx1.wait_for_commits(1), vec![(0, BLOCK_SIZE as usize)]);
        assert_eq!(rx1.output(), new_data);
        assert_eq!(rx2.wait_for_commits(1), vec![(0, BLOCK_SIZE as usize)]);
        assert_eq!(rx2.output(), new_data);
    }

    #[fuchsia::test]
    async fn test_close_blob_cancels_in_flight_metadata_read_and_waits_for_in_flight_reads() {
        let blob_key = 78;
        let data = vec![0xCCu8; BLOCK_SIZE as usize];
        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[..BLOCK_SIZE as usize].copy_from_slice(&data);
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let (req1, rx1) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let handler = TrackingDeliveryHandler {
            page_requests: Mutex::new(VecDeque::from([req1])),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let mut fifo = TestMappingFifo::new();

        // 1. Start an initial mapping; its metadata read is now pending on disk.
        let _ = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        fifo.process_next(&files);
        assert!(files.is_loading(blob_key));

        // Process `CloseBlob` while the metadata read is in flight. Because the initial `Mappings`
        // command's `SlotGuard` is held by `LoadingFileGuard` until the in-flight metadata read
        // finishes, `close_slot1` remains unpopped until the metadata read completes.
        let close_slot1 = fifo.push_close(blob_key).await;
        let mut wait_close1 = pin!(fifo.sender.wait_until_popped(close_slot1));
        fifo.process_next(&files);
        assert!(!files.is_loading(blob_key));
        assert!(futures::poll!(&mut wait_close1).is_pending());

        // Complete the metadata read; `commit` must not re-insert the closed file, and both slots
        // now retire.
        service.wait_and_trigger_sync();
        assert!(!files.is_loading(blob_key));
        assert!(!files.is_loaded(blob_key));
        wait_close1
            .await
            .expect("close_slot1 should pop once the in-flight metadata read completes");

        // 2. Load the blob, start an in-flight data read, and verify `CloseBlob`'s slot stays
        // unpopped until the in-flight data read finishes.
        let _ = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        fifo.process_next(&files);
        service.wait_and_trigger_sync();
        assert!(files.is_loaded(blob_key));

        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 1);

        let close_slot2 = fifo.push_close(blob_key).await;
        let mut wait_close2 = pin!(fifo.sender.wait_until_popped(close_slot2));
        fifo.process_next(&files);
        assert!(!files.is_loaded(blob_key));
        assert!(futures::poll!(&mut wait_close2).is_pending());

        service.wait_and_trigger_sync();
        assert_eq!(rx1.output(), data);
        wait_close2.await.expect("close_slot2 should pop once in-flight read completes");
    }

    #[fuchsia::test]
    async fn test_superseded_or_failed_overwrite_holds_slot_until_old_file_read_completes() {
        let blob_key = 79;
        let old_data = vec![0xAAu8; BLOCK_SIZE as usize];
        let new_data = vec![0xBBu8; BLOCK_SIZE as usize];
        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);

        // Block 0: old data, Block 1: valid metadata, Block 2: new data, Block 3: valid metadata,
        // Block 4: corrupt metadata (0xFF bytes).
        let mut device_data = vec![0u8; (5 * BLOCK_SIZE) as usize];
        device_data[..BLOCK_SIZE as usize].copy_from_slice(&old_data);
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(2 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize]
            .copy_from_slice(&new_data);
        device_data[(3 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(4 * BLOCK_SIZE) as usize..].fill(0xFF);

        let service = DelayedBlockService::new(device_data);
        let (req1, rx1) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let (req2, rx2) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let handler = TrackingDeliveryHandler {
            page_requests: Mutex::new(VecDeque::from([req1, req2])),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let mut fifo = TestMappingFifo::new();

        // Case 1: Overwrite superseded by `CloseBlob` while `old_file` still has an in-flight read.
        let _ = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        fifo.process_next(&files);
        service.wait_and_trigger_sync();
        assert!(files.is_loaded(blob_key));

        // Start an in-flight data read on `old_file` (pending index 0).
        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 1);

        // Push Overwrite 1 (pending index 1 = metadata read for Overwrite 1), then supersede it
        // with `CloseBlob`.
        let overwrite_slot1 = fifo.push_mapping(blob_key, 2, 3, BLOCK_SIZE).await;
        fifo.process_next(&files);
        assert_eq!(service.pending.lock().len(), 2);

        let close_slot = fifo.push_close(blob_key).await;
        fifo.process_next(&files);
        assert!(!files.is_loaded(blob_key));

        // Complete Overwrite 1's superseded metadata read BEFORE `old_file`'s data read finishes.
        // `overwrite_slot1` must remain unpopped until `old_file`'s data read completes!
        {
            let metadata_cb = service.pending.lock().remove(1);
            metadata_cb();
        }
        let mut wait_overwrite1 = pin!(fifo.sender.wait_until_popped(overwrite_slot1));
        let mut wait_close = pin!(fifo.sender.wait_until_popped(close_slot));
        assert!(futures::poll!(&mut wait_overwrite1).is_pending());
        assert!(futures::poll!(&mut wait_close).is_pending());

        // Now complete `old_file`'s in-flight data read; both slots retire.
        service.wait_and_trigger_sync();
        assert_eq!(rx1.output(), old_data);
        wait_overwrite1.await.expect("overwrite_slot1 should pop after old_file read finishes");
        wait_close.await.expect("close_slot should pop after old_file read finishes");

        // Case 2: Overwrite metadata read fails (corrupt Block 4) while `old_file` has an in-flight
        // data read. `old_file` must be removed from `files` and `overwrite_slot2` must stay
        // unpopped until `old_file`'s in-flight data read completes.
        let _ = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        fifo.process_next(&files);
        service.wait_and_trigger_sync();
        assert!(files.is_loaded(blob_key));

        files.handle_page_request(blob_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 1);

        let overwrite_slot2 = fifo.push_mapping(blob_key, 2, 4, BLOCK_SIZE).await;
        fifo.process_next(&files);
        assert_eq!(service.pending.lock().len(), 2);

        // Complete the corrupt metadata read first.
        {
            let bad_metadata_cb = service.pending.lock().remove(1);
            bad_metadata_cb();
        }
        assert!(!files.is_loaded(blob_key));
        let mut wait_overwrite2 = pin!(fifo.sender.wait_until_popped(overwrite_slot2));
        assert!(futures::poll!(&mut wait_overwrite2).is_pending());

        // Complete `old_file`'s data read; `overwrite_slot2` now pops cleanly.
        service.wait_and_trigger_sync();
        assert_eq!(rx2.output(), old_data);
        wait_overwrite2.await.expect("overwrite_slot2 should pop after old_file read finishes");
    }

    #[fuchsia::test]
    async fn test_child_files_resolves_overwritten_parent_and_holds_parent_slot() {
        let parent_key = 90;
        let child_key = 1;
        let old_data = vec![0xAAu8; BLOCK_SIZE as usize];
        let new_data = vec![0xBBu8; BLOCK_SIZE as usize];
        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);

        // Block 0: old parent data, Block 1: metadata, Block 2: new parent data, Block 3: metadata.
        let mut device_data = vec![0u8; (4 * BLOCK_SIZE) as usize];
        device_data[..BLOCK_SIZE as usize].copy_from_slice(&old_data);
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(2 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize]
            .copy_from_slice(&new_data);
        device_data[(3 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let (req1, rx1) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let (req2, rx2) = TestVecBuffer::new_with_range(0..BLOCK_SIZE);
        let shared_handler = Arc::new(TrackingDeliveryHandler {
            child_page_requests: Mutex::new(VecDeque::from([req1, req2])),
            ..Default::default()
        });
        let parent_files = Arc::new(Files::new_with_shared_handler(
            service.clone(),
            shared_handler.clone(),
            zx::Port::create(),
            None,
        ));
        let child_files = Arc::new(Files::new_with_shared_handler(
            service.clone(),
            shared_handler,
            zx::Port::create(),
            Some((parent_files.clone(), parent_key)),
        ));
        let mut parent_fifo = TestMappingFifo::new();

        // Map `parent_key` in `parent_files` and `child_key` (mapping logical 0..BLOCK_SIZE ->
        // parent offset 0) in `child_files`.
        let _ = parent_fifo.push_mapping(parent_key, 0, 1, BLOCK_SIZE).await;
        parent_fifo.process_next(&parent_files);
        service.wait_and_trigger_sync();
        assert!(parent_files.is_loaded(parent_key));

        let child_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).expect("child extents");
        child_files
            .insert(child_key, Arc::new(File::new(child_extents, BLOCK_SIZE, Transform::None)));

        // Start a child page request so an asynchronous block read is in flight on `old_parent`.
        child_files.handle_page_request(child_key, 0..BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 1);

        // Overwrite `parent_key` in `parent_files` and complete the overwrite's metadata read while
        // the child read on `old_parent` is still in flight.
        let overwrite_slot = parent_fifo.push_mapping(parent_key, 2, 3, BLOCK_SIZE).await;
        let mut wait_overwrite = pin!(parent_fifo.sender.wait_until_popped(overwrite_slot));
        parent_fifo.process_next(&parent_files);
        assert_eq!(service.pending.lock().len(), 2);
        {
            let metadata_cb = service.pending.lock().remove(1);
            metadata_cb();
        }
        assert!(
            futures::poll!(&mut wait_overwrite).is_pending(),
            "parent overwrite slot must wait for in-flight child read on old parent File"
        );

        // Complete the child read on `old_parent`; `overwrite_slot` now pops.
        service.wait_and_trigger_sync();
        assert_eq!(rx1.output(), old_data);
        wait_overwrite.await.expect("overwrite_slot should pop once child read completes");

        // Subsequent child page requests dynamically resolve the overwritten parent `File`.
        child_files.handle_page_request(child_key, 0..BLOCK_SIZE);
        service.wait_and_trigger_sync();
        assert_eq!(rx2.output(), new_data);
    }

    #[fuchsia::test]
    async fn test_multi_extent_partial_error_holds_slot_until_all_in_flight_reads_finish() {
        let blob_key = 95;
        let metadata =
            BlobMetadata { merkle_leaves: MerkleLeaves::new(), format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);

        // Layout on mock device (6 blocks):
        // Block 0: data extent 0, Block 2: data extent 1 (non-contiguous so `SplittableBuffer`
        // splits a 2-block read into two `read_blocks` calls), Block 1: valid 1-block metadata,
        // Block 3 + Block 5: 2-extent metadata for Case 3.
        let mut device_data = vec![0xDDu8; (6 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        device_data[(3 * BLOCK_SIZE) as usize..(3 * BLOCK_SIZE) as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let (req1, _rx1) = TestVecBuffer::new_with_range(0..2 * BLOCK_SIZE);
        let handler = TrackingDeliveryHandler {
            page_requests: Mutex::new(VecDeque::from([req1])),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let mut fifo = TestMappingFifo::new();

        let two_block_data_extents = Extents::try_new(
            [
                Extent::new(0..BLOCK_SIZE, Some(0)),
                Extent::new(BLOCK_SIZE..2 * BLOCK_SIZE, Some(2 * BLOCK_SIZE)),
            ],
            0,
        )
        .expect("data extents failed");
        let one_block_meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0)
                .expect("meta extents failed");

        // 1. Multi-extent data read where extent 0 fails while extent 1 is still in flight:
        // `SplittableBuffer` / `ReadContext` drop the outer completion callback immediately on
        // extent 0's error, yet `CloseBlob`'s slot must stay unpopped until extent 1 finishes.
        let _ = fifo
            .push_extents_mapping(
                blob_key,
                &two_block_data_extents,
                &one_block_meta_extents,
                2 * BLOCK_SIZE,
            )
            .await;
        fifo.process_next(&files);
        service.wait_and_trigger_sync();
        assert!(files.is_loaded(blob_key));

        service.fail_at_offset(0);
        files.handle_page_request(blob_key, 0..2 * BLOCK_SIZE);
        assert_eq!(service.pending.lock().len(), 2);

        let close_slot1 = fifo.push_close(blob_key).await;
        let mut wait_close1 = pin!(fifo.sender.wait_until_popped(close_slot1));
        fifo.process_next(&files);

        // Complete extent 0 (which fails and drops the outer read callback), leaving extent 1 in
        // flight.
        {
            let fail_cb = service.pending.lock().remove(0);
            fail_cb();
        }
        assert!(
            futures::poll!(&mut wait_close1).is_pending(),
            "CloseBlob slot must wait for remaining in-flight extent after sibling extent error"
        );

        service.wait_and_trigger_sync();
        wait_close1.await.expect("close_slot1 failed to pop after remaining extent finished");

        // 2. Multi-extent `ChildBlockService::read_blocks` where extent 0 fails while extent 1 is
        // still in flight on the parent file.
        let _ = fifo
            .push_extents_mapping(
                blob_key,
                &two_block_data_extents,
                &one_block_meta_extents,
                2 * BLOCK_SIZE,
            )
            .await;
        fifo.process_next(&files);
        service.wait_and_trigger_sync();

        let child_service = crate::ChildBlockService::new(service.clone(), files.clone(), blob_key);
        service.fail_at_offset(0);
        let child_buf = child_service.allocate_buffer((2 * BLOCK_SIZE) as usize);
        child_service
            .read_blocks(0, child_buf, Box::new(|res| assert!(res.is_err())))
            .expect("child read_blocks failed");
        assert_eq!(service.pending.lock().len(), 2);

        let close_slot2 = fifo.push_close(blob_key).await;
        let mut wait_close2 = pin!(fifo.sender.wait_until_popped(close_slot2));
        fifo.process_next(&files);

        {
            let fail_cb = service.pending.lock().remove(0);
            fail_cb();
        }
        assert!(
            futures::poll!(&mut wait_close2).is_pending(),
            "parent CloseBlob slot must wait for remaining ChildBlockService extent"
        );

        service.wait_and_trigger_sync();
        wait_close2.await.expect("close_slot2 failed to pop after remaining child extent finished");

        // 3. Multi-extent metadata read where extent 0 fails while extent 1 is still in flight.
        let two_block_meta_extents = Extents::try_new(
            [
                Extent::new(0..BLOCK_SIZE, Some(3 * BLOCK_SIZE)),
                Extent::new(BLOCK_SIZE..2 * BLOCK_SIZE, Some(5 * BLOCK_SIZE)),
            ],
            0,
        )
        .expect("two-block meta extents failed");
        service.fail_at_offset(3 * BLOCK_SIZE);
        let meta_slot = fifo
            .push_extents_mapping(
                blob_key,
                &two_block_data_extents,
                &two_block_meta_extents,
                2 * BLOCK_SIZE,
            )
            .await;
        let mut wait_meta = pin!(fifo.sender.wait_until_popped(meta_slot));
        fifo.process_next(&files);
        assert_eq!(service.pending.lock().len(), 2);

        {
            let fail_cb = service.pending.lock().remove(0);
            fail_cb();
        }
        assert!(!files.is_loading(blob_key));
        assert!(
            futures::poll!(&mut wait_meta).is_pending(),
            "Mappings slot must wait for remaining metadata extent after sibling extent error"
        );

        service.wait_and_trigger_sync();
        wait_meta.await.expect("meta_slot failed to pop after remaining metadata extent finished");
    }

    #[fuchsia::test]
    async fn test_failed_registration_commit_removes_loading_entry_and_retires_slot() {
        let blob_key = 96;
        let metadata =
            BlobMetadata { merkle_leaves: vec![[5u8; 32]], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let handler = TrackingDeliveryHandler {
            fail_next_registration_commit: AtomicBool::new(true),
            ..Default::default()
        };
        let files = Arc::new(Files::new(service.clone(), handler, zx::Port::create()));
        let mut fifo = TestMappingFifo::new();

        let slot = fifo.push_mapping(blob_key, 0, 1, BLOCK_SIZE).await;
        let mut wait_slot = pin!(fifo.sender.wait_until_popped(slot));
        fifo.process_next(&files);
        assert!(files.is_loading(blob_key));
        assert!(futures::poll!(&mut wait_slot).is_pending());

        service.wait_and_trigger_sync();
        wait_slot.await.expect("slot failed to pop after registration commit failure");
        assert!(!files.is_loading(blob_key));
        assert!(!files.is_loaded(blob_key));
        assert_eq!(files.delivery_handler.prepared_registrations.load(Ordering::SeqCst), 1);
        assert!(files.delivery_handler.registered_alignments.lock().is_empty());
        assert_eq!(files.delivery_handler.unregister_count.load(Ordering::SeqCst), 1);
    }

    #[fuchsia::test]
    fn test_read_range_flush_alignment_only_for_lz4() {
        struct FlushAlignmentRecorder {
            range: Range<u64>,
            buf: Vec<u8>,
            flush_alignment: Arc<Mutex<Option<Option<NonZero<usize>>>>>,
        }

        impl DataBuffer for FlushAlignmentRecorder {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                MutPtrByteSlice::from(&mut self.buf[..])
            }
            fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }

        impl PageRequest for FlushAlignmentRecorder {
            fn prepare(
                &mut self,
                read_range: Range<u64>,
                flush_alignment: Option<NonZero<usize>>,
            ) -> Result<(), ChunkedArchiveError> {
                self.buf = vec![0u8; (read_range.end - read_range.start) as usize];
                self.range = read_range;
                *self.flush_alignment.lock() = Some(flush_alignment);
                Ok(())
            }
            fn split_at(&mut self, _mid: u64) -> Self {
                unreachable!()
            }
        }

        let uncompressed = vec![0x42u8; 65536];
        let check_transform = |transform: Transform, data: &[u8]| {
            let aligned_len = data.len().next_multiple_of(BLOCK_SIZE as usize);
            let mut disk = vec![0u8; aligned_len];
            disk[..data.len()].copy_from_slice(data);
            let service = FakeBlockService::new(disk);
            let extents = Extents::try_new([Extent::new(0..(aligned_len as u64), Some(0))], 0)
                .expect("extents");
            let file = Arc::new(File::new(extents, uncompressed.len() as u64, transform));
            let recorded = Arc::new(Mutex::new(None));
            file.read_range(
                &service,
                FlushAlignmentRecorder {
                    range: 0..4096,
                    buf: Vec::new(),
                    flush_alignment: recorded.clone(),
                },
            );
            recorded.lock().expect("prepare was not called")
        };

        assert_eq!(check_transform(Transform::None, &uncompressed), None);

        // Raw LZ4 chunks (`ReadMode::Raw`) perform no decompression and should also wait for the
        // full window.
        let raw_lz4 =
            CompressionInfo::new_lz4_with_raw_chunks(32768, 65536, 65536, &[0, 32768]).unwrap();
        assert_eq!(check_transform(Transform::Compressed(raw_lz4), &uncompressed), None);

        for (alg, expected) in
            [(CompressionAlgorithm::Zstd, None), (CompressionAlgorithm::Lz4, NonZero::new(32768))]
        {
            let archive = ChunkedArchive::new(
                &uncompressed,
                ChunkedArchiveOptions::V3 { compression_algorithm: alg },
            )
            .expect("archive");
            let chunk_size = archive.chunk_size() as u64;
            let mut compressed = Vec::new();
            let mut offsets = Vec::new();
            for chunk in archive.chunks() {
                offsets.push(compressed.len() as u64);
                compressed.extend_from_slice(&chunk.compressed_data);
            }
            let info =
                CompressionInfo::new(chunk_size, compressed.len() as u64, &offsets, alg).unwrap();
            assert_eq!(check_transform(Transform::Compressed(info), &compressed), expected);
        }
    }
}
