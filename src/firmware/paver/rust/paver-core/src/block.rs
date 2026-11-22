// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers for opening named block partitions from a capability directory and mapping logical paver
//! partition names onto physical block partitions.

use crate::partition::{BlockPartitionClient, BoardPartitions, PartitionClient};
use async_trait::async_trait;
use block_client::RemoteBlockClient;
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_storage_block as fblock;
use fidl_next_fuchsia_io as fio;
use futures::stream::{FuturesUnordered, StreamExt as _};
use std::num::NonZero;

/// Maximum time to wait when probing a named partition entry in a block directory.
///
/// `fshost` pre-creates a `StagedPublisher` (`vfs::remote::remote_dir`) for each configured
/// `block_devices` rule and leaves the server end unserved until a matching partition binds; if no
/// partition on storage satisfies that rule, the channel stays unserved indefinitely, so probes
/// need a finite timeout to return `zx::Status::NOT_FOUND`.
const STAGED_ENTRY_PROBE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

/// Component namespace path for the `BootManager` block subdirectory.
const BOOT_MANAGER_BLOCK_DIR_PATH: &str = "/block/bootmanager";

/// Default bounce-buffer and transfer-VMO capacity (64 KiB) used when opening small A/B/R metadata
/// partitions via [`boot_manager_open_partition`] and [`boot_manager_open_any_partition`].
const BOOT_MANAGER_BOUNCE_VMO_BYTES: NonZero<usize> = NonZero::new(64 * 1024).unwrap();

/// Maps logical paver partition names (such as `"zircon_a"`, `"vbmeta_a"`, or `"bootloader_a"`)
/// to a [`PartitionClient`] by opening one or more underlying physical block partitions via
/// `physical_part_opener`.
#[allow(async_fn_in_trait)]
pub trait NamedPhysicalPartitionMapper {
    /// Opens a [`PartitionClient`] for `partition_name` by resolving its backing physical
    /// partition(s) through `physical_part_opener`.
    ///
    /// Returns `Box<dyn PartitionClient>` because a single board mapper may return plain `P` for
    /// standard partitions and composite wrappers (such as
    /// [`FixedOffsetPartitionClient`](crate::partition::FixedOffsetPartitionClient) or
    /// [`MultiPartitionClient`](crate::partition::MultiPartitionClient)) for firmware partitions.
    ///
    /// # Errors
    ///
    /// - Returns `Err(zx::Status::NOT_SUPPORTED)` if `partition_name` is not recognized by this
    ///   board.
    /// - Returns `Err(zx::Status::NOT_FOUND)` if `partition_name` is supported by this board, but
    ///   a required backing physical partition (or every fallback candidate) cannot be found on
    ///   storage.
    /// - Propagates any other error returned by `physical_part_opener`.
    async fn open_partition<P: PartitionClient + 'static>(
        &self,
        partition_name: &str,
        physical_part_opener: impl AsyncFn(&str) -> Result<P, zx::Status>,
    ) -> Result<Box<dyn PartitionClient>, zx::Status>;
}

/// Concurrently opens candidate physical partition names in `candidates` via
/// `physical_part_opener`, returning the first opened `P` and cancelling any remaining in-flight
/// probes.
///
/// Candidates must represent mutually-exclusive physical partition names for the same logical
/// partition (for example, modern `"zircon_a"` vs. legacy `"zircon-a"` during a partition-table
/// migration). Probing candidates concurrently ensures that an unserved `fshost` `StagedPublisher`
/// entry for one spelling does not stall opening a live sibling alias by
/// [`STAGED_ENTRY_PROBE_TIMEOUT`], and that completely absent multi-candidate lookups time out in
/// a single [`STAGED_ENTRY_PROBE_TIMEOUT`] window.
///
/// # Errors
///
/// - Returns `Err(zx::Status::NOT_SUPPORTED)` if `candidates` is empty.
/// - Returns `Err(zx::Status::NOT_FOUND)` if every candidate fails with `zx::Status::NOT_FOUND`.
/// - Propagates any non-`NOT_FOUND` error immediately, cancelling remaining candidate probes.
pub async fn open_any_matching_partition<P: PartitionClient>(
    candidates: &[&str],
    physical_part_opener: impl AsyncFn(&str) -> Result<P, zx::Status>,
) -> Result<P, zx::Status> {
    match candidates {
        [] => Err(zx::Status::NOT_SUPPORTED),
        [single] => physical_part_opener(single).await,
        candidates => {
            let mut probes: FuturesUnordered<_> =
                candidates.iter().map(|&candidate| physical_part_opener(candidate)).collect();
            while let Some(res) = probes.next().await {
                match res {
                    Ok(client) => return Ok(client),
                    Err(zx::Status::NOT_FOUND) => {}
                    Err(err) => return Err(err),
                }
            }
            Err(zx::Status::NOT_FOUND)
        }
    }
}

/// [`BoardPartitions`] adapter for boards whose block partitions are resolved by name via
/// [`NamedPhysicalPartitionMapper`] and [`open_partition_in_block_dir`].
#[derive(Debug)]
pub struct NamedBlockBoardPartitions<M> {
    mapper: M,
    bounce_buffer_bytes: NonZero<usize>,
    transfer_vmo_pool_bytes: NonZero<usize>,
}

impl<M> NamedBlockBoardPartitions<M> {
    /// Creates a [`NamedBlockBoardPartitions`] wrapping `mapper` and configuring opened
    /// [`BlockPartitionClient`] instances with `bounce_buffer_bytes` and `transfer_vmo_pool_bytes`
    /// (see [`BlockPartitionClient::new`]).
    ///
    /// # Errors
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `transfer_vmo_pool_bytes < bounce_buffer_bytes`.
    pub fn new(
        mapper: M,
        bounce_buffer_bytes: NonZero<usize>,
        transfer_vmo_pool_bytes: NonZero<usize>,
    ) -> Result<Self, zx::Status> {
        if transfer_vmo_pool_bytes < bounce_buffer_bytes {
            return Err(zx::Status::INVALID_ARGS);
        }
        Ok(Self { mapper, bounce_buffer_bytes, transfer_vmo_pool_bytes })
    }
}

#[async_trait(?Send)]
impl<M> BoardPartitions for NamedBlockBoardPartitions<M>
where
    M: NamedPhysicalPartitionMapper,
{
    async fn open_partition(
        &self,
        partition_name: &str,
        block_dir: &fidl_next::ClientEnd<fio::Directory>,
    ) -> Result<Box<dyn PartitionClient>, zx::Status> {
        let bounce_buffer_bytes = self.bounce_buffer_bytes;
        let transfer_vmo_pool_bytes = self.transfer_vmo_pool_bytes;
        self.mapper
            .open_partition(partition_name, async |name: &str| {
                open_partition_in_block_dir(
                    block_dir,
                    name,
                    bounce_buffer_bytes,
                    transfer_vmo_pool_bytes,
                )
                .await
            })
            .await
    }
}

/// Returns `true` if `name` is a valid single relative directory entry segment.
fn is_valid_partition_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// Returns `true` if `channel` already has `ZX_CHANNEL_PEER_CLOSED` asserted.
fn is_channel_peer_closed(channel: &zx::Channel) -> bool {
    channel
        .wait_one(zx::Signals::CHANNEL_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
        .to_result()
        .is_ok()
}

/// Probes `block_proxy` with `GetInfo` within [`STAGED_ENTRY_PROBE_TIMEOUT`] to check that the
/// directory entry exists and is served by a block device.
async fn probe_block_entry(block_proxy: &fblock::BlockProxy) -> Result<(), zx::Status> {
    let probe_fut = async {
        match block_proxy.get_info().await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(raw)) => Err(zx::Status::err_from_raw(raw)),
            Err(_) => Err(zx::Status::NOT_FOUND),
        }
    };
    fuchsia_async::TimeoutExt::on_timeout(
        probe_fut,
        fuchsia_async::MonotonicInstant::after(STAGED_ENTRY_PROBE_TIMEOUT),
        || Err(zx::Status::NOT_FOUND),
    )
    .await
}

/// Opens a [`BlockPartitionClient<RemoteBlockClient>`] from an already-probed `block_proxy`.
async fn open_block_partition_client(
    block_proxy: fblock::BlockProxy,
    bounce_buffer_bytes: NonZero<usize>,
    transfer_vmo_pool_bytes: NonZero<usize>,
) -> Result<BlockPartitionClient<RemoteBlockClient>, zx::Status> {
    let client = RemoteBlockClient::new(block_proxy).await?;
    BlockPartitionClient::new(client, bounce_buffer_bytes, transfer_vmo_pool_bytes).await
}

/// Opens a [`BlockPartitionClient<RemoteBlockClient>`] from the direct entry
/// `<block_dir>/<partition_name>`, configuring the client with `bounce_buffer_bytes` and
/// `transfer_vmo_pool_bytes` (see [`BlockPartitionClient::new`]).
///
/// Callers supply the directory whose direct entries are named block partitions (for example, the
/// `/block/ota` directory passed to [`PartitionIo`](crate::partition_io::PartitionIo)). Discovery
/// connects directly to `<partition_name>/fuchsia.storage.block.Block` without enumerating sibling
/// entries or traversing subdirectories, probing `Block.GetInfo` within
/// [`STAGED_ENTRY_PROBE_TIMEOUT`] to confirm the entry is served without requiring its underlying
/// partition label to match `partition_name`.
///
/// # Errors
///
/// - Returns `Err(zx::Status::INVALID_ARGS)` if `transfer_vmo_pool_bytes < bounce_buffer_bytes`.
/// - Returns `Err(zx::Status::NOT_FOUND)` if `partition_name` is not a single relative path segment
///   or no served `<partition_name>` entry responds within [`STAGED_ENTRY_PROBE_TIMEOUT`].
/// - Returns `Err(zx::Status::IO)` (or the underlying [`zx::Status`]) if `block_dir` is closed or
///   probing/opening a session on the block device fails.
pub async fn open_partition_in_block_dir(
    block_dir: &fidl_next::ClientEnd<fio::Directory>,
    partition_name: &str,
    bounce_buffer_bytes: NonZero<usize>,
    transfer_vmo_pool_bytes: NonZero<usize>,
) -> Result<BlockPartitionClient<RemoteBlockClient>, zx::Status> {
    // Fail fast before opening block protocol connections if buffer sizes are invalid.
    if transfer_vmo_pool_bytes < bounce_buffer_bytes {
        return Err(zx::Status::INVALID_ARGS);
    }
    if !is_valid_partition_segment(partition_name) {
        return Err(zx::Status::NOT_FOUND);
    }
    let raw_block_dir = block_dir.as_ref().into_untyped();
    // Best-effort check to tell "directory gone" (`IO`) from "entry missing" (`NOT_FOUND`);
    // concurrent channel closure can still race with the probe below.
    if is_channel_peer_closed(raw_block_dir) {
        return Err(zx::Status::IO);
    }
    let (block_proxy, server_end) = fidl::endpoints::create_proxy::<fblock::BlockMarker>();
    let protocol_path = format!("{partition_name}/{}", fblock::BlockMarker::PROTOCOL_NAME);
    fdio::service_connect_at(raw_block_dir, &protocol_path, server_end.into_channel())?;
    if let Err(err) = probe_block_entry(&block_proxy).await {
        if err == zx::Status::NOT_FOUND {
            if is_channel_peer_closed(raw_block_dir) {
                return Err(zx::Status::IO);
            }
            log::debug!(partition_name; "Partition not found in block_dir");
        }
        return Err(err);
    }
    log::info!(partition_name; "Found partition in block directory");
    open_block_partition_client(block_proxy, bounce_buffer_bytes, transfer_vmo_pool_bytes).await
}

/// Opens the component namespace's [`BOOT_MANAGER_BLOCK_DIR_PATH`] (`/block/bootmanager`)
/// directory.
///
/// # Errors
///
/// Returns the [`zx::Status`] from `fdio::open` (logged at `error`), for example
/// `Err(zx::Status::NOT_FOUND)` if `/block/bootmanager` is not routed to this component.
fn open_boot_manager_block_dir() -> Result<fidl_next::ClientEnd<fio::Directory>, zx::Status> {
    let (block_dir, server_end) = fidl_next::fuchsia::create_channel::<fio::Directory>();
    fdio::open(
        BOOT_MANAGER_BLOCK_DIR_PATH,
        fidl_fuchsia_io::Flags::PROTOCOL_DIRECTORY | fidl_fuchsia_io::PERM_READABLE,
        server_end.into_untyped(),
    )
    .inspect_err(|status| {
        log::error!(
            status:?;
            "Failed to open {BOOT_MANAGER_BLOCK_DIR_PATH}; check that it is routed to this component"
        );
    })?;
    Ok(block_dir)
}

/// Concurrently opens candidate partitions in `candidates` from the direct entries of
/// `block_dir` via [`open_partition_in_block_dir`], returning the first opened partition and
/// cancelling any remaining in-flight probes.
async fn boot_manager_open_any_partition_in(
    block_dir: &fidl_next::ClientEnd<fio::Directory>,
    candidates: &[&str],
) -> Result<BlockPartitionClient<RemoteBlockClient>, zx::Status> {
    let res = open_any_matching_partition(candidates, async |candidate| {
        open_partition_in_block_dir(
            block_dir,
            candidate,
            BOOT_MANAGER_BOUNCE_VMO_BYTES,
            BOOT_MANAGER_BOUNCE_VMO_BYTES,
        )
        .await
    })
    .await;
    if matches!(res, Err(zx::Status::NOT_FOUND)) {
        log::debug!(candidates:?; "Partition not found under {BOOT_MANAGER_BLOCK_DIR_PATH}");
    }
    res
}

/// Opens a [`BlockPartitionClient<RemoteBlockClient>`] at `/block/bootmanager/<partition_name>` in
/// the component namespace for [`BootManager`](crate::boot_manager::BootManager).
///
/// # Capability Routing Contract
///
/// To expose an A/B/R metadata partition to [`BootManager`](crate::boot_manager::BootManager), the
/// board configuration publishes it under `/block/bootmanager/<partition_name>` and the component
/// manifest routes that directory at `/block/bootmanager` (for example, using
/// `subdir: "bootmanager"` from `block`, or via a mounted `/block` directory during transition).
///
/// Only [`BootManager`](crate::boot_manager::BootManager) implementations should call this helper;
/// [`PartitionIo`](crate::partition_io::PartitionIo) and [`DataSink`](crate::data_sink::DataSink)
/// open block partitions from an explicit `block_dir` via [`open_partition_in_block_dir`].
///
/// # Errors
///
/// - Returns the [`zx::Status`] from `fdio::open` (logged at `error`) if `/block/bootmanager`
///   cannot be opened, for example because it is not routed to this component.
/// - Returns `Err(zx::Status::NOT_FOUND)` if `partition_name` is not a single relative path segment
///   or no served entry at `/block/bootmanager/<partition_name>` responds within
///   [`STAGED_ENTRY_PROBE_TIMEOUT`].
/// - Returns `Err(zx::Status::IO)` (or the underlying [`zx::Status`]) if the `/block/bootmanager`
///   directory is closed or probing/opening a block session on the partition fails.
pub async fn boot_manager_open_partition(
    partition_name: &str,
) -> Result<BlockPartitionClient<RemoteBlockClient>, zx::Status> {
    boot_manager_open_any_partition(&[partition_name]).await
}

/// Concurrently opens candidate partitions in `candidates` at `/block/bootmanager/<candidate>` for
/// [`BootManager`](crate::boot_manager::BootManager), returning the first opened partition and
/// cancelling any remaining in-flight probes.
///
/// Candidates must represent mutually-exclusive partition aliases for the same logical A/B/R
/// metadata partition (for example, `"durable_boot"` vs. legacy `"misc"`). Probing concurrently
/// avoids stalling on an unserved `fshost` `StagedPublisher` entry when a sibling alias is present,
/// but means there is no priority order between candidates: whichever served candidate responds
/// first is returned. Board configurations uphold mutual exclusivity by publishing exactly one
/// A/B/R metadata entry under `/block/bootmanager`, so at most one candidate is ever served. See
/// [`boot_manager_open_partition`] for the capability routing contract.
///
/// # Errors
///
/// - Returns `Err(zx::Status::NOT_SUPPORTED)` if `candidates` is empty.
/// - Returns the [`zx::Status`] from `fdio::open` (logged at `error`) if `/block/bootmanager`
///   cannot be opened, for example because it is not routed to this component.
/// - Returns `Err(zx::Status::NOT_FOUND)` if no valid candidate is served under
///   `/block/bootmanager` within [`STAGED_ENTRY_PROBE_TIMEOUT`].
/// - Propagates any non-`NOT_FOUND` error immediately, cancelling remaining candidate probes.
pub async fn boot_manager_open_any_partition(
    candidates: &[&str],
) -> Result<BlockPartitionClient<RemoteBlockClient>, zx::Status> {
    if candidates.is_empty() {
        return Err(zx::Status::NOT_SUPPORTED);
    }
    let block_dir = open_boot_manager_block_dir()?;
    boot_manager_open_any_partition_in(&block_dir, candidates).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::tests::FakePartitionClient;
    use fuchsia_async as fasync;
    use fuchsia_component::server::ServiceFs;
    use std::cell::{Cell, RefCell};
    use std::task::Poll;

    const TEST_BOUNCE_BYTES: NonZero<usize> = NonZero::new(64 * 1024).unwrap();

    #[derive(Clone, Copy)]
    enum MockBlockBehavior {
        Ready { block_count: u64, block_size: u32 },
        FailGetInfo(zx::Status),
    }

    /// RAII guard that unbinds a test path from the process `fdio::Namespace` when dropped.
    struct NamespaceBindGuard(&'static str);

    impl NamespaceBindGuard {
        fn bind(
            path: &'static str,
            dir: fidl::endpoints::ClientEnd<fidl_fuchsia_io::DirectoryMarker>,
        ) -> Self {
            let ns = fdio::Namespace::installed().expect("installed namespace");
            let _ = ns.unbind(path);
            ns.bind(path, dir).expect("bind test namespace path");
            Self(path)
        }
    }

    impl Drop for NamespaceBindGuard {
        fn drop(&mut self) {
            if let Ok(ns) = fdio::Namespace::installed() {
                let _ = ns.unbind(self.0);
            }
        }
    }

    async fn serve_mock_session(mut stream: fblock::SessionRequestStream) {
        let (server_fifo, _client_fifo) =
            zx::Fifo::<block_client::BlockFifoResponse, block_client::BlockFifoRequest>::create(16)
                .expect("create mock block fifo");
        let mut maybe_fifo = Some(server_fifo);
        let mut next_vmo_id = 1u16;
        while let Some(Ok(req)) = stream.next().await {
            match req {
                fblock::SessionRequest::GetFifo { responder } => match maybe_fifo.take() {
                    Some(fifo) => {
                        let _ = responder.send(Ok(fifo.downcast()));
                    }
                    None => {
                        let _ = responder.send(Err(zx::Status::NO_RESOURCES.into_raw()));
                    }
                },
                fblock::SessionRequest::AttachVmo { vmo: _, responder } => {
                    let id = next_vmo_id;
                    next_vmo_id = next_vmo_id.saturating_add(1);
                    let _ = responder.send(Ok(&fblock::VmoId { id }));
                }
                fblock::SessionRequest::RegisterKey { responder, .. } => {
                    let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                }
                fblock::SessionRequest::Close { responder } => {
                    let _ = responder.send(Ok(()));
                    break;
                }
            }
        }
    }

    /// Serves a mock `fuchsia.storage.block.Block` stream on its own spawned task so live
    /// `OpenSession` connections never block `ServiceFs` from accepting subsequent requests.
    fn spawn_mock_block(
        scope: &fasync::ScopeHandle,
        mut stream: fblock::BlockRequestStream,
        label: Option<&'static str>,
        behavior: MockBlockBehavior,
    ) {
        let session_scope = scope.clone();
        scope.spawn_local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fblock::BlockRequest::GetMetadata { responder } => match label {
                        Some(name) => {
                            let _ = responder.send(Ok(&fblock::PartitionInfo {
                                name: Some(name.to_string()),
                                ..Default::default()
                            }));
                        }
                        None => {
                            let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                        }
                    },
                    fblock::BlockRequest::GetName { responder } => match label {
                        Some(name) => {
                            let _ = responder.send(zx::sys::ZX_OK, Some(name));
                        }
                        None => {
                            let _ = responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None);
                        }
                    },
                    fblock::BlockRequest::GetInfo { responder } => match behavior {
                        MockBlockBehavior::Ready { block_count, block_size } => {
                            let _ = responder.send(Ok(&fblock::BlockInfo {
                                block_count,
                                block_size,
                                max_transfer_size: 1024 * 1024,
                                flags: fblock::DeviceFlag::empty(),
                            }));
                        }
                        MockBlockBehavior::FailGetInfo(status) => {
                            let _ = responder.send(Err(status.into_raw()));
                        }
                    },
                    fblock::BlockRequest::OpenSession { session, .. } => {
                        session_scope.spawn_local(serve_mock_session(session.into_stream()));
                    }
                    _ => {}
                }
            }
        });
    }

    /// Verifies [`open_partition_in_block_dir`] direct `<partition_name>` lookup (including entries
    /// whose underlying physical label differs or is unsupported), sibling `StagedPublisher`
    /// isolation, invalid path segment rejection, and propagation of `GetInfo` errors.
    #[fuchsia::test]
    async fn test_open_partition_in_block_dir_scoping_and_discovery() {
        enum MockBlock {
            DirectVbmetaA(fblock::BlockRequestStream),
            AliasedZirconA(fblock::BlockRequestStream),
            GetInfoErr(fblock::BlockRequestStream),
            UnrelatedStagedFts(fblock::BlockRequestStream),
            NestedSubdirEntry(fblock::BlockRequestStream),
        }

        let mut fs = ServiceFs::new_local();
        fs.dir("vbmeta_a").add_fidl_service(MockBlock::DirectVbmetaA);
        fs.dir("zircon_a").add_fidl_service(MockBlock::AliasedZirconA);
        fs.dir("broken_part").add_fidl_service(MockBlock::GetInfoErr);
        fs.dir("fts").add_fidl_service(MockBlock::UnrelatedStagedFts);
        fs.dir("subdir").dir("nested_only").add_fidl_service(MockBlock::NestedSubdirEntry);

        let (block_dir_client, block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        fs.serve_connection(fidl::endpoints::ServerEnd::new(block_dir_server.into_untyped()))
            .expect("serve mock block_dir");

        let scope = fasync::Scope::new_with_name("test_block_dir");
        let worker_scope = scope.to_handle();
        scope.spawn_local(async move {
            let mut stalled_streams = Vec::new();
            while let Some(service) = fs.next().await {
                match service {
                    MockBlock::DirectVbmetaA(stream) => {
                        // Simulates a non-GPT block device (`GetMetadata`/`GetName` unsupported).
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            None,
                            MockBlockBehavior::Ready { block_count: 16, block_size: 512 },
                        );
                    }
                    MockBlock::AliasedZirconA(stream) => {
                        // Simulates an entry published under a normalized name (`"zircon_a"`) whose
                        // underlying partition label (`"zircon-a"`) differs.
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            Some("zircon-a"),
                            MockBlockBehavior::Ready { block_count: 32, block_size: 512 },
                        );
                    }
                    MockBlock::GetInfoErr(stream) => {
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            Some("broken_part"),
                            MockBlockBehavior::FailGetInfo(zx::Status::IO_REFUSED),
                        );
                    }
                    MockBlock::UnrelatedStagedFts(stream) => {
                        // Simulates an unserved `StagedPublisher` for an unrelated partition;
                        // opening other partitions must never touch `"fts"`.
                        stalled_streams.push(stream);
                    }
                    MockBlock::NestedSubdirEntry(stream) => {
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            Some("nested_only"),
                            MockBlockBehavior::FailGetInfo(zx::Status::INTERNAL),
                        );
                    }
                }
            }
        });

        // 1. Direct `"vbmeta_a"` opens via `GetInfo` even when `GetMetadata`/`GetName` return
        // `NOT_SUPPORTED`, without touching unrelated `"fts"`.
        let vbmeta_client = open_partition_in_block_dir(
            &block_dir_client,
            "vbmeta_a",
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        )
        .await
        .expect("open vbmeta_a");
        assert_eq!(vbmeta_client.partition_size(), Ok(16 * 512));

        // 2. Entry published under an alias (`"zircon_a"` backed by physical `"zircon-a"`) opens.
        let zircon_client = open_partition_in_block_dir(
            &block_dir_client,
            "zircon_a",
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        )
        .await
        .expect("open aliased zircon_a");
        assert_eq!(zircon_client.partition_size(), Ok(32 * 512));

        // 3. Explicit driver `GetInfo` error (`IO_REFUSED`) propagates instead of masking as
        // `NOT_FOUND`.
        assert_eq!(
            open_partition_in_block_dir(
                &block_dir_client,
                "broken_part",
                TEST_BOUNCE_BYTES,
                TEST_BOUNCE_BYTES,
            )
            .await
            .err(),
            Some(zx::Status::IO_REFUSED)
        );

        // 4. Invalid buffer sizes (`transfer_vmo_pool_bytes < bounce_buffer_bytes`) fail fast with
        // `INVALID_ARGS` before probing entries.
        assert_eq!(
            open_partition_in_block_dir(
                &block_dir_client,
                "nonexistent_part",
                TEST_BOUNCE_BYTES,
                NonZero::new(4096).unwrap(),
            )
            .await
            .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // 5. Empty, `.`, `..`, multi-segment, and nonexistent names return `NOT_FOUND`.
        for invalid_or_missing in
            ["", ".", "..", "subdir/nested_only", "nested_only", "nonexistent_part"]
        {
            assert_eq!(
                open_partition_in_block_dir(
                    &block_dir_client,
                    invalid_or_missing,
                    TEST_BOUNCE_BYTES,
                    TEST_BOUNCE_BYTES,
                )
                .await
                .err(),
                Some(zx::Status::NOT_FOUND),
                "unexpected result for {invalid_or_missing:?}"
            );
        }

        // 6. Closed `block_dir` channel fails immediately with `IO`.
        let (closed_dir, closed_server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        drop(closed_server);
        assert_eq!(
            open_partition_in_block_dir(
                &closed_dir,
                "zircon_a",
                TEST_BOUNCE_BYTES,
                TEST_BOUNCE_BYTES,
            )
            .await
            .err(),
            Some(zx::Status::IO)
        );

        drop(vbmeta_client);
        drop(zircon_client);
        scope.cancel().await;
    }

    /// Verifies that an unserved exact-name `StagedPublisher` entry (`"zircon_a"`) times out after
    /// [`STAGED_ENTRY_PROBE_TIMEOUT`] under fake time and returns `Err(zx::Status::NOT_FOUND)`.
    #[fuchsia::test]
    fn test_open_partition_in_block_dir_staged_entry_timeout() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let mut fs = ServiceFs::new_local();
        fs.dir("zircon_a").add_fidl_service(|stream: fblock::BlockRequestStream| stream);

        let (block_dir_client, block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        fs.serve_connection(fidl::endpoints::ServerEnd::new(block_dir_server.into_untyped()))
            .expect("serve mock block_dir");

        let scope = fasync::Scope::new_with_name("test_staged_timeout");
        scope.spawn_local(async move {
            let mut stalled_streams = Vec::new();
            while let Some(stream) = fs.next().await {
                stalled_streams.push(stream);
            }
        });

        let start = exec.now();
        let mut open_fut = std::pin::pin!(open_partition_in_block_dir(
            &block_dir_client,
            "zircon_a",
            TEST_BOUNCE_BYTES,
            TEST_BOUNCE_BYTES,
        ));

        assert!(matches!(exec.run_until_stalled(&mut open_fut), Poll::Pending));

        // 1 ms before `STAGED_ENTRY_PROBE_TIMEOUT`: still waiting on staged `"zircon_a"`.
        exec.set_fake_time(
            start + STAGED_ENTRY_PROBE_TIMEOUT - zx::MonotonicDuration::from_millis(1),
        );
        exec.wake_expired_timers();
        assert!(matches!(exec.run_until_stalled(&mut open_fut), Poll::Pending));

        // At `STAGED_ENTRY_PROBE_TIMEOUT`: staged `"zircon_a"` times out with `NOT_FOUND`.
        exec.set_fake_time(start + STAGED_ENTRY_PROBE_TIMEOUT);
        exec.wake_expired_timers();
        assert!(matches!(
            exec.run_until_stalled(&mut open_fut),
            Poll::Ready(Err(zx::Status::NOT_FOUND))
        ));
    }

    #[fuchsia::test]
    async fn test_open_any_matching_partition_empty_candidates() {
        let empty_res = open_any_matching_partition(&[], async |_name| {
            Ok::<_, zx::Status>(FakePartitionClient::new(512, 512, 0x00))
        })
        .await;
        assert!(matches!(empty_res, Err(zx::Status::NOT_SUPPORTED)));
    }

    #[fuchsia::test]
    async fn test_open_any_matching_partition_returns_ready_candidate_and_cancels_pending() {
        struct DropTracker<'a>(&'a Cell<bool>);
        impl Drop for DropTracker<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let pending_started = Cell::new(false);
        let pending_dropped = Cell::new(false);
        let client = open_any_matching_partition(&["zircon_a", "zircon-a"], async |name| {
            if name == "zircon_a" {
                pending_started.set(true);
                let _guard = DropTracker(&pending_dropped);
                futures::future::pending::<Result<FakePartitionClient, zx::Status>>().await
            } else {
                Ok(FakePartitionClient::new(1024, 512, 0xaa))
            }
        })
        .await
        .expect("open ready sibling candidate while primary is pending");

        assert!(pending_started.get(), "primary candidate should have been polled");
        assert!(pending_dropped.get(), "pending probe must be cancelled as soon as sibling opens");
        assert_eq!(client.partition_size(), Ok(1024));
    }

    #[fuchsia::test]
    async fn test_open_any_matching_partition_skips_not_found_candidates() {
        let tried = RefCell::new(Vec::new());
        let client = open_any_matching_partition(&["zircon_a", "zircon-a"], async |name| {
            tried.borrow_mut().push(name.to_string());
            if name == "zircon-a" {
                Ok(FakePartitionClient::new(1024, 512, 0xaa))
            } else {
                Err(zx::Status::NOT_FOUND)
            }
        })
        .await
        .expect("open fallback candidate");

        let mut tried = tried.into_inner();
        tried.sort();
        assert_eq!(tried, vec!["zircon-a", "zircon_a"]);
        assert_eq!(client.partition_size(), Ok(1024));
    }

    #[fuchsia::test]
    async fn test_open_any_matching_partition_all_not_found() {
        let tried = RefCell::new(Vec::new());
        let missing_res = open_any_matching_partition(&["zircon_a", "zircon-a"], async |name| {
            tried.borrow_mut().push(name.to_string());
            Err::<FakePartitionClient, _>(zx::Status::NOT_FOUND)
        })
        .await;

        assert!(matches!(missing_res, Err(zx::Status::NOT_FOUND)));
        let mut tried = tried.into_inner();
        tried.sort();
        assert_eq!(tried, vec!["zircon-a", "zircon_a"]);
    }

    #[fuchsia::test]
    async fn test_open_any_matching_partition_propagates_io_error_immediately() {
        struct DropTracker<'a>(&'a Cell<bool>);
        impl Drop for DropTracker<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let pending_dropped = Cell::new(false);
        let err_res = open_any_matching_partition(&["zircon_a", "zircon-a"], async |name| {
            if name == "zircon_a" {
                let _guard = DropTracker(&pending_dropped);
                futures::future::pending::<Result<FakePartitionClient, zx::Status>>().await
            } else {
                Err(zx::Status::IO_REFUSED)
            }
        })
        .await;

        assert!(matches!(err_res, Err(zx::Status::IO_REFUSED)));
        assert!(pending_dropped.get(), "pending probe must be cancelled on non-NOT_FOUND error");
    }

    /// Verifies [`NamedBlockBoardPartitions`] buffer-size validation, logical-to-candidate mapping
    /// (including [`FixedOffsetPartitionClient`](crate::partition::FixedOffsetPartitionClient)
    /// wrappers), error propagation, immediate resolution of a live candidate (`"zircon-a"`)
    /// without waiting [`STAGED_ENTRY_PROBE_TIMEOUT`] on an unserved sibling `StagedPublisher`
    /// (`"zircon_a"`), and parallel timeout when all staged candidates are unserved.
    #[fuchsia::test]
    fn test_named_block_board_partitions_mapping_and_staged_fallback() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        enum MapperMock {
            StalledZirconA(fblock::BlockRequestStream),
            LegacyNamedZirconA(fblock::BlockRequestStream),
            BrokenVbmetaA(fblock::BlockRequestStream),
            StalledMissingA(fblock::BlockRequestStream),
            StalledMissingB(fblock::BlockRequestStream),
        }

        let mut fs = ServiceFs::new_local();
        fs.dir("zircon_a").add_fidl_service(MapperMock::StalledZirconA);
        fs.dir("zircon-a").add_fidl_service(MapperMock::LegacyNamedZirconA);
        fs.dir("vbmeta_a").add_fidl_service(MapperMock::BrokenVbmetaA);
        fs.dir("stalled_a").add_fidl_service(MapperMock::StalledMissingA);
        fs.dir("stalled_b").add_fidl_service(MapperMock::StalledMissingB);

        let (block_dir_client, block_dir_server) =
            fidl_next::fuchsia::create_channel::<fio::Directory>();
        fs.serve_connection(fidl::endpoints::ServerEnd::new(block_dir_server.into_untyped()))
            .expect("serve mock block_dir");

        let stalled_zircon_a_stream: std::rc::Rc<RefCell<Option<fblock::BlockRequestStream>>> =
            std::rc::Rc::new(RefCell::new(None));
        let stalled_zircon_a_capture = stalled_zircon_a_stream.clone();

        let scope = fasync::Scope::new_with_name("test_mapper_adapter");
        let worker_scope = scope.to_handle();
        scope.spawn_local(async move {
            let mut stalled_streams = Vec::new();
            while let Some(service) = fs.next().await {
                match service {
                    MapperMock::StalledZirconA(stream) => {
                        *stalled_zircon_a_capture.borrow_mut() = Some(stream);
                    }
                    MapperMock::LegacyNamedZirconA(stream) => {
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            Some("zircon-a"),
                            MockBlockBehavior::Ready { block_count: 16, block_size: 512 },
                        );
                    }
                    MapperMock::BrokenVbmetaA(stream) => {
                        spawn_mock_block(
                            &worker_scope,
                            stream,
                            Some("vbmeta_a"),
                            MockBlockBehavior::FailGetInfo(zx::Status::IO_REFUSED),
                        );
                    }
                    MapperMock::StalledMissingA(stream) | MapperMock::StalledMissingB(stream) => {
                        stalled_streams.push(stream);
                    }
                }
            }
        });

        #[derive(Debug, Default)]
        struct TestMapper;
        impl NamedPhysicalPartitionMapper for TestMapper {
            async fn open_partition<P: PartitionClient + 'static>(
                &self,
                partition_name: &str,
                physical_part_opener: impl AsyncFn(&str) -> Result<P, zx::Status>,
            ) -> Result<Box<dyn PartitionClient>, zx::Status> {
                let client = match partition_name {
                    "zircon_a" => {
                        open_any_matching_partition(&["zircon_a", "zircon-a"], physical_part_opener)
                            .await?
                    }
                    "bootloader" => {
                        return Ok(Box::new(
                            physical_part_opener("zircon-a").await?.with_offset(512),
                        ));
                    }
                    "vbmeta_a" => physical_part_opener("vbmeta_a").await?,
                    "stalled_missing" => {
                        open_any_matching_partition(
                            &["stalled_a", "stalled_b"],
                            physical_part_opener,
                        )
                        .await?
                    }
                    "missing_logical" => physical_part_opener("nonexistent_part").await?,
                    _ => return Err(zx::Status::NOT_SUPPORTED),
                };
                Ok(Box::new(client))
            }
        }

        assert_eq!(
            NamedBlockBoardPartitions::new(
                TestMapper,
                TEST_BOUNCE_BYTES,
                NonZero::new(4096).unwrap(),
            )
            .unwrap_err(),
            zx::Status::INVALID_ARGS
        );

        let board =
            NamedBlockBoardPartitions::new(TestMapper, TEST_BOUNCE_BYTES, TEST_BOUNCE_BYTES)
                .expect("create NamedBlockBoardPartitions");

        // 1. Live `"zircon-a"` resolves immediately without advancing fake time past
        //    `STAGED_ENTRY_PROBE_TIMEOUT` on stalled `"zircon_a"`, and cancels `"zircon_a"`'s
        //    in-flight `GetInfo` channel.
        let start = exec.now();
        let mut open_fut = std::pin::pin!(board.open_partition("zircon_a", &block_dir_client));
        let Poll::Ready(Ok(client)) = exec.run_until_stalled(&mut open_fut) else {
            panic!("expected immediate resolution of live zircon-a without 5s staged timeout");
        };
        assert_eq!(exec.now(), start);
        assert_eq!(client.partition_size(), Ok(16 * 512));
        drop(client);

        let mut stalled_stream = stalled_zircon_a_stream
            .borrow_mut()
            .take()
            .expect("stalled zircon_a stream connected during concurrent probe");
        // Drain the queued `GetInfo` request, then confirm the client channel was closed on cancel.
        assert!(matches!(exec.run_until_stalled(&mut stalled_stream.next()), Poll::Ready(Some(_))));
        assert!(matches!(exec.run_until_stalled(&mut stalled_stream.next()), Poll::Ready(None)));

        // 2. Heterogeneous composite return (`FixedOffsetPartitionClient` via `with_offset(512)`).
        let mut bl_fut = std::pin::pin!(board.open_partition("bootloader", &block_dir_client));
        let Poll::Ready(Ok(bl_client)) = exec.run_until_stalled(&mut bl_fut) else {
            panic!("expected immediate open of offset bootloader partition");
        };
        assert_eq!(bl_client.partition_size(), Ok(16 * 512 - 512));
        drop(bl_client);

        // 3. Explicit driver error (`IO_REFUSED`) propagates immediately.
        let mut io_err_fut = std::pin::pin!(board.open_partition("vbmeta_a", &block_dir_client));
        assert!(matches!(
            exec.run_until_stalled(&mut io_err_fut),
            Poll::Ready(Err(zx::Status::IO_REFUSED))
        ));

        // 4. Two unserved staged candidates time out concurrently in a single
        //    `STAGED_ENTRY_PROBE_TIMEOUT` window rather than `2 * STAGED_ENTRY_PROBE_TIMEOUT`.
        let mut stalled_missing_fut =
            std::pin::pin!(board.open_partition("stalled_missing", &block_dir_client));
        assert!(matches!(exec.run_until_stalled(&mut stalled_missing_fut), Poll::Pending));
        exec.set_fake_time(
            start + STAGED_ENTRY_PROBE_TIMEOUT - zx::MonotonicDuration::from_millis(1),
        );
        exec.wake_expired_timers();
        assert!(matches!(exec.run_until_stalled(&mut stalled_missing_fut), Poll::Pending));
        exec.set_fake_time(start + STAGED_ENTRY_PROBE_TIMEOUT);
        exec.wake_expired_timers();
        assert!(matches!(
            exec.run_until_stalled(&mut stalled_missing_fut),
            Poll::Ready(Err(zx::Status::NOT_FOUND))
        ));

        // 5. Unstaged missing and unsupported partition names fail immediately.
        let mut missing_fut =
            std::pin::pin!(board.open_partition("missing_logical", &block_dir_client));
        assert!(matches!(
            exec.run_until_stalled(&mut missing_fut),
            Poll::Ready(Err(zx::Status::NOT_FOUND))
        ));

        let mut unsupported_fut =
            std::pin::pin!(board.open_partition("unsupported_part", &block_dir_client));
        assert!(matches!(
            exec.run_until_stalled(&mut unsupported_fut),
            Poll::Ready(Err(zx::Status::NOT_SUPPORTED))
        ));
    }

    /// Verifies [`boot_manager_open_any_partition_in`] against a `BootManager` block directory,
    /// including empty/invalid candidate handling, immediate resolution of a live
    /// candidate (`"misc"`) without waiting [`STAGED_ENTRY_PROBE_TIMEOUT`] on an unserved sibling
    /// `StagedPublisher` (`"durable_boot"`), non-`NOT_FOUND` error propagation, and parallel
    /// timeout when all staged candidates are unserved.
    #[fuchsia::test]
    fn test_boot_manager_open_any_partition_in_and_staged_fallback() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        enum BlockDirMock {
            StalledDurableBoot(fblock::BlockRequestStream),
            NamedMisc(fblock::BlockRequestStream),
            DeniedPart(fblock::BlockRequestStream),
            StalledMissingA(fblock::BlockRequestStream),
            StalledMissingB(fblock::BlockRequestStream),
        }
        let mut bm_fs = ServiceFs::new_local();
        bm_fs.dir("durable_boot").add_fidl_service(BlockDirMock::StalledDurableBoot);
        bm_fs.dir("misc").add_fidl_service(BlockDirMock::NamedMisc);
        bm_fs.dir("denied_part").add_fidl_service(BlockDirMock::DeniedPart);
        bm_fs.dir("stalled_a").add_fidl_service(BlockDirMock::StalledMissingA);
        bm_fs.dir("stalled_b").add_fidl_service(BlockDirMock::StalledMissingB);
        let (bm_dir, bm_server) = fidl_next::fuchsia::create_channel::<fio::Directory>();
        bm_fs
            .serve_connection(fidl::endpoints::ServerEnd::new(bm_server.into_untyped()))
            .expect("serve bootmanager block dir");

        let stalled_durable_boot: std::rc::Rc<RefCell<Option<fblock::BlockRequestStream>>> =
            std::rc::Rc::new(RefCell::new(None));
        let stalled_durable_boot_capture = stalled_durable_boot.clone();

        let scope = fasync::Scope::new_with_name("test_bm_bootmanager");
        let bm_worker = scope.to_handle();
        scope.spawn_local(async move {
            let mut stalled = Vec::new();
            while let Some(req) = bm_fs.next().await {
                match req {
                    BlockDirMock::StalledDurableBoot(stream) => {
                        *stalled_durable_boot_capture.borrow_mut() = Some(stream);
                    }
                    BlockDirMock::NamedMisc(stream) => {
                        spawn_mock_block(
                            &bm_worker,
                            stream,
                            Some("misc"),
                            MockBlockBehavior::Ready { block_count: 32, block_size: 512 },
                        );
                    }
                    BlockDirMock::DeniedPart(stream) => {
                        spawn_mock_block(
                            &bm_worker,
                            stream,
                            Some("denied_part"),
                            MockBlockBehavior::FailGetInfo(zx::Status::ACCESS_DENIED),
                        );
                    }
                    BlockDirMock::StalledMissingA(stream)
                    | BlockDirMock::StalledMissingB(stream) => {
                        stalled.push(stream);
                    }
                }
            }
        });

        let mut empty_fut = std::pin::pin!(boot_manager_open_any_partition_in(&bm_dir, &[]));
        assert!(matches!(
            exec.run_until_stalled(&mut empty_fut),
            Poll::Ready(Err(zx::Status::NOT_SUPPORTED))
        ));

        let mut invalid_fut = std::pin::pin!(boot_manager_open_any_partition_in(
            &bm_dir,
            &["", ".", "..", "sub/part"],
        ));
        assert!(matches!(
            exec.run_until_stalled(&mut invalid_fut),
            Poll::Ready(Err(zx::Status::NOT_FOUND))
        ));

        // Live `"misc"` resolves immediately without advancing fake time past
        // `STAGED_ENTRY_PROBE_TIMEOUT` on stalled `"durable_boot"`, and cancels `"durable_boot"`'s
        // in-flight `GetInfo` channel.
        let start = exec.now();
        let mut open_fut = std::pin::pin!(boot_manager_open_any_partition_in(
            &bm_dir,
            &["", "abr-meta", "durable_boot", "misc"],
        ));
        let Poll::Ready(Ok(client)) = exec.run_until_stalled(&mut open_fut) else {
            panic!("expected immediate resolution of misc without 5s staged timeout");
        };
        assert_eq!(exec.now(), start);
        assert_eq!(client.partition_size(), Ok(32 * 512));
        drop(client);

        let mut stalled_stream = stalled_durable_boot
            .borrow_mut()
            .take()
            .expect("stalled durable_boot stream connected during concurrent probe");
        assert!(matches!(exec.run_until_stalled(&mut stalled_stream.next()), Poll::Ready(Some(_))));
        assert!(matches!(exec.run_until_stalled(&mut stalled_stream.next()), Poll::Ready(None)));

        // Non-`NOT_FOUND` errors propagate immediately without waiting on stalled siblings.
        let mut denied_fut = std::pin::pin!(boot_manager_open_any_partition_in(
            &bm_dir,
            &["stalled_a", "denied_part"],
        ));
        assert!(matches!(
            exec.run_until_stalled(&mut denied_fut),
            Poll::Ready(Err(zx::Status::ACCESS_DENIED))
        ));
        assert_eq!(exec.now(), start);

        // Two unserved staged candidates time out concurrently in a single
        // `STAGED_ENTRY_PROBE_TIMEOUT` window.
        let mut stalled_missing_fut = std::pin::pin!(boot_manager_open_any_partition_in(
            &bm_dir,
            &["stalled_a", "stalled_b"],
        ));
        assert!(matches!(exec.run_until_stalled(&mut stalled_missing_fut), Poll::Pending));
        exec.set_fake_time(
            start + STAGED_ENTRY_PROBE_TIMEOUT - zx::MonotonicDuration::from_millis(1),
        );
        exec.wake_expired_timers();
        assert!(matches!(exec.run_until_stalled(&mut stalled_missing_fut), Poll::Pending));
        exec.set_fake_time(start + STAGED_ENTRY_PROBE_TIMEOUT);
        exec.wake_expired_timers();
        assert!(matches!(
            exec.run_until_stalled(&mut stalled_missing_fut),
            Poll::Ready(Err(zx::Status::NOT_FOUND))
        ));
    }

    /// Verifies that the public [`boot_manager_open_partition`] and
    /// [`boot_manager_open_any_partition`] helpers resolve entries strictly under
    /// `/block/bootmanager` and ignore unrelated entries under `/block`.
    #[fuchsia::test]
    async fn test_boot_manager_public_apis_open_under_block_bootmanager() {
        let mut bm_fs = ServiceFs::new_local();
        bm_fs.dir("durable_boot").add_fidl_service(|stream: fblock::BlockRequestStream| stream);
        let (bm_client, bm_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_io::DirectoryMarker>();
        bm_fs.serve_connection(bm_server).expect("serve /block/bootmanager");
        let _bm_guard = NamespaceBindGuard::bind(BOOT_MANAGER_BLOCK_DIR_PATH, bm_client);

        let mut root_only_fs = ServiceFs::new_local();
        root_only_fs.add_fidl_service(|stream: fblock::BlockRequestStream| stream);
        let (root_only_client, root_only_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_io::DirectoryMarker>();
        root_only_fs.serve_connection(root_only_server).expect("serve /block/root_only");
        let _root_only_guard = NamespaceBindGuard::bind("/block/root_only", root_only_client);

        let scope = fasync::Scope::new_with_name("test_bm_public_ns");
        let bm_worker = scope.to_handle();
        scope.spawn_local(async move {
            while let Some(stream) = bm_fs.next().await {
                spawn_mock_block(
                    &bm_worker,
                    stream,
                    Some("durable_boot"),
                    MockBlockBehavior::Ready { block_count: 12, block_size: 512 },
                );
            }
        });
        let root_worker = scope.to_handle();
        scope.spawn_local(async move {
            while let Some(stream) = root_only_fs.next().await {
                spawn_mock_block(
                    &root_worker,
                    stream,
                    Some("root_only"),
                    MockBlockBehavior::Ready { block_count: 16, block_size: 512 },
                );
            }
        });

        let single = boot_manager_open_partition("durable_boot")
            .await
            .expect("open /block/bootmanager/durable_boot");
        assert_eq!(single.partition_size(), Ok(12 * 512));
        drop(single);

        let any = boot_manager_open_any_partition(&["abr_meta", "durable_boot"])
            .await
            .expect("open matching candidate under /block/bootmanager");
        assert_eq!(any.partition_size(), Ok(12 * 512));
        drop(any);

        assert_eq!(
            boot_manager_open_partition("root_only").await.err(),
            Some(zx::Status::NOT_FOUND)
        );

        scope.cancel().await;
    }
}
