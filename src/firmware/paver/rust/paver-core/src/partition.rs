// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Traits ([`BoardPartitions`] and [`PartitionClient`]) for board-specific partition mapping and
//! byte-granularity storage I/O.

use crate::status_from_error;
use async_trait::async_trait;
use block_client::BlockClient;
use fidl_next_fuchsia_io as fio;
use std::num::NonZero;
use std::ops::Range;
use storage_device::Device as _;
use storage_device::block_device::BlockDevice;

/// Board-specific mapper for opening partitions by their logical paver name
/// (e.g. `"zircon_a"`, `"vbmeta_b"`, `"abl_a"`, `"super"`).
#[async_trait(?Send)]
pub trait BoardPartitions {
    /// Opens a partition client for `partition_name` using `block_dir`.
    ///
    /// Returns:
    /// - `Err(zx::Status::NOT_SUPPORTED)` if the partition name is not supported by this board.
    /// - `Err(zx::Status::NOT_FOUND)` if the partition is supported by the board, but cannot be
    ///   found on storage.
    ///
    /// # Single-Slot Firmware Mapping
    ///
    /// Neither `PartitionIo` nor [`crate::data_sink::DataSink`] performs a silent fallback from
    /// `Configuration::B` to `Configuration::A`. For boards that don't have A/B firmware slots,
    /// board backends must map both the `"_a"` and `"_b"` partition names (e.g.
    /// `"bootloader_a" | "bootloader_b"`) to the single underlying target here so `PartitionIo` and
    /// `DataSink` behave identically.
    async fn open_partition(
        &self,
        partition_name: &str,
        block_dir: &fidl_next::ClientEnd<fio::Directory>,
    ) -> Result<Box<dyn PartitionClient>, zx::Status>;

    /// Flushes any board-level buffered state across partitions to persistent storage.
    ///
    /// Standard block-backed [`PartitionClient`] implementations flush their block FIFO session
    /// during [`PartitionClient::flush`]; boards with cross-partition buffered state (such as
    /// Astro buffering multiple sub-partitions in a shared NAND context) can override this method
    /// to flush when `fuchsia.paver.PartitionIo/Flush` is called.
    async fn flush(&self) -> Result<(), zx::Status> {
        Ok(())
    }
}

/// Object-safe partition client interface supporting byte-granularity offsets and payload sizes.
#[async_trait(?Send)]
pub trait PartitionClient {
    /// Returns the total usable size of the partition in bytes.
    fn partition_size(&self) -> Result<u64, zx::Status>;

    /// Reads `size` bytes starting at `dev_offset` on the partition into `vmo` starting at
    /// `vmo_offset`.
    ///
    /// Implementations must return `Err(zx::Status::OUT_OF_RANGE)` if `dev_offset + size` exceeds
    /// [`Self::partition_size`] or (when `size > 0`) `vmo_offset + size` exceeds `vmo.get_size()`
    /// (see [`validate_vmo_range`]).
    async fn read_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status>;

    /// Writes `size` bytes from `vmo` starting at `vmo_offset` to `dev_offset` on the partition,
    /// preserving existing bytes in any leading or trailing partial blocks via read-modify-write.
    ///
    /// Implementations must return `Err(zx::Status::OUT_OF_RANGE)` if `dev_offset + size` exceeds
    /// [`Self::partition_size`] or (when `size > 0`) `vmo_offset + size` exceeds `vmo.get_size()`
    /// (see [`validate_vmo_range`]).
    async fn write_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status>;

    /// Discards (trims) all full blocks starting at the next block boundary at or after `offset`
    /// up to the end of the partition.
    async fn trim_tail(&self, _offset: u64) -> Result<(), zx::Status> {
        Ok(())
    }

    /// Flushes any buffered writes on this partition to persistent storage.
    async fn flush(&self) -> Result<(), zx::Status> {
        Ok(())
    }

    /// Reads `buf.len()` bytes starting at an arbitrary byte `offset` on the partition.
    ///
    /// Note: The default implementation allocates a temporary VMO and copies data between that VMO
    /// and `buf`. It is intended for small metadata reads (such as A/B/R slot metadata or GPT
    /// headers); callers transferring large partition images should use [`Self::read_vmo`]
    /// instead.
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<(), zx::Status> {
        let len = u64::try_from(buf.len()).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        validate_range(self, offset, len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let vmo = zx::Vmo::create(len)?;
        self.read_vmo(&vmo, 0, offset, len).await?;
        vmo.read(buf, 0)
    }

    /// Writes `buf` starting at an arbitrary byte `offset` on the partition, preserving existing
    /// bytes in any leading or trailing partial blocks via read-modify-write.
    ///
    /// Note: The default implementation allocates a temporary VMO and copies `buf` into it before
    /// delegating to [`Self::write_vmo`]. It is intended for small metadata updates (such as A/B/R
    /// slot metadata or GPT headers); callers writing large partition images should use
    /// [`Self::write_vmo`] instead.
    async fn write_at(&self, buf: &[u8], offset: u64) -> Result<(), zx::Status> {
        let len = u64::try_from(buf.len()).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        validate_range(self, offset, len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let vmo = zx::Vmo::create(len)?;
        vmo.write(buf, 0)?;
        self.write_vmo(&vmo, 0, offset, len).await
    }
}

/// [`PartitionClient`] adapter around a [`BlockClient`] backed by [`BlockDevice`] with a
/// configurable per-transfer bounce buffer capacity and total sparse transfer VMO pool capacity.
pub struct BlockPartitionClient<B> {
    device: BlockDevice<B>,
    bounce_buffer_bytes: NonZero<usize>,
}

impl<B: BlockClient> BlockPartitionClient<B> {
    /// Wraps `client` in a [`BlockDevice`] with a per-transfer bounce buffer capacity
    /// `bounce_buffer_bytes` (rounded down to a multiple of the device block size, or at least one
    /// block if smaller than a single block) and an underlying sparse transfer VMO pool capacity
    /// `transfer_vmo_pool_bytes` (rounded up to a power-of-two multiple of the device block size,
    /// and at least `bounce_buffer_bytes`).
    ///
    /// Board backends can tune `bounce_buffer_bytes` and `transfer_vmo_pool_bytes` based on their
    /// storage throughput and memory constraints (including sizing `transfer_vmo_pool_bytes`
    /// larger than `bounce_buffer_bytes` when allocating multiple buffers from the pool).
    pub async fn new(
        client: B,
        bounce_buffer_bytes: NonZero<usize>,
        transfer_vmo_pool_bytes: NonZero<usize>,
    ) -> Result<Self, zx::Status> {
        if transfer_vmo_pool_bytes < bounce_buffer_bytes {
            return Err(zx::Status::INVALID_ARGS);
        }
        let block_size =
            usize::try_from(client.block_size()).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        if !block_size.is_power_of_two() {
            return Err(zx::Status::INVALID_ARGS);
        }
        let device_blocks = usize::try_from(client.block_count()).unwrap_or(usize::MAX).max(1);
        // `BlockFifoRequest.length` is a `u32` block count, so cap a single transfer to `u32::MAX`
        // blocks (and no more than the partition's block count).
        let max_blocks = (bounce_buffer_bytes.get() / block_size)
            .clamp(1, usize::try_from(u32::MAX).unwrap_or(usize::MAX))
            .min(device_blocks);
        let bounce_bytes = max_blocks.checked_mul(block_size).ok_or(zx::Status::OUT_OF_RANGE)?;
        // `BufferAllocator` rounds allocations up to a power-of-two block count, so the underlying
        // transfer VMO pool must be sized to at least `max_blocks` and rounded up to a power-of-two
        // multiple of `block_size`.
        let requested_pool_blocks = (transfer_vmo_pool_bytes.get() / block_size).max(max_blocks);
        let pool_blocks =
            requested_pool_blocks.checked_next_power_of_two().ok_or(zx::Status::OUT_OF_RANGE)?;
        let pool_bytes = pool_blocks.checked_mul(block_size).ok_or(zx::Status::OUT_OF_RANGE)?;
        let device = BlockDevice::with_transfer_vmo_size(client, false, pool_bytes)
            .await
            .map_err(|err| status_from_error(&err))?;
        Ok(Self {
            device,
            bounce_buffer_bytes: NonZero::new(bounce_bytes)
                .expect("bounce_bytes >= block_size > 0"),
        })
    }

    /// Returns a reference to the underlying block client.
    pub fn client(&self) -> &B {
        self.device.remote()
    }

    /// Reads `aligned_len` block-aligned bytes starting at `aligned_dev_offset` into `dst_vmo` at
    /// `vmo_offset` using a buffer allocated from [`BlockDevice`]'s shared transfer VMO.
    async fn read_aligned_blocks(
        &self,
        dst_vmo: &zx::Vmo,
        vmo_offset: u64,
        aligned_dev_offset: u64,
        aligned_len: u64,
    ) -> Result<(), zx::Status> {
        // Using a single bounce buffer causes throughput loss on high-speed storage (e.g. UFS,
        // NVMe). However, the absolute wall-clock difference has been insignificant so far for
        // typical sizes of Fuchsia images. If this turns out to be an issue, one validated
        // approach is to use multiple bounce buffers and parallelize I/O and data copy, which
        // reduces ~90% of copy time.
        let bounce_bytes = aligned_bounce_buffer_bytes(self.bounce_buffer_bytes.get(), aligned_len);
        let mut buf = self.device.allocate_buffer(bounce_bytes).await;

        let mut transferred = 0u64;
        while transferred < aligned_len {
            let cur_len =
                usize::try_from(aligned_len - transferred).unwrap_or(usize::MAX).min(bounce_bytes);

            self.device
                .read(aligned_dev_offset + transferred, buf.subslice_mut(0..cur_len))
                .await
                .map_err(|err| status_from_error(&err))?;

            buf.subslice(0..cur_len).copy_to_vmo(dst_vmo, vmo_offset + transferred)?;

            transferred += u64::try_from(cur_len).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        }
        Ok(())
    }

    /// Writes `aligned_len` block-aligned bytes from `src_vmo` starting at `vmo_offset` to
    /// `aligned_dev_offset` using a buffer allocated from [`BlockDevice`]'s shared transfer VMO.
    async fn write_aligned_blocks(
        &self,
        src_vmo: &zx::Vmo,
        vmo_offset: u64,
        aligned_dev_offset: u64,
        aligned_len: u64,
    ) -> Result<(), zx::Status> {
        let bounce_bytes = aligned_bounce_buffer_bytes(self.bounce_buffer_bytes.get(), aligned_len);
        let mut buf = self.device.allocate_buffer(bounce_bytes).await;

        let mut transferred = 0u64;
        while transferred < aligned_len {
            let cur_len =
                usize::try_from(aligned_len - transferred).unwrap_or(usize::MAX).min(bounce_bytes);

            buf.subslice_mut(0..cur_len).copy_from_vmo(src_vmo, vmo_offset + transferred)?;

            self.device
                .write(aligned_dev_offset + transferred, buf.subslice(0..cur_len))
                .await
                .map_err(|err| status_from_error(&err))?;

            transferred += u64::try_from(cur_len).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        }
        Ok(())
    }
}

/// Guard that cleans up [`BlockDevice`]'s sparse transfer buffer when dropped.
///
/// Cleaning the transfer buffer at the end of each [`BlockPartitionClient::read_vmo`] and
/// [`BlockPartitionClient::write_vmo`] call ensures that read-only callers (which do not call
/// [`PartitionClient::flush`]) also decommit transfer pages immediately. If sparse image flashing
/// with many small `write_vmo` calls ever shows measurable page-fault overhead from repeated
/// decommits, `clean_transfer_buffer()` can be moved to [`BlockPartitionClient::flush`] for the
/// write path.
struct CleanTransferBufferGuard<'a, B: BlockClient>(&'a BlockDevice<B>);

impl<B: BlockClient> Drop for CleanTransferBufferGuard<'_, B> {
    fn drop(&mut self) {
        self.0.clean_transfer_buffer();
    }
}

#[async_trait(?Send)]
impl<B: BlockClient + 'static> PartitionClient for BlockPartitionClient<B> {
    fn partition_size(&self) -> Result<u64, zx::Status> {
        let block_size = u64::from(self.device.block_size());
        block_size.checked_mul(self.device.block_count()).ok_or(zx::Status::OUT_OF_RANGE)
    }

    async fn read_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status> {
        validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
        if size == 0 {
            return Ok(());
        }
        let block_size = u64::from(self.device.block_size());
        let block_size_usize = usize::try_from(block_size).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        let segments = block_io_segments(vmo_offset, dev_offset, size, block_size)?;
        let _cleanup = CleanTransferBufferGuard(&self.device);

        if let Some(head) = segments.head {
            let mut block_buf = self.device.allocate_buffer(block_size_usize).await;
            self.device
                .read(head.aligned_dev_offset, block_buf.as_mut())
                .await
                .map_err(|err| status_from_error(&err))?;
            block_buf.subslice(head.in_block_range).copy_to_vmo(vmo, head.vmo_offset)?;
        }

        if let Some(aligned) = segments.aligned {
            self.read_aligned_blocks(
                vmo,
                aligned.vmo_offset,
                aligned.aligned_dev_offset,
                aligned.len,
            )
            .await?;
        }

        if let Some(tail) = segments.tail {
            let mut block_buf = self.device.allocate_buffer(block_size_usize).await;
            self.device
                .read(tail.aligned_dev_offset, block_buf.as_mut())
                .await
                .map_err(|err| status_from_error(&err))?;
            block_buf.subslice(tail.in_block_range).copy_to_vmo(vmo, tail.vmo_offset)?;
        }

        Ok(())
    }

    async fn write_vmo(
        &self,
        vmo: &zx::Vmo,
        vmo_offset: u64,
        dev_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status> {
        validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
        if size == 0 {
            return Ok(());
        }
        let block_size = u64::from(self.device.block_size());
        let block_size_usize = usize::try_from(block_size).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        let segments = block_io_segments(vmo_offset, dev_offset, size, block_size)?;
        let _cleanup = CleanTransferBufferGuard(&self.device);

        if let Some(head) = segments.head {
            let mut block_buf = self.device.allocate_buffer(block_size_usize).await;
            self.device
                .read(head.aligned_dev_offset, block_buf.as_mut())
                .await
                .map_err(|err| status_from_error(&err))?;
            block_buf.subslice_mut(head.in_block_range).copy_from_vmo(vmo, head.vmo_offset)?;
            self.device
                .write(head.aligned_dev_offset, block_buf.as_ref())
                .await
                .map_err(|err| status_from_error(&err))?;
        }

        if let Some(aligned) = segments.aligned {
            self.write_aligned_blocks(
                vmo,
                aligned.vmo_offset,
                aligned.aligned_dev_offset,
                aligned.len,
            )
            .await?;
        }

        if let Some(tail) = segments.tail {
            let mut block_buf = self.device.allocate_buffer(block_size_usize).await;
            self.device
                .read(tail.aligned_dev_offset, block_buf.as_mut())
                .await
                .map_err(|err| status_from_error(&err))?;
            block_buf.subslice_mut(tail.in_block_range).copy_from_vmo(vmo, tail.vmo_offset)?;
            self.device
                .write(tail.aligned_dev_offset, block_buf.as_ref())
                .await
                .map_err(|err| status_from_error(&err))?;
        }

        Ok(())
    }

    async fn trim_tail(&self, offset: u64) -> Result<(), zx::Status> {
        let partition_size = self.partition_size()?;
        if offset > partition_size {
            return Err(zx::Status::OUT_OF_RANGE);
        }
        if !self.device.supports_trim() {
            return Ok(());
        }
        let block_size = u64::from(self.device.block_size());
        // `BlockFifoRequest.length` is a `u32` block count, so each `Device::trim` call can span
        // at most `u32::MAX` blocks.
        let max_trim_bytes = u64::from(u32::MAX) * block_size;
        let mut cur_offset = offset.next_multiple_of(block_size);
        while cur_offset < partition_size {
            let chunk_end = cur_offset + (partition_size - cur_offset).min(max_trim_bytes);
            self.device.trim(cur_offset..chunk_end).await.map_err(|err| status_from_error(&err))?;
            cur_offset = chunk_end;
        }
        Ok(())
    }

    async fn flush(&self) -> Result<(), zx::Status> {
        self.device.flush().await.map_err(|err| status_from_error(&err))
    }
}

/// Computes the bounce buffer byte capacity for a block-aligned transfer of `aligned_len` bytes,
/// given an already block-aligned `max_bounce_buffer_bytes`.
fn aligned_bounce_buffer_bytes(max_bounce_buffer_bytes: usize, aligned_len: u64) -> usize {
    usize::try_from(aligned_len).unwrap_or(usize::MAX).min(max_bounce_buffer_bytes)
}

/// Pre-computed partial-block (`head` or `tail`) segment for read-modify-write I/O.
#[derive(Debug, PartialEq, Eq)]
struct PartialBlockSegment {
    /// Block-aligned device byte offset of the block containing this partial segment.
    aligned_dev_offset: u64,
    /// Byte offset in the caller's VMO where this segment starts.
    vmo_offset: u64,
    /// Byte range within the single-block buffer (`0..block_size`) covered by this segment.
    in_block_range: Range<usize>,
}

/// Pre-computed block-aligned middle segment for direct or chunked block I/O.
#[derive(Debug, PartialEq, Eq)]
struct AlignedBlockSegment {
    /// Block-aligned device byte offset where the middle segment starts.
    aligned_dev_offset: u64,
    /// Byte offset in the caller's VMO where the middle segment starts.
    vmo_offset: u64,
    /// Block-aligned byte length of the middle segment (`> 0`).
    len: u64,
}

/// Decomposition of a `[dev_offset, dev_offset + size)` transfer into optional partial leading
/// (`head`), block-aligned middle (`aligned`), and partial trailing (`tail`) segments.
#[derive(Debug, Default, PartialEq, Eq)]
struct BlockIoSegments {
    head: Option<PartialBlockSegment>,
    aligned: Option<AlignedBlockSegment>,
    tail: Option<PartialBlockSegment>,
}

/// Splits a transfer starting at `vmo_offset` and `dev_offset` of `size` bytes into optional
/// partial leading (`head`), full-block (`aligned`), and partial trailing (`tail`) segments
/// relative to `block_size`.
fn block_io_segments(
    vmo_offset: u64,
    dev_offset: u64,
    size: u64,
    block_size: u64,
) -> Result<BlockIoSegments, zx::Status> {
    let end = dev_offset + size;
    let head_end = if dev_offset % block_size != 0 {
        end.min(dev_offset.next_multiple_of(block_size))
    } else {
        dev_offset
    };
    let aligned_end = head_end + ((end - head_end) / block_size) * block_size;

    let head = if dev_offset < head_end {
        let aligned_dev_offset = dev_offset - (dev_offset % block_size);
        let in_block_start = usize::try_from(dev_offset - aligned_dev_offset)
            .map_err(|_| zx::Status::OUT_OF_RANGE)?;
        let len = usize::try_from(head_end - dev_offset).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        Some(PartialBlockSegment {
            aligned_dev_offset,
            vmo_offset,
            in_block_range: in_block_start..in_block_start + len,
        })
    } else {
        None
    };

    let aligned = if head_end < aligned_end {
        Some(AlignedBlockSegment {
            aligned_dev_offset: head_end,
            vmo_offset: vmo_offset + (head_end - dev_offset),
            len: aligned_end - head_end,
        })
    } else {
        None
    };

    let tail = if aligned_end < end {
        let len = usize::try_from(end - aligned_end).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        Some(PartialBlockSegment {
            aligned_dev_offset: aligned_end,
            vmo_offset: vmo_offset + (aligned_end - dev_offset),
            in_block_range: 0..len,
        })
    } else {
        None
    };

    Ok(BlockIoSegments { head, aligned, tail })
}

/// Validates that `[offset, offset + size)` is within `client.partition_size()`.
pub(crate) fn validate_range<P: PartitionClient + ?Sized>(
    client: &P,
    offset: u64,
    size: u64,
) -> Result<(), zx::Status> {
    let partition_size = client.partition_size()?;
    let end_offset = offset.checked_add(size).ok_or(zx::Status::OUT_OF_RANGE)?;
    if end_offset > partition_size {
        return Err(zx::Status::OUT_OF_RANGE);
    }
    Ok(())
}

/// Validates both partition bounds `[dev_offset, dev_offset + size)` and VMO bounds
/// `[vmo_offset, vmo_offset + size)`.
pub(crate) fn validate_vmo_range<P: PartitionClient + ?Sized>(
    client: &P,
    vmo: &zx::Vmo,
    vmo_offset: u64,
    dev_offset: u64,
    size: u64,
) -> Result<(), zx::Status> {
    validate_range(client, dev_offset, size)?;
    if size > 0 {
        let vmo_end = vmo_offset.checked_add(size).ok_or(zx::Status::OUT_OF_RANGE)?;
        if vmo_end > vmo.get_size()? {
            return Err(zx::Status::OUT_OF_RANGE);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use block_client::{BlockDeviceFlag, BufferSlice, MutableBufferSlice, VmoId};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    pub(crate) const TEST_BLOCK_SIZE: usize = 512;
    pub(crate) const TEST_PARTITION_SIZE: usize = 2560; // 5 blocks

    /// In-memory [`PartitionClient`] for unit tests.
    #[derive(Clone)]
    pub(crate) struct FakePartitionClient {
        data: Rc<RefCell<Vec<u8>>>,
        block_size: usize,
        flushes: Rc<Cell<u32>>,
    }

    impl FakePartitionClient {
        /// Creates a [`FakePartitionClient`] with `size` bytes initialized to `fill` and a
        /// simulated `block_size` for `trim_tail`.
        pub(crate) fn new(size: usize, block_size: usize, fill: u8) -> Self {
            Self {
                data: Rc::new(RefCell::new(vec![fill; size])),
                block_size,
                flushes: Rc::new(Cell::new(0)),
            }
        }

        /// Returns a copy of the current in-memory partition contents.
        pub(crate) fn bytes(&self) -> Vec<u8> {
            self.data.borrow().clone()
        }

        /// Returns the number of times [`PartitionClient::flush`] has been called.
        #[allow(dead_code)]
        pub(crate) fn flushes(&self) -> u32 {
            self.flushes.get()
        }
    }

    #[async_trait(?Send)]
    impl PartitionClient for FakePartitionClient {
        fn partition_size(&self) -> Result<u64, zx::Status> {
            Ok(self.data.borrow().len() as u64)
        }

        async fn read_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
            if size > 0 {
                let start = dev_offset as usize;
                let end = start + size as usize;
                vmo.write(&self.data.borrow()[start..end], vmo_offset)?;
            }
            Ok(())
        }

        async fn write_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            validate_vmo_range(self, vmo, vmo_offset, dev_offset, size)?;
            if size > 0 {
                let info = vmo.info()?;
                if info.flags.contains(zx::VmoInfoFlags::PAGER_BACKED) {
                    assert_ne!(info.parent_koid, zx::Koid::from_raw(0));
                    assert!(info.committed_bytes >= size);
                }
                let start = dev_offset as usize;
                let end = start + size as usize;
                vmo.read(&mut self.data.borrow_mut()[start..end], vmo_offset)?;
            }
            Ok(())
        }

        async fn trim_tail(&self, offset: u64) -> Result<(), zx::Status> {
            validate_range(self, offset, 0)?;
            let aligned_start = (offset as usize).next_multiple_of(self.block_size);
            let mut data = self.data.borrow_mut();
            if aligned_start < data.len() {
                data[aligned_start..].fill(0xab);
            }
            Ok(())
        }

        async fn flush(&self) -> Result<(), zx::Status> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }

    /// In-memory [`BoardPartitions`] for unit tests.
    pub(crate) struct FakeBoardPartitions {
        partitions: RefCell<HashMap<String, Option<FakePartitionClient>>>,
        pub(crate) flushes: Rc<Cell<u32>>,
    }

    impl FakeBoardPartitions {
        /// Creates an empty [`FakeBoardPartitions`].
        pub(crate) fn new() -> Self {
            Self { partitions: RefCell::new(HashMap::new()), flushes: Rc::new(Cell::new(0)) }
        }

        /// Registers an in-memory partition named `name` filled with `fill`.
        pub(crate) fn add_partition(&self, name: &str, fill: u8) -> FakePartitionClient {
            self.add_partition_with_size(name, TEST_PARTITION_SIZE, fill)
        }

        /// Registers an in-memory partition named `name` with `size` bytes filled with `fill`.
        pub(crate) fn add_partition_with_size(
            &self,
            name: &str,
            size: usize,
            fill: u8,
        ) -> FakePartitionClient {
            let client = FakePartitionClient::new(size, TEST_BLOCK_SIZE, fill);
            self.partitions.borrow_mut().insert(name.to_string(), Some(client.clone()));
            client
        }

        /// Registers `name` as a supported partition name that is missing on storage.
        pub(crate) fn add_missing_partition(&self, name: &str) {
            self.partitions.borrow_mut().insert(name.to_string(), None);
        }
    }

    #[async_trait(?Send)]
    impl BoardPartitions for FakeBoardPartitions {
        async fn open_partition(
            &self,
            partition_name: &str,
            _block_dir: &fidl_next::ClientEnd<fio::Directory>,
        ) -> Result<Box<dyn PartitionClient>, zx::Status> {
            match self.partitions.borrow().get(partition_name) {
                Some(Some(client)) => Ok(Box::new(client.clone())),
                Some(None) => Err(zx::Status::NOT_FOUND),
                None => Err(zx::Status::NOT_SUPPORTED),
            }
        }

        async fn flush(&self) -> Result<(), zx::Status> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }

    /// Verifies unaligned VMO reads and writes and out-of-range bounds validation.
    #[fuchsia::test]
    async fn test_vmo_read_and_write_unaligned_and_out_of_range() {
        let client = FakePartitionClient::new(4096, 512, 0x33);

        let payload: Vec<u8> = (0..1234).map(|i| ((i * 5 + 1) % 251) as u8).collect();
        let vmo = zx::Vmo::create(4096).expect("create vmo");
        vmo.write(&payload, 17).expect("write vmo at offset 17");

        // Write 1234 bytes from vmo offset 17 to device offset 333.
        client.write_vmo(&vmo, 17, 333, 1234).await.expect("write_vmo unaligned");

        let read_vmo = zx::Vmo::create(4096).expect("create read vmo");
        client.read_vmo(&read_vmo, 29, 333, 1234).await.expect("read_vmo unaligned");
        let mut read_back = vec![0u8; 1234];
        read_vmo.read(&mut read_back, 29).expect("read from vmo");
        assert_eq!(read_back, payload);

        // Surrounding bytes outside [333..1567) are untouched.
        let full = client.bytes();
        assert_eq!(&full[..333], &vec![0x33u8; 333][..]);
        assert_eq!(&full[333..1567], &payload[..]);
        assert_eq!(&full[1567..], &vec![0x33u8; 4096 - 1567][..]);

        // Out-of-range and u64::MAX overflow checks on partition and VMO bounds.
        assert_eq!(client.write_vmo(&vmo, 0, 4000, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 0, 4000, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, 4000, 0, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 4000, 0, 100).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, 0, u64::MAX, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, 0, u64::MAX, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.write_vmo(&vmo, u64::MAX, 0, 1).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(client.read_vmo(&read_vmo, u64::MAX, 0, 1).await, Err(zx::Status::OUT_OF_RANGE));
    }

    /// Future that yields once before completing, allowing concurrent futures on a single-threaded
    /// executor to interleave across `.await` points during block I/O.
    struct YieldOnce(bool);

    impl std::future::Future for YieldOnce {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    /// Configurable [`BlockClient`] wrapper around [`fake_block_client::FakeBlockClient`] for
    /// testing transfer VMO attachment counts, chunking, concurrency, flags, and error handling.
    struct TestBlockClient {
        inner: fake_block_client::FakeBlockClient,
        override_block_size: Option<u32>,
        override_block_count: Option<u64>,
        block_flags: BlockDeviceFlag,
        attach_count: std::sync::atomic::AtomicU32,
        io_calls: std::sync::atomic::AtomicU32,
        in_flight_io: std::sync::atomic::AtomicU32,
        max_in_flight_io: std::sync::atomic::AtomicU32,
        yield_on_io: bool,
        fail_on_io_call: Option<u32>,
    }

    impl TestBlockClient {
        fn new(block_size: u32, block_count: usize) -> Self {
            Self {
                inner: fake_block_client::FakeBlockClient::new(block_size, block_count),
                override_block_size: None,
                override_block_count: None,
                block_flags: BlockDeviceFlag::TRIM_SUPPORT,
                attach_count: std::sync::atomic::AtomicU32::new(0),
                io_calls: std::sync::atomic::AtomicU32::new(0),
                in_flight_io: std::sync::atomic::AtomicU32::new(0),
                max_in_flight_io: std::sync::atomic::AtomicU32::new(0),
                yield_on_io: false,
                fail_on_io_call: None,
            }
        }
    }

    impl BlockClient for TestBlockClient {
        async unsafe fn attach_vmo(&self, vmo: &zx::Vmo) -> Result<VmoId, zx::Status> {
            self.attach_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // SAFETY: Delegating to FakeBlockClient in test.
            unsafe { self.inner.attach_vmo(vmo).await }
        }

        async fn detach_vmo(&self, vmo_id: VmoId) -> Result<(), zx::Status> {
            self.inner.detach_vmo(vmo_id).await
        }

        async fn read_at_with_opts_traced(
            &self,
            buffer_slice: MutableBufferSlice<'_>,
            device_offset: u64,
            opts: block_client::ReadOptions,
            trace_flow_id: u64,
        ) -> Result<(), zx::Status> {
            let call = self.io_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if self.fail_on_io_call == Some(call) {
                return Err(zx::Status::IO);
            }
            self.inner
                .read_at_with_opts_traced(buffer_slice, device_offset, opts, trace_flow_id)
                .await
        }

        async fn write_at_with_opts_traced(
            &self,
            buffer_slice: BufferSlice<'_>,
            device_offset: u64,
            opts: block_client::WriteOptions,
            trace_flow_id: u64,
        ) -> Result<(), zx::Status> {
            let call = self.io_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if self.fail_on_io_call == Some(call) {
                return Err(zx::Status::IO);
            }
            let cur_in_flight =
                self.in_flight_io.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            self.max_in_flight_io.fetch_max(cur_in_flight, std::sync::atomic::Ordering::SeqCst);
            if self.yield_on_io {
                YieldOnce(false).await;
            }
            let res = self
                .inner
                .write_at_with_opts_traced(buffer_slice, device_offset, opts, trace_flow_id)
                .await;
            self.in_flight_io.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            res
        }

        async fn trim_traced(
            &self,
            range: std::ops::Range<u64>,
            trace_flow_id: u64,
        ) -> Result<(), zx::Status> {
            self.inner.trim_traced(range, trace_flow_id).await
        }

        async fn flush_traced(&self, trace_flow_id: u64) -> Result<(), zx::Status> {
            self.inner.flush_traced(trace_flow_id).await
        }

        async fn close(&self) -> Result<(), zx::Status> {
            self.inner.close().await
        }

        fn block_size(&self) -> u32 {
            self.override_block_size.unwrap_or_else(|| self.inner.block_size())
        }

        fn block_count(&self) -> u64 {
            self.override_block_count.unwrap_or_else(|| self.inner.block_count())
        }

        fn max_transfer_blocks(&self) -> Option<std::num::NonZero<u32>> {
            self.inner.max_transfer_blocks()
        }

        fn block_flags(&self) -> BlockDeviceFlag {
            self.block_flags
        }

        fn is_connected(&self) -> bool {
            self.inner.is_connected()
        }
    }

    /// Table test for [`block_io_segments`] across all 6 boundary alignment shapes.
    #[fuchsia::test]
    fn test_block_io_segments_all_shapes() {
        let bs = 512u64;
        // 1. Empty transfer.
        assert_eq!(
            block_io_segments(10, 100, 0, bs).unwrap(),
            BlockIoSegments { head: None, aligned: None, tail: None }
        );
        // 2. Sub-block transfer within a single block.
        assert_eq!(
            block_io_segments(10, 100, 200, bs).unwrap(),
            BlockIoSegments {
                head: Some(PartialBlockSegment {
                    aligned_dev_offset: 0,
                    vmo_offset: 10,
                    in_block_range: 100..300,
                }),
                aligned: None,
                tail: None,
            }
        );
        // 3. Head partial block + aligned middle (ends on block boundary 1024).
        assert_eq!(
            block_io_segments(10, 100, 924, bs).unwrap(),
            BlockIoSegments {
                head: Some(PartialBlockSegment {
                    aligned_dev_offset: 0,
                    vmo_offset: 10,
                    in_block_range: 100..512,
                }),
                aligned: Some(AlignedBlockSegment {
                    aligned_dev_offset: 512,
                    vmo_offset: 422,
                    len: 512,
                }),
                tail: None,
            }
        );
        // 4. Aligned middle + tail partial block (starts on block boundary 512).
        assert_eq!(
            block_io_segments(10, 512, 700, bs).unwrap(),
            BlockIoSegments {
                head: None,
                aligned: Some(AlignedBlockSegment {
                    aligned_dev_offset: 512,
                    vmo_offset: 10,
                    len: 512,
                }),
                tail: Some(PartialBlockSegment {
                    aligned_dev_offset: 1024,
                    vmo_offset: 522,
                    in_block_range: 0..188,
                }),
            }
        );
        // 5. Aligned middle only.
        assert_eq!(
            block_io_segments(10, 512, 1024, bs).unwrap(),
            BlockIoSegments {
                head: None,
                aligned: Some(AlignedBlockSegment {
                    aligned_dev_offset: 512,
                    vmo_offset: 10,
                    len: 1024,
                }),
                tail: None,
            }
        );
        // 6. Head partial block + aligned middle + tail partial block.
        assert_eq!(
            block_io_segments(10, 100, 1600, bs).unwrap(),
            BlockIoSegments {
                head: Some(PartialBlockSegment {
                    aligned_dev_offset: 0,
                    vmo_offset: 10,
                    in_block_range: 100..512,
                }),
                aligned: Some(AlignedBlockSegment {
                    aligned_dev_offset: 512,
                    vmo_offset: 422,
                    len: 1024,
                }),
                tail: Some(PartialBlockSegment {
                    aligned_dev_offset: 1536,
                    vmo_offset: 1446,
                    in_block_range: 0..164,
                }),
            }
        );
    }

    const TEST_BOUNCE_BYTES: NonZero<usize> = NonZero::new(4 * 1024 * 1024).unwrap();

    /// Verifies [`PartitionClient`] implemented for [`BlockPartitionClient`] wrapping
    /// [`fake_block_client::FakeBlockClient`] across unaligned sub-block and multi-block RMW
    /// transfers, default `read_at`/`write_at`, and `trim_tail`.
    #[fuchsia::test]
    async fn test_fake_block_client_as_partition_client() {
        let client = BlockPartitionClient::new(
            fake_block_client::FakeBlockClient::new(512, 8),
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        )
        .await
        .unwrap();
        assert_eq!(client.partition_size().unwrap(), 4096);

        // Seed entire partition with 0x55 via default write_at.
        client.write_at(&vec![0x55u8; 4096], 0).await.unwrap();
        assert_eq!(client.client().flush_count(), 0);

        // 1. Single-block unaligned RMW via write_vmo and read_vmo.
        let vmo = zx::Vmo::create(4096).unwrap();
        let sub_payload: Vec<u8> = (0..120).map(|i| (i + 1) as u8).collect();
        vmo.write(&sub_payload, 13).unwrap();
        client.write_vmo(&vmo, 13, 100, 120).await.unwrap();
        assert_eq!(client.client().flush_count(), 0);
        client.flush().await.unwrap();
        assert_eq!(client.client().flush_count(), 1);

        let read_vmo = zx::Vmo::create(4096).unwrap();
        client.read_vmo(&read_vmo, 7, 100, 120).await.unwrap();
        let mut sub_back = vec![0u8; 120];
        read_vmo.read(&mut sub_back, 7).unwrap();
        assert_eq!(sub_back, sub_payload);

        // Surrounding bytes in block 0 outside [100..220) are preserved.
        let mut block0 = vec![0u8; 512];
        client.read_at(&mut block0, 0).await.unwrap();
        assert_eq!(&block0[..100], &vec![0x55u8; 100][..]);
        assert_eq!(&block0[100..220], &sub_payload[..]);
        assert_eq!(&block0[220..], &vec![0x55u8; 512 - 220][..]);

        // 2. Multi-block head + aligned + tail RMW via write_vmo and read_vmo.
        let multi_payload: Vec<u8> = (0..1600).map(|i| ((i * 7 + 3) % 253) as u8).collect();
        vmo.write(&multi_payload, 512).unwrap();
        client.write_vmo(&vmo, 512, 600, 1600).await.unwrap();
        assert_eq!(client.client().flush_count(), 1);
        client.flush().await.unwrap();
        assert_eq!(client.client().flush_count(), 2);

        let mut verify_all = vec![0u8; 4096];
        client.read_at(&mut verify_all, 0).await.unwrap();
        assert_eq!(&verify_all[512..600], &vec![0x55u8; 88][..]);
        assert_eq!(&verify_all[600..2200], &multi_payload[..]);
        assert_eq!(&verify_all[2200..], &vec![0x55u8; 4096 - 2200][..]);

        // 3. trim_tail: unaligned offset rounds up to next block boundary (2500 -> 2560).
        client.trim_tail(2500).await.unwrap();
        client.read_at(&mut verify_all, 0).await.unwrap();
        assert_eq!(&verify_all[2200..2560], &vec![0x55u8; 360][..]);
        assert_eq!(&verify_all[2560..], &vec![0xabu8; 4096 - 2560][..]);

        // Aligned offset trims from exact boundary (2048).
        client.trim_tail(2048).await.unwrap();
        client.read_at(&mut verify_all, 0).await.unwrap();
        assert_eq!(&verify_all[2048..], &vec![0xabu8; 4096 - 2048][..]);

        // offset == partition_size is a no-op; offset > partition_size returns OUT_OF_RANGE.
        client.trim_tail(4096).await.unwrap();
        assert_eq!(client.trim_tail(4097).await, Err(zx::Status::OUT_OF_RANGE));
    }

    /// Verifies transfer VMO transfers across all caller VMO alignments, handle rights, VMO flags,
    /// and multi-chunk transfers.
    #[fuchsia::test]
    async fn test_bounce_buffer_and_caller_vmo_shapes() {
        use std::sync::atomic::Ordering::SeqCst;
        let client = BlockPartitionClient::new(
            TestBlockClient::new(512, 64),
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        )
        .await
        .unwrap(); // 32 KiB
        assert_eq!(client.client().attach_count.load(SeqCst), 1);
        let payload: Vec<u8> = (0..1024).map(|i| ((i * 11 + 5) % 251) as u8).collect();
        let mut buf = vec![0u8; 1024];

        // 1. Aligned mappable R/W VMO reuses the single attached transfer VMO across calls.
        let vmo = zx::Vmo::create(4096).unwrap();
        vmo.write(&payload, 512).unwrap();
        client.write_vmo(&vmo, 512, 512, 1024).await.unwrap();
        assert_eq!(client.client().attach_count.load(SeqCst), 1);

        let read_vmo = zx::Vmo::create(4096).unwrap();
        client.read_vmo(&read_vmo, 512, 512, 1024).await.unwrap();
        assert_eq!(client.client().attach_count.load(SeqCst), 1);
        read_vmo.read(&mut buf, 512).unwrap();
        assert_eq!(buf, payload);

        // 2. Unaligned vmo_offset also reuses the attached transfer VMO cleanly.
        let unaligned_vmo = zx::Vmo::create(4096).unwrap();
        unaligned_vmo.write(&payload, 17).unwrap();
        client.write_vmo(&unaligned_vmo, 17, 2048, 1024).await.unwrap();
        let unaligned_read_vmo = zx::Vmo::create(4096).unwrap();
        client.read_vmo(&unaligned_read_vmo, 23, 2048, 1024).await.unwrap();
        assert_eq!(client.client().attach_count.load(SeqCst), 1);
        unaligned_read_vmo.read(&mut buf, 23).unwrap();
        assert_eq!(buf, payload);

        // 3. Caller VMO missing ZX_RIGHT_WRITE succeeds on write_vmo via the transfer VMO.
        let ro_rights =
            zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        let ro_vmo = vmo.duplicate_handle(ro_rights).unwrap();
        client.write_vmo(&ro_vmo, 512, 4096, 1024).await.unwrap();

        // 4. Caller VMO missing ZX_RIGHT_READ succeeds on read_vmo via the transfer VMO.
        let wo_rights =
            zx::Rights::BASIC | zx::Rights::WRITE | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        let wo_vmo = read_vmo.duplicate_handle(wo_rights).unwrap();
        client.read_vmo(&wo_vmo, 512, 4096, 1024).await.unwrap();
        read_vmo.read(&mut buf, 512).unwrap();
        assert_eq!(buf, payload);

        // 5. Caller VMO missing ZX_RIGHT_MAP succeeds on both write_vmo and read_vmo.
        let nomap_rights =
            zx::Rights::BASIC | zx::Rights::READ | zx::Rights::WRITE | zx::Rights::GET_PROPERTY;
        let nomap_vmo = vmo.duplicate_handle(nomap_rights).unwrap();
        client.write_vmo(&nomap_vmo, 512, 6144, 1024).await.unwrap();
        client.read_vmo(&nomap_vmo, 1536, 6144, 1024).await.unwrap();
        nomap_vmo.read(&mut buf, 1536).unwrap();
        assert_eq!(buf, payload);

        // 6. RESIZABLE caller VMO succeeds on both write_vmo and read_vmo.
        let resizable_vmo = zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 4096).unwrap();
        resizable_vmo.write(&payload, 0).unwrap();
        client.write_vmo(&resizable_vmo, 0, 8192, 1024).await.unwrap();
        client.read_vmo(&resizable_vmo, 1024, 8192, 1024).await.unwrap();
        resizable_vmo.read(&mut buf, 1024).unwrap();
        assert_eq!(buf, payload);

        // 7. PAGER_BACKED caller VMO succeeds via the transfer VMO.
        let port = zx::Port::create();
        let pager = zx::Pager::create(zx::PagerOptions::empty()).unwrap();
        let pager_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, 1, 4096).unwrap();
        let aux_vmo = zx::Vmo::create(4096).unwrap();
        aux_vmo.write(&payload, 0).unwrap();
        pager.supply_pages(&pager_vmo, 0..4096, &aux_vmo, 0).unwrap();
        let child_vmo = pager_vmo
            .create_child(
                zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE | zx::VmoChildOptions::NO_WRITE,
                0,
                4096,
            )
            .unwrap();
        client.write_vmo(&child_vmo, 0, 10240, 1024).await.unwrap();

        // 8. Multi-chunk transfer with custom bounce buffer size (1024 bytes = 2 blocks) and a
        // larger sparse transfer VMO pool (8192 bytes) across 5 chunks.
        let wrapped = BlockPartitionClient::new(
            TestBlockClient::new(512, 64),
            std::num::NonZero::new(1024).unwrap(),
            std::num::NonZero::new(8192).unwrap(),
        )
        .await
        .unwrap();
        let large_len = 1024 * 4 + 512; // 4608 bytes (9 blocks -> 5 chunks: 1024*4 + 512)
        let large_len_u64 = u64::try_from(large_len).unwrap();
        let large_payload: Vec<u8> = (0..large_len).map(|i| (i % 251) as u8).collect();
        let large_vmo = zx::Vmo::create(large_len_u64 + 512).unwrap();
        large_vmo.write(&large_payload, 9).unwrap();

        wrapped.client().io_calls.store(0, SeqCst);
        wrapped.write_vmo(&large_vmo, 9, 12288, large_len_u64).await.unwrap();
        assert_eq!(wrapped.client().io_calls.load(SeqCst), 5);

        wrapped.client().io_calls.store(0, SeqCst);
        let large_read_vmo = zx::Vmo::create(large_len_u64 + 512).unwrap();
        wrapped.read_vmo(&large_read_vmo, 15, 12288, large_len_u64).await.unwrap();
        assert_eq!(wrapped.client().io_calls.load(SeqCst), 5);
        let mut large_back = vec![0u8; large_len];
        large_read_vmo.read(&mut large_back, 15).unwrap();
        assert_eq!(large_back, large_payload);

        // 9. Single large block I/O chunk (600 KiB) transferred in a single pass.
        let multi_copy_client = BlockPartitionClient::new(
            TestBlockClient::new(512, 2048),
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        )
        .await
        .unwrap(); // 1 MiB
        let multi_copy_len = 600 * 1024;
        let multi_copy_len_u64 = u64::try_from(multi_copy_len).unwrap();
        let multi_copy_payload: Vec<u8> =
            (0..multi_copy_len).map(|i| ((i * 13 + 7) % 251) as u8).collect();
        let multi_copy_vmo = zx::Vmo::create(multi_copy_len_u64).unwrap();
        multi_copy_vmo.write(&multi_copy_payload, 0).unwrap();

        multi_copy_client.client().io_calls.store(0, SeqCst);
        multi_copy_client.write_vmo(&multi_copy_vmo, 0, 0, multi_copy_len_u64).await.unwrap();
        assert_eq!(multi_copy_client.client().io_calls.load(SeqCst), 1);

        multi_copy_client.client().io_calls.store(0, SeqCst);
        let multi_copy_read_vmo = zx::Vmo::create(multi_copy_len_u64).unwrap();
        multi_copy_client.read_vmo(&multi_copy_read_vmo, 0, 0, multi_copy_len_u64).await.unwrap();
        assert_eq!(multi_copy_client.client().io_calls.load(SeqCst), 1);
        let mut multi_copy_back = vec![0u8; multi_copy_len];
        multi_copy_read_vmo.read(&mut multi_copy_back, 0).unwrap();
        assert_eq!(multi_copy_back, multi_copy_payload);
    }

    /// Verifies edge cases (`block_size == 0`, non-power-of-two `block_size`,
    /// `transfer_vmo_pool_bytes < bounce_buffer_bytes`, overflow, `!TRIM_SUPPORT`,
    /// `BlockPartitionClient` bounce buffer sizing, concurrent `write_vmo` serialization and
    /// parallelism, and mid-loop I/O error recovery).
    #[fuchsia::test]
    async fn test_block_client_edge_cases() {
        use std::sync::atomic::Ordering::SeqCst;
        let vmo = zx::Vmo::create(8192).unwrap();

        // 1. block_size == 0, non-power-of-two, or transfer_vmo_pool_bytes < bounce_buffer_bytes
        // returns INVALID_ARGS on construction.
        let mut zero_bs_inner = TestBlockClient::new(512, 4);
        zero_bs_inner.override_block_size = Some(0);
        assert_eq!(
            BlockPartitionClient::new(zero_bs_inner, TEST_BOUNCE_BYTES, TEST_BOUNCE_BYTES)
                .await
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        let mut non_pot_inner = TestBlockClient::new(512, 4);
        non_pot_inner.override_block_size = Some(300);
        assert_eq!(
            BlockPartitionClient::new(non_pot_inner, TEST_BOUNCE_BYTES, TEST_BOUNCE_BYTES)
                .await
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        assert_eq!(
            BlockPartitionClient::new(
                TestBlockClient::new(512, 4),
                std::num::NonZero::new(2048).unwrap(),
                std::num::NonZero::new(1024).unwrap(),
            )
            .await
            .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // 2. block_size * block_count overflow returns OUT_OF_RANGE.
        let mut overflow_inner = TestBlockClient::new(512, 4);
        overflow_inner.override_block_count = Some(u64::MAX);
        let overflow_client =
            BlockPartitionClient::new(overflow_inner, TEST_BOUNCE_BYTES, TEST_BOUNCE_BYTES)
                .await
                .unwrap();
        assert_eq!(overflow_client.partition_size(), Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(overflow_client.read_vmo(&vmo, 0, 0, 512).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(overflow_client.write_vmo(&vmo, 0, 0, 512).await, Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(overflow_client.trim_tail(0).await, Err(zx::Status::OUT_OF_RANGE));

        // 3. !BlockDeviceFlag::TRIM_SUPPORT makes trim_tail a no-op.
        let mut no_trim_inner = TestBlockClient::new(512, 4);
        no_trim_inner.block_flags = BlockDeviceFlag::empty();
        let no_trim =
            BlockPartitionClient::new(no_trim_inner, TEST_BOUNCE_BYTES, TEST_BOUNCE_BYTES)
                .await
                .unwrap();
        no_trim.write_at(&[0x77u8; 2048], 0).await.unwrap();
        no_trim.trim_tail(0).await.unwrap();
        let mut check = vec![0u8; 2048];
        no_trim.read_at(&mut check, 0).await.unwrap();
        assert_eq!(check, vec![0x77u8; 2048]);

        // 4. BlockPartitionClient::new splits transfers into configured bounce_buffer_bytes chunks,
        // and sub-block capacities clamp up to 1 block.
        let custom_bounce = BlockPartitionClient::new(
            TestBlockClient::new(512, 16),
            std::num::NonZero::new(1024).unwrap(), // 2 blocks per chunk
            std::num::NonZero::new(4096).unwrap(), // 8-block pool
        )
        .await
        .unwrap();
        assert_eq!(custom_bounce.partition_size().unwrap(), 8192);
        let payload: Vec<u8> = (0..2560).map(|i| (i % 253) as u8).collect();
        vmo.write(&payload, 0).unwrap();
        custom_bounce.write_vmo(&vmo, 0, 0, 2560).await.unwrap();
        assert_eq!(custom_bounce.client().io_calls.load(SeqCst), 3); // 1024 + 1024 + 512
        assert_eq!(custom_bounce.client().attach_count.load(SeqCst), 1);

        custom_bounce.client().io_calls.store(0, SeqCst);
        let read_vmo = zx::Vmo::create(8192).unwrap();
        custom_bounce.read_vmo(&read_vmo, 5, 0, 2560).await.unwrap();
        assert_eq!(custom_bounce.client().io_calls.load(SeqCst), 3);
        let mut read_back = vec![0u8; 2560];
        read_vmo.read(&mut read_back, 5).unwrap();
        assert_eq!(read_back, payload);

        custom_bounce.trim_tail(2560).await.unwrap();
        let sub_block_bounce = BlockPartitionClient::new(
            TestBlockClient::new(512, 16),
            std::num::NonZero::new(100).unwrap(), // < 512-byte block_size -> clamps to 512 bytes
            std::num::NonZero::new(100).unwrap(),
        )
        .await
        .unwrap();
        sub_block_bounce.client().io_calls.store(0, SeqCst);
        sub_block_bounce.write_vmo(&vmo, 0, 0, 1536).await.unwrap();
        assert_eq!(sub_block_bounce.client().io_calls.load(SeqCst), 3); // 512 + 512 + 512

        // 5. Concurrent write_vmo calls:
        // (a) When transfer_vmo_pool_bytes == bounce_buffer_bytes, the second write_vmo blocks in
        // allocate_buffer until the first completes and frees its buffer (max_in_flight_io == 1).
        let mut serial_inner = TestBlockClient::new(512, 16);
        serial_inner.yield_on_io = true;
        let serial_client = BlockPartitionClient::new(
            serial_inner,
            std::num::NonZero::new(1024).unwrap(),
            std::num::NonZero::new(1024).unwrap(),
        )
        .await
        .unwrap();
        let vmo_a = zx::Vmo::create(4096).unwrap();
        vmo_a.write(&[0xaau8; 2048], 0).unwrap();
        let vmo_b = zx::Vmo::create(4096).unwrap();
        vmo_b.write(&[0xbbu8; 2048], 0).unwrap();
        let (res_a, res_b) = futures::future::join(
            serial_client.write_vmo(&vmo_a, 0, 0, 2048),
            serial_client.write_vmo(&vmo_b, 0, 2048, 2048),
        )
        .await;
        res_a.unwrap();
        res_b.unwrap();
        assert_eq!(serial_client.client().max_in_flight_io.load(SeqCst), 1);
        let mut serial_check = vec![0u8; 4096];
        serial_client.read_at(&mut serial_check, 0).await.unwrap();
        assert_eq!(&serial_check[..2048], &[0xaau8; 2048][..]);
        assert_eq!(&serial_check[2048..], &[0xbbu8; 2048][..]);

        // (b) When transfer_vmo_pool_bytes == 2 * bounce_buffer_bytes, two concurrent write_vmo
        // calls allocate separate buffers and execute in parallel (max_in_flight_io == 2), and when
        // the shorter 1-chunk write completes first and calls clean_transfer_buffer(), it only
        // decommits the freed region without disturbing the longer 2-chunk write's in-flight
        // buffer.
        let mut parallel_inner = TestBlockClient::new(512, 16);
        parallel_inner.yield_on_io = true;
        let parallel_client = BlockPartitionClient::new(
            parallel_inner,
            std::num::NonZero::new(1024).unwrap(),
            std::num::NonZero::new(2048).unwrap(),
        )
        .await
        .unwrap();
        let vmo_short = zx::Vmo::create(4096).unwrap();
        vmo_short.write(&[0xccu8; 1024], 0).unwrap();
        let vmo_long = zx::Vmo::create(4096).unwrap();
        vmo_long.write(&[0xddu8; 2048], 0).unwrap();
        let (res_short, res_long) = futures::future::join(
            parallel_client.write_vmo(&vmo_short, 0, 0, 1024),
            parallel_client.write_vmo(&vmo_long, 0, 1024, 2048),
        )
        .await;
        res_short.unwrap();
        res_long.unwrap();
        assert_eq!(parallel_client.client().max_in_flight_io.load(SeqCst), 2);
        let mut parallel_check = vec![0u8; 3072];
        parallel_client.read_at(&mut parallel_check, 0).await.unwrap();
        assert_eq!(&parallel_check[..1024], &[0xccu8; 1024][..]);
        assert_eq!(&parallel_check[1024..], &[0xddu8; 2048][..]);

        // 6. Mid-loop I/O error frees the allocated transfer buffer cleanly on both write_vmo and
        // read_vmo.
        let mut err_inner = TestBlockClient::new(512, 16);
        err_inner.fail_on_io_call = Some(2);
        let err_client = BlockPartitionClient::new(
            err_inner,
            std::num::NonZero::new(1024).unwrap(),
            std::num::NonZero::new(1024).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(err_client.write_vmo(&vmo, 0, 0, 2048).await, Err(zx::Status::IO));
        assert_eq!(err_client.client().attach_count.load(SeqCst), 1);

        err_client.client().io_calls.store(0, SeqCst);
        assert_eq!(err_client.read_vmo(&vmo, 0, 0, 2048).await, Err(zx::Status::IO));
        assert_eq!(err_client.client().attach_count.load(SeqCst), 1);
    }
}
