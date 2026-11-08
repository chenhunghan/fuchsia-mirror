// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Block I/O server for a single UFS Logical Unit (LUN) via `block_server::callback_interface`.
//!
//! Session FIFO loops (`ufs-lun<N>-session`) and `block_server` mapper threads submit requests
//! synchronously to the `TransferRequestProcessor` with pushback and receive completions directly
//! from the interrupt path.

use crate::constants::{
    LU_WRITE_PROTECT_PERMANENT, MAX_PRDT_ENTRIES, PROVISIONING_TYPE_THIN,
    PROVISIONING_TYPE_THIN_READ_ZERO, WORKER_SCHEDULER_ROLE,
};
use crate::descriptors::UnitDescriptor;
use crate::scsi::{Direction, Fua, ScsiOpcode, UnmapParameterList};
use crate::transfer_request_processor::{
    DataTransfer, IoCompleter, IoCompletion, TransferRequestProcessor,
};
use block_server::callback_interface::{Interface, Request, Session, SessionManager};
use block_server::{BlockServer, DeviceInfo, Operation, RequestId};
use fidl_fuchsia_storage_block::DeviceFlag;
use log::{error, warn};
use mmio::Mmio;
use std::borrow::Cow;
use std::num::NonZeroU32;
use std::sync::{Arc, Weak};
use zerocopy::IntoBytes;
use zx::Status;

/// Block server serving one LUN through a [`UfsBlockDevice`].
pub type LunBlockServer<M> = BlockServer<SessionManager<UfsBlockDevice<M>>>;

/// Per-LUN request translation and submission state of a [`UfsBlockDevice`].
struct LunIo<M: Mmio + Send + Sync + 'static> {
    lun: u8,
    block_size: u32,
    block_count: u64,
    device_flags: DeviceFlag,
    processor: Arc<TransferRequestProcessor<M>>,
}

/// Block device adapter translating `block_server` requests into UFS SCSI commands for a LUN.
pub struct UfsBlockDevice<M: Mmio + Send + Sync + 'static> {
    io: LunIo<M>,
    device_info: DeviceInfo,
}

impl<M: Mmio + Send + Sync + 'static> std::fmt::Debug for UfsBlockDevice<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UfsBlockDevice")
            .field("lun", &self.io.lun)
            .field("block_size", &self.io.block_size)
            .field("block_count", &self.io.block_count)
            .field("device_flags", &self.io.device_flags)
            .finish_non_exhaustive()
    }
}

impl<M: Mmio + Send + Sync + 'static> UfsBlockDevice<M> {
    /// Creates a `UfsBlockDevice` for `lun` with the geometry of `unit` and `device_flags` (see
    /// [`Self::device_flags`]).
    pub fn new(
        lun: u8,
        block_size: u32,
        unit: &UnitDescriptor,
        device_flags: DeviceFlag,
        processor: Arc<TransferRequestProcessor<M>>,
    ) -> Self {
        let block_count = unit.logical_block_count.get();
        let device_info = DeviceInfo::Partition(block_server::PartitionInfo {
            device_flags,
            max_transfer_blocks: Self::max_transfer_blocks(block_size),
            start_block_offset: Some(0),
            block_count,
            name: format!("lun{lun}"),
            flags: Some(0),
            ..Default::default()
        });
        let io = LunIo { lun, block_size, block_count, device_flags, processor };
        Self { io, device_info }
    }

    /// Wraps the device in a block server whose sessions run on worker threads.
    pub fn into_server(self) -> Arc<LunBlockServer<M>> {
        let block_size = self.io.block_size;
        Arc::new(BlockServer::new(
            block_size,
            Arc::new(SessionManager::new(Arc::new(self), block_size)),
        ))
    }

    /// Translates and synchronously submits a block request, completing rejected requests
    /// immediately.
    pub fn handle_operation(
        &self,
        operation: &Operation,
        vmo: Option<&Arc<zx::Vmo>>,
        completion: IoCompletion,
    ) {
        self.io.handle_operation(operation, vmo, completion)
    }

    /// Returns block flags for `unit`: FUA, TRIM (thin-provisioned only), and READONLY for
    /// permanently write-protected LUs.
    pub fn device_flags(unit: &UnitDescriptor) -> DeviceFlag {
        let mut flags = DeviceFlag::FUA_SUPPORT;
        if matches!(
            unit.provisioning_type,
            PROVISIONING_TYPE_THIN | PROVISIONING_TYPE_THIN_READ_ZERO
        ) {
            flags |= DeviceFlag::TRIM_SUPPORT;
        }
        if unit.lu_write_protect == LU_WRITE_PROTECT_PERMANENT {
            flags |= DeviceFlag::READONLY;
        }
        flags
    }

    /// Returns the maximum transfer size in blocks, accounting for one unaligned page offset.
    pub fn max_transfer_blocks(block_size: u32) -> Option<NonZeroU32> {
        let page_size = zx::system_get_page_size() as usize;
        let max_bytes = (MAX_PRDT_ENTRIES - 1) * page_size;
        let max_blocks = max_bytes.checked_div(usize::try_from(block_size).ok()?)?;
        NonZeroU32::new(u32::try_from(max_blocks).ok()?.max(1))
    }
}

impl<M: Mmio + Send + Sync + 'static> LunIo<M> {
    /// Validates that `block_count > 0` and `[device_block_offset, device_block_offset +
    /// block_count)` is within bounds.
    fn validate_block_range(
        &self,
        device_block_offset: u64,
        block_count: u32,
    ) -> Result<(), Status> {
        if block_count == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if device_block_offset >= self.block_count
            || self.block_count - device_block_offset < block_count as u64
        {
            return Err(Status::OUT_OF_RANGE);
        }
        Ok(())
    }

    /// Validates a READ or WRITE of `block_count` blocks at `device_block_offset` and builds its
    /// 10-byte or 16-byte CDB and data transfer from `vmo` at `vmo_offset`.
    ///
    /// # Errors
    ///
    /// Returns `ACCESS_DENIED` for a write to a READONLY LU, `OUT_OF_RANGE` if the block range is
    /// out of bounds or its byte length overflows, and `INVALID_ARGS` if no VMO is attached.
    fn prepare_read_write(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: Option<&Arc<zx::Vmo>>,
        vmo_offset: u64,
        direction: Direction,
    ) -> Result<([u8; 16], DataTransfer<'static>), Status> {
        let is_write = matches!(direction, Direction::Write { .. });
        if is_write && self.device_flags.contains(DeviceFlag::READONLY) {
            return Err(Status::ACCESS_DENIED);
        }
        self.validate_block_range(device_block_offset, block_count)?;
        let expected_transfer_len =
            block_count.checked_mul(self.block_size).ok_or(Status::OUT_OF_RANGE)?;
        // The block server always attaches a VMO to reads and writes.
        let vmo = Arc::clone(vmo.ok_or(Status::INVALID_ARGS)?);

        let cdb = ScsiOpcode::build_rw_cdb(direction, device_block_offset, block_count);

        let transfer = if is_write {
            DataTransfer::write(vmo, vmo_offset, expected_transfer_len)
        } else {
            DataTransfer::read(vmo, vmo_offset, expected_transfer_len)
        };
        Ok((cdb, transfer))
    }

    /// Validates a TRIM (`UNMAP`) of `block_count` blocks at `device_block_offset` and builds its
    /// CDB, inline parameter list, and unmapped byte count.
    fn prepare_trim(
        &self,
        device_block_offset: u64,
        block_count: u32,
    ) -> Result<([u8; 16], UnmapParameterList, u64), Status> {
        if !self.device_flags.contains(DeviceFlag::TRIM_SUPPORT) {
            return Err(Status::NOT_SUPPORTED);
        }
        if self.device_flags.contains(DeviceFlag::READONLY) {
            return Err(Status::ACCESS_DENIED);
        }
        self.validate_block_range(device_block_offset, block_count)?;

        let param_list = UnmapParameterList::new(device_block_offset, block_count);
        let cdb = ScsiOpcode::build_unmap_cdb(param_list.as_bytes().len() as u16);
        let unmapped_bytes = u64::from(block_count).saturating_mul(u64::from(self.block_size));
        Ok((cdb, param_list, unmapped_bytes))
    }

    /// Translates and synchronously submits a block request, completing rejected requests
    /// immediately.
    fn handle_operation(
        &self,
        operation: &Operation,
        vmo: Option<&Arc<zx::Vmo>>,
        completion: IoCompletion,
    ) {
        let unmap_params;
        let prepared = match operation {
            // TODO(https://fxbug.dev/527952709): Support inline crypto in the Rust UFS driver.
            Operation::Read {
                options: block_protocol::ReadOptions { inline_crypto, .. }, ..
            }
            | Operation::Write {
                options: block_protocol::WriteOptions { inline_crypto, .. },
                ..
            } if inline_crypto.is_enabled => Err(Status::NOT_SUPPORTED),
            Operation::Read { device_block_offset, block_count, vmo_offset, .. } => self
                .prepare_read_write(
                    *device_block_offset,
                    *block_count,
                    vmo,
                    *vmo_offset,
                    Direction::Read,
                )
                .map(|(cdb, transfer)| (cdb, Some(transfer))),
            Operation::Write { device_block_offset, block_count, vmo_offset, options, .. } => {
                if options.flags.contains(block_protocol::WriteFlags::PRE_BARRIER) {
                    Err(Status::NOT_SUPPORTED)
                } else {
                    let fua = if options.flags.contains(block_protocol::WriteFlags::FORCE_ACCESS) {
                        Fua::On
                    } else {
                        Fua::Off
                    };
                    self.prepare_read_write(
                        *device_block_offset,
                        *block_count,
                        vmo,
                        *vmo_offset,
                        Direction::Write { fua },
                    )
                    .map(|(cdb, transfer)| (cdb, Some(transfer)))
                }
            }
            Operation::Flush => Ok((ScsiOpcode::build_synchronize_cache_10_cdb(), None)),
            Operation::Trim { device_block_offset, block_count } => {
                match self.prepare_trim(*device_block_offset, *block_count) {
                    Ok((cdb, params, unmapped_bytes)) => {
                        unmap_params = params;
                        Ok((
                            cdb,
                            Some(DataTransfer::inline_unmap(
                                unmap_params.as_bytes(),
                                unmapped_bytes,
                            )),
                        ))
                    }
                    Err(status) => Err(status),
                }
            }
            // Unused by this interface: VMO closes are handled by the session; decompression is
            // not supported.
            Operation::CloseVmo
            | Operation::StartDecompressedRead { .. }
            | Operation::ContinueDecompressedRead { .. } => Err(Status::NOT_SUPPORTED),
        };
        match prepared {
            Ok((cdb, data)) => self.processor.submit_io(self.lun, cdb, data, completion),
            Err(status) => self.processor.reject_io(self.lun, completion, status),
        }
    }

    /// Submits `requests` in order on the calling thread, blocking for pushback as needed.
    fn handle_requests_blocking(&self, requests: &[Request]) {
        for request in requests {
            self.handle_operation(
                &request.operation,
                request.vmo.as_ref(),
                IoCompletion::Request(request.request_id),
            );
        }
    }
}

impl<M: Mmio + Send + Sync + 'static> Interface for UfsBlockDevice<M> {
    type Orchestrator = SessionManager<Self>;

    /// Reports the LU as a partition named `lun<N>`.
    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Borrowed(&self.device_info)
    }

    /// Runs the session FIFO loop on a dedicated worker thread.
    fn spawn_session(&self, session: Arc<Session<Self>>) {
        let lun = self.io.lun;
        let spawned =
            std::thread::Builder::new().name(format!("ufs-lun{lun}-session")).spawn(move || {
                if let Err(e) = fuchsia_scheduler::set_role_for_this_thread(WORKER_SCHEDULER_ROLE) {
                    warn!(lun, err:? = e; "Failed to set session thread role");
                }
                session.run();
            });
        if let Err(e) = spawned {
            error!(lun, err:% = e; "Failed to spawn session thread");
        }
    }

    /// Submits `requests` synchronously on the calling session or mapper worker thread, blocking
    /// for pushback while all I/O slots are busy.
    fn on_requests(&self, requests: &[Request]) {
        self.io.handle_requests_blocking(requests);
    }
}

/// Routes [`IoCompletion::Request`] outcomes to the block server for each LUN via weak references.
///
/// Holds [`Weak<LunBlockServer<M>>`] rather than [`Arc`] to break the reference cycle
/// `LunBlockServer -> UfsBlockDevice -> TransferRequestProcessor -> BlockServerCompleter ->
/// LunBlockServer`.
pub(crate) struct BlockServerCompleter<M: Mmio + Send + Sync + 'static> {
    /// Indexed by LUN.
    servers: Vec<Option<Weak<LunBlockServer<M>>>>,
}

impl<M: Mmio + Send + Sync + 'static> std::fmt::Debug for BlockServerCompleter<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let luns: Vec<usize> = self
            .servers
            .iter()
            .enumerate()
            .filter_map(|(lun, server)| server.as_ref().map(|_| lun))
            .collect();
        f.debug_struct("BlockServerCompleter").field("luns", &luns).finish()
    }
}

impl<M: Mmio + Send + Sync + 'static> BlockServerCompleter<M> {
    /// Creates a completer for `servers`, given as `(lun, server)` pairs.
    pub(crate) fn new<'a>(
        servers: impl IntoIterator<Item = (u8, &'a Arc<LunBlockServer<M>>)>,
    ) -> Self {
        let mut servers_by_lun: Vec<Option<Weak<LunBlockServer<M>>>> = Vec::new();
        for (lun, server) in servers {
            if servers_by_lun.len() <= lun as usize {
                servers_by_lun.resize_with(lun as usize + 1, || None);
            }
            servers_by_lun[lun as usize] = Some(Arc::downgrade(server));
        }
        Self { servers: servers_by_lun }
    }
}

impl<M: Mmio + Send + Sync + 'static> IoCompleter for BlockServerCompleter<M> {
    fn complete(&self, lun: u8, request_id: RequestId, status: Result<(), Status>) {
        match self.servers.get(lun as usize).and_then(|server| server.as_ref()?.upgrade()) {
            Some(server) => server.session_manager().complete_request(request_id, status),
            None => warn!(lun, request_id:?; "Dropping completion: no block server"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{CDB_FUA_BIT, INLINE_DATA_OFFSET};
    use crate::request_list::{OverallCommandStatus, SlotState};
    use crate::scsi::ScsiStatus;
    use crate::testing::fake_ufs::FakeMmio;
    use crate::testing::{
        ADMIN_SLOT, OCS_SUCCESS, PAGE_SIZE, TestProcessor, create_test_processor, delivered,
        fake_utp, io_callback, scsi_response, slot_state, wait_for_any_doorbell,
    };
    use crate::upiu::CommandUpiu;
    use block_protocol::{BlockFifoCommand, BlockFifoRequest, BlockFifoResponse};
    use fidl_fuchsia_storage_block as fblock;
    use fuchsia_async as fasync;
    use zerocopy::IntoBytes;
    use zerocopy::byteorder::big_endian::U64;

    type Device = UfsBlockDevice<FakeMmio>;

    fn device(lun: u8, unit: &UnitDescriptor, processor: Arc<TestProcessor>) -> Device {
        Device::new(lun, 4096, unit, Device::device_flags(unit), processor)
    }

    fn read(device_block_offset: u64, block_count: u32, vmo_offset: u64) -> Operation {
        Operation::Read {
            device_block_offset,
            block_count,
            _unused: 0,
            vmo_offset,
            options: Default::default(),
        }
    }

    fn write(device_block_offset: u64, block_count: u32, fua: Fua) -> Operation {
        let mut options = block_protocol::WriteOptions::default();
        if fua == Fua::On {
            options.flags |= block_protocol::WriteFlags::FORCE_ACCESS;
        }
        Operation::Write { device_block_offset, block_count, _unused: 0, vmo_offset: 0, options }
    }

    #[fuchsia::test]
    fn test_block_device_flags_and_max_transfer() {
        assert_eq!(Device::device_flags(&UnitDescriptor::default()), DeviceFlag::FUA_SUPPORT);
        let thin =
            UnitDescriptor { provisioning_type: PROVISIONING_TYPE_THIN, ..Default::default() };
        assert_eq!(Device::device_flags(&thin), DeviceFlag::FUA_SUPPORT | DeviceFlag::TRIM_SUPPORT);
        let thin_read_zero = UnitDescriptor {
            provisioning_type: PROVISIONING_TYPE_THIN_READ_ZERO,
            ..Default::default()
        };
        assert_eq!(
            Device::device_flags(&thin_read_zero),
            DeviceFlag::FUA_SUPPORT | DeviceFlag::TRIM_SUPPORT
        );
        let protected =
            UnitDescriptor { lu_write_protect: LU_WRITE_PROTECT_PERMANENT, ..Default::default() };
        assert_eq!(
            Device::device_flags(&protected),
            DeviceFlag::FUA_SUPPORT | DeviceFlag::READONLY
        );

        // One of the MAX_PRDT_ENTRIES pages may be consumed by an unaligned VMO offset.
        let blocks = |block_size| Device::max_transfer_blocks(block_size).map(|n| n.get());
        assert_eq!(blocks(0), None);
        assert_eq!(blocks(4096), Some(255));
        assert_eq!(blocks(8192), Some(127));
        assert_eq!(blocks(u32::MAX), Some(1));
    }

    #[fuchsia::test]
    async fn test_block_device_rejects_invalid_requests() {
        let (_mmio_vmo, processor) = create_test_processor();
        let vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let unit = UnitDescriptor {
            logical_block_count: U64::new(16),
            lu_write_protect: LU_WRITE_PROTECT_PERMANENT,
            ..Default::default()
        };
        let device = device(0, &unit, Arc::clone(&processor));

        // Reported as a partition named `lun<N>`.
        match &*device.get_info() {
            DeviceInfo::Partition(info) => {
                assert_eq!(info.name, "lun0");
                assert_eq!(info.block_count, 16);
                assert_eq!(info.start_block_offset, Some(0));
                assert_eq!(info.device_flags, DeviceFlag::FUA_SUPPORT | DeviceFlag::READONLY);
            }
            other => panic!("Unexpected device info: {other:?}"),
        }

        // Rejections are completed immediately, before touching hardware.
        let reject = |operation: Operation| {
            let (completion, mut rx) = io_callback();
            device.handle_operation(&operation, Some(&vmo), completion);
            delivered(&mut rx)
        };
        // No TRIM_SUPPORT: no UNMAP is sent.
        let trim = Operation::Trim { device_block_offset: 0, block_count: 1 };
        assert_eq!(reject(trim.clone()), Err(Status::NOT_SUPPORTED));
        // Thin-provisioned + READONLY LUN rejects Trim with ACCESS_DENIED.
        let thin_ro_unit = UnitDescriptor {
            logical_block_count: U64::new(16),
            provisioning_type: PROVISIONING_TYPE_THIN,
            lu_write_protect: LU_WRITE_PROTECT_PERMANENT,
            ..Default::default()
        };
        let thin_ro_device = Device::new(
            1,
            4096,
            &thin_ro_unit,
            Device::device_flags(&thin_ro_unit),
            Arc::clone(&processor),
        );
        let (completion, mut rx) = io_callback();
        thin_ro_device.handle_operation(&trim, Some(&vmo), completion);
        assert_eq!(delivered(&mut rx), Err(Status::ACCESS_DENIED));
        // READONLY.
        assert_eq!(reject(write(0, 1, Fua::Off)), Err(Status::ACCESS_DENIED));
        // PRE_BARRIER is not supported.
        let mut barrier_write = write(0, 1, Fua::Off);
        let Operation::Write { options, .. } = &mut barrier_write else { unreachable!() };
        options.flags |= block_protocol::WriteFlags::PRE_BARRIER;
        assert_eq!(reject(barrier_write), Err(Status::NOT_SUPPORTED));
        // Inline crypto.
        let mut crypto_read = read(0, 1, 0);
        let Operation::Read { options, .. } = &mut crypto_read else { unreachable!() };
        options.inline_crypto.is_enabled = true;
        assert_eq!(reject(crypto_read), Err(Status::NOT_SUPPORTED));
        let mut crypto_write = write(0, 1, Fua::Off);
        let Operation::Write { options, .. } = &mut crypto_write else { unreachable!() };
        options.inline_crypto.is_enabled = true;
        assert_eq!(reject(crypto_write), Err(Status::NOT_SUPPORTED));
        // Zero block count is rejected with INVALID_ARGS.
        assert_eq!(reject(read(0, 0, 0)), Err(Status::INVALID_ARGS));
        // Out of range, including offset + count overflow.
        assert_eq!(reject(read(16, 1, 0)), Err(Status::OUT_OF_RANGE));
        assert_eq!(reject(read(15, 2, 0)), Err(Status::OUT_OF_RANGE));
        assert_eq!(reject(read(u64::MAX, u32::MAX, 0)), Err(Status::OUT_OF_RANGE));
        // Never issued by the block server.
        assert_eq!(reject(Operation::CloseVmo), Err(Status::NOT_SUPPORTED));
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
    }

    #[fuchsia::test]
    async fn test_block_device_commands() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let vmo = Arc::new(zx::Vmo::create(4 * PAGE_SIZE).unwrap());
        let unit = UnitDescriptor {
            logical_block_count: U64::new(16),
            provisioning_type: PROVISIONING_TYPE_THIN,
            ..Default::default()
        };
        let device = device(2, &unit, Arc::clone(&processor));

        let (read_done, mut read_rx) = io_callback();
        device.handle_operation(&read(3, 2, 4096), Some(&vmo), read_done);
        let (write_done, mut write_rx) = io_callback();
        device.handle_operation(&write(5, 1, Fua::On), Some(&vmo), write_done);
        let (flush_done, mut flush_rx) = io_callback();
        device.handle_operation(&Operation::Flush, None, flush_done);
        let (trim_done, mut trim_rx) = io_callback();
        let trim = Operation::Trim { device_block_offset: 8, block_count: 4 };
        device.handle_operation(&trim, None, trim_done);
        assert_eq!(utp.doorbell(), 0b1111);

        // READ(10) of 2 blocks at LBA 3 from VMO offset 4096.
        let command: CommandUpiu = utp.request(0);
        assert_eq!(command.header.lun, 2);
        assert_eq!(command.cdb[0], ScsiOpcode::Read10 as u8);
        assert_eq!(u32::from_be_bytes(command.cdb[2..6].try_into().unwrap()), 3);
        assert_eq!(u16::from_be_bytes(command.cdb[7..9].try_into().unwrap()), 2);
        assert_eq!(command.expected_data_transfer_length.get(), 8192);
        assert_eq!(utp.utrd(0).prdt_length(), 2);
        // WRITE(10) with FUA.
        let command: CommandUpiu = utp.request(1);
        assert_eq!(command.cdb[0], ScsiOpcode::Write10 as u8);
        assert_ne!(command.cdb[1] & CDB_FUA_BIT, 0, "FUA bit not set");
        assert_eq!(command.header.flags, CommandUpiu::FLAG_WRITE);
        // SYNCHRONIZE CACHE(10) without data.
        let command: CommandUpiu = utp.request(2);
        assert_eq!(command.cdb[0], ScsiOpcode::SynchronizeCache10 as u8);
        assert_eq!(utp.utrd(2).prdt_length(), 0);
        // UNMAP with an inline parameter list in the UCD tail.
        let command: CommandUpiu = utp.request(3);
        assert_eq!(command.cdb[0], ScsiOpcode::Unmap as u8);
        assert_eq!(command.header.flags, CommandUpiu::FLAG_WRITE);
        let expected_param = UnmapParameterList::new(8, 4);
        assert_eq!(
            utp.read_ucd(3, INLINE_DATA_OFFSET, expected_param.as_bytes().len()),
            expected_param.as_bytes()
        );

        // Each completion interrupt delivers its outcome.
        let inspector = fuchsia_inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.record_inspect(&ufs_node);
        inspector.root().record(ufs_node);

        for slot in 0..4 {
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );
        }
        assert_eq!(delivered(&mut read_rx), Ok(()));
        assert_eq!(delivered(&mut write_rx), Ok(()));
        assert_eq!(delivered(&mut flush_rx), Ok(()));
        assert_eq!(delivered(&mut trim_rx), Ok(()));

        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: {
                "block.read.count": 1u64,
                "block.write.count": 1u64,
                "block.flush.count": 1u64,
                "block.trim.count": 1u64,
                "block.read.kib": 8u64,
                "block.write.kib": 4u64,
                "block.flush.kib": 0u64,
                "block.trim.kib": 16u64,
                "auto_hibern8_error_count": 0u64,
            }
        });

        // An aborted UNMAP completes with Status::IO, releases its slot immediately, and does not
        // increment the Inspect trim counter.
        let (trim_done, mut trim_rx) = io_callback();
        device.handle_operation(&trim, None, trim_done);
        utp.complete(
            0,
            scsi_response(0, ScsiStatus::Good.as_u8()).as_bytes(),
            OverallCommandStatus::Aborted as u8,
        );
        assert_eq!(delivered(&mut trim_rx), Err(Status::IO));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        diagnostics_assertions::assert_data_tree!(inspector, root: contains {
            ufs: contains {
                "block.trim.count": 1u64,
                "block.trim.kib": 16u64,
            }
        });

        // Large LBAs (> u32::MAX) select READ(16) and WRITE(16).
        let high_lba = u64::from(u32::MAX) + 7;
        let large_unit =
            UnitDescriptor { logical_block_count: U64::new(high_lba + 8), ..Default::default() };
        let large_device = super::tests::device(3, &large_unit, Arc::clone(&processor));
        let (read16_done, mut read16_rx) = io_callback();
        large_device.handle_operation(&read(high_lba, 1, 0), Some(&vmo), read16_done);
        let (write16_done, mut write16_rx) = io_callback();
        large_device.handle_operation(&write(high_lba + 1, 1, Fua::On), Some(&vmo), write16_done);
        let read16_cmd: CommandUpiu = utp.request(0);
        assert_eq!(read16_cmd.cdb[0], ScsiOpcode::Read16 as u8);
        assert_eq!(u64::from_be_bytes(read16_cmd.cdb[2..10].try_into().unwrap()), high_lba);
        let write16_cmd: CommandUpiu = utp.request(1);
        assert_eq!(write16_cmd.cdb[0], ScsiOpcode::Write16 as u8);
        assert_ne!(write16_cmd.cdb[1] & CDB_FUA_BIT, 0);
        assert_eq!(u64::from_be_bytes(write16_cmd.cdb[2..10].try_into().unwrap()), high_lba + 1);
        utp.complete(0, scsi_response(0, ScsiStatus::Good.as_u8()).as_bytes(), OCS_SUCCESS);
        utp.complete(1, scsi_response(1, ScsiStatus::Good.as_u8()).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut read16_rx), Ok(()));
        assert_eq!(delivered(&mut write16_rx), Ok(()));
    }

    /// Drives a request through a real block session: the FIFO is served on the device's worker
    /// thread, the request completes from the (fake) interrupt path through the processor's
    /// completer, and the response arrives on the FIFO.
    #[fuchsia::test]
    async fn test_block_device_session_round_trip() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let unit = UnitDescriptor { logical_block_count: U64::new(16), ..Default::default() };
        let server = device(0, &unit, Arc::clone(&processor)).into_server();
        processor.set_io_completer(Arc::new(BlockServerCompleter::new([(0u8, &server)])));

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();
        let server_task = fasync::Task::local({
            let server = Arc::clone(&server);
            async move {
                let _ = server.handle_requests(stream).await;
            }
        });
        let (session, session_server_end) =
            fidl::endpoints::create_proxy::<fblock::SessionMarker>();
        proxy.open_session(session_server_end).unwrap();
        let fifo: zx::Fifo<BlockFifoResponse, BlockFifoRequest> =
            session.get_fifo().await.unwrap().unwrap().into();
        let vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let vmo_id = session
            .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
            .await
            .unwrap()
            .unwrap();

        let hardware = std::thread::spawn(move || {
            let mut utp = utp;
            let slot = wait_for_any_doorbell(&utp);
            let command: CommandUpiu = utp.request(slot);
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );
            command
        });
        fifo.write(&[BlockFifoRequest {
            command: BlockFifoCommand {
                opcode: fblock::BlockOpcode::Read.into_primitive(),
                ..Default::default()
            },
            reqid: 7,
            vmoid: vmo_id.id,
            length: 1,
            vmo_offset: 0,
            dev_offset: 3,
            ..Default::default()
        }])
        .unwrap();

        fasync::OnSignals::new(&fifo, zx::Signals::FIFO_READABLE).await.unwrap();
        let mut response = [BlockFifoResponse::default()];
        fifo.read(&mut response).unwrap();
        assert_eq!(response[0].reqid, 7);
        assert_eq!(response[0].status, zx::sys::ZX_OK);
        let command = hardware.join().unwrap();
        assert_eq!(command.cdb[0], ScsiOpcode::Read10 as u8);
        assert_eq!(u32::from_be_bytes(command.cdb[2..6].try_into().unwrap()), 3);

        drop(session);
        drop(proxy);
        server_task.await;
    }

    /// Reads through `DefaultCallbackBlockService` on an off-executor worker thread (`unblock`),
    /// matching `block_server` mapper threads: `on_requests` blocks inline for slot pushback when
    /// all I/O slots are busy and completes through `BlockServerCompleter` once a slot frees.
    #[fuchsia::test]
    async fn test_read_blocks_via_unblock_on_callback_service() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let unit = UnitDescriptor { logical_block_count: U64::new(16), ..Default::default() };
        let device = Arc::new(device(0, &unit, Arc::clone(&processor)));
        let session_manager = Arc::new(SessionManager::new(Arc::clone(&device), 4096));
        let server = Arc::new(BlockServer::new(4096, Arc::clone(&session_manager)));
        processor.set_io_completer(Arc::new(BlockServerCompleter::new([(0u8, &server)])));
        let service = session_manager.into_block_service(&session_manager);

        for _ in 0..ADMIN_SLOT {
            let (flush_done, _flush_rx) = io_callback();
            device.handle_operation(&Operation::Flush, None, flush_done);
        }
        let (done_tx, done_rx) = futures::channel::oneshot::channel();
        let submit_task = fasync::unblock(move || {
            let buffer = service.allocate_buffer(4096);
            service
                .read_blocks(
                    3 * 4096,
                    buffer,
                    Box::new(move |result| {
                        let _ = done_tx.send(result.is_ok());
                    }),
                )
                .unwrap();
        });

        let hardware = std::thread::spawn(move || {
            let mut utp = utp;
            utp.complete(0, scsi_response(0, ScsiStatus::Good.as_u8()).as_bytes(), OCS_SUCCESS);
            assert!(utp.wait_for_doorbell(0));
            let command: CommandUpiu = utp.request(0);
            for slot in 0..ADMIN_SLOT {
                utp.complete(
                    slot,
                    scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                    OCS_SUCCESS,
                );
            }
            command
        });
        submit_task.await;
        assert!(done_rx.await.unwrap());
        let command = hardware.join().unwrap();
        assert_eq!(command.cdb[0], ScsiOpcode::Read10 as u8);
        assert_eq!(u32::from_be_bytes(command.cdb[2..6].try_into().unwrap()), 3);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_block_device_sub_kib_io_metrics_accumulation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let unit = UnitDescriptor {
            logical_block_count: U64::new(16),
            provisioning_type: PROVISIONING_TYPE_THIN,
            ..Default::default()
        };
        let device =
            Device::new(0, 512, &unit, Device::device_flags(&unit), Arc::clone(&processor));

        let inspector = fuchsia_inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.record_inspect(&ufs_node);
        inspector.root().record(ufs_node);

        let mut submit_and_complete = |operation: &Operation, status: u8| {
            let (done, mut rx) = io_callback();
            let vmo_arg = matches!(operation, Operation::Read { .. } | Operation::Write { .. })
                .then_some(&vmo);
            device.handle_operation(operation, vmo_arg, done);
            utp.complete(0, scsi_response(0, ScsiStatus::Good.as_u8()).as_bytes(), status);
            delivered(&mut rx)
        };
        let trim_one_block = Operation::Trim { device_block_offset: 2, block_count: 1 };

        // First 512-byte Read/Write/Trim + Flush: counts increment, truncating KiB stays 0.
        assert_eq!(submit_and_complete(&read(0, 1, 0), OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&write(1, 1, Fua::Off), OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&Operation::Flush, OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&trim_one_block, OCS_SUCCESS), Ok(()));
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: contains {
                "block.read.count": 1u64,
                "block.write.count": 1u64,
                "block.flush.count": 1u64,
                "block.trim.count": 1u64,
                "block.read.kib": 0u64,
                "block.write.kib": 0u64,
                "block.flush.kib": 0u64,
                "block.trim.kib": 0u64,
            }
        });

        // Failed 512-byte ops must not increment counts or advance sub-KiB byte accumulators
        // across the 1024-byte threshold.
        let aborted = OverallCommandStatus::Aborted as u8;
        assert_eq!(submit_and_complete(&read(0, 1, 0), aborted), Err(Status::IO));
        assert_eq!(submit_and_complete(&write(1, 1, Fua::Off), aborted), Err(Status::IO));
        assert_eq!(submit_and_complete(&trim_one_block, aborted), Err(Status::IO));
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: contains {
                "block.read.count": 1u64,
                "block.write.count": 1u64,
                "block.flush.count": 1u64,
                "block.trim.count": 1u64,
                "block.read.kib": 0u64,
                "block.write.kib": 0u64,
                "block.flush.kib": 0u64,
                "block.trim.kib": 0u64,
            }
        });

        // Second successful 512-byte Read/Write/Trim + Flush reaches 1024 B -> 1 KiB (Flush -> 0).
        assert_eq!(submit_and_complete(&read(0, 1, 0), OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&write(1, 1, Fua::Off), OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&Operation::Flush, OCS_SUCCESS), Ok(()));
        assert_eq!(submit_and_complete(&trim_one_block, OCS_SUCCESS), Ok(()));
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: contains {
                "block.read.count": 2u64,
                "block.write.count": 2u64,
                "block.flush.count": 2u64,
                "block.trim.count": 2u64,
                "block.read.kib": 1u64,
                "block.write.kib": 1u64,
                "block.flush.kib": 0u64,
                "block.trim.kib": 1u64,
            }
        });

        // Non-multiple remainder (3 * 512 = 1536 B -> 1 KiB, then +512 B = 2048 B -> 2 KiB).
        assert_eq!(submit_and_complete(&read(0, 1, 0), OCS_SUCCESS), Ok(()));
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: contains { "block.read.count": 3u64, "block.read.kib": 1u64 }
        });
        assert_eq!(submit_and_complete(&read(0, 1, 0), OCS_SUCCESS), Ok(()));
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: contains { "block.read.count": 4u64, "block.read.kib": 2u64 }
        });
    }
}
