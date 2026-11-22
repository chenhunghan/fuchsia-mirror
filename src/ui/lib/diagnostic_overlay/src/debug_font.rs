// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! GPU-based bitmap font and HUD rectangle blitter ported from Escher's
//! `//src/ui/lib/escher/debug/debug_font.h` and `debug_font.cc`.
//!
//! At initialization time, `DebugFont` uploads a 7x7-cell monospace ASCII bitmap font atlas (plus
//! solid-color swatches for HUD rectangles) into a device-local `vk::Image` in
//! `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`. Per-frame rendering uses `vkCmdClearColorImage` and
//! `vkCmdBlitImage` (`VK_FILTER_NEAREST`) directly into the sysmem-backed `vk::Image` in
//! `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`, requiring zero shaders, zero render passes, and zero
//! per-frame CPU pixel writes.

use anyhow::{Error, anyhow, ensure};
use ash::vk;
use fuchsia_trace as ftrace;
use std::rc::Rc;

use crate::vulkan::{VulkanDevice, find_memory_type_index};

/// Vulkan image format used for the `DebugFont` GPU atlas.
pub(crate) const ATLAS_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Required optimal-tiling format features on [`ATLAS_FORMAT`] for staging upload (`TRANSFER_DST`)
/// and `vkCmdBlitImage()` source sampling (`TRANSFER_SRC | BLIT_SRC`).
pub(crate) const ATLAS_FORMAT_FEATURES: vk::FormatFeatureFlags = vk::FormatFeatureFlags::from_raw(
    vk::FormatFeatureFlags::TRANSFER_DST.as_raw()
        | vk::FormatFeatureFlags::TRANSFER_SRC.as_raw()
        | vk::FormatFeatureFlags::BLIT_SRC.as_raw(),
);

/// Width of each glyph cell in the atlas (5px inner glyph + 1px padding on left and right),
/// matching `escher::DebugFont::kGlyphWidth`.
pub(crate) const GLYPH_WIDTH: u32 = 7;

/// Height of each glyph cell in the atlas (5px inner glyph + 1px padding on top and bottom),
/// matching `escher::DebugFont::kGlyphHeight`.
const GLYPH_HEIGHT: u32 = 7;

/// Padding in pixels around each 5x5 glyph cell, matching `escher::DebugFont::kGlyphPadding`.
const GLYPH_PADDING: u32 = 1;

/// First printable ASCII code stored in the compact GPU atlas (`b' '` = 32).
const FIRST_ATLAS_GLYPH: u32 = 32;

/// Number of printable ASCII glyphs (`32..=126` = 95) plus 1 fallback slot (row 95) for any
/// non-printable or non-ASCII byte.
const ATLAS_GLYPH_COUNT: u32 = 96;

const INNER_WIDTH: u32 = GLYPH_WIDTH - 2 * GLYPH_PADDING;
const INNER_HEIGHT: u32 = GLYPH_HEIGHT - 2 * GLYPH_PADDING;
const MAX_GLYPH_SHIFT: u32 = INNER_WIDTH * INNER_HEIGHT - 1;

pub(crate) const COLOR_SUBRESOURCE_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

const COLOR_SUBRESOURCE_LAYERS: vk::ImageSubresourceLayers = vk::ImageSubresourceLayers {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    mip_level: 0,
    base_array_layer: 0,
    layer_count: 1,
};

/// Semantic color palette used for both HUD rectangle swatches and `(fg, bg)` glyph rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Color {
    BodyBg = 0,
    BottomBarBg = 1,
    AccentIdle = 2,
    AccentOrange = 3,
    AccentRed = 4,
    AccentReboot = 5,
    White = 6,
    Cream = 7,
    Cyan = 8,
    BodyText = 9,
    CloseBtnBg = 10,
    ConfirmYesBg = 11,
    ConfirmNoBg = 12,
    SliderTrackBg = 13,
    SliderTrackFill = 14,
    SliderThumbBg = 15,
}

const PALETTE: [[u8; 4]; 16] = [
    [16, 20, 28, 255],    // BodyBg
    [24, 32, 46, 255],    // BottomBarBg
    [25, 115, 155, 255],  // AccentIdle
    [215, 115, 20, 255],  // AccentOrange
    [210, 45, 45, 255],   // AccentRed
    [185, 25, 25, 255],   // AccentReboot
    [255, 255, 255, 255], // White
    [240, 240, 210, 255], // Cream
    [130, 210, 255, 255], // Cyan
    [235, 242, 248, 255], // BodyText
    [35, 15, 20, 255],    // CloseBtnBg
    [185, 35, 40, 255],   // ConfirmYesBg
    [45, 95, 135, 255],   // ConfirmNoBg
    [55, 68, 88, 255],    // SliderTrackBg
    [65, 185, 250, 255],  // SliderTrackFill
    [235, 245, 255, 255], // SliderThumbBg
];

impl Color {
    #[inline]
    pub(crate) const fn rgba(self) -> [u8; 4] {
        PALETTE[self as usize]
    }
}

/// Foreground/background `(fg, bg)` pairs materialized as columns in the GPU `DebugFont` atlas
/// (`vkCmdBlitImage` copies 5x5 glyph cells without blending, so each `(fg, bg)` pair occupies
/// one 7px-wide column in the compact atlas).
const TEXT_STYLES: [(Color, Color); 16] = [
    (Color::White, Color::AccentIdle),
    (Color::Cream, Color::AccentIdle),
    (Color::White, Color::AccentOrange),
    (Color::Cream, Color::AccentOrange),
    (Color::White, Color::AccentRed),
    (Color::Cream, Color::AccentRed),
    (Color::White, Color::AccentReboot),
    (Color::Cream, Color::AccentReboot),
    (Color::Cyan, Color::BodyBg),
    (Color::BodyText, Color::BodyBg),
    (Color::White, Color::BodyBg),
    (Color::White, Color::CloseBtnBg),
    (Color::White, Color::ConfirmYesBg),
    (Color::White, Color::ConfirmNoBg),
    (Color::Cyan, Color::BottomBarBg),
    (Color::White, Color::BottomBarBg),
];

const SWATCH_COLUMN_INDEX: u32 = TEXT_STYLES.len() as u32;
const TOTAL_ATLAS_COLUMNS: u32 = SWATCH_COLUMN_INDEX + 1;

/// Total width in pixels of the GPU atlas image (`17 * 7 = 119` px).
pub(crate) const ATLAS_WIDTH: u32 = TOTAL_ATLAS_COLUMNS * GLYPH_WIDTH;

/// Total height in pixels of the GPU atlas image (`96 * 7 = 672` px).
pub(crate) const ATLAS_HEIGHT: u32 = ATLAS_GLYPH_COUNT * GLYPH_HEIGHT;

#[inline]
const fn text_style_column(fg: Color, bg: Color) -> u32 {
    let mut i = 0;
    while i < TEXT_STYLES.len() {
        if TEXT_STYLES[i].0 as usize == fg as usize && TEXT_STYLES[i].1 as usize == bg as usize {
            return i as u32;
        }
        i += 1;
    }
    panic!("(fg, bg) color pair must be registered in TEXT_STYLES");
}

/// Maps an ASCII byte to its row index (`0..ATLAS_GLYPH_COUNT`) in the compact GPU atlas.
#[inline]
const fn atlas_row_for_byte(byte: u8) -> u32 {
    let b = byte as u32;
    if b >= FIRST_ATLAS_GLYPH && b < FIRST_ATLAS_GLYPH + (ATLAS_GLYPH_COUNT - 1) {
        b - FIRST_ATLAS_GLYPH
    } else {
        ATLAS_GLYPH_COUNT - 1
    }
}

/// 5x5 glyph bitmasks for printable ASCII (`32..=126`), followed by a solid fallback block at
/// row 95 (`0x1ffffff`).
///
/// Extends `escher::DebugFont::GetFontPixels()` (`//src/ui/lib/escher/debug/debug_font.cc`),
/// which omits `%`, `&`, `+`, `G`, `J`, `K`, `Q`, `Z`, and `a`..=`z`, and uses `0x1ffffff`
/// placeholders for `'`, `,`, `/`, `?`, `@`, `[`, `]`, `_`, and `|`.
/// Code points still using the solid fallback block (`0x1ffffff`): `$`, `\`, `^`, `` ` ``, `{`,
/// `}`, and `~`.
const ATLAS_GLYPH_BITS: [u32; ATLAS_GLYPH_COUNT as usize] = [
    0x0000000, 0x0421004, 0x0a50000, 0x0afabea, 0x1ffffff, 0x19d1173, 0x0c9364d,
    0x0420000, // 32..=39: ' '..='\''
    0x0221082, 0x0821088, 0x15711d5, 0x0023880, 0x0000088, 0x0007c00, 0x0000004,
    0x0111110, // 40..=47: '('..='/'
    0x1f8c63f, 0x0421084, 0x3f0fe1f, 0x1f09c3f, 0x118fc21, 0x1f87c3f, 0x1f87e3f,
    0x1f08421, // 48..=55: '0'..='7'
    0x1f8fe3f, 0x1f8fc3f, 0x0020080, 0x002008c, 0x0222082, 0x00701c0, 0x0820888,
    0x0e89804, // 56..=63: '8'..='?'
    0x0ebd60e, 0x0457e31, 0x1e8fa3e, 0x1f8421f, 0x1e8c63e, 0x1f87a1f, 0x1f87a10,
    0x0f85e2f, // 64..=71: '@'..='G'
    0x118fe31, 0x1f2109f, 0x0710a4c, 0x1197251, 0x108421f, 0x0add6b1, 0x19cd673,
    0x0e8c62e, // 72..=79: 'H'..='O'
    0x1e8fa10, 0x0e8d64d, 0x1e8fa93, 0x0f8383e, 0x1f21084, 0x118c62e, 0x118a944,
    0x11ad76a, // 80..=87: 'P'..='W'
    0x1151151, 0x1151084, 0x1f1111f, 0x0e4210e, 0x1ffffff, 0x0e1084e, 0x1ffffff,
    0x000001f, // 88..=95: 'X'..='_'
    0x1ffffff, 0x0071e2f, 0x10f463e, 0x007c20f, 0x017c62f, 0x0077e0f, 0x0747908,
    0x0f8bc2e, // 96..=103: '`'..='g'
    0x10f4631, 0x040308e, 0x0200a4c, 0x1097251, 0x0c2108e, 0x00d56b1, 0x00f4631,
    0x007462e, // 104..=111: 'h'..='o'
    0x00f47d0, 0x007c5e1, 0x00be210, 0x007b83e, 0x08f2107, 0x008c62f, 0x008c544,
    0x008d76a, // 112..=119: 'p'..='w'
    0x008b9d1, 0x008bc2e, 0x00f991f, 0x1ffffff, 0x0421084, 0x1ffffff, 0x1ffffff,
    0x1ffffff, // 120..=127: 'x'..='~' + fallback (row 95)
];

/// Builds a `vk::ImageMemoryBarrier` for `COLOR_SUBRESOURCE_RANGE`.
pub(crate) fn image_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access_mask: vk::AccessFlags,
    dst_access_mask: vk::AccessFlags,
    src_queue_family_index: u32,
    dst_queue_family_index: u32,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access_mask)
        .dst_access_mask(dst_access_mask)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(src_queue_family_index)
        .dst_queue_family_index(dst_queue_family_index)
        .image(image)
        .subresource_range(COLOR_SUBRESOURCE_RANGE)
}

struct StagingBuffer {
    vulkan: Rc<VulkanDevice>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        if self.vulkan.is_abandoned() {
            return;
        }
        let device = self.vulkan.device();
        // SAFETY: `StagingBuffer` is dropped only after `VulkanDevice::run_one_shot()` waits for
        // the upload fence (or fails prior to submission); if the wait timed out,
        // `self.vulkan.is_abandoned()` is `true` and destruction is skipped.
        unsafe {
            if self.buffer != vk::Buffer::null() {
                device.destroy_buffer(self.buffer, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                device.free_memory(self.memory, None);
            }
        }
    }
}

/// Rust port of `escher::DebugFont` (`//src/ui/lib/escher/debug/debug_font.h`).
///
/// Holds a device-local `vk::Image` in `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL` containing the
/// monospace 7x7 bitmap font atlas and solid-color swatches, and blits glyphs and rectangles into
/// a destination `vk::Image` in `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL` via `vkCmdBlitImage()`.
///
/// # Drop Order Contract
///
/// `Rc<VulkanDevice>` orders only logical device teardown. Any [`crate::swapchain::Swapchain`]
/// whose recorded command buffers reference this [`DebugFont`] atlas image must be dropped (waiting
/// for its in-flight submissions) *before* [`DebugFont`] drops.
pub(crate) struct DebugFont {
    vulkan: Rc<VulkanDevice>,
    image: vk::Image,
    device_memory: vk::DeviceMemory,
}

impl Drop for DebugFont {
    fn drop(&mut self) {
        if self.vulkan.is_abandoned() {
            return;
        }
        let device = self.vulkan.device();
        // SAFETY: Any `Swapchain` referencing `self.image` drops before `DebugFont`, waiting for
        // all in-flight command buffers to complete (or marking `self.vulkan` abandoned) before
        // `self` drops.
        unsafe {
            if self.image != vk::Image::null() {
                device.destroy_image(self.image, None);
            }
            if self.device_memory != vk::DeviceMemory::null() {
                device.free_memory(self.device_memory, None);
            }
        }
    }
}

impl DebugFont {
    /// Generates the compact opaque RGBA atlas (`ATLAS_WIDTH x ATLAS_HEIGHT` = `119 x 672` pixels)
    /// containing all `TEXT_STYLES` `(fg, bg)` columns (rows `0..95` for printable ASCII `32..=126`,
    /// plus row `95` as the non-printable fallback block) and the `PALETTE` swatch column.
    pub(crate) fn atlas_pixels() -> Vec<u8> {
        let atlas_w = ATLAS_WIDTH as usize;
        let atlas_h = ATLAS_HEIGHT as usize;
        let mut output = [0_u8, 0, 0, 255].repeat(atlas_w * atlas_h);

        let fill_cell = |buf: &mut [u8], col_x: usize, row_y: usize, color: [u8; 4]| {
            for y in 0..GLYPH_HEIGHT as usize {
                for x in 0..GLYPH_WIDTH as usize {
                    let px_idx = ((row_y + y) * atlas_w + (col_x + x)) * 4;
                    buf[px_idx..px_idx + 4].copy_from_slice(&color);
                }
            }
        };

        for (col_idx, &(fg_color, bg_color)) in TEXT_STYLES.iter().enumerate() {
            let col_x = col_idx * (GLYPH_WIDTH as usize);
            let fg = fg_color.rgba();
            let bg = bg_color.rgba();
            for (atlas_row, &bits) in ATLAS_GLYPH_BITS.iter().enumerate() {
                let row_y = atlas_row * (GLYPH_HEIGHT as usize);
                fill_cell(&mut output, col_x, row_y, bg);
                for x in 0..INNER_WIDTH as usize {
                    for y in 0..INNER_HEIGHT as usize {
                        let shift = MAX_GLYPH_SHIFT as usize - (y * (INNER_WIDTH as usize) + x);
                        let color = if ((bits >> shift) & 1) != 0 { fg } else { bg };
                        let gx = col_x + x + (GLYPH_PADDING as usize);
                        let gy = row_y + y + (GLYPH_PADDING as usize);
                        let px_idx = (gy * atlas_w + gx) * 4;
                        output[px_idx..px_idx + 4].copy_from_slice(&color);
                    }
                }
            }
        }

        let swatch_col_x = (SWATCH_COLUMN_INDEX as usize) * (GLYPH_WIDTH as usize);
        for (swatch_idx, &color) in PALETTE.iter().enumerate() {
            fill_cell(&mut output, swatch_col_x, swatch_idx * (GLYPH_HEIGHT as usize), color);
        }

        output
    }

    /// Creates and uploads the `DebugFont` GPU image (`VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`),
    /// matching `escher::DebugFont::New()` in `debug_font.cc`.
    ///
    /// `vulkan` must not be abandoned and must support [`ATLAS_FORMAT`] with
    /// [`ATLAS_FORMAT_FEATURES`].
    pub(crate) fn new(vulkan: Rc<VulkanDevice>) -> Result<Self, Error> {
        ensure!(!vulkan.is_abandoned(), "Vulkan device is abandoned");
        ftrace::duration!(c"gfx", c"DebugFont::new");
        let pixels = Self::atlas_pixels();
        let buffer_size = pixels.len() as vk::DeviceSize;
        let device = vulkan.device();
        let mem_props = vulkan.physical_device_memory_properties();

        let buffer_info = vk::BufferCreateInfo::default()
            .size(buffer_size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: `device` is a live `ash::Device` and `buffer_info` has non-zero `size` and no
        // `p_next` chain.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }
            .map_err(|e| anyhow!("create staging buffer failed: {e:?}"))?;
        let mut staging =
            StagingBuffer { vulkan: Rc::clone(&vulkan), buffer, memory: vk::DeviceMemory::null() };

        // SAFETY: `staging.buffer` was just created on `device` and is still alive.
        let buf_reqs = unsafe { device.get_buffer_memory_requirements(staging.buffer) };
        let staging_mem_type = find_preferred_memory_type(
            &mem_props,
            buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
        )
        .ok_or_else(|| anyhow!("no host-visible memory type for DebugFont staging buffer"))?;

        let staging_alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(staging_mem_type);
        // SAFETY: `staging_alloc_info` uses a valid memory type index from `mem_props` and the
        // non-zero `buf_reqs.size` returned by `device`.
        staging.memory = unsafe { device.allocate_memory(&staging_alloc_info, None) }
            .map_err(|e| anyhow!("allocate staging memory failed: {e:?}"))?;

        // SAFETY: `staging.buffer` and `staging.memory` are valid, unbound handles on `device`,
        // and `staging.memory` was allocated with `buf_reqs.size >= pixels.len()`.
        unsafe {
            device
                .bind_buffer_memory(staging.buffer, staging.memory, 0)
                .map_err(|e| anyhow!("bind staging buffer memory failed: {e:?}"))?;
        }
        // SAFETY: `staging.memory` is host-visible, not currently mapped, and `buf_reqs.size >=
        // pixels.len()`. Mapping `vk::WHOLE_SIZE` ensures the full `buf_reqs.size` allocation is
        // mapped when flushing `vk::WHOLE_SIZE` below (satisfying `nonCoherentAtomSize` alignment).
        let mapped = unsafe {
            device
                .map_memory(staging.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(|e| anyhow!("map staging memory failed: {e:?}"))? as *mut u8
        };
        // SAFETY: `mapped` points to a valid host-mapped region of at least `pixels.len()` bytes
        // that does not overlap `pixels`.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped, pixels.len());
        }
        let flush_range = [vk::MappedMemoryRange::default()
            .memory(staging.memory)
            .offset(0)
            .size(vk::WHOLE_SIZE)];
        // SAFETY: `staging.memory` is currently mapped for the full range, and we unmap it
        // immediately after flushing.
        let flush_res = unsafe {
            let res = device
                .flush_mapped_memory_ranges(&flush_range)
                .map_err(|e| anyhow!("flush staging memory failed: {e:?}"));
            device.unmap_memory(staging.memory);
            res
        };
        flush_res?;

        let image_create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(ATLAS_FORMAT)
            .extent(vk::Extent3D { width: ATLAS_WIDTH, height: ATLAS_HEIGHT, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `device` is a live `ash::Device` and `image_create_info` has valid non-zero
        // extent and no `p_next` chain.
        let image = unsafe { device.create_image(&image_create_info, None) }
            .map_err(|e| anyhow!("create DebugFont image failed: {e:?}"))?;
        let mut font = DebugFont {
            vulkan: Rc::clone(&vulkan),
            image,
            device_memory: vk::DeviceMemory::null(),
        };

        // SAFETY: `font.image` was just created on `device` and is still alive.
        let img_reqs = unsafe { device.get_image_memory_requirements(font.image) };
        let img_mem_type = find_preferred_memory_type(
            &mem_props,
            img_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        )
        .ok_or_else(|| anyhow!("no suitable memory type for DebugFont image"))?;

        let img_alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(img_reqs.size)
            .memory_type_index(img_mem_type);
        // SAFETY: `img_alloc_info` uses a valid memory type index from `mem_props` and
        // `img_reqs.size` returned by `device`.
        font.device_memory = unsafe { device.allocate_memory(&img_alloc_info, None) }
            .map_err(|e| anyhow!("allocate DebugFont image memory failed: {e:?}"))?;

        // SAFETY: `font.image` and `font.device_memory` are valid, unbound handles on `device`
        // with matching memory requirements.
        unsafe {
            device
                .bind_image_memory(font.image, font.device_memory, 0)
                .map_err(|e| anyhow!("bind DebugFont image memory failed: {e:?}"))?;
        }

        // Construct `DebugFont` before submitting the staging copy so an upload fence timeout
        // leaves both `staging` and `font` handles intact while `vulkan` is marked abandoned.
        vulkan.run_one_shot(|device, cb| {
            let barrier_to_dst = image_barrier(
                font.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );
            let copy_region = vk::BufferImageCopy::default()
                .image_subresource(COLOR_SUBRESOURCE_LAYERS)
                .image_extent(vk::Extent3D { width: ATLAS_WIDTH, height: ATLAS_HEIGHT, depth: 1 });
            let barrier_to_src = image_barrier(
                font.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            );

            // SAFETY: `cb` is recording a one-time-submit primary command buffer on `device`,
            // `staging.buffer` and `font.image` are valid and bound with size >=
            // `ATLAS_WIDTH * ATLAS_HEIGHT * 4`, and `VulkanDevice::run_one_shot()` submits and
            // waits on `cb` before returning.
            unsafe {
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier_to_dst],
                );
                device.cmd_copy_buffer_to_image(
                    cb,
                    staging.buffer,
                    font.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy_region],
                );
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier_to_src],
                );
            }
            Ok(())
        })?;

        Ok(font)
    }

    #[inline]
    pub(crate) fn vulkan_device_handle(&self) -> vk::Device {
        self.vulkan.device().handle()
    }

    /// Issues a single `vkCmdBlitImage()` (`VK_FILTER_NEAREST`) for all accumulated `regions`.
    ///
    /// # Safety
    ///
    /// `cb` must be a primary command buffer in the recording state on `self.vulkan.device()`,
    /// `output_image` must belong to `self.vulkan.device()` and be in
    /// `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`, and every destination rectangle in `regions` must
    /// lie within `output_image`'s bounds.
    pub(crate) unsafe fn submit_blit_regions(
        &self,
        cb: vk::CommandBuffer,
        output_image: vk::Image,
        regions: &[vk::ImageBlit],
    ) {
        if !regions.is_empty() {
            // SAFETY: Caller guarantees `cb` is recording on `self.vulkan.device()`,
            // `output_image` is in `TRANSFER_DST_OPTIMAL` with `regions` in bounds, and
            // `self.image` is a valid `TRANSFER_SRC_OPTIMAL` atlas image on `self.vulkan`.
            unsafe {
                self.vulkan.device().cmd_blit_image(
                    cb,
                    self.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    output_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    regions,
                    vk::Filter::NEAREST,
                );
            }
        }
    }

    /// Appends `vk::ImageBlit` regions for `text` at `target_offset` scaled by `(scale_x, scale_y)`
    /// using the `(fg, bg)` column in the `DebugFont` atlas.
    ///
    /// Because the underlying header, body, button, or bottom-bar surface is already filled with
    /// `bg`:
    /// 1. Space characters (`' '`) only advance the cursor without emitting a `vk::ImageBlit`.
    /// 2. Only the `5x5` inner glyph bounds (`INNER_WIDTH x INNER_HEIGHT`) are blitted, leaving the
    ///    1px outer background padding ring untouched.
    ///
    /// Every `(fg, bg)` pair passed to this function must be registered in `TEXT_STYLES`.
    pub(crate) fn append_text_blit_regions(
        regions: &mut Vec<vk::ImageBlit>,
        text: &str,
        target_offset: vk::Offset2D,
        scale_x: i32,
        scale_y: i32,
        fg: Color,
        bg: Color,
    ) {
        if scale_x <= 0 || scale_y <= 0 {
            return;
        }
        let col = text_style_column(fg, bg) as i32;
        let glyph_w = GLYPH_WIDTH as i32;
        let glyph_h = GLYPH_HEIGHT as i32;
        let pad = GLYPH_PADDING as i32;
        let src_left = col * glyph_w + pad;
        let src_right = (col + 1) * glyph_w - pad;
        let dst_top = target_offset.y + pad * scale_y;
        let dst_bottom = target_offset.y + (glyph_h - pad) * scale_y;

        for (char_idx, byte) in text.bytes().enumerate() {
            if byte == b' ' {
                continue;
            }
            let atlas_row = atlas_row_for_byte(byte) as i32;
            let src_top = atlas_row * glyph_h + pad;
            let src_bottom = (atlas_row + 1) * glyph_h - pad;
            let cell_left = target_offset.x + (char_idx as i32) * glyph_w * scale_x;
            let dst_left = cell_left + pad * scale_x;
            let dst_right = cell_left + (glyph_w - pad) * scale_x;

            regions.push(vk::ImageBlit {
                src_subresource: COLOR_SUBRESOURCE_LAYERS,
                src_offsets: [
                    vk::Offset3D { x: src_left, y: src_top, z: 0 },
                    vk::Offset3D { x: src_right, y: src_bottom, z: 1 },
                ],
                dst_subresource: COLOR_SUBRESOURCE_LAYERS,
                dst_offsets: [
                    vk::Offset3D { x: dst_left, y: dst_top, z: 0 },
                    vk::Offset3D { x: dst_right, y: dst_bottom, z: 1 },
                ],
            });
        }
    }

    /// Appends a `vk::ImageBlit` region that fills `rect` with the solid `color` swatch from the
    /// `DebugFont` atlas.
    pub(crate) fn append_rect_blit(
        regions: &mut Vec<vk::ImageBlit>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: Color,
    ) {
        if w <= 0 || h <= 0 {
            return;
        }
        let src_left = (SWATCH_COLUMN_INDEX as i32) * (GLYPH_WIDTH as i32) + (GLYPH_PADDING as i32);
        let src_top = (color as i32) * (GLYPH_HEIGHT as i32) + (GLYPH_PADDING as i32);

        regions.push(vk::ImageBlit {
            src_subresource: COLOR_SUBRESOURCE_LAYERS,
            src_offsets: [
                vk::Offset3D { x: src_left, y: src_top, z: 0 },
                vk::Offset3D { x: src_left + 1, y: src_top + 1, z: 1 },
            ],
            dst_subresource: COLOR_SUBRESOURCE_LAYERS,
            dst_offsets: [vk::Offset3D { x, y, z: 0 }, vk::Offset3D { x: x + w, y: y + h, z: 1 }],
        });
    }

    /// Appends 4 `vk::ImageBlit` regions to draw a 2px rectangular border around `(x, y, w, h)`.
    pub(crate) fn append_border_blits(
        regions: &mut Vec<vk::ImageBlit>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        border_color: Color,
    ) {
        Self::append_rect_blit(regions, x, y, w, 2, border_color);
        Self::append_rect_blit(regions, x, (y + h - 2).max(y), w, 2, border_color);
        Self::append_rect_blit(regions, x, y, 2, h, border_color);
        Self::append_rect_blit(regions, (x + w - 2).max(x), y, 2, h, border_color);
    }

    /// Appends 5 `vk::ImageBlit` regions to draw a filled rectangle with a 2px border.
    pub(crate) fn append_bordered_box_blits(
        regions: &mut Vec<vk::ImageBlit>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bg_color: Color,
        border_color: Color,
    ) {
        Self::append_rect_blit(regions, x, y, w, h, bg_color);
        Self::append_border_blits(regions, x, y, w, h, border_color);
    }

    /// Appends a bordered button with centered `2x2`-scaled `label` text.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_button_blits(
        regions: &mut Vec<vk::ImageBlit>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bg_color: Color,
        border_color: Color,
        fg_color: Color,
        label: &str,
    ) {
        Self::append_bordered_box_blits(regions, x, y, w, h, bg_color, border_color);
        let text_w = (label.len() as i32) * (GLYPH_WIDTH as i32) * 2;
        let text_h = (GLYPH_HEIGHT as i32) * 2;
        let text_x = x + (w - text_w) / 2;
        let text_y = y + (h - text_h) / 2;
        Self::append_text_blit_regions(
            regions,
            label,
            vk::Offset2D { x: text_x, y: text_y },
            2,
            2,
            fg_color,
            bg_color,
        );
    }
}

fn find_preferred_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    preferred_flags: vk::MemoryPropertyFlags,
    fallback_flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    find_memory_type_index(mem_props, type_bits, preferred_flags)
        .or_else(|| find_memory_type_index(mem_props, type_bits, fallback_flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas_pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * ATLAS_WIDTH + x) * 4) as usize;
        pixels[idx..idx + 4].try_into().expect("4 RGBA bytes")
    }

    #[fuchsia::test]
    fn test_atlas_row_for_byte_boundaries() {
        assert_eq!(atlas_row_for_byte(31), 95);
        assert_eq!(atlas_row_for_byte(32), 0);
        assert_eq!(atlas_row_for_byte(126), 94);
        assert_eq!(atlas_row_for_byte(127), 95);
        assert_eq!(atlas_row_for_byte(255), 95);
    }

    #[fuchsia::test]
    fn test_atlas_pixels_glyphs_swatches_and_golden() {
        let pixels = DebugFont::atlas_pixels();
        assert_eq!(pixels.len(), (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize);

        // Style column 0 is `(Color::White, Color::AccentIdle)`, and `'A'` (ASCII 65) is row 33.
        let row_a = atlas_row_for_byte(b'A');
        assert_eq!(row_a, 33);
        let row_a_top = row_a * GLYPH_HEIGHT;
        assert_eq!(atlas_pixel(&pixels, 3, row_a_top + 1), Color::White.rgba());
        assert_eq!(atlas_pixel(&pixels, 1, row_a_top + 1), Color::AccentIdle.rgba());
        for dy in 0..GLYPH_HEIGHT {
            for dx in 0..GLYPH_WIDTH {
                if dx == 0 || dx == GLYPH_WIDTH - 1 || dy == 0 || dy == GLYPH_HEIGHT - 1 {
                    assert_eq!(
                        atlas_pixel(&pixels, dx, row_a_top + dy),
                        Color::AccentIdle.rgba(),
                        "padding ring at ({dx}, {dy}) for 'A'"
                    );
                }
            }
        }

        // Verify every palette swatch texel in `SWATCH_COLUMN_INDEX`.
        let all_colors = [
            Color::BodyBg,
            Color::BottomBarBg,
            Color::AccentIdle,
            Color::AccentOrange,
            Color::AccentRed,
            Color::AccentReboot,
            Color::White,
            Color::Cream,
            Color::Cyan,
            Color::BodyText,
            Color::CloseBtnBg,
            Color::ConfirmYesBg,
            Color::ConfirmNoBg,
            Color::SliderTrackBg,
            Color::SliderTrackFill,
            Color::SliderThumbBg,
        ];
        for color in all_colors {
            let sx = SWATCH_COLUMN_INDEX * GLYPH_WIDTH + 1;
            let sy = (color as u32) * GLYPH_HEIGHT + 1;
            assert_eq!(atlas_pixel(&pixels, sx, sy), color.rgba(), "swatch for {color:?}");
        }

        // Golden 5x5 bitmap check for newly added lowercase `'a'` (ASCII 97 -> row 65) in column 0.
        let row_lower_a = atlas_row_for_byte(b'a');
        assert_eq!(row_lower_a, 65);
        let expected_lower_a = [".....", ".###.", "..###", "#...#", ".####"];
        for (gy, expected_row) in expected_lower_a.iter().enumerate() {
            let mut actual_row = String::with_capacity(INNER_WIDTH as usize);
            for gx in 0..INNER_WIDTH {
                let px = atlas_pixel(
                    &pixels,
                    GLYPH_PADDING + gx,
                    row_lower_a * GLYPH_HEIGHT + GLYPH_PADDING + (gy as u32),
                );
                actual_row.push(if px == Color::White.rgba() { '#' } else { '.' });
            }
            assert_eq!(actual_row, *expected_row, "golden row {gy} for 'a'");
        }
    }

    #[fuchsia::test]
    fn test_find_memory_type_and_image_barrier() {
        let mut memory_types = [vk::MemoryType::default(); vk::MAX_MEMORY_TYPES];
        memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        memory_types[1].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
        memory_types[2].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let mem_props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            memory_types,
            ..Default::default()
        };

        assert_eq!(
            find_memory_type_index(
                &mem_props,
                0b111,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            ),
            Some(2)
        );
        // Bit 2 excluded -> returns None for coherent, falls back to index 1 for host-visible.
        assert_eq!(
            find_memory_type_index(
                &mem_props,
                0b011,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            ),
            None
        );
        assert_eq!(
            find_preferred_memory_type(
                &mem_props,
                0b011,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::HOST_VISIBLE
            ),
            Some(1)
        );
        assert_eq!(
            find_preferred_memory_type(
                &mem_props,
                0b111,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::HOST_VISIBLE
            ),
            Some(2)
        );

        use ash::vk::Handle as _;
        let img = vk::Image::from_raw(42);
        let barrier = image_barrier(
            img,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
            1,
            2,
        );
        assert_eq!(barrier.image, img);
        assert!(barrier.old_layout == vk::ImageLayout::UNDEFINED);
        assert!(barrier.new_layout == vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert!(barrier.src_access_mask == vk::AccessFlags::empty());
        assert!(barrier.dst_access_mask == vk::AccessFlags::TRANSFER_WRITE);
        assert_eq!(barrier.src_queue_family_index, 1);
        assert_eq!(barrier.dst_queue_family_index, 2);
        assert!(barrier.subresource_range.aspect_mask == COLOR_SUBRESOURCE_RANGE.aspect_mask);
        assert_eq!(
            barrier.subresource_range.base_mip_level,
            COLOR_SUBRESOURCE_RANGE.base_mip_level
        );
        assert_eq!(barrier.subresource_range.level_count, COLOR_SUBRESOURCE_RANGE.level_count);
        assert_eq!(
            barrier.subresource_range.base_array_layer,
            COLOR_SUBRESOURCE_RANGE.base_array_layer
        );
        assert_eq!(barrier.subresource_range.layer_count, COLOR_SUBRESOURCE_RANGE.layer_count);
    }
}
