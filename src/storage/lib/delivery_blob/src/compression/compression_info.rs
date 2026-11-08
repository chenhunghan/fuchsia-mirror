// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::compression::{
    ChunkedArchiveError, CompressionAlgorithm, RAW_CHUNK_ALIGNMENT, ThreadLocalDecompressor,
};
use std::borrow::Borrow;
use std::ops::Range;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};

const ALIGNMENT: u64 = RAW_CHUNK_ALIGNMENT as u64;

/// Trait for destination buffers where uncompressed or decompressed blob data is written.
pub trait DataBuffer: Send + 'static {
    /// Returns the logical byte range for this buffer.
    fn range(&self) -> Range<u64>;

    /// Returns a raw pointer slice to the remaining uncommitted memory in this allocation.
    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_>;

    /// Incrementally commits `size` bytes of data within this allocation, advancing the start
    /// of the remaining memory returned by subsequent calls to [`mut_ptr_slice`].
    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError>;
}

pub struct CompressionInfo {
    chunk_size: u64,
    compressed_size: u64,
    // The chunked compression format stores 0 as the first offset but it's not stored here. Not
    // storing the 0 avoids the allocation for blobs smaller than the chunk size.
    small_offsets: Box<[u32]>,
    large_offsets: Box<[u64]>,
    decompressor: ThreadLocalDecompressor,
    // True if any chunk in the blob is stored raw. See `new_lz4_with_raw_chunks`.
    has_raw_chunks: bool,
    // Whether the last chunk is raw. This can't be derived from the offsets like it can for other
    // chunks because the last chunk's uncompressed size isn't stored.
    last_chunk_is_raw: bool,
}

impl CompressionInfo {
    pub fn new(
        chunk_size: u64,
        compressed_size: u64,
        offsets: &[u64],
        compression_algorithm: CompressionAlgorithm,
    ) -> Result<Self, ChunkedArchiveError> {
        let decompressor = compression_algorithm.thread_local_decompressor();
        if chunk_size == 0 {
            return Err(ChunkedArchiveError::IntegrityError);
        } else if offsets.is_empty() || *offsets.first().unwrap() != 0 {
            // There should always be at least 1 offset and the first offset must always be 0.
            return Err(ChunkedArchiveError::IntegrityError);
        } else if !offsets.array_windows().all(|[a, b]| a < b)
            || *offsets.last().unwrap() > compressed_size
        {
            // The offsets must be in ascending order and within the compressed size.
            return Err(ChunkedArchiveError::IntegrityError);
        }
        let (small_offsets, large_offsets) = if offsets.len() == 1 {
            // Simple case where the blob is smaller than the chunk size so only the 0 offset is
            // present. The 0 isn't stored so no allocation is necessary.
            (Box::default(), Box::default())
        } else if *offsets.last().unwrap() <= u32::MAX as u64 {
            // Check the last index first since most compressed blobs are going to be smaller
            // than 4GiB making all offsets small.
            (offsets[1..].iter().map(|x| *x as u32).collect(), Box::default())
        } else {
            // The partition point is the index of the first compressed offset that's > u32::MAX.
            let partition_point = offsets.partition_point(|&x| x <= u32::MAX as u64);
            (
                offsets[1..partition_point].iter().map(|x| *x as u32).collect(),
                offsets[partition_point..].into(),
            )
        };
        Ok(Self {
            chunk_size,
            compressed_size,
            small_offsets,
            large_offsets,
            decompressor,
            has_raw_chunks: false,
            last_chunk_is_raw: false,
        })
    }

    /// Creates a `CompressionInfo` for an LZ4 blob in which chunks that LZ4 can't make smaller are
    /// stored raw. Raw chunks start at the next multiple of [`RAW_CHUNK_ALIGNMENT`] at or after
    /// the end of the previous chunk, and `offsets[i]` is where chunk `i - 1` ends (0 for chunk 0,
    /// before any padding), so a chunk is raw if it spans at least its uncompressed size.
    pub fn new_lz4_with_raw_chunks(
        chunk_size: u64,
        uncompressed_size: u64,
        compressed_size: u64,
        offsets: &[u64],
    ) -> Result<Self, ChunkedArchiveError> {
        let mut info = Self::new(chunk_size, compressed_size, offsets, CompressionAlgorithm::Lz4)?;
        if !chunk_size.is_multiple_of(ALIGNMENT)
            || *offsets.last().unwrap() >= compressed_size
            || offsets.len() as u64 != uncompressed_size.div_ceil(chunk_size)
        {
            return Err(ChunkedArchiveError::IntegrityError);
        }
        // Returns whether the chunk spanning `unaligned_start..end` with uncompressed size `size`
        // is raw, and checks that raw chunks are aligned.
        let is_raw = |unaligned_start: u64, end: u64, size: u64| {
            if end - unaligned_start < size {
                Ok(false)
            } else if unaligned_start.checked_next_multiple_of(ALIGNMENT) == Some(end - size) {
                Ok(true)
            } else {
                Err(ChunkedArchiveError::IntegrityError)
            }
        };
        let last_chunk_size = uncompressed_size - (offsets.len() as u64 - 1) * chunk_size;
        info.last_chunk_is_raw =
            is_raw(*offsets.last().unwrap(), compressed_size, last_chunk_size)?;
        info.has_raw_chunks = info.last_chunk_is_raw;
        for &[unaligned_start, end] in offsets.array_windows() {
            info.has_raw_chunks |= is_raw(unaligned_start, end, chunk_size)?;
        }
        Ok(info)
    }

    /// Returns the chunk size for this compressed blob.
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Returns the total compressed size of this blob.
    pub fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Returns the total number of chunks in this compressed blob.
    pub fn num_chunks(&self) -> usize {
        1 + self.small_offsets.len() + self.large_offsets.len()
    }

    /// Returns the range of the stored bytes for the chunk at `chunk_index` (excluding any padding
    /// before it) and whether the chunk is stored raw.
    fn chunk_compressed_range(&self, chunk_index: usize) -> Option<(Range<u64>, /*is_raw*/ bool)> {
        let unaligned_start = self.compressed_offset_for_chunk_index(chunk_index)?;
        let next = self.compressed_offset_for_chunk_index(chunk_index + 1);
        let end = next.unwrap_or(self.compressed_size);
        let is_raw = self.has_raw_chunks
            && match next {
                Some(end) => end - unaligned_start >= self.chunk_size,
                None => self.last_chunk_is_raw,
            };
        let start =
            if is_raw { unaligned_start.next_multiple_of(ALIGNMENT) } else { unaligned_start };
        Some((start..end, is_raw))
    }

    /// Returns the compressed range for the specified uncompressed range.
    pub fn compressed_range_for_uncompressed_range(
        &self,
        range: &Range<u64>,
    ) -> Result<Range<u64>, ChunkedArchiveError> {
        if range.start % self.chunk_size != 0 || range.start >= range.end {
            return Err(ChunkedArchiveError::IntegrityError);
        }

        let start_chunk_index = (range.start / self.chunk_size) as usize;
        let start_offset = self
            .chunk_compressed_range(start_chunk_index)
            .ok_or(ChunkedArchiveError::OutOfRange)?
            .0
            .start;

        // The end of the range may not be aligned to the chunk size for the last chunk.
        let num_chunks = self.num_chunks();
        let end_chunk_index = range.end.div_ceil(self.chunk_size) as usize;
        let end_offset = if end_chunk_index > num_chunks {
            return Err(ChunkedArchiveError::OutOfRange);
        } else if end_chunk_index == num_chunks {
            self.compressed_size
        } else {
            // This isn't the last chunk so the end must be aligned.
            if !range.end.is_multiple_of(self.chunk_size) {
                return Err(ChunkedArchiveError::IntegrityError);
            }
            // `end_chunk_index` is strictly less than `num_chunks`, so lookup is guaranteed `Some`.
            self.compressed_offset_for_chunk_index(end_chunk_index).unwrap()
        };

        Ok(start_offset..end_offset)
    }

    /// Returns the offset of the stored bytes for `range` if every chunk in `range` is stored raw
    /// and the offset is a multiple of `block_size`. The stored bytes are then identical to the
    /// uncompressed bytes and contiguous, so they can be read directly into the destination buffer.
    pub fn raw_offset_for_uncompressed_range(
        &self,
        range: &Range<u64>,
        block_size: u64,
    ) -> Option<u64> {
        if !self.has_raw_chunks
            || !self.chunk_size.is_multiple_of(block_size)
            || !range.start.is_multiple_of(self.chunk_size)
            || range.start >= range.end
        {
            return None;
        }
        let start_chunk = (range.start / self.chunk_size) as usize;
        let end_chunk = range.end.div_ceil(self.chunk_size) as usize;
        let (first, true) = self.chunk_compressed_range(start_chunk)? else {
            return None;
        };
        if !first.start.is_multiple_of(block_size) {
            return None;
        }
        let mut end = first.end;
        for i in (start_chunk + 1)..end_chunk {
            let (chunk, true) = self.chunk_compressed_range(i)? else {
                return None;
            };
            end = chunk.end;
        }
        // `range` must cover at least the stored raw bytes. Callers issuing block-aligned disk
        // reads must round the on-disk read length up to `block_size`.
        (range.end - range.start >= end - first.start).then_some(first.start)
    }

    fn compressed_offset_for_chunk_index(&self, chunk_index: usize) -> Option<u64> {
        if chunk_index == 0 {
            Some(0)
        } else if chunk_index - 1 < self.small_offsets.len() {
            Some(self.small_offsets[chunk_index - 1] as u64)
        } else if chunk_index - 1 - self.small_offsets.len() < self.large_offsets.len() {
            Some(self.large_offsets[chunk_index - 1 - self.small_offsets.len()])
        } else {
            None
        }
    }

    /// Decompress the bytes of `src` into `dst`.
    ///   - `src` is allowed to span multiple chunks.
    ///   - `dst` must have the exact size of the uncompressed bytes.
    ///   - `dst_start_offset` is the location of the uncompressed bytes within the blob and must be
    ///     chunk aligned. This is necessary for determining the chunk boundaries in `src`.
    pub fn decompress<'a>(
        &self,
        src: impl Into<PtrByteSlice<'a>>,
        mut dst: &mut [u8],
        dst_start_offset: u64,
    ) -> Result<(), ChunkedArchiveError> {
        let mut src = src.into();
        if dst_start_offset % self.chunk_size != 0 {
            return Err(ChunkedArchiveError::IntegrityError);
        }

        let start_chunk_index = (dst_start_offset / self.chunk_size) as usize;
        let chunk_count = dst.len().div_ceil(self.chunk_size as usize);
        let mut start_offset = self
            .chunk_compressed_range(start_chunk_index)
            .ok_or(ChunkedArchiveError::IntegrityError)?
            .0
            .start;

        // Decompress each chunk individually.
        for chunk_index in start_chunk_index..(start_chunk_index + chunk_count) {
            let (chunk_range, is_raw) = self
                .chunk_compressed_range(chunk_index)
                .ok_or(ChunkedArchiveError::IntegrityError)?;
            let len = (chunk_range.end - start_offset) as usize;
            if len > src.len() {
                return Err(ChunkedArchiveError::IntegrityError);
            }
            let (to_decompress, src_remaining) = src.split_at(len);
            let to_decompress =
                to_decompress.subslice((chunk_range.start - start_offset) as usize..len);
            let (to_decompress_into, dst_remaining) =
                dst.split_at_mut(std::cmp::min(dst.len(), self.chunk_size as usize));
            self.decompress_chunk(chunk_index, is_raw, to_decompress, to_decompress_into)?;
            src = src_remaining;
            dst = dst_remaining;
            start_offset = chunk_range.end;
        }

        Ok(())
    }

    /// Decompresses the chunk at `chunk_index` from `src` into `dst`, copying it if `is_raw` is
    /// true.
    fn decompress_chunk(
        &self,
        chunk_index: usize,
        is_raw: bool,
        src: PtrByteSlice<'_>,
        dst: &mut [u8],
    ) -> Result<(), ChunkedArchiveError> {
        if is_raw {
            if src.len() != dst.len() {
                return Err(ChunkedArchiveError::IntegrityError);
            }
            src.copy_to_slice(dst);
        } else if self.decompressor.decompress_into(src, dst, chunk_index)? != dst.len() {
            return Err(ChunkedArchiveError::IntegrityError);
        }
        Ok(())
    }
}

/// Stateful streaming decompressor that receives compressed block buffers
/// and decompresses complete chunks into `page_request`.
pub struct StreamingDecompressor<C, B> {
    /// Reference or owned container for blob compression metadata.
    info: C,

    /// Target destination implementing `DataBuffer`.
    data_buffer: B,

    /// Uncompressed logical byte range remaining to be decompressed.
    range: Range<u64>,

    /// Total uncompressed size of the blob.
    uncompressed_size: u64,

    /// Index of the chunk currently being decompressed.
    chunk_index: usize,

    /// Accumulates compressed bytes for chunks that straddle buffer boundaries.
    accumulator: Vec<u8>,

    /// The current compressed device byte offset expected for incoming buffers.
    current_compressed_offset: u64,

    /// Indicates if an error occurred during decompression. Once `true`, `push` fuses and returns
    /// error.
    failed: bool,
}

impl<C: Borrow<CompressionInfo>, B: DataBuffer> StreamingDecompressor<C, B> {
    /// Creates a new streaming decompressor for `data_buffer`.
    ///
    /// Accepts any container `info` implementing `Borrow<CompressionInfo>` (e.g.
    /// `&CompressionInfo` or `Arc<CompressionInfo>`).
    /// Returns the decompressor and the block-aligned compressed byte range (`Range<u64>`) to
    /// read from storage.
    ///
    /// `data_buffer.range().start` must be chunk aligned (a multiple of `chunk_size`).
    pub fn new(
        info: C,
        uncompressed_size: u64,
        data_buffer: B,
    ) -> Result<(Self, Range<u64>), ChunkedArchiveError> {
        const BLOCK_SIZE: u64 = 4096;
        let info_ref = info.borrow();
        let chunk_size = info_ref.chunk_size();
        if uncompressed_size == 0
            || uncompressed_size.div_ceil(chunk_size) != info_ref.num_chunks() as u64
        {
            return Err(ChunkedArchiveError::IntegrityError);
        }
        let range = data_buffer.range();
        if range.start >= range.end {
            return Err(ChunkedArchiveError::IntegrityError);
        }
        if !range.end.is_multiple_of(chunk_size) && range.end < uncompressed_size {
            return Err(ChunkedArchiveError::IntegrityError);
        }
        let clamped_range = range.start..std::cmp::min(range.end, uncompressed_size);
        let compressed = if clamped_range.start < clamped_range.end {
            info_ref.compressed_range_for_uncompressed_range(&clamped_range)?
        } else if !range.start.is_multiple_of(chunk_size) {
            return Err(ChunkedArchiveError::IntegrityError);
        } else {
            return Err(ChunkedArchiveError::OutOfRange);
        };
        let aligned = (compressed.start / BLOCK_SIZE) * BLOCK_SIZE
            ..compressed.end.next_multiple_of(BLOCK_SIZE);

        let chunk_index = (range.start / chunk_size) as usize;

        let decompressor = StreamingDecompressor {
            info,
            data_buffer,
            range,
            uncompressed_size,
            chunk_index,
            accumulator: Vec::new(),
            current_compressed_offset: aligned.start,
            failed: false,
        };

        Ok((decompressor, aligned))
    }

    /// Pushes a newly read compressed block buffer slice and decompresses any complete chunks.
    /// Fuses on error: if an error occurs or has previously occurred, returns `Err`.
    pub fn push<'b>(
        &mut self,
        buffer_slice: impl Into<PtrByteSlice<'b>>,
    ) -> Result<(), ChunkedArchiveError> {
        let buffer_slice = buffer_slice.into();
        if self.failed {
            return Err(ChunkedArchiveError::IntegrityError);
        }

        if self.range.is_empty() {
            return Ok(());
        }

        let buffer = self.current_compressed_offset
            ..self.current_compressed_offset + buffer_slice.len() as u64;
        self.current_compressed_offset = buffer.end;

        let info = self.info.borrow();
        let chunk_size = info.chunk_size();

        while self.range.start < self.range.end {
            let (chunk, is_raw) = info
                .chunk_compressed_range(self.chunk_index)
                .ok_or(ChunkedArchiveError::OutOfRange)?;
            if chunk.start >= buffer.end {
                // The remaining bytes in this buffer are before the next chunk (e.g. padding).
                break;
            }

            let decompress_chunk = |compressed_src: PtrByteSlice<'_>,
                                    data_buffer: &mut B|
             -> Result<u64, ChunkedArchiveError> {
                let remaining = self.uncompressed_size.saturating_sub(self.range.start);
                let avail_len = data_buffer.mut_ptr_slice().len();
                let min_required_len = if remaining <= chunk_size {
                    std::cmp::min(self.range.end - self.range.start, chunk_size) as usize
                } else {
                    chunk_size as usize
                };
                if avail_len < min_required_len {
                    return Err(ChunkedArchiveError::IntegrityError);
                }
                let buffer_len = if remaining <= chunk_size {
                    std::cmp::min(
                        avail_len,
                        std::cmp::max(chunk_size, self.range.end - self.range.start) as usize,
                    )
                } else {
                    chunk_size as usize
                };
                let mut dest_buffer = data_buffer.mut_ptr_slice().subslice_mut(0..buffer_len);
                let remaining = remaining as usize;
                if remaining < buffer_len {
                    // Zero the tail if this partial final chunk is smaller than `buffer_len` (e.g.
                    // due to block, chunk, or page alignment padding past `uncompressed_size`).
                    let (head, mut tail) = dest_buffer.split_at_mut(remaining);
                    tail.fill(0);
                    dest_buffer = head;
                }

                // SAFETY: `data_buffer` is exclusively held by this decompressor, and
                // `dest_buffer` points to uncommitted remaining memory.
                let dst_slice = unsafe { &mut *dest_buffer.as_raw_mut_slice_ptr() };

                info.decompress_chunk(self.chunk_index, is_raw, compressed_src, dst_slice)?;
                data_buffer.commit(buffer_len)?;
                Ok(std::cmp::min(self.range.end - self.range.start, buffer_len as u64))
            };

            if chunk.start < buffer.start {
                // Chunk started in a previous buffer; accumulate remainder and decompress if
                // complete.
                assert!(!self.accumulator.is_empty());
                if chunk.end <= buffer.end {
                    let needed = (chunk.end - buffer.start) as usize;
                    buffer_slice.subslice(0..needed).append_to(&mut self.accumulator);
                    let src = self.accumulator.as_slice().into();
                    match decompress_chunk(src, &mut self.data_buffer) {
                        Ok(written) => {
                            self.accumulator.clear();
                            self.range.start += written;
                            self.chunk_index += 1;
                            continue;
                        }
                        Err(e) => {
                            self.failed = true;
                            return Err(e);
                        }
                    }
                } else {
                    buffer_slice.append_to(&mut self.accumulator);
                    break;
                }
            } else if chunk.end <= buffer.end {
                // Chunk is fully contained in current buffer.
                let rel_start = (chunk.start - buffer.start) as usize;
                let rel_end = (chunk.end - buffer.start) as usize;
                let compressed_slice = buffer_slice.subslice(rel_start..rel_end);

                match decompress_chunk(compressed_slice, &mut self.data_buffer) {
                    Ok(written) => {
                        self.range.start += written;
                        self.chunk_index += 1;
                        continue;
                    }
                    Err(e) => {
                        self.failed = true;
                        return Err(e);
                    }
                }
            } else {
                // Chunk extends past current buffer; accumulate prefix and await next buffer.
                let rel_start = (chunk.start - buffer.start) as usize;
                buffer_slice
                    .subslice(rel_start..buffer_slice.len())
                    .append_to(&mut self.accumulator);
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_info_new_small_and_large_offsets() {
        let info = CompressionInfo::new(4096, 50, &[0], CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(info.chunk_size(), 4096);
        assert_eq!(info.compressed_size(), 50);
        assert_eq!(info.compressed_offset_for_chunk_index(0), Some(0));
        assert_eq!(info.compressed_offset_for_chunk_index(1), None);

        let info =
            CompressionInfo::new(4096, 350, &[0, 100, 250], CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(info.compressed_offset_for_chunk_index(0), Some(0));
        assert_eq!(info.compressed_offset_for_chunk_index(1), Some(100));
        assert_eq!(info.compressed_offset_for_chunk_index(2), Some(250));
        assert_eq!(info.compressed_offset_for_chunk_index(3), None);

        let large_val = u32::MAX as u64 + 1000;
        let info = CompressionInfo::new(
            4096,
            large_val + 500,
            &[0, 500, large_val],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert_eq!(info.compressed_offset_for_chunk_index(0), Some(0));
        assert_eq!(info.compressed_offset_for_chunk_index(1), Some(500));
        assert_eq!(info.compressed_offset_for_chunk_index(2), Some(large_val));
        assert_eq!(info.compressed_offset_for_chunk_index(3), None);
    }

    #[test]
    fn test_compressed_range_for_uncompressed_range() {
        let info = CompressionInfo::new(4096, 500, &[0, 100, 250, 400], CompressionAlgorithm::Zstd)
            .unwrap();
        let range = info.compressed_range_for_uncompressed_range(&(0..4096)).unwrap();
        assert_eq!(range, 0..100);

        let range = info.compressed_range_for_uncompressed_range(&(4096..12288)).unwrap();
        assert_eq!(range, 100..400);

        let range = info.compressed_range_for_uncompressed_range(&(4096..16384)).unwrap();
        assert_eq!(range, 100..500);

        assert!(matches!(
            info.compressed_range_for_uncompressed_range(&(4096..20480)),
            Err(ChunkedArchiveError::OutOfRange)
        ));
        assert!(matches!(
            info.compressed_range_for_uncompressed_range(&(4096..16385)),
            Err(ChunkedArchiveError::OutOfRange)
        ));
        assert!(matches!(
            info.compressed_range_for_uncompressed_range(&(16384..20480)),
            Err(ChunkedArchiveError::OutOfRange)
        ));
        assert!(matches!(
            info.compressed_range_for_uncompressed_range(&(4096..10000)),
            Err(ChunkedArchiveError::IntegrityError)
        ));
    }

    #[test]
    fn test_compression_info_offsets_must_start_with_zero() {
        assert!(CompressionInfo::new(4096, 100, &[], CompressionAlgorithm::Zstd).is_err());
        assert!(CompressionInfo::new(4096, 100, &[1], CompressionAlgorithm::Zstd).is_err());
        assert!(CompressionInfo::new(4096, 100, &[0], CompressionAlgorithm::Zstd).is_ok());
    }

    #[test]
    fn test_compression_info_offsets_must_be_sorted() {
        assert!(CompressionInfo::new(4096, 100, &[0, 1, 2], CompressionAlgorithm::Zstd).is_ok());
        assert!(CompressionInfo::new(4096, 100, &[0, 2, 1], CompressionAlgorithm::Zstd).is_err());
        assert!(CompressionInfo::new(4096, 100, &[0, 1, 1], CompressionAlgorithm::Zstd).is_err());
    }

    #[test]
    fn test_compression_info_splitting_offsets() {
        const MAX_SMALL_OFFSET: u64 = u32::MAX as u64;
        let compression_info =
            CompressionInfo::new(4096, 100, &[0], CompressionAlgorithm::Zstd).unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert!(compression_info.large_offsets.is_empty());

        let compression_info =
            CompressionInfo::new(4096, 20, &[0, 10], CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(&*compression_info.small_offsets, &[10]);
        assert!(compression_info.large_offsets.is_empty());

        let compression_info =
            CompressionInfo::new(4096, 40, &[0, 10, 20, 30], CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(&*compression_info.small_offsets, &[10, 20, 30]);
        assert!(compression_info.large_offsets.is_empty());

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET,
            &[0, MAX_SMALL_OFFSET - 1],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX - 1]);
        assert!(compression_info.large_offsets.is_empty());

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET + 1,
            &[0, MAX_SMALL_OFFSET],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX]);
        assert!(compression_info.large_offsets.is_empty());

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET + 2,
            &[0, MAX_SMALL_OFFSET + 1],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 1]);

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET + 2,
            &[0, MAX_SMALL_OFFSET - 1, MAX_SMALL_OFFSET, MAX_SMALL_OFFSET + 1],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX - 1, u32::MAX]);
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 1]);

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET + 20,
            &[0, MAX_SMALL_OFFSET + 10],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 10]);

        let compression_info = CompressionInfo::new(
            4096,
            MAX_SMALL_OFFSET + 30,
            &[0, MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20],
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(
            &*compression_info.large_offsets,
            &[MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20]
        );
    }

    struct TestBuffer {
        data: Vec<u8>,
        range: Range<u64>,
        committed: usize,
    }

    impl TestBuffer {
        fn new(size: usize) -> Self {
            Self { data: vec![0u8; size], range: 0..size as u64, committed: 0 }
        }

        fn new_with_range(range: Range<u64>) -> Self {
            let size = (range.end - range.start) as usize;
            Self { data: vec![0u8; size], range, committed: 0 }
        }
    }

    impl DataBuffer for TestBuffer {
        fn range(&self) -> Range<u64> {
            self.range.clone()
        }

        fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
            let slice = &mut self.data[self.committed..];
            MutPtrByteSlice::from(slice)
        }

        fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
            self.committed += size;
            Ok(())
        }
    }

    #[test]
    fn test_streaming_decompressor_single_buffer() {
        let uncompressed_data: Vec<u8> = (0..32768).map(|i| (i % 251) as u8).collect();
        let options = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Zstd,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let info = CompressionInfo::new(
            archive.chunk_size() as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();

        let buf = TestBuffer::new(32768);
        let (mut decompressor, aligned) = StreamingDecompressor::new(&info, 32768, buf).unwrap();
        assert_eq!(aligned, 0..4096);

        decompressor.push(&compressed_data).unwrap();
        assert_eq!(&decompressor.data_buffer.data[..32768], &uncompressed_data[..]);
    }

    #[test]
    fn test_streaming_decompressor_straddled_buffers() {
        let uncompressed_data: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
        let options = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Zstd,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let info = CompressionInfo::new(
            archive.chunk_size() as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();

        let buf = TestBuffer::new(65536);
        let (mut decompressor, _) = StreamingDecompressor::new(&info, 65536, buf).unwrap();

        for slice in compressed_data.chunks(10) {
            decompressor.push(slice).unwrap();
        }
        assert_eq!(&decompressor.data_buffer.data[..65536], &uncompressed_data[..]);
    }

    #[test]
    fn test_streaming_decompressor_partial_last_chunk_zero_tail() {
        let uncompressed_size = 32768 + 1024;
        let uncompressed_data: Vec<u8> = (0..uncompressed_size).map(|i| (i % 251) as u8).collect();

        let options = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Zstd,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let info = CompressionInfo::new(
            archive.chunk_size() as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();

        let mut buf = TestBuffer::new_with_range(0..uncompressed_size as u64);
        buf.data.resize(65536, 0);
        buf.data.fill(0xFF);

        let (mut decompressor, _) =
            StreamingDecompressor::new(&info, uncompressed_size as u64, buf).unwrap();
        decompressor.push(&compressed_data).unwrap();

        assert_eq!(&decompressor.data_buffer.data[..uncompressed_size], &uncompressed_data[..]);
        assert_eq!(&decompressor.data_buffer.data[uncompressed_size..65536], &[0u8; 31744]);
    }

    #[test]
    fn test_streaming_decompressor_unaligned_start_returns_err() {
        let info = CompressionInfo::new(4096, 500, &[0], CompressionAlgorithm::Zstd).unwrap();
        let buf = TestBuffer::new_with_range(100..4096);
        assert!(StreamingDecompressor::new(&info, 4096, buf).is_err());
    }

    #[test]
    fn test_streaming_decompressor_fused_error() {
        let info = CompressionInfo::new(4096, 500, &[0], CompressionAlgorithm::Zstd).unwrap();
        let buf = TestBuffer::new(4096);
        let (mut decompressor, _) = StreamingDecompressor::new(&info, 4096, buf).unwrap();

        let invalid_compressed_data = vec![0xFFu8; 4096];
        assert!(decompressor.push(&invalid_compressed_data).is_err());
        assert!(decompressor.push(&invalid_compressed_data).is_err());
    }

    #[test]
    fn test_lz4_with_raw_chunks() {
        let chunk_size = crate::compression::ChunkedArchiveOptions::V3_CHUNK_SIZE;
        // Chunks 0 and 2 are compressible; chunks 1 and 3 (partial) are stored raw and aligned.
        let mut uncompressed_data = vec![0xAAu8; chunk_size * 3 + 5000];
        rand::fill(&mut uncompressed_data[chunk_size..chunk_size * 2]);
        rand::fill(&mut uncompressed_data[chunk_size * 3..]);
        let uncompressed_size = uncompressed_data.len();
        let options = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Lz4,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = Vec::new();
        let mut compressed_data = Vec::new();
        for (padding, chunk) in archive.chunks_with_padding() {
            compressed_offsets.push(compressed_data.len() as u64);
            compressed_data.extend_from_slice(padding);
            compressed_data.extend_from_slice(&chunk.compressed_data);
        }
        assert_eq!(compressed_offsets, archive.compressed_offsets());
        let info = CompressionInfo::new_lz4_with_raw_chunks(
            chunk_size as u64,
            uncompressed_size as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
        )
        .unwrap();

        // Raw chunks are aligned and can be decompressed on their own or as part of the full blob.
        let chunk_size_u64 = chunk_size as u64;
        let chunk1_range = info
            .compressed_range_for_uncompressed_range(&(chunk_size_u64..chunk_size_u64 * 2))
            .unwrap();
        assert_eq!(chunk1_range.start % ALIGNMENT, 0);
        assert_eq!(chunk1_range.end - chunk1_range.start, chunk_size_u64);
        let mut chunk1_dst = vec![0u8; chunk_size];
        info.decompress(
            &compressed_data[chunk1_range.start as usize..chunk1_range.end as usize],
            &mut chunk1_dst,
            chunk_size_u64,
        )
        .unwrap();
        assert_eq!(&chunk1_dst[..], &uncompressed_data[chunk_size..chunk_size * 2]);

        let mut full_dst = vec![0u8; uncompressed_size];
        info.decompress(&compressed_data, &mut full_dst, 0).unwrap();
        assert_eq!(full_dst, uncompressed_data);

        for slice_size in [4096, 13] {
            let mut buf = TestBuffer::new_with_range(0..uncompressed_size as u64);
            buf.data.resize(chunk_size * 4, 0);
            let (mut decompressor, aligned) =
                StreamingDecompressor::new(&info, uncompressed_size as u64, buf).unwrap();
            let mut device_data = compressed_data.clone();
            device_data.resize(aligned.end as usize, 0);
            for slice in device_data.chunks(slice_size) {
                decompressor.push(slice).unwrap();
            }
            assert_eq!(&decompressor.data_buffer.data[..uncompressed_size], &uncompressed_data[..]);
        }

        // Only chunk-aligned ranges made up entirely of raw chunks can be read directly.
        let raw_offset =
            |range: Range<u64>| info.raw_offset_for_uncompressed_range(&range, ALIGNMENT);
        assert_eq!(raw_offset(0..chunk_size_u64), None);
        assert_eq!(raw_offset(chunk_size_u64..chunk_size_u64 * 2), Some(chunk1_range.start));
        assert_eq!(raw_offset(chunk_size_u64..chunk_size_u64 + ALIGNMENT), None);
        assert_eq!(raw_offset(0..chunk_size_u64 * 2), None);
        assert_eq!(
            raw_offset(chunk_size_u64 * 3..uncompressed_size as u64),
            Some(info.chunk_compressed_range(3).unwrap().0.start)
        );
        assert_eq!(
            raw_offset(chunk_size_u64 * 3..(uncompressed_size as u64).next_multiple_of(ALIGNMENT)),
            Some(info.chunk_compressed_range(3).unwrap().0.start)
        );
        assert_eq!(raw_offset(chunk_size_u64 * 3..chunk_size_u64 * 3 + ALIGNMENT), None);
        // If `block_size` is larger than the raw chunk's alignment or `chunk_size`, direct read is
        // not used.
        assert_eq!(
            info.raw_offset_for_uncompressed_range(
                &(chunk_size_u64..chunk_size_u64 * 2),
                ALIGNMENT * 2
            ),
            None
        );
        assert_eq!(
            info.raw_offset_for_uncompressed_range(
                &(chunk_size_u64..chunk_size_u64 * 2),
                chunk_size_u64 * 2
            ),
            None
        );
    }

    #[test]
    fn test_lz4_with_raw_chunks_validation() {
        let new = |offsets: &[u64], compressed_size| {
            CompressionInfo::new_lz4_with_raw_chunks(32768, 65536, compressed_size, offsets)
        };
        // Chunk 0 saves less than `ALIGNMENT` bytes, so the padding before raw chunk 1 makes chunks
        // 0..2 span 65536 bytes, but chunk 0 is still compressed so it can't be read directly.
        let info = new(&[0, 30000], 65536).unwrap();
        assert_eq!(info.raw_offset_for_uncompressed_range(&(0..65536), ALIGNMENT), None);
        assert_eq!(info.raw_offset_for_uncompressed_range(&(32768..65536), ALIGNMENT), Some(32768));

        // Consecutive raw chunks are contiguous and can be read in a single range.
        let info = CompressionInfo::new_lz4_with_raw_chunks(
            32768,
            98304,
            ALIGNMENT + 65536,
            &[0, 100, ALIGNMENT + 32768],
        )
        .unwrap();
        assert_eq!(
            info.raw_offset_for_uncompressed_range(&(32768..98304), ALIGNMENT),
            Some(ALIGNMENT)
        );

        // Raw chunks must be aligned, chunk_size must be a multiple of ALIGNMENT, and the number of
        // offsets must match the uncompressed size.
        assert!(new(&[0, 30000], 30000 + 32768).is_err());
        assert!(new(&[0, 32768 + 1], 65536).is_err());
        assert!(new(&[0, 32768], 32768).is_err());
        assert!(new(&[0], 30000).is_err());
        assert!(new(&[0, 100, 200], 300).is_err());
        assert!(CompressionInfo::new_lz4_with_raw_chunks(30000, 60000, 200, &[0, 100]).is_err());
        assert!(CompressionInfo::new_lz4_with_raw_chunks(32768, 0, 100, &[0]).is_err());
    }

    #[test]
    fn test_legacy_lz4_oversized_chunk() {
        let chunk_size = crate::compression::ChunkedArchiveOptions::V3_CHUNK_SIZE;
        let mut uncompressed_data = vec![0u8; chunk_size * 2];
        rand::fill(&mut uncompressed_data[..]);
        let compressor = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Lz4,
        }
        .thread_local_compressor();
        let chunk0 = compressor.compress(&uncompressed_data[..chunk_size], 0).unwrap();
        let chunk1 = compressor.compress(&uncompressed_data[chunk_size..], 1).unwrap();
        assert!(chunk0.len() > chunk_size);
        assert!(chunk1.len() > chunk_size);

        let mut compressed_data = Vec::new();
        compressed_data.extend_from_slice(&chunk0);
        compressed_data.extend_from_slice(&chunk1);
        let offsets = [0, chunk0.len() as u64];
        let info = CompressionInfo::new(
            chunk_size as u64,
            compressed_data.len() as u64,
            &offsets,
            CompressionAlgorithm::Lz4,
        )
        .unwrap();

        let mut dst = vec![0u8; uncompressed_data.len()];
        info.decompress(&compressed_data, &mut dst, 0).unwrap();
        assert_eq!(dst, uncompressed_data);

        let buf = TestBuffer::new(uncompressed_data.len());
        let (mut decompressor, aligned) =
            StreamingDecompressor::new(&info, uncompressed_data.len() as u64, buf).unwrap();
        let mut device_data = compressed_data;
        device_data.resize(aligned.end as usize, 0);
        decompressor.push(&device_data).unwrap();
        assert_eq!(decompressor.data_buffer.data, uncompressed_data);
    }

    #[test]
    fn test_streaming_decompressor_validation() {
        let info_two_chunks =
            CompressionInfo::new(32768, 500, &[0, 250], CompressionAlgorithm::Zstd).unwrap();

        // Zero uncompressed_size is rejected.
        assert!(matches!(
            StreamingDecompressor::new(&info_two_chunks, 0, TestBuffer::new(32768)),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // Mismatched uncompressed_size vs num_chunks is rejected (both too small and too large).
        assert!(matches!(
            StreamingDecompressor::new(&info_two_chunks, 32768, TestBuffer::new(32768)),
            Err(ChunkedArchiveError::IntegrityError)
        ));
        assert!(matches!(
            StreamingDecompressor::new(&info_two_chunks, 65537, TestBuffer::new(65536)),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // Stopping short inside a partial final chunk (uncompressed_size = 32768 + 2048,
        // range = 0..32768 + 1024) is rejected.
        assert!(matches!(
            StreamingDecompressor::new(
                &info_two_chunks,
                32768 + 2048,
                TestBuffer::new_with_range(0..32768 + 1024),
            ),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // Stopping short inside a full final chunk (uncompressed_size = 65536,
        // range = 0..32768 + 4096) is rejected.
        assert!(matches!(
            StreamingDecompressor::new(
                &info_two_chunks,
                65536,
                TestBuffer::new_with_range(0..32768 + 4096),
            ),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // Aligned chunk boundaries and page-aligned ranges >= uncompressed_size are accepted.
        assert!(
            StreamingDecompressor::new(&info_two_chunks, 32768 + 2048, TestBuffer::new(32768))
                .is_ok()
        );
        assert!(
            StreamingDecompressor::new(
                &info_two_chunks,
                32768 + 2048,
                TestBuffer::new_with_range(0..65536),
            )
            .is_ok()
        );
    }

    #[test]
    fn test_streaming_decompressor_short_data_buffer_returns_err() {
        let uncompressed_size = 32768 + 1024;
        let uncompressed_data: Vec<u8> = (0..uncompressed_size).map(|i| (i % 251) as u8).collect();
        let options = crate::compression::ChunkedArchiveOptions::V3 {
            compression_algorithm: CompressionAlgorithm::Zstd,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let info = CompressionInfo::new(
            archive.chunk_size() as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();

        // DataBuffer whose mut_ptr_slice() is shorter than the remaining partial final chunk
        // returns IntegrityError cleanly without panicking in split_at_mut.
        let mut short_partial_buf = TestBuffer::new_with_range(0..uncompressed_size as u64);
        short_partial_buf.data.truncate(32768 + 512);
        let (mut decompressor, aligned) =
            StreamingDecompressor::new(&info, uncompressed_size as u64, short_partial_buf).unwrap();
        let mut device_data = compressed_data.clone();
        device_data.resize(aligned.end as usize, 0);
        assert!(matches!(
            decompressor.push(&device_data),
            Err(ChunkedArchiveError::IntegrityError)
        ));
        assert!(matches!(
            decompressor.push(&device_data),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // DataBuffer whose mut_ptr_slice() is shorter than a full chunk also returns IntegrityError.
        let mut short_full_buf = TestBuffer::new_with_range(0..32768);
        short_full_buf.data.truncate(16384);
        let (mut decompressor, _) =
            StreamingDecompressor::new(&info, uncompressed_size as u64, short_full_buf).unwrap();
        assert!(matches!(
            decompressor.push(&device_data),
            Err(ChunkedArchiveError::IntegrityError)
        ));
    }

    #[test]
    fn test_streaming_decompressor_page_size_larger_than_chunk_size() {
        let uncompressed_data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let options = crate::compression::ChunkedArchiveOptions::V2 {
            minimum_chunk_size: 4096,
            chunk_alignment: 4096,
            compression_level: 1,
        };
        let archive = crate::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let info = CompressionInfo::new(
            archive.chunk_size() as u64,
            compressed_data.len() as u64,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();

        // Simulate a 16 KiB page-aligned read covering the entire 5000-byte blob (2 x 4 KiB chunks
        // followed by 8 KiB of page-alignment padding past uncompressed_size).
        let buf = TestBuffer::new_with_range(0..16384);
        let (mut decompressor, aligned) = StreamingDecompressor::new(&info, 5000, buf).unwrap();
        let mut device_data = compressed_data;
        device_data.resize(aligned.end as usize, 0);
        decompressor.push(&device_data).unwrap();

        assert_eq!(decompressor.data_buffer.committed, 16384);
        assert_eq!(&decompressor.data_buffer.data[..5000], &uncompressed_data[..]);
        assert!(decompressor.data_buffer.data[5000..16384].iter().all(|&b| b == 0));
    }
}
