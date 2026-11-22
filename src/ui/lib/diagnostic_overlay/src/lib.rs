// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Diagnostic and reboot-countdown HUD overlay library for Flatland2 window managers.
//!
//! # Host Integration Protocol
//!
//! Hosts using [`OverlayController`] must open their Flatland session via
//! `fuchsia.ui.composition.FlatlandFactory/CreateFlatland` with
//! `FlatlandConfig { use_flatland2: Some(true), ..Default::default() }`, and route the protocols
//! declared in `meta/client.shard.cml`.
//!
//! 1. **Initialization:** Call [`OverlayController::new()`] once with the host's `FlatlandProxy`,
//!    shared [`IdGenerator`], initial logical display size, `enabled` flag (see
//!    [`is_overlay_enabled()`]), an optional `fuchsia.ui.pointer.TouchSource` proxy, and an
//!    `UnboundedSender<OverlayEvent>`. When `enabled` is `true`, attach
//!    [`OverlayController::overlay_transform_id()`] once to the host root or container
//!    transform above client window transforms. When `enabled` is `false`, zero Flatland
//!    commands, background tasks, or protocol connections are created.
//! 2. **Event loop:** Forward incoming [`OverlayEvent`]s to [`OverlayController::handle_event()`].
//!    Whenever `handle_event()` returns `Ok(PresentNeeded::Yes)`, Flatland2 commands were enqueued
//!    on `flatland`, so the host should schedule a `Flatland::Present()`.
//! 3. **Present-credit deferral (`b/549201450`):** Until Flatland present credits are deprecated,
//!    pass `has_present_credit` to [`OverlayController::handle_event()`] and
//!    [`OverlayController::set_display_size()`]. When `has_present_credit` is `false`, visible
//!    updates are marked dirty and return `Ok(PresentNeeded::No)` (do not call `Present()`); once
//!    the host receives `Flatland::OnNextFrameBegin` credits, call
//!    [`OverlayController::flush_pending()`] and `Present()` if it returns
//!    `Ok(PresentNeeded::Yes)`.
//! 4. **Display resize:** Call [`OverlayController::set_display_size()`] whenever the logical
//!    display size changes, and `Present()` if it returns `Ok(PresentNeeded::Yes)`.

mod debug_font;
mod overlay;
mod renderer;
mod swapchain;
mod touch;
mod vulkan;

use anyhow::{Context, Error, anyhow};
use fidl::endpoints::{ClientEnd, Proxy, create_endpoints, create_request_stream};
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_sysmem2 as fsysmem2;
use fidl_fuchsia_ui_composition as ui_comp;
use fidl_fuchsia_ui_input::MediaButtonsEvent;
use fidl_fuchsia_ui_pointer as fptr;
use fidl_fuchsia_ui_policy::{
    DeviceListenerRegistryMarker, DeviceListenerRegistryProxy, MediaButtonsListenerRequest,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_scenic::flatland::IdGenerator;
use fuchsia_scenic::{
    BufferCollectionTokenPair, duplicate_buffer_collection_import_token,
    duplicate_buffer_collection_token,
};
use fuchsia_trace as ftrace;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use futures::future::LocalBoxFuture;
use log::{info, warn};
use std::rc::Rc;

pub use overlay::{
    DIAGNOSTIC_LINE_COUNT, HudLine, MAX_DIAGNOSTIC_LINE_CHARS, MAX_PRESS_INTERVAL_NS,
    OVERLAY_VISIBLE_PRESS_COUNT, OverlayDisplayMode, REQUIRED_PRESS_COUNT, ShortcutTracker,
    default_hud_lines,
};
pub use renderer::{
    DEFAULT_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY, MIN_OVERLAY_OPACITY, OVERLAY_BUFFER_HEIGHT,
    OVERLAY_BUFFER_WIDTH, compute_overlay_placement,
};
pub use touch::{
    OverlayHitTargets, OverlayLayout, SharedOverlayHitTargets, compute_overlay_layout,
    new_overlay_hit_targets, spawn_touch_source_watcher,
};

use debug_font::{ATLAS_FORMAT, ATLAS_FORMAT_FEATURES};
use renderer::{HudRenderer, OverlayRenderParams, RenderedFrameKey, sanitize_opacity_pct};
use swapchain::{
    AcquireOutcome, AcquiredSlot, RenderedSlot, SWAPCHAIN_FORMAT, SWAPCHAIN_FORMAT_FEATURES,
    Swapchain,
};
use vulkan::VulkanDevice;

/// Triple-buffered sysmem image count: one buffer displayed by Scenic, one in flight in a pending
/// `Flatland::Present()`, and one free for the next render.
const IMAGE_COUNT: usize = 3;

/// Events processed by [`OverlayController`].
#[derive(Debug, PartialEq)]
pub enum OverlayEvent {
    /// Hardware media-button state update from `fuchsia.ui.policy.DeviceListenerRegistry`.
    MediaButtons(MediaButtonsEvent),
    /// Multi-press shortcut timeout tick.
    PressTimeoutTick,
    /// A previously busy framebuffer's Scenic release fence signaled, allowing a deferred render
    /// to proceed.
    ReleaseFenceSignaled,
    /// User tapped the `[X]` close button on the HUD header.
    CloseRequested,
    /// User tapped `[Close]` on the close confirmation prompt.
    CloseConfirmed,
    /// User tapped `[Cancel]` on the close confirmation prompt.
    CloseCancelled,
    /// User dragged the HUD header bar to `(x, y)` in logical view coordinates.
    MoveRequested { x: i32, y: i32 },
    /// User adjusted the HUD opacity slider to `opacity`.
    SetOpacity { opacity: f32 },
}

/// Whether Flatland commands were enqueued and the host must call `Flatland::Present()`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentNeeded {
    /// No Flatland commands were enqueued.
    No,
    /// Flatland commands were enqueued; the host should call `Flatland::Present()`.
    Yes,
}

impl PresentNeeded {
    /// Returns `true` if `PresentNeeded::Yes`.
    #[inline]
    pub const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Returns `true` when the diagnostic overlay listener should be enabled for the given session
/// configuration flags.
#[inline]
pub fn is_overlay_enabled(enable_debug_shortcut: bool, visual_debugging_level: u8) -> bool {
    enable_debug_shortcut || visual_debugging_level > 0
}

/// Connects to `fuchsia.ui.policy.DeviceListenerRegistry` and returns the background listener task
/// forwarding `MediaButtonsEvent`s as [`OverlayEvent::MediaButtons`].
pub fn spawn_media_buttons_listener(
    sender: UnboundedSender<OverlayEvent>,
) -> Option<fasync::Task<()>> {
    let Ok(registry) = connect_to_protocol::<DeviceListenerRegistryMarker>() else {
        warn!("fuchsia.ui.policy.DeviceListenerRegistry not available for diagnostic_overlay");
        return None;
    };
    Some(spawn_media_buttons_listener_with_registry(registry, sender))
}

/// Registers a media-buttons listener on an explicit `registry` proxy and forwards incoming
/// events to `sender`.
pub fn spawn_media_buttons_listener_with_registry(
    registry: DeviceListenerRegistryProxy,
    sender: UnboundedSender<OverlayEvent>,
) -> fasync::Task<()> {
    fasync::Task::spawn(async move {
        let (listener_client, mut listener_stream) = create_request_stream();
        match registry.register_listener(listener_client).await {
            Ok(()) => info!("Registered diagnostic_overlay media buttons listener."),
            Err(e) => {
                warn!("Failed to register diagnostic_overlay media buttons listener: {:?}", e);
                return;
            }
        }
        drop(registry);

        while let Some(item) = listener_stream.next().await {
            let request = match item {
                Ok(request) => request,
                Err(e) => {
                    warn!("MediaButtonsListener stream error: {:?}", e);
                    break;
                }
            };
            if let MediaButtonsListenerRequest::OnEvent { mut event, responder } = request {
                let _ = event.wake_lease.take();
                let _ = responder.send();
                if sender.unbounded_send(OverlayEvent::MediaButtons(event)).is_err() {
                    break;
                }
            }
        }
    })
}

/// Outcome of a renderer frame update attempt.
enum RenderOutcome {
    /// Rendering completed (or was skipped because cached pixels were up to date);
    /// `present_needed` indicates whether Flatland commands were enqueued and require a
    /// `Flatland::Present()`.
    Rendered { present_needed: PresentNeeded },
    /// The next framebuffer's Scenic release fence has not signaled yet; caller should wait
    /// asynchronously on `release_fence` and retry once signaled.
    Deferred { release_fence: zx::Event },
}

/// Presenter abstraction implemented by [`FlatlandOverlayRenderer`] and test fakes.
trait OverlayRenderer {
    fn render_and_attach(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        hit_targets: &SharedOverlayHitTargets,
        params: &OverlayRenderParams<'_>,
    ) -> Result<RenderOutcome, Error>;

    fn update_position(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        layout: &OverlayLayout,
        hit_targets: &SharedOverlayHitTargets,
    ) -> Result<PresentNeeded, Error>;

    fn release(self: Box<Self>, flatland: &ui_comp::FlatlandProxy) -> Result<(), Error>;
}

trait OverlayRendererFactory {
    fn create<'a>(
        &'a mut self,
        flatland: &'a ui_comp::FlatlandProxy,
        id_generator: &'a mut IdGenerator,
        transform_id: ui_comp::TransformId,
        layer_id: ui_comp::LayerId,
    ) -> LocalBoxFuture<'a, Result<Box<dyn OverlayRenderer>, Error>>;
}

struct VulkanRendererFactory;

impl OverlayRendererFactory for VulkanRendererFactory {
    fn create<'a>(
        &'a mut self,
        flatland: &'a ui_comp::FlatlandProxy,
        id_generator: &'a mut IdGenerator,
        transform_id: ui_comp::TransformId,
        layer_id: ui_comp::LayerId,
    ) -> LocalBoxFuture<'a, Result<Box<dyn OverlayRenderer>, Error>> {
        Box::pin(async move {
            let renderer =
                FlatlandOverlayRenderer::new(flatland, id_generator, transform_id, layer_id)
                    .await?;
            Ok(Box::new(renderer) as Box<dyn OverlayRenderer>)
        })
    }
}

#[derive(Default)]
struct LayerState {
    layer_configured: bool,
    last_dest_size: Option<(u32, u32)>,
    last_translation: Option<(i32, i32)>,
    last_hit_regions_key: Option<(u32, u32, bool)>,
    last_applied_opacity: Option<u8>,
    last_blend_is_opaque: Option<bool>,
}

impl LayerState {
    fn apply(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        transform_id: &ui_comp::TransformId,
        layer_id: &ui_comp::LayerId,
        layout: &OverlayLayout,
        hit_targets: &SharedOverlayHitTargets,
        opacity_pct: Option<u8>,
    ) -> Result<PresentNeeded, Error> {
        let mut layer_props = ui_comp::LayerProperties::default();
        let mut update_layer_props = false;
        if !self.layer_configured {
            layer_props.composition_mode = Some(ui_comp::CompositionMode::Image);
            update_layer_props = true;
        }
        if self.last_dest_size != Some((layout.dest_w, layout.dest_h)) {
            layer_props.display_rect =
                Some(fmath::RectU { x: 0, y: 0, width: layout.dest_w, height: layout.dest_h });
            update_layer_props = true;
        }
        if let Some(pct) = opacity_pct {
            let is_opaque = pct >= 100;
            let applied_opacity = f32::from(pct) / 100.0;
            if self.last_applied_opacity != Some(pct) {
                layer_props.opacity = Some(applied_opacity);
                update_layer_props = true;
            }
            if self.last_blend_is_opaque != Some(is_opaque) {
                layer_props.blend_mode = Some(if is_opaque {
                    ui_comp::BlendMode2::Replace
                } else {
                    ui_comp::BlendMode2::StraightAlpha
                });
                update_layer_props = true;
            }
        }

        let mut present_needed = PresentNeeded::No;
        if update_layer_props {
            flatland
                .set_layer_properties(layer_id, &layer_props)
                .context("set diagnostic overlay layer properties")?;
            self.layer_configured = true;
            self.last_dest_size = Some((layout.dest_w, layout.dest_h));
            if let Some(pct) = opacity_pct {
                self.last_applied_opacity = Some(pct);
                self.last_blend_is_opaque = Some(pct >= 100);
            }
            present_needed = PresentNeeded::Yes;
        }

        if self.last_translation != Some((layout.x, layout.y)) {
            flatland
                .set_translation(transform_id, &fmath::Vec_ { x: layout.x, y: layout.y })
                .context("set diagnostic overlay translation")?;
            self.last_translation = Some((layout.x, layout.y));
            present_needed = PresentNeeded::Yes;
        }

        let hit_key = (layout.dest_w, layout.dest_h, layout.confirm_close);
        if self.last_hit_regions_key != Some(hit_key) {
            flatland
                .set_hit_regions(transform_id, &layout.local_hit_regions)
                .context("set diagnostic overlay hit regions")?;
            self.last_hit_regions_key = Some(hit_key);
            present_needed = PresentNeeded::Yes;
        }

        *hit_targets.lock() = Some(layout.targets);
        Ok(present_needed)
    }
}

fn create_flatland_images<const N: usize>(
    flatland: &ui_comp::FlatlandProxy,
    id_generator: &mut IdGenerator,
    collection_import_token: &ui_comp::BufferCollectionImportToken,
    width: u32,
    height: u32,
) -> Result<[ui_comp::ImageId; N], Error> {
    let image_ids: [ui_comp::ImageId; N] =
        std::array::from_fn(|_| ui_comp::ImageId { value: id_generator.next_content_id().value });
    let image_props = ui_comp::ImageProperties {
        size: Some(fmath::SizeU { width, height }),
        ..Default::default()
    };
    for (idx, image_id) in image_ids.iter().enumerate() {
        let create_res = (|| -> Result<(), Error> {
            let import_token = duplicate_buffer_collection_import_token(collection_import_token)?;
            flatland
                .create_image2(image_id, import_token, idx as u32, &image_props)
                .context("Flatland::CreateImage2")
        })();
        if let Err(e) = create_res {
            for prev_id in &image_ids[..idx] {
                let _ = flatland.release_image2(prev_id);
            }
            return Err(e);
        }
    }
    Ok(image_ids)
}

fn release_flatland_images(
    flatland: &ui_comp::FlatlandProxy,
    image_ids: &[ui_comp::ImageId],
) -> Result<(), Error> {
    let mut first_err: Option<Error> = None;
    for image_id in image_ids {
        if let Err(e) =
            flatland.release_image2(image_id).context("release diagnostic overlay image")
        {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Internal seam allowing [`FlatlandOverlayRenderer`] to be unit-tested without a live Vulkan ICD.
trait OverlaySwapchain {
    type Renderer;

    fn acquire_next(&mut self) -> Result<AcquireOutcome, Error>;

    fn render_frame(
        &mut self,
        renderer: &mut Self::Renderer,
        slot: AcquiredSlot,
        params: &OverlayRenderParams<'_>,
    ) -> Result<RenderedSlot, Error>;
}

impl OverlaySwapchain for Swapchain<IMAGE_COUNT> {
    type Renderer = HudRenderer;

    fn acquire_next(&mut self) -> Result<AcquireOutcome, Error> {
        Swapchain::acquire_next(self)
    }

    fn render_frame(
        &mut self,
        renderer: &mut HudRenderer,
        slot: AcquiredSlot,
        params: &OverlayRenderParams<'_>,
    ) -> Result<RenderedSlot, Error> {
        self.render_slot(slot, |ctx| renderer.draw_frame(ctx, params))
    }
}

/// Presents the GPU-rendered diagnostic overlay onto a single Flatland2 layer by coordinating
/// [`Swapchain`], [`HudRenderer`], and [`LayerState`].
///
/// # Drop Order Invariant
///
/// `swapchain` is declared before `renderer` so in-flight swapchain fences finish before
/// [`crate::debug_font::DebugFont`] drops.
struct FlatlandOverlayRenderer<S: OverlaySwapchain = Swapchain<IMAGE_COUNT>> {
    swapchain: S,
    renderer: S::Renderer,
    image_ids: [ui_comp::ImageId; IMAGE_COUNT],
    transform_id: ui_comp::TransformId,
    layer_id: ui_comp::LayerId,
    layer_state: LayerState,
    last_rendered_frame: Option<(RenderedFrameKey, u8)>,
}

impl<S: OverlaySwapchain> OverlayRenderer for FlatlandOverlayRenderer<S> {
    fn render_and_attach(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        hit_targets: &SharedOverlayHitTargets,
        params: &OverlayRenderParams<'_>,
    ) -> Result<RenderOutcome, Error> {
        FlatlandOverlayRenderer::render_and_attach(self, flatland, hit_targets, params)
    }

    fn update_position(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        layout: &OverlayLayout,
        hit_targets: &SharedOverlayHitTargets,
    ) -> Result<PresentNeeded, Error> {
        FlatlandOverlayRenderer::update_position(self, flatland, layout, hit_targets)
    }

    fn release(self: Box<Self>, flatland: &ui_comp::FlatlandProxy) -> Result<(), Error> {
        FlatlandOverlayRenderer::release(*self, flatland)
    }
}

impl FlatlandOverlayRenderer<Swapchain<IMAGE_COUNT>> {
    /// Initializes a Vulkan instance/device, allocates a triple-buffered sysmem2 swapchain shared
    /// with Scenic, registers the Flatland2 images, and uploads the [`HudRenderer`] font atlas.
    async fn new(
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        transform_id: ui_comp::TransformId,
        layer_id: ui_comp::LayerId,
    ) -> Result<Self, Error> {
        let _trace =
            ftrace::async_enter!(ftrace::Id::new(), c"gfx", c"FlatlandOverlayRenderer::new");

        let sysmem_allocator = connect_to_protocol::<fsysmem2::AllocatorMarker>()
            .context("connect to fuchsia.sysmem2.Allocator")?;
        let _ =
            sysmem_allocator.set_debug_client_info(&fsysmem2::AllocatorSetDebugClientInfoRequest {
                name: Some("diagnostic_overlay_vk".to_string()),
                id: Some(fuchsia_runtime::process_self().koid()?.raw_koid()),
                ..Default::default()
            });
        let (token_client, token_server) =
            create_endpoints::<fsysmem2::BufferCollectionTokenMarker>();
        sysmem_allocator.allocate_shared_collection(
            fsysmem2::AllocatorAllocateSharedCollectionRequest {
                token_request: Some(token_server),
                ..Default::default()
            },
        )?;

        let mut token_proxy = token_client.into_proxy();
        let vulkan_token = duplicate_buffer_collection_token(&mut token_proxy)
            .await
            .context("duplicate sysmem token for Vulkan")?;
        let scenic_token = ClientEnd::<fsysmem2::BufferCollectionTokenMarker>::new(
            token_proxy
                .into_channel()
                .map_err(|_| anyhow!("into_channel() failed"))?
                .into_zx_channel(),
        );

        let import_export_tokens = BufferCollectionTokenPair::new();
        let scenic_allocator = connect_to_protocol::<ui_comp::AllocatorMarker>()
            .context("connect to fuchsia.ui.composition.Allocator")?;
        scenic_allocator
            .register_buffer_collection(ui_comp::RegisterBufferCollectionArgs {
                export_token: Some(import_export_tokens.export_token),
                buffer_collection_token2: Some(scenic_token),
                ..Default::default()
            })
            .await
            .context("register_buffer_collection() FIDL")?
            .map_err(|e| anyhow!("register_buffer_collection() failed: {e:?}"))?;

        const {
            assert!(SWAPCHAIN_FORMAT.as_raw() == ATLAS_FORMAT.as_raw());
        }
        let vulkan = VulkanDevice::new(
            c"diagnostic_overlay",
            SWAPCHAIN_FORMAT,
            SWAPCHAIN_FORMAT_FEATURES | ATLAS_FORMAT_FEATURES,
        )
        .context("initialize Vulkan device")?;
        let swapchain = Swapchain::<IMAGE_COUNT>::new(
            Rc::clone(&vulkan),
            vulkan_token,
            OVERLAY_BUFFER_WIDTH,
            OVERLAY_BUFFER_HEIGHT,
        )?;
        let renderer = HudRenderer::new(vulkan)?;
        let image_ids = create_flatland_images::<IMAGE_COUNT>(
            flatland,
            id_generator,
            &import_export_tokens.import_token,
            OVERLAY_BUFFER_WIDTH,
            OVERLAY_BUFFER_HEIGHT,
        )?;

        Ok(Self {
            swapchain,
            renderer,
            image_ids,
            transform_id,
            layer_id,
            layer_state: LayerState::default(),
            last_rendered_frame: None,
        })
    }
}

impl<S: OverlaySwapchain> FlatlandOverlayRenderer<S> {
    /// Updates Flatland2 layer geometry, translation, hit-test regions, and shared view-space
    /// [`OverlayHitTargets`] during a header drag without re-rendering Vulkan framebuffers.
    pub(crate) fn update_position(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        layout: &OverlayLayout,
        hit_targets: &SharedOverlayHitTargets,
    ) -> Result<PresentNeeded, Error> {
        self.layer_state.apply(
            flatland,
            &self.transform_id,
            &self.layer_id,
            layout,
            hit_targets,
            None,
        )
    }

    /// Releases all Flatland2 `ImageId`s owned by this presenter so `Self` can be dropped when the
    /// overlay is dismissed, freeing all sysmem VMOs and Vulkan resources.
    ///
    /// Live-session callers must also unbind the layer (via `Flatland::ResetLayer()`,
    /// `Flatland::ResetLayerImage()`, or `Flatland::ReleaseLayer()`) before presenting so Scenic
    /// releases the currently displayed buffer.
    fn release(self, flatland: &ui_comp::FlatlandProxy) -> Result<(), Error> {
        release_flatland_images(flatland, &self.image_ids)
    }

    /// Renders the overlay frame on the GPU into the next triple-buffered sysmem `vk::Image` via
    /// [`Swapchain`] when the HUD content or opacity changes, and updates [`LayerState`] and
    /// `SetLayerImage()` on `self.layer_id`.
    fn render_and_attach(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        hit_targets: &SharedOverlayHitTargets,
        params: &OverlayRenderParams<'_>,
    ) -> Result<RenderOutcome, Error> {
        ftrace::duration!(c"gfx", c"FlatlandOverlayRenderer::render_and_attach");
        let opacity_pct = sanitize_opacity_pct(params.opacity);
        let current_frame = (RenderedFrameKey::from_params(params), opacity_pct);
        let layout =
            compute_overlay_layout(params.display_size, params.custom_origin, params.confirm_close);

        if self.last_rendered_frame == Some(current_frame) {
            let present_needed = self.layer_state.apply(
                flatland,
                &self.transform_id,
                &self.layer_id,
                &layout,
                hit_targets,
                Some(opacity_pct),
            )?;
            return Ok(RenderOutcome::Rendered { present_needed });
        }

        let slot = match self.swapchain.acquire_next()? {
            AcquireOutcome::Deferred { release_fence } => {
                return Ok(RenderOutcome::Deferred { release_fence });
            }
            AcquireOutcome::Ready(slot) => slot,
        };

        let rendered = self.swapchain.render_frame(&mut self.renderer, slot, params)?;

        let _ = self.layer_state.apply(
            flatland,
            &self.transform_id,
            &self.layer_id,
            &layout,
            hit_targets,
            Some(opacity_pct),
        )?;
        flatland
            .set_layer_image(
                &self.layer_id,
                &self.image_ids[rendered.slot_index],
                Some(ui_comp::WaitFence::Basic(rendered.acquire_fence.into_handle())),
                Some(ui_comp::SignalFence::Basic(rendered.release_fence.into_handle())),
            )
            .context("set diagnostic overlay layer image")?;
        self.last_rendered_frame = Some(current_frame);
        Ok(RenderOutcome::Rendered { present_needed: PresentNeeded::Yes })
    }
}

/// Manages the diagnostic overlay transform, Flatland2 layer, and touch controls on a host
/// session.
///
/// See the crate-level documentation for the host call sequence.
pub struct OverlayController {
    enabled: bool,
    overlay_transform_id: ui_comp::TransformId,
    overlay_layer_id: ui_comp::LayerId,
    display_size: fmath::SizeU,
    custom_origin: Option<(i32, i32)>,
    opacity: f32,
    render_dirty: bool,
    position_dirty: bool,
    hit_targets: SharedOverlayHitTargets,
    shortcut_tracker: ShortcutTracker,
    renderer: Option<Box<dyn OverlayRenderer>>,
    renderer_factory: Box<dyn OverlayRendererFactory>,
    renderer_init_failed: bool,
    event_sender: UnboundedSender<OverlayEvent>,
    timer_task: Option<fasync::Task<()>>,
    release_fence_wait_task: Option<fasync::Task<()>>,
    waiting_release_fence_koid: Option<zx::Koid>,
    _media_listener_task: Option<fasync::Task<()>>,
    _touch_watcher_task: Option<fasync::Task<()>>,
}

impl std::fmt::Debug for OverlayController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayController")
            .field("enabled", &self.enabled)
            .field("overlay_transform_id", &self.overlay_transform_id)
            .field("display_size", &self.display_size)
            .field("custom_origin", &self.custom_origin)
            .field("opacity", &self.opacity)
            .field("mode", &self.shortcut_tracker.mode())
            .field("confirm_close", &self.shortcut_tracker.confirm_close())
            .field("render_dirty", &self.render_dirty)
            .field("position_dirty", &self.position_dirty)
            .field("has_renderer", &self.renderer.is_some())
            .field("renderer_init_failed", &self.renderer_init_failed)
            .finish_non_exhaustive()
    }
}

impl OverlayController {
    /// Creates an [`OverlayController`] backed by a dedicated Flatland2 transform, layer, and
    /// layer stack on `flatland`.
    ///
    /// When `enabled` is `false`, zero Flatland commands, background tasks, or protocol connections
    /// are created.
    ///
    /// # Errors
    ///
    /// Returns an error if creating or wiring the initial Flatland2 transform, hit regions, layer,
    /// or layer stack fails on `flatland`.
    pub fn new(
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        display_size: fmath::SizeU,
        enabled: bool,
        touch_source: Option<fptr::TouchSourceProxy>,
        event_sender: UnboundedSender<OverlayEvent>,
    ) -> Result<Self, Error> {
        let touch_sender = enabled.then(|| event_sender.clone());
        let mut controller = Self::new_with_factory(
            flatland,
            id_generator,
            display_size,
            enabled,
            enabled,
            event_sender,
            Box::new(VulkanRendererFactory),
        )?;
        if let (Some(ts), Some(sender)) = (touch_source, touch_sender) {
            info!("Enabling diagnostic_overlay touch source watcher.");
            controller._touch_watcher_task =
                Some(spawn_touch_source_watcher(ts, controller.hit_targets.clone(), sender));
        }
        Ok(controller)
    }

    fn new_with_factory(
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        display_size: fmath::SizeU,
        enabled: bool,
        spawn_listener: bool,
        event_sender: UnboundedSender<OverlayEvent>,
        renderer_factory: Box<dyn OverlayRendererFactory>,
    ) -> Result<Self, Error> {
        let (overlay_transform_id, overlay_layer_id) = if enabled {
            let transform_id = id_generator.next_transform_id();
            flatland.create_transform(&transform_id)?;
            // Clear hit regions initially so the overlay never intercepts pointer input while hidden.
            flatland.set_hit_regions(&transform_id, &[])?;

            let layer_id = ui_comp::LayerId { value: id_generator.next_content_id().value };
            let layer_stack_id =
                ui_comp::LayerStackId { value: id_generator.next_content_id().value };
            flatland.create_layer(&layer_id)?;
            flatland.create_layer_stack(&layer_stack_id)?;
            flatland.set_stack_layers(&layer_stack_id, &[layer_id])?;
            flatland.set_transform_content(
                &transform_id,
                Some(&ui_comp::TransformContent::LayerStack(layer_stack_id)),
            )?;
            (transform_id, layer_id)
        } else {
            (ui_comp::TransformId { value: 0 }, ui_comp::LayerId { value: 0 })
        };

        let hit_targets = new_overlay_hit_targets();
        let media_listener_task = if enabled && spawn_listener {
            info!("Enabling diagnostic_overlay media buttons listener.");
            spawn_media_buttons_listener(event_sender.clone())
        } else {
            None
        };

        Ok(Self {
            enabled,
            overlay_transform_id,
            overlay_layer_id,
            display_size,
            custom_origin: None,
            opacity: DEFAULT_OVERLAY_OPACITY,
            render_dirty: false,
            position_dirty: false,
            hit_targets,
            shortcut_tracker: ShortcutTracker::new(),
            renderer: None,
            renderer_factory,
            renderer_init_failed: false,
            event_sender,
            timer_task: None,
            release_fence_wait_task: None,
            waiting_release_fence_koid: None,
            _media_listener_task: media_listener_task,
            _touch_watcher_task: None,
        })
    }

    /// Returns the Flatland [`ui_comp::TransformId`] owned by this overlay.
    #[inline]
    pub fn overlay_transform_id(&self) -> ui_comp::TransformId {
        self.overlay_transform_id
    }

    /// Returns the current [`OverlayDisplayMode`].
    #[inline]
    pub fn mode(&self) -> OverlayDisplayMode {
        self.shortcut_tracker.mode()
    }

    /// Updates the logical display size used for HUD placement and hit regions, triggering a
    /// re-render if the overlay is currently visible.
    ///
    /// Returns `Ok(PresentNeeded::Yes)` if Flatland2 commands were enqueued on `flatland` and the
    /// host should call `Flatland::Present()`.
    ///
    /// # Errors
    ///
    /// Returns an error if issuing Flatland2 placement or layer updates fails.
    pub async fn set_display_size(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        has_present_credit: bool,
        display_size: fmath::SizeU,
    ) -> Result<PresentNeeded, Error> {
        if !self.enabled || self.display_size == display_size {
            return Ok(PresentNeeded::No);
        }
        self.display_size = display_size;
        if self.shortcut_tracker.mode().is_visible() {
            if !self.renderer_init_failed {
                let layout = compute_overlay_layout(
                    self.display_size,
                    self.custom_origin,
                    self.shortcut_tracker.confirm_close(),
                );
                *self.hit_targets.lock() = Some(layout.targets);
                self.position_dirty = true;
            }
            self.request_sync_overlay(flatland, id_generator, has_present_credit).await
        } else {
            Ok(PresentNeeded::No)
        }
    }

    /// Processes an [`OverlayEvent`] and returns `Ok(PresentNeeded::Yes)` if Flatland2 commands
    /// were enqueued on `flatland` so the caller should schedule a `Flatland::Present()`.
    ///
    /// When `has_present_credit` is `false` (until present credits are deprecated in
    /// `b/549201450`), visible updates are marked dirty and `Ok(PresentNeeded::No)` is returned;
    /// call [`OverlayController::flush_pending()`] once `Flatland::OnNextFrameBegin` grants
    /// credits.
    ///
    /// # Errors
    ///
    /// Returns an error if issuing Flatland2 scene-graph or layer commands fails.
    pub async fn handle_event(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        has_present_credit: bool,
        event: OverlayEvent,
    ) -> Result<PresentNeeded, Error> {
        if !self.enabled {
            return Ok(PresentNeeded::No);
        }
        match event {
            OverlayEvent::MediaButtons(media_event) => {
                let now_ns = fasync::BootInstant::now().into_nanos();
                let prev_mode = self.shortcut_tracker.mode();
                let prev_count = self.shortcut_tracker.count();
                let prev_confirm = self.shortcut_tracker.confirm_close();
                let mode = self.shortcut_tracker.on_media_buttons_event(&media_event, now_ns);

                // Arm/refresh the timeout timer before any `.await` so async renderer setup latency
                // or early errors never delay or drop the press-interval deadline.
                if mode != prev_mode || self.shortcut_tracker.count() != prev_count {
                    self.rearm_timer(now_ns);
                }
                if mode != prev_mode || self.shortcut_tracker.confirm_close() != prev_confirm {
                    self.request_sync_overlay(flatland, id_generator, has_present_credit).await
                } else {
                    Ok(PresentNeeded::No)
                }
            }
            OverlayEvent::PressTimeoutTick => {
                let now_ns = fasync::BootInstant::now().into_nanos();
                let prev_mode = self.shortcut_tracker.mode();
                let mode = self.shortcut_tracker.on_timer_tick(now_ns);
                self.rearm_timer(now_ns);
                if mode != prev_mode {
                    self.request_sync_overlay(flatland, id_generator, has_present_credit).await
                } else {
                    Ok(PresentNeeded::No)
                }
            }
            OverlayEvent::ReleaseFenceSignaled => {
                self.release_fence_wait_task = None;
                self.waiting_release_fence_koid = None;
                if self.shortcut_tracker.mode().is_visible() {
                    self.request_sync_overlay(flatland, id_generator, has_present_credit).await
                } else {
                    Ok(PresentNeeded::No)
                }
            }
            OverlayEvent::CloseRequested => {
                let prev_mode = self.shortcut_tracker.mode();
                let prev_confirm = self.shortcut_tracker.confirm_close();
                self.shortcut_tracker.request_close();
                self.sync_if_display_changed(
                    flatland,
                    id_generator,
                    has_present_credit,
                    prev_mode,
                    prev_confirm,
                )
                .await
            }
            OverlayEvent::CloseConfirmed => {
                let prev_mode = self.shortcut_tracker.mode();
                let prev_confirm = self.shortcut_tracker.confirm_close();
                self.shortcut_tracker.dismiss();
                self.sync_if_display_changed(
                    flatland,
                    id_generator,
                    has_present_credit,
                    prev_mode,
                    prev_confirm,
                )
                .await
            }
            OverlayEvent::CloseCancelled => {
                let prev_mode = self.shortcut_tracker.mode();
                let prev_confirm = self.shortcut_tracker.confirm_close();
                self.shortcut_tracker.cancel_close();
                self.sync_if_display_changed(
                    flatland,
                    id_generator,
                    has_present_credit,
                    prev_mode,
                    prev_confirm,
                )
                .await
            }
            OverlayEvent::MoveRequested { x, y } => {
                let (clamped_x, clamped_y, _, _) =
                    compute_overlay_placement(self.display_size, Some((x, y)));
                if self.custom_origin != Some((clamped_x, clamped_y)) {
                    self.custom_origin = Some((clamped_x, clamped_y));
                    if self.shortcut_tracker.mode().is_visible() && !self.renderer_init_failed {
                        let layout = compute_overlay_layout(
                            self.display_size,
                            self.custom_origin,
                            self.shortcut_tracker.confirm_close(),
                        );
                        if has_present_credit
                            && !self.render_dirty
                            && let Some(renderer) = &mut self.renderer
                        {
                            self.position_dirty = false;
                            return renderer.update_position(flatland, &layout, &self.hit_targets);
                        }
                        *self.hit_targets.lock() = Some(layout.targets);
                        self.position_dirty = true;
                    }
                }
                Ok(PresentNeeded::No)
            }
            OverlayEvent::SetOpacity { opacity } => {
                let clamped = renderer::sanitize_opacity(opacity);
                let quantized = (clamped * 100.0).round() / 100.0;
                if (self.opacity - quantized).abs() >= 0.005 {
                    self.opacity = quantized;
                    if self.shortcut_tracker.mode().is_visible() {
                        return self
                            .request_sync_overlay(flatland, id_generator, has_present_credit)
                            .await;
                    }
                }
                Ok(PresentNeeded::No)
            }
        }
    }

    /// Flushes any deferred overlay render or drag position update once the host receives
    /// `Flatland::OnNextFrameBegin` present credits.
    ///
    /// Returns `Ok(PresentNeeded::Yes)` if Flatland2 commands were enqueued on `flatland` so the
    /// caller should schedule a `Flatland::Present()`.
    ///
    /// # Errors
    ///
    /// Returns an error if issuing Flatland2 scene-graph or layer commands fails.
    pub async fn flush_pending(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
    ) -> Result<PresentNeeded, Error> {
        if !self.enabled {
            return Ok(PresentNeeded::No);
        }
        if self.render_dirty {
            self.sync_overlay(flatland, id_generator).await
        } else if self.position_dirty {
            self.flush_position(flatland)
        } else {
            Ok(PresentNeeded::No)
        }
    }

    async fn sync_if_display_changed(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        has_present_credit: bool,
        prev_mode: OverlayDisplayMode,
        prev_confirm: bool,
    ) -> Result<PresentNeeded, Error> {
        if self.shortcut_tracker.mode() != prev_mode
            || self.shortcut_tracker.confirm_close() != prev_confirm
        {
            self.request_sync_overlay(flatland, id_generator, has_present_credit).await
        } else {
            Ok(PresentNeeded::No)
        }
    }

    fn flush_position(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
    ) -> Result<PresentNeeded, Error> {
        self.position_dirty = false;
        if self.shortcut_tracker.mode().is_visible()
            && !self.renderer_init_failed
            && let Some(renderer) = &mut self.renderer
        {
            let layout = compute_overlay_layout(
                self.display_size,
                self.custom_origin,
                self.shortcut_tracker.confirm_close(),
            );
            renderer.update_position(flatland, &layout, &self.hit_targets)
        } else {
            Ok(PresentNeeded::No)
        }
    }

    /// Syncs overlay rendering and Flatland layer state when present credits are available, or
    /// marks the overlay dirty and returns `Ok(PresentNeeded::No)` to defer until
    /// [`Self::flush_pending()`].
    async fn request_sync_overlay(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
        has_present_credit: bool,
    ) -> Result<PresentNeeded, Error> {
        let mode = self.shortcut_tracker.mode();
        if !mode.is_visible() {
            *self.hit_targets.lock() = None;
        }
        // A no-op hide when no renderer or layer image is allocated enqueues zero Flatland
        // commands, so it can clear pending latches immediately without waiting for a credit.
        if has_present_credit || (!mode.is_visible() && self.renderer.is_none()) {
            self.sync_overlay(flatland, id_generator).await
        } else {
            self.render_dirty = true;
            Ok(PresentNeeded::No)
        }
    }

    /// Synchronizes Vulkan rendering and Flatland2 layer commands with the current overlay state,
    /// returning `Ok(PresentNeeded::Yes)` when Flatland commands were enqueued and require a
    /// `Flatland::Present()`.
    async fn sync_overlay(
        &mut self,
        flatland: &ui_comp::FlatlandProxy,
        id_generator: &mut IdGenerator,
    ) -> Result<PresentNeeded, Error> {
        let mode = self.shortcut_tracker.mode();

        if !mode.is_visible() {
            // Tear down all overlay rendering state on hide: reset the Flatland2 layer to
            // `CompositionMode::Invisible` (which unbinds its image without mutating the transform
            // or layer-stack graph), release the Flatland2 image IDs, and drop the Vulkan renderer.
            self.render_dirty = false;
            self.position_dirty = false;
            self.renderer_init_failed = false;
            self.release_fence_wait_task = None;
            self.waiting_release_fence_koid = None;
            *self.hit_targets.lock() = None;
            if let Some(renderer) = self.renderer.take() {
                flatland
                    .set_hit_regions(&self.overlay_transform_id, &[])
                    .context("clear diagnostic overlay hit regions")?;
                flatland
                    .reset_layer(&self.overlay_layer_id)
                    .context("reset diagnostic overlay layer")?;
                // Full teardown on hide trades show-time Vulkan/sysmem setup latency on the host
                // loop for zero GPU, sysmem VMO, and ICD memory footprint while hidden/idle.
                renderer.release(flatland)?;
                return Ok(PresentNeeded::Yes);
            }
            return Ok(PresentNeeded::No);
        }

        if self.renderer.is_none() && !self.renderer_init_failed {
            match self
                .renderer_factory
                .create(flatland, id_generator, self.overlay_transform_id, self.overlay_layer_id)
                .await
            {
                Ok(renderer) => self.renderer = Some(renderer),
                Err(e) => {
                    warn!("Failed to initialize Flatland diagnostic overlay renderer: {:?}", e);
                    self.render_dirty = false;
                    self.position_dirty = false;
                    self.renderer_init_failed = true;
                    return Ok(PresentNeeded::No);
                }
            }
        }
        // Clear pending latches after async initialization succeeds so canceling the future at
        // `.await` preserves the pending update for `flush_pending()`.
        self.render_dirty = false;
        self.position_dirty = false;

        let Some(renderer) = &mut self.renderer else {
            return Ok(PresentNeeded::No);
        };

        let hud_lines = default_hud_lines();
        // TODO(https://fxbug.dev/571154588): Wire live diagnostic lines and increment
        // `hud_generation` on updates.
        let params = OverlayRenderParams {
            display_size: self.display_size,
            custom_origin: self.custom_origin,
            mode,
            confirm_close: self.shortcut_tracker.confirm_close(),
            opacity: self.opacity,
            hud_generation: 0,
            hud_lines: &hud_lines,
        };
        let render_outcome = match renderer.render_and_attach(flatland, &self.hit_targets, &params)
        {
            Ok(outcome) => outcome,
            Err(e) => {
                // Treat per-frame render/submit failures as fatal for this renderer instance so the
                // next show starts with fresh Vulkan fences and buffers rather than reusing a
                // potentially broken buffer or fence state.
                warn!("Failed to render diagnostic overlay frame: {:?}", e);
                self.release_fence_wait_task = None;
                self.waiting_release_fence_koid = None;
                self.renderer_init_failed = true;
                let broken_renderer = self.renderer.take();
                flatland
                    .set_hit_regions(&self.overlay_transform_id, &[])
                    .context("clear diagnostic overlay hit regions after render failure")?;
                flatland
                    .reset_layer(&self.overlay_layer_id)
                    .context("reset diagnostic overlay layer after render failure")?;
                if let Some(renderer) = broken_renderer {
                    renderer.release(flatland)?;
                }
                return Ok(PresentNeeded::Yes);
            }
        };

        match render_outcome {
            RenderOutcome::Deferred { release_fence } => {
                let fence_koid = release_fence.koid().ok();
                if self.release_fence_wait_task.is_none()
                    || self.waiting_release_fence_koid != fence_koid
                {
                    self.waiting_release_fence_koid = fence_koid;
                    let sender = self.event_sender.clone();
                    self.release_fence_wait_task = Some(fasync::Task::spawn(async move {
                        let _ = fasync::OnSignals::new(&release_fence, zx::Signals::EVENT_SIGNALED)
                            .await;
                        let _ = sender.unbounded_send(OverlayEvent::ReleaseFenceSignaled);
                    }));
                }
                Ok(PresentNeeded::No)
            }
            RenderOutcome::Rendered { present_needed } => Ok(present_needed),
        }
    }

    fn rearm_timer(&mut self, now_ns: i64) {
        if let Some(delay_ns) = self.shortcut_tracker.next_timer_delay_ns(now_ns) {
            let sender = self.event_sender.clone();
            let deadline = fasync::BootInstant::from_nanos(now_ns.saturating_add(delay_ns));
            self.timer_task = Some(fasync::Task::spawn(async move {
                fasync::Timer::new(deadline).await;
                let _ = sender.unbounded_send(OverlayEvent::PressTimeoutTick);
            }));
        } else {
            self.timer_task = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{create_proxy, create_proxy_and_stream};
    use fidl_fuchsia_ui_policy::DeviceListenerRegistryRequest;
    use futures::channel::mpsc::{UnboundedReceiver, unbounded};
    use futures::{FutureExt, TryStreamExt};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::task::Poll;

    /// Connects a `FlatlandProxy` via `FlatlandFactory::CreateFlatland()` with
    /// `FlatlandConfig { use_flatland2: Some(true), ..Default::default() }`, verifying the
    /// Flatland2 session opt-in handshake while returning the server `FlatlandRequestStream`.
    async fn create_flatland2_proxy_and_stream()
    -> (ui_comp::FlatlandProxy, ui_comp::FlatlandRequestStream) {
        let (factory_proxy, mut factory_stream) =
            create_proxy_and_stream::<ui_comp::FlatlandFactoryMarker>();
        let (flatland_proxy, flatland_server_end) = create_proxy::<ui_comp::FlatlandMarker>();
        let (res, stream) = futures::join!(
            factory_proxy.create_flatland(
                flatland_server_end,
                &ui_comp::FlatlandConfig { use_flatland2: Some(true), ..Default::default() },
            ),
            async {
                let Some(ui_comp::FlatlandFactoryRequest::CreateFlatland {
                    server_end,
                    config,
                    responder,
                }) = factory_stream.try_next().await.expect("CreateFlatland request")
                else {
                    panic!("Expected FlatlandFactory::CreateFlatland");
                };
                assert_eq!(config.use_flatland2, Some(true));
                let stream = server_end.into_stream();
                responder.send(Ok(())).expect("ack CreateFlatland");
                stream
            }
        );
        res.expect("CreateFlatland FIDL call succeeded").expect("CreateFlatland returned Ok");
        (flatland_proxy, stream)
    }

    fn press_event() -> MediaButtonsEvent {
        MediaButtonsEvent { volume_up: Some(true), volume_down: Some(true), ..Default::default() }
    }

    fn release_event() -> MediaButtonsEvent {
        MediaButtonsEvent { volume_up: Some(false), volume_down: Some(false), ..Default::default() }
    }

    #[derive(Default)]
    struct FakeRendererState {
        create_calls: usize,
        last_transform_id: Option<ui_comp::TransformId>,
        fail_create: bool,
        create_delay: Option<zx::BootDuration>,
        fail_render: bool,
        render_calls: usize,
        update_position_calls: usize,
        last_display_size: Option<fmath::SizeU>,
        release_calls: usize,
        deferred_fence: Option<zx::Event>,
    }

    struct FakeRendererFactory {
        state: Rc<RefCell<FakeRendererState>>,
    }

    impl OverlayRendererFactory for FakeRendererFactory {
        fn create<'a>(
            &'a mut self,
            flatland: &'a ui_comp::FlatlandProxy,
            id_generator: &'a mut IdGenerator,
            transform_id: ui_comp::TransformId,
            layer_id: ui_comp::LayerId,
        ) -> LocalBoxFuture<'a, Result<Box<dyn OverlayRenderer>, Error>> {
            let state = self.state.clone();
            Box::pin(async move {
                let delay = {
                    let mut st = state.borrow_mut();
                    st.create_calls += 1;
                    st.last_transform_id = Some(transform_id);
                    if st.fail_create {
                        return Err(anyhow!("simulated renderer init failure"));
                    }
                    st.create_delay
                };
                if let Some(delay) = delay {
                    fasync::Timer::new(fasync::BootInstant::after(delay)).await;
                }
                let image_id = ui_comp::ImageId { value: id_generator.next_content_id().value };
                let (token_a, _token_b) = zx::EventPair::create();
                flatland.create_image2(
                    &image_id,
                    ui_comp::BufferCollectionImportToken { value: token_a },
                    0,
                    &ui_comp::ImageProperties::default(),
                )?;
                Ok(
                    Box::new(FakeRenderer {
                        state: state.clone(),
                        image_id,
                        transform_id,
                        layer_id,
                    }) as Box<dyn OverlayRenderer>,
                )
            })
        }
    }

    struct FakeRenderer {
        state: Rc<RefCell<FakeRendererState>>,
        image_id: ui_comp::ImageId,
        transform_id: ui_comp::TransformId,
        layer_id: ui_comp::LayerId,
    }

    impl OverlayRenderer for FakeRenderer {
        fn render_and_attach(
            &mut self,
            flatland: &ui_comp::FlatlandProxy,
            hit_targets: &SharedOverlayHitTargets,
            params: &OverlayRenderParams<'_>,
        ) -> Result<RenderOutcome, Error> {
            let mut st = self.state.borrow_mut();
            st.render_calls += 1;
            st.last_display_size = Some(params.display_size);
            if st.fail_render {
                return Err(anyhow!("simulated render failure"));
            }
            if let Some(fence) = st.deferred_fence.as_ref() {
                if matches!(
                    fence
                        .wait_one(zx::Signals::EVENT_SIGNALED, zx::MonotonicInstant::INFINITE_PAST),
                    zx::WaitResult::TimedOut(_)
                ) {
                    let dup = fence.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
                    return Ok(RenderOutcome::Deferred { release_fence: dup });
                }
                st.deferred_fence = None;
            }
            let layout = compute_overlay_layout(
                params.display_size,
                params.custom_origin,
                params.confirm_close,
            );
            flatland.set_layer_properties(
                &self.layer_id,
                &ui_comp::LayerProperties {
                    composition_mode: Some(ui_comp::CompositionMode::Image),
                    display_rect: Some(fmath::RectU {
                        x: 0,
                        y: 0,
                        width: layout.dest_w,
                        height: layout.dest_h,
                    }),
                    ..Default::default()
                },
            )?;
            flatland
                .set_translation(&self.transform_id, &fmath::Vec_ { x: layout.x, y: layout.y })?;
            flatland.set_hit_regions(&self.transform_id, &layout.local_hit_regions)?;
            flatland.set_layer_image(&self.layer_id, &self.image_id, None, None)?;
            *hit_targets.lock() = Some(layout.targets);
            Ok(RenderOutcome::Rendered { present_needed: PresentNeeded::Yes })
        }

        fn update_position(
            &mut self,
            flatland: &ui_comp::FlatlandProxy,
            layout: &OverlayLayout,
            hit_targets: &SharedOverlayHitTargets,
        ) -> Result<PresentNeeded, Error> {
            self.state.borrow_mut().update_position_calls += 1;
            flatland
                .set_translation(&self.transform_id, &fmath::Vec_ { x: layout.x, y: layout.y })?;
            *hit_targets.lock() = Some(layout.targets);
            Ok(PresentNeeded::Yes)
        }

        fn release(self: Box<Self>, flatland: &ui_comp::FlatlandProxy) -> Result<(), Error> {
            self.state.borrow_mut().release_calls += 1;
            flatland.release_image2(&self.image_id)?;
            Ok(())
        }
    }

    async fn setup_test_controller() -> (
        OverlayController,
        ui_comp::FlatlandProxy,
        ui_comp::FlatlandRequestStream,
        IdGenerator,
        UnboundedReceiver<OverlayEvent>,
        Rc<RefCell<FakeRendererState>>,
    ) {
        let (flatland, mut stream) = create_flatland2_proxy_and_stream().await;
        let mut id_gen = IdGenerator::new();
        let (tx, rx) = unbounded();
        let state = Rc::new(RefCell::new(FakeRendererState::default()));
        let controller = OverlayController::new_with_factory(
            &flatland,
            &mut id_gen,
            fmath::SizeU { width: 480, height: 800 },
            true,
            false,
            tx,
            Box::new(FakeRendererFactory { state: state.clone() }),
        )
        .expect("create controller");
        for _ in 0..6 {
            let _ = stream.try_next().await.expect("init request");
        }
        (controller, flatland, stream, id_gen, rx, state)
    }

    async fn press_chords(
        controller: &mut OverlayController,
        flatland: &ui_comp::FlatlandProxy,
        id_gen: &mut IdGenerator,
        has_present_credit: bool,
        count: usize,
    ) -> PresentNeeded {
        let mut last = PresentNeeded::No;
        for _ in 0..count {
            last = controller
                .handle_event(
                    flatland,
                    id_gen,
                    has_present_credit,
                    OverlayEvent::MediaButtons(press_event()),
                )
                .await
                .unwrap();
            let rel = controller
                .handle_event(
                    flatland,
                    id_gen,
                    has_present_credit,
                    OverlayEvent::MediaButtons(release_event()),
                )
                .await
                .unwrap();
            if rel.is_yes() {
                last = rel;
            }
        }
        last
    }

    async fn press_power(
        controller: &mut OverlayController,
        flatland: &ui_comp::FlatlandProxy,
        id_gen: &mut IdGenerator,
        has_present_credit: bool,
    ) -> PresentNeeded {
        let power_press = MediaButtonsEvent { power: Some(true), ..Default::default() };
        let power_release = MediaButtonsEvent { power: Some(false), ..Default::default() };
        let press_needed = controller
            .handle_event(
                flatland,
                id_gen,
                has_present_credit,
                OverlayEvent::MediaButtons(power_press),
            )
            .await
            .unwrap();
        let rel_needed = controller
            .handle_event(
                flatland,
                id_gen,
                has_present_credit,
                OverlayEvent::MediaButtons(power_release),
            )
            .await
            .unwrap();
        if press_needed.is_yes() || rel_needed.is_yes() {
            PresentNeeded::Yes
        } else {
            PresentNeeded::No
        }
    }

    async fn assert_flatland2_image_update(
        stream: &mut ui_comp::FlatlandRequestStream,
        expected_layer_id: u64,
    ) {
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { layer_id, properties, .. }) =
            stream.try_next().await.expect("SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(layer_id.value, expected_layer_id);
        assert_eq!(properties.composition_mode, Some(ui_comp::CompositionMode::Image));
        let Some(ui_comp::FlatlandRequest::SetTranslation { .. }) =
            stream.try_next().await.expect("SetTranslation")
        else {
            panic!("Expected SetTranslation");
        };
        let Some(ui_comp::FlatlandRequest::SetHitRegions { regions, .. }) =
            stream.try_next().await.expect("SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(regions.len(), 3);
        let Some(ui_comp::FlatlandRequest::SetLayerImage { layer_id, .. }) =
            stream.try_next().await.expect("SetLayerImage")
        else {
            panic!("Expected SetLayerImage");
        };
        assert_eq!(layer_id.value, expected_layer_id);
        assert!(
            stream.try_next().now_or_never().is_none(),
            "unexpected extra Flatland command (such as AddChild) after SetLayerImage"
        );
    }

    async fn assert_flatland2_image_attach(
        stream: &mut ui_comp::FlatlandRequestStream,
        expected_layer_id: u64,
    ) {
        let Some(ui_comp::FlatlandRequest::CreateImage2 { .. }) =
            stream.try_next().await.expect("CreateImage2")
        else {
            panic!("Expected CreateImage2");
        };
        assert_flatland2_image_update(stream, expected_layer_id).await;
    }

    async fn assert_flatland2_layer_teardown(
        stream: &mut ui_comp::FlatlandRequestStream,
        expected_layer_id: u64,
    ) {
        let Some(ui_comp::FlatlandRequest::SetHitRegions { regions, .. }) =
            stream.try_next().await.expect("SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert!(regions.is_empty());
        let Some(ui_comp::FlatlandRequest::ResetLayer { layer_id, .. }) =
            stream.try_next().await.expect("ResetLayer")
        else {
            panic!("Expected ResetLayer");
        };
        assert_eq!(layer_id.value, expected_layer_id);
        let Some(ui_comp::FlatlandRequest::ReleaseImage2 { .. }) =
            stream.try_next().await.expect("ReleaseImage2")
        else {
            panic!("Expected ReleaseImage2");
        };
    }

    #[fuchsia::test]
    fn test_is_overlay_enabled() {
        assert!(!is_overlay_enabled(false, 0));
        assert!(is_overlay_enabled(true, 0));
        assert!(is_overlay_enabled(false, 1));
        assert!(is_overlay_enabled(true, 2));
    }

    #[fuchsia::test]
    async fn test_create_and_release_flatland_images() {
        let (flatland, mut stream) = create_proxy_and_stream::<ui_comp::FlatlandMarker>();
        let mut id_gen = IdGenerator::new();
        let (token_a, _token_b) = zx::EventPair::create();
        let import_token = ui_comp::BufferCollectionImportToken { value: token_a };

        let image_ids = create_flatland_images::<IMAGE_COUNT>(
            &flatland,
            &mut id_gen,
            &import_token,
            OVERLAY_BUFFER_WIDTH,
            OVERLAY_BUFFER_HEIGHT,
        )
        .expect("create_flatland_images");

        for (expected_vmo_idx, expected_id) in image_ids.iter().enumerate() {
            let Some(ui_comp::FlatlandRequest::CreateImage2 {
                image_id,
                vmo_index,
                properties,
                ..
            }) = stream.try_next().await.expect("CreateImage2")
            else {
                panic!("Expected CreateImage2");
            };
            assert_eq!(image_id, *expected_id);
            assert_eq!(vmo_index, expected_vmo_idx as u32);
            assert_eq!(
                properties.size,
                Some(fmath::SizeU { width: OVERLAY_BUFFER_WIDTH, height: OVERLAY_BUFFER_HEIGHT })
            );
        }

        release_flatland_images(&flatland, &image_ids).expect("release_flatland_images");
        for expected_id in &image_ids {
            let Some(ui_comp::FlatlandRequest::ReleaseImage2 { image_id, .. }) =
                stream.try_next().await.expect("ReleaseImage2")
            else {
                panic!("Expected ReleaseImage2");
            };
            assert_eq!(image_id, *expected_id);
        }

        // Token without `DUPLICATE` right fails cleanly.
        let (no_dup_token, _peer) = zx::EventPair::create();
        let no_dup_token = no_dup_token.replace_handle(zx::Rights::WAIT).unwrap();
        let bad_import = ui_comp::BufferCollectionImportToken { value: no_dup_token };
        assert!(
            create_flatland_images::<IMAGE_COUNT>(
                &flatland,
                &mut id_gen,
                &bad_import,
                OVERLAY_BUFFER_WIDTH,
                OVERLAY_BUFFER_HEIGHT,
            )
            .is_err()
        );
    }

    #[fuchsia::test]
    async fn test_layer_state_flatland2_properties_and_placement_state_machine() {
        let (flatland, mut stream) = create_proxy_and_stream::<ui_comp::FlatlandMarker>();
        let transform_id = ui_comp::TransformId { value: 2 };
        let layer_id = ui_comp::LayerId { value: 3 };
        let hit_targets = new_overlay_hit_targets();
        let mut layer_state = LayerState::default();

        // Initial `apply()` sets `composition_mode`, `display_rect`, `opacity`,
        // `BlendMode2::Replace`, `SetTranslation()`, `SetHitRegions()`, and populates
        // `hit_targets`.
        let layout_480 =
            compute_overlay_layout(fmath::SizeU { width: 480, height: 800 }, None, false);
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_480, &hit_targets, Some(100))
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetLayerProperties {
            layer_id: req_layer,
            properties,
            ..
        }) = stream.try_next().await.expect("initial SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(req_layer, layer_id);
        assert_eq!(properties.composition_mode, Some(ui_comp::CompositionMode::Image));
        assert_eq!(
            properties.display_rect,
            Some(fmath::RectU { x: 0, y: 0, width: 420, height: 241 })
        );
        assert_eq!(properties.opacity, Some(1.0));
        assert_eq!(properties.blend_mode, Some(ui_comp::BlendMode2::Replace));

        let Some(ui_comp::FlatlandRequest::SetTranslation {
            transform_id: req_transform,
            translation,
            ..
        }) = stream.try_next().await.expect("initial SetTranslation")
        else {
            panic!("Expected SetTranslation");
        };
        assert_eq!(req_transform, transform_id);
        assert_eq!(translation, fmath::Vec_ { x: 30, y: 66 });

        let Some(ui_comp::FlatlandRequest::SetHitRegions {
            transform_id: hit_transform,
            regions,
            ..
        }) = stream.try_next().await.expect("initial SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(hit_transform, transform_id);
        assert_eq!(regions.len(), 3);
        assert_eq!(*hit_targets.lock(), Some(layout_480.targets));

        // Unchanged inputs deduplicate to `PresentNeeded::No` with zero FIDL commands emitted.
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_480, &hit_targets, Some(100))
                .unwrap(),
            PresentNeeded::No,
        );
        assert!(stream.next().now_or_never().is_none());

        // Dropping opacity below 100% transitions `blend_mode` to `StraightAlpha` without
        // re-sending `composition_mode`, `display_rect`, `SetTranslation()`, or
        // `SetHitRegions()`.
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_480, &hit_targets, Some(60))
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { properties, .. }) =
            stream.try_next().await.expect("translucent SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(properties.composition_mode, None);
        assert_eq!(properties.display_rect, None);
        assert_eq!(properties.opacity, Some(0.60));
        assert_eq!(properties.blend_mode, Some(ui_comp::BlendMode2::StraightAlpha));
        assert!(stream.next().now_or_never().is_none());

        // Subsequent translucent opacity change omits redundant `blend_mode`.
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_480, &hit_targets, Some(40))
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { properties, .. }) =
            stream.try_next().await.expect("second translucent SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(properties.opacity, Some(0.40));
        assert_eq!(properties.blend_mode, None);
        assert!(stream.next().now_or_never().is_none());

        // Returning to 100% opacity transitions `blend_mode` back to `Replace`.
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_480, &hit_targets, Some(100))
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { properties, .. }) =
            stream.try_next().await.expect("opaque SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(properties.opacity, Some(1.0));
        assert_eq!(properties.blend_mode, Some(ui_comp::BlendMode2::Replace));
        assert!(stream.next().now_or_never().is_none());

        // Display resize updates `display_rect`, `SetTranslation()`, and `SetHitRegions()`.
        let layout_800 =
            compute_overlay_layout(fmath::SizeU { width: 800, height: 480 }, None, false);
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_800, &hit_targets, None)
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { properties, .. }) =
            stream.try_next().await.expect("resize SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(properties.composition_mode, None);
        assert_eq!(
            properties.display_rect,
            Some(fmath::RectU { x: 0, y: 0, width: 700, height: 402 })
        );
        assert_eq!(properties.opacity, None);
        assert_eq!(properties.blend_mode, None);

        let Some(ui_comp::FlatlandRequest::SetTranslation { translation, .. }) =
            stream.try_next().await.expect("resize SetTranslation")
        else {
            panic!("Expected SetTranslation");
        };
        assert_eq!(translation, fmath::Vec_ { x: 50, y: 39 });

        let Some(ui_comp::FlatlandRequest::SetHitRegions { regions, .. }) =
            stream.try_next().await.expect("resize SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(regions.len(), 3);
        assert!(stream.next().now_or_never().is_none());

        // Changing `custom_origin` without resizing emits `SetTranslation()` only (local hit
        // regions are transform-relative, while view-space `hit_targets` updates).
        let layout_dragged =
            compute_overlay_layout(fmath::SizeU { width: 800, height: 480 }, Some((12, 34)), false);
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_dragged, &hit_targets, None)
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetTranslation { translation, .. }) =
            stream.try_next().await.expect("custom_origin SetTranslation")
        else {
            panic!("Expected SetTranslation");
        };
        assert_eq!(translation, fmath::Vec_ { x: 12, y: 34 });
        assert!(stream.next().now_or_never().is_none());
        assert_eq!(*hit_targets.lock(), Some(layout_dragged.targets));

        // Enabling `confirm_close` expands `SetHitRegions()` from 3 to 5 regions.
        let layout_confirm =
            compute_overlay_layout(fmath::SizeU { width: 800, height: 480 }, Some((12, 34)), true);
        assert_eq!(
            layer_state
                .apply(&flatland, &transform_id, &layer_id, &layout_confirm, &hit_targets, None)
                .unwrap(),
            PresentNeeded::Yes,
        );
        let Some(ui_comp::FlatlandRequest::SetHitRegions { regions, .. }) =
            stream.try_next().await.expect("confirm_close SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(regions.len(), 5);
        assert!(stream.next().now_or_never().is_none());

        // Channel write error preserves previously committed `LayerState` and `hit_targets`.
        let (bad_client, _bad_server) = create_endpoints::<ui_comp::FlatlandMarker>();
        let no_write_channel = bad_client.into_channel().replace_handle(zx::Rights::WAIT).unwrap();
        let failing_flatland =
            ui_comp::FlatlandProxy::new(fasync::Channel::from_channel(no_write_channel));
        let layout_1024 =
            compute_overlay_layout(fmath::SizeU { width: 1024, height: 768 }, Some((0, 0)), false);
        assert!(
            layer_state
                .apply(
                    &failing_flatland,
                    &transform_id,
                    &layer_id,
                    &layout_1024,
                    &hit_targets,
                    Some(50),
                )
                .is_err()
        );
        assert_eq!(layer_state.last_dest_size, Some((700, 402)));
        assert_eq!(layer_state.last_translation, Some((12, 34)));
        assert_eq!(layer_state.last_hit_regions_key, Some((700, 402, true)));
        assert_eq!(layer_state.last_applied_opacity, Some(100));
        assert_eq!(layer_state.last_blend_is_opaque, Some(true));
        assert_eq!(*hit_targets.lock(), Some(layout_confirm.targets));

        // Second `apply()` with unchanged `display_size` (`800x480`) and `opacity_pct` (`None`)
        // skips `set_layer_properties()` and fails directly on `set_translation()`, leaving
        // `last_translation` at `Some((12, 34))` and `hit_targets` unchanged.
        let layout_800_new_pos =
            compute_overlay_layout(fmath::SizeU { width: 800, height: 480 }, Some((0, 0)), true);
        assert!(
            layer_state
                .apply(
                    &failing_flatland,
                    &transform_id,
                    &layer_id,
                    &layout_800_new_pos,
                    &hit_targets,
                    None,
                )
                .is_err()
        );
        assert_eq!(layer_state.last_translation, Some((12, 34)));
        assert_eq!(*hit_targets.lock(), Some(layout_confirm.targets));
    }

    #[fuchsia::test]
    async fn test_overlay_controller_public_new_debug_and_disabled() {
        // 1. Disabled controller skips all Flatland2 setup calls and ignores events/resizes/flushes.
        let (flatland_disabled, mut disabled_stream) = create_flatland2_proxy_and_stream().await;
        let mut id_gen_disabled = IdGenerator::new();
        let (tx_disabled, _rx_disabled) = unbounded();
        let mut disabled = OverlayController::new(
            &flatland_disabled,
            &mut id_gen_disabled,
            fmath::SizeU { width: 480, height: 800 },
            false,
            None,
            tx_disabled,
        )
        .expect("create disabled controller");
        assert!(format!("{disabled:?}").contains("OverlayController"));
        assert!(
            disabled_stream.try_next().now_or_never().is_none(),
            "disabled controller must not enqueue Flatland setup commands"
        );
        assert_eq!(
            disabled
                .set_display_size(
                    &flatland_disabled,
                    &mut id_gen_disabled,
                    true,
                    fmath::SizeU { width: 800, height: 1280 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(
            press_chords(&mut disabled, &flatland_disabled, &mut id_gen_disabled, true, 3).await,
            PresentNeeded::No
        );
        assert_eq!(disabled.mode(), OverlayDisplayMode::Hidden);
        assert_eq!(
            disabled.flush_pending(&flatland_disabled, &mut id_gen_disabled).await.unwrap(),
            PresentNeeded::No
        );
        assert!(disabled_stream.try_next().now_or_never().is_none());

        // 2. Enabled public `OverlayController::new()` emits the 6 Flatland2 setup commands.
        let (flatland, mut stream) = create_flatland2_proxy_and_stream().await;
        let mut id_gen = IdGenerator::new();
        let (tx, _rx) = unbounded();
        let enabled = OverlayController::new(
            &flatland,
            &mut id_gen,
            fmath::SizeU { width: 480, height: 800 },
            true,
            None,
            tx,
        )
        .expect("create enabled controller");
        assert!(format!("{enabled:?}").contains("OverlayController"));
        for _ in 0..6 {
            assert!(stream.try_next().await.expect("init request").is_some());
        }

        // 3. Unwritable Flatland channel causes `OverlayController::new()` to return `Err`.
        let (client_end, _server_end) = zx::Channel::create();
        let read_only_channel =
            client_end.replace_handle(zx::Rights::READ | zx::Rights::WAIT).unwrap();
        let read_only_flatland =
            ui_comp::FlatlandProxy::new(fidl::AsyncChannel::from_channel(read_only_channel));
        assert!(
            OverlayController::new(
                &read_only_flatland,
                &mut id_gen,
                fmath::SizeU { width: 480, height: 800 },
                true,
                None,
                unbounded().0,
            )
            .is_err()
        );
    }

    #[fuchsia::test]
    async fn test_overlay_controller_creates_flatland2_layer_and_defers_without_credit() {
        let (flatland, mut stream) = create_flatland2_proxy_and_stream().await;
        let mut id_gen = IdGenerator::new();
        let (tx, _rx) = unbounded();
        let state = Rc::new(RefCell::new(FakeRendererState::default()));
        let mut controller = OverlayController::new_with_factory(
            &flatland,
            &mut id_gen,
            fmath::SizeU { width: 480, height: 800 },
            true,
            false,
            tx,
            Box::new(FakeRendererFactory { state: state.clone() }),
        )
        .expect("create controller");
        assert_eq!(controller.overlay_transform_id().value, 1);
        assert_eq!(controller.overlay_layer_id.value, 2);
        assert_eq!(controller.mode(), OverlayDisplayMode::Hidden);
        assert!(controller.hit_targets.lock().is_none());

        // Verify Flatland2 initialization sequence on `stream`.
        let Some(ui_comp::FlatlandRequest::CreateTransform { transform_id, .. }) =
            stream.try_next().await.expect("CreateTransform")
        else {
            panic!("Expected CreateTransform");
        };
        assert_eq!(transform_id.value, 1);
        let Some(ui_comp::FlatlandRequest::SetHitRegions { transform_id, regions, .. }) =
            stream.try_next().await.expect("SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(transform_id.value, 1);
        assert!(regions.is_empty());
        let Some(ui_comp::FlatlandRequest::CreateLayer { layer_id, .. }) =
            stream.try_next().await.expect("CreateLayer")
        else {
            panic!("Expected CreateLayer");
        };
        assert_eq!(layer_id.value, 2);
        let Some(ui_comp::FlatlandRequest::CreateLayerStack { stack_id, .. }) =
            stream.try_next().await.expect("CreateLayerStack")
        else {
            panic!("Expected CreateLayerStack");
        };
        assert_eq!(stack_id.value, 3);
        let Some(ui_comp::FlatlandRequest::SetStackLayers { stack_id, layers, .. }) =
            stream.try_next().await.expect("SetStackLayers")
        else {
            panic!("Expected SetStackLayers");
        };
        assert_eq!(stack_id.value, 3);
        assert_eq!(layers, vec![ui_comp::LayerId { value: 2 }]);
        let Some(ui_comp::FlatlandRequest::SetTransformContent { transform_id, content, .. }) =
            stream.try_next().await.expect("SetTransformContent")
        else {
            panic!("Expected SetTransformContent");
        };
        assert_eq!(transform_id.value, 1);
        assert_eq!(
            content,
            Some(Box::new(ui_comp::TransformContent::LayerStack(ui_comp::LayerStackId {
                value: 3
            })))
        );

        // 1. Uncredited show + Power dismiss BEFORE attach clears `render_dirty` immediately so a
        // subsequent `flush_pending()` is a no-op.
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, false, 3).await,
            PresentNeeded::No
        );
        assert!(controller.mode().is_visible());
        assert_eq!(state.borrow().create_calls, 0);
        assert_eq!(
            press_power(&mut controller, &flatland, &mut id_gen, false).await,
            PresentNeeded::No
        );
        assert_eq!(controller.mode(), OverlayDisplayMode::Hidden);
        assert_eq!(
            controller.flush_pending(&flatland, &mut id_gen).await.unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().create_calls, 0);
        assert!(stream.try_next().now_or_never().is_none());

        // 2. Three presses without present credit defer rendering (`PresentNeeded::No`), and
        // `MoveRequested` while deferred clamps coordinates, updates `hit_targets`, and marks
        // position dirty without enqueuing FIDL commands until `flush_pending()`.
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, false, 3).await,
            PresentNeeded::No
        );
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    false,
                    OverlayEvent::MoveRequested { x: -50, y: 9_999 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        let expected_clamped =
            compute_overlay_placement(fmath::SizeU { width: 480, height: 800 }, Some((-50, 9_999)));
        assert_eq!(controller.custom_origin, Some((expected_clamped.0, expected_clamped.1)));
        assert!(controller.hit_targets.lock().is_some());
        assert!(stream.try_next().now_or_never().is_none());

        assert_eq!(
            controller.flush_pending(&flatland, &mut id_gen).await.unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().create_calls, 1);
        assert_eq!(state.borrow().last_transform_id, Some(controller.overlay_transform_id()));
        assert_eq!(state.borrow().render_calls, 1);
        assert_flatland2_image_attach(&mut stream, 2).await;

        assert_eq!(
            controller.flush_pending(&flatland, &mut id_gen).await.unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().render_calls, 1);

        // 3. `set_display_size()` with `has_present_credit = false` while visible defers until
        // `flush_pending()`.
        assert_eq!(
            controller
                .set_display_size(
                    &flatland,
                    &mut id_gen,
                    false,
                    fmath::SizeU { width: 720, height: 1280 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().render_calls, 1);
        assert_eq!(
            controller.flush_pending(&flatland, &mut id_gen).await.unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().render_calls, 2);
        assert_flatland2_image_update(&mut stream, 2).await;

        // 4. Power dismiss with `has_present_credit = false` after attach defers `ResetLayer()` and
        // `ReleaseImage2()` until `flush_pending()`.
        assert_eq!(
            press_power(&mut controller, &flatland, &mut id_gen, false).await,
            PresentNeeded::No
        );
        assert_eq!(state.borrow().release_calls, 0);
        assert_eq!(
            controller.flush_pending(&flatland, &mut id_gen).await.unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().release_calls, 1);
        assert_flatland2_layer_teardown(&mut stream, 2).await;
    }

    #[fuchsia::test]
    async fn test_overlay_controller_touch_controls_drag_clamping_and_close_flow() {
        let (mut controller, flatland, _stream, mut id_gen, _rx, state) =
            setup_test_controller().await;

        // Duplicate `CloseRequested` while hidden is a no-op without arming the press timer.
        assert_eq!(
            controller
                .handle_event(&flatland, &mut id_gen, true, OverlayEvent::CloseRequested)
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert!(controller.timer_task.is_none());

        // Show overlay with present credit.
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 3).await,
            PresentNeeded::Yes
        );
        assert!(controller.mode().is_visible());
        assert_eq!(state.borrow().render_calls, 1);
        assert!(controller.hit_targets.lock().is_some());

        // Clamped `MoveRequested` calls `update_position` once and deduplicates subsequent
        // out-of-bounds drags that clamp to the same origin.
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    true,
                    OverlayEvent::MoveRequested { x: 9_999, y: -100 },
                )
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().update_position_calls, 1);
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    true,
                    OverlayEvent::MoveRequested { x: 12_345, y: -200 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().update_position_calls, 1);

        // Opacity change triggers re-render.
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    true,
                    OverlayEvent::SetOpacity { opacity: 0.75 },
                )
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().render_calls, 2);

        // `CloseRequested` -> `CloseCancelled` -> `CloseConfirmed` transitions without
        // arming or rearming the shortcut timer.
        controller.timer_task = None;
        assert_eq!(
            controller
                .handle_event(&flatland, &mut id_gen, true, OverlayEvent::CloseRequested)
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert!(controller.shortcut_tracker.confirm_close());
        assert!(controller.timer_task.is_none());

        assert_eq!(
            controller
                .handle_event(&flatland, &mut id_gen, true, OverlayEvent::CloseCancelled)
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert!(!controller.shortcut_tracker.confirm_close());
        assert!(controller.timer_task.is_none());

        assert_eq!(
            controller
                .handle_event(&flatland, &mut id_gen, true, OverlayEvent::CloseConfirmed)
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(controller.mode(), OverlayDisplayMode::Hidden);
        assert!(controller.hit_targets.lock().is_none());
        assert!(controller.timer_task.is_none());
    }

    #[fuchsia::test]
    async fn test_overlay_controller_async_release_fence_hide_and_failure_latch() {
        let (mut controller, flatland, mut stream, mut id_gen, mut rx, state) =
            setup_test_controller().await;

        // Start with a failing factory to verify failure latches while visible and resets on hide.
        state.borrow_mut().fail_create = true;
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 3).await,
            PresentNeeded::No
        );
        assert_eq!(state.borrow().create_calls, 1);

        // Touch/press inputs while visible do not retry the failed factory.
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    true,
                    OverlayEvent::MoveRequested { x: 10, y: 20 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 1).await,
            PresentNeeded::No
        );
        assert_eq!(state.borrow().create_calls, 1);

        // Hiding resets `renderer_init_failed`.
        assert_eq!(
            press_power(&mut controller, &flatland, &mut id_gen, true).await,
            PresentNeeded::No
        );

        // Allow factory creation, but gate the first render behind an unsignaled release fence.
        let release_fence = zx::Event::create();
        let fence_to_signal = release_fence.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        {
            let mut st = state.borrow_mut();
            st.fail_create = false;
            st.deferred_fence = Some(release_fence);
        }

        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 3).await,
            PresentNeeded::No
        );
        assert_eq!(state.borrow().create_calls, 2);
        assert_eq!(state.borrow().render_calls, 1);

        // A second event while the same `release_fence` is unsignaled deduplicates the waiter so
        // signaling once sends only one `ReleaseFenceSignaled`.
        assert_eq!(
            controller
                .handle_event(
                    &flatland,
                    &mut id_gen,
                    true,
                    OverlayEvent::MediaButtons(release_event()),
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );

        fence_to_signal.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        let wake_event = rx.next().await.expect("wake event");
        assert_eq!(wake_event, OverlayEvent::ReleaseFenceSignaled);
        assert!(
            rx.try_recv().is_err(),
            "duplicate ReleaseFenceSignaled should not be queued for the same fence"
        );

        assert_eq!(
            controller.handle_event(&flatland, &mut id_gen, true, wake_event).await.unwrap(),
            PresentNeeded::Yes,
            "render should proceed and enqueue Flatland2 commands once release fence signals"
        );
        assert_eq!(state.borrow().render_calls, 2);
        assert_flatland2_image_attach(&mut stream, 2).await;

        // Hiding after attach clears `SetHitRegions()`, sends `ResetLayer()`, then
        // `ReleaseImage2()`.
        assert_eq!(
            press_power(&mut controller, &flatland, &mut id_gen, true).await,
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().release_calls, 1);
        assert_flatland2_layer_teardown(&mut stream, 2).await;

        // A stale `ReleaseFenceSignaled` arriving while `Hidden` is a no-op and does not recreate
        // the renderer.
        assert_eq!(
            controller
                .handle_event(&flatland, &mut id_gen, true, OverlayEvent::ReleaseFenceSignaled)
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert!(controller.renderer.is_none());
        assert_eq!(state.borrow().create_calls, 2);
    }

    #[fuchsia::test]
    fn test_overlay_controller_fake_time_timeout_and_power_dismiss() {
        fn run_ready<F: std::future::Future>(exec: &mut fasync::TestExecutor, fut: F) -> F::Output {
            let mut fut = std::pin::pin!(fut);
            let val = match exec.run_until_stalled(&mut fut) {
                Poll::Ready(val) => val,
                Poll::Pending => panic!("future unexpectedly pending under fake time"),
            };
            // Poll newly spawned background tasks (such as `timer_task`) so their timers
            // register with the executor before the caller advances fake time.
            let _ = exec.run_until_stalled(&mut std::future::pending::<()>());
            val
        }

        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let (flatland, _stream) = run_ready(&mut exec, create_flatland2_proxy_and_stream());
        let mut id_gen = IdGenerator::new();
        let (tx, mut rx) = unbounded();
        let state = Rc::new(RefCell::new(FakeRendererState::default()));
        let mut controller = OverlayController::new_with_factory(
            &flatland,
            &mut id_gen,
            fmath::SizeU { width: 480, height: 800 },
            true,
            false,
            tx,
            Box::new(FakeRendererFactory { state: state.clone() }),
        )
        .expect("create controller");

        // 1. Two presses (mode stays `Hidden`) arm the press-timeout timer; advancing past
        // `MAX_PRESS_INTERVAL_NS` fires `PressTimeoutTick`, returns `PresentNeeded::No`, clears
        // `timer_task`, and resets count so 2 subsequent presses still stay `Hidden`.
        assert_eq!(
            run_ready(&mut exec, press_chords(&mut controller, &flatland, &mut id_gen, true, 2)),
            PresentNeeded::No
        );
        assert!(controller.timer_task.is_some());
        exec.set_fake_time(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
            MAX_PRESS_INTERVAL_NS + 1,
        )));
        assert!(exec.wake_expired_timers());
        let Poll::Ready(Some(early_tick)) = exec.run_until_stalled(&mut rx.next()) else {
            panic!("Expected PressTimeoutTick after press 2 timeout");
        };
        assert_eq!(
            run_ready(
                &mut exec,
                controller.handle_event(&flatland, &mut id_gen, true, early_tick),
            )
            .unwrap(),
            PresentNeeded::No
        );
        assert!(controller.timer_task.is_none());
        assert_eq!(
            run_ready(&mut exec, press_chords(&mut controller, &flatland, &mut id_gen, true, 2)),
            PresentNeeded::No
        );
        assert_eq!(controller.mode(), OverlayDisplayMode::Hidden);
        exec.set_fake_time(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
            MAX_PRESS_INTERVAL_NS + 1,
        )));
        assert!(exec.wake_expired_timers());
        let Poll::Ready(Some(reset_tick)) = exec.run_until_stalled(&mut rx.next()) else {
            panic!("Expected PressTimeoutTick after second 2-press sequence");
        };
        assert_eq!(
            run_ready(
                &mut exec,
                controller.handle_event(&flatland, &mut id_gen, true, reset_tick),
            )
            .unwrap(),
            PresentNeeded::No
        );

        // 2. Three presses with 120ms async renderer init latency still fire `PressTimeoutTick`
        // at `MAX_PRESS_INTERVAL_NS` from press 3's input timestamp rather than drifting by 120ms.
        state.borrow_mut().create_delay = Some(zx::BootDuration::from_millis(120));
        let show_present = {
            let show_fut = press_chords(&mut controller, &flatland, &mut id_gen, true, 3);
            let mut show_fut = std::pin::pin!(show_fut);
            assert!(exec.run_until_stalled(&mut show_fut).is_pending());
            exec.set_fake_time(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(120),
            ));
            assert!(exec.wake_expired_timers());
            let Poll::Ready(show_present) = exec.run_until_stalled(&mut show_fut) else {
                panic!("Expected show future to finish after 120ms init delay");
            };
            let _ = exec.run_until_stalled(&mut std::future::pending::<()>());
            show_present
        };
        assert_eq!(show_present, PresentNeeded::Yes);
        assert_eq!(controller.mode(), OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 });
        assert!(controller.timer_task.is_some());

        // Advance only the remaining `500ms - 120ms + 1ns` (`380_000_001ns`): timer must expire now!
        exec.set_fake_time(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
            MAX_PRESS_INTERVAL_NS - 120_000_000 + 1,
        )));
        assert!(exec.wake_expired_timers());
        let Poll::Ready(Some(tick_event)) = exec.run_until_stalled(&mut rx.next()) else {
            panic!("Expected PressTimeoutTick at exact 500ms deadline despite 120ms init latency");
        };
        assert_eq!(tick_event, OverlayEvent::PressTimeoutTick);

        let needs_present =
            run_ready(&mut exec, controller.handle_event(&flatland, &mut id_gen, true, tick_event))
                .unwrap();
        assert_eq!(needs_present, PresentNeeded::Yes);
        assert_eq!(controller.mode(), OverlayDisplayMode::DiagnosticsIdle);
        assert!(controller.timer_task.is_none());

        // 3. A later Power press transitions to `Hidden` and keeps the timer cleared.
        let hide_present =
            run_ready(&mut exec, press_power(&mut controller, &flatland, &mut id_gen, true));
        assert_eq!(hide_present, PresentNeeded::Yes);
        assert_eq!(controller.mode(), OverlayDisplayMode::Hidden);
        assert!(controller.timer_task.is_none());
    }

    #[fuchsia::test]
    async fn test_overlay_controller_set_display_size_and_render_failure_recovery() {
        let (mut controller, flatland, mut stream, mut id_gen, _rx, state) =
            setup_test_controller().await;

        // Changing display size while hidden does not render.
        assert_eq!(
            controller
                .set_display_size(
                    &flatland,
                    &mut id_gen,
                    true,
                    fmath::SizeU { width: 600, height: 900 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().render_calls, 0);

        // Show overlay -> renders with `600x900`.
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 3).await,
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().render_calls, 1);
        assert_eq!(
            state.borrow().last_display_size,
            Some(fmath::SizeU { width: 600, height: 900 })
        );
        assert_flatland2_image_attach(&mut stream, 2).await;

        // Setting the same size while visible is a no-op.
        assert_eq!(
            controller
                .set_display_size(
                    &flatland,
                    &mut id_gen,
                    true,
                    fmath::SizeU { width: 600, height: 900 },
                )
                .await
                .unwrap(),
            PresentNeeded::No
        );
        assert_eq!(state.borrow().render_calls, 1);

        // Changing to a new size while visible triggers a re-render.
        assert_eq!(
            controller
                .set_display_size(
                    &flatland,
                    &mut id_gen,
                    true,
                    fmath::SizeU { width: 800, height: 480 },
                )
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().render_calls, 2);
        assert_eq!(
            state.borrow().last_display_size,
            Some(fmath::SizeU { width: 800, height: 480 })
        );
        assert_flatland2_image_update(&mut stream, 2).await;

        // Simulate a render error: controller drops the broken renderer, latches
        // `renderer_init_failed`, emits `ResetLayer()` + `ReleaseImage2()`, and returns
        // `PresentNeeded::Yes`.
        state.borrow_mut().fail_render = true;
        assert_eq!(
            controller
                .set_display_size(
                    &flatland,
                    &mut id_gen,
                    true,
                    fmath::SizeU { width: 720, height: 1280 },
                )
                .await
                .unwrap(),
            PresentNeeded::Yes
        );
        assert!(controller.renderer.is_none());
        assert!(controller.renderer_init_failed);
        assert_eq!(state.borrow().release_calls, 1);
        assert_flatland2_layer_teardown(&mut stream, 2).await;

        // Hide and re-show creates a new renderer and attaches cleanly without `AddChild()`.
        state.borrow_mut().fail_render = false;
        assert_eq!(
            press_power(&mut controller, &flatland, &mut id_gen, true).await,
            PresentNeeded::No
        );
        assert_eq!(
            press_chords(&mut controller, &flatland, &mut id_gen, true, 3).await,
            PresentNeeded::Yes
        );
        assert_eq!(state.borrow().create_calls, 2);
        assert_eq!(state.borrow().last_transform_id, Some(controller.overlay_transform_id()));
        assert!(controller.renderer.is_some());
        assert_flatland2_image_attach(&mut stream, 2).await;
    }

    struct FakeSwapchainEntry {
        outcome: AcquireOutcome,
        acquire_fence: Option<zx::Event>,
        release_fence: Option<zx::Event>,
        fail_render: bool,
    }

    struct FakeOverlaySwapchain {
        entries: VecDeque<FakeSwapchainEntry>,
        render_calls: usize,
    }

    impl OverlaySwapchain for FakeOverlaySwapchain {
        type Renderer = ();

        fn acquire_next(&mut self) -> Result<AcquireOutcome, Error> {
            let entry = self.entries.front().ok_or_else(|| anyhow!("no fake swapchain entry"))?;
            match &entry.outcome {
                AcquireOutcome::Ready(slot) => {
                    Ok(AcquireOutcome::Ready(AcquiredSlot::new_for_testing(slot.index())))
                }
                AcquireOutcome::Deferred { release_fence } => {
                    let dup = release_fence.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
                    self.entries.pop_front();
                    Ok(AcquireOutcome::Deferred { release_fence: dup })
                }
            }
        }

        fn render_frame(
            &mut self,
            _renderer: &mut (),
            slot: AcquiredSlot,
            _params: &OverlayRenderParams<'_>,
        ) -> Result<RenderedSlot, Error> {
            self.render_calls += 1;
            let entry =
                self.entries.pop_front().ok_or_else(|| anyhow!("no fake swapchain entry"))?;
            if entry.fail_render {
                return Err(anyhow!("simulated render_frame failure"));
            }
            Ok(RenderedSlot {
                slot_index: slot.index(),
                acquire_fence: entry.acquire_fence.expect("acquire_fence"),
                release_fence: entry.release_fence.expect("release_fence"),
            })
        }
    }

    #[fuchsia::test]
    async fn test_flatland_overlay_renderer_render_and_attach_and_release() {
        let (flatland, mut stream) = create_proxy_and_stream::<ui_comp::FlatlandMarker>();
        let image_ids = [
            ui_comp::ImageId { value: 10 },
            ui_comp::ImageId { value: 11 },
            ui_comp::ImageId { value: 12 },
        ];
        let transform_id = ui_comp::TransformId { value: 20 };
        let layer_id = ui_comp::LayerId { value: 30 };

        let acq0 = zx::Event::create();
        let acq0_koid = acq0.koid().unwrap();
        let rel0 = zx::Event::create();
        let rel0_koid = rel0.koid().unwrap();

        let acq_op = zx::Event::create();
        let acq_op_koid = acq_op.koid().unwrap();
        let rel_op = zx::Event::create();
        let rel_op_koid = rel_op.koid().unwrap();

        let acq_fail_attach = zx::Event::create();
        let rel_fail_attach = zx::Event::create();

        let acq1 = zx::Event::create();
        let acq1_koid = acq1.koid().unwrap();
        let rel1 = zx::Event::create();
        let rel1_koid = rel1.koid().unwrap();

        let deferred_fence = zx::Event::create();
        let deferred_koid = deferred_fence.koid().unwrap();

        let mut entries = VecDeque::new();
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Ready(AcquiredSlot::new_for_testing(1)),
            acquire_fence: Some(acq0),
            release_fence: Some(rel0),
            fail_render: false,
        });
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Ready(AcquiredSlot::new_for_testing(0)),
            acquire_fence: Some(acq_op),
            release_fence: Some(rel_op),
            fail_render: false,
        });
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Deferred { release_fence: deferred_fence },
            acquire_fence: None,
            release_fence: None,
            fail_render: false,
        });
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Ready(AcquiredSlot::new_for_testing(2)),
            acquire_fence: None,
            release_fence: None,
            fail_render: true,
        });
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Ready(AcquiredSlot::new_for_testing(2)),
            acquire_fence: Some(acq_fail_attach),
            release_fence: Some(rel_fail_attach),
            fail_render: false,
        });
        entries.push_back(FakeSwapchainEntry {
            outcome: AcquireOutcome::Ready(AcquiredSlot::new_for_testing(2)),
            acquire_fence: Some(acq1),
            release_fence: Some(rel1),
            fail_render: false,
        });

        let mut presenter = FlatlandOverlayRenderer {
            swapchain: FakeOverlaySwapchain { entries, render_calls: 0 },
            renderer: (),
            image_ids,
            transform_id,
            layer_id,
            layer_state: LayerState::default(),
            last_rendered_frame: None,
        };

        let hud_lines = default_hud_lines();
        let hit_targets = new_overlay_hit_targets();
        let params = OverlayRenderParams {
            display_size: fmath::SizeU { width: 480, height: 800 },
            custom_origin: None,
            mode: OverlayDisplayMode::DiagnosticsIdle,
            confirm_close: false,
            opacity: 1.0,
            hud_generation: 1,
            hud_lines: &hud_lines,
        };

        // 1. Initial `OverlayRenderer::render_and_attach()` renders into slot 1, configures
        // `LayerState`, and calls `SetLayerImage()` with `image_ids[1]` and the expected
        // wait/signal fence koids.
        let RenderOutcome::Rendered { present_needed } =
            OverlayRenderer::render_and_attach(&mut presenter, &flatland, &hit_targets, &params)
                .unwrap()
        else {
            panic!("Expected Rendered on initial frame");
        };
        assert_eq!(present_needed, PresentNeeded::Yes);
        assert_eq!(presenter.swapchain.render_calls, 1);

        let Some(ui_comp::FlatlandRequest::SetLayerProperties { layer_id: req_layer, .. }) =
            stream.try_next().await.expect("SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties");
        };
        assert_eq!(req_layer, layer_id);
        let Some(ui_comp::FlatlandRequest::SetTranslation { transform_id: req_transform, .. }) =
            stream.try_next().await.expect("SetTranslation")
        else {
            panic!("Expected SetTranslation");
        };
        assert_eq!(req_transform, transform_id);
        let Some(ui_comp::FlatlandRequest::SetHitRegions { transform_id: hit_transform, .. }) =
            stream.try_next().await.expect("SetHitRegions")
        else {
            panic!("Expected SetHitRegions");
        };
        assert_eq!(hit_transform, transform_id);
        let Some(ui_comp::FlatlandRequest::SetLayerImage {
            layer_id: img_layer,
            image_id,
            acquire_fence,
            release_fence,
            ..
        }) = stream.try_next().await.expect("SetLayerImage")
        else {
            panic!("Expected SetLayerImage");
        };
        assert_eq!(img_layer, layer_id);
        assert_eq!(image_id, image_ids[1]);
        let Some(ui_comp::WaitFence::Basic(wait_handle)) = acquire_fence.as_deref() else {
            panic!("Expected WaitFence::Basic");
        };
        assert_eq!(wait_handle.koid().unwrap(), acq0_koid);
        let Some(ui_comp::SignalFence::Basic(sig_handle)) = release_fence.as_deref() else {
            panic!("Expected SignalFence::Basic");
        };
        assert_eq!(sig_handle.koid().unwrap(), rel0_koid);
        assert!(stream.next().now_or_never().is_none());

        // 2. Placement-only skip and opacity updates:
        //    a) Unchanged content and placement returns `PresentNeeded::No` with zero FIDL calls.
        let RenderOutcome::Rendered { present_needed } =
            OverlayRenderer::render_and_attach(&mut presenter, &flatland, &hit_targets, &params)
                .unwrap()
        else {
            panic!("Expected Rendered on unchanged frame");
        };
        assert_eq!(present_needed, PresentNeeded::No);
        assert_eq!(presenter.swapchain.render_calls, 1);
        assert!(stream.next().now_or_never().is_none());

        //    b) Resize or origin change with unchanged content skips swapchain rendering (no
        //       `SetLayerImage()`) and returns `PresentNeeded::Yes`.
        let resized_params = OverlayRenderParams {
            display_size: fmath::SizeU { width: 800, height: 480 },
            custom_origin: Some((15, 25)),
            ..params
        };
        let RenderOutcome::Rendered { present_needed } = OverlayRenderer::render_and_attach(
            &mut presenter,
            &flatland,
            &hit_targets,
            &resized_params,
        )
        .unwrap() else {
            panic!("Expected Rendered on placement-only update");
        };
        assert_eq!(present_needed, PresentNeeded::Yes);
        assert_eq!(presenter.swapchain.render_calls, 1);
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { .. }) =
            stream.try_next().await.expect("resize SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties on resize");
        };
        let Some(ui_comp::FlatlandRequest::SetTranslation { translation, .. }) =
            stream.try_next().await.expect("origin SetTranslation")
        else {
            panic!("Expected SetTranslation on origin change");
        };
        assert_eq!(translation, fmath::Vec_ { x: 15, y: 25 });
        let Some(ui_comp::FlatlandRequest::SetHitRegions { .. }) =
            stream.try_next().await.expect("resize SetHitRegions")
        else {
            panic!("Expected SetHitRegions on resize");
        };
        assert!(
            stream.next().now_or_never().is_none(),
            "placement-only update must not call SetLayerImage()"
        );

        //    c) Changing `opacity` from `1.0` to `0.55` at unchanged `hud_generation` triggers a
        //       new swapchain render (for the HUD slider), updates `SetLayerProperties` with
        //       `opacity: Some(0.55)` and `BlendMode2::StraightAlpha`, and attaches slot 0.
        let opacity_params = OverlayRenderParams { opacity: 0.55, ..resized_params };
        let RenderOutcome::Rendered { present_needed } = OverlayRenderer::render_and_attach(
            &mut presenter,
            &flatland,
            &hit_targets,
            &opacity_params,
        )
        .unwrap() else {
            panic!("Expected Rendered on opacity change");
        };
        assert_eq!(present_needed, PresentNeeded::Yes);
        assert_eq!(presenter.swapchain.render_calls, 2);
        let Some(ui_comp::FlatlandRequest::SetLayerProperties { properties, .. }) =
            stream.try_next().await.expect("opacity SetLayerProperties")
        else {
            panic!("Expected SetLayerProperties on opacity change");
        };
        assert_eq!(properties.opacity, Some(0.55));
        assert_eq!(properties.blend_mode, Some(ui_comp::BlendMode2::StraightAlpha));
        let Some(ui_comp::FlatlandRequest::SetLayerImage {
            image_id,
            acquire_fence,
            release_fence,
            ..
        }) = stream.try_next().await.expect("opacity SetLayerImage")
        else {
            panic!("Expected SetLayerImage on opacity change");
        };
        assert_eq!(image_id, image_ids[0]);
        let Some(ui_comp::WaitFence::Basic(wait_handle)) = acquire_fence.as_deref() else {
            panic!("Expected WaitFence::Basic");
        };
        assert_eq!(wait_handle.koid().unwrap(), acq_op_koid);
        let Some(ui_comp::SignalFence::Basic(sig_handle)) = release_fence.as_deref() else {
            panic!("Expected SignalFence::Basic");
        };
        assert_eq!(sig_handle.koid().unwrap(), rel_op_koid);
        assert!(stream.next().now_or_never().is_none());

        //    d) Passing `opacity: 0.554` (`55%` after rounding) skips rendering and emits zero FIDL
        //       calls.
        let rounded_same_opacity_params = OverlayRenderParams { opacity: 0.554, ..resized_params };
        let RenderOutcome::Rendered { present_needed } = OverlayRenderer::render_and_attach(
            &mut presenter,
            &flatland,
            &hit_targets,
            &rounded_same_opacity_params,
        )
        .unwrap() else {
            panic!("Expected Rendered on rounded-identical opacity");
        };
        assert_eq!(present_needed, PresentNeeded::No);
        assert_eq!(presenter.swapchain.render_calls, 2);
        assert!(stream.next().now_or_never().is_none());

        // 3. `AcquireOutcome::Deferred`, `render_frame()` failure, and `set_layer_image()` channel
        // failure all leave `last_rendered_frame` unchanged so a subsequent retry still renders.
        let prev_rendered_frame = presenter.last_rendered_frame;
        let updated_content_params = OverlayRenderParams { hud_generation: 2, ..opacity_params };
        let RenderOutcome::Deferred { release_fence } = OverlayRenderer::render_and_attach(
            &mut presenter,
            &flatland,
            &hit_targets,
            &updated_content_params,
        )
        .unwrap() else {
            panic!("Expected Deferred when swapchain returns Deferred");
        };
        assert_eq!(release_fence.koid().unwrap(), deferred_koid);
        assert_eq!(presenter.last_rendered_frame, prev_rendered_frame);
        assert_eq!(presenter.swapchain.render_calls, 2);
        assert!(stream.next().now_or_never().is_none());

        assert!(
            OverlayRenderer::render_and_attach(
                &mut presenter,
                &flatland,
                &hit_targets,
                &updated_content_params,
            )
            .is_err()
        );
        assert_eq!(presenter.swapchain.render_calls, 3);
        assert_eq!(presenter.last_rendered_frame, prev_rendered_frame);
        assert!(stream.next().now_or_never().is_none());

        let (bad_client, _bad_server) = create_endpoints::<ui_comp::FlatlandMarker>();
        let no_write_channel = bad_client.into_channel().replace_handle(zx::Rights::WAIT).unwrap();
        let failing_flatland =
            ui_comp::FlatlandProxy::new(fasync::Channel::from_channel(no_write_channel));
        assert!(
            OverlayRenderer::render_and_attach(
                &mut presenter,
                &failing_flatland,
                &hit_targets,
                &updated_content_params,
            )
            .is_err()
        );
        assert_eq!(presenter.swapchain.render_calls, 4);
        assert_eq!(presenter.last_rendered_frame, prev_rendered_frame);

        // 4. Retrying the content update with unchanged `LayerState` renders into slot 2, calls
        // `SetLayerImage()` only (no redundant `SetLayerProperties()` or `SetTranslation()`), and
        // returns `PresentNeeded::Yes`.
        let RenderOutcome::Rendered { present_needed } = OverlayRenderer::render_and_attach(
            &mut presenter,
            &flatland,
            &hit_targets,
            &updated_content_params,
        )
        .unwrap() else {
            panic!("Expected Rendered after Deferred and transient errors clear");
        };
        assert_eq!(present_needed, PresentNeeded::Yes);
        assert_eq!(presenter.swapchain.render_calls, 5);
        let Some(ui_comp::FlatlandRequest::SetLayerImage {
            image_id,
            acquire_fence,
            release_fence,
            ..
        }) = stream.try_next().await.expect("SetLayerImage on content update")
        else {
            panic!("Expected SetLayerImage on content update");
        };
        assert_eq!(image_id, image_ids[2]);
        let Some(ui_comp::WaitFence::Basic(wait_handle)) = acquire_fence.as_deref() else {
            panic!("Expected WaitFence::Basic");
        };
        assert_eq!(wait_handle.koid().unwrap(), acq1_koid);
        let Some(ui_comp::SignalFence::Basic(sig_handle)) = release_fence.as_deref() else {
            panic!("Expected SignalFence::Basic");
        };
        assert_eq!(sig_handle.koid().unwrap(), rel1_koid);
        assert!(stream.next().now_or_never().is_none());

        // 5. `OverlayRenderer::release(Box::new(presenter), &flatland)` emits `ReleaseImage2` for
        // every image ID, and fails on an unwritable channel.
        OverlayRenderer::release(Box::new(presenter), &flatland).expect("release presenter images");
        for expected_id in &image_ids {
            let Some(ui_comp::FlatlandRequest::ReleaseImage2 { image_id, .. }) =
                stream.try_next().await.expect("ReleaseImage2")
            else {
                panic!("Expected ReleaseImage2");
            };
            assert_eq!(image_id, *expected_id);
        }

        let failing_presenter = FlatlandOverlayRenderer {
            swapchain: FakeOverlaySwapchain { entries: VecDeque::new(), render_calls: 0 },
            renderer: (),
            image_ids,
            transform_id,
            layer_id,
            layer_state: LayerState::default(),
            last_rendered_frame: None,
        };
        assert!(OverlayRenderer::release(Box::new(failing_presenter), &failing_flatland).is_err());
    }

    #[fuchsia::test]
    async fn test_media_buttons_listener_forwards_events() {
        let (registry_proxy, mut registry_stream) =
            create_proxy_and_stream::<DeviceListenerRegistryMarker>();
        let (sender, mut receiver) = unbounded();
        let _task = spawn_media_buttons_listener_with_registry(registry_proxy, sender);

        let Some(DeviceListenerRegistryRequest::RegisterListener { listener, responder }) =
            registry_stream.try_next().await.expect("register request")
        else {
            panic!("Expected RegisterListener request");
        };
        responder.send().expect("ack register");

        let listener_proxy = listener.into_proxy();
        let (lease_a, _lease_b) = fidl::EventPair::create();
        let mut event = press_event();
        event.wake_lease = Some(lease_a);
        listener_proxy.on_event(event).await.expect("send media button event");

        let Some(OverlayEvent::MediaButtons(ev)) = receiver.next().await else {
            panic!("Expected OverlayEvent::MediaButtons");
        };
        assert_eq!(ev.volume_up, Some(true));
        assert_eq!(ev.volume_down, Some(true));
        assert!(ev.wake_lease.is_none());
    }
}
