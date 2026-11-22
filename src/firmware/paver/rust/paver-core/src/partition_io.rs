// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Server implementation for the `fuchsia.paver.PartitionIo` FIDL protocol, providing scoped
//! byte-granularity partition I/O backed by [`BoardPartitions`].

use crate::partition::{BoardPartitions, PartitionClient};
use crate::{ResponderExt as _, status_from_error};
use anyhow::{Context as _, Error};
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_io as fio;
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_paver::partition_io;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

/// `fuchsia.paver.PartitionIo` implementation backed by a board's [`BoardPartitions`] and a
/// caller-supplied `block_dir`.
pub struct PartitionIo {
    board_partitions: Box<dyn BoardPartitions>,
    block_dir: fidl_next::ClientEnd<fio::Directory>,
    cached_clients: RefCell<HashMap<String, Rc<dyn PartitionClient>>>,
    dirty_partitions: RefCell<HashSet<String>>,
}

impl fmt::Debug for PartitionIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartitionIo").finish_non_exhaustive()
    }
}

impl PartitionIo {
    /// Creates a [`PartitionIo`] backed by `board_partitions` and a caller-supplied `block_dir`.
    pub fn new(
        board_partitions: Box<dyn BoardPartitions>,
        block_dir: fidl_next::ClientEnd<fio::Directory>,
    ) -> Self {
        Self {
            board_partitions,
            block_dir,
            cached_clients: RefCell::new(HashMap::new()),
            dirty_partitions: RefCell::new(HashSet::new()),
        }
    }

    /// Opens `partition` through `self.board_partitions` using `self.block_dir`, reusing a cached
    /// [`PartitionClient`] when available in this `PartitionIo` session.
    async fn open_partition(&self, partition: &str) -> Result<Rc<dyn PartitionClient>, zx::Status> {
        if let Some(client) = self.cached_clients.borrow().get(partition).cloned() {
            return Ok(client);
        }
        let client: Rc<dyn PartitionClient> =
            Rc::from(self.board_partitions.open_partition(partition, &self.block_dir).await?);
        self.cached_clients.borrow_mut().insert(partition.to_string(), client.clone());
        Ok(client)
    }

    /// Evicts `partition` from `self.cached_clients` if `status` indicates a potential
    /// device/session error, returning `true` if evicted.
    ///
    /// Cached `PartitionClient` instances may hold stateful block FIFO sessions (such as
    /// `RemoteBlockClient`) that do not reconnect once closed or broken, so evicting on
    /// device/session errors allows retries to open a fresh session. Caller validation errors
    /// (`INVALID_ARGS`, `OUT_OF_RANGE`) and capability errors (`NOT_SUPPORTED`, e.g. from
    /// `trim_tail` on partitions without trim support) do not indicate a broken session and
    /// retain the cached client.
    fn evict_cached_partition_on_error(&self, partition: &str, status: zx::Status) -> bool {
        if matches!(
            status,
            zx::Status::INVALID_ARGS | zx::Status::OUT_OF_RANGE | zx::Status::NOT_SUPPORTED
        ) {
            return false;
        }
        self.cached_clients.borrow_mut().remove(partition);
        true
    }

    /// Returns the total size in bytes of `partition`.
    pub async fn get_partition_size(&self, partition: &str) -> Result<u64, zx::Status> {
        let client = self.open_partition(partition).await?;
        client.partition_size().inspect_err(|&status| {
            self.evict_cached_partition_on_error(partition, status);
        })
    }

    /// Reads `size` bytes from `partition` starting at byte `offset` into a newly allocated VMO.
    pub async fn read_partition(
        &self,
        partition: &str,
        offset: u64,
        size: u64,
    ) -> Result<zx::Vmo, zx::Status> {
        let result = self.read_partition_inner(partition, offset, size).await;
        match &result {
            Ok(_) => log::info!(partition, offset, size; "Partition read successfully"),
            Err(err) => {
                self.evict_cached_partition_on_error(partition, status_from_error(err));
                log::warn!(partition, offset, size; "Partition read error: {err:#}");
            }
        }
        result.map_err(|err| status_from_error(&err))
    }

    /// Opens `partition`, allocates a VMO of `size` bytes, and reads `[offset, offset + size)` into
    /// it, attaching descriptive [`anyhow::Context`] on failure.
    async fn read_partition_inner(
        &self,
        partition: &str,
        offset: u64,
        size: u64,
    ) -> Result<zx::Vmo, Error> {
        if size == 0 {
            return Err(zx::Status::INVALID_ARGS).context("read size must be non-zero");
        }
        let client =
            self.open_partition(partition).await.context("failed to open partition for read")?;
        let vmo = zx::Vmo::create(size).context("failed to allocate VMO for partition read")?;
        client
            .read_vmo(&vmo, 0, offset, size)
            .await
            .context("failed to read partition into VMO")?;
        Ok(vmo)
    }

    /// Writes `payload` to `partition` starting at byte `offset`, handling bytes past the written
    /// range according to `trailing`.
    pub async fn write_partition(
        &self,
        partition: &str,
        offset: u64,
        payload: zx::Vmo,
        trailing: fpaver::TrailingBehavior,
    ) -> Result<(), zx::Status> {
        let size = match payload.get_content_size() {
            Ok(size) => size,
            Err(status) => {
                log::warn!(
                    partition,
                    offset;
                    "Partition write error: failed to get VMO content size: {status}"
                );
                return Err(status);
            }
        };
        let result = self.write_partition_inner(partition, offset, &payload, size, trailing).await;
        match &result {
            Ok(()) => log::info!(partition, offset, size; "Partition written successfully"),
            Err(err) => {
                self.evict_cached_partition_on_error(partition, status_from_error(err));
                log::warn!(partition, offset, size; "Partition write error: {err:#}");
            }
        }
        result.map_err(|err| status_from_error(&err))
    }

    /// Opens `partition` and writes `size` bytes from `payload` at `offset` according to
    /// `trailing`, attaching descriptive [`anyhow::Context`] on failure.
    async fn write_partition_inner(
        &self,
        partition: &str,
        offset: u64,
        payload: &zx::Vmo,
        size: u64,
        trailing: fpaver::TrailingBehavior,
    ) -> Result<(), Error> {
        let discard_trailing = match trailing {
            fpaver::TrailingBehavior::Retain => false,
            fpaver::TrailingBehavior::Discard => true,
            fpaver::TrailingBehavior::UnknownOrdinal_(_) => {
                return Err(zx::Status::INVALID_ARGS).context("unknown TrailingBehavior ordinal");
            }
        };
        let client =
            self.open_partition(partition).await.context("failed to open partition for write")?;
        client
            .write_vmo(payload, 0, offset, size)
            .await
            .context("failed to write partition VMO")?;
        if size > 0 || discard_trailing {
            self.dirty_partitions.borrow_mut().insert(partition.to_string());
        }
        if discard_trailing {
            // `write_vmo` validates that `offset + size <= partition_size`.
            if let Err(err) = client.trim_tail(offset + size).await
                && self.evict_cached_partition_on_error(partition, err)
            {
                log::warn!(
                    partition,
                    offset,
                    size,
                    err:?;
                    "Partition tail trim failed; evicted cached client"
                );
            }
        }
        Ok(())
    }

    /// Flushes all previously written partition data to persistent storage.
    ///
    /// Cached [`PartitionClient`] handles are retained across successful flushes (until an error
    /// evicts them or the `PartitionIo` session closes) so subsequent operations on the same
    /// partitions reuse open handles.
    pub async fn flush(&self) -> Result<(), zx::Status> {
        let dirty: Vec<String> = self.dirty_partitions.borrow().iter().cloned().collect();
        let mut flush_err = None;
        for partition in dirty {
            let flush_res = async {
                let client = self.open_partition(&partition).await?;
                client.flush().await
            }
            .await;
            match flush_res {
                Ok(()) => {
                    self.dirty_partitions.borrow_mut().remove(&partition);
                }
                Err(err) => {
                    self.evict_cached_partition_on_error(&partition, err);
                    log::warn!(partition = partition.as_str(), err:?; "Partition flush error");
                    flush_err = flush_err.or(Some(err));
                }
            }
        }
        let board_res = self.board_partitions.flush().await;
        if let Err(err) = board_res {
            log::warn!(err:?; "Board partitions flush error");
        }
        flush_err.map_or(board_res, Err)
    }
}

impl fpaver::PartitionIoLocalServerHandler for PartitionIo {
    async fn get_partition_size(
        &mut self,
        request: Request<partition_io::GetPartitionSize>,
        responder: Responder<partition_io::GetPartitionSize>,
    ) {
        let res = Self::get_partition_size(self, &request.payload().partition).await;
        responder.send_result(res).await;
    }

    async fn read_partition(
        &mut self,
        request: Request<partition_io::ReadPartition>,
        responder: Responder<partition_io::ReadPartition>,
    ) {
        let req = request.payload();
        let res = Self::read_partition(self, &req.partition, req.offset, req.size).await;
        responder.send_result(res).await;
    }

    async fn write_partition(
        &mut self,
        request: Request<partition_io::WritePartition>,
        responder: Responder<partition_io::WritePartition>,
    ) {
        let req = request.payload();
        let res =
            Self::write_partition(self, &req.partition, req.offset, req.payload, req.trailing)
                .await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<partition_io::Flush>) {
        let res = Self::flush(self).await;
        responder.send_result(res).await;
    }
}

/// Serves the `fuchsia.paver.PartitionIo` FIDL protocol over `server_end` using `partition_io`.
///
/// Returns an error if the client closes the channel with unflushed writes (without calling
/// `Flush`), rather than silently auto-flushing.
pub async fn serve_partition_io(
    partition_io: PartitionIo,
    server_end: fidl_next::ServerEnd<fpaver::PartitionIo>,
) -> Result<(), Error> {
    let partition_io = fidl_next::ServerDispatcher::new(server_end)
        .run_local(partition_io)
        .await
        .context("serving PartitionIo")?;
    let mut dirty: Vec<String> = partition_io.dirty_partitions.into_inner().into_iter().collect();
    if !dirty.is_empty() {
        dirty.sort();
        anyhow::bail!("PartitionIo channel closed with unflushed partitions: {dirty:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::tests::{
        FakeBoardPartitions, FakePartitionClient, TEST_BLOCK_SIZE, TEST_PARTITION_SIZE,
    };
    use async_trait::async_trait;
    use fuchsia_async as fasync;
    use std::cell::Cell;

    /// [`PartitionClient`] wrapper with per-operation injectable errors for caching and flush
    /// tests.
    #[derive(Clone)]
    struct FaultyPartition {
        inner: FakePartitionClient,
        fail_size: Rc<Cell<Option<zx::Status>>>,
        fail_read: Rc<Cell<Option<zx::Status>>>,
        fail_write: Rc<Cell<Option<zx::Status>>>,
        fail_trim: Rc<Cell<Option<zx::Status>>>,
        fail_flush: Rc<Cell<Option<zx::Status>>>,
    }

    impl FaultyPartition {
        fn new(fill: u8) -> Self {
            Self {
                inner: FakePartitionClient::new(TEST_PARTITION_SIZE, TEST_BLOCK_SIZE, fill),
                fail_size: Rc::new(Cell::new(None)),
                fail_read: Rc::new(Cell::new(None)),
                fail_write: Rc::new(Cell::new(None)),
                fail_trim: Rc::new(Cell::new(None)),
                fail_flush: Rc::new(Cell::new(None)),
            }
        }

        fn flushes(&self) -> u32 {
            self.inner.flushes()
        }
    }

    #[async_trait(?Send)]
    impl PartitionClient for FaultyPartition {
        fn partition_size(&self) -> Result<u64, zx::Status> {
            if let Some(err) = self.fail_size.get() {
                return Err(err);
            }
            self.inner.partition_size()
        }

        async fn read_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            if let Some(err) = self.fail_read.get() {
                return Err(err);
            }
            self.inner.read_vmo(vmo, vmo_offset, dev_offset, size).await
        }

        async fn write_vmo(
            &self,
            vmo: &zx::Vmo,
            vmo_offset: u64,
            dev_offset: u64,
            size: u64,
        ) -> Result<(), zx::Status> {
            if let Some(err) = self.fail_write.get() {
                return Err(err);
            }
            self.inner.write_vmo(vmo, vmo_offset, dev_offset, size).await
        }

        async fn trim_tail(&self, offset: u64) -> Result<(), zx::Status> {
            if let Some(err) = self.fail_trim.get() {
                return Err(err);
            }
            self.inner.trim_tail(offset).await
        }

        async fn flush(&self) -> Result<(), zx::Status> {
            if let Some(err) = self.fail_flush.get() {
                return Err(err);
            }
            self.inner.flush().await
        }
    }

    /// [`BoardPartitions`] backed by [`FaultyPartition`] entries with injectable open and flush
    /// counters/errors.
    struct FaultyBoard {
        partitions: HashMap<String, FaultyPartition>,
        opens: Rc<Cell<usize>>,
        flushes: Rc<Cell<u32>>,
        fail_flush: Rc<Cell<Option<zx::Status>>>,
    }

    impl FaultyBoard {
        fn new() -> Self {
            Self {
                partitions: HashMap::new(),
                opens: Rc::new(Cell::new(0)),
                flushes: Rc::new(Cell::new(0)),
                fail_flush: Rc::new(Cell::new(None)),
            }
        }

        fn add_partition(&mut self, name: &str, fill: u8) -> FaultyPartition {
            let part = FaultyPartition::new(fill);
            self.partitions.insert(name.to_string(), part.clone());
            part
        }
    }

    #[async_trait(?Send)]
    impl BoardPartitions for FaultyBoard {
        async fn open_partition(
            &self,
            partition_name: &str,
            _block_dir: &fidl_next::ClientEnd<fio::Directory>,
        ) -> Result<Box<dyn PartitionClient>, zx::Status> {
            let part = self.partitions.get(partition_name).ok_or(zx::Status::NOT_SUPPORTED)?;
            self.opens.set(self.opens.get() + 1);
            Ok(Box::new(part.clone()))
        }

        async fn flush(&self) -> Result<(), zx::Status> {
            self.flushes.set(self.flushes.get() + 1);
            if let Some(err) = self.fail_flush.get() {
                return Err(err);
            }
            Ok(())
        }
    }

    /// Creates a VMO containing `data` with its `ZX_PROP_VMO_CONTENT_SIZE` set to `data.len()`.
    fn make_vmo(data: &[u8]) -> zx::Vmo {
        let vmo = zx::Vmo::create(data.len().max(64) as u64).expect("create vmo");
        vmo.write(data, 0).expect("write vmo");
        vmo.set_content_size(&(data.len() as u64)).expect("set content size");
        vmo
    }

    /// Verifies `PartitionIo` size queries, unaligned reads/writes, `TrailingBehavior`, and bounds
    /// checks.
    #[fuchsia::test]
    async fn test_read_and_write_partition() {
        let board_partitions = FakeBoardPartitions::new();
        let raw = board_partitions.add_partition("raw_part", 0xaa);
        let _large = board_partitions.add_partition_with_size("large_part", 200 * 1024, 0xcc);
        board_partitions.add_missing_partition("missing_part");
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board_partitions), block_dir_client);

        let total_size = TEST_PARTITION_SIZE as u64;
        assert_eq!(pio.get_partition_size("raw_part").await, Ok(total_size));
        assert_eq!(pio.get_partition_size("missing_part").await, Err(zx::Status::NOT_FOUND));
        assert_eq!(pio.get_partition_size("unknown_part").await, Err(zx::Status::NOT_SUPPORTED));

        // Write 600 bytes at unaligned offset 100 with TrailingBehavior::Retain.
        let payload: Vec<u8> = (0..600).map(|i| ((i * 7 + 3) % 251) as u8).collect();
        pio.write_partition("raw_part", 100, make_vmo(&payload), fpaver::TrailingBehavior::Retain)
            .await
            .expect("write_partition at unaligned offset 100");

        let disk = raw.bytes();
        assert_eq!(&disk[..100], &[0xaau8; 100]);
        assert_eq!(&disk[100..700], &payload[..]);
        assert_eq!(&disk[700..], &[0xaau8; TEST_PARTITION_SIZE - 700]);

        // Read back exact 600 bytes at offset 100.
        let read_exact =
            pio.read_partition("raw_part", 100, 600).await.expect("read_partition 600 bytes");
        assert_eq!(read_exact.get_content_size().expect("content size"), 600);
        let mut read_vec = vec![0u8; 600];
        read_exact.read(&mut read_vec, 0).expect("read vmo");
        assert_eq!(read_vec, payload);

        // size == 0 is rejected with INVALID_ARGS.
        assert_eq!(
            pio.read_partition("raw_part", 100, 0).await.err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // Write 600 bytes at offset 0 with TrailingBehavior::Discard: preserves [600..1024) in the
        // trailing partial block and trims [1024..total_size) (filled with 0xab by
        // FakePartitionClient).
        pio.write_partition("raw_part", 0, make_vmo(&payload), fpaver::TrailingBehavior::Discard)
            .await
            .expect("write_partition with discard 600 bytes");
        let disk_after_discard = raw.bytes();
        assert_eq!(&disk_after_discard[..600], &payload[..]);
        assert_eq!(&disk_after_discard[600..700], &payload[500..600]);
        assert_eq!(&disk_after_discard[700..1024], &[0xaau8; 324]);
        assert_eq!(&disk_after_discard[1024..], &[0xabu8; TEST_PARTITION_SIZE - 1024]);

        // Zero-length payload with Retain is a no-op; with Discard it trims from the next block
        // boundary.
        pio.write_partition("raw_part", 0, make_vmo(&[]), fpaver::TrailingBehavior::Retain)
            .await
            .expect("write_partition empty retain");
        pio.write_partition("raw_part", 0, make_vmo(&[]), fpaver::TrailingBehavior::Discard)
            .await
            .expect("write_partition empty discard");
        assert_eq!(&raw.bytes()[..], &[0xabu8; TEST_PARTITION_SIZE]);

        // Out-of-range and u64::MAX overflow checks for read_partition and write_partition.
        assert_eq!(
            pio.read_partition("raw_part", total_size, 1).await.err(),
            Some(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.read_partition("raw_part", u64::MAX, 1).await.err(),
            Some(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.write_partition(
                "raw_part",
                total_size - 100,
                make_vmo(&payload),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.write_partition(
                "raw_part",
                u64::MAX,
                make_vmo(&payload),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );

        // Large (140 KiB) write_partition and read_partition.
        let large_payload: Vec<u8> = (0..140_000).map(|i| ((i * 11 + 5) % 251) as u8).collect();
        pio.write_partition(
            "large_part",
            300,
            make_vmo(&large_payload),
            fpaver::TrailingBehavior::Discard,
        )
        .await
        .expect("large write_partition");
        let read_large = pio
            .read_partition("large_part", 300, large_payload.len() as u64)
            .await
            .expect("large read_partition");
        let mut read_large_vec = vec![0u8; large_payload.len()];
        read_large.read(&mut read_large_vec, 0).expect("read large vmo");
        assert_eq!(read_large_vec, large_payload);
    }

    /// Verifies `fuchsia.paver.PartitionIo` FIDL dispatch over [`serve_partition_io`].
    #[fuchsia::test]
    async fn test_serve_partition_io_fidl() {
        let board_partitions = FakeBoardPartitions::new();
        let board_opens = board_partitions.opens.clone();
        let board_flushes = board_partitions.flushes.clone();
        let vbmeta_a = board_partitions.add_partition("vbmeta_a", 0xbb);
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board_partitions), block_dir_client);

        let scope = fasync::Scope::new_with_name("test_partition_io");
        let (pio_client, pio_server) = fidl_next::fuchsia::create_channel::<fpaver::PartitionIo>();
        let pio_client = pio_client.spawn();
        scope.spawn_local(async move {
            serve_partition_io(pio, pio_server).await.expect("serve_partition_io");
        });

        let total_size = TEST_PARTITION_SIZE as u64;
        assert_eq!(
            pio_client.get_partition_size("vbmeta_a").await.expect("fidl").map(|r| r.size),
            Ok(total_size)
        );
        assert_eq!(board_opens.get(), 1);
        assert_eq!(
            pio_client.get_partition_size("unsupported_part").await.expect("fidl").map(|r| r.size),
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(board_opens.get(), 2);

        // Subsequent write_partition on "vbmeta_a" reuses the cached PartitionClient without
        // calling BoardPartitions::open_partition again.
        let payload: Vec<u8> = (0..600).map(|i| ((i * 5 + 9) % 251) as u8).collect();
        assert_eq!(
            pio_client
                .write_partition(
                    "vbmeta_a",
                    200,
                    make_vmo(&payload),
                    fpaver::TrailingBehavior::Retain,
                )
                .await
                .expect("write_partition fidl"),
            Ok(())
        );
        assert_eq!(board_opens.get(), 2);

        // flush() flushes written clients, delegates to BoardPartitions::flush(), and keeps
        // healthy cached handles open for subsequent operations.
        assert_eq!(vbmeta_a.flushes(), 0);
        assert_eq!(pio_client.flush().await.expect("flush fidl"), Ok(()));
        assert_eq!(vbmeta_a.flushes(), 1);
        assert_eq!(board_flushes.get(), 1);

        let read_vmo = pio_client
            .read_partition("vbmeta_a", 200, 600)
            .await
            .expect("read_partition fidl")
            .expect("read_partition ok")
            .payload;
        assert_eq!(board_opens.get(), 2);
        let mut read_bytes = vec![0u8; 600];
        read_vmo.read(&mut read_bytes, 0).expect("read vmo");
        assert_eq!(read_bytes, payload);

        let disk = vbmeta_a.bytes();
        assert_eq!(&disk[..200], &[0xbbu8; 200]);
        assert_eq!(&disk[200..800], &payload[..]);
        assert_eq!(&disk[800..1024], &[0xbbu8; 224]);

        drop(pio_client);
        scope.cancel().await;
    }

    /// Verifies that `PartitionIo` evicts cached `PartitionClient` instances on device/session
    /// I/O errors (`get_partition_size`, `read_partition`, `write_partition`, and
    /// non-`NOT_SUPPORTED` `trim_tail` failures), while retaining cached clients across caller
    /// parameter validation errors (`INVALID_ARGS` and `OUT_OF_RANGE`).
    #[fuchsia::test]
    async fn test_partition_io_cache_eviction_on_io_error() {
        let mut board = FaultyBoard::new();
        let opens = board.opens.clone();
        let zircon_a = board.add_partition("zircon_a", 0xaa);
        let (block_dir, _server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board), block_dir);

        // 0. Invalid TrailingBehavior is rejected before opening/caching the partition.
        assert_eq!(
            pio.write_partition(
                "zircon_a",
                0,
                make_vmo(&[1, 2, 3]),
                fpaver::TrailingBehavior::UnknownOrdinal_(99),
            )
            .await,
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(opens.get(), 0);

        // 1. Initial get_partition_size caches client (opens == 1).
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 1);

        // 2. Caller validation errors (size == 0 -> INVALID_ARGS, out-of-bounds -> OUT_OF_RANGE)
        // do NOT evict the cached client.
        assert_eq!(
            pio.read_partition("zircon_a", 0, 0).await.err(),
            Some(zx::Status::INVALID_ARGS)
        );
        assert_eq!(
            pio.read_partition("zircon_a", TEST_PARTITION_SIZE as u64, 1).await.err(),
            Some(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            pio.write_partition(
                "zircon_a",
                TEST_PARTITION_SIZE as u64,
                make_vmo(&[1, 2, 3]),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 1);

        // 3. get_partition_size error evicts cached client -> next call re-opens (opens == 2).
        zircon_a.fail_size.set(Some(zx::Status::IO));
        assert_eq!(pio.get_partition_size("zircon_a").await, Err(zx::Status::IO));
        zircon_a.fail_size.set(None);
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 2);

        // 4. read_partition I/O error evicts cached client -> next call re-opens (opens == 3).
        zircon_a.fail_read.set(Some(zx::Status::IO));
        assert_eq!(pio.read_partition("zircon_a", 0, 512).await.err(), Some(zx::Status::IO));
        zircon_a.fail_read.set(None);
        pio.read_partition("zircon_a", 0, 512).await.expect("read after reopen");
        assert_eq!(opens.get(), 3);

        // 5. write_partition I/O error evicts cached client -> next call re-opens (opens == 4).
        zircon_a.fail_write.set(Some(zx::Status::IO));
        assert_eq!(
            pio.write_partition(
                "zircon_a",
                0,
                make_vmo(&[0x55; 512]),
                fpaver::TrailingBehavior::Retain,
            )
            .await,
            Err(zx::Status::IO)
        );
        zircon_a.fail_write.set(None);
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x55; 512]),
            fpaver::TrailingBehavior::Retain,
        )
        .await
        .expect("write after reopen");
        assert_eq!(opens.get(), 4);

        // 6. TrailingBehavior::Discard with trim_tail(NOT_SUPPORTED) succeeds without evicting,
        // whereas trim_tail(IO) succeeds the write while evicting the cached client (opens == 5).
        zircon_a.fail_trim.set(Some(zx::Status::NOT_SUPPORTED));
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x66; 512]),
            fpaver::TrailingBehavior::Discard,
        )
        .await
        .expect("discard with NOT_SUPPORTED trim");
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 4);

        zircon_a.fail_trim.set(Some(zx::Status::IO));
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x66; 512]),
            fpaver::TrailingBehavior::Discard,
        )
        .await
        .expect("discard with IO trim error still succeeds write");
        zircon_a.fail_trim.set(None);
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 5);
    }

    /// Verifies dirty-partition tracking across `PartitionIo::flush()`: read-only cached handles
    /// are retained without flushing, evicted dirty partitions are re-opened and flushed,
    /// multi-partition flush errors evict and retain failed partitions for retry while continuing
    /// to flush remaining partitions, and `BoardPartitions::flush()` errors propagate.
    #[fuchsia::test]
    async fn test_partition_io_flush_dirty_tracking_and_errors() {
        let mut board = FaultyBoard::new();
        let opens = board.opens.clone();
        let board_flushes = board.flushes.clone();
        let fail_board_flush = board.fail_flush.clone();
        let zircon_a = board.add_partition("zircon_a", 0xaa);
        let zircon_b = board.add_partition("zircon_b", 0xbb);
        let (block_dir, _server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board), block_dir);

        // 1. Read-only / size-queried cached partitions are retained across flush() without
        // calling client.flush().
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        pio.read_partition("zircon_a", 0, 512).await.expect("read zircon_a");
        assert_eq!(opens.get(), 1);
        assert_eq!(pio.flush().await, Ok(()));
        assert_eq!(zircon_a.flushes(), 0);
        assert_eq!(board_flushes.get(), 1);
        // Handle remains cached across flush(), so the next operation does not re-open.
        assert_eq!(pio.get_partition_size("zircon_a").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 1);

        // 2. Evicting a dirty partition (e.g. non-NOT_SUPPORTED trim_tail error after write_vmo
        // succeeded) still re-opens and flushes it on flush().
        zircon_a.fail_trim.set(Some(zx::Status::IO));
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x11; 512]),
            fpaver::TrailingBehavior::Discard,
        )
        .await
        .expect("write with trim_tail IO error");
        zircon_a.fail_trim.set(None);
        assert_eq!(opens.get(), 1);
        assert_eq!(pio.flush().await, Ok(()));
        assert_eq!(opens.get(), 2);
        assert_eq!(zircon_a.flushes(), 1);
        assert_eq!(board_flushes.get(), 2);

        // 3. Multi-partition flush where one client fails evicts the failed client, still flushes
        // other dirty clients and board_partitions, returns the error, and retries only the failed
        // partition on the next flush().
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x22; 512]),
            fpaver::TrailingBehavior::Retain,
        )
        .await
        .expect("write zircon_a");
        pio.write_partition(
            "zircon_b",
            0,
            make_vmo(&[0x33; 512]),
            fpaver::TrailingBehavior::Retain,
        )
        .await
        .expect("write zircon_b");
        assert_eq!(opens.get(), 3);

        zircon_a.fail_flush.set(Some(zx::Status::IO));
        assert_eq!(pio.flush().await, Err(zx::Status::IO));
        assert_eq!(zircon_a.flushes(), 1);
        assert_eq!(zircon_b.flushes(), 1);
        assert_eq!(board_flushes.get(), 3);

        // Retry after clearing zircon_a's flush error: re-opens and flushes zircon_a only
        // (zircon_b stayed cached and clean).
        zircon_a.fail_flush.set(None);
        assert_eq!(pio.flush().await, Ok(()));
        assert_eq!(opens.get(), 4);
        assert_eq!(zircon_a.flushes(), 2);
        assert_eq!(zircon_b.flushes(), 1);
        assert_eq!(board_flushes.get(), 4);

        // Subsequent flush with no new writes does not re-flush either client, and both remain
        // cached.
        assert_eq!(pio.flush().await, Ok(()));
        assert_eq!(zircon_a.flushes(), 2);
        assert_eq!(zircon_b.flushes(), 1);
        assert_eq!(board_flushes.get(), 5);
        assert_eq!(pio.get_partition_size("zircon_b").await, Ok(TEST_PARTITION_SIZE as u64));
        assert_eq!(opens.get(), 4);

        // 4. BoardPartitions::flush() error propagates when client flushes succeed.
        pio.write_partition(
            "zircon_a",
            0,
            make_vmo(&[0x44; 512]),
            fpaver::TrailingBehavior::Retain,
        )
        .await
        .expect("write zircon_a before board flush error");
        assert_eq!(opens.get(), 4);
        fail_board_flush.set(Some(zx::Status::IO));
        assert_eq!(pio.flush().await, Err(zx::Status::IO));
        assert_eq!(zircon_a.flushes(), 3);
        assert_eq!(board_flushes.get(), 6);

        fail_board_flush.set(None);
        assert_eq!(pio.flush().await, Ok(()));
        assert_eq!(zircon_a.flushes(), 3);
        assert_eq!(board_flushes.get(), 7);
    }

    /// Verifies that closing a `fuchsia.paver.PartitionIo` channel with unflushed writes causes
    /// [`serve_partition_io`] to return an error rather than silently auto-flushing.
    #[fuchsia::test]
    async fn test_serve_partition_io_fidl_unflushed_close_returns_error() {
        let board_partitions = FakeBoardPartitions::new();
        let board_flushes = board_partitions.flushes.clone();
        let vbmeta_a = board_partitions.add_partition("vbmeta_a", 0xbb);
        let (block_dir_client, _block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        let pio = PartitionIo::new(Box::new(board_partitions), block_dir_client);

        let scope = fasync::Scope::new_with_name("test_partition_io_unflushed");
        let (pio_client, pio_server) = fidl_next::fuchsia::create_channel::<fpaver::PartitionIo>();
        let pio_client = pio_client.spawn();
        let server_task = scope.compute_local(serve_partition_io(pio, pio_server));

        assert_eq!(
            pio_client
                .write_partition(
                    "vbmeta_a",
                    0,
                    make_vmo(&[0x42; 512]),
                    fpaver::TrailingBehavior::Retain,
                )
                .await
                .expect("write_partition fidl"),
            Ok(())
        );
        drop(pio_client);

        let err = server_task.await.expect_err("closing without flush must error");
        assert!(err.to_string().contains("vbmeta_a"), "unexpected error: {err:#}");
        assert_eq!(vbmeta_a.flushes(), 0);
        assert_eq!(board_flushes.get(), 0);
    }
}
