// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure Vulkan HUD renderer (`DebugFont` + `BlitCache`) and overlay layout geometry.

use anyhow::{Context, Error, ensure};
use ash::vk;
use fidl_fuchsia_math as fmath;
use fuchsia_trace as ftrace;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::debug_font::{COLOR_SUBRESOURCE_RANGE, Color, DebugFont, GLYPH_WIDTH, image_barrier};
use crate::overlay::{DIAGNOSTIC_LINE_COUNT, HudLine, OverlayDisplayMode};
use crate::vulkan::{RenderContext, VulkanDevice};

/// Width in pixels of the offscreen overlay Vulkan framebuffer.
pub const OVERLAY_BUFFER_WIDTH: u32 = 480;

/// Height in pixels of the offscreen overlay Vulkan framebuffer.
pub const OVERLAY_BUFFER_HEIGHT: u32 = 276;

/// Minimum allowed overlay opacity (`10%`), keeping the HUD legible when faded.
pub const MIN_OVERLAY_OPACITY: f32 = 0.10;

/// Maximum allowed overlay opacity (`100%`, fully opaque).
pub const MAX_OVERLAY_OPACITY: f32 = 1.00;

/// Default overlay opacity (`80%`, translucent) when first shown so underlying UI remains visible
/// and interactive through non-control regions of the card.
pub const DEFAULT_OVERLAY_OPACITY: f32 = 0.80;

const OVERLAY_BUFFER_WIDTH_I32: i32 = OVERLAY_BUFFER_WIDTH as i32;
const OVERLAY_BUFFER_HEIGHT_I32: i32 = OVERLAY_BUFFER_HEIGHT as i32;

/// Height of the top header bar inside the `480x276` overlay buffer.
pub(crate) const HEADER_BUF_HEIGHT: i32 = 46;

/// Visual rectangle of the `[X]` close button inside the `480x276` overlay buffer.
pub(crate) const CLOSE_BUTTON_BUF_X: i32 = 434;
pub(crate) const CLOSE_BUTTON_BUF_Y: i32 = 5;
pub(crate) const CLOSE_BUTTON_BUF_W: i32 = 40;
pub(crate) const CLOSE_BUTTON_BUF_H: i32 = 36;

/// Secondary close-confirmation dialog button rectangles inside the `480x276` overlay buffer.
pub(crate) const CONFIRM_CLOSE_BTN_BUF_X: i32 = 64;
pub(crate) const CONFIRM_CANCEL_BTN_BUF_X: i32 = 256;
pub(crate) const CONFIRM_BTN_BUF_Y: i32 = 132;
pub(crate) const CONFIRM_BTN_BUF_W: i32 = 160;
pub(crate) const CONFIRM_BTN_BUF_H: i32 = 56;

/// Bottom bar geometry inside the `480x276` overlay buffer.
pub(crate) const BOTTOM_BAR_BUF_Y: i32 = 232;
pub(crate) const BOTTOM_BAR_BUF_H: i32 = 44;

/// Visual slider track and thumb geometry inside the bottom bar.
pub(crate) const SLIDER_TRACK_BUF_X: i32 = 92;
pub(crate) const SLIDER_TRACK_BUF_Y: i32 = 250;
pub(crate) const SLIDER_TRACK_BUF_W: i32 = 324;
pub(crate) const SLIDER_TRACK_BUF_H: i32 = 8;
const SLIDER_THUMB_BUF_W: i32 = 16;
const SLIDER_THUMB_BUF_H: i32 = 24;

const HEADER_TEXT_X: i32 = 10;
const HEADER_TITLE_Y: i32 = 6;
const HEADER_SUBTITLE_Y: i32 = 30;

const CONFIRM_PROMPT_X: i32 = 130;
const CONFIRM_PROMPT_LINE1_Y: i32 = 68;
const CONFIRM_PROMPT_LINE2_Y: i32 = 94;

const BODY_TEXT_X: i32 = 12;
const BODY_TEXT_START_Y: i32 = 54;
const BODY_TEXT_LINE_STEP_Y: i32 = 22;
const BODY_TEXT_BOTTOM_MARGIN: i32 = 4;
const BODY_TEXT_MIN_VISIBLE_H: i32 = 14;

const BOTTOM_BAR_ACCENT_BORDER_H: i32 = 2;
const OPACITY_LABEL_X: i32 = 12;
const BOTTOM_BAR_TEXT_OFFSET_Y: i32 = 15;

const SLIDER_THUMB_OFFSET_Y: i32 = 10;
const SLIDER_PCT_LABEL_X: i32 = 432;

/// Clamps finite opacity values into `[MIN_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY]`, falling back
/// to [`DEFAULT_OVERLAY_OPACITY`] for non-finite inputs (`NaN` / infinity).
#[inline]
pub(crate) fn sanitize_opacity(opacity: f32) -> f32 {
    if opacity.is_finite() {
        opacity.clamp(MIN_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY)
    } else {
        DEFAULT_OVERLAY_OPACITY
    }
}

/// Converts `opacity` into an integer percentage in `[10, 100]` after clamping via
/// `sanitize_opacity()`.
#[inline]
pub(crate) fn sanitize_opacity_pct(opacity: f32) -> u8 {
    (sanitize_opacity(opacity) * 100.0).round() as u8
}

/// Cache key identifying the static HUD base frame (excluding the opacity slider percentage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderedFrameKey {
    mode: OverlayDisplayMode,
    confirm_close: bool,
    hud_generation: u64,
}

impl RenderedFrameKey {
    #[inline]
    pub(crate) fn from_params(params: &OverlayRenderParams<'_>) -> Self {
        // Close-confirmation renders a static prompt independent of `hud_lines`, so ignore
        // `hud_generation` increments while `confirm_close` is active.
        let hud_generation = if params.confirm_close { 0 } else { params.hud_generation };
        Self { mode: params.mode, confirm_close: params.confirm_close, hud_generation }
    }
}

/// Parameters describing the HUD frame state and placement for [`HudRenderer`] and the Flatland
/// presenter.
pub(crate) struct OverlayRenderParams<'a> {
    /// Logical display size in pixels used by the presenter's `LayerState` to compute clamped
    /// Flatland layer placement; ignored by [`HudRenderer`], which always renders into the fixed
    /// [`OVERLAY_BUFFER_WIDTH`] x [`OVERLAY_BUFFER_HEIGHT`] buffer.
    pub display_size: fmath::SizeU,
    /// Optional user-dragged top-left origin in logical display coordinates (`None` centers near
    /// the top of the screen), used only by the presenter's `LayerState` and ignored by
    /// [`HudRenderer`].
    pub custom_origin: Option<(i32, i32)>,
    /// Current [`OverlayDisplayMode`] controlling the header banner color, title, and subtitle.
    pub mode: OverlayDisplayMode,
    /// When `true`, renders the close-confirmation dialog instead of `hud_lines`.
    pub confirm_close: bool,
    /// Requested overlay opacity in `[MIN_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY]`.
    pub opacity: f32,
    /// Monotonic generation counter for `hud_lines`; callers must increment this whenever
    /// `hud_lines` changes so cached base blits are invalidated (ignored while `confirm_close` is
    /// `true`).
    pub hud_generation: u64,
    /// Diagnostic text lines rendered in the card body when `confirm_close` is `false`.
    pub hud_lines: &'a [HudLine; DIAGNOSTIC_LINE_COUNT],
}

struct BlitCache {
    cached_base_frame: Option<RenderedFrameKey>,
    cached_base_blits: Vec<vk::ImageBlit>,
    blit_scratch: Vec<vk::ImageBlit>,
}

impl BlitCache {
    fn new() -> Self {
        Self {
            cached_base_frame: None,
            cached_base_blits: Vec::with_capacity(512),
            blit_scratch: Vec::with_capacity(512),
        }
    }

    fn prepare(&mut self, params: &OverlayRenderParams<'_>) -> &[vk::ImageBlit] {
        ftrace::duration!(c"gfx", c"BlitCache::prepare");
        let frame_key = RenderedFrameKey::from_params(params);
        let opacity_pct = sanitize_opacity_pct(params.opacity);
        if self.cached_base_frame != Some(frame_key) {
            let lines: &[HudLine] =
                if frame_key.confirm_close { &[] } else { params.hud_lines.as_slice() };
            build_overlay_base_blit_regions(
                &mut self.cached_base_blits,
                frame_key.mode,
                frame_key.confirm_close,
                lines,
            );
            self.cached_base_frame = Some(frame_key);
        }
        self.blit_scratch.clear();
        self.blit_scratch.extend_from_slice(&self.cached_base_blits);
        append_slider_widgets_blits(&mut self.blit_scratch, frame_key.mode, opacity_pct);
        &self.blit_scratch
    }
}

/// Pure Vulkan HUD renderer owning the [`DebugFont`] GPU atlas and CPU [`BlitCache`].
///
/// # Drop Order Contract
///
/// `Rc<VulkanDevice>` orders only logical device teardown. [`HudRenderer`] must be dropped *after*
/// any [`crate::swapchain::Swapchain`] whose recorded command buffers reference its [`DebugFont`]
/// atlas image (for example, by declaring `swapchain` before `renderer` in the presenter struct).
pub(crate) struct HudRenderer {
    debug_font: DebugFont,
    blit_cache: BlitCache,
}

impl HudRenderer {
    /// Uploads the [`DebugFont`] atlas to `vulkan` and initializes an empty [`BlitCache`].
    ///
    /// `vulkan` must not be abandoned and must have been created with
    /// [`crate::debug_font::ATLAS_FORMAT`] supporting
    /// [`crate::debug_font::ATLAS_FORMAT_FEATURES`].
    pub(crate) fn new(vulkan: Rc<VulkanDevice>) -> Result<Self, Error> {
        ftrace::duration!(c"gfx", c"HudRenderer::new");
        let debug_font = DebugFont::new(vulkan).context("initialize Vulkan DebugFont atlas")?;
        Ok(Self { debug_font, blit_cache: BlitCache::new() })
    }

    /// Records commands onto `ctx.command_buffer()` to transition `ctx.image()` from
    /// `vk::ImageLayout::UNDEFINED` (`VK_QUEUE_FAMILY_FOREIGN_EXT` -> `ctx.queue_family_index()`)
    /// to `vk::ImageLayout::TRANSFER_DST_OPTIMAL`, clear it with `vkCmdClearColorImage()`, blit all
    /// HUD rectangles and text glyphs from [`DebugFont`], and release `ctx.image()` back from
    /// `vk::ImageLayout::TRANSFER_DST_OPTIMAL` (`ctx.queue_family_index()` ->
    /// `VK_QUEUE_FAMILY_FOREIGN_EXT`) to `vk::ImageLayout::GENERAL`.
    ///
    /// Returns an error without recording GPU commands if `ctx.device()` does not match this
    /// renderer's [`VulkanDevice`] or if `ctx.extent()` is smaller than
    /// [`OVERLAY_BUFFER_WIDTH`] x [`OVERLAY_BUFFER_HEIGHT`].
    pub(crate) fn draw_frame(
        &mut self,
        ctx: RenderContext<'_>,
        params: &OverlayRenderParams<'_>,
    ) -> Result<(), Error> {
        ftrace::duration!(c"gfx", c"HudRenderer::draw_frame");
        ensure!(
            ctx.device().handle() == self.debug_font.vulkan_device_handle(),
            "RenderContext device does not match HudRenderer VulkanDevice"
        );
        let extent = ctx.extent();
        ensure!(
            extent.width >= OVERLAY_BUFFER_WIDTH && extent.height >= OVERLAY_BUFFER_HEIGHT,
            "RenderContext extent {}x{} is smaller than required {}x{}",
            extent.width,
            extent.height,
            OVERLAY_BUFFER_WIDTH,
            OVERLAY_BUFFER_HEIGHT,
        );

        let blits = self.blit_cache.prepare(params);
        let device = ctx.device();
        let cb = ctx.command_buffer();
        let image = ctx.image();
        let qfi = ctx.queue_family_index();
        let acquire_barrier = image_barrier(
            image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
            vk::QUEUE_FAMILY_FOREIGN_EXT,
            qfi,
        );
        let release_barrier = image_barrier(
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
            qfi,
            vk::QUEUE_FAMILY_FOREIGN_EXT,
        );
        let clear_rgba = Color::BodyBg.rgba();
        let clear_color = vk::ClearColorValue { float32: clear_rgba.map(|c| c as f32 / 255.0) };

        // SAFETY: `RenderContext` guarantees `cb` is recording on `device` and `image` is an idle
        // `TRANSFER_DST` image with dimensions `extent`; the checks above verified `device` matches
        // `self.debug_font` and `extent >= OVERLAY_BUFFER_WIDTH x OVERLAY_BUFFER_HEIGHT` so every
        // HUD blit region lies within `image`, and `self.debug_font` outlives the submission.
        unsafe {
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire_barrier],
            );
            device.cmd_clear_color_image(
                cb,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &clear_color,
                &[COLOR_SUBRESOURCE_RANGE],
            );
            let clear_to_blit_barrier = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[clear_to_blit_barrier],
                &[],
                &[],
            );
            // TODO(https://fxbug.dev/571154588): This single vkCmdBlitImage() call relies on
            // in-order application of overlapping regions, which the Vulkan spec does not
            // guarantee.
            self.debug_font.submit_blit_regions(cb, image, blits);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release_barrier],
            );
        }
        Ok(())
    }
}

/// Computes the clamped logical placement `(x, y, dest_w, dest_h)` of the HUD card inside
/// `display_size`.
pub fn compute_overlay_placement(
    display_size: fmath::SizeU,
    custom_origin: Option<(i32, i32)>,
) -> (i32, i32, u32, u32) {
    // Enforce a `320x240` floor so uninitialized `0x0` display sizes still produce a legible
    // `280x160` card, and cap at `i32::MAX` so coordinate casts never wrap negative.
    let disp_w = display_size.width.clamp(320, i32::MAX as u32);
    let disp_h = display_size.height.clamp(240, i32::MAX as u32);
    let margin_x = (disp_w / 16).max(12);
    let dest_w = disp_w.saturating_sub(margin_x * 2).max(280);
    let dest_h =
        ((dest_w as u64 * OVERLAY_BUFFER_HEIGHT as u64) / OVERLAY_BUFFER_WIDTH as u64) as u32;
    let dest_h = dest_h.min(disp_h.saturating_sub(24)).max(160);

    let max_x = disp_w.saturating_sub(dest_w) as i32;
    let max_y = disp_h.saturating_sub(dest_h) as i32;
    let (x, y) = match custom_origin {
        Some((cx, cy)) => (cx.clamp(0, max_x), cy.clamp(0, max_y)),
        None => {
            let default_x = ((disp_w.saturating_sub(dest_w)) / 2) as i32;
            let default_y = (disp_h / 12).min(disp_h.saturating_sub(dest_h) / 2).max(12) as i32;
            (default_x.clamp(0, max_x), default_y.clamp(0, max_y))
        }
    };

    (x, y, dest_w, dest_h)
}

#[inline]
const fn mode_accent(mode: OverlayDisplayMode) -> Color {
    match mode {
        OverlayDisplayMode::ActiveCountdown { remaining_presses: 1 } => Color::AccentRed,
        OverlayDisplayMode::ActiveCountdown { .. } => Color::AccentOrange,
        OverlayDisplayMode::Rebooting => Color::AccentReboot,
        OverlayDisplayMode::DiagnosticsIdle | OverlayDisplayMode::Hidden => Color::AccentIdle,
    }
}

/// Populates `regions` with all base `vk::ImageBlit` commands required to render the HUD card
/// into the fixed `480x276` (`OVERLAY_BUFFER_WIDTH x OVERLAY_BUFFER_HEIGHT`) buffer, excluding the
/// dynamic opacity slider widgets.
fn build_overlay_base_blit_regions(
    regions: &mut Vec<vk::ImageBlit>,
    mode: OverlayDisplayMode,
    confirm_close: bool,
    lines: &[HudLine],
) {
    regions.clear();
    let accent = mode_accent(mode);

    DebugFont::append_rect_blit(regions, 0, 0, OVERLAY_BUFFER_WIDTH_I32, HEADER_BUF_HEIGHT, accent);

    let (title, subtitle) = if confirm_close {
        ("Close diagnostic overlay?", "Confirm below to close or cancel to keep open")
    } else {
        (mode.banner_title(), mode.banner_subtitle())
    };
    DebugFont::append_text_blit_regions(
        regions,
        title,
        vk::Offset2D { x: HEADER_TEXT_X, y: HEADER_TITLE_Y },
        1,
        2,
        Color::White,
        accent,
    );
    DebugFont::append_text_blit_regions(
        regions,
        subtitle,
        vk::Offset2D { x: HEADER_TEXT_X, y: HEADER_SUBTITLE_Y },
        1,
        1,
        Color::Cream,
        accent,
    );

    DebugFont::append_button_blits(
        regions,
        CLOSE_BUTTON_BUF_X,
        CLOSE_BUTTON_BUF_Y,
        CLOSE_BUTTON_BUF_W,
        CLOSE_BUTTON_BUF_H,
        Color::CloseBtnBg,
        Color::White,
        Color::White,
        "X",
    );

    if confirm_close {
        for (y, prompt) in [
            (CONFIRM_PROMPT_LINE1_Y, "Are you sure you want to close"),
            (CONFIRM_PROMPT_LINE2_Y, "the diagnostic overlay window?"),
        ] {
            DebugFont::append_text_blit_regions(
                regions,
                prompt,
                vk::Offset2D { x: CONFIRM_PROMPT_X, y },
                1,
                2,
                Color::White,
                Color::BodyBg,
            );
        }
        DebugFont::append_button_blits(
            regions,
            CONFIRM_CLOSE_BTN_BUF_X,
            CONFIRM_BTN_BUF_Y,
            CONFIRM_BTN_BUF_W,
            CONFIRM_BTN_BUF_H,
            Color::ConfirmYesBg,
            Color::White,
            Color::White,
            "Close",
        );
        DebugFont::append_button_blits(
            regions,
            CONFIRM_CANCEL_BTN_BUF_X,
            CONFIRM_BTN_BUF_Y,
            CONFIRM_BTN_BUF_W,
            CONFIRM_BTN_BUF_H,
            Color::ConfirmNoBg,
            Color::White,
            Color::White,
            "Cancel",
        );
    } else {
        let mut y = BODY_TEXT_START_Y;
        let glyph_w = GLYPH_WIDTH as i32;
        for line in lines {
            if y + BODY_TEXT_MIN_VISIBLE_H > BOTTOM_BAR_BUF_Y - BODY_TEXT_BOTTOM_MARGIN {
                break;
            }
            let s = line.as_str();
            let (prefix, rest) = s.find(':').map_or(("", s), |idx| s.split_at(idx + 1));
            DebugFont::append_text_blit_regions(
                regions,
                prefix,
                vk::Offset2D { x: BODY_TEXT_X, y },
                1,
                2,
                Color::Cyan,
                Color::BodyBg,
            );
            DebugFont::append_text_blit_regions(
                regions,
                rest,
                vk::Offset2D { x: BODY_TEXT_X + (prefix.len() as i32) * glyph_w, y },
                1,
                2,
                Color::BodyText,
                Color::BodyBg,
            );
            y += BODY_TEXT_LINE_STEP_Y;
        }
    }

    DebugFont::append_rect_blit(
        regions,
        0,
        BOTTOM_BAR_BUF_Y,
        OVERLAY_BUFFER_WIDTH_I32,
        BOTTOM_BAR_BUF_H,
        Color::BottomBarBg,
    );
    DebugFont::append_rect_blit(
        regions,
        0,
        BOTTOM_BAR_BUF_Y,
        OVERLAY_BUFFER_WIDTH_I32,
        BOTTOM_BAR_ACCENT_BORDER_H,
        accent,
    );
    DebugFont::append_text_blit_regions(
        regions,
        "Opacity:",
        vk::Offset2D { x: OPACITY_LABEL_X, y: BOTTOM_BAR_BUF_Y + BOTTOM_BAR_TEXT_OFFSET_Y },
        1,
        2,
        Color::Cyan,
        Color::BottomBarBg,
    );

    DebugFont::append_border_blits(
        regions,
        0,
        0,
        OVERLAY_BUFFER_WIDTH_I32,
        OVERLAY_BUFFER_HEIGHT_I32,
        accent,
    );
}

fn append_slider_widgets_blits(
    regions: &mut Vec<vk::ImageBlit>,
    mode: OverlayDisplayMode,
    opacity_pct: u8,
) {
    let pct = u32::from(opacity_pct).clamp(10, 100);
    let mut pct_line = HudLine::new();
    let _ = write!(&mut pct_line, "{pct:3}%");
    DebugFont::append_text_blit_regions(
        regions,
        pct_line.as_str(),
        vk::Offset2D { x: SLIDER_PCT_LABEL_X, y: BOTTOM_BAR_BUF_Y + BOTTOM_BAR_TEXT_OFFSET_Y },
        1,
        2,
        Color::White,
        Color::BottomBarBg,
    );

    let clamped_opacity = (pct as f32 / 100.0).clamp(MIN_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY);
    let norm_t = ((clamped_opacity - MIN_OVERLAY_OPACITY)
        / (MAX_OVERLAY_OPACITY - MIN_OVERLAY_OPACITY))
        .clamp(0.0, 1.0);
    let fill_w = ((SLIDER_TRACK_BUF_W as f32) * norm_t).round() as i32;
    DebugFont::append_rect_blit(
        regions,
        SLIDER_TRACK_BUF_X,
        SLIDER_TRACK_BUF_Y,
        SLIDER_TRACK_BUF_W,
        SLIDER_TRACK_BUF_H,
        Color::SliderTrackBg,
    );
    DebugFont::append_rect_blit(
        regions,
        SLIDER_TRACK_BUF_X,
        SLIDER_TRACK_BUF_Y,
        fill_w,
        SLIDER_TRACK_BUF_H,
        Color::SliderTrackFill,
    );

    let thumb_x = (SLIDER_TRACK_BUF_X + fill_w - SLIDER_THUMB_BUF_W / 2).max(0);
    DebugFont::append_bordered_box_blits(
        regions,
        thumb_x,
        BOTTOM_BAR_BUF_Y + SLIDER_THUMB_OFFSET_Y,
        SLIDER_THUMB_BUF_W,
        SLIDER_THUMB_BUF_H,
        Color::SliderThumbBg,
        mode_accent(mode),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_font::{ATLAS_HEIGHT, ATLAS_WIDTH};
    use crate::overlay::{MAX_DIAGNOSTIC_LINE_CHARS, default_hud_lines};

    /// Nearest-neighbor CPU reference blitter applying `regions` from `atlas_pixels`
    /// (`ATLAS_WIDTH x ATLAS_HEIGHT`) into `dst_pixels` (`OVERLAY_BUFFER_WIDTH x
    /// OVERLAY_BUFFER_HEIGHT`).
    fn cpu_blit_nearest(atlas_pixels: &[u8], dst_pixels: &mut [u8], regions: &[vk::ImageBlit]) {
        let atlas_w = ATLAS_WIDTH as i32;
        let atlas_h = ATLAS_HEIGHT as i32;
        let dst_buf_w = OVERLAY_BUFFER_WIDTH_I32;
        let dst_buf_h = OVERLAY_BUFFER_HEIGHT_I32;
        for r in regions {
            let sx0 = r.src_offsets[0].x;
            let sy0 = r.src_offsets[0].y;
            let sw = r.src_offsets[1].x - sx0;
            let sh = r.src_offsets[1].y - sy0;
            let dx0 = r.dst_offsets[0].x;
            let dy0 = r.dst_offsets[0].y;
            let dw = r.dst_offsets[1].x - dx0;
            let dh = r.dst_offsets[1].y - dy0;
            assert!(sx0 >= 0 && sx0 + sw <= atlas_w && sw > 0);
            assert!(sy0 >= 0 && sy0 + sh <= atlas_h && sh > 0);
            assert!(dx0 >= 0 && dx0 + dw <= dst_buf_w && dw > 0);
            assert!(dy0 >= 0 && dy0 + dh <= dst_buf_h && dh > 0);

            for dy in 0..dh {
                let sy = sy0 + (dy * sh) / dh;
                for dx in 0..dw {
                    let sx = sx0 + (dx * sw) / dw;
                    let src_idx = ((sy * atlas_w + sx) * 4) as usize;
                    let dst_idx = (((dy0 + dy) * dst_buf_w + (dx0 + dx)) * 4) as usize;
                    dst_pixels[dst_idx..dst_idx + 4]
                        .copy_from_slice(&atlas_pixels[src_idx..src_idx + 4]);
                }
            }
        }
    }

    fn dst_pixel(dst_pixels: &[u8], x: i32, y: i32) -> [u8; 4] {
        let idx = ((y * OVERLAY_BUFFER_WIDTH_I32 + x) * 4) as usize;
        dst_pixels[idx..idx + 4].try_into().expect("4 RGBA bytes")
    }

    fn assert_regions_in_bounds(regions: &[vk::ImageBlit], case_name: &str) {
        for (idx, r) in regions.iter().enumerate() {
            assert!(
                r.src_offsets[0].x >= 0
                    && r.src_offsets[0].x < r.src_offsets[1].x
                    && r.src_offsets[1].x <= ATLAS_WIDTH as i32,
                "{case_name}: region {idx} src x out of bounds: ({}, {})..({}, {})",
                r.src_offsets[0].x,
                r.src_offsets[0].y,
                r.src_offsets[1].x,
                r.src_offsets[1].y
            );
            assert!(
                r.src_offsets[0].y >= 0
                    && r.src_offsets[0].y < r.src_offsets[1].y
                    && r.src_offsets[1].y <= ATLAS_HEIGHT as i32,
                "{case_name}: region {idx} src y out of bounds: ({}, {})..({}, {})",
                r.src_offsets[0].x,
                r.src_offsets[0].y,
                r.src_offsets[1].x,
                r.src_offsets[1].y
            );
            assert!(
                r.dst_offsets[0].x >= 0
                    && r.dst_offsets[0].x < r.dst_offsets[1].x
                    && r.dst_offsets[1].x <= OVERLAY_BUFFER_WIDTH_I32,
                "{case_name}: region {idx} dst x out of bounds: ({}, {})..({}, {})",
                r.dst_offsets[0].x,
                r.dst_offsets[0].y,
                r.dst_offsets[1].x,
                r.dst_offsets[1].y
            );
            assert!(
                r.dst_offsets[0].y >= 0
                    && r.dst_offsets[0].y < r.dst_offsets[1].y
                    && r.dst_offsets[1].y <= OVERLAY_BUFFER_HEIGHT_I32,
                "{case_name}: region {idx} dst y out of bounds: ({}, {})..({}, {})",
                r.dst_offsets[0].x,
                r.dst_offsets[0].y,
                r.dst_offsets[1].x,
                r.dst_offsets[1].y
            );
        }
    }

    #[fuchsia::test]
    fn test_cpu_blitter_rect_scaled_glyph_and_button() {
        let atlas_pixels = DebugFont::atlas_pixels();
        let mut dst =
            Color::BodyBg.rgba().repeat((OVERLAY_BUFFER_WIDTH * OVERLAY_BUFFER_HEIGHT) as usize);

        // 1. `append_rect_blit()` fills exact rectangle and leaves neighbors untouched.
        let mut regions = Vec::new();
        DebugFont::append_rect_blit(&mut regions, 10, 20, 6, 4, Color::AccentOrange);
        cpu_blit_nearest(&atlas_pixels, &mut dst, &regions);
        for y in 20..24 {
            for x in 10..16 {
                assert_eq!(dst_pixel(&dst, x, y), Color::AccentOrange.rgba());
            }
        }
        assert_eq!(dst_pixel(&dst, 9, 20), Color::BodyBg.rgba());
        assert_eq!(dst_pixel(&dst, 16, 23), Color::BodyBg.rgba());

        // 2. Scale-(1, 2) glyph `'A'` at `(40, 50)`: inner glyph spans x = 41..46 (5px wide) and
        //    y = 52..62 (10px tall, 2x vertical stretch). Top apex pixel `(x=2, y=0)` of `'A'`
        //    maps to `x = 43` and `y = 52..54` (2 rows), while `x = 44` stays `BodyBg` (catches
        //    swapped `scale_x`/`scale_y`).
        regions.clear();
        DebugFont::append_text_blit_regions(
            &mut regions,
            "A",
            vk::Offset2D { x: 40, y: 50 },
            1,
            2,
            Color::White,
            Color::BodyBg,
        );
        cpu_blit_nearest(&atlas_pixels, &mut dst, &regions);
        assert_eq!(dst_pixel(&dst, 43, 52), Color::White.rgba());
        assert_eq!(dst_pixel(&dst, 43, 53), Color::White.rgba());
        assert_eq!(dst_pixel(&dst, 44, 52), Color::BodyBg.rgba());
        assert_eq!(dst_pixel(&dst, 41, 60), Color::White.rgba());
        assert_eq!(dst_pixel(&dst, 41, 61), Color::White.rgba());

        // 3. 2x button label `"X"` centered in a `40x36` button at `(100, 100)`:
        //    `text_w = 1 * 7 * 2 = 14`, `text_h = 7 * 2 = 14`, `text_x = 113`, `text_y = 111`.
        //    Inner `'X'` spans `115..125` x `113..123`; top-left corner `(115, 113)` is `White`
        //    and top-center `(119, 113)` is `CloseBtnBg`.
        regions.clear();
        DebugFont::append_button_blits(
            &mut regions,
            100,
            100,
            40,
            36,
            Color::CloseBtnBg,
            Color::White,
            Color::White,
            "X",
        );
        cpu_blit_nearest(&atlas_pixels, &mut dst, &regions);
        assert_eq!(dst_pixel(&dst, 100, 100), Color::White.rgba());
        assert_eq!(dst_pixel(&dst, 103, 103), Color::CloseBtnBg.rgba());
        assert_eq!(dst_pixel(&dst, 115, 113), Color::White.rgba());
        assert_eq!(dst_pixel(&dst, 119, 113), Color::CloseBtnBg.rgba());
    }

    #[fuchsia::test]
    fn test_overlay_base_blit_regions_all_modes_and_close_button_isolation() {
        let atlas_pixels = DebugFont::atlas_pixels();
        // 9 lines: row 0 has no `':'` (must render in `Color::BodyText` with no `Color::Cyan`),
        // and row 8 exceeds the body height budget and must break before `BOTTOM_BAR_BUF_Y`.
        let mut full_lines = [HudLine::new(); DIAGNOSTIC_LINE_COUNT + 1];
        let _ = write!(full_lines[0], "No colon status row {}", "Y".repeat(30));
        for (i, line) in full_lines.iter_mut().enumerate().skip(1) {
            let _ = write!(line, "Row{i}: {}", "X".repeat(MAX_DIAGNOSTIC_LINE_CHARS - 6));
            assert_eq!(line.as_str().len(), MAX_DIAGNOSTIC_LINE_CHARS);
        }

        let modes = [
            OverlayDisplayMode::ActiveCountdown { remaining_presses: 1 },
            OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 },
            OverlayDisplayMode::Rebooting,
            OverlayDisplayMode::DiagnosticsIdle,
        ];

        for mode in modes {
            for confirm_close in [false, true] {
                let case_name = format!("mode={mode:?}, confirm_close={confirm_close}");
                let mut regions = Vec::new();
                build_overlay_base_blit_regions(&mut regions, mode, confirm_close, &full_lines);
                append_slider_widgets_blits(&mut regions, mode, 100);
                assert!(!regions.is_empty(), "{case_name}: expected non-empty blit regions");
                assert_regions_in_bounds(&regions, &case_name);

                let mut dst = Color::BodyBg
                    .rgba()
                    .repeat((OVERLAY_BUFFER_WIDTH * OVERLAY_BUFFER_HEIGHT) as usize);
                cpu_blit_nearest(&atlas_pixels, &mut dst, &regions);

                // Header background pixel matches `mode_accent(mode)`.
                assert_eq!(
                    dst_pixel(&dst, 4, 4),
                    mode_accent(mode).rgba(),
                    "{case_name}: header background mismatch"
                );
                // Bottom bar top border at `BOTTOM_BAR_BUF_Y` is not overwritten by row 9.
                assert_eq!(
                    dst_pixel(&dst, 4, BOTTOM_BAR_BUF_Y),
                    mode_accent(mode).rgba(),
                    "{case_name}: bottom bar accent border overwritten"
                );

                // Verify that no header/body text blit overlaps the `[X]` close button and that
                // every pixel rendered inside the close button belongs exclusively to the button's
                // background (`CloseBtnBg`) or border/label (`White`).
                for y in CLOSE_BUTTON_BUF_Y..CLOSE_BUTTON_BUF_Y + CLOSE_BUTTON_BUF_H {
                    for x in CLOSE_BUTTON_BUF_X..CLOSE_BUTTON_BUF_X + CLOSE_BUTTON_BUF_W {
                        let px = dst_pixel(&dst, x, y);
                        assert!(
                            px == Color::CloseBtnBg.rgba() || px == Color::White.rgba(),
                            "{case_name}: unexpected pixel {px:?} at ({x}, {y}) inside close button"
                        );
                    }
                }

                if confirm_close {
                    // Body line region stays `BodyBg` (no body lines drawn under confirmation) and
                    // confirmation buttons use `ConfirmYesBg` / `ConfirmNoBg`.
                    assert_eq!(
                        dst_pixel(&dst, BODY_TEXT_X + 2, BODY_TEXT_START_Y + 2),
                        Color::BodyBg.rgba(),
                        "{case_name}: body text should be suppressed when confirm_close=true"
                    );
                    assert_eq!(
                        dst_pixel(&dst, CONFIRM_CLOSE_BTN_BUF_X + 4, CONFIRM_BTN_BUF_Y + 4),
                        Color::ConfirmYesBg.rgba(),
                        "{case_name}: Close button background mismatch"
                    );
                    assert_eq!(
                        dst_pixel(&dst, CONFIRM_CANCEL_BTN_BUF_X + 4, CONFIRM_BTN_BUF_Y + 4),
                        Color::ConfirmNoBg.rgba(),
                        "{case_name}: Cancel button background mismatch"
                    );
                } else {
                    // Colon-less row 0 renders in `BodyText` with zero `Cyan` prefix pixels.
                    let mut saw_body_text = false;
                    for y in BODY_TEXT_START_Y..BODY_TEXT_START_Y + 12 {
                        for x in BODY_TEXT_X..BODY_TEXT_X + (GLYPH_WIDTH as i32) * 8 {
                            let px = dst_pixel(&dst, x, y);
                            assert_ne!(
                                px,
                                Color::Cyan.rgba(),
                                "{case_name}: colon-less row 0 should not use Cyan prefix color"
                            );
                            if px == Color::BodyText.rgba() {
                                saw_body_text = true;
                            }
                        }
                    }
                    assert!(
                        saw_body_text,
                        "{case_name}: expected BodyText pixels in colon-less row 0"
                    );
                }
            }
        }
    }

    #[fuchsia::test]
    fn test_blit_cache_prepare_reuse_invalidation_and_confirm_close() {
        let atlas_pixels = DebugFont::atlas_pixels();
        let mut cache = BlitCache::new();
        let render_with_cache =
            |cache: &mut BlitCache, params: &OverlayRenderParams<'_>| -> Vec<u8> {
                let blits = cache.prepare(params);
                assert_regions_in_bounds(blits, "BlitCache::prepare");
                let mut dst = Color::BodyBg
                    .rgba()
                    .repeat((OVERLAY_BUFFER_WIDTH * OVERLAY_BUFFER_HEIGHT) as usize);
                cpu_blit_nearest(&atlas_pixels, &mut dst, blits);
                dst
            };

        let mut lines = default_hud_lines();
        let mut params = OverlayRenderParams {
            display_size: fmath::SizeU { width: 480, height: 800 },
            custom_origin: None,
            mode: OverlayDisplayMode::DiagnosticsIdle,
            confirm_close: false,
            opacity: 1.0,
            hud_generation: 1,
            hud_lines: &lines,
        };

        // 1. Initial render populates `cached_base_blits` and draws 100% slider track fill.
        let dst_initial = render_with_cache(&mut cache, &params);
        let initial_base_len = cache.cached_base_blits.len();
        assert!(initial_base_len > 0);
        // Right end of the slider track (`SLIDER_TRACK_BUF_X + SLIDER_TRACK_BUF_W - 20`) is filled
        // at 100% opacity.
        let left_track_x = SLIDER_TRACK_BUF_X + 20;
        let mid_left_track_x = SLIDER_TRACK_BUF_X + SLIDER_TRACK_BUF_W / 4;
        let right_track_x = SLIDER_TRACK_BUF_X + SLIDER_TRACK_BUF_W - 20;
        let track_y = SLIDER_TRACK_BUF_Y + 2;
        assert_eq!(
            dst_pixel(&dst_initial, right_track_x, track_y),
            Color::SliderTrackFill.rgba(),
            "100% opacity should fill the right end of the slider track"
        );

        // At 55% opacity (halfway along `[10%, 100%]`), the first quarter of the slider track is
        // filled while the right end is unfilled.
        let params_half_opacity = OverlayRenderParams { opacity: 0.55, ..params };
        let dst_half = render_with_cache(&mut cache, &params_half_opacity);
        assert_eq!(cache.cached_base_blits.len(), initial_base_len);
        assert_eq!(
            dst_pixel(&dst_half, mid_left_track_x, track_y),
            Color::SliderTrackFill.rgba(),
            "55% opacity should fill the left half of the slider track"
        );
        assert_eq!(
            dst_pixel(&dst_half, right_track_x, track_y),
            Color::SliderTrackBg.rgba(),
            "55% opacity should leave the right half of the slider track unfilled"
        );

        // 2. Mutating `hud_lines` WITHOUT incrementing `hud_generation` while changing `opacity`
        // to `0.10` (`10%`) reuses `cached_base_blits` without rebuilding, while updating the
        // slider widgets in `blit_scratch`.
        let empty_lines = [HudLine::new(); DIAGNOSTIC_LINE_COUNT];
        let params_same_gen_new_opacity =
            OverlayRenderParams { opacity: MIN_OVERLAY_OPACITY, hud_lines: &empty_lines, ..params };
        let dst_reused = render_with_cache(&mut cache, &params_same_gen_new_opacity);
        assert_eq!(
            cache.cached_base_blits.len(),
            initial_base_len,
            "BlitCache::prepare() must reuse cached_base_blits when RenderedFrameKey matches"
        );
        assert_eq!(
            dst_pixel(&dst_reused, left_track_x, track_y),
            Color::SliderTrackBg.rgba(),
            "10% opacity should leave the left end of the slider track outside the thumb unfilled"
        );
        assert_eq!(
            dst_pixel(&dst_reused, right_track_x, track_y),
            Color::SliderTrackBg.rgba(),
            "10% opacity should leave the right end of the slider track unfilled"
        );

        // 3. Incrementing `hud_generation` invalidates `cached_base_frame` and rebuilds
        // `cached_base_blits` from the new `empty_lines`.
        let params_next_gen = OverlayRenderParams {
            hud_generation: 2,
            opacity: MIN_OVERLAY_OPACITY,
            hud_lines: &empty_lines,
            ..params
        };
        let dst_empty_body = render_with_cache(&mut cache, &params_next_gen);
        assert!(
            cache.cached_base_blits.len() < initial_base_len,
            "rebuilding with empty hud_lines should emit fewer base blits"
        );
        assert_eq!(
            dst_pixel(&dst_empty_body, BODY_TEXT_X + 2, BODY_TEXT_START_Y + 2),
            Color::BodyBg.rgba()
        );

        // 4. Changing `mode` invalidates `cached_base_frame` and updates the header accent.
        params.mode = OverlayDisplayMode::Rebooting;
        params.hud_generation = 2;
        let dst_reboot = render_with_cache(&mut cache, &params);
        assert_eq!(dst_pixel(&dst_reboot, 4, 4), Color::AccentReboot.rgba());

        // 5. Setting `confirm_close = true` suppresses non-empty `hud_lines` and normalizes
        // `hud_generation` to 0 so telemetry generation increments do not rebuild base blits.
        lines[0] = HudLine::new();
        let _ = write!(lines[0], "Row0: {}", "Z".repeat(40));
        let confirm_params_1 = OverlayRenderParams {
            display_size: fmath::SizeU { width: 480, height: 800 },
            custom_origin: None,
            mode: OverlayDisplayMode::DiagnosticsIdle,
            confirm_close: true,
            opacity: 1.0,
            hud_generation: 10,
            hud_lines: &lines,
        };
        let dst_confirm_1 = render_with_cache(&mut cache, &confirm_params_1);
        assert_eq!(
            dst_pixel(&dst_confirm_1, BODY_TEXT_X + 2, BODY_TEXT_START_Y + 2),
            Color::BodyBg.rgba(),
            "confirm_close=true must suppress non-empty hud_lines"
        );
        assert_eq!(
            dst_pixel(&dst_confirm_1, CONFIRM_CLOSE_BTN_BUF_X + 4, CONFIRM_BTN_BUF_Y + 4),
            Color::ConfirmYesBg.rgba()
        );

        let confirm_base_len = cache.cached_base_blits.len();
        DebugFont::append_rect_blit(
            &mut cache.cached_base_blits,
            BODY_TEXT_X,
            BODY_TEXT_START_Y,
            1,
            1,
            Color::BodyBg,
        );
        let confirm_params_2 = OverlayRenderParams { hud_generation: 99, ..confirm_params_1 };
        let dst_confirm_2 = render_with_cache(&mut cache, &confirm_params_2);
        assert_eq!(cache.cached_base_blits.len(), confirm_base_len + 1);
        assert_eq!(dst_confirm_1, dst_confirm_2);
    }

    #[fuchsia::test]
    fn test_sanitize_opacity_and_rendered_frame_key() {
        assert_eq!(sanitize_opacity(0.25), 0.25);
        assert_eq!(sanitize_opacity_pct(0.25), 25);
        assert_eq!(sanitize_opacity(f32::NAN), DEFAULT_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity_pct(f32::NAN), 80);
        assert_eq!(sanitize_opacity(f32::INFINITY), DEFAULT_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity(f32::NEG_INFINITY), DEFAULT_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity(-0.5), MIN_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity_pct(-0.5), 10);
        assert_eq!(sanitize_opacity(0.0), MIN_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity(2.0), MAX_OVERLAY_OPACITY);
        assert_eq!(sanitize_opacity_pct(2.0), 100);

        let hud_lines = default_hud_lines();
        let base_params = OverlayRenderParams {
            display_size: fmath::SizeU { width: 480, height: 800 },
            custom_origin: None,
            mode: OverlayDisplayMode::DiagnosticsIdle,
            confirm_close: false,
            opacity: 1.0,
            hud_generation: 7,
            hud_lines: &hud_lines,
        };
        let key_a = RenderedFrameKey::from_params(&base_params);
        let key_b =
            RenderedFrameKey::from_params(&OverlayRenderParams { opacity: 0.55, ..base_params });
        assert_eq!(key_a, key_b, "opacity change must not alter RenderedFrameKey");

        let key_next_gen = RenderedFrameKey::from_params(&OverlayRenderParams {
            hud_generation: 8,
            ..base_params
        });
        assert_ne!(key_a, key_next_gen);

        let key_confirm_1 = RenderedFrameKey::from_params(&OverlayRenderParams {
            confirm_close: true,
            hud_generation: 8,
            ..base_params
        });
        let key_confirm_2 = RenderedFrameKey::from_params(&OverlayRenderParams {
            confirm_close: true,
            hud_generation: 99,
            ..base_params
        });
        assert_eq!(key_confirm_1, key_confirm_2);
    }

    #[fuchsia::test]
    fn test_compute_overlay_placement_clamping() {
        // Standard portrait display (`480x800`).
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 480, height: 800 }, None),
            (30, 66, 420, 241)
        );

        // Minimum floor (`320x240`) and uninitialized (`0x0`).
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 320, height: 240 }, None),
            (20, 20, 280, 161)
        );
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 0, height: 0 }, None),
            (20, 20, 280, 161)
        );

        // Wide, short display (`1280x240`) hitting `dest_h` clamp (`240 - 24 = 216`) and
        // `default_y` clamp (`12`).
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 1280, height: 240 }, None),
            (80, 12, 1120, 216)
        );

        // In-bounds and out-of-bounds `custom_origin` clamps into `[0..=max_x, 0..=max_y]`.
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 480, height: 800 }, Some((25, 120))),
            (25, 120, 420, 241)
        );
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 480, height: 800 }, Some((-50, 9999))),
            (0, 559, 420, 241)
        );
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: 480, height: 800 }, Some((9999, -50))),
            (60, 0, 420, 241)
        );

        // Extreme dimensions (`u32::MAX`) do not overflow `i32` casts.
        assert_eq!(
            compute_overlay_placement(fmath::SizeU { width: u32::MAX, height: u32::MAX }, None),
            (134_217_727, 178_956_970, 1_879_048_193, 1_080_452_710)
        );
    }
}
