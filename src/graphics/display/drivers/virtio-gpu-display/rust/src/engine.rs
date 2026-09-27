// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_display_engine as fidl_display_engine;
use fidl_next_fuchsia_hardware_display_types as fidl_display_types;
use fidl_next_fuchsia_images2 as fidl_images2;
use fidl_next_fuchsia_math as fidl_math;
use fidl_next_fuchsia_sysmem2 as fidl_sysmem2;
use fuchsia_sync::Mutex;
use futures::lock::Mutex as AsyncMutex;
use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use crate::imported_image::ImportedImage;
use crate::imported_images::{ImportedImages, SysmemBufferInfo};
use crate::virtio_gpu_abi as abi;
use crate::virtio_gpu_device::VirtioGpuDevice;

/// Identifies the display managed by this driver to the Display Coordinator.
// TODO(https://fxbug.dev/504722357): Drive all the scanouts exposed by the
// virtio-gpu device, instead of only driving the first enabled scanout.
const DISPLAY_ID: fidl_display_types::DisplayId = fidl_display_types::DisplayId { value: 1 };

/// Static description of the hardware managed by this driver.
const ENGINE_INFO: fidl_display_engine::EngineInfo = fidl_display_engine::EngineInfo {
    // The driver shows one image per scanout, so it supports one layer.
    max_layer_count: 1,
    max_connected_display_count: 1,
    is_capture_supported: false,
};

/// The rate at which the driver transfers images to the virtio-gpu device.
///
/// The virtio-gpu specification does not expose a display refresh rate. This
/// value balances animation smoothness against the host CPU time spent
/// transferring and flushing images.
const REFRESH_RATE_MILLIHERTZ: u32 = 30_000;

/// Time between two consecutive image transfers to the virtio-gpu device.
const REFRESH_PERIOD: Duration =
    Duration::from_nanos(1_000_000_000_000 / REFRESH_RATE_MILLIHERTZ as u64);

/// Scanout geometry used when the device does not report any enabled scanout.
const FALLBACK_ACTIVE_AREA: fidl_math::SizeU = fidl_math::SizeU { width: 1280, height: 800 };

/// The pixel formats that this driver can scan out.
const SUPPORTED_PIXEL_FORMATS: [fidl_images2::PixelFormat; 2] =
    [fidl_images2::PixelFormat::B8G8R8A8, fidl_images2::PixelFormat::R8G8B8A8];

/// Number of bytes used by each pixel in [`SUPPORTED_PIXEL_FORMATS`].
const BYTES_PER_PIXEL: u32 = 4;

/// The virtio-gpu scanout that shows the Display Coordinator's configurations.
#[derive(Clone, Copy)]
struct ScanoutTarget {
    id: abi::ScanoutId,
    width: u32,
    height: u32,
}

/// A display configuration submitted by the Display Coordinator.
///
/// [The configuration states guide][config-states] defines the configuration
/// state terms used here.
///
/// [config-states]: /docs/development/drivers/driver_guides/display/config-states.md
#[derive(Clone, Copy)]
struct SubmittedConfiguration {
    /// The virtio-gpu resource backing the configuration's single layer.
    ///
    /// [`None`] disables the scanout.
    resource_id: abi::ResourceId,

    /// Identifies the configuration to the Display Coordinator.
    config_stamp: fidl_display_engine::ConfigStamp,
}

/// Emulates a display engine's refresh loop.
///
/// The virtio-gpu device does not scan out image data on its own. Instead, the
/// driver transfers the image data to the device, and asks the device to show
/// it. This type performs the transfers at a fixed rate, and reports each
/// transfer to the Display Coordinator as a VSync.
struct ScanoutFlusher {
    gpu_device: Arc<AsyncMutex<VirtioGpuDevice>>,

    /// The configuration that will be latched by the next loop iteration.
    ///
    /// Shared with the [`EngineServer`] that spawned this instance.
    queued_configuration: Arc<Mutex<Option<SubmittedConfiguration>>>,

    listener: fidl_next::Client<fidl_display_engine::EngineListener>,

    scanout: ScanoutTarget,
}

impl ScanoutFlusher {
    /// Runs the refresh loop until the task running it is stopped.
    async fn run(self) {
        // The resource used by the latched configuration. [`None`] while the
        // Display Coordinator has not submitted any configuration.
        let mut latched_resource_id: abi::ResourceId = None;

        loop {
            fuchsia_async::Timer::new(REFRESH_PERIOD).await;

            let Some(queued_configuration) = *self.queued_configuration.lock() else {
                continue;
            };

            // `latch_configuration()` logs all error conditions. Skipping the
            // VSync notification tells the Coordinator that the configuration
            // was not displayed.
            if self.latch_configuration(queued_configuration, latched_resource_id).await.is_err() {
                continue;
            }
            latched_resource_id = queued_configuration.resource_id;

            // Intentionally ignoring the failure. A failure means that the
            // Coordinator disconnected, and the driver will stop this task.
            let _ = self
                .listener
                .on_display_vsync(
                    DISPLAY_ID,
                    zx::MonotonicInstant::get().into_nanos(),
                    queued_configuration.config_stamp,
                )
                .await;
        }
    }

    /// Shows `configuration` on the scanout driven by this instance.
    ///
    /// `latched_resource_id` is the resource shown by the scanout, which is
    /// used to avoid redundant scanout reconfiguration.
    ///
    /// All error conditions are logged.
    async fn latch_configuration(
        &self,
        configuration: SubmittedConfiguration,
        latched_resource_id: abi::ResourceId,
    ) -> Result<(), zx::Status> {
        let mut gpu_device = self.gpu_device.lock().await;

        if configuration.resource_id != latched_resource_id {
            gpu_device
                .set_scanout_properties(
                    self.scanout.id,
                    configuration.resource_id,
                    self.scanout.width,
                    self.scanout.height,
                )
                .await?;
        }

        // The scanout is disabled, so there is no image data to transfer.
        let Some(resource_id) = configuration.resource_id else {
            return Ok(());
        };

        gpu_device
            .transfer_to_host_2d(resource_id, self.scanout.width, self.scanout.height)
            .await?;
        gpu_device.flush_resource(resource_id, self.scanout.width, self.scanout.height).await
    }
}

/// Serves the [`fuchsia.hardware.display.engine/Engine`] protocol.
///
/// See [`EngineService`] for obtaining instances.
pub struct EngineServer {
    gpu_device: Arc<AsyncMutex<VirtioGpuDevice>>,
    imported_images: ImportedImages,

    /// The configuration most recently submitted by the Display Coordinator.
    ///
    /// Shared with the [`ScanoutFlusher`] task.
    queued_configuration: Arc<Mutex<Option<SubmittedConfiguration>>>,

    /// Owns the tasks that communicate with the Display Coordinator's listener.
    ///
    /// Dropping the scope stops the tasks.
    scope: fuchsia_async::Scope,
}

impl EngineServer {
    /// Creates a server that drives `gpu_device`.
    ///
    /// All error conditions are logged.
    pub async fn new(
        gpu_device: Arc<AsyncMutex<VirtioGpuDevice>>,
        sysmem: fidl_next::Client<fidl_sysmem2::Allocator>,
    ) -> Result<Self, zx::Status> {
        let imported_images = ImportedImages::new(sysmem).await?;
        Ok(Self {
            gpu_device,
            imported_images,
            queued_configuration: Arc::new(Mutex::new(None)),
            scope: fuchsia_async::Scope::new(),
        })
    }

    /// Chooses the scanout that will show the Coordinator's configurations.
    ///
    /// All error conditions are logged.
    async fn discover_scanout(&self) -> Result<ScanoutTarget, zx::Status> {
        let display_infos = self.gpu_device.lock().await.get_display_info().await?;

        let Some(display_info) = display_infos.first() else {
            // Devices are allowed to report all their scanouts as disabled
            // until the driver points a scanout at a resource. The driver uses
            // the first scanout, which all devices must implement.
            log::warn!("virtio-gpu device reports no enabled scanout, using a fallback mode");
            return Ok(ScanoutTarget {
                id: abi::ScanoutId(0),
                width: FALLBACK_ACTIVE_AREA.width,
                height: FALLBACK_ACTIVE_AREA.height,
            });
        };

        Ok(ScanoutTarget {
            id: display_info.scanout_id,
            width: display_info.scanout_info.geometry.width,
            height: display_info.scanout_info.geometry.height,
        })
    }

    /// Creates a virtio-gpu resource that scans out an imported sysmem buffer.
    ///
    /// Implements [`fuchsia.hardware.display.engine/Engine.ImportImage`].
    ///
    /// All error conditions are logged.
    async fn import_image_resource(
        &mut self,
        image_metadata: fidl_display_types::ImageMetadata,
        buffer_collection_id: fidl_display_engine::BufferCollectionId,
        buffer_collection_index: u32,
    ) -> Result<fidl_display_engine::ImageId, zx::Status> {
        // TODO(https://fxbug.dev/504722357): Remove the image from
        // `imported_images` if any of the steps below fails.
        let image_id = self
            .imported_images
            .import_image(buffer_collection_id, buffer_collection_index)
            .await?;

        let sysmem_info =
            self.imported_images.find_sysmem_info(image_id).expect("Image was just imported");

        // virtio-gpu 2D resources are laid out linearly.
        // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
        if sysmem_info.pixel_format_modifier != fidl_images2::PixelFormatModifier::Linear {
            log::warn!(
                "Rejecting image with unsupported pixel format modifier: {:?}",
                sysmem_info.pixel_format_modifier
            );
            return Err(zx::Status::NOT_SUPPORTED);
        }
        let resource_format = abi::ResourceFormat::try_from(sysmem_info.pixel_format)?;

        let bytes_per_row = image_bytes_per_row(sysmem_info, image_metadata.dimensions.width)?;
        let image_size_bytes = image_size_bytes(bytes_per_row, image_metadata.dimensions.height)?;

        let mut gpu_device = self.gpu_device.lock().await;
        let mut imported_image = ImportedImage::new(
            gpu_device.bti(),
            &sysmem_info.image_vmo,
            sysmem_info.image_vmo_offset,
            image_size_bytes.into(),
            resource_format,
            bytes_per_row,
        )?;

        let resource_id = gpu_device
            .create_2d_resource(
                image_metadata.dimensions.width,
                image_metadata.dimensions.height,
                resource_format,
            )
            .await?;
        gpu_device
            .attach_resource_backing(
                resource_id,
                imported_image.physical_address(),
                image_size_bytes,
            )
            .await?;
        drop(gpu_device);

        imported_image.set_virtio_resource_id(Some(resource_id));
        self.imported_images.set_image(image_id, imported_image);
        Ok(image_id)
    }

    /// Records the configuration that the [`ScanoutFlusher`] will latch next.
    ///
    /// Implements
    /// [`fuchsia.hardware.display.engine/Engine.SubmitConfiguration`].
    fn queue_configuration(
        &mut self,
        display_config: &fidl_display_engine::DisplayConfig,
        config_stamp: fidl_display_engine::ConfigStamp,
    ) {
        let Some(layer) = display_config.layers.first() else {
            log::error!("Ignoring submitted configuration that does not have any layer");
            return;
        };

        let Some(imported_image) = self.imported_images.find_image(layer.image_id) else {
            log::error!(
                "Ignoring submitted configuration with unknown image ID: {}",
                layer.image_id.value
            );
            return;
        };

        *self.queued_configuration.lock() = Some(SubmittedConfiguration {
            resource_id: imported_image.virtio_resource_id(),
            config_stamp,
        });
    }

    /// Conveys the virtio-gpu image requirements to sysmem.
    ///
    /// Implements
    /// [`fuchsia.hardware.display.engine/Engine.SetBufferCollectionConstraints`].
    ///
    /// All error conditions are logged.
    async fn set_sysmem_constraints(
        &self,
        buffer_collection_id: &fidl_display_engine::BufferCollectionId,
    ) -> Result<(), zx::Status> {
        let Some(buffer_collection) =
            self.imported_images.find_buffer_collection(buffer_collection_id)
        else {
            log::warn!(
                "Rejected request to set constraints on BufferCollection with unknown ID: {}",
                buffer_collection_id.value
            );
            return Err(zx::Status::NOT_FOUND);
        };

        let constraints = fidl_sysmem2::BufferCollectionConstraints {
            usage: Some(fidl_sysmem2::BufferUsage {
                display: Some(fidl_sysmem2::DISPLAY_USAGE_LAYER),
                ..Default::default()
            }),
            buffer_memory_constraints: Some(fidl_sysmem2::BufferMemoryConstraints {
                // The device fetches each image from a single memory range.
                physically_contiguous_required: Some(true),
                secure_required: Some(false),
                ram_domain_supported: Some(true),
                cpu_domain_supported: Some(true),
                ..Default::default()
            }),
            image_format_constraints: Some(
                SUPPORTED_PIXEL_FORMATS
                    .iter()
                    .map(|pixel_format| fidl_sysmem2::ImageFormatConstraints {
                        pixel_format: Some(*pixel_format),
                        pixel_format_modifier: Some(fidl_images2::PixelFormatModifier::Linear),
                        color_spaces: Some(vec![fidl_images2::ColorSpace::Srgb]),
                        bytes_per_row_divisor: Some(BYTES_PER_PIXEL),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };

        let set_constraints_request = fidl_sysmem2::BufferCollectionSetConstraintsRequest {
            constraints: Some(constraints),
            ..Default::default()
        };

        buffer_collection.set_constraints_with(set_constraints_request).await.map_err(|error| {
            log::warn!("Failed to set sysmem BufferCollection constraints: {:?}", error);
            zx::Status::INTERNAL
        })
    }
}

// The methods below intentionally ignore responder failures. A failure means
// that the Display Coordinator disconnected, which tears down this server.
impl<T: fidl_next::Transport + 'static> fidl_display_engine::EngineLocalServerHandler<T>
    for EngineServer
{
    async fn complete_coordinator_connection(
        &mut self,
        request: Request<fidl_display_engine::engine::CompleteCoordinatorConnection, T>,
        responder: Responder<fidl_display_engine::engine::CompleteCoordinatorConnection, T>,
    ) {
        log::debug!("Engine.CompleteCoordinatorConnection()");

        let Ok(scanout) = self.discover_scanout().await else {
            // `discover_scanout()` logs all error conditions. Dropping the
            // responder closes the connection, which tells the Coordinator
            // that this driver cannot drive the device.
            return;
        };

        let listener_dispatcher =
            fidl_next::ClientDispatcher::new(request.payload().engine_listener);
        let listener = listener_dispatcher.client();
        self.scope.spawn_local(async move {
            // Intentionally ignoring the failure. The Coordinator disconnecting
            // is the expected way for the dispatcher to stop.
            let _ = listener_dispatcher.run_client().await;
        });

        let display_info = fidl_display_engine::RawDisplayInfo {
            display_id: DISPLAY_ID,
            preferred_modes: vec![fidl_display_types::Mode {
                active_area: fidl_math::SizeU { width: scanout.width, height: scanout.height },
                refresh_rate_millihertz: REFRESH_RATE_MILLIHERTZ,
                flags: fidl_display_types::ModeFlags::empty(),
            }],
            // The virtio-gpu device does not expose the display's E-EDID.
            edid_bytes: vec![],
            pixel_formats: SUPPORTED_PIXEL_FORMATS.to_vec(),
        };

        let flusher = ScanoutFlusher {
            gpu_device: self.gpu_device.clone(),
            queued_configuration: self.queued_configuration.clone(),
            listener: listener.clone(),
            scanout,
        };
        self.scope.spawn_local(async move {
            if let Err(error) = listener.on_display_added(display_info).await {
                log::warn!("Failed to report the connected display: {:?}", error);
                return;
            }
            flusher.run().await;
        });

        let _ = responder.respond(ENGINE_INFO).await;
    }

    async fn unset_listener(&mut self) {
        log::debug!("Engine.UnsetListener()");

        // Dropping the scope stops the tasks that use the listener.
        self.scope = fuchsia_async::Scope::new();
    }

    async fn import_buffer_collection(
        &mut self,
        request: Request<fidl_display_engine::engine::ImportBufferCollection, T>,
        responder: Responder<fidl_display_engine::engine::ImportBufferCollection, T>,
    ) {
        log::debug!("Engine.ImportBufferCollection()");
        let payload = request.payload();

        match self
            .imported_images
            .import_buffer_collection(payload.buffer_collection_id, payload.collection_token)
            .await
        {
            Ok(()) => {
                let _ = responder.respond(()).await;
            }
            Err(status) => {
                let _ = responder.respond_err(status).await;
            }
        }
    }

    async fn release_buffer_collection(
        &mut self,
        request: Request<fidl_display_engine::engine::ReleaseBufferCollection, T>,
        responder: Responder<fidl_display_engine::engine::ReleaseBufferCollection, T>,
    ) {
        log::debug!("Engine.ReleaseBufferCollection()");

        self.imported_images
            .release_buffer_collection(&request.payload().buffer_collection_id)
            .await;
        let _ = responder.respond(()).await;
    }

    async fn import_image(
        &mut self,
        request: Request<fidl_display_engine::engine::ImportImage, T>,
        responder: Responder<fidl_display_engine::engine::ImportImage, T>,
    ) {
        log::debug!("Engine.ImportImage()");
        let payload = request.payload();

        match self
            .import_image_resource(
                payload.image_metadata,
                payload.buffer_collection_id,
                payload.buffer_collection_index,
            )
            .await
        {
            Ok(image_id) => {
                let _ = responder.respond(image_id).await;
            }
            Err(status) => {
                let _ = responder.respond_err(status).await;
            }
        }
    }

    async fn import_image_for_capture(
        &mut self,
        _request: Request<fidl_display_engine::engine::ImportImageForCapture, T>,
        responder: Responder<fidl_display_engine::engine::ImportImageForCapture, T>,
    ) {
        // The driver reports that it does not support capture in
        // [`ENGINE_INFO`], so the Coordinator must not issue this call.
        log::error!("Rejecting capture image import, capture is not supported");
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn release_image(
        &mut self,
        request: Request<fidl_display_engine::engine::ReleaseImage, T>,
    ) {
        log::debug!("Engine.ReleaseImage()");
        let image_id = request.payload().image_id;

        // TODO(https://fxbug.dev/504722357): Check that the image is not used
        // by a queued or latched configuration.
        //
        // Intentionally ignoring the failure. `release_image()` logs all error
        // conditions, and this FIDL method does not report errors.
        //
        // SAFETY: The Coordinator's API contract states that released images
        // are not used by queued or latched configurations.
        let _ = unsafe { self.imported_images.release_image(image_id) };
    }

    async fn check_configuration(
        &mut self,
        request: Request<fidl_display_engine::engine::CheckConfiguration, T>,
        responder: Responder<fidl_display_engine::engine::CheckConfiguration, T>,
    ) {
        log::debug!("Engine.CheckConfiguration()");

        // TODO(https://fxbug.dev/504722357): Reject configurations that need
        // scaling, cropping, rotation, or alpha blending.
        let layer_count = request.payload().display_config.layers.len();
        if layer_count > ENGINE_INFO.max_layer_count as usize {
            log::warn!("Rejecting configuration with too many layers: {}", layer_count);
            let _ =
                responder.respond_err(fidl_display_types::ConfigResult::UnsupportedConfig).await;
            return;
        }

        let _ = responder.respond(()).await;
    }

    async fn submit_configuration(
        &mut self,
        request: Request<fidl_display_engine::engine::SubmitConfiguration, T>,
        responder: Responder<fidl_display_engine::engine::SubmitConfiguration, T>,
    ) {
        log::debug!("Engine.SubmitConfiguration()");
        let payload = request.payload();

        self.queue_configuration(&payload.display_config, payload.config_stamp);
        let _ = responder.respond(()).await;
    }

    async fn set_buffer_collection_constraints(
        &mut self,
        request: Request<fidl_display_engine::engine::SetBufferCollectionConstraints, T>,
        responder: Responder<fidl_display_engine::engine::SetBufferCollectionConstraints, T>,
    ) {
        log::debug!("Engine.SetBufferCollectionConstraints()");

        match self.set_sysmem_constraints(&request.payload().buffer_collection_id).await {
            Ok(()) => {
                let _ = responder.respond(()).await;
            }
            Err(status) => {
                let _ = responder.respond_err(status).await;
            }
        }
    }

    async fn set_display_power_mode(
        &mut self,
        _request: Request<fidl_display_engine::engine::SetDisplayPowerMode, T>,
        responder: Responder<fidl_display_engine::engine::SetDisplayPowerMode, T>,
    ) {
        // The virtio-gpu device does not expose display power management.
        log::debug!("Engine.SetDisplayPowerMode() is not supported");
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn set_minimum_rgb(
        &mut self,
        _request: Request<fidl_display_engine::engine::SetMinimumRgb, T>,
        responder: Responder<fidl_display_engine::engine::SetMinimumRgb, T>,
    ) {
        // The virtio-gpu device does not expose color channel clamping.
        log::debug!("Engine.SetMinimumRgb() is not supported");
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn start_capture(
        &mut self,
        _request: Request<fidl_display_engine::engine::StartCapture, T>,
        responder: Responder<fidl_display_engine::engine::StartCapture, T>,
    ) {
        // The driver reports that it does not support capture in
        // [`ENGINE_INFO`], so the Coordinator must not issue this call.
        log::error!("Rejecting capture start, capture is not supported");
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn release_capture(
        &mut self,
        _request: Request<fidl_display_engine::engine::ReleaseCapture, T>,
        responder: Responder<fidl_display_engine::engine::ReleaseCapture, T>,
    ) {
        // The driver reports that it does not support capture in
        // [`ENGINE_INFO`], so the Coordinator must not issue this call.
        log::error!("Rejecting capture release, capture is not supported");
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }
}

/// Computes the number of bytes used by each image row in a sysmem buffer.
///
/// All error conditions are logged.
fn image_bytes_per_row(
    sysmem_info: &SysmemBufferInfo,
    image_width: u32,
) -> Result<NonZero<u32>, zx::Status> {
    let packed_bytes_per_row = image_width.checked_mul(BYTES_PER_PIXEL).ok_or_else(|| {
        log::warn!("Rejecting image whose rows exceed 4GB: {} pixels", image_width);
        zx::Status::INVALID_ARGS
    })?;

    let bytes_per_row = std::cmp::max(sysmem_info.minimum_bytes_per_row, packed_bytes_per_row);
    let bytes_per_row = bytes_per_row
        .checked_next_multiple_of(sysmem_info.bytes_per_row_divisor.get())
        .ok_or_else(|| {
            log::warn!("Rejecting image whose rows exceed 4GB: {} bytes", bytes_per_row);
            zx::Status::INVALID_ARGS
        })?;

    NonZero::<u32>::new(bytes_per_row).ok_or_else(|| {
        log::warn!("Rejecting image with empty rows");
        zx::Status::INVALID_ARGS
    })
}

/// Computes the number of bytes used by an image in a sysmem buffer.
///
/// All error conditions are logged.
fn image_size_bytes(
    bytes_per_row: NonZero<u32>,
    image_height: u32,
) -> Result<NonZero<u32>, zx::Status> {
    // virtio-gpu memory entries describe their size using 32 bits, so larger
    // images cannot be attached to a virtio-gpu resource.
    // @cite(virtio): sec="5.7.6.8" title="Device Operation: controlq"
    let size_bytes = bytes_per_row.get().checked_mul(image_height).ok_or_else(|| {
        log::warn!("Rejecting image larger than 4GB: {} rows", image_height);
        zx::Status::INVALID_ARGS
    })?;

    NonZero::<u32>::new(size_bytes).ok_or_else(|| {
        log::warn!("Rejecting empty image");
        zx::Status::INVALID_ARGS
    })
}

/// Serves the [`fuchsia.hardware.display.engine/Service`] service.
pub struct EngineService {
    gpu_device: Arc<AsyncMutex<VirtioGpuDevice>>,
    sysmem: fidl_next::Client<fidl_sysmem2::Allocator>,

    /// Owns the tasks serving the service's protocol connections.
    ///
    /// Dropping the scope stops the tasks.
    scope: fuchsia_async::Scope,
}

impl EngineService {
    /// Exposes `gpu_device` as a display engine.
    ///
    /// `sysmem_client` must be a connection to the sysmem allocator service.
    pub fn new(
        gpu_device: VirtioGpuDevice,
        sysmem_client: fidl_next::ClientEnd<fidl_sysmem2::Allocator>,
    ) -> Self {
        Self {
            gpu_device: Arc::new(AsyncMutex::new(gpu_device)),
            sysmem: sysmem_client.spawn(),
            scope: fuchsia_async::Scope::new(),
        }
    }
}

impl fidl_display_engine::ServiceHandler for EngineService {
    fn engine(&self, server_end: ServerEnd<fidl_display_engine::Engine, fdf_fidl::DriverChannel>) {
        let gpu_device = self.gpu_device.clone();
        let sysmem = self.sysmem.clone();

        self.scope.spawn_local(async move {
            // `EngineServer::new()` logs all error conditions.
            let Ok(server) = EngineServer::new(gpu_device, sysmem).await else {
                return;
            };

            // Intentionally ignoring the result. The Coordinator disconnecting
            // is the expected way for the server to stop.
            let _ =
                server_end.spawn_local_on(server, &fidl_next::fuchsia_async::FuchsiaAsync).await;
        });
    }
}
