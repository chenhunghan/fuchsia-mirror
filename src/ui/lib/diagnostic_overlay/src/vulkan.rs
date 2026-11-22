// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared Vulkan 1.1 instance, physical/logical device, queue, and one-shot command submission.

use anyhow::{Error, anyhow, ensure};
use ash::vk;
use fuchsia_trace as ftrace;
use log::warn;
use std::cell::Cell;
use std::ffi::{CStr, c_char};
use std::rc::Rc;
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(test)]
use crate::debug_font::{ATLAS_FORMAT, ATLAS_FORMAT_FEATURES};
#[cfg(test)]
use crate::swapchain::{SWAPCHAIN_FORMAT, SWAPCHAIN_FORMAT_FEATURES};

const VK_API_VERSION: u32 = vk::make_api_version(0, 1, 1, 0);

#[cfg(test)]
const VALIDATION_LAYER_NAME: &CStr = c"VK_LAYER_KHRONOS_validation";

/// Upper bound in nanoseconds (`10 s`) for blocking Vulkan fence waits during initialization,
/// per-frame command-buffer submission, and teardown so stalled GPU work fails rather than hanging
/// the host event loop indefinitely.
pub(crate) const FENCE_WAIT_TIMEOUT_NS: u64 = 10_000_000_000;

/// Per-frame Vulkan recording context passed by [`crate::swapchain::Swapchain::render_slot()`] to
/// its command-recording closure.
#[derive(Clone, Copy)]
pub(crate) struct RenderContext<'a> {
    device: &'a ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    extent: vk::Extent2D,
    queue_family_index: u32,
}

impl<'a> RenderContext<'a> {
    /// # Safety
    ///
    /// `device` must be a live logical `ash::Device`, `command_buffer` must be a primary command
    /// buffer on `device` in the recording state, `image` must be a valid 2D `TRANSFER_DST`-capable
    /// image on `device` with dimensions at least `extent` that is idle for recording, and
    /// `queue_family_index` must be the graphics queue family owning `command_buffer`.
    pub(crate) unsafe fn new(
        device: &'a ash::Device,
        command_buffer: vk::CommandBuffer,
        image: vk::Image,
        extent: vk::Extent2D,
        queue_family_index: u32,
    ) -> Self {
        Self { device, command_buffer, image, extent, queue_family_index }
    }

    #[inline]
    pub(crate) fn device(&self) -> &'a ash::Device {
        self.device
    }

    #[inline]
    pub(crate) fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    #[inline]
    pub(crate) fn image(&self) -> vk::Image {
        self.image
    }

    #[inline]
    pub(crate) fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    #[inline]
    pub(crate) fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }
}

/// Finds the first memory type index in `mem_props` matching `type_bits` and `required_flags`.
pub(crate) fn find_memory_type_index(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required_flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        (type_bits & (1 << i)) != 0
            && mem_props.memory_types[i as usize].property_flags.contains(required_flags)
    })
}

#[cfg(test)]
unsafe extern "system" fn vulkan_debug_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    p_user_data: *mut std::ffi::c_void,
) -> vk::Bool32 {
    if !p_callback_data.is_null() {
        // SAFETY: `p_callback_data` is valid for the duration of the callback per
        // `VK_EXT_debug_utils`.
        let data = unsafe { &*p_callback_data };
        if !data.p_message.is_null() {
            // SAFETY: `p_message` is a NUL-terminated UTF-8 string provided by the loader/layers.
            let msg = unsafe { CStr::from_ptr(data.p_message) }.to_string_lossy();
            eprintln!("[VulkanValidation] {msg}");
        }
    }
    if message_severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR)
        && !p_user_data.is_null()
    {
        // SAFETY: `p_user_data` points to the `AtomicUsize` inside `_validation_errors`, which
        // outlives `destroy_debug_utils_messenger()` (or is leaked if the device is abandoned).
        let error_count = unsafe { &*(p_user_data as *const AtomicUsize) };
        error_count.fetch_add(1, Ordering::SeqCst);
    }
    vk::FALSE
}

/// Owns the Vulkan instance and logical device shared between a renderer and its swapchain.
///
/// # Drop Order Contract
///
/// Child GPU objects hold an `Rc<VulkanDevice>` so the logical device and instance are destroyed
/// last, and are left intact if a shutdown fence wait times out with GPU work still in flight.
/// Note that `Rc<VulkanDevice>` orders only `vkDestroyDevice()` / `vkDestroyInstance()` relative to
/// child handles: any external Vulkan resource referenced by recorded swapchain command buffers
/// (such as [`crate::debug_font::DebugFont`]) must still be dropped *after*
/// [`crate::swapchain::Swapchain`] waits for those command buffers to complete.
pub(crate) struct VulkanDevice {
    _entry: ash::Entry,
    instance: ash::Instance,
    #[cfg(test)]
    debug_messenger: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    #[cfg(test)]
    _validation_errors: Option<Arc<AtomicUsize>>,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family_index: u32,
    abandoned: Cell<bool>,
}

impl Drop for VulkanDevice {
    fn drop(&mut self) {
        if self.abandoned.get() {
            #[cfg(test)]
            if let Some(errors) = self._validation_errors.take() {
                // Keep the `AtomicUsize` allocation alive for the process lifetime alongside the
                // intentionally leaked `vk::DebugUtilsMessengerEXT`.
                std::mem::forget(errors);
            }
            return;
        }
        // SAFETY: All child Vulkan resources (`DebugFont`, `Swapchain`) hold an `Rc<Self>` and
        // drop before `VulkanDevice::drop()` executes, and `Drop` runs at most once. When
        // validation layers are active in tests, `debug_messenger` is destroyed before `instance`
        // and before `_validation_errors` drops.
        unsafe {
            self.device.destroy_device(None);
            #[cfg(test)]
            if let Some((debug_utils, messenger)) = self.debug_messenger.take() {
                debug_utils.destroy_debug_utils_messenger(messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

impl VulkanDevice {
    /// Creates a Vulkan 1.1 instance and logical device on the first graphics queue family whose
    /// `required_format` supports `required_format_features`.
    pub(crate) fn new(
        app_name: &CStr,
        required_format: vk::Format,
        required_format_features: vk::FormatFeatureFlags,
    ) -> Result<Rc<Self>, Error> {
        ftrace::duration!(c"gfx", c"VulkanDevice::new");
        // Fuchsia's `libvulkan.so` loader defers connecting to `fuchsia.vulkan.loader.Loader` and
        // loading an ICD until `vkCreateInstance()` runs below.
        let entry = ash::Entry::linked();
        let instance = Self::create_instance(&entry, app_name)?;
        Self::finish_init(
            entry,
            instance,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
            required_format,
            required_format_features,
        )
    }

    /// Creates a production [`VulkanDevice`] with `VK_LAYER_KHRONOS_validation` and
    /// `VK_EXT_debug_utils` enabled, returning the shared device and an error counter incremented
    /// whenever the validation layer emits a `VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT`
    /// message.
    #[cfg(test)]
    // Exercised by `diagnostic_overlay_vulkan_test` (`src/vulkan_tests.rs`), not
    // `diagnostic_overlay_test` (`src/lib.rs`).
    #[allow(dead_code)]
    pub(crate) fn new_with_validation() -> Result<(Rc<Self>, Arc<AtomicUsize>), Error> {
        let entry = ash::Entry::linked();
        // SAFETY: `entry` is valid and linked.
        let available_layers = unsafe { entry.enumerate_instance_layer_properties() }
            .map_err(|e| anyhow!("enumerate_instance_layer_properties failed: {e:?}"))?;
        let has_validation = available_layers.iter().any(|layer| {
            // SAFETY: `layer_name` is a NUL-terminated fixed-size C char array.
            unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) == VALIDATION_LAYER_NAME }
        });
        ensure!(
            has_validation,
            "VK_LAYER_KHRONOS_validation must be packaged in diagnostic_overlay_vulkan_tests"
        );

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"diagnostic_overlay_vulkan_tests")
            .application_version(vk::make_api_version(0, 1, 0, 0))
            .engine_name(c"escher_debug_font_rs")
            .engine_version(vk::make_api_version(0, 1, 0, 0))
            .api_version(VK_API_VERSION);

        let layer_ptrs: [*const c_char; 1] = [VALIDATION_LAYER_NAME.as_ptr()];
        let ext_names: [*const c_char; 3] = [
            vk::KHR_EXTERNAL_MEMORY_CAPABILITIES_NAME.as_ptr(),
            vk::KHR_EXTERNAL_SEMAPHORE_CAPABILITIES_NAME.as_ptr(),
            vk::EXT_DEBUG_UTILS_NAME.as_ptr(),
        ];

        let validation_errors = Arc::new(AtomicUsize::new(0));
        let mut messenger_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                    | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(vulkan_debug_callback))
            .user_data(Arc::as_ptr(&validation_errors).cast_mut().cast());

        let enabled_validation_features =
            [vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION];
        let mut validation_features = vk::ValidationFeaturesEXT::default()
            .enabled_validation_features(&enabled_validation_features);
        let create_info = vk::InstanceCreateInfo::default()
            .push_next(&mut messenger_info)
            .push_next(&mut validation_features)
            .application_info(&app_info)
            .enabled_layer_names(&layer_ptrs)
            .enabled_extension_names(&ext_names);

        // SAFETY: `create_info` and all referenced pointers/strings remain valid across
        // `create_instance()`.
        let instance = unsafe { entry.create_instance(&create_info, None) }
            .map_err(|e| anyhow!("vkCreateInstance with validation failed: {e:?}"))?;
        let debug_utils = ash::ext::debug_utils::Instance::new(&entry, &instance);
        // SAFETY: `instance` is valid and `messenger_info.user_data` points to `validation_errors`,
        // which is retained in `_validation_errors` until after `destroy_debug_utils_messenger()`.
        let messenger =
            match unsafe { debug_utils.create_debug_utils_messenger(&messenger_info, None) } {
                Ok(messenger) => messenger,
                Err(e) => {
                    // SAFETY: `instance` has no child objects yet.
                    unsafe {
                        instance.destroy_instance(None);
                    }
                    return Err(anyhow!("create_debug_utils_messenger failed: {e:?}"));
                }
            };

        const {
            assert!(SWAPCHAIN_FORMAT.as_raw() == ATLAS_FORMAT.as_raw());
        }
        let required_features = SWAPCHAIN_FORMAT_FEATURES | ATLAS_FORMAT_FEATURES;
        let device = Self::finish_init(
            entry,
            instance,
            Some((debug_utils, messenger)),
            Some(Arc::clone(&validation_errors)),
            SWAPCHAIN_FORMAT,
            required_features,
        )?;
        Ok((device, validation_errors))
    }

    fn finish_init(
        entry: ash::Entry,
        instance: ash::Instance,
        #[cfg(test)] debug_messenger: Option<(
            ash::ext::debug_utils::Instance,
            vk::DebugUtilsMessengerEXT,
        )>,
        #[cfg(test)] validation_errors: Option<Arc<AtomicUsize>>,
        required_format: vk::Format,
        required_format_features: vk::FormatFeatureFlags,
    ) -> Result<Rc<Self>, Error> {
        let cleanup_instance = |instance: &ash::Instance,
                                #[cfg(test)] mut debug_messenger: Option<(
            ash::ext::debug_utils::Instance,
            vk::DebugUtilsMessengerEXT,
        )>| {
            // SAFETY: Called only when initialization fails before a logical `vk::Device` is
            // created, destroying any debug messenger before destroying `instance`.
            unsafe {
                #[cfg(test)]
                if let Some((debug_utils, messenger)) = debug_messenger.take() {
                    debug_utils.destroy_debug_utils_messenger(messenger, None);
                }
                instance.destroy_instance(None);
            }
        };

        let (physical_device, queue_family_index) = match Self::find_suitable_physical_device(
            &instance,
            required_format,
            required_format_features,
        ) {
            Ok(found) => found,
            Err(e) => {
                cleanup_instance(
                    &instance,
                    #[cfg(test)]
                    debug_messenger,
                );
                return Err(e);
            }
        };
        let (device, queue) =
            match Self::create_device(&instance, physical_device, queue_family_index) {
                Ok(created) => created,
                Err(e) => {
                    cleanup_instance(
                        &instance,
                        #[cfg(test)]
                        debug_messenger,
                    );
                    return Err(e);
                }
            };

        Ok(Rc::new(Self {
            _entry: entry,
            instance,
            #[cfg(test)]
            debug_messenger,
            #[cfg(test)]
            _validation_errors: validation_errors,
            device,
            physical_device,
            queue,
            queue_family_index,
            abandoned: Cell::new(false),
        }))
    }

    pub(crate) fn instance(&self) -> &ash::Instance {
        &self.instance
    }

    pub(crate) fn device(&self) -> &ash::Device {
        &self.device
    }

    pub(crate) fn queue(&self) -> vk::Queue {
        self.queue
    }

    pub(crate) fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }

    pub(crate) fn physical_device_memory_properties(&self) -> vk::PhysicalDeviceMemoryProperties {
        // SAFETY: `self.instance` and `self.physical_device` remain valid for `self`'s lifetime.
        unsafe { self.instance.get_physical_device_memory_properties(self.physical_device) }
    }

    fn mark_abandoned(&self) {
        self.abandoned.set(true);
    }

    #[cfg(test)]
    // Exercised by `diagnostic_overlay_vulkan_test` (`src/vulkan_tests.rs`), not
    // `diagnostic_overlay_test` (`src/lib.rs`).
    #[allow(dead_code)]
    pub(crate) fn mark_abandoned_for_testing(&self) {
        self.mark_abandoned();
    }

    pub(crate) fn is_abandoned(&self) -> bool {
        self.abandoned.get()
    }

    /// # Safety
    ///
    /// Every fence in `fences` must be a valid `vk::Fence` created on `self.device` and remain
    /// alive for the duration of the wait.
    pub(crate) unsafe fn wait_for_fences(&self, fences: &[vk::Fence]) -> Result<(), vk::Result> {
        // SAFETY: Caller guarantees every fence in `fences` is a valid `vk::Fence` created on
        // `self.device` and remains alive for the duration of the wait.
        unsafe { self.wait_for_fences_with_timeout(fences, FENCE_WAIT_TIMEOUT_NS) }
    }

    /// # Safety
    ///
    /// Every fence in `fences` must be a valid `vk::Fence` created on `self.device` and remain
    /// alive for the duration of the wait.
    pub(crate) unsafe fn wait_for_fences_with_timeout(
        &self,
        fences: &[vk::Fence],
        timeout_ns: u64,
    ) -> Result<(), vk::Result> {
        if self.is_abandoned() {
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        if fences.is_empty() {
            return Ok(());
        }
        // SAFETY: Caller guarantees every fence in `fences` is a valid `vk::Fence` created on
        // `self.device` and remains alive for the duration of the wait.
        let res = unsafe { self.device.wait_for_fences(fences, true, timeout_ns) };
        if let Err(e) = res {
            warn!(
                "Timed out or failed waiting for Vulkan fences ({e:?}); \
                 abandoning Vulkan device to avoid destroying in-flight GPU work"
            );
            self.mark_abandoned();
        }
        res
    }

    /// Executes `f` with a recording primary command buffer, submits it to the graphics queue,
    /// and waits for completion before destroying the temporary pool and fence.
    ///
    /// If the fence wait times out or fails, [`VulkanDevice`] is marked abandoned and the
    /// in-flight fence, command buffer, and pool are left intact so live GPU work is never
    /// destroyed.
    pub(crate) fn run_one_shot<R>(
        &self,
        f: impl FnOnce(&ash::Device, vk::CommandBuffer) -> Result<R, Error>,
    ) -> Result<R, Error> {
        ensure!(!self.is_abandoned(), "Vulkan device is abandoned");
        let (pool, cbs) = self.create_command_buffers(1)?;
        let mut guard = OneShotGuard {
            device: &self.device,
            pool,
            cb: cbs[0],
            fence: vk::Fence::null(),
            active: true,
        };

        let fence_info = vk::FenceCreateInfo::default();
        // SAFETY: `self.device` is live and `fence_info` has no `p_next` chain.
        guard.fence = unsafe { self.device.create_fence(&fence_info, None) }
            .map_err(|e| anyhow!("create Vulkan one-shot fence failed: {e:?}"))?;

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: `guard.cb` is a newly allocated primary command buffer in the initial state.
        unsafe {
            self.device
                .begin_command_buffer(guard.cb, &begin_info)
                .map_err(|e| anyhow!("begin one-shot command buffer failed: {e:?}"))?;
        }

        let result = f(&self.device, guard.cb)?;

        // SAFETY: `guard.cb` is in the recording state and `f` returned `Ok`.
        unsafe {
            self.device
                .end_command_buffer(guard.cb)
                .map_err(|e| anyhow!("end one-shot command buffer failed: {e:?}"))?;
        }

        let cbs_to_submit = [guard.cb];
        let submit_info = [vk::SubmitInfo::default().command_buffers(&cbs_to_submit)];
        // SAFETY: `guard.cb` is executable, `self.queue` belongs to `self.device`, and
        // `guard.fence` is an unsignaled fence on `self.device`.
        unsafe {
            self.device
                .queue_submit(self.queue, &submit_info, guard.fence)
                .map_err(|e| anyhow!("submit one-shot command buffer failed: {e:?}"))?;
        }

        // Disarm cleanup while waiting so a fence timeout leaves the in-flight fence, command
        // buffer, and pool intact alongside the abandoned device.
        guard.active = false;
        // SAFETY: `guard.fence` was created on `self.device` above and outlives the wait.
        unsafe { self.wait_for_fences(&[guard.fence]) }
            .map_err(|e| anyhow!("wait for one-shot fence failed: {e:?}"))?;
        guard.active = true;

        Ok(result)
    }

    fn create_instance(entry: &ash::Entry, app_name: &CStr) -> Result<ash::Instance, Error> {
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(vk::make_api_version(0, 1, 0, 0))
            .engine_name(c"escher_debug_font_rs")
            .engine_version(vk::make_api_version(0, 1, 0, 0))
            .api_version(VK_API_VERSION);

        let ext_names: [*const c_char; 2] = [
            vk::KHR_EXTERNAL_MEMORY_CAPABILITIES_NAME.as_ptr(),
            vk::KHR_EXTERNAL_SEMAPHORE_CAPABILITIES_NAME.as_ptr(),
        ];

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_names);

        // SAFETY: `app_info` and `ext_names` remain valid for `create_instance()`, and all
        // extension names are static NUL-terminated C strings.
        unsafe {
            entry
                .create_instance(&create_info, None)
                .map_err(|e| anyhow!("vkCreateInstance failed: {e:?}"))
        }
    }

    fn find_suitable_physical_device(
        instance: &ash::Instance,
        required_format: vk::Format,
        required_features: vk::FormatFeatureFlags,
    ) -> Result<(vk::PhysicalDevice, u32), Error> {
        // SAFETY: `instance` is a live `ash::Instance` created by `create_instance()`.
        let physical_devices = unsafe {
            instance
                .enumerate_physical_devices()
                .map_err(|e| anyhow!("vkEnumeratePhysicalDevices failed: {e:?}"))?
        };

        for &pd in &physical_devices {
            // SAFETY: `pd` was returned by `enumerate_physical_devices()` on `instance`.
            let queue_families =
                unsafe { instance.get_physical_device_queue_family_properties(pd) };
            let Some(qfi) = queue_families
                .iter()
                .position(|qf| {
                    qf.queue_count > 0 && qf.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                })
                .map(|idx| idx as u32)
            else {
                continue;
            };

            // SAFETY: `pd` is a valid physical device on `instance`.
            let fmt_props =
                unsafe { instance.get_physical_device_format_properties(pd, required_format) };
            if fmt_props.optimal_tiling_features.contains(required_features) {
                return Ok((pd, qfi));
            }
        }

        Err(anyhow!("No suitable Vulkan physical device found"))
    }

    fn create_device(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
    ) -> Result<(ash::Device, vk::Queue), Error> {
        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities)];

        let ext_names: [*const c_char; 6] = [
            vk::EXT_QUEUE_FAMILY_FOREIGN_NAME.as_ptr(),
            vk::FUCHSIA_BUFFER_COLLECTION_NAME.as_ptr(),
            vk::FUCHSIA_EXTERNAL_MEMORY_NAME.as_ptr(),
            vk::FUCHSIA_EXTERNAL_SEMAPHORE_NAME.as_ptr(),
            vk::KHR_EXTERNAL_MEMORY_NAME.as_ptr(),
            vk::KHR_EXTERNAL_SEMAPHORE_NAME.as_ptr(),
        ];

        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&ext_names);

        // SAFETY: `physical_device` is a valid device on `instance`, `queue_family_index` was
        // validated in `find_suitable_physical_device()`, and `queue_info`/`ext_names` outlive the
        // call.
        let device = unsafe {
            instance
                .create_device(physical_device, &create_info, None)
                .map_err(|e| anyhow!("vkCreateDevice failed: {e:?}"))?
        };
        // SAFETY: `device` was just created with 1 queue at `(queue_family_index, 0)`.
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        Ok((device, queue))
    }

    pub(crate) fn create_command_buffers(
        &self,
        count: u32,
    ) -> Result<(vk::CommandPool, Vec<vk::CommandBuffer>), Error> {
        ensure!(!self.is_abandoned(), "cannot create command buffers on abandoned VulkanDevice");
        ensure!(count > 0, "command buffer count must be non-zero");
        let pool_info =
            vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family_index).flags(
                vk::CommandPoolCreateFlags::TRANSIENT
                    | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
            );
        // SAFETY: `self.device` is a live `ash::Device` and `self.queue_family_index` is valid on
        // `self.device`.
        let pool = unsafe {
            self.device
                .create_command_pool(&pool_info, None)
                .map_err(|e| anyhow!("vkCreateCommandPool failed: {e:?}"))?
        };
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count);
        // SAFETY: `pool` is a valid command pool on `self.device` and `count > 0`.
        let cbs = match unsafe { self.device.allocate_command_buffers(&alloc_info) } {
            Ok(cbs) => cbs,
            Err(e) => {
                // SAFETY: `pool` has no allocated command buffers because allocation failed.
                unsafe {
                    self.device.destroy_command_pool(pool, None);
                }
                return Err(anyhow!("vkAllocateCommandBuffers failed: {e:?}"));
            }
        };
        Ok((pool, cbs))
    }
}

struct OneShotGuard<'a> {
    device: &'a ash::Device,
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    fence: vk::Fence,
    active: bool,
}

impl Drop for OneShotGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // SAFETY: `OneShotGuard` only drops while active before `queue_submit()` succeeds or
        // after `wait_for_fences()` confirms the one-shot submission has completed.
        unsafe {
            if self.fence != vk::Fence::null() {
                self.device.destroy_fence(self.fence, None);
            }
            self.device.free_command_buffers(self.pool, &[self.cb]);
            self.device.destroy_command_pool(self.pool, None);
        }
    }
}
