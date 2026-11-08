// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `fuchsia.hardware.rpmb/Rpmb` server for the Replay Protected Memory Block well-known LU
//! (W-LUN `0xC4`, UFS 3.1/4.0 section 12). Copies 512-byte frames through a staging VMO, sending
//! `tx_frames` in chunks of up to [`MAX_FRAMES_PER_TRANSFER`] frames per `SECURITY PROTOCOL OUT`
//! (`0xB5`) command and receiving `rx_frames` in a single `SECURITY PROTOCOL IN` (`0xA2`,
//! protocol `0xEC`, specific `0x0001`) command.

use crate::constants::{
    COPY_CHUNK_SIZE, DEFAULT_RPMB_SIZE_UNITS, MAX_FRAMES_PER_TRANSFER,
    MAX_RPMB_LOGICAL_BLOCK_SIZE_SHIFT, MAX_RPMB_RELIABLE_WRITE_SECTORS, MAX_RPMB_TRANSFER_SIZE,
    MIN_RPMB_LOGICAL_BLOCK_SIZE_SHIFT, RPMB_DEFAULT_BLOCK_SIZE, RPMB_FRAME_SIZE, RPMB_LUN,
    RPMB_SIZE_UNIT, STAGING_VMO_SIZE,
};
use crate::descriptors::RpmbUnitDescriptor;
use crate::device_manager::DeviceManager;
use crate::request_list::DataDirection;
use crate::scsi::ScsiOpcode;
use crate::transfer_request_processor::{DataTransfer, TransferRequestProcessor};
use crate::{DriverMmio, test_unit_ready};
use fidl_fuchsia_hardware_rpmb as frpmb;
use futures::StreamExt;
use futures::lock::Mutex as AsyncMutex;
use log::{info, warn};
use mmio::Mmio;
use std::sync::Arc;

/// Server for the `fuchsia.hardware.rpmb/Rpmb` protocol on UFS well-known LU 0xC4.
pub struct UfsRpmbServer<M: Mmio = DriverMmio> {
    processor: Arc<TransferRequestProcessor<M>>,
    /// Static device info, read once at creation.
    device_info: frpmb::DeviceInfo,
    /// Maximum number of 512-byte frames sent or received in a single `SECURITY PROTOCOL` command,
    /// bounded by both [`MAX_FRAMES_PER_TRANSFER`] and `reliable_write_sector_count`.
    max_frames_per_transfer: u64,
    /// Staging VMO for RPMB frames, pinned by the processor per command. The lock is held for a
    /// whole request so the OUT/IN sequences of concurrent clients do not interleave.
    staging_vmo: AsyncMutex<Arc<zx::Vmo>>,
}

impl<M: Mmio> std::fmt::Debug for UfsRpmbServer<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UfsRpmbServer")
            .field("device_info", &self.device_info)
            .field("max_frames_per_transfer", &self.max_frames_per_transfer)
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> UfsRpmbServer<M> {
    /// Creates an RPMB server and caches its `DeviceInfo`, derived from the RPMB Unit Descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error if allocating the staging VMO fails.
    pub async fn new(
        device_manager: &DeviceManager,
        processor: Arc<TransferRequestProcessor<M>>,
    ) -> Result<Self, zx::Status> {
        let staging_vmo = Arc::new(zx::Vmo::create(STAGING_VMO_SIZE)?);
        let (device_info, max_frames_per_transfer) =
            Self::read_device_info(device_manager, &processor).await;
        Ok(Self {
            processor,
            device_info,
            max_frames_per_transfer,
            staging_vmo: AsyncMutex::new(staging_vmo),
        })
    }

    /// Sends TEST UNIT READY to the RPMB well-known LU, retrying to clear any pending Unit
    /// Attention. Returns whether the LU is ready.
    pub async fn probe_rpmb_lun(processor: &TransferRequestProcessor<M>) -> bool {
        match test_unit_ready(processor, RPMB_LUN).await {
            Ok(()) => {
                info!(lun = RPMB_LUN; "UFS RPMB well-known LU ready");
                true
            }
            Err(e) => {
                warn!(lun = RPMB_LUN, err:? = e; "UFS RPMB well-known LU not ready");
                false
            }
        }
    }

    /// Serves incoming `fuchsia.hardware.rpmb/Rpmb` FIDL requests until the stream closes or
    /// returns an error.
    pub async fn serve(self: Arc<Self>, mut stream: frpmb::RpmbRequestStream) {
        while let Some(Ok(request)) = stream.next().await {
            match request {
                frpmb::RpmbRequest::GetDeviceInfo { responder } => {
                    let _ = responder.send(&self.device_info);
                }
                frpmb::RpmbRequest::Request { request, responder } => {
                    let res = self.handle_rpmb_request(request).await;
                    let _ = responder.send(res.map_err(|s| s.into_raw()));
                }
            }
        }
    }

    /// Builds the `DeviceInfo` reported by `GetDeviceInfo` and the per-command frame bound from
    /// the RPMB Unit and Geometry Descriptors. Sets `cid` to zeros, `rpmb_size` in 128 KiB units
    /// (or [`DEFAULT_RPMB_SIZE_UNITS`] on fallback), and `reliable_write_sector_count` to
    /// `bRPMB_ReadWriteSize` bounded by [`MAX_RPMB_RELIABLE_WRITE_SECTORS`] and the Geometry
    /// `bMaxOutBufferSize`/`bMaxInBufferSize` (in 512-byte units).
    async fn read_device_info(
        device_manager: &DeviceManager,
        processor: &TransferRequestProcessor<M>,
    ) -> (frpmb::DeviceInfo, u64) {
        let geo = device_manager.geometry_descriptor();
        let max_out = if geo.max_out_buffer_size > 0 {
            geo.max_out_buffer_size
        } else {
            MAX_RPMB_RELIABLE_WRITE_SECTORS
        };
        let max_in = if geo.max_in_buffer_size > 0 {
            geo.max_in_buffer_size
        } else {
            MAX_RPMB_RELIABLE_WRITE_SECTORS
        };
        let reliable_write_sector_count = geo
            .rpmb_read_write_size
            .max(1)
            .min(MAX_RPMB_RELIABLE_WRITE_SECTORS)
            .min(max_out)
            .min(max_in);
        let max_frames_per_transfer =
            MAX_FRAMES_PER_TRANSFER.min(u64::from(reliable_write_sector_count));
        let rpmb_size = match device_manager
            .read_descriptor::<_, RpmbUnitDescriptor>(processor, RPMB_LUN)
            .await
        {
            Ok(rpmb_desc) => {
                // UFS 3.1/4.0 section 14.1.4.6: RPMB `bLogicalBlockSize` is log2(bytes); valid
                // exponent range is 8..=16 (256 B..=64 KiB, nominally 8 = 256 B).
                let block_size = match rpmb_desc.logical_block_size {
                    MIN_RPMB_LOGICAL_BLOCK_SIZE_SHIFT..=MAX_RPMB_LOGICAL_BLOCK_SIZE_SHIFT => {
                        1u64 << rpmb_desc.logical_block_size
                    }
                    _ => RPMB_DEFAULT_BLOCK_SIZE,
                };
                let size_units =
                    rpmb_desc.logical_block_count.get().saturating_mul(block_size) / RPMB_SIZE_UNIT;
                u8::try_from(size_units)
                    .ok()
                    .filter(|&size| size > 0)
                    .unwrap_or(DEFAULT_RPMB_SIZE_UNITS)
            }
            Err(e) => {
                warn!(err:? = e; "Failed to read RPMB Unit Descriptor, reporting default size");
                DEFAULT_RPMB_SIZE_UNITS
            }
        };
        let device_info = frpmb::DeviceInfo::EmmcInfo(frpmb::EmmcDeviceInfo {
            cid: [0u8; 16],
            rpmb_size,
            reliable_write_sector_count,
        });
        (device_info, max_frames_per_transfer)
    }

    /// Validates that `[offset, offset + size)` lies within `vmo` and holds 1 to 16 whole RPMB
    /// frames.
    fn validate_frame_range(vmo: &zx::Vmo, offset: u64, size: u64) -> Result<(), zx::Status> {
        if size < RPMB_FRAME_SIZE || size % RPMB_FRAME_SIZE != 0 {
            warn!(offset, size; "Invalid RPMB frame size");
            return Err(zx::Status::INVALID_ARGS);
        }
        if size > MAX_RPMB_TRANSFER_SIZE {
            warn!(offset, size, max = MAX_RPMB_TRANSFER_SIZE; "RPMB transfer exceeds maximum size");
            return Err(zx::Status::OUT_OF_RANGE);
        }
        let end = offset.checked_add(size).ok_or_else(|| {
            warn!(offset, size; "RPMB frame range offset overflow");
            zx::Status::OUT_OF_RANGE
        })?;
        let vmo_size = vmo.get_size()?;
        if end > vmo_size {
            warn!(offset, size, vmo_size; "RPMB frame range exceeds VMO size");
            return Err(zx::Status::OUT_OF_RANGE);
        }
        Ok(())
    }

    /// Copies `size` bytes from `src` to `dst` through a [`COPY_CHUNK_SIZE`] stack buffer.
    fn copy_vmo(
        src: &zx::Vmo,
        src_offset: u64,
        dst: &zx::Vmo,
        dst_offset: u64,
        size: u64,
    ) -> Result<(), zx::Status> {
        let mut buf = [0u8; COPY_CHUNK_SIZE];
        let mut copied = 0u64;
        while copied < size {
            let chunk = (size - copied).min(buf.len() as u64) as usize;
            src.read(&mut buf[..chunk], src_offset + copied).map_err(|status| {
                warn!(status:%, src_offset = src_offset + copied, chunk; "RPMB VMO read failed");
                status
            })?;
            dst.write(&buf[..chunk], dst_offset + copied).map_err(|status| {
                warn!(status:%, dst_offset = dst_offset + copied, chunk; "RPMB VMO write failed");
                status
            })?;
            copied += chunk as u64;
        }
        Ok(())
    }

    /// Sends `request.tx_frames` to the device in chunks of up to `self.max_frames_per_transfer`
    /// frames per `SECURITY PROTOCOL OUT` command and, if `rx_frames` is present and non-empty,
    /// reads the response frames back in a single `SECURITY PROTOCOL IN` command.
    ///
    /// Chunking `tx_frames` at [`MAX_FRAMES_PER_TRANSFER`] (2 frames, matching the C++ driver's
    /// `rpmb.cc`) is required because `trusty_storage_proxy` concatenates up to 2 `Authenticated
    /// Data Write Request` frames (`reliable_write_size = 1024`) and 1 `Result Read Request` frame
    /// (`write_size = 512`) into a single 3-frame `tx_frames` VMO range, which JEDEC UFS 3.1/4.0
    /// section 12.4.4.1 requires to be sent as two separate `SECURITY PROTOCOL OUT` commands.
    /// Conversely, `rx_frames` is received in a single `SECURITY PROTOCOL IN` command so
    /// multi-frame reads follow one `SECURITY PROTOCOL OUT` request (UFS 3.1/4.0 section 12.4.4.2).
    async fn handle_rpmb_request(&self, request: frpmb::Request) -> Result<(), zx::Status> {
        let tx_size = request.tx_frames.size;
        Self::validate_frame_range(&request.tx_frames.vmo, request.tx_frames.offset, tx_size)?;

        // An empty rx buffer means the client expects no response frames.
        let rx_frames = request.rx_frames.as_ref().filter(|rx| rx.size > 0);
        if let Some(rx_frames) = rx_frames {
            Self::validate_frame_range(&rx_frames.vmo, rx_frames.offset, rx_frames.size)?;
        }

        let staging_vmo = self.staging_vmo.lock().await;

        // 1. Transmit tx_frames via SCSI SECURITY PROTOCOL OUT (0xB5) in chunks of up to
        //    max_frames_per_transfer frames (2 frames = 1 KiB).
        let tx_frame_count = tx_size / RPMB_FRAME_SIZE;
        let mut frames_sent = 0u64;

        while frames_sent < tx_frame_count {
            let frames_to_send =
                std::cmp::min(tx_frame_count - frames_sent, self.max_frames_per_transfer);
            let chunk_size = (frames_to_send * RPMB_FRAME_SIZE) as u32;
            let chunk_offset = frames_sent * RPMB_FRAME_SIZE;

            Self::copy_vmo(
                &request.tx_frames.vmo,
                request.tx_frames.offset + chunk_offset,
                &staging_vmo,
                0,
                chunk_size as u64,
            )?;

            let cdb =
                ScsiOpcode::build_security_protocol_cdb(DataDirection::HostToDevice, chunk_size);
            self.processor
                .send_scsi_command(
                    RPMB_LUN,
                    cdb,
                    Some(DataTransfer::write(Arc::clone(&staging_vmo), 0, chunk_size)),
                )
                .await
                .map_err(|e| {
                    warn!(err:% = e, chunk_size; "RPMB SECURITY PROTOCOL OUT failed");
                    zx::Status::from(e)
                })?;

            frames_sent += frames_to_send;
        }

        // 2. Receive rx_frames (if requested) in a single SCSI SECURITY PROTOCOL IN (0xA2) command.
        if let Some(rx_frames) = rx_frames {
            let rx_bytes = rx_frames.size as u32;
            let cdb =
                ScsiOpcode::build_security_protocol_cdb(DataDirection::DeviceToHost, rx_bytes);
            self.processor
                .send_scsi_command(
                    RPMB_LUN,
                    cdb,
                    Some(DataTransfer::read(Arc::clone(&staging_vmo), 0, rx_bytes)),
                )
                .await
                .map_err(|e| {
                    warn!(err:% = e, rx_bytes; "RPMB SECURITY PROTOCOL IN failed");
                    zx::Status::from(e)
                })?;

            Self::copy_vmo(&staging_vmo, 0, &rx_frames.vmo, rx_frames.offset, rx_frames.size)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::ScsiStatus;
    use crate::testing::fake_ufs::{FakeMmio, FakeUfs};
    use crate::testing::{
        OCS_SUCCESS, PAGE_SIZE, create_test_device_manager, create_test_processor, fake_utp,
        scsi_response, wait_for_any_doorbell,
    };
    use crate::upiu::CommandUpiu;
    use fidl_fuchsia_mem::Range;
    use fuchsia_async as fasync;
    use zerocopy::IntoBytes;

    #[fuchsia::test]
    fn test_rpmb_validate_frame_range() {
        type Rpmb = UfsRpmbServer<FakeMmio>;

        let vmo = zx::Vmo::create(4 * PAGE_SIZE).unwrap();
        assert_eq!(Rpmb::validate_frame_range(&vmo, 0, RPMB_FRAME_SIZE), Ok(()));
        assert_eq!(Rpmb::validate_frame_range(&vmo, 0, MAX_RPMB_TRANSFER_SIZE), Ok(()));
        assert_eq!(Rpmb::validate_frame_range(&vmo, 0, 0), Err(zx::Status::INVALID_ARGS));
        assert_eq!(
            Rpmb::validate_frame_range(&vmo, 0, RPMB_FRAME_SIZE + 1),
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(
            Rpmb::validate_frame_range(&vmo, 0, MAX_RPMB_TRANSFER_SIZE + RPMB_FRAME_SIZE),
            Err(zx::Status::OUT_OF_RANGE)
        );
        let end = 4 * PAGE_SIZE;
        assert_eq!(
            Rpmb::validate_frame_range(&vmo, end - RPMB_FRAME_SIZE, RPMB_FRAME_SIZE),
            Ok(())
        );
        assert_eq!(
            Rpmb::validate_frame_range(&vmo, end - RPMB_FRAME_SIZE / 2, RPMB_FRAME_SIZE),
            Err(zx::Status::OUT_OF_RANGE)
        );
        assert_eq!(
            Rpmb::validate_frame_range(&vmo, u64::MAX - RPMB_FRAME_SIZE / 2, RPMB_FRAME_SIZE),
            Err(zx::Status::OUT_OF_RANGE)
        );
    }

    #[fuchsia::test]
    async fn test_rpmb_probe_and_serve_request() {
        let mut fake_ufs = FakeUfs::new();
        fake_ufs.geometry_desc.rpmb_read_write_size = 4;
        let device_manager = create_test_device_manager(&fake_ufs);
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            let mut utp = utp;
            // 1. probe_rpmb_lun issues TEST UNIT READY on RPMB_LUN.
            let slot = wait_for_any_doorbell(&utp);
            let cmd: CommandUpiu = utp.request(slot);
            assert_eq!(cmd.header.lun, RPMB_LUN);
            assert_eq!(cmd.cdb[0], ScsiOpcode::TestUnitReady as u8);
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );

            // 2. UfsRpmbServer::new reads the RPMB Unit Descriptor via Query Request on ADMIN_SLOT;
            //    answer with a non-zero response to exercise the DEFAULT_RPMB_SIZE_UNITS fallback.
            let slot = wait_for_any_doorbell(&utp);
            let mut query_resp: crate::query::QueryUpiu = utp.request(slot);
            query_resp.header.trans_type = crate::upiu::UpiuTransactionCode::QueryResponse as u8;
            query_resp.header.response = 1;
            utp.complete(slot, query_resp.as_bytes(), OCS_SUCCESS);

            // 3. handle_rpmb_request splits a 3-frame tx_frames buffer into a 2-frame
            //    SECURITY PROTOCOL OUT command (Authenticated Data Write) followed by a 1-frame
            //    SECURITY PROTOCOL OUT command (Result Read Request), then issues a single 2-frame
            //    SECURITY PROTOCOL IN command.
            let mut out_lens = Vec::new();
            for _ in 0..2 {
                let slot = wait_for_any_doorbell(&utp);
                let cmd: CommandUpiu = utp.request(slot);
                assert_eq!(cmd.header.lun, RPMB_LUN);
                assert_eq!(cmd.cdb[0], ScsiOpcode::SecurityProtocolOut as u8);
                out_lens.push(cmd.expected_data_transfer_length.get());
                utp.complete(
                    slot,
                    scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                    OCS_SUCCESS,
                );
            }

            let slot = wait_for_any_doorbell(&utp);
            let cmd: CommandUpiu = utp.request(slot);
            assert_eq!(cmd.header.lun, RPMB_LUN);
            assert_eq!(cmd.cdb[0], ScsiOpcode::SecurityProtocolIn as u8);
            let in_len = cmd.expected_data_transfer_length.get();
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );
            (out_lens, in_len)
        });

        assert!(UfsRpmbServer::probe_rpmb_lun(&processor).await);
        let server = Arc::new(UfsRpmbServer::new(&device_manager, processor).await.unwrap());
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<frpmb::RpmbMarker>();
        let server_task = fasync::Task::local(server.serve(stream));

        let info = proxy.get_device_info().await.unwrap();
        match info {
            frpmb::DeviceInfo::EmmcInfo(emmc) => {
                assert_eq!(emmc.rpmb_size, DEFAULT_RPMB_SIZE_UNITS);
                assert_eq!(emmc.reliable_write_sector_count, 4);
            }
            other => panic!("unexpected DeviceInfo: {other:?}"),
        };

        let tx_vmo = zx::Vmo::create(4 * PAGE_SIZE).unwrap();
        let rx_vmo = zx::Vmo::create(4 * PAGE_SIZE).unwrap();
        let req = frpmb::Request {
            tx_frames: Range { vmo: tx_vmo, offset: 0, size: 3 * RPMB_FRAME_SIZE },
            rx_frames: Some(Box::new(Range { vmo: rx_vmo, offset: 0, size: 2 * RPMB_FRAME_SIZE })),
        };
        assert_eq!(proxy.request(req).await.unwrap(), Ok(()));
        let (out_lens, in_len) = hardware.join().unwrap();
        assert_eq!(out_lens, vec![2 * RPMB_FRAME_SIZE as u32, RPMB_FRAME_SIZE as u32]);
        assert_eq!(in_len, 2 * RPMB_FRAME_SIZE as u32);
        drop(proxy);
        server_task.await;
    }
}
