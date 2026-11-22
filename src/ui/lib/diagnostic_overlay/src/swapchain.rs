// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Metadata-free Vulkan + sysmem2 + `zx::Event` swapchain pool.

use anyhow::{Context as _, Error, anyhow, ensure};
use ash::prelude::VkResult;
use ash::{Device, Instance, RawPtr, vk};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_images2 as fimages2;
use fidl_fuchsia_sysmem2 as fsysmem2;
use fuchsia_trace as ftrace;
use std::rc::Rc;

use crate::vulkan::{RenderContext, VulkanDevice, find_memory_type_index};

/// Vulkan image format negotiated for sysmem2 swapchain images.
pub(crate) const SWAPCHAIN_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const SWAPCHAIN_IMAGE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::TRANSFER_DST;
/// Required optimal-tiling format features on [`SWAPCHAIN_FORMAT`] for clears and HUD blits.
pub(crate) const SWAPCHAIN_FORMAT_FEATURES: vk::FormatFeatureFlags =
    vk::FormatFeatureFlags::from_raw(
        vk::FormatFeatureFlags::TRANSFER_DST.as_raw() | vk::FormatFeatureFlags::BLIT_DST.as_raw(),
    );

/// Wrapper for the `VK_FUCHSIA_buffer_collection` device extension.
#[derive(Clone)]
struct FuchsiaBufferCollectionLoader {
    handle: vk::Device,
    fp: ash::fuchsia::buffer_collection::DeviceFn,
}

impl FuchsiaBufferCollectionLoader {
    fn new(instance: &Instance, device: &Device) -> Self {
        let ext = ash::fuchsia::buffer_collection::Device::new(instance, device);
        Self { handle: ext.device(), fp: ext.fp().clone() }
    }

    /// # Safety
    ///
    /// `create_info.collection_token` must be a valid Zircon sysmem buffer collection token handle
    /// whose ownership is surrendered to `vkCreateBufferCollectionFUCHSIA()` (callers must not
    /// close it on either success or failure), and `self.handle` must remain a live `vk::Device`.
    unsafe fn create_buffer_collection(
        &self,
        create_info: &vk::BufferCollectionCreateInfoFUCHSIA<'_>,
        allocation_callbacks: Option<&vk::AllocationCallbacks<'_>>,
    ) -> VkResult<vk::BufferCollectionFUCHSIA> {
        let mut collection = vk::BufferCollectionFUCHSIA::null();
        let raw_callbacks = allocation_callbacks.as_raw_ptr();
        // SAFETY: Caller guarantees `self.handle` is a live `vk::Device` and surrenders ownership
        // of `create_info.collection_token` to `vkCreateBufferCollectionFUCHSIA()`, which consumes
        // the handle on both success and failure.
        let res = unsafe {
            (self.fp.create_buffer_collection_fuchsia)(
                self.handle,
                create_info,
                raw_callbacks,
                &mut collection,
            )
        };
        res.result_with_success(collection)
    }

    /// # Safety
    ///
    /// `collection` must be a valid `vk::BufferCollectionFUCHSIA` created on `self.handle`.
    unsafe fn set_buffer_collection_image_constraints(
        &self,
        collection: vk::BufferCollectionFUCHSIA,
        info: &vk::ImageConstraintsInfoFUCHSIA<'_>,
    ) -> VkResult<()> {
        // SAFETY: Caller guarantees `collection` is a live collection handle created on
        // `self.handle` and `info` has valid nested pointers for the duration of the call.
        let res = unsafe {
            (self.fp.set_buffer_collection_image_constraints_fuchsia)(
                self.handle,
                collection,
                info as *const vk::ImageConstraintsInfoFUCHSIA<'_>,
            )
        };
        res.result_with_success(())
    }

    /// # Safety
    ///
    /// `collection` must be a valid `vk::BufferCollectionFUCHSIA` created on `self.handle` and not
    /// used after destruction.
    unsafe fn destroy_buffer_collection(
        &self,
        collection: vk::BufferCollectionFUCHSIA,
        allocation_callbacks: Option<&vk::AllocationCallbacks<'_>>,
    ) {
        let raw_callbacks = allocation_callbacks.as_raw_ptr();
        // SAFETY: Caller guarantees `collection` is a valid, un-destroyed collection on
        // `self.handle`.
        unsafe {
            (self.fp.destroy_buffer_collection_fuchsia)(self.handle, collection, raw_callbacks);
        }
    }

    /// # Safety
    ///
    /// `collection` must be a valid `vk::BufferCollectionFUCHSIA` with image constraints already
    /// set on `self.handle`.
    unsafe fn get_buffer_collection_properties(
        &self,
        collection: vk::BufferCollectionFUCHSIA,
    ) -> VkResult<vk::BufferCollectionPropertiesFUCHSIA<'_>> {
        let mut props = vk::BufferCollectionPropertiesFUCHSIA::default();
        // SAFETY: Caller guarantees `collection` is a valid collection on `self.handle` whose
        // constraints have already been negotiated.
        let res = unsafe {
            (self.fp.get_buffer_collection_properties_fuchsia)(self.handle, collection, &mut props)
        };
        res.result_with_success(props)
    }
}

/// Wrapper for the `VK_FUCHSIA_external_semaphore` device extension.
#[derive(Clone)]
struct FuchsiaExternalSemaphoreLoader {
    handle: vk::Device,
    fp: ash::fuchsia::external_semaphore::DeviceFn,
}

impl FuchsiaExternalSemaphoreLoader {
    fn new(instance: &Instance, device: &Device) -> Self {
        let ext = ash::fuchsia::external_semaphore::Device::new(instance, device);
        Self { handle: ext.device(), fp: ext.fp().clone() }
    }

    /// # Safety
    ///
    /// `semaphore` must be a valid `vk::Semaphore` owned by `self.handle` that is not currently
    /// waited on or signaled by in-flight GPU work.
    unsafe fn import_event(&self, semaphore: vk::Semaphore, event: zx::Event) -> Result<(), Error> {
        let raw_handle = event.into_raw();
        let info = vk::ImportSemaphoreZirconHandleInfoFUCHSIA::default()
            .semaphore(semaphore)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::ZIRCON_EVENT_FUCHSIA)
            .zircon_handle(raw_handle);
        // SAFETY: Caller guarantees `semaphore` is an idle `vk::Semaphore` on `self.handle`, and
        // `raw_handle` is a valid owned `zx::Event` handle whose ownership transfers to Vulkan on
        // `VK_SUCCESS` (or is reclaimed below on error).
        let import_res = unsafe {
            (self.fp.import_semaphore_zircon_handle_fuchsia)(self.handle, &info).result()
        };
        if let Err(e) = import_res {
            // SAFETY: Vulkan did not consume `raw_handle` because the import call failed, so we
            // reclaim ownership to close the Zircon event handle cleanly.
            let _ = unsafe { zx::NullableHandle::from_raw(raw_handle) };
            return Err(anyhow!("vkImportSemaphoreZirconHandleFUCHSIA failed: {e:?}"));
        }
        Ok(())
    }
}

/// Lifecycle state of an individual swapchain image slot.
///
/// At most one slot in a ring is [`SlotLifecycle::Bound`] at any point. When a subsequent slot is
/// bound, the previously bound slot transitions to [`SlotLifecycle::Retiring`], holding the release
/// fence that the consumer (e.g. Scenic) will signal once that image is no longer in use.
#[derive(Debug)]
enum SlotLifecycle {
    /// Detached and free of any pending release fence.
    Free,
    /// Currently bound as the active swapchain image; `release_fence` will be signaled by the
    /// consumer after a later image supersedes this slot.
    Bound { release_fence: zx::Event },
    /// Superseded by a newer bound slot and waiting for the consumer to signal `release_fence`
    /// before reuse. `retired_seq` increases monotonically so [`AcquireOutcome::Deferred`] waits
    /// on the earliest-retired slot first.
    Retiring { release_fence: zx::Event, retired_seq: u64 },
}

#[derive(Debug)]
struct SlotState {
    lifecycle: SlotLifecycle,
    last_used_seq: u64,
}

/// Non-blockingly polls a [`SlotLifecycle::Retiring`] slot's release fence, transitioning it to
/// [`SlotLifecycle::Free`] and returning `Ok(true)` if signaled.
fn poll_slot_release_fence(slot: &mut SlotState, index: usize) -> Result<bool, Error> {
    let signaled = match &slot.lifecycle {
        SlotLifecycle::Retiring { release_fence, .. } => {
            match release_fence
                .wait_one(zx::Signals::EVENT_SIGNALED, zx::MonotonicInstant::INFINITE_PAST)
            {
                zx::WaitResult::Ok(_) => true,
                zx::WaitResult::TimedOut(_) => false,
                zx::WaitResult::Canceled(_) => {
                    return Err(anyhow!("poll release fence for buffer {index}: handle canceled"));
                }
                zx::WaitResult::Err(status) => {
                    return Err(anyhow!("poll release fence for buffer {index}: {status}"));
                }
            }
        }
        SlotLifecycle::Free | SlotLifecycle::Bound { .. } => return Ok(false),
    };
    if signaled {
        slot.lifecycle = SlotLifecycle::Free;
    }
    Ok(signaled)
}

/// Selects the least-recently-used [`SlotLifecycle::Free`] slot (`min_by_key()` on
/// `(slot.last_used_seq, idx)`).
fn select_free_slot(slots: &[SlotState]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.lifecycle, SlotLifecycle::Free))
        .min_by_key(|(idx, slot)| (slot.last_used_seq, *idx))
        .map(|(idx, _)| idx)
}

/// Token representing an acquired [`SlotLifecycle::Free`] swapchain slot ready to be rendered via
/// [`Swapchain::render_slot()`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AcquiredSlot {
    index: usize,
}

impl AcquiredSlot {
    #[inline]
    pub(crate) const fn index(&self) -> usize {
        self.index
    }

    #[cfg(test)]
    #[inline]
    pub(crate) const fn new_for_testing(index: usize) -> Self {
        Self { index }
    }
}

/// Result of attempting to acquire the next free swapchain slot.
#[derive(Debug)]
pub(crate) enum AcquireOutcome {
    /// A free image slot was selected for rendering; pass to [`Swapchain::render_slot()`].
    Ready(AcquiredSlot),
    /// Zero free images remain; wait asynchronously on `release_fence` and retry.
    Deferred { release_fence: zx::Event },
}

/// Output of [`Swapchain::render_slot()`] containing the bound slot index and the acquire/release
/// fence pair for the consumer.
///
/// # Consumer Contract
///
/// - At most one slot is bound in the consumer at a time.
/// - Replacing or unbinding a slot schedules `release_fence` once the consumer will no longer read
///   that slot; [`Swapchain`] tracks GPU submission completion separately via its per-slot
///   `command_fence` if the consumer drops `acquire_fence` un-waited.
/// - If attaching a [`RenderedSlot`] to the consumer fails, the caller must drop the [`Swapchain`]
///   instead of reusing it.
#[derive(Debug)]
pub(crate) struct RenderedSlot {
    pub slot_index: usize,
    pub acquire_fence: zx::Event,
    pub release_fence: zx::Event,
}

/// Tracks `Free | Bound | Retiring` slot lifecycles and LRU ordering for an `N`-slot swapchain
/// independently of Vulkan handles.
struct SwapchainRing<const N: usize> {
    slots: [SlotState; N],
    next_seq: u64,
}

impl<const N: usize> SwapchainRing<N> {
    fn new() -> Self {
        const {
            assert!(N >= 2, "ring swapchain requires at least two image slots");
        }
        Self {
            slots: std::array::from_fn(|_| SlotState {
                lifecycle: SlotLifecycle::Free,
                last_used_seq: 0,
            }),
            next_seq: 1,
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    fn acquire_next(&mut self) -> Result<AcquireOutcome, Error> {
        // 1. If any slot is already `Free`, select the least-recently-used `Free` slot without
        // polling kernel fence handles.
        if let Some(index) = select_free_slot(&self.slots) {
            return Ok(AcquireOutcome::Ready(AcquiredSlot { index }));
        }

        // 2. Otherwise, poll `Retiring` slots in FIFO retirement order and return immediately once
        // the first signaled release fence reclaims a `Free` slot.
        let mut retiring: [(u64, usize); N] = [(u64::MAX, 0); N];
        let mut retiring_count = 0;
        for (idx, slot) in self.slots.iter().enumerate() {
            if let SlotLifecycle::Retiring { retired_seq, .. } = slot.lifecycle {
                retiring[retiring_count] = (retired_seq, idx);
                retiring_count += 1;
            }
        }
        retiring[..retiring_count].sort_unstable();
        for &(_, idx) in &retiring[..retiring_count] {
            if poll_slot_release_fence(&mut self.slots[idx], idx)? {
                return Ok(AcquireOutcome::Ready(AcquiredSlot { index: idx }));
            }
        }

        // 3. Zero slots are `Free` (all non-bound slots are `Retiring` with unsignaled fences);
        // duplicate the earliest-retired slot's release fence for asynchronous waiting.
        let &(_, oldest_idx) = retiring[..retiring_count]
            .first()
            .ok_or_else(|| anyhow!("swapchain has neither Free nor Retiring slots"))?;
        let SlotLifecycle::Retiring { release_fence: oldest_fence, .. } =
            &self.slots[oldest_idx].lifecycle
        else {
            return Err(anyhow!("swapchain slot {oldest_idx} expected to be Retiring"));
        };
        let release_fence = oldest_fence
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .with_context(|| format!("duplicate release fence for buffer {oldest_idx}"))?;
        Ok(AcquireOutcome::Deferred { release_fence })
    }

    fn bind_slot(&mut self, slot: AcquiredSlot, release_fence: zx::Event) -> Result<usize, Error> {
        let index = slot.index();
        ensure!(index < N, "swapchain slot index {index} out of bounds for pool size {N}");
        ensure!(
            matches!(self.slots[index].lifecycle, SlotLifecycle::Free),
            "swapchain slot {index} must be Free before binding (found {:?})",
            self.slots[index].lifecycle
        );

        let retired_seq = self.alloc_seq();
        for (slot_idx, existing) in self.slots.iter_mut().enumerate() {
            if slot_idx != index && matches!(existing.lifecycle, SlotLifecycle::Bound { .. }) {
                let prev = std::mem::replace(&mut existing.lifecycle, SlotLifecycle::Free);
                if let SlotLifecycle::Bound { release_fence: prior_release_fence } = prev {
                    existing.lifecycle =
                        SlotLifecycle::Retiring { release_fence: prior_release_fence, retired_seq };
                }
            }
        }
        let used_seq = self.alloc_seq();
        self.slots[index].lifecycle = SlotLifecycle::Bound { release_fence };
        self.slots[index].last_used_seq = used_seq;
        Ok(index)
    }
}

struct VulkanSlot {
    image: vk::Image,
    device_memory: vk::DeviceMemory,
    command_fence: vk::Fence,
    fence_in_flight: bool,
    signal_semaphore: vk::Semaphore,
}

impl VulkanSlot {
    const fn null() -> Self {
        Self {
            image: vk::Image::null(),
            device_memory: vk::DeviceMemory::null(),
            command_fence: vk::Fence::null(),
            fence_in_flight: false,
            signal_semaphore: vk::Semaphore::null(),
        }
    }
}

/// # Safety
///
/// All GPU work referencing `slots` must have completed (or failed before submission), and each
/// non-null handle in `slots` must belong to `device` and not be used after this call.
unsafe fn destroy_vulkan_slots(device: &Device, slots: &[VulkanSlot]) {
    // SAFETY: Caller guarantees all GPU work referencing `slots` has completed (or failed before
    // submission), and null handles are skipped so partial initialization cleans up safely.
    unsafe {
        for slot in slots {
            if slot.signal_semaphore != vk::Semaphore::null() {
                device.destroy_semaphore(slot.signal_semaphore, None);
            }
            if slot.command_fence != vk::Fence::null() {
                device.destroy_fence(slot.command_fence, None);
            }
            if slot.image != vk::Image::null() {
                device.destroy_image(slot.image, None);
            }
            if slot.device_memory != vk::DeviceMemory::null() {
                device.free_memory(slot.device_memory, None);
            }
        }
    }
}

struct SlotCleanupGuard<'a> {
    device: &'a Device,
    slots: Vec<VulkanSlot>,
    active: bool,
}

impl<'a> SlotCleanupGuard<'a> {
    fn with_capacity(device: &'a Device, capacity: usize) -> Self {
        Self { device, slots: Vec::with_capacity(capacity), active: true }
    }

    fn from_slots(device: &'a Device, slots: Vec<VulkanSlot>) -> Self {
        Self { device, slots, active: true }
    }

    fn disarm(mut self) -> Vec<VulkanSlot> {
        self.active = false;
        std::mem::take(&mut self.slots)
    }
}

impl Drop for SlotCleanupGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // SAFETY: `SlotCleanupGuard` only drops while active when initialization fails before any
        // slot resources are submitted to a queue.
        unsafe {
            destroy_vulkan_slots(self.device, &self.slots);
        }
    }
}

/// Manages an `N`-image sysmem2-backed Vulkan swapchain pool with `zx::Event` acquire and release
/// fences.
///
/// # Drop Order Contract
///
/// [`Swapchain::drop()`] waits up to [`crate::vulkan::FENCE_WAIT_TIMEOUT_NS`] for all in-flight
/// swapchain command buffers to finish before destroying the swapchain's command pool, images, and
/// semaphores. Any external Vulkan resource referenced by commands recorded in
/// [`Swapchain::render_slot()`] (such as [`crate::debug_font::DebugFont`]) must be dropped *after*
/// [`Swapchain`].
pub(crate) struct Swapchain<const N: usize> {
    vulkan: Rc<VulkanDevice>,
    semaphore_loader: FuchsiaExternalSemaphoreLoader,
    command_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,
    vulkan_slots: Vec<VulkanSlot>,
    extent: vk::Extent2D,
    ring: SwapchainRing<N>,
}

impl<const N: usize> Drop for Swapchain<N> {
    fn drop(&mut self) {
        if !self.wait_for_idle() || self.vulkan.is_abandoned() {
            return;
        }
        // SAFETY: `wait_for_idle()` confirmed all submitted GPU work referencing
        // `self.vulkan_slots` and `self.command_buffers` has completed, and `Drop` runs at most
        // once.
        unsafe {
            let device = self.vulkan.device();
            destroy_vulkan_slots(device, &self.vulkan_slots);
            if !self.command_buffers.is_empty() {
                device.free_command_buffers(self.command_pool, &self.command_buffers);
            }
            device.destroy_command_pool(self.command_pool, None);
        }
    }
}

impl<const N: usize> Swapchain<N> {
    /// Waits up to [`crate::vulkan::FENCE_WAIT_TIMEOUT_NS`] for all in-flight swapchain GPU
    /// submissions to finish, marking the shared [`VulkanDevice`] abandoned on timeout so teardown
    /// never destroys live GPU objects or hangs the host event loop.
    fn wait_for_idle(&self) -> bool {
        if self.vulkan.is_abandoned() {
            return false;
        }
        let mut command_fences = [vk::Fence::null(); N];
        let mut count = 0;
        for slot in self.vulkan_slots.iter().take(N) {
            if slot.fence_in_flight && slot.command_fence != vk::Fence::null() {
                command_fences[count] = slot.command_fence;
                count += 1;
            }
        }
        // SAFETY: Every fence in `command_fences[..count]` is a valid non-null `vk::Fence` created
        // on `self.vulkan.device()` and owned by `self.vulkan_slots`.
        unsafe { self.vulkan.wait_for_fences(&command_fences[..count]) }.is_ok()
    }

    /// Negotiates an `N`-image sysmem2 `BufferCollection` for `width x height` [`SWAPCHAIN_FORMAT`]
    /// images with Vulkan `TRANSFER_DST` usage, imports them into `vk::Image` and
    /// `vk::DeviceMemory` via `VK_FUCHSIA_buffer_collection`, and pre-allocates `N` primary
    /// command buffers, fences, and external signal semaphores.
    ///
    /// # Preconditions
    ///
    /// - `vulkan` must not be abandoned and must have been created with [`SWAPCHAIN_FORMAT`]
    ///   supporting [`SWAPCHAIN_FORMAT_FEATURES`].
    /// - `vulkan_token` must already have completed `fuchsia.sysmem2/Node.Sync()` so sysmem
    ///   observes token creation/duplication before the Vulkan ICD calls `BindSharedCollection()`.
    /// - Any peer collection tokens (such as Scenic's) must already be registered or serviced
    ///   concurrently because `Swapchain::new()` blocks synchronously waiting for sysmem
    ///   allocation.
    pub(crate) fn new(
        vulkan: Rc<VulkanDevice>,
        vulkan_token: ClientEnd<fsysmem2::BufferCollectionTokenMarker>,
        width: u32,
        height: u32,
    ) -> Result<Self, Error> {
        const {
            assert!(N >= 2, "ring swapchain requires at least two image slots");
        }
        ensure!(!vulkan.is_abandoned(), "Vulkan device is abandoned");
        ensure!(width > 0 && height > 0, "swapchain dimensions must be non-zero");
        ftrace::duration!(c"gfx", c"Swapchain::new");

        let vulkan_slots = Self::allocate_slots(&vulkan, vulkan_token, width, height, N)
            .context("allocate sysmem-backed Vulkan swapchain")?;
        let slots_guard = SlotCleanupGuard::from_slots(vulkan.device(), vulkan_slots);

        let semaphore_loader =
            FuchsiaExternalSemaphoreLoader::new(vulkan.instance(), vulkan.device());
        let (command_pool, command_buffers) = vulkan.create_command_buffers(N as u32)?;

        let vulkan_slots = slots_guard.disarm();
        Ok(Self {
            vulkan,
            semaphore_loader,
            command_pool,
            command_buffers,
            vulkan_slots,
            extent: vk::Extent2D { width, height },
            ring: SwapchainRing::new(),
        })
    }

    fn allocate_slots(
        vulkan: &VulkanDevice,
        vulkan_token: ClientEnd<fsysmem2::BufferCollectionTokenMarker>,
        width: u32,
        height: u32,
        image_count: usize,
    ) -> Result<Vec<VulkanSlot>, Error> {
        ftrace::duration!(c"gfx", c"Swapchain::allocate_slots");
        let vk_image_create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(SWAPCHAIN_FORMAT)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(SWAPCHAIN_IMAGE_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let color_spaces = [vk::SysmemColorSpaceFUCHSIA::default()
            .color_space(fimages2::ColorSpace::Srgb.into_primitive())];

        let format_constraints = [vk::ImageFormatConstraintsInfoFUCHSIA::default()
            .image_create_info(vk_image_create_info)
            .required_format_features(SWAPCHAIN_FORMAT_FEATURES)
            .sysmem_pixel_format(0)
            .color_spaces(&color_spaces)];

        // Omit `max_buffer_count` so sysmem can satisfy any additional camping buffers requested
        // by Scenic or the display controller without failing collection allocation.
        let image_constraints = vk::ImageConstraintsInfoFUCHSIA::default()
            .format_constraints(&format_constraints)
            .buffer_collection_constraints(
                vk::BufferCollectionConstraintsInfoFUCHSIA::default()
                    .min_buffer_count(image_count as u32),
            );

        let collection_create_info = vk::BufferCollectionCreateInfoFUCHSIA {
            collection_token: vulkan_token.into_channel().into_raw(),
            ..Default::default()
        };

        let device = vulkan.device();
        let mem_props = vulkan.physical_device_memory_properties();
        let loader = FuchsiaBufferCollectionLoader::new(vulkan.instance(), device);
        // SAFETY: `device` is a live `ash::Device` and `collection_create_info` holds a valid
        // Zircon sysmem2 token handle transferred from `vulkan_token`.
        let vk_collection =
            unsafe { loader.create_buffer_collection(&collection_create_info, None) }
                .map_err(|e| anyhow!("vkCreateBufferCollectionFUCHSIA failed: {e:?}"))?;

        let alloc_res = (|| -> Result<Vec<VulkanSlot>, Error> {
            // SAFETY: `vk_collection` is a valid buffer collection on `device` and
            // `image_constraints` borrows `format_constraints` and `color_spaces` for this call.
            unsafe {
                loader.set_buffer_collection_image_constraints(vk_collection, &image_constraints)
            }
            .map_err(|e| anyhow!("vkSetBufferCollectionImageConstraintsFUCHSIA failed: {e:?}"))?;

            // SAFETY: `vk_collection` is a valid collection with constraints set above.
            let collection_props =
                unsafe { loader.get_buffer_collection_properties(vk_collection) }
                    .map_err(|e| anyhow!("vkGetBufferCollectionPropertiesFUCHSIA failed: {e:?}"))?;

            let mut guard = SlotCleanupGuard::with_capacity(device, image_count);
            for index in 0..image_count as u32 {
                // Push a null-initialized slot before populating handles so `SlotCleanupGuard`
                // cleans up any partial failure automatically.
                guard.slots.push(VulkanSlot::null());
                let slot_idx = guard.slots.len() - 1;
                let slot = &mut guard.slots[slot_idx];

                let mut collection_img_info = vk::BufferCollectionImageCreateInfoFUCHSIA::default()
                    .collection(vk_collection)
                    .index(index);
                let img_info = vk_image_create_info.push_next(&mut collection_img_info);
                // SAFETY: `device` is live and `img_info` borrows `collection_img_info` for a
                // valid `vk_collection` and buffer `index`.
                slot.image = unsafe { device.create_image(&img_info, None) }.map_err(|e| {
                    anyhow!("vkCreateImage for sysmem buffer {index} failed: {e:?}")
                })?;
                // SAFETY: `slot.image` was just created on `device`.
                let mem_reqs = unsafe { device.get_image_memory_requirements(slot.image) };
                let memory_type_index = find_memory_type_index(
                    &mem_props,
                    collection_props.memory_type_bits & mem_reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::empty(),
                )
                .ok_or_else(|| anyhow!("no valid memory type for sysmem buffer {index}"))?;

                let mut import_mem = vk::ImportMemoryBufferCollectionFUCHSIA::default()
                    .collection(vk_collection)
                    .index(index);
                let alloc_info = vk::MemoryAllocateInfo::default()
                    .push_next(&mut import_mem)
                    .allocation_size(mem_reqs.size)
                    .memory_type_index(memory_type_index);
                // SAFETY: `alloc_info` borrows `import_mem` for a valid `vk_collection` and
                // `index`, and `memory_type_index` was validated against `collection_props`.
                slot.device_memory =
                    unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
                        anyhow!("vkAllocateMemory for sysmem buffer {index} failed: {e:?}")
                    })?;
                // SAFETY: `slot.image` and `slot.device_memory` are valid unbound handles
                // imported from the same sysmem collection index.
                unsafe { device.bind_image_memory(slot.image, slot.device_memory, 0) }.map_err(
                    |e| anyhow!("vkBindImageMemory for sysmem buffer {index} failed: {e:?}"),
                )?;
                let fence_info = vk::FenceCreateInfo::default();
                // SAFETY: `device` is a live `ash::Device`.
                slot.command_fence = unsafe { device.create_fence(&fence_info, None) }
                    .map_err(|e| anyhow!("vkCreateFence for buffer {index} failed: {e:?}"))?;
                let sem_info = vk::SemaphoreCreateInfo::default();
                // SAFETY: `device` is a live `ash::Device`.
                slot.signal_semaphore = unsafe { device.create_semaphore(&sem_info, None) }
                    .map_err(|e| anyhow!("vkCreateSemaphore (signal) {index} failed: {e:?}"))?;
            }
            Ok(guard.disarm())
        })();

        // SAFETY: `vk_collection` is no longer needed once all images and memories are bound,
        // and is destroyed once here.
        unsafe {
            loader.destroy_buffer_collection(vk_collection, None);
        }

        alloc_res
    }

    /// Polls retired release fences lazily and returns the next [`AcquireOutcome`].
    pub(crate) fn acquire_next(&mut self) -> Result<AcquireOutcome, Error> {
        ensure!(!self.vulkan.is_abandoned(), "Vulkan device is abandoned");
        self.ring.acquire_next()
    }

    /// Records GPU commands into `slot`, submits them to the graphics queue with a fresh exported
    /// `acquire_fence` semaphore, binds the slot in the swapchain ring with a fresh
    /// `release_fence`, and returns [`RenderedSlot`].
    ///
    /// # Recording, Queue-Family Ownership, Consumer, and Lifetime Contract
    ///
    /// - `record` receives a [`RenderContext`] whose `command_buffer()` is already in the recording
    ///   state (`vkBeginCommandBuffer()` has been called); `record` must *not* call
    ///   `vkBeginCommandBuffer()` or `vkEndCommandBuffer()`. If `record` returns `Err`, the command
    ///   buffer is reset via `vkResetCommandBuffer()` before the error is propagated.
    /// - Because `ctx.image()` is shared with an external consumer (such as Scenic) via sysmem2,
    ///   `record` must transition `ctx.image()` from `VK_QUEUE_FAMILY_FOREIGN_EXT` to
    ///   `ctx.queue_family_index()` before writing and release it back from
    ///   `ctx.queue_family_index()` to `VK_QUEUE_FAMILY_FOREIGN_EXT` (`vk::ImageLayout::GENERAL`)
    ///   before returning `Ok(())`.
    /// - At most one slot is bound in the consumer at a time; replacing or unbinding a slot
    ///   schedules `release_fence` once the consumer will no longer read that slot, while
    ///   [`Swapchain`] tracks GPU submission completion separately via its per-slot `command_fence`
    ///   if the consumer drops `acquire_fence` un-waited. If attaching a [`RenderedSlot`] fails,
    ///   the caller must drop the [`Swapchain`] instead of reusing it.
    /// - Any external Vulkan resource referenced by commands recorded in `record` (such as
    ///   [`crate::debug_font::DebugFont`]) must outlive this [`Swapchain`] so [`Swapchain::drop()`]
    ///   waits for the submission fence before that external resource is destroyed.
    pub(crate) fn render_slot<F>(
        &mut self,
        slot: AcquiredSlot,
        record: F,
    ) -> Result<RenderedSlot, Error>
    where
        F: FnOnce(RenderContext<'_>) -> Result<(), Error>,
    {
        ensure!(!self.vulkan.is_abandoned(), "Vulkan device is abandoned");
        let idx = slot.index();
        ensure!(idx < N, "swapchain slot index {idx} out of bounds for pool size {N}");
        ensure!(
            matches!(self.ring.slots[idx].lifecycle, SlotLifecycle::Free),
            "swapchain slot {idx} must be Free before rendering (found {:?})",
            self.ring.slots[idx].lifecycle
        );

        let cb = self.command_buffers[idx];
        let (image, command_fence, fence_in_flight, signal_semaphore) = {
            let s = &self.vulkan_slots[idx];
            (s.image, s.command_fence, s.fence_in_flight, s.signal_semaphore)
        };
        let device = self.vulkan.device();
        let queue_family_index = self.vulkan.queue_family_index();

        if fence_in_flight {
            // SAFETY: `command_fence` is a valid non-null `vk::Fence` created on `device` and
            // owned by `self.vulkan_slots[idx]`.
            unsafe { self.vulkan.wait_for_fences(&[command_fence]) }
                .map_err(|e| anyhow!("vkWaitForFences failed: {e:?}"))?;
            self.vulkan_slots[idx].fence_in_flight = false;
        }

        let acquire_fence = zx::Event::create();
        let sem_event = acquire_fence
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .context("duplicate acquire fence event")?;
        let release_fence = zx::Event::create();
        let ring_release_fence = release_fence
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .context("duplicate release fence event")?;

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: Any prior submission into `idx` completed above so `cb` and `image` are idle,
        // and `cb` was allocated with `RESET_COMMAND_BUFFER`.
        unsafe {
            device
                .begin_command_buffer(cb, &begin_info)
                .map_err(|e| anyhow!("vkBeginCommandBuffer failed: {e:?}"))?;
        }

        // SAFETY: `device` is live, `cb` is a primary command buffer in the recording state on
        // `device` (`queue_family_index`), and `image` is an idle `TRANSFER_DST` image on `device`
        // with dimensions `self.extent`.
        let ctx = unsafe { RenderContext::new(device, cb, image, self.extent, queue_family_index) };
        if let Err(e) = record(ctx) {
            // SAFETY: `cb` is in the recording state and not pending execution, and its pool was
            // created with `VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT`.
            let _ =
                unsafe { device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty()) };
            return Err(e);
        }

        // SAFETY: `cb` is in the recording state and `record()` completed without error.
        unsafe {
            device
                .end_command_buffer(cb)
                .map_err(|e| anyhow!("vkEndCommandBuffer failed: {e:?}"))?;
        }

        // SAFETY: Any prior submission completed above so `signal_semaphore` is idle, and
        // `sem_event` is a valid `zx::Event` handle consumed by `import_event()`.
        unsafe {
            self.semaphore_loader.import_event(signal_semaphore, sem_event)?;
        }

        let cbs = [cb];
        let sig_sems = [signal_semaphore];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs).signal_semaphores(&sig_sems);

        // SAFETY: `command_fence` is idle, `cb` is in the executable state, `signal_semaphore` is
        // a valid semaphore on `device`, and `self.vulkan.queue()` is a graphics queue on
        // `device`. Only set `fence_in_flight = true` after `queue_submit()` succeeds so `Drop`
        // never waits on an unsubmitted reset fence.
        unsafe {
            device
                .reset_fences(&[command_fence])
                .map_err(|e| anyhow!("vkResetFences failed: {e:?}"))?;
            device
                .queue_submit(self.vulkan.queue(), &[submit], command_fence)
                .map_err(|e| anyhow!("vkQueueSubmit failed: {e:?}"))?;
        }

        self.vulkan_slots[idx].fence_in_flight = true;
        let slot_index = self.ring.bind_slot(slot, ring_release_fence)?;
        Ok(RenderedSlot { slot_index, acquire_fence, release_fence })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_slot(lifecycle: SlotLifecycle, last_used_seq: u64) -> SlotState {
        SlotState { lifecycle, last_used_seq }
    }

    #[fuchsia::test]
    fn test_select_free_slot_lru_order() {
        // All empty `Free` slots pick the lowest index.
        let all_empty = [
            make_test_slot(SlotLifecycle::Free, 0),
            make_test_slot(SlotLifecycle::Free, 0),
            make_test_slot(SlotLifecycle::Free, 0),
        ];
        assert_eq!(select_free_slot(&all_empty), Some(0));

        // Least-recently-used `Free` slot (`last_used_seq`) is selected first.
        let lru_slots = [
            make_test_slot(SlotLifecycle::Free, 10),
            make_test_slot(SlotLifecycle::Free, 3),
            make_test_slot(SlotLifecycle::Free, 7),
        ];
        assert_eq!(select_free_slot(&lru_slots), Some(1));

        // Non-`Free` (`Bound` or `Retiring`) slots are skipped even if their `last_used_seq` is
        // lower.
        let mixed = [
            make_test_slot(SlotLifecycle::Bound { release_fence: zx::Event::create() }, 1),
            make_test_slot(
                SlotLifecycle::Retiring { release_fence: zx::Event::create(), retired_seq: 2 },
                2,
            ),
            make_test_slot(SlotLifecycle::Free, 9),
        ];
        assert_eq!(select_free_slot(&mixed), Some(2));

        // When zero slots are `Free`, `select_free_slot()` returns `None`.
        let no_free = [
            make_test_slot(
                SlotLifecycle::Retiring { release_fence: zx::Event::create(), retired_seq: 1 },
                1,
            ),
            make_test_slot(SlotLifecycle::Bound { release_fence: zx::Event::create() }, 2),
            make_test_slot(
                SlotLifecycle::Retiring { release_fence: zx::Event::create(), retired_seq: 2 },
                3,
            ),
        ];
        assert_eq!(select_free_slot(&no_free), None);
    }

    #[fuchsia::test]
    fn test_poll_slot_release_fence() {
        let mut free_slot = make_test_slot(SlotLifecycle::Free, 0);
        assert!(!poll_slot_release_fence(&mut free_slot, 0).unwrap());

        let event = zx::Event::create();
        let signaler = event.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut retiring_slot =
            make_test_slot(SlotLifecycle::Retiring { release_fence: event, retired_seq: 1 }, 0);
        assert!(!poll_slot_release_fence(&mut retiring_slot, 0).unwrap());
        assert!(matches!(retiring_slot.lifecycle, SlotLifecycle::Retiring { .. }));

        signaler.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        assert!(poll_slot_release_fence(&mut retiring_slot, 0).unwrap());
        assert!(matches!(retiring_slot.lifecycle, SlotLifecycle::Free));

        // Event handle without `zx::Rights::WAIT` yields `ZX_ERR_ACCESS_DENIED`.
        let no_wait_event = zx::Event::create().replace_handle(zx::Rights::SIGNAL).unwrap();
        let mut bad_rights_slot = make_test_slot(
            SlotLifecycle::Retiring { release_fence: no_wait_event, retired_seq: 2 },
            0,
        );
        assert!(poll_slot_release_fence(&mut bad_rights_slot, 0).is_err());
    }

    #[fuchsia::test]
    fn test_swapchain_ring_bind_slot_validates_bounds_and_free_state() {
        let mut ring = SwapchainRing::<3>::new();

        // Out-of-bounds slot index is rejected.
        assert!(ring.bind_slot(AcquiredSlot { index: 3 }, zx::Event::create()).is_err());

        // Binding a `Free` slot succeeds and marks it `Bound`.
        let AcquireOutcome::Ready(slot0) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for slot 0");
        };
        assert_eq!(slot0.index(), 0);
        assert_eq!(ring.bind_slot(slot0, zx::Event::create()).unwrap(), 0);

        // Re-binding a currently `Bound` slot is rejected.
        assert!(ring.bind_slot(AcquiredSlot { index: 0 }, zx::Event::create()).is_err());

        // Binding slot 1 transitions slot 0 to `Retiring`; binding `Retiring` slot 0 is rejected.
        let AcquireOutcome::Ready(slot1) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for slot 1");
        };
        assert_eq!(slot1.index(), 1);
        assert_eq!(ring.bind_slot(slot1, zx::Event::create()).unwrap(), 1);
        assert!(matches!(ring.slots[0].lifecycle, SlotLifecycle::Retiring { .. }));
        assert!(ring.bind_slot(AcquiredSlot { index: 0 }, zx::Event::create()).is_err());
    }

    #[fuchsia::test]
    fn test_swapchain_ring_lifecycle_fifo_stop_on_first_signaled_and_lru_free() {
        let mut ring = SwapchainRing::<3>::new();

        // 1. Acquire and bind all 3 slots in order `0 -> 1 -> 2`.
        let AcquireOutcome::Ready(slot0) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for slot 0");
        };
        assert_eq!(slot0.index(), 0);
        let rel0 = zx::Event::create();
        let rel0_sig = rel0.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let rel0_koid = rel0.koid().unwrap();
        assert_eq!(ring.bind_slot(slot0, rel0).unwrap(), 0);

        let AcquireOutcome::Ready(slot1) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for slot 1");
        };
        assert_eq!(slot1.index(), 1);
        let rel1 = zx::Event::create();
        let rel1_sig = rel1.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        assert_eq!(ring.bind_slot(slot1, rel1).unwrap(), 1);

        // Even while slot 0 (`Retiring`) is still unsignaled, `acquire_next()` selects `Free`
        // slot 2 without polling or stalling.
        let AcquireOutcome::Ready(slot2) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for Free slot 2 while slot 0 is Retiring");
        };
        assert_eq!(slot2.index(), 2);
        let rel2 = zx::Event::create();
        let rel2_sig = rel2.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let rel2_koid = rel2.koid().unwrap();
        assert_eq!(ring.bind_slot(slot2, rel2).unwrap(), 2);

        // 2. Now slot 0 (`retired_seq` earliest) and slot 1 (`retired_seq` second) are `Retiring`
        // with unsignaled fences, and slot 2 is `Bound`. `acquire_next()` returns `Deferred` with
        // a duplicate of slot 0's release fence.
        let AcquireOutcome::Deferred { release_fence: deferred0 } = ring.acquire_next().unwrap()
        else {
            panic!("Expected Deferred when zero slots are Free");
        };
        assert_eq!(
            deferred0.koid().unwrap(),
            rel0_koid,
            "Deferred must wait on earliest-retired slot first"
        );

        // 3. Signal BOTH `rel0_sig` and `rel1_sig` before calling `acquire_next()`. Because
        // `acquire_next()` polls `Retiring` slots in FIFO retirement order and returns immediately
        // once the first fence signals, slot 0 is reclaimed to `Free` and returned while slot 1
        // remains `Retiring` (not yet polled).
        rel0_sig.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        rel1_sig.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        let AcquireOutcome::Ready(reclaimed0) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for earliest-retired slot 0");
        };
        assert_eq!(reclaimed0.index(), 0);
        assert!(matches!(ring.slots[0].lifecycle, SlotLifecycle::Free));
        assert!(
            matches!(ring.slots[1].lifecycle, SlotLifecycle::Retiring { .. }),
            "acquire_next() must stop polling Retiring slots after the first signaled fence"
        );

        // 4. If slot 1 is also reclaimed before slot 0 is bound, `select_free_slot()` chooses
        // between `Free` slots 0 and 1 by LRU (`last_used_seq`): slot 0 was bound before slot 1,
        // so slot 0 has the smaller `last_used_seq` and is selected first.
        assert!(poll_slot_release_fence(&mut ring.slots[1], 1).unwrap());
        assert!(matches!(ring.slots[1].lifecycle, SlotLifecycle::Free));
        let AcquireOutcome::Ready(lru_first) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for LRU Free slot 0");
        };
        assert_eq!(lru_first.index(), 0);
        let rel0_second = zx::Event::create();
        let rel0_second_sig = rel0_second.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        assert_eq!(ring.bind_slot(lru_first, rel0_second).unwrap(), 0);

        let AcquireOutcome::Ready(lru_second) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for remaining Free slot 1");
        };
        assert_eq!(lru_second.index(), 1);

        // 5. Bind slot 1 so slot 2 (retired when slot 0 was bound) has an earlier `retired_seq`
        // than slot 0 (retired when slot 1 is bound) despite its higher slot index (`2 > 0`).
        assert_eq!(ring.bind_slot(lru_second, zx::Event::create()).unwrap(), 1);
        let AcquireOutcome::Deferred { release_fence: deferred2 } = ring.acquire_next().unwrap()
        else {
            panic!("Expected Deferred when slots 2 and 0 are Retiring");
        };
        assert_eq!(
            deferred2.koid().unwrap(),
            rel2_koid,
            "Deferred must return earliest-retired slot 2 despite higher slot index 2 > 0"
        );

        // Signal both slot 2 and slot 0 fences: `acquire_next()` must reclaim earliest-retired
        // slot 2 first and leave slot 0 `Retiring`.
        rel2_sig.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        rel0_second_sig.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
        let AcquireOutcome::Ready(reclaimed2) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for earliest-retired slot 2");
        };
        assert_eq!(reclaimed2.index(), 2);
        assert!(matches!(ring.slots[2].lifecycle, SlotLifecycle::Free));
        assert!(matches!(ring.slots[0].lifecycle, SlotLifecycle::Retiring { .. }));

        // Once slot 0 is also `Free`, `acquire_next()` selects LRU slot 2 (bound earlier than
        // slot 0's second bind) before slot 0.
        assert!(poll_slot_release_fence(&mut ring.slots[0], 0).unwrap());
        assert!(matches!(ring.slots[0].lifecycle, SlotLifecycle::Free));
        let AcquireOutcome::Ready(lru_slot2) = ring.acquire_next().unwrap() else {
            panic!("Expected Ready for LRU Free slot 2");
        };
        assert_eq!(lru_slot2.index(), 2);
    }
}
