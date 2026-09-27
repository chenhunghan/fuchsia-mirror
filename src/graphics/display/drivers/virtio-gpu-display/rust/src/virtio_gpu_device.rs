// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::virtio::{
    VirtioMemoryRange, VirtioMemoryRangeAccess, VirtioPciDevice, VirtioPciDeviceBuilder,
};
use crate::virtio_gpu_abi as abi;
use fuchsia_runtime;
use std::num::NonZero;
use std::ptr::NonNull;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx;
use zx_sys::zx_paddr_t;

/// Index of the virtqueue that carries 2D commands and their responses.
// @cite(virtio): sec="5.7.2" title="Virtqueues"
// @alias(virtio): theirs="controlq" ours="control queue"
const CONTROL_QUEUE_INDEX: u16 = 0;

/// Bounce buffers for the virtio-gpu control queue.
///
/// The virtio-gpu device reads commands from, and writes responses to, memory
/// that is contiguous in the device's physical address space. Instances own one
/// page of such memory, split into a command area and a response area.
struct ControlQueueBuffer {
    /// Keeps the buffer pinned in the device's physical address space.
    ///
    /// Emptied by [`Self::drop()`].
    pinned_memory_token: Option<zx::Pmt>,

    /// Points to the command area in this process' address space.
    command_ptr: NonNull<u8>,

    /// Points to the response area in this process' address space.
    response_ptr: NonNull<u8>,

    /// Device-accessible physical address of the command area.
    command_physical_address: u64,

    /// Device-accessible physical address of the response area.
    response_physical_address: u64,
}

// SAFETY: Instances own the memory that they point into, so they are
// conceptually equivalent to any other Rust type that owns its data.
unsafe impl Send for ControlQueueBuffer {}

impl ControlQueueBuffer {
    /// Number of bytes in the command area, and in the response area.
    ///
    /// The largest command used by this driver is
    /// [`abi::AttachResourceBackingCommand1`], and the largest response is
    /// [`abi::DisplayInfoResponse`]. Both fit comfortably in half a page.
    const AREA_SIZE_BYTES: usize = 2048;

    /// Number of bytes covered by the command area and the response area.
    const SIZE_BYTES: usize = 2 * Self::AREA_SIZE_BYTES;

    /// Allocates and pins the command and response areas.
    ///
    /// `bti` must be able to pin memory addressable by the virtio device.
    ///
    /// All error conditions are logged.
    fn new(bti: &zx::Bti) -> Result<Self, zx::Status> {
        debug_assert!(!bti.is_invalid());

        let vmo = zx::Vmo::create_contiguous(bti, Self::SIZE_BYTES, 0).map_err(|status| {
            log::warn!("Failed to allocate the virtio-gpu control queue buffer: {:?}", status);
            status
        })?;

        let mut physical_addresses: [zx_paddr_t; 1] = [0];
        let pinned_memory_token = bti
            .pin(
                zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE | zx::BtiOptions::CONTIGUOUS,
                &vmo,
                0,
                Self::SIZE_BYTES as u64,
                &mut physical_addresses,
            )
            .map_err(|status| {
                log::warn!("Failed to pin the virtio-gpu control queue buffer: {:?}", status);
                status
            })?;

        let mapped_address = fuchsia_runtime::vmar_root_self()
            .map(
                0,
                &vmo,
                0,
                Self::SIZE_BYTES,
                zx::VmarFlags::PERM_READ
                    | zx::VmarFlags::PERM_WRITE
                    | zx::VmarFlags::REQUIRE_NON_RESIZABLE,
            )
            .map_err(|status| {
                log::warn!("Failed to map the virtio-gpu control queue buffer: {:?}", status);
                status
            })?;
        let command_ptr = NonNull::new(std::ptr::with_exposed_provenance_mut::<u8>(mapped_address))
            .expect("zx::vmar::map() returned a null address");

        // SAFETY: The mapping covers the command area followed by the response
        // area, and each area is [`Self::AREA_SIZE_BYTES`] long.
        let response_ptr = unsafe { command_ptr.byte_add(Self::AREA_SIZE_BYTES) };

        let command_physical_address = physical_addresses[0] as u64;
        Ok(Self {
            pinned_memory_token: Some(pinned_memory_token),
            command_ptr,
            response_ptr,
            command_physical_address,
            response_physical_address: command_physical_address + Self::AREA_SIZE_BYTES as u64,
        })
    }
}

impl Drop for ControlQueueBuffer {
    fn drop(&mut self) {
        // TODO(https://fxbug.dev/547974944): Reset the virtio device before
        // unpinning memory that the device may still access.
        if let Some(pinned_memory_token) = self.pinned_memory_token.take() {
            // SAFETY: The driver only drops the buffer while it is shutting
            // down, after the device returned all the submitted buffers.
            //
            // Intentionally ignoring the failure. There is nothing the driver
            // can do about it while shutting down.
            let _ = unsafe { pinned_memory_token.unpin() };
        }

        // SAFETY: The mapping was created by [`Self::new()`], and the buffer
        // areas are not referenced anymore.
        //
        // Intentionally ignoring the failure, for the reason above.
        let _ = unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.command_ptr.as_ptr() as usize, Self::SIZE_BYTES)
        };
    }
}

/// Builder pattern instantiation for [`VirtioGpuDevice`].
pub struct VirtioGpuDeviceBuilder {
    pci_builder: VirtioPciDeviceBuilder,
}

impl VirtioGpuDeviceBuilder {
    /// Wraps a virtio device that is ready for feature negotiation.
    pub fn new(pci_builder: VirtioPciDeviceBuilder) -> Self {
        Self { pci_builder }
    }

    /// Consumes the builder and produces an initialized virtio-gpu device.
    ///
    /// All error conditions are logged.
    pub async fn build(mut self) -> Result<VirtioGpuDevice, zx::Status> {
        let offered_features: abi::GpuFeatureBits = self.pci_builder.offered_features().into();
        log::debug!("virtio-gpu device offered features: {:?}", offered_features);

        // TODO(https://fxbug.dev/504722357): Negotiate EDID support and blob
        // resource support, once the driver implements them.
        let accepted_features = abi::GpuFeatureBits::default();

        // The device configuration reports display configuration changes.
        // TODO(https://fxbug.dev/504722357): Handle display change events.
        // @cite(virtio): sec="5.7.4" title="Device configuration layout"
        if self.pci_builder.take_device_configuration().is_none() {
            log::warn!("virtio device missing required PCI capability: device configuration");
            return Err(zx::Status::IO_DATA_LOSS);
        }

        self.pci_builder.accept_features(accepted_features.into()).await?;

        let control_queue_buffer = ControlQueueBuffer::new(self.pci_builder.bti())?;
        let device = self.pci_builder.build()?;

        Ok(VirtioGpuDevice { device, next_resource_id: 1, control_queue_buffer })
    }
}

/// A virtual display exposed by a virtio-gpu device.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub scanout_id: abi::ScanoutId,
    pub scanout_info: abi::ScanoutInfo,
}

/// Implements the display-related subset of the virtio-gpu device specification.
///
/// See [`VirtioGpuDeviceBuilder`] for obtaining instances.
pub struct VirtioGpuDevice {
    device: VirtioPciDevice,

    /// The resource ID handed out by the next [`Self::allocate_resource_id()`].
    next_resource_id: u32,

    control_queue_buffer: ControlQueueBuffer,
}

impl VirtioGpuDevice {
    /// Returns a BTI that can pin VMOs in memory addressable by the device.
    pub fn bti(&self) -> &zx::Bti {
        self.device.bti()
    }

    /// Retrieves the device's current output configuration.
    ///
    /// Returns one entry for each enabled scanout.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn get_display_info(&mut self) -> Result<Vec<DisplayInfo>, zx::Status> {
        let command = abi::BufferHeader::new(abi::BufferType::GET_DISPLAY_INFO_COMMAND);
        let response: abi::DisplayInfoResponse =
            self.exchange_control_command(&command, abi::BufferType::DISPLAY_INFO_RESPONSE).await?;

        Ok(response
            .scanouts
            .iter()
            .enumerate()
            .filter(|(_scanout_index, scanout_info)| scanout_info.enabled != 0)
            .map(|(scanout_index, scanout_info)| DisplayInfo {
                // `as` does not truncate, because the array has
                // [`abi::MAX_SCANOUT_COUNT`] elements.
                scanout_id: abi::ScanoutId(scanout_index as u32),
                scanout_info: *scanout_info,
            })
            .collect())
    }

    /// Creates a 2D resource in the device's memory.
    ///
    /// Returns the ID assigned to the new resource. The returned ID is
    /// guaranteed to not be used by another active resource.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn create_2d_resource(
        &mut self,
        width: u32,
        height: u32,
        format: abi::ResourceFormat,
    ) -> Result<NonZero<u32>, zx::Status> {
        debug_assert!(format.is_known(), "Invalid resource format: {:?}", format);

        let resource_id = self.allocate_resource_id()?;
        let command = abi::Create2DResourceCommand {
            header: abi::BufferHeader::new(abi::BufferType::CREATE_2D_RESOURCE_COMMAND),
            resource_id: Some(resource_id),
            format,
            width,
            height,
        };
        let _response: abi::EmptyResponse =
            self.exchange_control_command(&command, abi::BufferType::EMPTY_RESPONSE).await?;

        Ok(resource_id)
    }

    /// Assigns a range of driver-owned memory as a resource's backing store.
    ///
    /// `physical_address` must point to memory that is pinned, and contiguous
    /// in the device's physical address space.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn attach_resource_backing(
        &mut self,
        resource_id: NonZero<u32>,
        physical_address: u64,
        size_bytes: NonZero<u32>,
    ) -> Result<(), zx::Status> {
        let command = abi::AttachResourceBackingCommand1 {
            header: abi::BufferHeader::new(abi::BufferType::ATTACH_RESOURCE_BACKING_COMMAND),
            resource_id: Some(resource_id),
            entry_count: 1,
            entries: [abi::MemoryEntry {
                address: physical_address,
                length: size_bytes.get(),
                _padding: 0,
            }],
        };
        let _response: abi::EmptyResponse =
            self.exchange_control_command(&command, abi::BufferType::EMPTY_RESPONSE).await?;

        Ok(())
    }

    /// Points a scanout at a resource.
    ///
    /// A [`None`] `resource_id` disables the scanout.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn set_scanout_properties(
        &mut self,
        scanout_id: abi::ScanoutId,
        resource_id: abi::ResourceId,
        width: u32,
        height: u32,
    ) -> Result<(), zx::Status> {
        debug_assert!(scanout_id.is_valid(), "Invalid scanout ID: {:?}", scanout_id);

        let command = abi::SetScanoutCommand {
            header: abi::BufferHeader::new(abi::BufferType::SET_SCANOUT_COMMAND),
            image_source: abi::Rectangle { x: 0, y: 0, width, height },
            scanout_id,
            resource_id,
        };
        let _response: abi::EmptyResponse =
            self.exchange_control_command(&command, abi::BufferType::EMPTY_RESPONSE).await?;

        Ok(())
    }

    /// Copies pixel data from a resource's backing store into the resource.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn transfer_to_host_2d(
        &mut self,
        resource_id: NonZero<u32>,
        width: u32,
        height: u32,
    ) -> Result<(), zx::Status> {
        let command = abi::Transfer2DResourceToHostCommand {
            header: abi::BufferHeader::new(abi::BufferType::TRANSFER_2D_RESOURCE_TO_HOST_COMMAND),
            image_source: abi::Rectangle { x: 0, y: 0, width, height },
            destination_offset: 0,
            resource_id: Some(resource_id),
            _padding: 0,
        };
        let _response: abi::EmptyResponse =
            self.exchange_control_command(&command, abi::BufferType::EMPTY_RESPONSE).await?;

        Ok(())
    }

    /// Shows the resource's pixel data on all the scanouts that use it.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    pub async fn flush_resource(
        &mut self,
        resource_id: NonZero<u32>,
        width: u32,
        height: u32,
    ) -> Result<(), zx::Status> {
        let command = abi::FlushResourceCommand {
            header: abi::BufferHeader::new(abi::BufferType::FLUSH_RESOURCE_COMMAND),
            image_source: abi::Rectangle { x: 0, y: 0, width, height },
            resource_id: Some(resource_id),
            _padding: 0,
        };
        let _response: abi::EmptyResponse =
            self.exchange_control_command(&command, abi::BufferType::EMPTY_RESPONSE).await?;

        Ok(())
    }

    /// Assigns a resource ID that is not used by any active resource.
    ///
    /// All error conditions are logged.
    fn allocate_resource_id(&mut self) -> Result<NonZero<u32>, zx::Status> {
        // TODO(https://fxbug.dev/504722357): Reuse the IDs of destroyed
        // resources, instead of failing after the ID space is exhausted.
        let resource_id = NonZero::<u32>::new(self.next_resource_id).ok_or_else(|| {
            log::warn!("Exhausted the virtio-gpu resource ID space");
            zx::Status::NO_RESOURCES
        })?;

        self.next_resource_id = self.next_resource_id.wrapping_add(1);
        Ok(resource_id)
    }

    /// Submits `command` to the control queue and returns the device's response.
    ///
    /// Errors if the device's response is not `expected_response_type`.
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    async fn exchange_control_command<Command, Response>(
        &mut self,
        command: &Command,
        expected_response_type: abi::BufferType,
    ) -> Result<Response, zx::Status>
    where
        Command: IntoBytes + Immutable,
        Response: FromBytes + IntoBytes + Immutable,
    {
        let mut response = Response::new_zeroed();
        self.exchange_control_buffers(command.as_bytes(), response.as_mut_bytes()).await?;

        let (header, _remainder) = abi::BufferHeader::read_from_prefix(response.as_bytes())
            .expect("virtio-gpu responses start with a BufferHeader");
        if header.type_ != expected_response_type {
            log::warn!("virtio-gpu device returned unexpected response: {:?}", header.type_);
            return Err(zx::Status::IO);
        }

        Ok(response)
    }

    /// Submits one control queue buffer and waits for the device to return it.
    ///
    /// `command_bytes` and `response_bytes` must each be non-empty, and must
    /// each fit in [`ControlQueueBuffer::AREA_SIZE_BYTES`].
    ///
    /// All error conditions are logged.
    // @cite(virtio): sec="2.7" title="Split Virtqueues"
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    async fn exchange_control_buffers(
        &mut self,
        command_bytes: &[u8],
        response_bytes: &mut [u8],
    ) -> Result<(), zx::Status> {
        // The checks below guard memory writes, so they must also run in
        // production builds.
        assert!(
            command_bytes.len() <= ControlQueueBuffer::AREA_SIZE_BYTES,
            "virtio-gpu command exceeds the control queue buffer: {} bytes",
            command_bytes.len()
        );
        assert!(
            response_bytes.len() <= ControlQueueBuffer::AREA_SIZE_BYTES,
            "virtio-gpu response exceeds the control queue buffer: {} bytes",
            response_bytes.len()
        );

        // `as` does not truncate, and [`NonZero::new()`] does not return
        // [`None`], because of the checks above and because all virtio-gpu
        // commands and responses include a header.
        let command_size_bytes = NonZero::<u32>::new(command_bytes.len() as u32)
            .expect("virtio-gpu commands are not empty");
        let response_size_bytes = NonZero::<u32>::new(response_bytes.len() as u32)
            .expect("virtio-gpu responses are not empty");

        // SAFETY: The checks above ensure that the writes stay inside the
        // command area and the response area. The device does not own the
        // areas, because the buffer is submitted below.
        unsafe {
            std::ptr::copy_nonoverlapping(
                command_bytes.as_ptr(),
                self.control_queue_buffer.command_ptr.as_ptr(),
                command_bytes.len(),
            );
            std::ptr::write_bytes(
                self.control_queue_buffer.response_ptr.as_ptr(),
                0,
                response_bytes.len(),
            );
        }

        // SAFETY: The ranges cover pinned memory that is contiguous in the
        // device's physical address space. No Rust reference points into the
        // ranges while the buffer is submitted.
        let buffer = unsafe {
            [
                VirtioMemoryRange::new(
                    self.control_queue_buffer.command_physical_address,
                    command_size_bytes,
                    VirtioMemoryRangeAccess::Input,
                ),
                VirtioMemoryRange::new(
                    self.control_queue_buffer.response_physical_address,
                    response_size_bytes,
                    VirtioMemoryRangeAccess::Output,
                ),
            ]
        };
        self.device.submit_and_wait_for_buffer(CONTROL_QUEUE_INDEX, &buffer).await?;

        // SAFETY: The device returned the buffer, so it no longer accesses the
        // response area. The check above ensures that the read stays inside the
        // response area.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.control_queue_buffer.response_ptr.as_ptr(),
                response_bytes.as_mut_ptr(),
                response_bytes.len(),
            );
        }

        Ok(())
    }
}
