// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::InputDevice;
use fidl::endpoints::Proxy;
use fidl_fuchsia_input_report as fir;
use fidl_fuchsia_ui_input_internal as fuii;
use fuchsia_async::{self as fasync, TimeoutExt as _};
use futures::future::{AbortHandle, BoxFuture, abortable};
use futures::{FutureExt as _, StreamExt as _};
use starnix_core::device::kobject::Device;
use starnix_core::task::Kernel;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_logging::{log_debug, log_warn};
use starnix_modules_input_report_event_conversion::InputReportConverter;
use starnix_sync::{
    DirectInputConverterLock, DirectInputReservedMinorsLock, DirectInputStateLock, LockDepMutex,
};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// Maximum unacknowledged reports passed to `InputReportsReaderV2`.
pub const MAX_UNACKNOWLEDGED_REPORTS_LIMIT: u16 = 120;

/// Signal bit corresponding to `InputOwnershipSignal.DISPLAY_UNOWNED` (ZX_USER_SIGNAL_0).
pub(crate) const DISPLAY_UNOWNED_SIGNAL: zx::Signals = zx::Signals::USER_0;

/// Signal bit corresponding to `InputOwnershipSignal.DISPLAY_PLATFORM_OWNED` (ZX_USER_SIGNAL_1).
pub(crate) const DISPLAY_PLATFORM_OWNED_SIGNAL: zx::Signals = zx::Signals::USER_1;

/// Signal bit corresponding to `InputOwnershipSignal.INPUT_CLIENT_OWNED` (ZX_USER_SIGNAL_3).
pub(crate) const INPUT_CLIENT_OWNED_SIGNAL: zx::Signals = zx::Signals::USER_3;

/// Signal bit corresponding to `InputOwnershipSignal.INPUT_PLATFORM_OWNED` (ZX_USER_SIGNAL_4).
pub(crate) const INPUT_PLATFORM_OWNED_SIGNAL: zx::Signals = zx::Signals::USER_4;

/// Combined signals required for Starnix direct input ownership.
pub(crate) const STARNIX_DIRECT_OWNERSHIP_SIGNALS: zx::Signals = zx::Signals::from_bits_truncate(
    DISPLAY_PLATFORM_OWNED_SIGNAL.bits() | INPUT_CLIENT_OWNED_SIGNAL.bits(),
);

/// Signals that disqualify Starnix direct input ownership if asserted.
pub(crate) const DISQUALIFYING_SIGNALS: zx::Signals = zx::Signals::from_bits_truncate(
    DISPLAY_UNOWNED_SIGNAL.bits() | INPUT_PLATFORM_OWNED_SIGNAL.bits(),
);

/// Combined mask of all signals governing display and input ownership.
pub(crate) const ALL_OWNERSHIP_SIGNALS: zx::Signals = zx::Signals::from_bits_truncate(
    DISPLAY_UNOWNED_SIGNAL.bits()
        | DISPLAY_PLATFORM_OWNED_SIGNAL.bits()
        | INPUT_CLIENT_OWNED_SIGNAL.bits()
        | INPUT_PLATFORM_OWNED_SIGNAL.bits(),
);

/// Evaluates whether the given signal set grants Starnix direct input ownership.
///
/// Under the 4-bit orthogonal ownership model:
/// - Starnix direct input is active iff both `DISPLAY_PLATFORM_OWNED` (Bit 1) and
///   `INPUT_CLIENT_OWNED` (Bit 3) are asserted, AND neither `DISPLAY_UNOWNED` (Bit 0)
///   nor `INPUT_PLATFORM_OWNED` (Bit 4) is asserted.
pub(crate) fn evaluate_ownership(signals: zx::Signals) -> bool {
    let has_required = (signals.bits() & STARNIX_DIRECT_OWNERSHIP_SIGNALS.bits())
        == STARNIX_DIRECT_OWNERSHIP_SIGNALS.bits();
    let has_disqualifying = (signals.bits() & DISQUALIFYING_SIGNALS.bits()) != 0;
    has_required && !has_disqualifying
}

/// Helper for extracting currently asserted signals on a `zx::Event` without blocking.
pub(crate) fn current_signals(event: &zx::Event) -> zx::Signals {
    match event.wait_one(zx::Signals::NONE, zx::MonotonicInstant::INFINITE_PAST) {
        zx::WaitResult::Ok(s) | zx::WaitResult::TimedOut(s) | zx::WaitResult::Canceled(s) => s,
        zx::WaitResult::Err(_) => zx::Signals::NONE,
    }
}

/// Trait abstracting button shortcut processing and action execution for dependency injection.
pub trait ButtonShortcutProcessor: Send + Sync {
    fn process_button_state(&self, volume_up: bool, volume_down: bool, power: bool) -> bool;
    fn trigger_action(self: Arc<Self>) -> BoxFuture<'static, bool>;
}

/// Represents an active or registered direct input device managed by [`DirectInputManager`].
#[derive(Clone)]
pub(crate) struct DirectInputDevice {
    pub(crate) device_id: u32,
    pub(crate) instance_name: String,
    pub(crate) minor: u32,
    pub(crate) input_device: InputDevice,
    pub(crate) starnix_device: Device,
    pub(crate) device_proxy: fir::InputDeviceProxy,
    pub(crate) converter: Arc<LockDepMutex<InputReportConverter, DirectInputConverterLock>>,
}

impl std::fmt::Debug for DirectInputDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectInputDevice")
            .field("device_id", &self.device_id)
            .field("instance_name", &self.instance_name)
            .field("minor", &self.minor)
            .field("starnix_device", &self.starnix_device)
            .finish()
    }
}

struct ReservedMinorGuard<'a> {
    reserved_minors: &'a LockDepMutex<HashSet<u32>, DirectInputReservedMinorsLock>,
    minor: u32,
}

impl Drop for ReservedMinorGuard<'_> {
    fn drop(&mut self) {
        self.reserved_minors.lock().remove(&self.minor);
    }
}

#[derive(Default)]
struct DirectInputState {
    is_paused: bool,
    devices: HashMap<u32, DirectInputDevice>,
    abort_handles: HashMap<u32, AbortHandle>,
    watcher_abort_handles: HashMap<u32, AbortHandle>,
}

/// Manages direct hardware input report reading, device lifecycle, shortcut detection,
/// and runtime ownership negotiation via `fuchsia.ui.input.internal.InputOwnership`.
pub struct DirectInputManager {
    kernel: Weak<Kernel>,
    shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
    ownership_event: OnceLock<zx::Event>,
    ownership_active: Arc<AtomicBool>,
    resume_generation: AtomicU64,
    next_device_id: AtomicU32,
    state: LockDepMutex<DirectInputState, DirectInputStateLock>,
    reserved_minors: LockDepMutex<HashSet<u32>, DirectInputReservedMinorsLock>,
    scope: fasync::Scope,
}

impl std::fmt::Debug for DirectInputManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectInputManager")
            .field("ownership_active", &self.ownership_active.load(Ordering::SeqCst))
            .field("devices", &self.state.lock().devices.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Drop for DirectInputManager {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        for (_, handle) in state.abort_handles.drain() {
            handle.abort();
        }
        for (_, handle) in state.watcher_abort_handles.drain() {
            handle.abort();
        }
        if let Some(kernel) = self.kernel.upgrade() {
            let devices = std::mem::take(&mut state.devices);
            for (_, device) in devices {
                unregister_device(&kernel, device.starnix_device);
            }
        }
    }
}

impl DirectInputManager {
    /// Creates a new `DirectInputManager`.
    ///
    /// If `ownership_event` is provided, spawns an asynchronous listener to dynamically
    /// observe ownership transitions (pause on lost, resume on acquired).
    /// If `ownership_event` is `None`, direct input defaults to inactive.
    pub fn new(
        kernel: Weak<Kernel>,
        shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
        ownership_event: Option<zx::Event>,
    ) -> Arc<Self> {
        Self::new_with_channel(kernel, shortcut_handler, ownership_event, None)
    }

    /// Creates a new `DirectInputManager` optionally monitoring an `ownership_channel` for
    /// `PEER_CLOSED` alongside the `ownership_event`.
    pub fn new_with_channel(
        kernel: Weak<Kernel>,
        shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
        ownership_event: Option<zx::Event>,
        ownership_channel: Option<zx::Channel>,
    ) -> Arc<Self> {
        let initial_active = match &ownership_event {
            Some(event) => evaluate_ownership(current_signals(event)),
            None => false,
        };

        let ownership_event_lock = OnceLock::new();
        if let Some(event) = ownership_event.as_ref() {
            match event.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                Ok(dup) => {
                    let _ = ownership_event_lock.set(dup);
                }
                Err(status) => {
                    log_warn!("Failed to duplicate ownership event handle: {:?}", status);
                }
            }
        }

        let manager = Arc::new(Self {
            kernel,
            shortcut_handler,
            ownership_event: ownership_event_lock,
            ownership_active: Arc::new(AtomicBool::new(initial_active)),
            resume_generation: AtomicU64::new(if initial_active { 1 } else { 0 }),
            next_device_id: AtomicU32::new(1),
            state: LockDepMutex::new(DirectInputState {
                is_paused: !initial_active,
                devices: HashMap::new(),
                abort_handles: HashMap::new(),
                watcher_abort_handles: HashMap::new(),
            }),
            reserved_minors: LockDepMutex::new(HashSet::new()),
            scope: fasync::Scope::new(),
        });

        if let Some(event) = ownership_event {
            let this = Arc::downgrade(&manager);
            manager.scope.spawn(async move {
                Self::run_ownership_listener(event, ownership_channel, this).await;
            });
        }

        manager
    }

    /// Creates a new `DirectInputManager` and initializes ownership monitoring using the provided
    /// `ownership_proxy`.
    ///
    /// If `ownership_proxy` is `Some`, calls `GetEvent(InputOwnershipTarget::Platform(Platform {}))`,
    /// duplicates and stores the event handle for `sync_ownership()`, and spawns an asynchronous
    /// listener task on the manager's scope. When direct input ownership is acquired, direct reading
    /// resumes and `ownership_active` is asserted (suppressing app-protocol relays); when ownership
    /// is lost, direct reading pauses and `ownership_active` is cleared (falling back to the legacy
    /// app protocol).
    ///
    /// If `ownership_proxy` is `None` or fails to retrieve the ownership event, ownership defaults to
    /// inactive, falling back to the legacy app protocol.
    pub fn new_with_ownership_proxy(
        kernel: Weak<Kernel>,
        shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
        ownership_proxy: Option<fuii::InputOwnershipProxy>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            kernel,
            shortcut_handler,
            ownership_event: OnceLock::new(),
            ownership_active: Arc::new(AtomicBool::new(false)),
            resume_generation: AtomicU64::new(0),
            next_device_id: AtomicU32::new(1),
            state: LockDepMutex::new(DirectInputState {
                is_paused: true,
                devices: HashMap::new(),
                abort_handles: HashMap::new(),
                watcher_abort_handles: HashMap::new(),
            }),
            reserved_minors: LockDepMutex::new(HashSet::new()),
            scope: fasync::Scope::new(),
        });

        if let Some(proxy) = ownership_proxy {
            let this = Arc::downgrade(&manager);
            manager.scope.spawn(async move {
                match query_ownership_event(
                    &proxy,
                    fuii::InputOwnershipTarget::Platform(fuii::Platform {}),
                )
                .await
                {
                    Ok(event) => {
                        if let Ok(dup) = event.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                            if let Some(m) = this.upgrade() {
                                let _ = m.ownership_event.set(dup);
                            }
                        }
                        let ownership_channel =
                            proxy.into_channel().ok().map(|c| c.into_zx_channel());
                        Self::run_ownership_listener(event, ownership_channel, this).await;
                    }
                    Err(e) => {
                        log_debug!(
                            "Failed to query ownership event from InputOwnership: {:?}; falling back to app protocol",
                            e
                        );
                    }
                }
            });
        }

        manager
    }

    /// Connects to `fuchsia.ui.input.internal.InputOwnership` from the incoming namespace and creates
    /// a `DirectInputManager`. Also starts watching driver instances via `start_service_watcher`.
    ///
    /// Direct input devices are registered as they are discovered, even while paused, so that
    /// device nodes are enumerated ahead of time. Their reader loops remain paused until direct
    /// input ownership is acquired.
    ///
    /// If connecting to `InputOwnership` fails or is unavailable, ownership remains inactive, falling
    /// back to the legacy app protocol.
    pub fn connect_and_start(
        kernel: Weak<Kernel>,
        shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
    ) -> Arc<Self> {
        let ownership_proxy =
            fuchsia_component::client::connect_to_protocol::<fuii::InputOwnershipMarker>().ok();
        let manager = Self::new_with_ownership_proxy(kernel, shortcut_handler, ownership_proxy);
        manager.spawn_service_watcher();
        manager
    }

    /// Spawns the driver service watcher on the manager's scope.
    pub fn spawn_service_watcher(self: &Arc<Self>) {
        let this = Arc::downgrade(self);
        self.scope.spawn(async move {
            if let Some(this) = this.upgrade() {
                if let Err(e) = this.start_service_watcher().await {
                    log_debug!("Direct input service watcher stopped: {:?}", e);
                }
            }
        });
    }

    /// Background task monitoring `fuchsia.ui.input.internal.InputOwnership` signals.
    async fn run_ownership_listener(
        event: zx::Event,
        ownership_channel: Option<zx::Channel>,
        manager_weak: Weak<Self>,
    ) {
        let fasync_channel = ownership_channel.map(fasync::Channel::from_channel);
        let channel_closed_fut = match &fasync_channel {
            Some(chan) => futures::future::Either::Left(fasync::OnSignals::new(
                chan.as_ref(),
                zx::Signals::CHANNEL_PEER_CLOSED,
            )),
            None => futures::future::Either::Right(futures::future::pending()),
        }
        .fuse();
        let mut channel_closed_fut = std::pin::pin!(channel_closed_fut);

        loop {
            let wait_signals = {
                let Some(manager) = manager_weak.upgrade() else {
                    break;
                };

                let current = current_signals(&event);
                let is_active = evaluate_ownership(current);
                let prev_active = manager.ownership_active.load(Ordering::SeqCst);

                if is_active != prev_active {
                    if is_active {
                        manager.resume();
                    } else {
                        manager.pause();
                    }
                }

                // Decide which signals to wait for to detect the next transition.
                // Any signal bit currently asserted on `event` is excluded from wait_signals,
                // preventing immediate return and 100% CPU spinning.
                let wait = zx::Signals::from_bits_truncate(
                    ALL_OWNERSHIP_SIGNALS.bits() ^ (current.bits() & ALL_OWNERSHIP_SIGNALS.bits()),
                );

                if wait.is_empty() {
                    log_warn!("All ownership signals asserted simultaneously; stopping listener");
                    manager.pause();
                    break;
                }
                wait
            }; // Arc<DirectInputManager> dropped here before awaiting signals!

            // Await signal transition or server channel closure.
            let mut signal_fut =
                std::pin::pin!(fasync::OnSignals::new(&event, wait_signals).fuse());
            futures::select! {
                signal_res = signal_fut => {
                    match signal_res {
                        Ok(_) => {}
                        Err(e) => {
                            log_warn!("Ownership event wait returned error: {:?}", e);
                            if let Some(manager) = manager_weak.upgrade() {
                                manager.pause();
                            }
                            break;
                        }
                    }
                }
                closed_res = channel_closed_fut => {
                    log_warn!("Ownership channel closed by peer (Scenic crash/restart): {:?}", closed_res);
                    if let Some(manager) = manager_weak.upgrade() {
                        manager.pause();
                    }
                    break;
                }
            }
        }
    }

    /// Returns `true` if Starnix currently holds direct input ownership.
    pub fn is_ownership_active(&self) -> bool {
        self.ownership_active.load(Ordering::SeqCst)
    }

    /// Returns the current generation counter, incremented whenever ownership resumes.
    pub fn resume_generation(&self) -> u64 {
        self.resume_generation.load(Ordering::SeqCst)
    }

    /// Returns `true` if any direct input devices are registered.
    pub fn has_devices(&self) -> bool {
        !self.state.lock().devices.is_empty()
    }

    /// Pauses all direct reader loops, marks direct input ownership inactive,
    /// and flushes synthetic release events for any active touches/buttons.
    pub(crate) fn pause(&self) {
        let mut state = self.state.lock();
        if state.is_paused {
            return;
        }
        state.is_paused = true;
        self.ownership_active.store(false, Ordering::SeqCst);
        for (_, handle) in state.abort_handles.drain() {
            handle.abort();
        }

        // Flush releases for all registered direct input devices to avoid stuck contacts/buttons.
        let empty_report = fir::InputReport {
            event_time: Some(zx::MonotonicInstant::get().into_nanos()),
            touch: Some(fir::TouchInputReport { contacts: Some(vec![]), ..Default::default() }),
            mouse: Some(fir::MouseInputReport {
                pressed_buttons: Some(vec![]),
                ..Default::default()
            }),
            keyboard: Some(fir::KeyboardInputReport {
                pressed_keys3: Some(vec![]),
                ..Default::default()
            }),
            consumer_control: Some(fir::ConsumerControlInputReport {
                pressed_buttons: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        for device in state.devices.values() {
            let mut conv = device.converter.lock();
            conv.paused = true;
            let releases = conv.handle_report(&empty_report);
            if !releases.is_empty() {
                device.input_device.dispatch_events(&releases);
            }
        }
    }

    /// Synchronously resumes all direct reader loops and marks direct input ownership active.
    pub(crate) fn resume(self: &Arc<Self>) {
        let mut state = self.state.lock();
        if !state.is_paused {
            return;
        }
        state.is_paused = false;
        self.ownership_active.store(true, Ordering::SeqCst);
        self.resume_generation.fetch_add(1, Ordering::SeqCst);

        for device in state.devices.values() {
            let mut conv = device.converter.lock();
            conv.paused = false;
        }

        // Reader tasks are only spawned for currently existing devices,
        // and duplicate tasks are never spawned if already active.
        let to_spawn: Vec<(
            u32,
            fir::InputDeviceProxy,
            InputDevice,
            Arc<LockDepMutex<InputReportConverter, DirectInputConverterLock>>,
        )> = state
            .devices
            .iter()
            .filter(|(id, _)| !state.abort_handles.contains_key(id))
            .map(|(id, dev)| {
                (*id, dev.device_proxy.clone(), dev.input_device.clone(), dev.converter.clone())
            })
            .collect();

        for (device_id, device_proxy, input_device, converter) in to_spawn {
            let handle = self.spawn_reader_task(device_id, device_proxy, input_device, converter);
            state.abort_handles.insert(device_id, handle);
        }
    }

    fn update_ownership_active(self: &Arc<Self>, active: bool) {
        if active {
            self.resume();
        } else {
            self.pause();
        }
    }

    /// Explicitly updates ownership state (useful in test harnesses).
    #[cfg(test)]
    pub fn set_ownership_active(self: &Arc<Self>, active: bool) {
        self.update_ownership_active(active);
    }

    /// Synchronizes ownership status with the underlying `ownership_event` (if available, either
    /// passed at construction or retrieved via `InputOwnership`).
    pub fn sync_ownership(self: &Arc<Self>) {
        if let Some(event) = self.ownership_event.get() {
            let active = evaluate_ownership(current_signals(event));
            self.update_ownership_active(active);
        }
    }

    /// Registers a discovered input driver instance, parses its descriptor (with 5s timeout),
    /// registers a character device in the kernel, and starts a reader loop if ownership is active.
    pub async fn add_device(
        self: &Arc<Self>,
        device_proxy: fir::InputDeviceProxy,
        instance_name: Option<String>,
    ) -> Result<u32, Errno> {
        let descriptor = device_proxy
            .get_descriptor()
            .on_timeout(
                fasync::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(5)),
                || {
                    Err(fidl::Error::ClientChannelClosed {
                        epitaph: zx::Status::TIMED_OUT.into(),
                        protocol_name: "InputDevice",
                    })
                },
            )
            .await
            .map_err(|e| {
                log_warn!("Failed getting descriptor: {:?}", e);
                errno!(EIO)
            })?;
        self.add_device_with_descriptor(device_proxy, instance_name, descriptor).await
    }

    /// Registers an input driver instance with a pre-fetched descriptor.
    pub async fn add_device_with_descriptor(
        self: &Arc<Self>,
        device_proxy: fir::InputDeviceProxy,
        instance_name: Option<String>,
        descriptor: fir::DeviceDescriptor,
    ) -> Result<u32, Errno> {
        if descriptor.touch.is_none()
            && descriptor.keyboard.is_none()
            && descriptor.mouse.is_none()
            && descriptor.consumer_control.is_none()
        {
            log_warn!(
                "Ignoring unsupported direct input device {:?}: descriptor has no touch, keyboard, mouse, or consumer_control capabilities",
                instance_name
            );
            return Err(errno!(ENOTSUP));
        }

        let device_id = self.next_device_id.fetch_add(1, Ordering::Relaxed);
        let name = instance_name.unwrap_or_else(|| format!("direct_input_{device_id}"));

        let kernel = self.kernel.upgrade().ok_or_else(|| errno!(ENODEV))?;
        let inspect_node = kernel.inspect_node.clone_weak();

        let input_device =
            InputDevice::from_device_descriptor(&descriptor, None, &name, &inspect_node);

        let (starnix_device, minor, _minor_guard) = loop {
            let candidate_minor = {
                let mut reserved = self.reserved_minors.lock();
                let state = self.state.lock();
                let mut used_minors: HashSet<u32> =
                    state.devices.values().map(|d| d.minor).collect();
                drop(state);
                used_minors.extend(reserved.iter().copied());
                let devt_start = starnix_uapi::device_id::DeviceId::new(
                    starnix_uapi::device_id::INPUT_MAJOR,
                    crate::DIRECT_INPUT_MINOR_RANGE.start,
                );
                let devt_end = starnix_uapi::device_id::DeviceId::new(
                    starnix_uapi::device_id::INPUT_MAJOR,
                    crate::DIRECT_INPUT_MINOR_RANGE.end,
                );
                for (devt, _) in kernel.device_registry.list_minor_devices(
                    starnix_core::device::DeviceMode::Char,
                    devt_start..devt_end,
                ) {
                    used_minors.insert(devt.minor());
                }
                let cand = crate::DIRECT_INPUT_MINOR_RANGE
                    .clone()
                    .find(|m| !used_minors.contains(m))
                    .ok_or_else(|| errno!(ENOSPC))?;
                reserved.insert(cand);
                cand
            };

            match input_device.clone().register(&kernel, candidate_minor) {
                Ok(dev) => {
                    let guard = ReservedMinorGuard {
                        reserved_minors: &self.reserved_minors,
                        minor: candidate_minor,
                    };
                    break (dev, candidate_minor, guard);
                }
                Err(e) => {
                    self.reserved_minors.lock().remove(&candidate_minor);
                    if e == errno!(EEXIST) {
                        continue;
                    }
                    return Err(e);
                }
            }
        };

        let mut conv = InputReportConverter::from_device_descriptor(&descriptor);

        let this_weak = Arc::downgrade(self);
        let watcher_proxy = device_proxy.clone();
        let (watcher_task, watcher_abort_handle) = abortable(async move {
            let _ = watcher_proxy.on_closed().await;
            if let Some(this) = this_weak.upgrade() {
                this.remove_device(device_id);
            }
        });
        {
            let mut state = self.state.lock();
            conv.paused = state.is_paused;
            let converter = Arc::new(LockDepMutex::new(conv));

            let direct_device = DirectInputDevice {
                device_id,
                instance_name: name,
                minor,
                input_device: input_device.clone(),
                starnix_device,
                device_proxy: device_proxy.clone(),
                converter: converter.clone(),
            };

            state.devices.insert(device_id, direct_device);
            state.watcher_abort_handles.insert(device_id, watcher_abort_handle);

            if !state.is_paused {
                let handle =
                    self.spawn_reader_task(device_id, device_proxy, input_device, converter);
                state.abort_handles.insert(device_id, handle);
            }
        }

        self.scope.spawn(async move {
            let _ = watcher_task.await;
        });

        Ok(device_id)
    }

    /// Spawns an asynchronous report reader task for the given device.
    fn spawn_reader_task(
        self: &Arc<Self>,
        device_id: u32,
        device_proxy: fir::InputDeviceProxy,
        input_device: InputDevice,
        converter: Arc<LockDepMutex<InputReportConverter, DirectInputConverterLock>>,
    ) -> AbortHandle {
        let this_weak = Arc::downgrade(self);
        let ownership_active = self.ownership_active.clone();
        let shortcut_handler = self.shortcut_handler.clone();
        let scope_handle = self.scope.to_handle();

        let (abortable_task, abort_handle) = abortable(async move {
            let (v2_client, v2_server) =
                fidl::endpoints::create_endpoints::<fir::InputReportsReaderV2Marker>();
            let v2_result = device_proxy
                .get_input_reports_reader_v2(v2_server, MAX_UNACKNOWLEDGED_REPORTS_LIMIT)
                .await;

            match v2_result {
                Ok(max_unacknowledged_reports) => {
                    let effective_max =
                        max_unacknowledged_reports.min(MAX_UNACKNOWLEDGED_REPORTS_LIMIT);
                    let ack_threshold = ((effective_max as u64 * 7) / 10).max(1);
                    let reader_proxy = v2_client.into_proxy();
                    Self::run_reports_reader_v2(
                        reader_proxy,
                        input_device,
                        converter,
                        ownership_active,
                        shortcut_handler,
                        scope_handle,
                        ack_threshold,
                    )
                    .await;
                }
                Err(e) => {
                    log_warn!(
                        "Device {} does not support InputReportsReaderV2 or failed connecting: {:?}",
                        device_id,
                        e
                    );
                }
            }

            // On driver channel disconnect / termination:
            if let Some(this) = this_weak.upgrade() {
                this.remove_device(device_id);
            }
        });

        self.scope.spawn(async move {
            let _ = abortable_task.await;
        });

        abort_handle
    }

    /// Asynchronous report processing loop for `fuchsia.input.report.InputReportsReaderV2`.
    async fn run_reports_reader_v2(
        reader_proxy: fir::InputReportsReaderV2Proxy,
        input_device: InputDevice,
        converter: Arc<LockDepMutex<InputReportConverter, DirectInputConverterLock>>,
        ownership_active: Arc<AtomicBool>,
        shortcut_handler: Option<Arc<dyn ButtonShortcutProcessor>>,
        scope: fasync::ScopeHandle,
        ack_threshold: u64,
    ) {
        let mut event_stream = reader_proxy.take_event_stream();
        let mut last_acknowledged_stamp: u64 = 0;

        while let Some(event) = event_stream.next().await {
            match event {
                Ok(fir::InputReportsReaderV2Event::OnInputReports {
                    reports,
                    last_report_stamp,
                }) => {
                    if ownership_active.load(Ordering::SeqCst) {
                        fuchsia_trace::duration!("input", "input-device-process-reports");
                        for report in &reports {
                            if let Some(trace_id) = report.trace_id {
                                fuchsia_trace::flow_end!("input", "input_report", trace_id.into());
                            }
                            process_input_report(
                                report,
                                &converter,
                                &input_device,
                                shortcut_handler.as_ref(),
                                &scope,
                            );
                        }
                    }

                    if last_report_stamp.saturating_sub(last_acknowledged_stamp) >= ack_threshold {
                        if let Err(e) = reader_proxy.acknowledge_reports(last_report_stamp) {
                            log_debug!("Failed to acknowledge input reports: {:?}", e);
                            break;
                        }
                        last_acknowledged_stamp = last_report_stamp;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log_debug!("InputReportsReaderV2 event stream error: {:?}", e);
                    break;
                }
            }
        }
    }

    /// Removes and unregisters a device by its ID.
    pub fn remove_device(&self, device_id: u32) {
        let (removed, reader_handle, watcher_handle) = {
            let mut state = self.state.lock();
            let reader_handle = state.abort_handles.remove(&device_id);
            let watcher_handle = state.watcher_abort_handles.remove(&device_id);
            let dev = state.devices.remove(&device_id);
            (dev, reader_handle, watcher_handle)
        };
        if let Some(handle) = reader_handle {
            handle.abort();
        }
        if let Some(handle) = watcher_handle {
            handle.abort();
        }
        if let Some(device) = removed {
            if let Some(kernel) = self.kernel.upgrade() {
                unregister_device(&kernel, device.starnix_device);
            }
        }
    }

    /// Returns a registered device by its ID.
    #[cfg(test)]
    pub(crate) fn device(&self, device_id: u32) -> Option<DirectInputDevice> {
        self.state.lock().devices.get(&device_id).cloned()
    }

    /// Returns the number of registered devices.
    pub fn num_devices(&self) -> usize {
        self.state.lock().devices.len()
    }

    /// Starts monitoring driver instances from the component namespace (`/svc/fuchsia.input.report.Service`).
    pub async fn start_service_watcher(self: &Arc<Self>) -> Result<(), Errno> {
        let service =
            fuchsia_component::client::Service::open(fir::ServiceMarker).map_err(|e| {
                log_debug!("Failed to open fuchsia.input.report.Service: {:?}", e);
                errno!(ENOENT)
            })?;
        let mut watcher = service.watch().await.map_err(|e| {
            log_debug!("Failed to watch fuchsia.input.report.Service: {:?}", e);
            errno!(EIO)
        })?;

        let weak_manager = Arc::downgrade(self);
        self.scope.spawn(async move {
            while let Some(instance_res) = watcher.next().await {
                let Some(this) = weak_manager.upgrade() else {
                    break;
                };
                match instance_res {
                    Ok(instance) => {
                        let instance_name = instance.instance_name().to_string();
                        match instance.connect_to_input_device() {
                            Ok(device_proxy) => {
                                let weak_for_task = Arc::downgrade(&this);
                                this.scope.spawn(async move {
                                    let Some(this) = weak_for_task.upgrade() else {
                                        return;
                                    };
                                    if let Err(e) =
                                        this.add_device(device_proxy, Some(instance_name)).await
                                    {
                                        log_warn!("Failed to add direct input device: {:?}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                log_warn!(
                                    "Failed to connect to input device in {}: {:?}",
                                    instance_name,
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        log_warn!("Driver service watcher error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Dependency injection entry point: monitors a custom stream of driver `InputDeviceProxy` instances.
    pub fn start_with_driver_stream<S>(self: &Arc<Self>, mut stream: S)
    where
        S: futures::Stream<Item = fir::InputDeviceProxy> + Send + Unpin + 'static,
    {
        let weak_manager = Arc::downgrade(self);
        self.scope.spawn(async move {
            while let Some(device_proxy) = stream.next().await {
                let Some(this) = weak_manager.upgrade() else {
                    break;
                };
                if let Err(e) = this.add_device(device_proxy, None).await {
                    log_warn!("Failed to add injected driver device: {:?}", e);
                }
            }
        });
    }
}

fn process_input_report(
    report: &fir::InputReport,
    converter: &LockDepMutex<InputReportConverter, DirectInputConverterLock>,
    input_device: &InputDevice,
    shortcut_handler: Option<&Arc<dyn ButtonShortcutProcessor>>,
    scope: &fasync::ScopeHandle,
) {
    // Forward ConsumerControl button state to ButtonShortcutProcessor.
    if let Some(ref consumer_control) = report.consumer_control {
        if let Some(ref pressed_buttons) = consumer_control.pressed_buttons {
            let volume_up = pressed_buttons.contains(&fir::ConsumerControlButton::VolumeUp);
            let volume_down = pressed_buttons.contains(&fir::ConsumerControlButton::VolumeDown);
            let power = pressed_buttons.contains(&fir::ConsumerControlButton::Power);
            if let Some(handler) = shortcut_handler {
                if handler.process_button_state(volume_up, volume_down, power) {
                    let h = handler.clone();
                    scope.spawn(async move {
                        h.trigger_action().await;
                    });
                }
            }
        }
    }

    // Convert and dispatch events to OpenedFiles while holding converter lock.
    let mut conv = converter.lock();
    if !conv.paused {
        let events = conv.handle_report(report);
        if !events.is_empty() {
            input_device.dispatch_events(&events);
        }
    }
}

fn unregister_device(kernel: &Arc<Kernel>, event_device: Device) {
    let spawner = kernel.kthreads.spawner();
    let req = SpawnRequestBuilder::new()
        .with_sync_closure(move |current_task| {
            InputDevice::unregister(current_task, event_device);
        })
        .build();
    spawner.spawn_from_request(req);
}

/// Helper connecting to `fuchsia.ui.input.internal.InputOwnership` and requesting an ownership event.
pub async fn query_ownership_event(
    ownership_proxy: &fuii::InputOwnershipProxy,
    target: fuii::InputOwnershipTarget,
) -> Result<zx::Event, Errno> {
    let res = ownership_proxy.get_event(target).await.map_err(|_| errno!(EIO))?;
    res.map_err(|_| errno!(EACCES))
}

/// Helper for testing that asynchronously waits until `manager.is_ownership_active() == expected`.
#[cfg(test)]
pub(crate) async fn wait_for_ownership(manager: &DirectInputManager, expected: bool) {
    while manager.is_ownership_active() != expected {
        fasync::Timer::new(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_millis(2)))
            .await;
    }
}

/// Spawns a fake `fuchsia.ui.input.internal.InputOwnership` server task for testing.
#[cfg(test)]
pub(crate) fn spawn_fake_input_ownership(
    event: &zx::Event,
) -> (fuii::InputOwnershipProxy, futures::channel::oneshot::Receiver<()>, fasync::Task<()>) {
    let (proxy, mut stream) =
        fidl::endpoints::create_proxy_and_stream::<fuii::InputOwnershipMarker>();
    let server_event = event.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let (get_event_tx, get_event_rx) = futures::channel::oneshot::channel();
    let mut get_event_tx = Some(get_event_tx);
    let server_task = fasync::Task::local(async move {
        while let Some(Ok(request)) = stream.next().await {
            match request {
                fuii::InputOwnershipRequest::GetEvent { target, responder } => match target {
                    fuii::InputOwnershipTarget::Platform(_) => {
                        let ev = server_event
                            .duplicate_handle(
                                zx::Rights::TRANSFER
                                    | zx::Rights::DUPLICATE
                                    | zx::Rights::WAIT
                                    | zx::Rights::INSPECT,
                            )
                            .expect("duplicate event");
                        responder.send(Ok(ev)).expect("send GetEvent response");
                        if let Some(tx) = get_event_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    _ => {
                        responder
                            .send(Err(fuii::InputOwnershipError::InvalidTarget))
                            .expect("send error");
                    }
                },
            }
        }
    });
    (proxy, get_event_rx, server_task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{self, ServerEnd};
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::{FileHandle, VecOutputBuffer};
    use starnix_uapi::errors::EAGAIN;
    use zerocopy::FromBytes;

    const INPUT_EVENT_SIZE: usize = std::mem::size_of::<starnix_uapi::input_event>();

    fn make_test_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0xabcd),
                version: Some(1),
                product_name: Some("Direct Touch Screen".to_string()),
                ..Default::default()
            }),
            touch: Some(fir::TouchDescriptor {
                input: Some(fir::TouchInputDescriptor {
                    contacts: Some(vec![fir::ContactInputDescriptor {
                        position_x: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 1000 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        position_y: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 2000 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        ..Default::default()
                    }]),
                    max_contacts: Some(10),
                    touch_type: Some(fir::TouchType::Touchscreen),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_consumer_control_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x5678),
                version: Some(1),
                product_name: Some("Direct Consumer Control".to_string()),
                ..Default::default()
            }),
            consumer_control: Some(fir::ConsumerControlDescriptor {
                input: Some(fir::ConsumerControlInputDescriptor {
                    buttons: Some(vec![
                        fir::ConsumerControlButton::VolumeUp,
                        fir::ConsumerControlButton::VolumeDown,
                        fir::ConsumerControlButton::Power,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_mouse_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x1234),
                version: Some(1),
                product_name: Some("Direct Mouse".to_string()),
                ..Default::default()
            }),
            mouse: Some(fir::MouseDescriptor {
                input: Some(fir::MouseInputDescriptor {
                    movement_x: Some(fir::Axis {
                        range: fir::Range { min: -127, max: 127 },
                        unit: fir::Unit { type_: fir::UnitType::None, exponent: 0 },
                    }),
                    movement_y: Some(fir::Axis {
                        range: fir::Range { min: -127, max: 127 },
                        unit: fir::Unit { type_: fir::UnitType::None, exponent: 0 },
                    }),
                    buttons: Some(vec![1, 2]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn read_uapi_events(
        file: &FileHandle,
        current_task: &starnix_core::task::CurrentTask,
    ) -> Vec<starnix_uapi::input_event> {
        std::iter::from_fn(|| {
            let mut event_bytes = VecOutputBuffer::new(INPUT_EVENT_SIZE);
            match file.read(current_task, &mut event_bytes) {
                Ok(INPUT_EVENT_SIZE) => Some(
                    starnix_uapi::input_event::read_from_bytes(Vec::from(event_bytes).as_slice())
                        .map_err(|_| anyhow::anyhow!("failed to read input_event from buffer")),
                ),
                Ok(other_size) => Some(Err(anyhow::anyhow!(
                    "got {} bytes (expected {})",
                    other_size,
                    INPUT_EVENT_SIZE
                ))),
                Err(Errno { code: EAGAIN, .. }) => None,
                Err(other_error) => Some(Err(anyhow::anyhow!("read failed: {:?}", other_error))),
            }
        })
        .enumerate()
        .map(|(i, read_res)| match read_res {
            Ok(event) => event,
            Err(e) => panic!("unexpected result {:?} on iteration {}", e, i),
        })
        .collect()
    }

    #[::fuchsia::test]
    fn test_ownership_signal_evaluation() {
        // Active: Bit 1 (DISPLAY_PLATFORM_OWNED) & Bit 3 (INPUT_CLIENT_OWNED)
        let active_signals = DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL;
        assert!(evaluate_ownership(active_signals));

        // Inactive: Bit 1 (DISPLAY_PLATFORM_OWNED) & Bit 4 (INPUT_PLATFORM_OWNED)
        let normal_ui = DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_PLATFORM_OWNED_SIGNAL;
        assert!(!evaluate_ownership(normal_ui));

        // Inactive: Bit 0 (DISPLAY_UNOWNED) & Bit 3 (INPUT_CLIENT_OWNED) - Virtcon active
        let virtcon_active = DISPLAY_UNOWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL;
        assert!(!evaluate_ownership(virtcon_active));

        // Inactive: All zeros
        assert!(!evaluate_ownership(zx::Signals::NONE));

        // Inactive: Bit 1 alone or Bit 3 alone
        assert!(!evaluate_ownership(DISPLAY_PLATFORM_OWNED_SIGNAL));
        assert!(!evaluate_ownership(INPUT_CLIENT_OWNED_SIGNAL));
    }

    #[::fuchsia::test]
    async fn test_ownership_listener_pause_and_resume() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let event = zx::Event::create();

            // Initialize with active signals: Bit 1 & Bit 3
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL,
                )
                .unwrap();

            let manager = DirectInputManager::new(
                Arc::downgrade(&kernel),
                None,
                Some(
                    event
                        .duplicate_handle(
                            zx::Rights::TRANSFER | zx::Rights::WAIT | zx::Rights::INSPECT,
                        )
                        .unwrap(),
                ),
            );

            assert!(manager.is_ownership_active());

            // Switch ownership to Virtcon: Clear Bit 1, Set Bit 0
            event.signal(DISPLAY_PLATFORM_OWNED_SIGNAL, DISPLAY_UNOWNED_SIGNAL).unwrap();

            // Wait deterministically for manager to process signal change to paused
            while manager.is_ownership_active() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(!manager.is_ownership_active());

            // Switch back to Starnix display: Clear Bit 0, Set Bit 1
            event.signal(DISPLAY_UNOWNED_SIGNAL, DISPLAY_PLATFORM_OWNED_SIGNAL).unwrap();

            while !manager.is_ownership_active() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(manager.is_ownership_active());

            // Switch input ownership to platform: Clear Bit 3, Set Bit 4
            event.signal(INPUT_CLIENT_OWNED_SIGNAL, INPUT_PLATFORM_OWNED_SIGNAL).unwrap();

            while manager.is_ownership_active() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(!manager.is_ownership_active());

            // Switch input ownership back to client: Clear Bit 4, Set Bit 3
            event.signal(INPUT_PLATFORM_OWNED_SIGNAL, INPUT_CLIENT_OWNED_SIGNAL).unwrap();

            while !manager.is_ownership_active() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(manager.is_ownership_active());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_driver_discovery_and_report_dispatch() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(device_id).expect("device added");

            let file_handle = dev.input_device.open_test(&current_task).expect("open input device");

            let reader_server = reader_receiver.await.unwrap();
            let (_, reader_control) = reader_server.into_stream_and_control_handle();

            // Send touch report from driver
            let contact = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(300),
                position_y: Some(600),
                ..Default::default()
            };
            let report = fir::InputReport {
                event_time: Some(100_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            reader_control.send_on_input_reports(vec![report], 1).expect("send report");

            // Wait deterministically for report to be received and processed
            let mut received_events = Vec::new();
            for _ in 0..100 {
                received_events = read_uapi_events(&file_handle, &current_task);
                if !received_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }

            assert!(!received_events.is_empty());
            let has_x = received_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                    && e.value == 300
            });
            let has_y = received_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                    && e.value == 600
            });
            assert!(has_x);
            assert!(has_y);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reader_v2_acknowledge_reports_deferred_to_threshold() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            // Report limit of 10 -> ack_threshold = (10 * 7 / 10).max(1) = 7
            const CONFIGURED_LIMIT: u16 = 10;
            const EXPECTED_THRESHOLD: u64 = 7;

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            max_unacknowledged_reports_limit,
                        } => {
                            assert_eq!(
                                max_unacknowledged_reports_limit,
                                MAX_UNACKNOWLEDGED_REPORTS_LIMIT
                            );
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(CONFIGURED_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let _device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();

            let reader_server = reader_receiver.await.unwrap();
            let (mut reader_stream, reader_control) =
                reader_server.into_stream_and_control_handle();

            let make_report = |x, y| {
                let contact = fir::ContactInputReport {
                    contact_id: Some(1),
                    position_x: Some(x),
                    position_y: Some(y),
                    ..Default::default()
                };
                fir::InputReport {
                    event_time: Some(100_000_000),
                    touch: Some(fir::TouchInputReport {
                        contacts: Some(vec![contact]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            };

            // Send reports 1..=6 (below threshold 7). No acknowledgment should be received.
            for stamp in 1..EXPECTED_THRESHOLD {
                reader_control
                    .send_on_input_reports(vec![make_report(100, 200)], stamp)
                    .expect("send report");
            }

            // Yield / wait briefly to ensure events were delivered and processed
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(20),
            ))
            .await;

            // Assert no AcknowledgeReports request has arrived yet
            assert!(
                reader_stream.next().now_or_never().is_none(),
                "AcknowledgeReports must not be sent before threshold is reached"
            );

            // Send 7th report (stamp = 7). Now stamp - last_ack (7 - 0) >= 7.
            reader_control
                .send_on_input_reports(vec![make_report(100, 200)], 7)
                .expect("send 7th report");

            // Expect AcknowledgeReports with last_acknowledged_report_stamp == 7
            let ack_event = reader_stream.next().await.expect("receive ack request").unwrap();
            match ack_event {
                fir::InputReportsReaderV2Request::AcknowledgeReports {
                    last_acknowledged_report_stamp,
                    ..
                } => {
                    assert_eq!(last_acknowledged_report_stamp, 7);
                }
                other => panic!("Unexpected request: {:?}", other),
            }

            // Now last_acknowledged_stamp is 7.
            // Send reports 8..=13 (stamps 8 to 13: 13 - 7 = 6 < 7).
            for stamp in 8..14 {
                reader_control
                    .send_on_input_reports(vec![make_report(100, 200)], stamp)
                    .expect("send report");
            }

            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(20),
            ))
            .await;

            assert!(
                reader_stream.next().now_or_never().is_none(),
                "AcknowledgeReports must not be sent before threshold delta from last ack"
            );

            // Send 14th report (stamp = 14: 14 - 7 = 7 >= 7).
            reader_control
                .send_on_input_reports(vec![make_report(100, 200)], 14)
                .expect("send 14th report");

            let ack_event2 = reader_stream.next().await.expect("receive ack request").unwrap();
            match ack_event2 {
                fir::InputReportsReaderV2Request::AcknowledgeReports {
                    last_acknowledged_report_stamp,
                    ..
                } => {
                    assert_eq!(last_acknowledged_report_stamp, 14);
                }
                other => panic!("Unexpected request: {:?}", other),
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reader_v2_acknowledge_reports_called_when_unowned() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            const CONFIGURED_LIMIT: u16 = 10;

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(CONFIGURED_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(device_id).expect("device added");
            let file_handle = dev.input_device.open_test(&current_task).expect("open input device");

            // Simulate unowned input while keeping the active reader loop running
            manager.ownership_active.store(false, Ordering::SeqCst);
            assert!(!manager.is_ownership_active());

            let reader_server = reader_receiver.await.unwrap();
            let (mut reader_stream, reader_control) =
                reader_server.into_stream_and_control_handle();

            let make_report = || {
                let contact = fir::ContactInputReport {
                    contact_id: Some(1),
                    position_x: Some(100),
                    position_y: Some(200),
                    ..Default::default()
                };
                fir::InputReport {
                    event_time: Some(100_000_000),
                    touch: Some(fir::TouchInputReport {
                        contacts: Some(vec![contact]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            };

            // Send 7 reports while unowned
            for stamp in 1..=7 {
                reader_control
                    .send_on_input_reports(vec![make_report()], stamp)
                    .expect("send report");
            }

            // Even when unowned, AcknowledgeReports must be sent once threshold is reached
            let ack_event = reader_stream.next().await.expect("receive ack request").unwrap();
            match ack_event {
                fir::InputReportsReaderV2Request::AcknowledgeReports {
                    last_acknowledged_report_stamp,
                    ..
                } => {
                    assert_eq!(last_acknowledged_report_stamp, 7);
                }
                other => panic!("Unexpected request: {:?}", other),
            }

            // Verify that while reports were acknowledged, no events were dispatched to UAPI
            let received = read_uapi_events(&file_handle, &current_task);
            assert!(received.is_empty(), "Reports received while unowned must be dropped");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_consumer_control_button_shortcut() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let action_triggered = Arc::new(AtomicBool::new(false));
            let action_triggered_clone = action_triggered.clone();

            struct MockShortcutProcessor {
                press_count: AtomicU32,
                action_triggered: Arc<AtomicBool>,
            }
            impl ButtonShortcutProcessor for MockShortcutProcessor {
                fn process_button_state(
                    &self,
                    volume_up: bool,
                    volume_down: bool,
                    _power: bool,
                ) -> bool {
                    if volume_up && volume_down {
                        if self.press_count.fetch_add(1, Ordering::SeqCst) + 1 >= 5 {
                            return true;
                        }
                    }
                    false
                }
                fn trigger_action(self: Arc<Self>) -> BoxFuture<'static, bool> {
                    let triggered = self.action_triggered.clone();
                    Box::pin(async move {
                        triggered.store(true, Ordering::SeqCst);
                        true
                    })
                }
            }

            let shortcut_handler = Arc::new(MockShortcutProcessor {
                press_count: AtomicU32::new(0),
                action_triggered: action_triggered_clone,
            });

            let manager =
                DirectInputManager::new(Arc::downgrade(&kernel), Some(shortcut_handler), None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_consumer_control_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let _device_id =
                manager.add_device(device_proxy, Some("buttons0".to_string())).await.unwrap();

            let reader_server = reader_receiver.await.unwrap();
            let (_, reader_control) = reader_server.into_stream_and_control_handle();

            // Send 5 consecutive VolumeUp + VolumeDown button presses
            for i in 1..=5 {
                let report_down = fir::InputReport {
                    event_time: Some(i * 10_000_000),
                    consumer_control: Some(fir::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![
                            fir::ConsumerControlButton::VolumeUp,
                            fir::ConsumerControlButton::VolumeDown,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let report_up = fir::InputReport {
                    event_time: Some(i * 10_000_000 + 5_000),
                    consumer_control: Some(fir::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![]),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                reader_control
                    .send_on_input_reports(vec![report_down], (i * 2 - 1) as u64)
                    .unwrap();
                // Brief pause so boot clock advances between presses
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
                reader_control.send_on_input_reports(vec![report_up], (i * 2) as u64).unwrap();
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }

            // Wait deterministically for shortcut action to trigger
            for _ in 0..100 {
                if action_triggered.load(Ordering::SeqCst) {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(10),
                ))
                .await;
            }

            assert!(action_triggered.load(Ordering::SeqCst));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pause_drops_reports_until_resumed() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let event = zx::Event::create();
            // Start active
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL,
                )
                .unwrap();

            let manager = DirectInputManager::new(
                Arc::downgrade(&kernel),
                None,
                Some(
                    event
                        .duplicate_handle(
                            zx::Rights::TRANSFER | zx::Rights::WAIT | zx::Rights::INSPECT,
                        )
                        .unwrap(),
                ),
            );
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, mut reader_receiver) =
                futures::channel::mpsc::unbounded::<ServerEnd<fir::InputReportsReaderV2Marker>>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = reader_sender.unbounded_send(reader);
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(device_id).expect("device added");

            let file_handle = dev.input_device.open_test(&current_task).expect("open input device");

            let reader_server = reader_receiver.next().await.unwrap();
            let (_, reader_control) = reader_server.into_stream_and_control_handle();

            // Send initial report before pause
            let contact_initial = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(300),
                position_y: Some(700),
                ..Default::default()
            };
            let report_initial = fir::InputReport {
                event_time: Some(50_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact_initial]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            reader_control.send_on_input_reports(vec![report_initial], 1).expect("send report");

            let mut initial_events = Vec::new();
            for _ in 0..100 {
                initial_events = read_uapi_events(&file_handle, &current_task);
                if !initial_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(!initial_events.is_empty());
            assert!(initial_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_KEY as u16
                    && e.code == starnix_uapi::BTN_TOUCH as u16
                    && e.value == 1
            }));

            // Pause manager -> must flush releases for active contact
            manager.pause();
            assert!(!manager.is_ownership_active());

            let mut lift_events = Vec::new();
            for _ in 0..100 {
                lift_events = read_uapi_events(&file_handle, &current_task);
                if !lift_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(lift_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_TRACKING_ID as u16
                    && e.value == -1
            }));
            assert!(lift_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_KEY as u16
                    && e.code == starnix_uapi::BTN_TOUCH as u16
                    && e.value == 0
            }));

            // Send report while paused
            let contact = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(400),
                position_y: Some(800),
                ..Default::default()
            };
            let report = fir::InputReport {
                event_time: Some(100_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            reader_control.send_on_input_reports(vec![report], 2).expect("send report");

            // Give some time for processing loop to drop it
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(20),
            ))
            .await;

            let received = read_uapi_events(&file_handle, &current_task);
            assert!(received.is_empty(), "Reports received while paused must be dropped");

            // Resume manager and verify that report reading resumes
            manager.resume();
            assert!(manager.is_ownership_active());

            let reader_server_resumed = reader_receiver.next().await.unwrap();
            let (_, reader_control_resumed) =
                reader_server_resumed.into_stream_and_control_handle();

            let contact_resumed = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(500),
                position_y: Some(900),
                ..Default::default()
            };
            let report_resumed = fir::InputReport {
                event_time: Some(200_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact_resumed]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            reader_control_resumed
                .send_on_input_reports(vec![report_resumed], 1)
                .expect("send report after resume");

            let mut received_resumed = Vec::new();
            for _ in 0..100 {
                received_resumed = read_uapi_events(&file_handle, &current_task);
                if !received_resumed.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(
                !received_resumed.is_empty(),
                "Reports received after resume must be delivered"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clean_teardown_on_driver_disconnect() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            assert!(manager.device(device_id).is_some());

            // Receive reader server end and drop it immediately to simulate driver disconnect
            let reader_server = reader_receiver.await.unwrap();
            drop(reader_server);

            // Wait deterministically for disconnect handler to trigger
            while manager.device(device_id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(manager.device(device_id).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_start_with_driver_stream() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, mut reader_receiver) =
                futures::channel::mpsc::unbounded::<ServerEnd<fir::InputReportsReaderV2Marker>>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = reader_sender.unbounded_send(reader);
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let (sender, receiver) = futures::channel::mpsc::unbounded();
            manager.start_with_driver_stream(receiver);

            let proxy = device_client.into_proxy();
            sender.unbounded_send(proxy).unwrap();

            let _reader = reader_receiver.next().await.unwrap();

            while manager.num_devices() == 0 {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert_eq!(manager.num_devices(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reports_reader_v2_with_trace_id() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(s) = reader_sender_opt.take() {
                                let _ = s.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(device_id).expect("device added");

            let file_handle = dev.input_device.open_test(&current_task).expect("open input device");

            let reader_server = reader_receiver.await.unwrap();
            let (_, reader_control) = reader_server.into_stream_and_control_handle();

            let contact = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(300),
                position_y: Some(600),
                ..Default::default()
            };
            let report = fir::InputReport {
                event_time: Some(100_000_000),
                trace_id: Some(12345),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            reader_control.send_on_input_reports(vec![report], 1).expect("send report");

            let mut received_events = Vec::new();
            for _ in 0..100 {
                received_events = read_uapi_events(&file_handle, &current_task);
                if !received_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }

            assert!(!received_events.is_empty());
            let has_x = received_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                    && e.value == 300
            });
            let has_y = received_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                    && e.value == 600
            });
            assert!(has_x);
            assert!(has_y);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_query_ownership_event() {
        let (proxy, mut stream) =
            endpoints::create_proxy_and_stream::<fuii::InputOwnershipMarker>();

        let task = fasync::Task::local(async move {
            if let Some(Ok(fuii::InputOwnershipRequest::GetEvent { responder, .. })) =
                stream.next().await
            {
                let ev = zx::Event::create();
                responder.send(Ok(ev)).unwrap();
            }
            if let Some(Ok(fuii::InputOwnershipRequest::GetEvent { responder, .. })) =
                stream.next().await
            {
                responder.send(Err(fuii::InputOwnershipError::AccessDenied)).unwrap();
            }
        });

        // 1. Success path
        let ev =
            query_ownership_event(&proxy, fuii::InputOwnershipTarget::Platform(fuii::Platform {}))
                .await
                .expect("query succeeds");
        assert!(ev.duplicate_handle(zx::Rights::SAME_RIGHTS).is_ok());

        // 2. Application error path (ACCESS_DENIED -> EACCES)
        let err =
            query_ownership_event(&proxy, fuii::InputOwnershipTarget::Platform(fuii::Platform {}))
                .await
                .expect_err("query returns access denied");
        assert_eq!(err, errno!(EACCES));

        drop(task);
        // 3. Transport error path (closed -> EIO)
        let err2 =
            query_ownership_event(&proxy, fuii::InputOwnershipTarget::Platform(fuii::Platform {}))
                .await
                .expect_err("query returns EIO when channel closed");
        assert_eq!(err2, errno!(EIO));
    }

    #[::fuchsia::test]
    async fn test_multi_device_registration_and_lifecycle() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            manager.set_ownership_active(true);
            let mut _tasks = Vec::new();

            // Set up mock touch device
            let (touch_client, touch_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (touch_reader_sender, mut touch_reader_receiver) =
                futures::channel::mpsc::unbounded::<ServerEnd<fir::InputReportsReaderV2Marker>>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = touch_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = touch_reader_sender.unbounded_send(reader);
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            // Set up mock mouse device
            let (mouse_client, mouse_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (mouse_reader_sender, mut mouse_reader_receiver) =
                futures::channel::mpsc::unbounded::<ServerEnd<fir::InputReportsReaderV2Marker>>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = mouse_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_mouse_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = mouse_reader_sender.unbounded_send(reader);
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            // Register touch device
            let touch_proxy = touch_client.into_proxy();
            let touch_id =
                manager.add_device(touch_proxy, Some("touch0".to_string())).await.unwrap();

            // Register mouse device
            let mouse_proxy = mouse_client.into_proxy();
            let mouse_id =
                manager.add_device(mouse_proxy, Some("mouse0".to_string())).await.unwrap();

            // Verify both devices registered and have unique minors
            assert_eq!(manager.num_devices(), 2);
            let touch_dev = manager.device(touch_id).expect("touch device exists");
            let mouse_dev = manager.device(mouse_id).expect("mouse device exists");
            assert_ne!(touch_id, mouse_id);
            assert_ne!(touch_dev.minor, mouse_dev.minor);

            let touch_file = touch_dev.input_device.open_test(&current_task).expect("open touch");
            let mouse_file = mouse_dev.input_device.open_test(&current_task).expect("open mouse");

            let touch_reader_server = touch_reader_receiver.next().await.unwrap();
            let (_, touch_reader_control) = touch_reader_server.into_stream_and_control_handle();

            let mouse_reader_server = mouse_reader_receiver.next().await.unwrap();
            let (_, mouse_reader_control) = mouse_reader_server.into_stream_and_control_handle();

            // Pause manager and verify reports are dropped for both devices
            manager.pause();
            assert!(!manager.is_ownership_active());

            let touch_report = fir::InputReport {
                event_time: Some(10_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(100),
                        position_y: Some(200),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            touch_reader_control.send_on_input_reports(vec![touch_report], 1).unwrap();

            let mouse_report = fir::InputReport {
                event_time: Some(10_000_000),
                mouse: Some(fir::MouseInputReport {
                    movement_x: Some(15),
                    movement_y: Some(25),
                    ..Default::default()
                }),
                ..Default::default()
            };
            mouse_reader_control.send_on_input_reports(vec![mouse_report], 1).unwrap();

            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(20),
            ))
            .await;

            assert!(read_uapi_events(&touch_file, &current_task).is_empty());
            assert!(read_uapi_events(&mouse_file, &current_task).is_empty());

            // Resume manager
            manager.resume();
            assert!(manager.is_ownership_active());

            let _touch_reader_resumed = touch_reader_receiver.next().await.unwrap();
            let mouse_reader_resumed = mouse_reader_receiver.next().await.unwrap();
            let (_, mouse_reader_control_resumed) =
                mouse_reader_resumed.into_stream_and_control_handle();

            // Remove touch device while mouse device continues
            manager.remove_device(touch_id);
            assert_eq!(manager.num_devices(), 1);
            assert!(manager.device(touch_id).is_none());
            assert!(manager.device(mouse_id).is_some());

            // Send report from the remaining mouse device
            let mouse_report2 = fir::InputReport {
                event_time: Some(20_000_000),
                mouse: Some(fir::MouseInputReport {
                    movement_x: Some(30),
                    movement_y: Some(40),
                    ..Default::default()
                }),
                ..Default::default()
            };
            mouse_reader_control_resumed.send_on_input_reports(vec![mouse_report2], 1).unwrap();

            let mut received_mouse_events = Vec::new();
            for _ in 0..100 {
                received_mouse_events = read_uapi_events(&mouse_file, &current_task);
                if !received_mouse_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }

            assert!(
                !received_mouse_events.is_empty(),
                "Mouse device should continue receiving reports after touch device removal"
            );
            let has_rel_x = received_mouse_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_REL as u16
                    && e.code == starnix_uapi::REL_X as u16
                    && e.value == 30
            });
            assert!(has_rel_x);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_manager_drops_cleanly() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let event = zx::Event::create();
            let weak_manager = {
                let manager = DirectInputManager::new(
                    Arc::downgrade(&kernel),
                    None,
                    Some(
                        event
                            .duplicate_handle(
                                zx::Rights::TRANSFER | zx::Rights::WAIT | zx::Rights::INSPECT,
                            )
                            .unwrap(),
                    ),
                );
                Arc::downgrade(&manager)
            };
            // With outer manager dropped, the listener must not hold a strong cycle across OnSignals.
            // Wait briefly for drop to take effect.
            for _ in 0..50 {
                if weak_manager.upgrade().is_none() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(
                weak_manager.upgrade().is_none(),
                "DirectInputManager should be dropped without reference cycle in listener"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ownership_channel_closure_pauses_direct_input() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let event = zx::Event::create();
            // Start active
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL,
                )
                .unwrap();

            let (client_channel, server_channel) = zx::Channel::create();
            let manager = DirectInputManager::new_with_channel(
                Arc::downgrade(&kernel),
                None,
                Some(
                    event
                        .duplicate_handle(
                            zx::Rights::TRANSFER | zx::Rights::WAIT | zx::Rights::INSPECT,
                        )
                        .unwrap(),
                ),
                Some(client_channel),
            );

            assert!(manager.is_ownership_active());

            // Simulate Scenic crash/restart by dropping server channel end
            drop(server_channel);

            while manager.is_ownership_active() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(
                !manager.is_ownership_active(),
                "Manager should pause when ownership channel is closed by peer"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_minor_range_partitioning() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            let mut _tasks = Vec::new();

            // Set up mock touch device
            let (touch_client, touch_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = touch_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let touch_proxy = touch_client.into_proxy();
            let touch_id =
                manager.add_device(touch_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(touch_id).expect("device added");

            // Verify minor is allocated within DIRECT_INPUT_MINOR_RANGE (64..128)
            assert!(
                crate::DIRECT_INPUT_MINOR_RANGE.contains(&dev.minor),
                "Direct input minor {} must fall within DIRECT_INPUT_MINOR_RANGE ({:?})",
                dev.minor,
                crate::DIRECT_INPUT_MINOR_RANGE
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_ownership_transitions_with_fake_server() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            let event = zx::Event::create();
            // Initially, platform owns input
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_PLATFORM_OWNED_SIGNAL,
                )
                .unwrap();

            let (proxy, get_event_rx, _server_task) = spawn_fake_input_ownership(&event);

            let manager = DirectInputManager::new_with_ownership_proxy(
                Arc::downgrade(&kernel),
                None,
                Some(proxy),
            );

            // Wait for fake server to handle GetEvent
            get_event_rx.await.expect("GetEvent handled by fake server");

            // Initially platform owns input, so direct input ownership is inactive
            assert!(!manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 0);

            let mut _tasks = Vec::new();
            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let (reader_sender, mut reader_receiver) =
                futures::channel::mpsc::unbounded::<ServerEnd<fir::InputReportsReaderV2Marker>>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = reader_sender.unbounded_send(reader);
                            responder.send(MAX_UNACKNOWLEDGED_REPORTS_LIMIT).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_proxy = device_client.into_proxy();
            let device_id =
                manager.add_device(device_proxy, Some("touch0".to_string())).await.unwrap();
            let dev = manager.device(device_id).expect("device added");
            let file_handle = dev.input_device.open_test(&current_task).expect("open input device");
            assert_eq!(manager.num_devices(), 1);
            assert!(reader_receiver.try_recv().is_err());

            // Transition 1: Grant direct input ownership to Starnix client view
            // Clear INPUT_PLATFORM_OWNED and assert INPUT_CLIENT_OWNED
            event.signal(INPUT_PLATFORM_OWNED_SIGNAL, INPUT_CLIENT_OWNED_SIGNAL).unwrap();

            wait_for_ownership(&manager, true).await;
            assert!(manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 1);

            let reader_server = reader_receiver.next().await.unwrap();
            let (_, reader_control) = reader_server.into_stream_and_control_handle();

            let contact_down = fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(100),
                position_y: Some(200),
                ..Default::default()
            };
            let report_down = fir::InputReport {
                event_time: Some(100_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![contact_down]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            reader_control.send_on_input_reports(vec![report_down], 1).expect("send report");

            let mut touch_down_events = Vec::new();
            for _ in 0..100 {
                touch_down_events = read_uapi_events(&file_handle, &current_task);
                if !touch_down_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(!touch_down_events.is_empty());
            assert!(touch_down_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_KEY as u16
                    && e.code == starnix_uapi::BTN_TOUCH as u16
                    && e.value == 1
            }));
            assert!(touch_down_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                    && e.value == 100
            }));

            // Transition 2: Revoke direct input ownership back to platform
            // Clear INPUT_CLIENT_OWNED and assert INPUT_PLATFORM_OWNED
            event.signal(INPUT_CLIENT_OWNED_SIGNAL, INPUT_PLATFORM_OWNED_SIGNAL).unwrap();

            wait_for_ownership(&manager, false).await;
            assert!(!manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 1);

            let mut lift_events = Vec::new();
            for _ in 0..100 {
                lift_events = read_uapi_events(&file_handle, &current_task);
                if !lift_events.is_empty() {
                    break;
                }
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(5),
                ))
                .await;
            }
            assert!(!lift_events.is_empty());
            assert!(lift_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_ABS as u16
                    && e.code == starnix_uapi::ABS_MT_TRACKING_ID as u16
                    && e.value == -1
            }));
            assert!(lift_events.iter().any(|e| {
                e.type_ == starnix_uapi::EV_KEY as u16
                    && e.code == starnix_uapi::BTN_TOUCH as u16
                    && e.value == 0
            }));

            // Transition 3: Re-grant direct input ownership
            event.signal(INPUT_PLATFORM_OWNED_SIGNAL, INPUT_CLIENT_OWNED_SIGNAL).unwrap();

            wait_for_ownership(&manager, true).await;
            assert!(manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 2);

            drop(_server_task);
            wait_for_ownership(&manager, false).await;
            assert!(!manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_ownership_fallback_when_unavailable() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            // Case 1: ownership_proxy is None
            let manager1 =
                DirectInputManager::new_with_ownership_proxy(Arc::downgrade(&kernel), None, None);
            assert!(!manager1.is_ownership_active());

            // Case 2: ownership_proxy is connected to a closed server endpoint
            let (proxy, server_end) = endpoints::create_proxy::<fuii::InputOwnershipMarker>();
            drop(server_end);

            let manager2 = DirectInputManager::new_with_ownership_proxy(
                Arc::downgrade(&kernel),
                None,
                Some(proxy),
            );
            manager2.scope.on_no_tasks().await;
            assert!(!manager2.is_ownership_active());

            // Case 3: ownership_proxy returns an error (e.g. AccessDenied)
            let (proxy3, mut stream3) =
                endpoints::create_proxy_and_stream::<fuii::InputOwnershipMarker>();
            let (case3_tx, case3_rx) = futures::channel::oneshot::channel();
            let mut case3_tx = Some(case3_tx);
            let _server_task3 = fasync::Task::local(async move {
                if let Some(Ok(fuii::InputOwnershipRequest::GetEvent { responder, .. })) =
                    stream3.next().await
                {
                    responder.send(Err(fuii::InputOwnershipError::AccessDenied)).unwrap();
                    if let Some(tx) = case3_tx.take() {
                        let _ = tx.send(());
                    }
                }
            });

            let manager3 = DirectInputManager::new_with_ownership_proxy(
                Arc::downgrade(&kernel),
                None,
                Some(proxy3),
            );
            case3_rx.await.expect("GetEvent handled by fake server");
            manager3.scope.on_no_tasks().await;
            assert!(!manager3.is_ownership_active());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_ownership_pre_signalled_active() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            let event = zx::Event::create();
            // Pre-signal INPUT_CLIENT_OWNED before GetEvent returns
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL,
                )
                .unwrap();

            let (proxy, _get_event_rx, _server_task) = spawn_fake_input_ownership(&event);

            let manager = DirectInputManager::new_with_ownership_proxy(
                Arc::downgrade(&kernel),
                None,
                Some(proxy),
            );

            // Ownership should transition to active because the event was already INPUT_CLIENT_OWNED.
            wait_for_ownership(&manager, true).await;
            assert!(manager.is_ownership_active());
            assert_eq!(manager.resume_generation(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_direct_input_ownership_handle_without_wait_pauses_manager() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            let event = zx::Event::create();
            event
                .signal(
                    zx::Signals::NONE,
                    DISPLAY_PLATFORM_OWNED_SIGNAL | INPUT_CLIENT_OWNED_SIGNAL,
                )
                .unwrap();
            // Duplicate event handle without ZX_RIGHT_WAIT.
            let event_without_wait = event
                .duplicate_handle(
                    zx::Rights::TRANSFER | zx::Rights::DUPLICATE | zx::Rights::INSPECT,
                )
                .expect("duplicate event without wait rights");

            let manager =
                DirectInputManager::new_with_ownership_proxy(Arc::downgrade(&kernel), None, None);

            // Make ownership active initially
            manager.set_ownership_active(true);
            assert!(manager.is_ownership_active());

            // Run the listener directly with an event handle lacking WAIT rights.
            // On the initial iteration, `current_signals()` cannot inspect signals without
            // ZX_RIGHT_WAIT, returns `Signals::NONE`, evaluates ownership as inactive,
            // and pauses the manager.
            let this = Arc::downgrade(&manager);
            manager.scope.spawn(async move {
                DirectInputManager::run_ownership_listener(event_without_wait, None, this).await;
            });

            wait_for_ownership(&manager, false).await;
            manager.scope.on_no_tasks().await;
            assert!(!manager.is_ownership_active());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_add_device_unsupported_descriptor_fails() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let manager = DirectInputManager::new(Arc::downgrade(&kernel), None, None);

            let (device_proxy, _server_end) = endpoints::create_proxy::<fir::InputDeviceMarker>();
            let descriptor =
                fir::DeviceDescriptor { sensor: Some(Default::default()), ..Default::default() };

            let result = manager
                .add_device_with_descriptor(device_proxy, Some("sensor0".to_string()), descriptor)
                .await;
            assert_eq!(result.unwrap_err(), errno!(ENOTSUP));
            assert_eq!(manager.num_devices(), 0);
        })
        .await;
    }
}
