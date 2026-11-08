// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `fuchsia.hardware.ufs/Ufs` server: descriptor, flag and attribute queries, and SCSI READ/WRITE
//! BUFFER.
//!
//! Clients are non-driver components, so writes are restricted to safe allowlists.

use crate::constants::{
    DESCRIPTOR_HEADER_SIZE, DWORD_ALIGN_MASK, MAX_DESCRIPTOR_LENGTH, MAX_PRDT_ENTRIES,
};
use crate::query::{QueryOpcode, QueryUpiu};
use crate::request_list::DataDirection;
use crate::scsi::{self, ScsiOpcode};
use crate::transfer_request_processor::{DataTransfer, TransferRequestProcessor};
use crate::{DriverMmio, UfsError};
use fidl_fuchsia_hardware_scsi as fscsi;
use fidl_fuchsia_hardware_ufs as fufs;
use futures::StreamExt;
use log::{error, warn};
use mmio::Mmio;
use std::sync::Arc;

/// Serves `fuchsia.hardware.ufs/Ufs`.
pub struct UfsServer<M: Mmio = DriverMmio> {
    processor: Arc<TransferRequestProcessor<M>>,
}

impl<M: Mmio> std::fmt::Debug for UfsServer<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UfsServer").finish_non_exhaustive()
    }
}

impl<M: Mmio> UfsServer<M> {
    /// Creates a server issuing requests through `processor`.
    pub fn new(processor: Arc<TransferRequestProcessor<M>>) -> Self {
        Self { processor }
    }

    /// Serves `stream` until it closes; returns `IoError` on a FIDL stream error.
    pub async fn serve(
        self: Arc<Self>,
        mut stream: fufs::UfsRequestStream,
    ) -> Result<(), UfsError> {
        while let Some(request) = stream.next().await {
            let request = request.map_err(|e| {
                warn!(err:? = e; "FIDL UfsRequestStream error");
                UfsError::IoError
            })?;
            self.handle_request(request).await;
        }
        Ok(())
    }

    /// Returns whether clients are denied writes to `flag` (write-once, permanent or power-on
    /// write-protection, and unknown flags).
    fn is_protected_flag(flag: fufs::FlagType) -> bool {
        match flag {
            fufs::FlagType::PermanentWpEn
            | fufs::FlagType::PowerOnWpEn
            | fufs::FlagType::PhyResourceRemoval
            | fufs::FlagType::PermanentlyDisableFwUpdate
            | fufs::FlagType::__SourceBreaking { .. } => true,
            fufs::FlagType::DeviceInit
            | fufs::FlagType::BackgroundOpsEn
            | fufs::FlagType::DeviceLifeSpanModeEn
            | fufs::FlagType::PurgeEnable
            | fufs::FlagType::RefreshEnable
            | fufs::FlagType::BusyRtc
            | fufs::FlagType::WriteBoosterEn
            | fufs::FlagType::WriteBoosterBufferFlushEn
            | fufs::FlagType::WriteBoosterBufferFlushHibernate => false,
        }
    }

    /// Returns the `(index, selector)` of a query target, defaulting each to 0.
    fn index_and_selector(id: Option<&fufs::Identifier>) -> (u8, u8) {
        let index = id.and_then(|i| i.index).unwrap_or(0);
        let selector = id.and_then(|i| i.selector).unwrap_or(0);
        (index, selector)
    }

    /// Sends a query and maps a non-zero `Query Response` to the matching `QueryErrorCode`.
    async fn send_query(&self, req: &QueryUpiu) -> Result<QueryUpiu, fufs::QueryErrorCode> {
        let resp = self.processor.send_query_request_upiu(req).await.map_err(|err| {
            warn!(opcode = req.opcode, idn = req.idn, err:?; "FIDL query request transport failed");
            fufs::QueryErrorCode::GeneralFailure
        })?;
        if resp.header.response != 0 {
            warn!(
                opcode = req.opcode,
                idn = req.idn,
                response = resp.header.response;
                "FIDL query request returned error response"
            );
            return Err(fufs::QueryErrorCode::from_primitive(resp.header.response.into())
                .unwrap_or(fufs::QueryErrorCode::GeneralFailure));
        }
        Ok(resp)
    }

    /// Issues a flag query; write opcodes on protected flags are rejected locally.
    async fn handle_flag_query(
        &self,
        flag: fufs::Flag,
        opcode: QueryOpcode,
    ) -> Result<u8, fufs::QueryErrorCode> {
        let flag_type = flag.type_.ok_or(fufs::QueryErrorCode::InvalidIdn)?;
        if opcode != QueryOpcode::ReadFlag && Self::is_protected_flag(flag_type) {
            warn!(opcode:?, flag_type:?; "Refusing write to protected UFS flag");
            return Err(fufs::QueryErrorCode::ParameterNotWriteable);
        }
        let (index, selector) = Self::index_and_selector(flag.identifier.as_ref());
        let req = QueryUpiu::new(opcode, flag_type.into_primitive(), index, selector);
        let resp = self.send_query(&req).await?;
        Ok(resp.flag_value() as u8)
    }

    /// Reads `descriptor.length` (defaulting to [`MAX_DESCRIPTOR_LENGTH`]) bytes of a descriptor.
    ///
    /// Unlike [`QueryUpiu::parse_descriptor`], this returns the raw slice of `min(resp.length,
    /// requested_length)` bytes without further truncating by `command_data[0]` (`bLength`), so a
    /// client that supplies an explicit `descriptor.length` receives the exact slice returned by
    /// the device up to that bound.
    async fn read_descriptor(
        &self,
        descriptor: fufs::Descriptor,
    ) -> Result<Vec<u8>, fufs::QueryErrorCode> {
        let desc_type = descriptor.type_.ok_or(fufs::QueryErrorCode::InvalidIdn)?;
        if let Some(len) = descriptor.length
            && (len < DESCRIPTOR_HEADER_SIZE as u16 || len > MAX_DESCRIPTOR_LENGTH)
        {
            warn!(desc_type:?, len; "ReadDescriptor invalid length");
            return Err(fufs::QueryErrorCode::InvalidLength);
        }
        let (index, selector) = Self::index_and_selector(descriptor.identifier.as_ref());
        let length = descriptor.length.unwrap_or(MAX_DESCRIPTOR_LENGTH);
        let req =
            QueryUpiu::read_descriptor_raw(desc_type.into_primitive(), index, selector, length);
        let resp = self.send_query(&req).await?;
        let actual_length = resp.length.get().min(length) as usize;
        Ok(resp.command_data[..actual_length].to_vec())
    }

    async fn read_attribute(&self, attr: fufs::Attribute) -> Result<u32, fufs::QueryErrorCode> {
        let attr_type = attr.type_.ok_or(fufs::QueryErrorCode::InvalidIdn)?;
        let (index, selector) = Self::index_and_selector(attr.identifier.as_ref());
        let req =
            QueryUpiu::new(QueryOpcode::ReadAttribute, attr_type.into_primitive(), index, selector);
        self.send_query(&req).await.map(|resp| resp.value())
    }

    /// Writes an attribute. Only `bBootLunEn` may be written, to prevent persistent configuration
    /// corruption.
    async fn write_attribute(
        &self,
        attr: fufs::Attribute,
        value: u32,
    ) -> Result<(), fufs::QueryErrorCode> {
        let attr_type = attr.type_.ok_or(fufs::QueryErrorCode::InvalidIdn)?;
        if attr_type != fufs::AttributeType::BootLunEn {
            warn!(attr_type:?; "Refusing WriteAttribute on non-BootLunEn attribute");
            return Err(fufs::QueryErrorCode::ParameterNotWriteable);
        }
        let (index, selector) = Self::index_and_selector(attr.identifier.as_ref());
        let req =
            QueryUpiu::write_attribute_raw(attr_type.into_primitive(), index, selector, value);
        self.send_query(&req).await.map(|_| ())
    }

    /// Runs READ BUFFER (`DeviceToHost`) or WRITE BUFFER (`HostToDevice`) on `lun` directly
    /// against `data`.
    ///
    /// Note that `buffer_offset` is the 24-bit byte offset inside the target UFS device buffer
    /// (encoded into CDB bytes `3..6`), whereas the host `data` VMO is always accessed starting at
    /// VMO offset `0` (`DataTransfer::read/write(data, 0, length)`).
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if `buffer_offset` or `length` exceeds 24 bits, `length` exceeds
    /// the PRDT or the size of `data`, or `lun` is a regular LUN above 31; `INVALID_ARGS` if
    /// `length` is not a multiple of 4 (PRDT byte counts are dword granular) or `lun` is
    /// malformed; otherwise the VMO error or the error from the SCSI transfer.
    async fn transfer_buffer(
        &self,
        direction: DataDirection,
        lun: u16,
        mode: u8,
        buffer_id: u8,
        buffer_offset: u32,
        length: u32,
        data: zx::Vmo,
    ) -> Result<(), zx::Status> {
        let page_size = u64::from(zx::system_get_page_size());
        if buffer_offset > fscsi::U24_MAX
            || length > fscsi::U24_MAX
            || u64::from(length) > MAX_PRDT_ENTRIES as u64 * page_size
            || data.get_size()? < u64::from(length)
        {
            warn!(lun, buffer_offset, length; "transfer_buffer out of range");
            return Err(zx::Status::OUT_OF_RANGE);
        }
        if u64::from(length) & DWORD_ALIGN_MASK != 0 {
            warn!(lun, length; "transfer_buffer length not dword-aligned");
            return Err(zx::Status::INVALID_ARGS);
        }
        let lun = scsi::ufs_lun_from_scsi_lun(lun)?;
        let cdb = ScsiOpcode::build_buffer_cdb(direction, mode, buffer_id, buffer_offset, length);
        let data = Arc::new(data);
        let transfer = match direction {
            DataDirection::HostToDevice => DataTransfer::write(data, 0, length),
            DataDirection::DeviceToHost | DataDirection::NoData => {
                DataTransfer::read(data, 0, length)
            }
        };
        self.processor.send_scsi_command(lun, cdb, Some(transfer)).await.map_err(|e| {
            warn!(lun, mode, buffer_id, length, err:% = e; "transfer_buffer SCSI command failed");
            zx::Status::from(e)
        })?;
        Ok(())
    }

    /// Dispatches one `fuchsia.hardware.ufs/Ufs` request.
    async fn handle_request(&self, request: fufs::UfsRequest) {
        // Reply errors mean the client went away; there is nothing to do about them.
        match request {
            fufs::UfsRequest::ReadDescriptor { descriptor, responder } => {
                let result = self.read_descriptor(descriptor).await;
                let _ = responder.send(result.as_deref().map_err(|e| *e));
            }
            fufs::UfsRequest::WriteDescriptor { descriptor, data: _, responder } => {
                // Descriptors are read-only to prevent destructive partition or configuration
                // changes.
                warn!(desc_type:? = descriptor.type_; "Refusing WriteDescriptor request");
                let result = match descriptor.type_ {
                    None => fufs::QueryErrorCode::InvalidIdn,
                    Some(_) => fufs::QueryErrorCode::ParameterNotWriteable,
                };
                let _ = responder.send(Err(result));
            }
            fufs::UfsRequest::ReadFlag { flag, responder } => {
                let _ = responder.send(self.handle_flag_query(flag, QueryOpcode::ReadFlag).await);
            }
            fufs::UfsRequest::SetFlag { flag, responder } => {
                let _ = responder.send(self.handle_flag_query(flag, QueryOpcode::SetFlag).await);
            }
            fufs::UfsRequest::ClearFlag { flag, responder } => {
                let _ = responder.send(self.handle_flag_query(flag, QueryOpcode::ClearFlag).await);
            }
            fufs::UfsRequest::ToggleFlag { flag, responder } => {
                let _ = responder.send(self.handle_flag_query(flag, QueryOpcode::ToggleFlag).await);
            }
            fufs::UfsRequest::ReadAttribute { attr, responder } => {
                let _ = responder.send(self.read_attribute(attr).await);
            }
            fufs::UfsRequest::WriteAttribute { attr, value, responder } => {
                let _ = responder.send(self.write_attribute(attr, value).await);
            }
            fufs::UfsRequest::SendUicCommand { opcode, responder, .. } => {
                warn!(opcode:?; "Refusing external SendUicCommand (not supported)");
                let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
            }
            fufs::UfsRequest::ReadBuffer {
                lun,
                mode,
                buffer_id,
                buffer_offset,
                length,
                data,
                responder,
            } => {
                let result = self
                    .transfer_buffer(
                        DataDirection::DeviceToHost,
                        lun,
                        mode.into_primitive(),
                        buffer_id,
                        buffer_offset,
                        length,
                        data,
                    )
                    .await;
                let _ = responder.send(result.map_err(zx::Status::into_raw));
            }
            fufs::UfsRequest::WriteBuffer {
                lun,
                mode,
                buffer_id,
                buffer_offset,
                length,
                data,
                responder,
            } => {
                // Microcode download (FFU) and vendor modes are refused.
                // TODO(https://fxbug.dev/555277260): Route FFU through a separate privileged
                // capability.
                let result = if mode != fscsi::WriteBufferMode::Data {
                    error!(mode = mode.into_primitive(); "WriteBuffer refusing unsupported mode");
                    Err(zx::Status::ACCESS_DENIED)
                } else {
                    self.transfer_buffer(
                        DataDirection::HostToDevice,
                        lun,
                        mode.into_primitive(),
                        buffer_id,
                        buffer_offset,
                        length,
                        data,
                    )
                    .await
                };
                let _ = responder.send(result.map_err(zx::Status::into_raw));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        PAGE_SIZE, TestProcessor, answer_queries, create_test_processor, fake_utp,
    };
    use zerocopy::byteorder::big_endian::U32;

    /// Serves `fuchsia.hardware.ufs/Ufs` on `processor` from a local task.
    fn serve_ufs(
        processor: Arc<TestProcessor>,
    ) -> (fidl_fuchsia_hardware_ufs::UfsProxy, fuchsia_async::Task<()>) {
        let server = Arc::new(UfsServer::new(processor));
        let (client, stream) =
            fidl::endpoints::create_proxy_and_stream::<fidl_fuchsia_hardware_ufs::UfsMarker>();
        let task = fuchsia_async::Task::local(async move {
            let _ = server.serve(stream).await;
        });
        (client, task)
    }

    #[fuchsia::test]
    async fn test_ufs_server_security_allowlists() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let (client, _server_task) = serve_ufs(processor);

        // 1. Reject writing Configuration Descriptor; a missing type is an invalid IDN.
        let desc = fufs::Descriptor {
            type_: Some(fufs::DescriptorType::Configuration),
            ..Default::default()
        };
        assert_eq!(
            client.write_descriptor(&desc, &[0u8; 16]).await.unwrap(),
            Err(fufs::QueryErrorCode::ParameterNotWriteable)
        );
        assert_eq!(
            client.write_descriptor(&fufs::Descriptor::default(), &[0u8; 16]).await.unwrap(),
            Err(fufs::QueryErrorCode::InvalidIdn)
        );

        // 2. Reject writing PermanentWpEn and PowerOnWpEn flags while allowing ReadFlag.
        for protected_type in [fufs::FlagType::PermanentWpEn, fufs::FlagType::PowerOnWpEn] {
            let flag = fufs::Flag { type_: Some(protected_type), ..Default::default() };
            assert_eq!(
                client.set_flag(&flag).await.unwrap(),
                Err(fufs::QueryErrorCode::ParameterNotWriteable)
            );
            assert_eq!(
                client.clear_flag(&flag).await.unwrap(),
                Err(fufs::QueryErrorCode::ParameterNotWriteable)
            );
            assert_eq!(
                client.toggle_flag(&flag).await.unwrap(),
                Err(fufs::QueryErrorCode::ParameterNotWriteable)
            );
        }
        assert_eq!(
            client.set_flag(&fufs::Flag::default()).await.unwrap(),
            Err(fufs::QueryErrorCode::InvalidIdn)
        );
        let hardware = answer_queries(utp, 1, |response| {
            response.value = U32::new(1);
        });
        let power_on_wp_flag =
            fufs::Flag { type_: Some(fufs::FlagType::PowerOnWpEn), ..Default::default() };
        assert_eq!(client.read_flag(&power_on_wp_flag).await.unwrap(), Ok(1));
        hardware.join().unwrap();

        // 3. Reject writing non-BootLunEn attributes; a missing type is an invalid IDN.
        let attr = fufs::Attribute {
            type_: Some(fufs::AttributeType::ConfigDescrLock),
            ..Default::default()
        };
        assert_eq!(
            client.write_attribute(&attr, 1).await.unwrap(),
            Err(fufs::QueryErrorCode::ParameterNotWriteable)
        );
        assert_eq!(
            client.write_attribute(&fufs::Attribute::default(), 1).await.unwrap(),
            Err(fufs::QueryErrorCode::InvalidIdn)
        );

        // 4. WriteBuffer is limited to Data mode.
        let vmo = || zx::Vmo::create(PAGE_SIZE).unwrap();
        for mode in
            [fscsi::WriteBufferMode::VendorSpecific, fscsi::WriteBufferMode::FfuOffsetSaveDefer]
        {
            let result = client.write_buffer(0, mode, 0, 0, 4, vmo()).await.unwrap();
            assert_eq!(result, Err(zx::Status::ACCESS_DENIED.into_raw()), "{mode:?}");
        }

        // 5. ReadBuffer argument validation.
        let data = fscsi::ReadBufferMode::Data;
        let read_buffer =
            |lun, offset, length| client.read_buffer(lun, data, 0, offset, length, vmo());
        let out_of_range = Err(zx::Status::OUT_OF_RANGE.into_raw());
        assert_eq!(read_buffer(0, 0, 6).await.unwrap(), Err(zx::Status::INVALID_ARGS.into_raw()));
        assert_eq!(read_buffer(0, 0, 8192).await.unwrap(), out_of_range);
        assert_eq!(read_buffer(0, 1 << 24, 4).await.unwrap(), out_of_range);
        assert_eq!(read_buffer(32, 0, 4).await.unwrap(), out_of_range);
    }

    #[fuchsia::test]
    async fn test_ufs_server_send_uic_command_not_supported() {
        let (_mmio_vmo, processor) = create_test_processor();
        let (client, _server_task) = serve_ufs(processor);
        for opcode in [
            fufs::UicCommandOpcode::DmeGet,
            fufs::UicCommandOpcode::DmeSet,
            fufs::UicCommandOpcode::DmePeerGet,
            fufs::UicCommandOpcode::DmePeerSet,
            fufs::UicCommandOpcode::DmeLinkStartup,
            fufs::UicCommandOpcode::DmeHiberEnter,
        ] {
            assert_eq!(
                client.send_uic_command(opcode, &[0, 0, 0]).await.unwrap(),
                Err(zx::Status::NOT_SUPPORTED.into_raw()),
                "{opcode:?}"
            );
        }
    }

    #[fuchsia::test]
    async fn test_ufs_server_maps_query_response() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let (client, _server_task) = serve_ufs(processor);

        let hardware = answer_queries(utp, 3, |response| {
            response.header.response = match response.opcode {
                op if op == QueryOpcode::ReadFlag as u8 => 0xF6,
                op if op == QueryOpcode::ReadAttribute as u8 => 0x42,
                _ => 0,
            };
            response.value = U32::new(0x33);
        });
        let flag = fufs::Flag { type_: Some(fufs::FlagType::DeviceInit), ..Default::default() };
        assert_eq!(
            client.read_flag(&flag).await.unwrap(),
            Err(fufs::QueryErrorCode::ParameterNotReadable)
        );
        // Unknown response codes map to GeneralFailure.
        let attr =
            fufs::Attribute { type_: Some(fufs::AttributeType::BootLunEn), ..Default::default() };
        assert_eq!(
            client.read_attribute(&attr).await.unwrap(),
            Err(fufs::QueryErrorCode::GeneralFailure)
        );
        assert_eq!(client.write_attribute(&attr, 1).await.unwrap(), Ok(()));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_ufs_server_operations() {
        use crate::scsi::ScsiStatus;
        use crate::testing::{OCS_SUCCESS, scsi_response, wait_for_any_doorbell};
        use crate::upiu::CommandUpiu;
        use zerocopy::IntoBytes;
        use zerocopy::byteorder::big_endian::U16;

        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let (client, _server_task) = serve_ufs(processor);

        // Invalid descriptor lengths are rejected locally before touching hardware.
        for bad_len in [0u16, 1, MAX_DESCRIPTOR_LENGTH + 1] {
            let desc = fufs::Descriptor {
                type_: Some(fufs::DescriptorType::Device),
                length: Some(bad_len),
                ..Default::default()
            };
            assert_eq!(
                client.read_descriptor(&desc).await.unwrap(),
                Err(fufs::QueryErrorCode::InvalidLength)
            );
        }

        let hardware = std::thread::spawn(move || {
            let mut utp = utp;
            // 1. ReadDescriptor (Device, length 4).
            let slot = wait_for_any_doorbell(&utp);
            let mut resp: QueryUpiu = utp.request(slot);
            resp.header.trans_type = crate::upiu::UpiuTransactionCode::QueryResponse as u8;
            resp.length = U16::new(4);
            resp.command_data[..4].copy_from_slice(&[4, 0, 0xAA, 0xBB]);
            utp.complete(slot, resp.as_bytes(), OCS_SUCCESS);

            // 2-4. SetFlag, ClearFlag, ToggleFlag on BackgroundOpsEn.
            for val in [1u32, 0, 1] {
                let slot = wait_for_any_doorbell(&utp);
                let mut resp: QueryUpiu = utp.request(slot);
                resp.header.trans_type = crate::upiu::UpiuTransactionCode::QueryResponse as u8;
                resp.value = U32::new(val);
                utp.complete(slot, resp.as_bytes(), OCS_SUCCESS);
            }

            // 5-6. ReadBuffer and WriteBuffer on LUN 0.
            for expected_op in [ScsiOpcode::ReadBuffer as u8, ScsiOpcode::WriteBuffer as u8] {
                let slot = wait_for_any_doorbell(&utp);
                let cmd: CommandUpiu = utp.request(slot);
                assert_eq!(cmd.cdb[0], expected_op);
                utp.complete(
                    slot,
                    scsi_response(slot, ScsiStatus::Good.as_u8()).as_bytes(),
                    OCS_SUCCESS,
                );
            }
        });

        let desc = fufs::Descriptor {
            type_: Some(fufs::DescriptorType::Device),
            length: Some(4),
            ..Default::default()
        };
        assert_eq!(client.read_descriptor(&desc).await.unwrap(), Ok(vec![4, 0, 0xAA, 0xBB]));

        let flag =
            fufs::Flag { type_: Some(fufs::FlagType::BackgroundOpsEn), ..Default::default() };
        assert_eq!(client.set_flag(&flag).await.unwrap(), Ok(1));
        assert_eq!(client.clear_flag(&flag).await.unwrap(), Ok(0));
        assert_eq!(client.toggle_flag(&flag).await.unwrap(), Ok(1));

        let vmo = || zx::Vmo::create(PAGE_SIZE).unwrap();
        assert_eq!(
            client.read_buffer(0, fscsi::ReadBufferMode::Data, 0, 0, 4, vmo()).await.unwrap(),
            Ok(())
        );
        assert_eq!(
            client.write_buffer(0, fscsi::WriteBufferMode::Data, 0, 0, 4, vmo()).await.unwrap(),
            Ok(())
        );
        hardware.join().unwrap();
    }
}
