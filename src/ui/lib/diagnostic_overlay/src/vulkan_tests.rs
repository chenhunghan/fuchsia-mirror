// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// `diagnostic_overlay_vulkan_test` compiles these submodules under a standalone crate root
// (`src/vulkan_tests.rs`) separate from `diagnostic_overlay_test` (`src/lib.rs`) so GPU validation
// tests run inside `vulkan_envs` without pulling `Flatland` presenter mocks into the Vulkan test
// binary. Submodule `#[cfg(test)]` helpers called only from this binary (such as
// `VulkanDevice::new_with_validation()` and `VulkanDevice::mark_abandoned_for_testing()`) carry
// `#[allow(dead_code)]` for `src/lib.rs`.
mod debug_font;
mod overlay;
mod renderer;
mod swapchain;
mod vulkan;

use anyhow::{Error, anyhow};
use ash::vk;
use debug_font::{
    ATLAS_FORMAT, ATLAS_FORMAT_FEATURES, COLOR_SUBRESOURCE_RANGE, Color, DebugFont, image_barrier,
};
use fidl::endpoints::{ClientEnd, create_endpoints};
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_sysmem2 as fsysmem2;
use fuchsia_component::client::connect_to_protocol_sync;
use overlay::{OverlayDisplayMode, default_hud_lines};
use renderer::{HudRenderer, OVERLAY_BUFFER_HEIGHT, OVERLAY_BUFFER_WIDTH, OverlayRenderParams};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use swapchain::{
    AcquireOutcome, AcquiredSlot, SWAPCHAIN_FORMAT, SWAPCHAIN_FORMAT_FEATURES, Swapchain,
};
use vulkan::{FENCE_WAIT_TIMEOUT_NS, RenderContext, VulkanDevice, find_memory_type_index};

fn allocate_synced_sysmem_token(
    sysmem_allocator: &fsysmem2::AllocatorSynchronousProxy,
) -> ClientEnd<fsysmem2::BufferCollectionTokenMarker> {
    let (token_client, token_server) = create_endpoints::<fsysmem2::BufferCollectionTokenMarker>();
    sysmem_allocator
        .allocate_shared_collection(fsysmem2::AllocatorAllocateSharedCollectionRequest {
            token_request: Some(token_server),
            ..Default::default()
        })
        .expect("allocate_shared_collection");
    // Synchronize the token channel so sysmem registers `AllocateSharedCollection` before the
    // Vulkan ICD binds the token on its own `fuchsia.sysmem2.Allocator` channel.
    let token_sync = token_client.into_sync_proxy();
    token_sync.sync(zx::MonotonicInstant::INFINITE).expect("sync sysmem token");
    ClientEnd::new(token_sync.into_channel())
}

struct ReadbackPixels {
    bytes: Vec<u8>,
    width: u32,
}

impl ReadbackPixels {
    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * self.width + x) * 4) as usize;
        [self.bytes[idx], self.bytes[idx + 1], self.bytes[idx + 2], self.bytes[idx + 3]]
    }
}

/// RAII offscreen `TRANSFER_DST | TRANSFER_SRC` target image and host-visible readback buffer
/// ensuring child Vulkan handles are destroyed before `VulkanDevice::drop()` even if a test
/// assertion panics.
struct ReadbackTarget {
    vulkan: Rc<VulkanDevice>,
    dst_image: vk::Image,
    dst_memory: vk::DeviceMemory,
    readback_buf: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    width: u32,
    height: u32,
    readback_size: vk::DeviceSize,
}

impl Drop for ReadbackTarget {
    fn drop(&mut self) {
        if self.vulkan.is_abandoned() {
            return;
        }
        let device = self.vulkan.device();
        // SAFETY: All GPU work using `self` executes synchronously via
        // `VulkanDevice::run_one_shot()`, which waits for completion (or marks `self.vulkan`
        // abandoned) before returning.
        unsafe {
            if self.readback_buf != vk::Buffer::null() {
                device.destroy_buffer(self.readback_buf, None);
            }
            if self.readback_memory != vk::DeviceMemory::null() {
                device.free_memory(self.readback_memory, None);
            }
            if self.dst_image != vk::Image::null() {
                device.destroy_image(self.dst_image, None);
            }
            if self.dst_memory != vk::DeviceMemory::null() {
                device.free_memory(self.dst_memory, None);
            }
        }
    }
}

impl ReadbackTarget {
    fn new(vulkan: Rc<VulkanDevice>, width: u32, height: u32) -> Self {
        let readback_size = (width * height * 4) as vk::DeviceSize;
        let mut target = Self {
            vulkan,
            dst_image: vk::Image::null(),
            dst_memory: vk::DeviceMemory::null(),
            readback_buf: vk::Buffer::null(),
            readback_memory: vk::DeviceMemory::null(),
            width,
            height,
            readback_size,
        };

        let mem_props = target.vulkan.physical_device_memory_properties();
        let device = target.vulkan.device();

        let dst_image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(SWAPCHAIN_FORMAT)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: `device` is live and `dst_image_info` is valid.
        target.dst_image = unsafe { device.create_image(&dst_image_info, None) }
            .expect("create destination readback image");
        // SAFETY: `target.dst_image` is alive on `device`.
        let dst_reqs = unsafe { device.get_image_memory_requirements(target.dst_image) };
        let dst_mem_type = find_memory_type_index(
            &mem_props,
            dst_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            find_memory_type_index(
                &mem_props,
                dst_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        })
        .expect("destination readback image memory type");
        let dst_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(dst_reqs.size)
            .memory_type_index(dst_mem_type);
        // SAFETY: `dst_alloc` is valid on `device`.
        target.dst_memory =
            unsafe { device.allocate_memory(&dst_alloc, None) }.expect("alloc destination memory");
        // SAFETY: `target.dst_image` and `target.dst_memory` are unbound handles on `device`.
        unsafe { device.bind_image_memory(target.dst_image, target.dst_memory, 0) }
            .expect("bind destination image memory");

        let readback_info = vk::BufferCreateInfo::default()
            .size(readback_size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: `device` is live and `readback_info` is valid.
        target.readback_buf =
            unsafe { device.create_buffer(&readback_info, None) }.expect("create readback buffer");
        // SAFETY: `target.readback_buf` is alive on `device`.
        let readback_reqs = unsafe { device.get_buffer_memory_requirements(target.readback_buf) };
        let readback_mem_type = find_memory_type_index(
            &mem_props,
            readback_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .expect("host-visible readback memory type");
        let readback_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(readback_reqs.size)
            .memory_type_index(readback_mem_type);
        // SAFETY: `readback_alloc` is valid on `device`.
        target.readback_memory = unsafe { device.allocate_memory(&readback_alloc, None) }
            .expect("alloc readback memory");
        // SAFETY: `target.readback_buf` and `target.readback_memory` are unbound handles on
        // `device`.
        unsafe { device.bind_buffer_memory(target.readback_buf, target.readback_memory, 0) }
            .expect("bind readback buffer");

        target
    }

    fn image(&self) -> vk::Image {
        self.dst_image
    }

    fn extent(&self) -> vk::Extent2D {
        vk::Extent2D { width: self.width, height: self.height }
    }

    fn run_and_read_pixels(
        &self,
        post_record_layout: vk::ImageLayout,
        post_record_queue_family: u32,
        record: impl FnOnce(&ash::Device, vk::CommandBuffer) -> Result<(), Error>,
    ) -> ReadbackPixels {
        let dst_queue_family = if post_record_queue_family == vk::QUEUE_FAMILY_IGNORED {
            vk::QUEUE_FAMILY_IGNORED
        } else {
            self.vulkan.queue_family_index()
        };
        let to_transfer_src = image_barrier(
            self.dst_image,
            post_record_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
            post_record_queue_family,
            dst_queue_family,
        );
        let copy_to_host = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D { width: self.width, height: self.height, depth: 1 });

        self.vulkan
            .run_one_shot(|dev, cb| {
                record(dev, cb)?;
                // SAFETY: `cb` is recording on `dev`, `record` left `self.dst_image` in
                // `post_record_layout` on `post_record_queue_family`, and `self.readback_buf` has
                // capacity `self.readback_size`.
                unsafe {
                    dev.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_transfer_src],
                    );
                    dev.cmd_copy_image_to_buffer(
                        cb,
                        self.dst_image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        self.readback_buf,
                        &[copy_to_host],
                    );
                }
                Ok(())
            })
            .expect("submit one-shot render and readback copy");

        let device = self.vulkan.device();
        // SAFETY: `self.readback_memory` is host-visible/coherent and `run_one_shot()` waited for
        // the GPU copy fence before returning.
        let bytes = unsafe {
            let ptr = device
                .map_memory(self.readback_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .expect("map readback memory") as *const u8;
            let copied = std::slice::from_raw_parts(ptr, self.readback_size as usize).to_vec();
            device.unmap_memory(self.readback_memory);
            copied
        };
        ReadbackPixels { bytes, width: self.width }
    }
}

#[fuchsia::test]
fn test_debug_font_vulkan_upload_and_blit_with_validation() {
    let (vulkan, validation_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice with validation layers");
    let font =
        DebugFont::new(Rc::clone(&vulkan)).expect("DebugFont::new() with Vulkan validation layers");
    let target = ReadbackTarget::new(Rc::clone(&vulkan), 64, 32);

    let mut regions = Vec::new();
    DebugFont::append_rect_blit(&mut regions, 4, 4, 10, 8, Color::AccentOrange);
    DebugFont::append_text_blit_regions(
        &mut regions,
        "A",
        vk::Offset2D { x: 20, y: 8 },
        1,
        1,
        Color::White,
        Color::BodyBg,
    );

    let to_dst = image_barrier(
        target.image(),
        vk::ImageLayout::UNDEFINED,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_WRITE,
        vk::QUEUE_FAMILY_IGNORED,
        vk::QUEUE_FAMILY_IGNORED,
    );
    let clear_color =
        vk::ClearColorValue { float32: Color::BodyBg.rgba().map(|c| c as f32 / 255.0) };

    let frame = target.run_and_read_pixels(
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::QUEUE_FAMILY_IGNORED,
        |dev, cb| {
            // SAFETY: `cb` is recording on `dev`, `target.image()` is valid on `dev` with size
            // `64x32` containing `regions`, and `font` belongs to the same `VulkanDevice`.
            unsafe {
                dev.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                dev.cmd_clear_color_image(
                    cb,
                    target.image(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &clear_color,
                    &[COLOR_SUBRESOURCE_RANGE],
                );
                let clear_to_blit_barrier = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                dev.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[clear_to_blit_barrier],
                    &[],
                    &[],
                );
                font.submit_blit_regions(cb, target.image(), &regions);
            }
            Ok(())
        },
    );

    assert_eq!(frame.pixel(0, 0), Color::BodyBg.rgba());
    assert_eq!(frame.pixel(6, 6), Color::AccentOrange.rgba());
    // `'A'` at `(20, 8)`: inner glyph at `(21..26, 9..14)`, top apex at `(23, 9)` is `White`.
    assert_eq!(frame.pixel(23, 9), Color::White.rgba());
    assert_eq!(frame.pixel(21, 9), Color::BodyBg.rgba());

    drop(target);
    drop(font);
    drop(vulkan);

    assert_eq!(
        validation_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors"
    );
}

#[fuchsia::test]
fn test_vulkan_device_new_error_paths_and_abandon_teardown() {
    // 1. Production `VulkanDevice::new()` succeeds for `SWAPCHAIN_FORMAT` and fails cleanly
    // (running `cleanup_instance`) when requesting transfer features on `vk::Format::UNDEFINED`.
    const {
        assert!(SWAPCHAIN_FORMAT.as_raw() == ATLAS_FORMAT.as_raw());
    }
    let prod_device = VulkanDevice::new(
        c"diagnostic_overlay_vulkan_tests",
        SWAPCHAIN_FORMAT,
        SWAPCHAIN_FORMAT_FEATURES | ATLAS_FORMAT_FEATURES,
    )
    .expect("VulkanDevice::new() with SWAPCHAIN_FORMAT");
    drop(prod_device);

    assert!(
        VulkanDevice::new(
            c"diagnostic_overlay_vulkan_tests",
            vk::Format::UNDEFINED,
            SWAPCHAIN_FORMAT_FEATURES,
        )
        .is_err(),
        "VulkanDevice::new() with Format::UNDEFINED must fail physical device format check"
    );

    // 2. `run_one_shot()` recording callback returning `Err` cleans up the temporary fence, command
    // buffer, and command pool without submitting or logging validation errors.
    let (vulkan_err, validation_errors_err) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice with validation layers");
    assert!(
        vulkan_err.create_command_buffers(0).is_err(),
        "create_command_buffers(0) must reject zero count"
    );
    assert!(
        vulkan_err
            .run_one_shot(|_dev, _cb| Err::<(), _>(anyhow!("simulated one-shot recording failure")))
            .is_err()
    );
    drop(vulkan_err);
    assert_eq!(
        validation_errors_err.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors after failed run_one_shot() teardown"
    );

    // 3. `mark_abandoned_for_testing()` rejects `DebugFont::new()`, command buffer allocations, and
    // one-shot submissions, and skips Vulkan destruction without crashing or logging validation
    // errors.
    let (vulkan, validation_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice with validation layers");
    let font = DebugFont::new(Rc::clone(&vulkan)).expect("create DebugFont");
    assert!(!vulkan.is_abandoned());

    vulkan.mark_abandoned_for_testing();
    assert!(vulkan.is_abandoned());
    assert!(
        DebugFont::new(Rc::clone(&vulkan)).is_err(),
        "DebugFont::new() must reject an abandoned VulkanDevice"
    );
    assert!(
        vulkan.create_command_buffers(1).is_err(),
        "create_command_buffers() must reject allocations on an abandoned VulkanDevice"
    );
    assert!(
        vulkan.run_one_shot(|_, _| Ok(())).is_err(),
        "run_one_shot() must reject submissions on an abandoned VulkanDevice"
    );

    drop(font);
    drop(vulkan);
    assert_eq!(
        validation_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors when abandoned device skips teardown"
    );

    // 4. `wait_for_fences_with_timeout()` on an unsignaled `vk::Fence` returns `TIMEOUT`, marks the
    // device abandoned so subsequent `wait_for_fences()` calls return `ERROR_DEVICE_LOST`, and
    // dropping `VulkanDevice` leaves the undestroyed fence intact with zero validation errors.
    let (timeout_vk, timeout_validation_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice for fence timeout test");
    // SAFETY: `timeout_vk.device()` is live and `FenceCreateInfo::default()` has no `p_next`.
    let unsignaled_fence =
        unsafe { timeout_vk.device().create_fence(&vk::FenceCreateInfo::default(), None) }
            .expect("create unsignaled vk::Fence");
    assert_eq!(
        // SAFETY: `unsignaled_fence` was created on `timeout_vk.device()` and remains alive.
        unsafe { timeout_vk.wait_for_fences_with_timeout(&[unsignaled_fence], 0) },
        Err(vk::Result::TIMEOUT),
    );
    assert!(timeout_vk.is_abandoned());
    assert_eq!(
        // SAFETY: `unsignaled_fence` remains alive; `wait_for_fences()` short-circuits once
        // `timeout_vk` is marked abandoned.
        unsafe { timeout_vk.wait_for_fences(&[unsignaled_fence]) },
        Err(vk::Result::ERROR_DEVICE_LOST),
    );
    drop(timeout_vk);
    assert_eq!(
        timeout_validation_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors when fence timeout abandons device"
    );
}

#[fuchsia::test]
fn test_hud_renderer_draw_frame_gpu_readback_with_validation() {
    let (vulkan, validation_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice with validation layers");
    let mut renderer = HudRenderer::new(Rc::clone(&vulkan)).expect("create HudRenderer");
    let target =
        ReadbackTarget::new(Rc::clone(&vulkan), OVERLAY_BUFFER_WIDTH, OVERLAY_BUFFER_HEIGHT);

    let mut draw_and_readback = |params: &OverlayRenderParams<'_>| -> ReadbackPixels {
        let qfi = vulkan.queue_family_index();
        target.run_and_read_pixels(
            vk::ImageLayout::GENERAL,
            vk::QUEUE_FAMILY_FOREIGN_EXT,
            |dev, cb| {
                // SAFETY: `cb` is recording on `dev`, and `target.image()` is an idle
                // `TRANSFER_DST` image on `dev` with dimensions `target.extent()`.
                let ctx =
                    unsafe { RenderContext::new(dev, cb, target.image(), target.extent(), qfi) };
                renderer.draw_frame(ctx, params)
            },
        )
    };

    let hud_lines = default_hud_lines();
    let params_100 = OverlayRenderParams {
        display_size: fmath::SizeU { width: 480, height: 800 },
        custom_origin: None,
        mode: OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 },
        confirm_close: false,
        opacity: 1.0,
        hud_generation: 1,
        hud_lines: &hud_lines,
    };
    let frame_100 = draw_and_readback(&params_100);

    // Body background, header accent (`AccentOrange`), `[X]` close button foreground (`White`),
    // and body text glyph foreground (`Cyan` prefix and `BodyText` content).
    assert_eq!(frame_100.pixel(4, 220), Color::BodyBg.rgba());
    assert_eq!(frame_100.pixel(4, 4), Color::AccentOrange.rgba());
    assert_eq!(frame_100.pixel(434, 5), Color::White.rgba());
    let mut saw_white_header_glyph = false;
    for y in 6..20 {
        for x in 10..80 {
            if frame_100.pixel(x, y) == Color::White.rgba() {
                saw_white_header_glyph = true;
            }
        }
    }
    assert!(saw_white_header_glyph, "expected White header title glyph pixels");

    let mut saw_body_glyph = false;
    for y in 54..68 {
        for x in 12..200 {
            let px = frame_100.pixel(x, y);
            if px == Color::Cyan.rgba() || px == Color::BodyText.rgba() {
                saw_body_glyph = true;
            }
        }
    }
    assert!(saw_body_glyph, "expected body text glyph foreground pixels");

    // At 100% opacity, a point in the right half of the slider track (`x = 92 + 324 - 20 = 396`,
    // `y = 252`) is filled with `Color::SliderTrackFill`.
    assert_eq!(frame_100.pixel(396, 252), Color::SliderTrackFill.rgba());

    // At 55% opacity, the left quarter (`x = 92 + 324 / 4 = 173`) is filled (`SliderTrackFill`)
    // while the right half (`x = 396`) is unfilled (`SliderTrackBg`).
    let params_55 = OverlayRenderParams { opacity: 0.55, ..params_100 };
    let frame_55 = draw_and_readback(&params_55);
    assert_eq!(frame_55.pixel(173, 252), Color::SliderTrackFill.rgba());
    assert_eq!(frame_55.pixel(396, 252), Color::SliderTrackBg.rgba());

    drop(target);
    drop(renderer);
    drop(vulkan);
    assert_eq!(
        validation_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors during HudRenderer GPU readback test"
    );
}

#[fuchsia::test]
fn test_swapchain_and_hud_renderer_with_sysmem2_and_validation() {
    let (vulkan, validation_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice with validation layers");
    let sysmem_allocator = connect_to_protocol_sync::<fsysmem2::AllocatorMarker>()
        .expect("connect to fuchsia.sysmem2.Allocator");

    // 1. `width == 0` or `height == 0` is rejected before importing into Vulkan.
    let zero_width_token = allocate_synced_sysmem_token(&sysmem_allocator);
    assert!(
        Swapchain::<3>::new(Rc::clone(&vulkan), zero_width_token, 0, OVERLAY_BUFFER_HEIGHT,)
            .is_err()
    );
    let zero_height_token = allocate_synced_sysmem_token(&sysmem_allocator);
    assert!(
        Swapchain::<3>::new(Rc::clone(&vulkan), zero_height_token, OVERLAY_BUFFER_WIDTH, 0,)
            .is_err()
    );

    // Creating and immediately dropping a `Swapchain<3>` with zero rendered frames exercises
    // `wait_for_idle()` when `count == 0` in-flight fences.
    let idle_token = allocate_synced_sysmem_token(&sysmem_allocator);
    let idle_swapchain = Swapchain::<3>::new(
        Rc::clone(&vulkan),
        idle_token,
        OVERLAY_BUFFER_WIDTH,
        OVERLAY_BUFFER_HEIGHT,
    )
    .expect("allocate idle 3-slot Swapchain");
    drop(idle_swapchain);

    // 2. Allocate a `HudRenderer` and verify `HudRenderer::draw_frame()` rejects a `Swapchain`
    // smaller than `OVERLAY_BUFFER_WIDTH x OVERLAY_BUFFER_HEIGHT`, exercising `render_slot()`'s
    // `reset_command_buffer()` path cleanly.
    let mut renderer = HudRenderer::new(Rc::clone(&vulkan)).expect("create HudRenderer");
    let hud_lines = default_hud_lines();
    let params_initial = OverlayRenderParams {
        display_size: fmath::SizeU { width: 480, height: 800 },
        custom_origin: None,
        mode: OverlayDisplayMode::DiagnosticsIdle,
        confirm_close: false,
        opacity: 1.0,
        hud_generation: 1,
        hud_lines: &hud_lines,
    };
    // Exercises `display_size` and `custom_origin` under `-D warnings` when `renderer.rs` compiles
    // without `lib.rs`.
    assert_eq!(
        renderer::compute_overlay_placement(
            params_initial.display_size,
            params_initial.custom_origin
        ),
        (30, 66, 420, 241)
    );

    let small_token = allocate_synced_sysmem_token(&sysmem_allocator);
    let mut small_swapchain = Swapchain::<3>::new(
        Rc::clone(&vulkan),
        small_token,
        OVERLAY_BUFFER_WIDTH - 1,
        OVERLAY_BUFFER_HEIGHT,
    )
    .expect("allocate undersized Swapchain");
    let AcquireOutcome::Ready(small_slot) =
        small_swapchain.acquire_next().expect("acquire undersized slot 0")
    else {
        panic!("Expected Ready for undersized slot 0");
    };
    assert!(
        small_swapchain
            .render_slot(small_slot, |ctx| renderer.draw_frame(ctx, &params_initial))
            .is_err(),
        "HudRenderer::draw_frame() must reject swapchain smaller than 480x276"
    );
    drop(small_swapchain);

    // Allocate the full-sized 3-slot `Swapchain` after `renderer` so `swapchain` drops first in
    // reverse declaration order.
    let vulkan_token = allocate_synced_sysmem_token(&sysmem_allocator);
    let mut swapchain = Swapchain::<3>::new(
        Rc::clone(&vulkan),
        vulkan_token,
        OVERLAY_BUFFER_WIDTH,
        OVERLAY_BUFFER_HEIGHT,
    )
    .expect("allocate 3-slot Swapchain with sysmem2");

    // 3. Out-of-bounds `AcquiredSlot` is rejected without running `record`, and an error in
    // `render_slot()`'s recording callback resets the command buffer and leaves slot 0 `Free`.
    assert!(
        swapchain
            .render_slot(AcquiredSlot::new_for_testing(3), |_| panic!("record must not run"))
            .is_err()
    );
    let AcquireOutcome::Ready(slot0_err) = swapchain.acquire_next().expect("acquire slot 0") else {
        panic!("Expected Ready for slot 0");
    };
    assert_eq!(slot0_err.index(), 0);
    assert!(
        swapchain.render_slot(slot0_err, |_ctx| Err(anyhow!("simulated record failure"))).is_err()
    );

    // 4. Render frame 1 (initial render) into slot 0, verify `acquire_fence` signals, and verify
    // `render_slot()` rejects slot 0 while `Bound`.
    let AcquireOutcome::Ready(slot0) = swapchain.acquire_next().expect("re-acquire slot 0") else {
        panic!("Expected Ready for slot 0 after failed record");
    };
    assert_eq!(slot0.index(), 0);
    let mut slot0_handles = None;
    let rendered0 = swapchain
        .render_slot(slot0, |ctx| {
            slot0_handles = Some((ctx.image(), ctx.command_buffer()));
            renderer.draw_frame(ctx, &params_initial)
        })
        .expect("render initial frame into slot 0");
    let slot0_handles = slot0_handles.expect("recorded slot 0 handles");
    assert_eq!(rendered0.slot_index, 0);
    assert!(
        swapchain
            .render_slot(AcquiredSlot::new_for_testing(0), |_| panic!("record must not run"))
            .is_err(),
        "render_slot() must reject a Bound slot"
    );
    assert!(
        matches!(
            rendered0.acquire_fence.wait_one(
                zx::Signals::EVENT_SIGNALED,
                zx::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
                    FENCE_WAIT_TIMEOUT_NS as i64
                )),
            ),
            zx::WaitResult::Ok(_)
        ),
        "GPU submission must signal slot 0 acquire_fence"
    );

    // 5. Render frame 2 (opacity change) into slot 1; slot 0 transitions to `Retiring` and is
    // still rejected by `render_slot()`.
    let params_opacity = OverlayRenderParams { opacity: 0.55, ..params_initial };
    let AcquireOutcome::Ready(slot1) = swapchain.acquire_next().expect("acquire slot 1") else {
        panic!("Expected Ready for slot 1");
    };
    assert_eq!(slot1.index(), 1);
    let mut slot1_handles = None;
    let rendered1 = swapchain
        .render_slot(slot1, |ctx| {
            slot1_handles = Some((ctx.image(), ctx.command_buffer()));
            renderer.draw_frame(ctx, &params_opacity)
        })
        .expect("render opacity change frame into slot 1");
    let slot1_handles = slot1_handles.expect("recorded slot 1 handles");
    assert_eq!(rendered1.slot_index, 1);
    assert!(
        swapchain
            .render_slot(AcquiredSlot::new_for_testing(0), |_| panic!("record must not run"))
            .is_err(),
        "render_slot() must reject a Retiring slot"
    );
    assert!(matches!(
        rendered1.acquire_fence.wait_one(
            zx::Signals::EVENT_SIGNALED,
            zx::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
                FENCE_WAIT_TIMEOUT_NS as i64
            )),
        ),
        zx::WaitResult::Ok(_)
    ));

    // 6. Render frame 3 (`confirm_close = true`) into slot 2; slot 1 transitions to `Retiring`.
    let params_confirm = OverlayRenderParams { confirm_close: true, ..params_opacity };
    let AcquireOutcome::Ready(slot2) = swapchain.acquire_next().expect("acquire slot 2") else {
        panic!("Expected Ready for slot 2");
    };
    assert_eq!(slot2.index(), 2);
    let mut slot2_handles = None;
    let rendered2 = swapchain
        .render_slot(slot2, |ctx| {
            slot2_handles = Some((ctx.image(), ctx.command_buffer()));
            renderer.draw_frame(ctx, &params_confirm)
        })
        .expect("render confirm_close frame into slot 2");
    let slot2_handles = slot2_handles.expect("recorded slot 2 handles");
    assert_eq!(rendered2.slot_index, 2);
    assert_ne!(slot0_handles.0, slot1_handles.0);
    assert_ne!(slot0_handles.0, slot2_handles.0);
    assert_ne!(slot1_handles.0, slot2_handles.0);
    assert_ne!(slot0_handles.1, slot1_handles.1);
    assert_ne!(slot0_handles.1, slot2_handles.1);
    assert_ne!(slot1_handles.1, slot2_handles.1);
    assert!(matches!(
        rendered2.acquire_fence.wait_one(
            zx::Signals::EVENT_SIGNALED,
            zx::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
                FENCE_WAIT_TIMEOUT_NS as i64
            )),
        ),
        zx::WaitResult::Ok(_)
    ));

    // 7. All 3 slots are now busy (`slot 0` and `slot 1` `Retiring` with unsignaled release fences,
    // `slot 2` `Bound`). `acquire_next()` returns `Deferred` with slot 0's `release_fence`.
    let AcquireOutcome::Deferred { release_fence: deferred_fence } =
        swapchain.acquire_next().expect("acquire when all slots busy")
    else {
        panic!("Expected Deferred when all 3 slots are busy");
    };
    assert_eq!(
        deferred_fence.koid().expect("deferred koid"),
        rendered0.release_fence.koid().expect("slot 0 release_fence koid"),
    );

    // 8. Signal slot 0's `release_fence` and verify `acquire_next()` reclaims slot 0 and
    // `render_slot()` reuses slot 0's `(image, command_buffer)` handles cleanly.
    rendered0
        .release_fence
        .signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED)
        .expect("signal slot 0 release_fence");
    let AcquireOutcome::Ready(reclaimed0) =
        swapchain.acquire_next().expect("acquire reclaimed slot 0")
    else {
        panic!("Expected Ready for reclaimed slot 0");
    };
    assert_eq!(reclaimed0.index(), 0);
    let mut reclaimed0_handles = None;
    let rendered0_again = swapchain
        .render_slot(reclaimed0, |ctx| {
            reclaimed0_handles = Some((ctx.image(), ctx.command_buffer()));
            renderer.draw_frame(ctx, &params_initial)
        })
        .expect("re-render into reclaimed slot 0");
    assert_eq!(reclaimed0_handles, Some(slot0_handles));
    assert_eq!(rendered0_again.slot_index, 0);
    assert!(matches!(
        rendered0_again.acquire_fence.wait_one(
            zx::Signals::EVENT_SIGNALED,
            zx::MonotonicInstant::after(zx::MonotonicDuration::from_nanos(
                FENCE_WAIT_TIMEOUT_NS as i64
            )),
        ),
        zx::WaitResult::Ok(_)
    ));

    // Drop `swapchain` before `renderer` and `vulkan`, then verify zero validation errors.
    drop(swapchain);
    drop(renderer);
    drop(vulkan);
    assert_eq!(
        validation_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors across Swapchain and HudRenderer lifecycle"
    );

    // 9. On a separate validated `VulkanDevice`, abandoning the device causes `Swapchain::new()`,
    // `Swapchain::acquire_next()`, and `Swapchain::render_slot()` to return `Err`, and dropping
    // the swapchain skips Vulkan teardown with zero validation errors.
    let (abandoned_vk, abandoned_errors) =
        VulkanDevice::new_with_validation().expect("create VulkanDevice for abandoned swapchain");
    let abandoned_token = allocate_synced_sysmem_token(&sysmem_allocator);
    let mut abandoned_swapchain = Swapchain::<3>::new(
        Rc::clone(&abandoned_vk),
        abandoned_token,
        OVERLAY_BUFFER_WIDTH,
        OVERLAY_BUFFER_HEIGHT,
    )
    .expect("allocate Swapchain for abandoned test");
    let AcquireOutcome::Ready(abandoned_slot) =
        abandoned_swapchain.acquire_next().expect("acquire slot before abandoning")
    else {
        panic!("Expected Ready slot before abandoning");
    };
    abandoned_vk.mark_abandoned_for_testing();
    let abandoned_new_token = allocate_synced_sysmem_token(&sysmem_allocator);
    assert!(
        Swapchain::<3>::new(
            Rc::clone(&abandoned_vk),
            abandoned_new_token,
            OVERLAY_BUFFER_WIDTH,
            OVERLAY_BUFFER_HEIGHT,
        )
        .is_err(),
        "Swapchain::new() must reject an abandoned VulkanDevice"
    );
    assert!(abandoned_swapchain.acquire_next().is_err());
    assert!(abandoned_swapchain.render_slot(abandoned_slot, |_| Ok(())).is_err());
    drop(abandoned_swapchain);
    drop(abandoned_vk);
    assert_eq!(
        abandoned_errors.load(Ordering::SeqCst),
        0,
        "expected zero Vulkan validation errors when dropping Swapchain on abandoned VulkanDevice"
    );
}
