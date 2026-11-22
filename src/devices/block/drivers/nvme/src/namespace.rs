// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Block server adapter for an NVMe namespace via [`block_server::callback_interface`].
//!
//! Session FIFO loops (`nvme-ns<N>-session`) and mapper page-in threads validate and submit I/O
//! synchronously through [`IoDispatcher`] with hardware slot pushback, receiving completions
//! directly from the controller interrupt handler.

use crate::commands::{IdentifyNamespace, NUM_LBA_FORMATS};
use crate::inspect::record_ints;
use crate::queue_pair::MAX_TRANSFER_PAGES;
use block_server::callback_interface::{Interface, Request, Session, SessionManager};
use block_server::{
    BlockInfo, BlockServer, DeviceInfo, Operation, ReadOptions, RequestId, WriteFlags, WriteOptions,
};
use fidl_fuchsia_driver_token as ftoken;
use fidl_fuchsia_hardware_block_volume as fvolume;
use fidl_fuchsia_storage_block::{BlockRequestStream, DeviceFlag, MapperRequestStream};
use fuchsia_inspect::Node as InspectNode;
use futures::TryStreamExt;
use log::{error, warn};
use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Weak};
use std::thread::Builder;
use zx::{Event, Rights, Status, Vmo, system_get_page_size};

/// Scheduler role applied to per-session FIFO worker threads.
pub const WORKER_SCHEDULER_ROLE: &str = "fuchsia.devices.block.drivers.nvme.worker";

/// Smallest logical block size allowed by the NVM Express Base Specification.
const MIN_BLOCK_SIZE: u32 = 512;

/// Largest logical block size supported by this driver.
const MAX_BLOCK_SIZE: u32 = 32768;

/// Maximum logical blocks addressable by one NVMe Read or Write command (0-based `u16` `NLB`).
const MAX_BLOCKS_PER_COMMAND: u64 = 1 << 16;

/// Defined bits of Identify Namespace `DPC` (End-to-end Data Protection Capabilities).
const DATA_PROTECTION_CAPABILITIES_MASK: u8 = 0x3f;

/// Protection information type mask of Identify Namespace `DPS` (End-to-end Data Protection Type).
const DATA_PROTECTION_TYPE_MASK: u8 = 0x3;

/// Bit 0 of Identify Namespace `NSATTR`: namespace is currently write-protected.
const NSATTR_WRITE_PROTECTED: u8 = 1 << 0;

/// Broadcast / invalid namespace identifier (`0xffff_ffff`).
const BROADCAST_NAMESPACE_ID: u32 = u32::MAX;

const _: () = {
    assert!(size_of::<usize>() >= size_of::<u32>());
    assert!(size_of::<u64>() >= size_of::<usize>());
};

/// Block server type serving one NVMe [`Namespace`].
pub type NamespaceBlockServer = BlockServer<SessionManager<Namespace>>;

/// Validated I/O operation submitted from [`Namespace::on_requests`] to [`IoDispatcher`].
#[derive(Debug)]
pub enum IoOperation {
    /// Read `block_count` logical blocks starting at `start_lba` into `vmo` at `vmo_offset`.
    Read {
        /// First logical block address to read.
        start_lba: u64,

        /// Non-zero number of logical blocks (`<= max_transfer_blocks`).
        block_count: u32,

        /// Destination VMO.
        vmo: Arc<Vmo>,

        /// Byte offset within `vmo`.
        vmo_offset: u64,

        /// Total byte length (`block_count * block_size`).
        bytes: usize,
    },

    /// Write `block_count` logical blocks starting at `start_lba` from `vmo` at `vmo_offset`.
    Write {
        /// First logical block address to write.
        start_lba: u64,

        /// Non-zero number of logical blocks (`<= max_transfer_blocks`).
        block_count: u32,

        /// Source VMO.
        vmo: Arc<Vmo>,

        /// Byte offset within `vmo`.
        vmo_offset: u64,

        /// Total byte length (`block_count * block_size`).
        bytes: usize,

        /// Whether Force Unit Access (`FUA`) is set on this command.
        force_unit_access: bool,
    },

    /// Commit volatile write caches of the namespace to non-volatile media.
    Flush,
}

/// Synchronously dispatches validated [`Namespace`] block operations to the NVMe I/O queue.
pub trait IoDispatcher: Send + Sync + 'static {
    /// Registers the [`SessionManager`] for `namespace_id` before any sessions are served.
    fn register_namespace(
        &self,
        namespace_id: u32,
        session_manager: Weak<SessionManager<Namespace>>,
    );

    /// Submits `operation` for `namespace_id`.
    ///
    /// Implementations may block the calling session thread when all hardware commands are occupied
    /// (pushback) and must eventually complete `request_id` on the namespace's [`SessionManager`].
    fn submit_io(&self, namespace_id: u32, operation: IoOperation, request_id: RequestId);
}

/// Controller-wide limits that apply to every namespace on the controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControllerLimits {
    /// Maximum data transfer size in bytes of a single command (`MDTS`), if bounded.
    pub max_data_transfer_bytes: Option<NonZeroU32>,

    /// Logical blocks written atomically during normal operation (`AWUN + 1`).
    pub atomic_write_unit_normal: u32,

    /// Logical blocks written atomically across a power failure (`AWUPF + 1`).
    pub atomic_write_unit_power_fail: u32,
}

/// [`block_server::callback_interface::Interface`] implementation for one NVMe namespace.
pub struct Namespace {
    namespace_id: u32,
    block_size: u32,
    block_count: u64,
    max_transfer_blocks: NonZeroU32,
    device_flags: DeviceFlag,
    device_info: DeviceInfo,
    dispatcher: Arc<dyn IoDispatcher>,
    session_manager: Weak<SessionManager<Self>>,
}

impl fmt::Debug for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namespace")
            .field("namespace_id", &self.namespace_id)
            .field("block_size", &self.block_size)
            .field("block_count", &self.block_count)
            .field("max_transfer_blocks", &self.max_transfer_blocks)
            .field("device_flags", &self.device_flags)
            .finish_non_exhaustive()
    }
}

impl Namespace {
    /// Returns the child node and `fuchsia.hardware.block.volume/Service` instance name for
    /// `namespace_id`.
    pub fn name(namespace_id: u32) -> String {
        format!("namespace-{namespace_id}")
    }

    fn complete_rejected(&self, request_id: RequestId, status: Status) {
        if let Some(sm) = self.session_manager.upgrade() {
            sm.complete_request(request_id, Err(status));
        }
    }

    fn validate_range(&self, device_block_offset: u64, block_count: u32) -> Result<u64, Status> {
        if block_count == 0 || block_count > self.max_transfer_blocks.get() {
            return Err(Status::INVALID_ARGS);
        }
        if device_block_offset
            .checked_add(u64::from(block_count))
            .is_none_or(|end| end > self.block_count)
        {
            return Err(Status::OUT_OF_RANGE);
        }
        Ok(u64::from(block_count) * u64::from(self.block_size))
    }

    fn validate_read<'a>(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: Option<&'a Arc<Vmo>>,
        vmo_offset: u64,
        options: ReadOptions,
    ) -> Result<(&'a Arc<Vmo>, u64), Status> {
        debug_assert!(!options.inline_crypto.is_enabled);
        let total_bytes = self.validate_range(device_block_offset, block_count)?;
        vmo_offset.checked_add(total_bytes).ok_or(Status::OUT_OF_RANGE)?;
        let vmo = vmo.ok_or(Status::INVALID_ARGS)?;
        Ok((vmo, total_bytes))
    }

    fn handle_read(
        &self,
        request_id: RequestId,
        device_block_offset: u64,
        block_count: u32,
        vmo: Option<&Arc<Vmo>>,
        vmo_offset: u64,
        options: ReadOptions,
    ) -> Result<(), Status> {
        let (vmo, total_bytes) =
            self.validate_read(device_block_offset, block_count, vmo, vmo_offset, options)?;
        self.dispatcher.submit_io(
            self.namespace_id,
            IoOperation::Read {
                start_lba: device_block_offset,
                block_count,
                vmo: Arc::clone(vmo),
                vmo_offset,
                bytes: total_bytes as usize,
            },
            request_id,
        );
        Ok(())
    }

    fn validate_write<'a>(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: Option<&'a Arc<Vmo>>,
        vmo_offset: u64,
        options: WriteOptions,
    ) -> Result<(&'a Arc<Vmo>, u64, bool), Status> {
        // `Session` rejects inline crypto when unsupported and emulates `PRE_BARRIER` via
        // `pre_flush` before calling `on_requests`.
        debug_assert!(!options.inline_crypto.is_enabled);
        debug_assert!(!options.flags.contains(WriteFlags::PRE_BARRIER));
        if self.device_flags.contains(DeviceFlag::READONLY) {
            return Err(Status::ACCESS_DENIED);
        }
        let total_bytes = self.validate_range(device_block_offset, block_count)?;
        vmo_offset.checked_add(total_bytes).ok_or(Status::OUT_OF_RANGE)?;
        let vmo = vmo.ok_or(Status::INVALID_ARGS)?;
        Ok((vmo, total_bytes, options.flags.contains(WriteFlags::FORCE_ACCESS)))
    }

    fn handle_write(
        &self,
        request_id: RequestId,
        device_block_offset: u64,
        block_count: u32,
        vmo: Option<&Arc<Vmo>>,
        vmo_offset: u64,
        options: WriteOptions,
    ) -> Result<(), Status> {
        let (vmo, total_bytes, force_unit_access) =
            self.validate_write(device_block_offset, block_count, vmo, vmo_offset, options)?;
        self.dispatcher.submit_io(
            self.namespace_id,
            IoOperation::Write {
                start_lba: device_block_offset,
                block_count,
                vmo: Arc::clone(vmo),
                vmo_offset,
                bytes: total_bytes as usize,
                force_unit_access,
            },
            request_id,
        );
        Ok(())
    }
}

impl Interface for Namespace {
    type Orchestrator = SessionManager<Self>;

    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Borrowed(&self.device_info)
    }

    fn spawn_session(&self, session: Arc<Session<Self>>) {
        let nsid = self.namespace_id;
        let session_for_thread = Arc::clone(&session);
        let spawned = Builder::new().name(format!("nvme-ns{nsid}-session")).spawn(move || {
            if let Err(err) = fuchsia_scheduler::set_role_for_this_thread(WORKER_SCHEDULER_ROLE) {
                warn!("Failed to set session thread role for namespace {nsid}: {err:?}");
            }
            session_for_thread.run();
        });
        if let Err(err) = spawned {
            error!("Failed to spawn session thread for namespace {nsid}: {err}");
            session.terminate_async();
        }
    }

    fn on_requests(&self, requests: &[Request]) {
        for request in requests {
            let result = match &request.operation {
                Operation::Read {
                    device_block_offset, block_count, vmo_offset, options, ..
                } => self.handle_read(
                    request.request_id,
                    *device_block_offset,
                    *block_count,
                    request.vmo.as_ref(),
                    *vmo_offset,
                    *options,
                ),
                Operation::Write {
                    device_block_offset, block_count, vmo_offset, options, ..
                } => self.handle_write(
                    request.request_id,
                    *device_block_offset,
                    *block_count,
                    request.vmo.as_ref(),
                    *vmo_offset,
                    *options,
                ),
                Operation::Flush => {
                    self.dispatcher.submit_io(
                        self.namespace_id,
                        IoOperation::Flush,
                        request.request_id,
                    );
                    Ok(())
                }
                Operation::Trim { .. }
                | Operation::CloseVmo
                | Operation::StartDecompressedRead { .. }
                | Operation::ContinueDecompressedRead { .. } => Err(Status::NOT_SUPPORTED),
            };
            if let Err(status) = result {
                self.complete_rejected(request.request_id, status);
            }
        }
    }
}

/// Block server wrapper for one NVMe namespace.
pub struct NamespaceServer {
    namespace_id: u32,
    server: NamespaceBlockServer,
}

impl fmt::Debug for NamespaceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamespaceServer")
            .field("namespace_id", &self.namespace_id)
            .finish_non_exhaustive()
    }
}

impl NamespaceServer {
    /// Creates the block device of the namespace with ID `namespace_id` described by `identify`.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` if `namespace_id` is `0` or `0xffff_ffff`, or `NOT_SUPPORTED` if
    /// `identify.nsze == 0`, the LBA format index/count is invalid, metadata is non-zero, the
    /// logical block size is unsupported, or one block exceeds the controller's transfer limit.
    pub fn new(
        namespace_id: u32,
        identify: &IdentifyNamespace,
        limits: &ControllerLimits,
        dispatcher: Arc<dyn IoDispatcher>,
        inspect: Option<&InspectNode>,
    ) -> Result<Arc<Self>, Status> {
        if namespace_id == 0 || namespace_id == BROADCAST_NAMESPACE_ID {
            error!("Attempted to create namespace with invalid ID {namespace_id}");
            return Err(Status::INVALID_ARGS);
        }

        let block_count = identify.nsze;
        if block_count == 0 {
            warn!("Namespace {namespace_id} reports zero size (NSZE=0); skipping");
            return Err(Status::NOT_SUPPORTED);
        }

        let nlbaf = usize::from(identify.nlbaf);
        let format_index = identify.lba_format_index();
        if nlbaf >= NUM_LBA_FORMATS || format_index > nlbaf {
            error!(
                "Namespace {namespace_id} reports invalid LBA format index {format_index} \
                 (NLBAF={nlbaf})"
            );
            return Err(Status::NOT_SUPPORTED);
        }

        let format = identify.active_lba_format();
        if format.metadata_size_bytes() != 0 {
            error!(
                "Namespace {namespace_id} uses an LBA format with {} bytes of metadata, which is \
                 not supported",
                format.metadata_size_bytes()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        let Some(block_size) = format
            .lba_data_size_bytes()
            .filter(|size| (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(size))
        else {
            error!(
                "Namespace {namespace_id} has a logical block size of 2^{}, which is not supported",
                format.lba_data_size_log2()
            );
            return Err(Status::NOT_SUPPORTED);
        };

        // A single command transfer is bounded by the 16-bit NLB count, the controller MDTS, and
        // the single PRP list page capacity of `QueuePair`.
        let page_size = u64::from(system_get_page_size());
        let max_transfer_pages = MAX_TRANSFER_PAGES as u64;
        let mut max_transfer_bytes =
            (u64::from(block_size) * MAX_BLOCKS_PER_COMMAND).min(max_transfer_pages * page_size);
        if let Some(controller_max) = limits.max_data_transfer_bytes {
            max_transfer_bytes = max_transfer_bytes.min(u64::from(controller_max.get()));
        }

        let max_transfer_blocks = u32::try_from(max_transfer_bytes / u64::from(block_size))
            .ok()
            .and_then(NonZeroU32::new);
        let Some(max_transfer_blocks) = max_transfer_blocks else {
            error!(
                "Namespace {namespace_id} has {block_size} byte blocks, but the controller \
                 transfers at most {max_transfer_bytes} bytes"
            );
            return Err(Status::NOT_SUPPORTED);
        };

        if let Some(inspect) = inspect {
            record_inspect(inspect, namespace_id, identify, limits, max_transfer_bytes, block_size);
        }

        let mut device_flags = DeviceFlag::FUA_SUPPORT;
        if (identify.nsattr & NSATTR_WRITE_PROTECTED) != 0 {
            device_flags |= DeviceFlag::READONLY;
        }

        let device_info = DeviceInfo::Block(BlockInfo {
            device_flags,
            block_count,
            max_transfer_blocks: Some(max_transfer_blocks),
        });

        let dispatcher_for_ns = Arc::clone(&dispatcher);
        let session_manager = Arc::new_cyclic(|weak_sm| {
            let namespace = Arc::new(Namespace {
                namespace_id,
                block_size,
                block_count,
                max_transfer_blocks,
                device_flags,
                device_info,
                dispatcher: dispatcher_for_ns,
                session_manager: weak_sm.clone(),
            });
            SessionManager::new(namespace, block_size)
        });
        dispatcher.register_namespace(namespace_id, Arc::downgrade(&session_manager));

        Ok(Arc::new(Self { namespace_id, server: BlockServer::new(block_size, session_manager) }))
    }

    /// Returns the underlying [`SessionManager`].
    pub fn session_manager(&self) -> &SessionManager<Namespace> {
        self.server.session_manager()
    }

    /// Serves the block protocol to one client until the client disconnects or the serving scope is
    /// cancelled.
    pub async fn serve_block(&self, stream: BlockRequestStream) {
        if let Err(error) = self.server.handle_requests(stream).await {
            warn!("Failed to serve a client of namespace {}: {error:?}", self.namespace_id);
        }
    }

    /// Serves the mapper protocol to one client until the client disconnects or the serving scope
    /// is cancelled.
    async fn serve_mapper(&self, stream: MapperRequestStream) {
        if let Err(error) = self.server.handle_mapper_requests(stream).await {
            warn!("Failed to serve mapper requests for namespace {}: {error:?}", self.namespace_id);
        }
    }

    /// Serves one connection to the `fuchsia.hardware.block.volume/Service` instance of this
    /// namespace. `node_token` is the DFv2 node token of the driver, if present.
    pub async fn serve_volume_service(
        &self,
        request: fvolume::ServiceRequest,
        node_token: Option<&Event>,
    ) {
        match request {
            fvolume::ServiceRequest::Volume(stream) => self.serve_block(stream).await,
            fvolume::ServiceRequest::Mapper(stream) => self.serve_mapper(stream).await,
            fvolume::ServiceRequest::Token(stream) => {
                if let Err(error) = serve_node_token(stream, node_token).await {
                    warn!(
                        "Failed to serve the node token of namespace {}: {error:?}",
                        self.namespace_id
                    );
                }
            }
            fvolume::ServiceRequest::InlineEncryption(_) => {
                warn!("Namespace {} does not offer the requested protocol", self.namespace_id);
            }
        }
    }

    /// Blocks the calling thread until all active session threads of this namespace have exited.
    ///
    /// Callers should cancel the outgoing FIDL serving scope first so active `Block` and `Mapper`
    /// streams close and any `MapperSession` pager threads join before draining active FIFO
    /// sessions.
    pub fn terminate(&self) {
        self.server.session_manager().terminate();
    }
}

/// Answers requests for the DFv2 node token of the driver.
async fn serve_node_token(
    mut stream: ftoken::NodeTokenRequestStream,
    node_token: Option<&Event>,
) -> Result<(), fidl::Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            ftoken::NodeTokenRequest::Get { responder } => {
                let token = match node_token {
                    Some(token) => {
                        token.duplicate_handle(Rights::SAME_RIGHTS).map_err(Status::into_raw)
                    }
                    None => Err(Status::NOT_FOUND.into_raw()),
                };
                responder.send(token)?;
            }
        }
    }
    Ok(())
}

/// Records the properties of a namespace in a child of `parent`.
fn record_inspect(
    parent: &InspectNode,
    namespace_id: u32,
    identify: &IdentifyNamespace,
    limits: &ControllerLimits,
    max_transfer_bytes: u64,
    block_size: u32,
) {
    // A namespace reports its own 0-based atomic write units when `NSFEAT` bit 1 is set.
    let (atomic_write_unit_normal, atomic_write_unit_power_fail) = if identify.ns_atomics() {
        (u32::from(identify.nawun) + 1, u32::from(identify.nawupf) + 1)
    } else {
        (limits.atomic_write_unit_normal, limits.atomic_write_unit_power_fail)
    };
    let node = parent.create_child(Namespace::name(namespace_id));
    record_ints(
        &node,
        &[
            ("atomic_write_unit_normal_blocks", i64::from(atomic_write_unit_normal)),
            ("atomic_write_unit_power_fail_blocks", i64::from(atomic_write_unit_power_fail)),
            ("namespace_atomic_boundary_size_normal_blocks", i64::from(identify.nabsn)),
            ("namespace_atomic_boundary_offset_blocks", i64::from(identify.nabo)),
            ("namespace_atomic_boundary_size_power_fail_blocks", i64::from(identify.nabspf)),
            ("namespace_optimal_io_boundary_blocks", i64::from(identify.noiob)),
            ("active_lba_format_index", saturating_int(identify.lba_format_index())),
            ("data_protection_caps", i64::from(identify.dpc & DATA_PROTECTION_CAPABILITIES_MASK)),
            ("data_protection_set", i64::from(identify.dps & DATA_PROTECTION_TYPE_MASK)),
            ("namespace_size_blocks", saturating_int(identify.nsze)),
            ("namespace_cap_blocks", saturating_int(identify.ncap)),
            ("namespace_util_blocks", saturating_int(identify.nuse)),
            ("max_transfer_bytes", saturating_int(max_transfer_bytes)),
            ("block_size_bytes", i64::from(block_size)),
        ],
    );

    for index in 0..=usize::from(identify.nlbaf).min(NUM_LBA_FORMATS - 1) {
        let Some(format) = identify.lba_formats.get(index).copied().filter(|format| format.0 != 0)
        else {
            continue;
        };
        let block_size_name = format!("lba_format_{index}_block_size_bytes");
        let rel_perf_name = format!("lba_format_{index}_relative_performance");
        let metadata_name = format!("lba_format_{index}_metadata_size_bytes");
        record_ints(
            &node,
            &[
                (&block_size_name, format.lba_data_size_bytes().map_or(0, i64::from)),
                (&rel_perf_name, i64::from(format.relative_performance())),
                (&metadata_name, i64::from(format.metadata_size_bytes())),
            ],
        );
    }
    parent.record(node);
}

/// Converts `value` to a signed `i64` Inspect property, clamping values above `i64::MAX`.
fn saturating_int(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::LbaFormat;
    use assert_matches::assert_matches;
    use block_client::{
        BlockClient, BufferSlice, MutableBufferSlice, RemoteBlockClient,
        WriteOptions as ClientWriteOptions,
    };
    use block_server::SessionManager as _;
    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::{create_proxy, create_proxy_and_stream};
    use fidl_fuchsia_driver_token::NodeTokenMarker;
    use fidl_fuchsia_storage_block::{
        BlockMarker, CreateVmoOptions, MapperMarker, MapperSessionMarker,
    };
    use fuchsia_async::{Scope, unblock};
    use fuchsia_inspect::Inspector;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use futures::{StreamExt, join};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    const NAMESPACE_ID: u32 = 1;
    const BLOCK_SIZE: u32 = 512;
    const BLOCK_SIZE_LOG2: u8 = 9;
    const BLOCK_COUNT: u64 = 1024;

    /// Mock [`IoDispatcher`] forwarding submitted requests to an async channel for test assertions.
    struct TestDispatcher {
        managers: Mutex<HashMap<u32, Weak<SessionManager<Namespace>>>>,
        closed: AtomicBool,
        sender: UnboundedSender<DispatchedRequest>,
    }

    impl TestDispatcher {
        fn new() -> (Arc<Self>, UnboundedReceiver<DispatchedRequest>) {
            let (sender, receiver) = unbounded();
            let dispatcher = Arc::new(Self {
                managers: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
                sender,
            });
            (dispatcher, receiver)
        }

        fn close(&self) {
            self.closed.store(true, Ordering::Release);
        }

        fn complete_request(
            &self,
            namespace_id: u32,
            request_id: RequestId,
            status: Result<(), Status>,
        ) {
            if let Some(sm) =
                self.managers.lock().unwrap().get(&namespace_id).and_then(Weak::upgrade)
            {
                sm.complete_request(request_id, status);
            }
        }
    }

    impl IoDispatcher for TestDispatcher {
        fn register_namespace(
            &self,
            namespace_id: u32,
            session_manager: Weak<SessionManager<Namespace>>,
        ) {
            self.managers.lock().unwrap().insert(namespace_id, session_manager);
        }

        fn submit_io(&self, namespace_id: u32, operation: IoOperation, request_id: RequestId) {
            if self.closed.load(Ordering::Acquire) {
                self.complete_request(namespace_id, request_id, Err(Status::PEER_CLOSED));
                return;
            }
            let req = DispatchedRequest {
                dispatcher: self.managers.lock().unwrap().get(&namespace_id).cloned(),
                namespace_id,
                operation,
                request_id: Some(request_id),
            };
            if let Err(err) = self.sender.unbounded_send(req) {
                err.into_inner().complete(Err(Status::PEER_CLOSED));
            }
        }
    }

    struct DispatchedRequest {
        dispatcher: Option<Weak<SessionManager<Namespace>>>,
        namespace_id: u32,
        operation: IoOperation,
        request_id: Option<RequestId>,
    }

    impl DispatchedRequest {
        fn complete(mut self, status: Result<(), Status>) {
            if let Some(request_id) = self.request_id.take()
                && let Some(sm) = self.dispatcher.as_ref().and_then(Weak::upgrade)
            {
                sm.complete_request(request_id, status);
            }
        }
    }

    impl Drop for DispatchedRequest {
        fn drop(&mut self) {
            if let Some(request_id) = self.request_id.take()
                && let Some(sm) = self.dispatcher.as_ref().and_then(Weak::upgrade)
            {
                sm.complete_request(request_id, Err(Status::PEER_CLOSED));
            }
        }
    }

    /// Describes a 1-format (`NLBAF == 0`) namespace of `BLOCK_COUNT` blocks of `BLOCK_SIZE` bytes.
    fn identify() -> IdentifyNamespace {
        let mut identify = IdentifyNamespace {
            nsze: BLOCK_COUNT,
            ncap: BLOCK_COUNT,
            nlbaf: 0,
            ..Default::default()
        };
        identify.lba_formats[0] = LbaFormat::from_parts(BLOCK_SIZE_LOG2, 0, 0);
        identify
    }

    fn page_size() -> u64 {
        u64::from(system_get_page_size())
    }

    /// A namespace with one connected `RemoteBlockClient` whose I/O arrives on `requests`.
    struct Fixture {
        server: Arc<NamespaceServer>,
        dispatcher: Arc<TestDispatcher>,
        requests: UnboundedReceiver<DispatchedRequest>,
        client: RemoteBlockClient,
        scope: Scope,
    }

    impl Fixture {
        async fn new() -> Self {
            Self::with_config(identify(), ControllerLimits::default()).await
        }

        async fn with_config(identify: IdentifyNamespace, limits: ControllerLimits) -> Self {
            let (dispatcher, requests) = TestDispatcher::new();
            let dyn_dispatcher: Arc<dyn IoDispatcher> = Arc::clone(&dispatcher) as _;
            let server =
                NamespaceServer::new(NAMESPACE_ID, &identify, &limits, dyn_dispatcher, None)
                    .unwrap();
            let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
            let scope = Scope::new();
            let serving = Arc::clone(&server);
            scope.spawn(async move { serving.serve_block(stream).await });
            let client = RemoteBlockClient::new(proxy).await.unwrap();
            Self { server, dispatcher, requests, client, scope }
        }
    }

    fn blocks(count: u64) -> u64 {
        count * u64::from(BLOCK_SIZE)
    }

    fn buffer(block_count: u64) -> Vec<u8> {
        vec![0; blocks(block_count) as usize]
    }

    #[fuchsia::test]
    fn name() {
        assert_eq!(Namespace::name(7), "namespace-7");
    }

    #[fuchsia::test]
    async fn device_info_and_read_only_flag() {
        let (dispatcher, _requests) = TestDispatcher::new();
        let server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify(),
            &ControllerLimits::default(),
            dispatcher,
            None,
        )
        .unwrap();

        let max_transfer_bytes = (MAX_TRANSFER_PAGES as u64) * page_size();
        let max_transfer_blocks =
            u32::try_from(max_transfer_bytes / u64::from(BLOCK_SIZE)).unwrap();
        assert_matches!(
            &*server.session_manager().get_info(),
            DeviceInfo::Block(info) => {
                assert_eq!(info.device_flags, DeviceFlag::FUA_SUPPORT);
                assert_eq!(info.block_count, BLOCK_COUNT);
                assert_eq!(info.max_transfer_blocks, NonZeroU32::new(max_transfer_blocks));
            }
        );

        // Unpartitioned namespaces report `ZX_ERR_NOT_SUPPORTED` on partition-only queries.
        let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
        let scope = Scope::new();
        let serving = Arc::clone(&server);
        scope.spawn(async move { serving.serve_block(stream).await });
        assert_eq!(proxy.get_type_guid().await.unwrap().0, Status::NOT_SUPPORTED.into_raw());
        assert_eq!(proxy.get_instance_guid().await.unwrap().0, Status::NOT_SUPPORTED.into_raw());
        assert_eq!(proxy.get_name().await.unwrap().0, Status::NOT_SUPPORTED.into_raw());
        assert_eq!(proxy.get_metadata().await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
        drop(proxy);
        scope.await;

        let mut ro_identify = identify();
        ro_identify.nsattr = NSATTR_WRITE_PROTECTED;
        let (ro_dispatcher, _ro_requests) = TestDispatcher::new();
        let ro_server = NamespaceServer::new(
            NAMESPACE_ID,
            &ro_identify,
            &ControllerLimits::default(),
            ro_dispatcher,
            None,
        )
        .unwrap();
        assert_matches!(
            &*ro_server.session_manager().get_info(),
            DeviceInfo::Block(info) => {
                assert_eq!(info.device_flags, DeviceFlag::FUA_SUPPORT | DeviceFlag::READONLY);
            }
        );
    }

    #[fuchsia::test]
    fn max_transfer_is_limited_by_controller_and_command() {
        let max_transfer_blocks = |identify: &IdentifyNamespace, limits: &ControllerLimits| {
            let (dispatcher, _requests) = TestDispatcher::new();
            let server = NamespaceServer::new(NAMESPACE_ID, identify, limits, dispatcher, None)?;
            let info = server.session_manager().get_info().into_owned();
            Ok::<_, Status>(info.max_transfer_blocks().map(NonZeroU32::get))
        };

        // Controller limited to 64 KiB.
        let limits = ControllerLimits {
            max_data_transfer_bytes: NonZeroU32::new(65536),
            ..Default::default()
        };
        assert_eq!(max_transfer_blocks(&identify(), &limits), Ok(Some(65536 / BLOCK_SIZE)));

        // Controller cannot transfer even a single logical block.
        let limits = ControllerLimits {
            max_data_transfer_bytes: NonZeroU32::new(256),
            ..Default::default()
        };
        assert_eq!(max_transfer_blocks(&identify(), &limits), Err(Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    fn rejects_invalid_nsid_zero_size_and_unsupported_formats() {
        let create_with_id = |nsid: u32, identify: &IdentifyNamespace| {
            let (dispatcher, _requests) = TestDispatcher::new();
            NamespaceServer::new(nsid, identify, &ControllerLimits::default(), dispatcher, None)
                .map(|_| ())
        };
        let create = |identify: &IdentifyNamespace| create_with_id(NAMESPACE_ID, identify);

        assert_eq!(create(&identify()), Ok(()));
        assert_eq!(create_with_id(0, &identify()), Err(Status::INVALID_ARGS));
        assert_eq!(create_with_id(u32::MAX, &identify()), Err(Status::INVALID_ARGS));

        // Inactive / zero-capacity namespace (`NSZE == 0`).
        let mut empty_ns = identify();
        empty_ns.nsze = 0;
        assert_eq!(create(&empty_ns), Err(Status::NOT_SUPPORTED));

        // Format with non-zero metadata.
        let mut with_metadata = identify();
        with_metadata.lba_formats[0] = LbaFormat::from_parts(BLOCK_SIZE_LOG2, 8, 0);
        assert_eq!(create(&with_metadata), Err(Status::NOT_SUPPORTED));

        // Logical blocks that are unused (`0`), too small (`8`), too large (`16`, `31`), or
        // overflowing (`32`, `64`).
        for block_size_log2 in [0, 8, 16, 31, 32, 64] {
            let mut unsupported = identify();
            unsupported.lba_formats[0] = LbaFormat::from_parts(block_size_log2, 0, 0);
            assert_eq!(
                create(&unsupported),
                Err(Status::NOT_SUPPORTED),
                "block_size_log2={block_size_log2}"
            );
        }

        // Largest supported logical block size (`32 KiB`).
        let mut largest = identify();
        largest.lba_formats[0] = LbaFormat::from_parts(15, 0, 0);
        assert_eq!(create(&largest), Ok(()));

        // Only the active format must have zero metadata and supported block size.
        let mut second_format = identify();
        second_format.nlbaf = 1;
        second_format.lba_formats[1] = LbaFormat::from_parts(BLOCK_SIZE_LOG2, 8, 0);
        assert_eq!(create(&second_format), Ok(()));
        second_format.flbas = 1;
        assert_eq!(create(&second_format), Err(Status::NOT_SUPPORTED));

        // Active format index exceeding `nlbaf` or `nlbaf >= NUM_LBA_FORMATS` is rejected.
        let mut index_past_nlbaf = identify();
        index_past_nlbaf.nlbaf = 0;
        index_past_nlbaf.flbas = 1;
        index_past_nlbaf.lba_formats[1] = LbaFormat::from_parts(BLOCK_SIZE_LOG2, 0, 0);
        assert_eq!(create(&index_past_nlbaf), Err(Status::NOT_SUPPORTED));

        let mut excessive_nlbaf = identify();
        excessive_nlbaf.nlbaf = NUM_LBA_FORMATS as u8;
        assert_eq!(create(&excessive_nlbaf), Err(Status::NOT_SUPPORTED));
    }

    #[fuchsia::test]
    async fn inspect() {
        // Single-format (`NLBAF == 0`) namespace records `lba_format_0_*`.
        let single_inspector = Inspector::default();
        let (single_dispatcher, _single_requests) = TestDispatcher::new();
        let _single_server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify(),
            &ControllerLimits::default(),
            single_dispatcher,
            Some(single_inspector.root()),
        )
        .unwrap();
        assert_data_tree!(single_inspector, root: {
            "namespace-1": contains {
                lba_format_0_block_size_bytes: 512i64,
                lba_format_0_relative_performance: 0i64,
                lba_format_0_metadata_size_bytes: 0i64,
                active_lba_format_index: 0i64,
                block_size_bytes: 512i64,
            }
        });

        let inspector = Inspector::default();
        let (dispatcher, _requests) = TestDispatcher::new();
        let mut identify = identify();
        identify.nuse = 100;
        identify.nlbaf = 2;
        identify.lba_formats[1] = LbaFormat::from_parts(12, 8, 2);

        // Non-zero secondary format with unsupported `lbads = 0` records `0` block size cleanly.
        identify.lba_formats[2] = LbaFormat::from_parts(0, 8, 1);
        identify.dpc = 0xff;
        identify.dps = 0xff;
        identify.nabsn = 3;
        identify.nabo = 4;
        identify.nabspf = 5;
        identify.noiob = 6;
        let limits = ControllerLimits {
            max_data_transfer_bytes: NonZeroU32::new(65536),
            atomic_write_unit_normal: 65536,
            atomic_write_unit_power_fail: 2,
        };
        let _server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify,
            &limits,
            dispatcher,
            Some(inspector.root()),
        )
        .unwrap();

        assert_data_tree!(inspector, root: {
            "namespace-1": {
                atomic_write_unit_normal_blocks: 65536i64,
                atomic_write_unit_power_fail_blocks: 2i64,
                namespace_atomic_boundary_size_normal_blocks: 3i64,
                namespace_atomic_boundary_offset_blocks: 4i64,
                namespace_atomic_boundary_size_power_fail_blocks: 5i64,
                namespace_optimal_io_boundary_blocks: 6i64,
                lba_format_0_block_size_bytes: 512i64,
                lba_format_0_relative_performance: 0i64,
                lba_format_0_metadata_size_bytes: 0i64,
                lba_format_1_block_size_bytes: 4096i64,
                lba_format_1_relative_performance: 2i64,
                lba_format_1_metadata_size_bytes: 8i64,
                lba_format_2_block_size_bytes: 0i64,
                lba_format_2_relative_performance: 1i64,
                lba_format_2_metadata_size_bytes: 8i64,
                active_lba_format_index: 0i64,
                data_protection_caps: 0x3fi64,
                data_protection_set: 3i64,
                namespace_size_blocks: 1024i64,
                namespace_cap_blocks: 1024i64,
                namespace_util_blocks: 100i64,
                max_transfer_bytes: 65536i64,
                block_size_bytes: 512i64,
            }
        });
    }

    #[fuchsia::test]
    async fn supports_nlbaf_16_formatted_at_lbaf16() {
        let inspector = Inspector::default();
        let (dispatcher, _requests) = TestDispatcher::new();
        let mut identify = identify();

        // 17 formats (`NLBAF == 16`, indices 0..=16), formatted at `LBAF16` (`FLBAS` bit 5 = 1).
        identify.nlbaf = 16;
        identify.flbas = 1 << 5;
        identify.lba_formats[16] = LbaFormat::from_parts(12, 0, 1);

        let _server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify,
            &ControllerLimits::default(),
            dispatcher,
            Some(inspector.root()),
        )
        .unwrap();

        assert_data_tree!(inspector, root: {
            "namespace-1": contains {
                lba_format_0_block_size_bytes: 512i64,
                lba_format_16_block_size_bytes: 4096i64,
                lba_format_16_relative_performance: 1i64,
                lba_format_16_metadata_size_bytes: 0i64,
                active_lba_format_index: 16i64,
                block_size_bytes: 4096i64,
            }
        });
    }

    #[fuchsia::test]
    async fn inspect_uses_atomic_write_units_of_namespace() {
        let inspector = Inspector::default();
        let (dispatcher, _requests) = TestDispatcher::new();
        let mut identify = identify();

        // Namespace-specific atomic write units are 0-based (`NSFEAT` bit 1).
        identify.nsfeat = 1 << 1;
        identify.nawun = 15;
        identify.nawupf = u16::MAX;
        let limits = ControllerLimits {
            atomic_write_unit_normal: 8,
            atomic_write_unit_power_fail: 2,
            ..Default::default()
        };
        let _server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify,
            &limits,
            dispatcher,
            Some(inspector.root()),
        )
        .unwrap();

        assert_data_tree!(inspector, root: {
            "namespace-1": contains {
                atomic_write_unit_normal_blocks: 16i64,
                atomic_write_unit_power_fail_blocks: 65536i64,
            }
        });
    }

    #[fuchsia::test]
    async fn read() {
        let mut fixture = Fixture::new().await;
        let mut data = buffer(4);
        let read = fixture.client.read_at(MutableBufferSlice::Memory(&mut data), blocks(8));
        let serve = async {
            let request = fixture.requests.next().await.unwrap();
            assert_eq!(request.namespace_id, NAMESPACE_ID);
            assert_matches!(
                &request.operation,
                IoOperation::Read { start_lba: 8, block_count: 4, bytes: 2048, .. }
            );
            request.complete(Ok(()));
        };
        let (result, ()) = join!(read, serve);
        assert_eq!(result, Ok(()));
    }

    #[fuchsia::test]
    async fn write_force_access_and_pre_barrier() {
        let mut fixture = Fixture::new().await;
        let data = buffer(2);

        // Normal write (`force_unit_access == false`).
        let write = fixture.client.write_at(BufferSlice::Memory(&data), blocks(16));
        let serve = async {
            let request = fixture.requests.next().await.unwrap();
            assert_matches!(
                &request.operation,
                IoOperation::Write {
                    start_lba: 16,
                    block_count: 2,
                    bytes: 1024,
                    force_unit_access: false,
                    ..
                }
            );
            request.complete(Ok(()));
        };
        assert_eq!(join!(write, serve).0, Ok(()));

        // `FORCE_ACCESS` is passed straight through to NVMe FUA without a simulated post-flush.
        let fua_opts = ClientWriteOptions { flags: WriteFlags::FORCE_ACCESS, ..Default::default() };
        let write_fua =
            fixture.client.write_at_with_opts(BufferSlice::Memory(&data), blocks(16), fua_opts);
        let serve_fua = async {
            let request = fixture.requests.next().await.unwrap();
            assert_matches!(
                &request.operation,
                IoOperation::Write { start_lba: 16, block_count: 2, force_unit_access: true, .. }
            );
            request.complete(Ok(()));
        };
        assert_eq!(join!(write_fua, serve_fua).0, Ok(()));

        // `PRE_BARRIER` is simulated by `callback_interface::Session` via a pre-flush followed by
        // the write once the flush completes.
        let barrier_opts = ClientWriteOptions {
            flags: WriteFlags::PRE_BARRIER | WriteFlags::FORCE_ACCESS,
            ..Default::default()
        };
        let write_barrier =
            fixture.client.write_at_with_opts(BufferSlice::Memory(&data), blocks(20), barrier_opts);
        let serve_barrier = async {
            let flush_req = fixture.requests.next().await.unwrap();
            assert_matches!(&flush_req.operation, IoOperation::Flush);
            assert!(futures::poll!(fixture.requests.next()).is_pending());
            flush_req.complete(Ok(()));

            let write_req = fixture.requests.next().await.unwrap();
            assert_matches!(
                &write_req.operation,
                IoOperation::Write { start_lba: 20, block_count: 2, force_unit_access: true, .. }
            );
            write_req.complete(Ok(()));
        };
        assert_eq!(join!(write_barrier, serve_barrier).0, Ok(()));
    }

    #[fuchsia::test]
    async fn fifo_splits_large_transfers() {
        // Limit transfers to 2 blocks (`1024` bytes).
        let limits = ControllerLimits {
            max_data_transfer_bytes: NonZeroU32::new(1024),
            ..Default::default()
        };
        let mut fixture = Fixture::with_config(identify(), limits).await;

        // FIFO 5-block write is split by `SessionHelper::map_request` into 2 + 2 + 1 blocks.
        let data = buffer(5);
        let write = fixture.client.write_at(BufferSlice::Memory(&data), blocks(10));
        let serve = async {
            let mut chunks = Vec::new();
            for _ in 0..3 {
                let req = fixture.requests.next().await.unwrap();
                if let IoOperation::Write { start_lba, block_count, vmo_offset, bytes, .. } =
                    req.operation
                {
                    chunks.push((start_lba, block_count, vmo_offset, bytes));
                } else {
                    panic!("unexpected operation: {:?}", req.operation);
                }
                req.complete(Ok(()));
            }
            chunks.sort_by_key(|c| c.0);
            assert_eq!(chunks, vec![(10, 2, 0, 1024), (12, 2, 1024, 1024), (14, 1, 2048, 512)]);
        };
        assert_eq!(join!(write, serve).0, Ok(()));
    }

    #[fuchsia::test]
    async fn read_only_namespace_rejects_writes_and_allows_reads() {
        let mut ro_identify = identify();
        ro_identify.nsattr = NSATTR_WRITE_PROTECTED;
        let mut fixture = Fixture::with_config(ro_identify, ControllerLimits::default()).await;

        let data = buffer(2);
        assert_eq!(
            fixture.client.write_at(BufferSlice::Memory(&data), 0).await,
            Err(Status::ACCESS_DENIED)
        );
        assert!(futures::poll!(fixture.requests.next()).is_pending());

        let mut read_buf = buffer(2);
        let read = fixture.client.read_at(MutableBufferSlice::Memory(&mut read_buf), 0);
        let serve = async {
            let req = fixture.requests.next().await.unwrap();
            assert_matches!(req.operation, IoOperation::Read { start_lba: 0, block_count: 2, .. });
            req.complete(Ok(()));
        };
        assert_eq!(join!(read, serve).0, Ok(()));
    }

    #[fuchsia::test]
    async fn validation_rejects_invalid_and_oversized_operations() {
        let (dispatcher, mut requests) = TestDispatcher::new();
        let namespace = Namespace {
            namespace_id: NAMESPACE_ID,
            block_size: BLOCK_SIZE,
            block_count: BLOCK_COUNT,
            max_transfer_blocks: NonZeroU32::new(16).unwrap(),
            device_flags: DeviceFlag::FUA_SUPPORT,
            device_info: DeviceInfo::Block(BlockInfo::default()),
            dispatcher,
            session_manager: Weak::new(),
        };
        let vmo = Arc::new(Vmo::create(page_size() * 4).unwrap());

        assert_eq!(
            namespace.validate_read(0, 0, Some(&vmo), 0, ReadOptions::default()).unwrap_err(),
            Status::INVALID_ARGS
        );
        assert_eq!(
            namespace.validate_read(0, 17, Some(&vmo), 0, ReadOptions::default()).unwrap_err(),
            Status::INVALID_ARGS
        );
        assert_eq!(
            namespace.validate_write(0, 17, Some(&vmo), 0, WriteOptions::default()).unwrap_err(),
            Status::INVALID_ARGS
        );
        assert_eq!(
            namespace.validate_read(0, 1, None, 0, ReadOptions::default()).unwrap_err(),
            Status::INVALID_ARGS
        );
        assert_eq!(
            namespace
                .validate_read(BLOCK_COUNT - 15, 16, Some(&vmo), 0, ReadOptions::default())
                .unwrap_err(),
            Status::OUT_OF_RANGE
        );
        assert_eq!(
            namespace
                .validate_read(u64::MAX, 1, Some(&vmo), 0, ReadOptions::default())
                .unwrap_err(),
            Status::OUT_OF_RANGE
        );
        assert_eq!(
            namespace
                .validate_read(0, 2, Some(&vmo), u64::MAX - 511, ReadOptions::default())
                .unwrap_err(),
            Status::OUT_OF_RANGE
        );
        assert!(futures::poll!(requests.next()).is_pending());
    }

    #[fuchsia::test]
    async fn flush_status_out_of_range_and_dropped_request() {
        let mut fixture = Fixture::new().await;

        // Successful flush.
        let flush = fixture.client.flush();
        let serve = async {
            let request = fixture.requests.next().await.unwrap();
            assert_matches!(&request.operation, IoOperation::Flush);
            request.complete(Ok(()));
        };
        assert_eq!(join!(flush, serve).0, Ok(()));

        // Controller error status propagated to client.
        let flush_err = fixture.client.flush();
        let serve_err = async {
            fixture.requests.next().await.unwrap().complete(Err(Status::IO));
        };
        assert_eq!(join!(flush_err, serve_err).0, Err(Status::IO));

        // Out-of-range read rejected before reaching `IoDispatcher`.
        let mut data = buffer(2);
        let oor = fixture
            .client
            .read_at(MutableBufferSlice::Memory(&mut data), blocks(BLOCK_COUNT - 1))
            .await;
        assert_eq!(oor, Err(Status::OUT_OF_RANGE));
        assert!(futures::poll!(fixture.requests.next()).is_pending());

        // Dropped dispatched request completes with `PEER_CLOSED`.
        let flush_drop = fixture.client.flush();
        let serve_drop = async {
            drop(fixture.requests.next().await.unwrap());
        };
        assert_eq!(join!(flush_drop, serve_drop).0, Err(Status::PEER_CLOSED));
    }

    #[fuchsia::test]
    async fn mapper_open_session_succeeds_over_block_and_volume_service() {
        const MAPPING_VMO_BYTES: u64 = 65536;

        let (dispatcher, _requests) = TestDispatcher::new();
        let server = NamespaceServer::new(
            NAMESPACE_ID,
            &identify(),
            &ControllerLimits::default(),
            dispatcher,
            None,
        )
        .unwrap();
        let scope = Scope::new();

        // 1. Connect Mapper via `Block::ConnectMapper`.
        let (block_proxy, block_stream) = create_proxy_and_stream::<BlockMarker>();
        let serving_block = Arc::clone(&server);
        scope.spawn(async move { serving_block.serve_block(block_stream).await });

        let (mapper_proxy, mapper_server) = create_proxy::<MapperMarker>();
        block_proxy.connect_mapper(mapper_server).await.unwrap().unwrap();

        let (session_proxy, session_server) = create_proxy::<MapperSessionMarker>();
        let mapping_vmo = Vmo::create(MAPPING_VMO_BYTES).unwrap();
        assert_eq!(
            mapper_proxy.open_session(session_server, mapping_vmo, None, None).await.unwrap(),
            Ok(())
        );
        let (_key, _paged_vmo) =
            session_proxy.create_vmo(8192, CreateVmoOptions::empty()).await.unwrap().unwrap();
        assert_eq!(session_proxy.close().await.unwrap(), Ok(()));
        drop(session_proxy);
        drop(mapper_proxy);
        drop(block_proxy);

        // 2. Connect Mapper directly via `fuchsia.hardware.block.volume/Service.mapper`.
        let (vol_mapper_proxy, vol_mapper_stream) = create_proxy_and_stream::<MapperMarker>();
        let serving_mapper = Arc::clone(&server);
        scope.spawn(async move {
            serving_mapper
                .serve_volume_service(fvolume::ServiceRequest::Mapper(vol_mapper_stream), None)
                .await;
        });

        let (vol_session_proxy, vol_session_server) = create_proxy::<MapperSessionMarker>();
        let vol_mapping_vmo = Vmo::create(MAPPING_VMO_BYTES).unwrap();
        assert_eq!(
            vol_mapper_proxy
                .open_session(vol_session_server, vol_mapping_vmo, None, None)
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(vol_session_proxy.close().await.unwrap(), Ok(()));
        drop(vol_session_proxy);
        drop(vol_mapper_proxy);

        scope.await;
    }

    #[fuchsia::test]
    async fn session_termination_and_dispatcher_closure() {
        let fixture = Fixture::new().await;

        // Closing the dispatcher fails new requests with `PEER_CLOSED`.
        fixture.dispatcher.close();
        assert_eq!(fixture.client.flush().await, Err(Status::PEER_CLOSED));

        // Cancelling the FIDL serving scope before `terminate()` closes active `Block` streams and
        // joins session worker threads cleanly.
        fixture.scope.cancel().await;
        let server = Arc::clone(&fixture.server);
        unblock(move || server.terminate()).await;

        assert_eq!(fixture.client.flush().await, Err(Status::CANCELED));
        drop(fixture.client);
    }

    #[fuchsia::test]
    async fn node_token() {
        let token = Event::create();
        for node_token in [Some(&token), None] {
            let (proxy, stream) = create_proxy_and_stream::<NodeTokenMarker>();
            let serve = async { serve_node_token(stream, node_token).await.unwrap() };
            let get = async {
                let result = proxy.get().await.unwrap();
                drop(proxy);
                result
            };
            let (result, ()) = join!(get, serve);
            match node_token {
                Some(token) => assert_eq!(result.unwrap().koid(), token.koid()),
                None => assert_eq!(result.unwrap_err(), Status::NOT_FOUND.into_raw()),
            }
        }
    }
}
