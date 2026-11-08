// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use fidl::endpoints::SynchronousProxy as _;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use futures::TryStreamExt as _;
use starnix_core::device::kobject::{Device, UEventAction};
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps, serialize_for_file};
use starnix_logging::log_warn;
use starnix_sync::{LockDepMutex, LockDepRwLock, PowerSupplyStateLock};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::fmt::Display;
use std::sync::Arc;

/// Default maximum charger voltage in microvolts (5V).
const DEFAULT_CHARGER_MAX_VOLTAGE_UV: i32 = 5_000_000;

/// Timeout for probing whether a `BatterySimulator` channel is connected and routed.
const SIMULATOR_PROBE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// Linux `power_supply` `usb_type` strings.
const USB_TYPE_UNKNOWN: &str = "[Unknown] SDP DCP CDP";
const USB_TYPE_SDP: &str = "Unknown [SDP] DCP CDP";
const USB_TYPE_DCP: &str = "Unknown SDP [DCP] CDP";

/// In-memory state of the `/sys/class/power_supply/battery` device.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct BatteryState {
    present: bool,
    capacity: u8,
    fake_capacity: Option<u8>,
    simulator_koid: Option<zx::Koid>,
    status: &'static str,
    health: &'static str,
    voltage_now_uv: i32,
    current_now_ua: i32,
    current_avg_ua: i32,
    charge_counter_uah: i32,
    charge_full_uah: i32,
    charge_full_design_uah: i32,
    temp_celsius_tenths: i32,
    cycle_count: i32,
    capacity_level: &'static str,
    time_to_full_now_seconds: Option<i64>,
    technology: &'static str,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            present: true,
            capacity: 0,
            fake_capacity: None,
            simulator_koid: None,
            status: "Unknown",
            health: "Unknown",
            voltage_now_uv: 0,
            current_now_ua: 0,
            current_avg_ua: 0,
            charge_counter_uah: 0,
            charge_full_uah: 0,
            charge_full_design_uah: 0,
            temp_celsius_tenths: 0,
            cycle_count: 0,
            capacity_level: "Unknown",
            time_to_full_now_seconds: None,
            technology: "Li-ion",
        }
    }
}

impl BatteryState {
    fn capacity_level_for_percent(capacity: u8, status: &str) -> &'static str {
        match capacity {
            100.. => "Full",
            80..=99 => "High",
            40..=79 => "Normal",
            1..=39 => "Low",
            0 if status == "Charging" => "Low",
            0 => "Critical",
        }
    }

    fn effective_capacity(&self) -> u8 {
        self.fake_capacity.unwrap_or(self.capacity)
    }

    fn effective_capacity_level(&self) -> &'static str {
        match self.fake_capacity {
            Some(cap) => Self::capacity_level_for_percent(cap, self.status),
            None => self.capacity_level,
        }
    }

    /// Returns a normalized copy with `effective_capacity()` and `effective_capacity_level()`
    /// materialized so `PartialEq` reflects user-visible sysfs changes.
    fn effective(&self) -> Self {
        Self {
            capacity: self.effective_capacity(),
            capacity_level: self.effective_capacity_level(),
            fake_capacity: None,
            simulator_koid: None,
            ..*self
        }
    }
}

/// In-memory state of a charger power supply device (`usb`, `ac`, or `wireless`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct ChargerState {
    online: bool,
    current_max_ua: i32,
    voltage_max_uv: i32,
}

impl Default for ChargerState {
    fn default() -> Self {
        Self { online: false, current_max_ua: 0, voltage_max_uv: DEFAULT_CHARGER_MAX_VOLTAGE_UV }
    }
}

impl ChargerState {
    /// Returns a normalized copy with `current_max_ua` and `voltage_max_uv` zeroed when offline
    /// so `PartialEq` reflects user-visible sysfs changes.
    fn effective(&self) -> Self {
        if self.online {
            *self
        } else {
            Self { online: false, current_max_ua: 0, voltage_max_uv: 0 }
        }
    }
}

/// In-memory representation of all power supply devices.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerSupplyState {
    battery: BatteryState,
    usb: ChargerState,
    usb_type: &'static str,
    ac: ChargerState,
    wireless: ChargerState,
}

impl Default for PowerSupplyState {
    fn default() -> Self {
        Self {
            battery: BatteryState::default(),
            usb: ChargerState::default(),
            usb_type: USB_TYPE_UNKNOWN,
            ac: ChargerState::default(),
            wireless: ChargerState::default(),
        }
    }
}

impl PowerSupplyState {
    /// Update the state from a Fuchsia `BatteryInfo` table.
    fn update_from_battery_info(&mut self, info: &fpower::BatteryInfo) {
        if let Some(status) = info.status {
            self.battery.present =
                matches!(status, fpower::BatteryStatus::Ok | fpower::BatteryStatus::NotAvailable);
        }

        if let Some(charge_status) = info.charge_status {
            self.battery.status = match charge_status {
                fpower::ChargeStatus::Charging => "Charging",
                fpower::ChargeStatus::Discharging => "Discharging",
                fpower::ChargeStatus::NotCharging => "Not charging",
                fpower::ChargeStatus::Full => "Full",
                _ => "Unknown",
            };
        }

        if let Some(source) = info.charge_source {
            self.usb.online =
                matches!(source, fpower::ChargeSource::Usb | fpower::ChargeSource::AcAdapter);
            self.usb_type = match source {
                fpower::ChargeSource::Usb => USB_TYPE_SDP,
                fpower::ChargeSource::AcAdapter => USB_TYPE_DCP,
                _ => USB_TYPE_UNKNOWN,
            };
            self.ac.online = source == fpower::ChargeSource::AcAdapter;
            self.wireless.online = source == fpower::ChargeSource::Wireless;
        }

        // Avoid overwriting the last known real `capacity` and `capacity_level` while
        // `BatterySimulator` simulation is active, so that `BatterySimulator` echoes do not
        // clobber the fallback values restored when `fake_capacity` is reset (`< 0`).
        if self.battery.simulator_koid.is_none() {
            if let Some(level_percent) = info.level_percent {
                // `battery_manager` already rounds `level_percent` to an integer percentage.
                self.battery.capacity = level_percent.clamp(0.0, 100.0) as u8;
                self.battery.capacity_level = BatteryState::capacity_level_for_percent(
                    self.battery.capacity,
                    self.battery.status,
                );
            } else if let Some(level_status) = info.level_status {
                self.battery.capacity_level = match level_status {
                    fpower::LevelStatus::Ok => {
                        if self.battery.capacity == 100 {
                            "Full"
                        } else {
                            "High"
                        }
                    }
                    fpower::LevelStatus::Warning => "Normal",
                    fpower::LevelStatus::Low => "Low",
                    fpower::LevelStatus::Critical => "Critical",
                    _ => "Unknown",
                };
            }
        }

        if let Some(health) = info.health {
            self.battery.health = match health {
                fpower::HealthStatus::Good => "Good",
                fpower::HealthStatus::Cool => "Cool",
                fpower::HealthStatus::Warm => "Warm",
                fpower::HealthStatus::Cold => "Cold",
                fpower::HealthStatus::Hot | fpower::HealthStatus::Overheat => "Overheat",
                fpower::HealthStatus::Dead => "Dead",
                fpower::HealthStatus::OverVoltage => "Over voltage",
                fpower::HealthStatus::UnspecifiedFailure => "Unspecified failure",
                _ => "Unknown",
            };
        }

        if let Some(voltage_mv) = info.present_voltage_mv {
            self.battery.voltage_now_uv = (voltage_mv as i32) * 1000;
        }

        if let Some(current_ua) = info.present_charging_current_ua {
            self.battery.current_now_ua = current_ua;
        }

        if let Some(current_avg_ua) = info.average_charging_current_ua {
            self.battery.current_avg_ua = current_avg_ua;
        }

        if let Some(charge_uah) = info.remaining_charge_uah {
            self.battery.charge_counter_uah = charge_uah as i32;
        }

        if let Some(full_charge_uah) = info.full_capacity_uah {
            self.battery.charge_full_uah = full_charge_uah;
        }

        if let Some(temp_mc) = info.temperature_mc {
            self.battery.temp_celsius_tenths = temp_mc / 100;
        }

        if let Some(spec) = &info.battery_spec {
            if let Some(design_uah) = spec.design_capacity_uah {
                self.battery.charge_full_design_uah = design_uah;
            }
            if let Some(max_current_ua) = spec.max_charging_current_ua {
                self.usb.current_max_ua = max_current_ua;
                self.ac.current_max_ua = max_current_ua;
                self.wireless.current_max_ua = max_current_ua;
            }
            if let Some(max_voltage_uv) = spec.max_charging_voltage_uv {
                self.usb.voltage_max_uv = max_voltage_uv;
                self.ac.voltage_max_uv = max_voltage_uv;
                self.wireless.voltage_max_uv = max_voltage_uv;
            }
        }

        self.battery.time_to_full_now_seconds = match &info.time_remaining {
            Some(fpower::TimeRemaining::FullCharge(ns)) if *ns >= 0 => {
                Some(std::time::Duration::from_nanos(*ns as u64).as_secs() as i64)
            }
            _ => None,
        };
    }
}

type SharedPowerSupplyState = Arc<LockDepRwLock<PowerSupplyState, PowerSupplyStateLock>>;

struct PowerSupplyFile<F> {
    state: SharedPowerSupplyState,
    read_fn: F,
}

impl<T, F> PowerSupplyFile<F>
where
    T: Display + 'static,
    F: Fn(&PowerSupplyState) -> T + Send + Sync + 'static,
{
    fn new_node(state: &SharedPowerSupplyState, read_fn: F) -> impl FsNodeOps {
        BytesFile::new_node(Self { state: Arc::clone(state), read_fn })
    }
}

impl<T, F> BytesFileOps for PowerSupplyFile<F>
where
    T: Display + 'static,
    F: Fn(&PowerSupplyState) -> T + Send + Sync + 'static,
{
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = self.state.read();
        Ok(serialize_for_file((self.read_fn)(&state)).into())
    }
}

fn power_supply_file<T, F>(
    dir: &SimpleDirectoryMutator,
    name: &'static str,
    state: &SharedPowerSupplyState,
    read_fn: F,
) where
    T: Display + 'static,
    F: Fn(&PowerSupplyState) -> T + Send + Sync + 'static,
{
    dir.entry(name, PowerSupplyFile::new_node(state, read_fn), mode!(IFREG, 0o444));
}

struct BatteryCapacityFile {
    state: SharedPowerSupplyState,
    device: Device,
    simulator:
        LockDepMutex<Option<Arc<spower::BatterySimulatorSynchronousProxy>>, PowerSupplyStateLock>,
}

impl BatteryCapacityFile {
    fn new_node(state: &SharedPowerSupplyState, device: &Device) -> impl FsNodeOps {
        BytesFile::new_node(Self {
            state: Arc::clone(state),
            device: device.clone(),
            simulator: LockDepMutex::new(None),
        })
    }

    fn probe_simulator(
        proxy: spower::BatterySimulatorSynchronousProxy,
    ) -> Option<spower::BatterySimulatorSynchronousProxy> {
        proxy
            .is_simulating(zx::MonotonicInstant::after(SIMULATOR_PROBE_TIMEOUT))
            .ok()
            .map(|_| proxy)
    }

    fn get_or_connect_simulator(
        &self,
        kernel: &Kernel,
    ) -> Option<Arc<spower::BatterySimulatorSynchronousProxy>> {
        {
            let mut guard = self.simulator.lock();
            if let Some(sim) = guard.as_ref() {
                if !sim.is_closed().unwrap_or(true) {
                    return Some(Arc::clone(sim));
                }
                *guard = None;
            }
        }
        let proxy =
            fuchsia_component::client::connect_to_protocol_sync::<spower::BatterySimulatorMarker>()
                .ok()
                .and_then(Self::probe_simulator)
                .or_else(|| {
                    kernel
                        .connect_to_protocol_at_container_svc::<spower::BatterySimulatorMarker>()
                        .ok()
                        .map(|client_end| client_end.into_sync_proxy())
                        .and_then(Self::probe_simulator)
                })
                .map(Arc::new)?;
        let mut guard = self.simulator.lock();
        if let Some(sim) = guard.as_ref() {
            if !sim.is_closed().unwrap_or(true) {
                return Some(Arc::clone(sim));
            }
        }
        *guard = Some(Arc::clone(&proxy));
        Some(proxy)
    }

    fn clear_simulator(&self, disconnected: &Arc<spower::BatterySimulatorSynchronousProxy>) {
        let mut guard = self.simulator.lock();
        if guard.as_ref().is_some_and(|current| Arc::ptr_eq(current, disconnected)) {
            *guard = None;
        }
    }

    /// Propagates the capacity override or reset to `BatterySimulator` while `self.state.write()`
    /// is held, keeping `fake_capacity`, `simulator_koid`, and the one-way FIDL calls serialized.
    /// Returns `true` if the `BatterySimulator` channel was observed closed and should be evicted.
    fn propagate_to_simulator(
        battery: &mut BatteryState,
        simulator: Option<&spower::BatterySimulatorSynchronousProxy>,
        prev_fake_capacity: Option<u8>,
        fake_capacity: Option<u8>,
    ) -> bool {
        let Some(simulator) = simulator else {
            battery.simulator_koid = None;
            return false;
        };
        let sim_koid = simulator.as_channel().koid().ok();
        match fake_capacity {
            Some(cap) => {
                let was_active = sim_koid.is_some() && battery.simulator_koid == sim_koid;
                if !was_active {
                    if let Err(e) = simulator.disconnect_real_battery() {
                        log_warn!(
                            "Failed to call BatterySimulator::DisconnectRealBattery: {:?}",
                            e
                        );
                        battery.simulator_koid = None;
                        return e.is_closed();
                    }
                    battery.simulator_koid = sim_koid;
                }
                if !was_active || prev_fake_capacity != Some(cap) {
                    if let Err(e) = simulator.set_battery_percentage(cap as f32) {
                        log_warn!("Failed to call BatterySimulator::SetBatteryPercentage: {:?}", e);
                        if e.is_closed() {
                            battery.simulator_koid = None;
                            return true;
                        }
                    }
                }
            }
            None => {
                if battery.simulator_koid.take().is_some() {
                    if let Err(e) = simulator.reconnect_real_battery() {
                        log_warn!("Failed to call BatterySimulator::ReconnectRealBattery: {:?}", e);
                        return e.is_closed();
                    }
                }
            }
        }
        false
    }
}

impl BytesFileOps for BatteryCapacityFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = self.state.read();
        Ok(serialize_for_file(state.battery.effective_capacity()).into())
    }

    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let s = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?.trim();
        let val: i32 = s.parse().map_err(|_| errno!(EINVAL))?;
        let fake_capacity = if (0..=100).contains(&val) {
            Some(val as u8)
        } else if val < 0 {
            None
        } else {
            return error!(EINVAL);
        };
        let kernel = current_task.kernel();
        let simulator = self.get_or_connect_simulator(kernel);
        let (changed, simulator_closed) = {
            let mut state = self.state.write();
            let prev = state.battery.effective();
            let prev_fake_capacity = state.battery.fake_capacity;
            state.battery.fake_capacity = fake_capacity;
            let simulator_closed = Self::propagate_to_simulator(
                &mut state.battery,
                simulator.as_deref(),
                prev_fake_capacity,
                fake_capacity,
            );
            (state.battery.effective() != prev, simulator_closed)
        };
        if simulator_closed {
            if let Some(sim) = simulator.as_ref() {
                self.clear_simulator(sim);
            }
        }
        if changed {
            kernel.device_registry.dispatch_uevent(UEventAction::Change, self.device.clone());
        }
        Ok(())
    }
}

/// Handle to registered power supply devices in sysfs.
struct PowerSupplyDevices {
    battery: Device,
    usb: Device,
    ac: Device,
    wireless: Device,
    state: SharedPowerSupplyState,
}

impl PowerSupplyDevices {
    /// Update the shared state from a `BatteryInfo` struct and dispatch uevents for changed power
    /// supply devices.
    fn update_from_battery_info(&self, kernel: &Kernel, info: &fpower::BatteryInfo) {
        let (battery_changed, usb_changed, ac_changed, wireless_changed) = {
            let mut state = self.state.write();
            let prev = state.clone();
            state.update_from_battery_info(info);
            (
                state.battery.effective() != prev.battery.effective(),
                state.usb.effective() != prev.usb.effective() || state.usb_type != prev.usb_type,
                state.ac.effective() != prev.ac.effective(),
                state.wireless.effective() != prev.wireless.effective(),
            )
        };

        if battery_changed {
            kernel.device_registry.dispatch_uevent(UEventAction::Change, self.battery.clone());
        }
        if usb_changed {
            kernel.device_registry.dispatch_uevent(UEventAction::Change, self.usb.clone());
        }
        if ac_changed {
            kernel.device_registry.dispatch_uevent(UEventAction::Change, self.ac.clone());
        }
        if wireless_changed {
            kernel.device_registry.dispatch_uevent(UEventAction::Change, self.wireless.clone());
        }
    }
}

/// Handle events from a `BatteryInfoWatcher` stream, updating `devices` and sending replies.
///
/// Returns `Ok(true)` if at least one `OnChangeBatteryInfo` update was received before the stream
/// closed, or `Ok(false)` if the stream closed immediately without delivering an initial update.
async fn handle_battery_watcher_stream(
    kernel: &Kernel,
    devices: &PowerSupplyDevices,
    mut stream: fpower::BatteryInfoWatcherRequestStream,
) -> Result<bool, fidl::Error> {
    let mut received_update = false;
    while let Some(event) = stream.try_next().await? {
        received_update = true;
        match event {
            fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                // Keep `_wake_lease` alive until after sysfs/uevent updates and `responder.send()`
                // complete so the system cannot suspend mid-update.
                wake_lease: _wake_lease,
                responder,
            } => {
                devices.update_from_battery_info(kernel, &info);
                if let Err(e) = responder.send() {
                    log_warn!("Failed to send BatteryInfoWatcher response: {:?}", e);
                }
            }
        }
    }
    Ok(received_update)
}

async fn run_battery_watcher(
    kernel: &Kernel,
    devices: &PowerSupplyDevices,
    battery_manager: fpower::BatteryManagerProxy,
) -> Result<bool, fidl::Error> {
    let (client_end, server_end) =
        fidl::endpoints::create_endpoints::<fpower::BatteryInfoWatcherMarker>();
    battery_manager.watch(client_end)?;
    handle_battery_watcher_stream(kernel, devices, server_end.into_stream()).await
}

async fn monitor_battery_manager(kernel: Arc<Kernel>, devices: Arc<PowerSupplyDevices>) {
    if let Ok(proxy) =
        fuchsia_component::client::connect_to_protocol::<fpower::BatteryManagerMarker>()
    {
        match run_battery_watcher(&kernel, &devices, proxy).await {
            Ok(true) => return,
            Ok(false) => {
                log_warn!(
                    "BatteryInfoWatcher via kernel svc closed before initial update, trying container svc"
                );
            }
            Err(e) => {
                log_warn!(
                    "BatteryInfoWatcher via kernel svc closed ({:?}), trying container svc",
                    e
                );
            }
        }
    }

    if let Ok(client_end) =
        kernel.connect_to_protocol_at_container_svc::<fpower::BatteryManagerMarker>()
    {
        if let Err(e) = run_battery_watcher(&kernel, &devices, client_end.into_proxy()).await {
            log_warn!("BatteryInfoWatcher via container svc closed: {:?}", e);
        }
        return;
    }

    log_warn!("Failed to connect to BatteryManager");
}

/// Return a device directory builder that populates the common sysfs attributes (`type`, `online`,
/// optional `present`, `current_max`, and `voltage_max`) for a charger device (`usb`, `ac`, or
/// `wireless`).
fn build_charger_directory(
    charger_type: &'static [u8],
    state: &SharedPowerSupplyState,
    select: fn(&PowerSupplyState) -> &ChargerState,
    include_present: bool,
) -> impl Fn(&Device, &SimpleDirectoryMutator) + Send + Sync + 'static {
    let state = Arc::clone(state);
    move |device, dir| {
        build_device_directory(device, dir);
        dir.entry("type", BytesFile::new_node(charger_type.to_vec()), mode!(IFREG, 0o444));
        power_supply_file(dir, "online", &state, move |s| u8::from(select(s).online));
        if include_present {
            power_supply_file(dir, "present", &state, move |s| u8::from(select(s).online));
        }
        power_supply_file(dir, "current_max", &state, move |s| {
            let c = select(s);
            if c.online { c.current_max_ua } else { 0 }
        });
        power_supply_file(dir, "voltage_max", &state, move |s| {
            let c = select(s);
            if c.online { c.voltage_max_uv } else { 0 }
        });
    }
}

/// Register power supply devices under `/sys/class/power_supply/`.
fn register_power_supply_devices(kernel: &Kernel) -> Arc<PowerSupplyDevices> {
    let registry = &kernel.device_registry;
    let power_supply_class = registry.objects.power_supply_class();

    let state = Arc::new(LockDepRwLock::new(PowerSupplyState::default()));

    // 1. /sys/class/power_supply/battery
    let state_clone = Arc::clone(&state);
    let battery = registry.add_numberless_device(
        "battery".into(),
        /* parent = */ None,
        power_supply_class.clone(),
        move |device, dir| {
            let state = &state_clone;
            build_device_directory(device, dir);
            dir.entry("type", BytesFile::new_node(b"Battery\n".to_vec()), mode!(IFREG, 0o444));
            power_supply_file(dir, "present", state, |s| u8::from(s.battery.present));
            dir.entry(
                "capacity",
                BatteryCapacityFile::new_node(state, device),
                mode!(IFREG, 0o644),
            );
            power_supply_file(dir, "status", state, |s| s.battery.status);
            power_supply_file(dir, "health", state, |s| s.battery.health);
            power_supply_file(dir, "voltage_now", state, |s| s.battery.voltage_now_uv);
            power_supply_file(dir, "current_now", state, |s| s.battery.current_now_ua);
            power_supply_file(dir, "current_avg", state, |s| s.battery.current_avg_ua);
            power_supply_file(dir, "charge_counter", state, |s| s.battery.charge_counter_uah);
            power_supply_file(dir, "charge_full", state, |s| s.battery.charge_full_uah);
            power_supply_file(dir, "charge_full_design", state, |s| {
                s.battery.charge_full_design_uah
            });
            power_supply_file(dir, "temp", state, |s| s.battery.temp_celsius_tenths);
            power_supply_file(dir, "cycle_count", state, |s| s.battery.cycle_count);
            power_supply_file(dir, "capacity_level", state, |s| {
                s.battery.effective_capacity_level()
            });
            // Linux's `google_battery.c` sets `val->intval = -EINVAL` (-22) with `err = 0`
            // when not charging, which Linux sysfs formats as `"-22\n"` (and Android
            // `BatteryMonitor` on Iris parses into `batteryChargeTimeToFullNowSeconds = -22`).
            power_supply_file(dir, "time_to_full_now", state, |s| {
                s.battery.time_to_full_now_seconds.unwrap_or(-(starnix_uapi::EINVAL as i64))
            });
            power_supply_file(dir, "technology", state, |s| s.battery.technology);
        },
    );

    // 2. /sys/class/power_supply/usb
    let state_clone = Arc::clone(&state);
    let build_usb =
        build_charger_directory(b"USB\n", &state, |s| &s.usb, /* include_present = */ true);
    let usb = registry.add_numberless_device(
        "usb".into(),
        /* parent = */ None,
        power_supply_class.clone(),
        move |device, dir| {
            build_usb(device, dir);
            power_supply_file(dir, "usb_type", &state_clone, |s| s.usb_type);
        },
    );

    // 3. /sys/class/power_supply/ac
    let ac = registry.add_numberless_device(
        "ac".into(),
        /* parent = */ None,
        power_supply_class.clone(),
        build_charger_directory(b"Mains\n", &state, |s| &s.ac, /* include_present = */ false),
    );

    // 4. /sys/class/power_supply/wireless
    let wireless = registry.add_numberless_device(
        "wireless".into(),
        /* parent = */ None,
        power_supply_class,
        build_charger_directory(
            b"Wireless\n",
            &state,
            |s| &s.wireless,
            /* include_present = */ true,
        ),
    );

    Arc::new(PowerSupplyDevices { battery, usb, ac, wireless, state })
}

/// Initialize power supply devices under `/sys/class/power_supply/` and spawn watcher kthread.
pub fn battery_manager_device_init(kernel: &Arc<Kernel>) {
    let devices = register_power_supply_devices(kernel);
    let kernel_clone = Arc::clone(kernel);
    kernel.kthreads.spawn_future(
        move || async move {
            monitor_battery_manager(kernel_clone, devices).await;
        },
        "power_supply_monitor",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::{anon_test_file, spawn_kernel_and_run};
    use starnix_core::vfs::{FsNodeHandle, VecInputBuffer, VecOutputBuffer};
    use starnix_uapi::open_flags::OpenFlags;
    use std::sync::Mutex;

    fn read_sysfs_node(current_task: &CurrentTask, node: &FsNodeHandle) -> String {
        let ops = node.create_file_ops(current_task, OpenFlags::RDONLY).unwrap();
        let file = anon_test_file(current_task, ops, OpenFlags::RDONLY);
        let mut output = VecOutputBuffer::new(64);
        let n = file.read(current_task, &mut output).unwrap();
        String::from_utf8(Vec::from(output)[..n].to_vec()).unwrap()
    }

    #[::fuchsia::test]
    fn test_default_state() {
        let state = PowerSupplyState::default();
        assert!(state.battery.present);
        assert_eq!(state.battery.capacity, 0);
        assert_eq!(state.battery.fake_capacity, None);
        assert_eq!(state.battery.status, "Unknown");
        assert_eq!(state.battery.health, "Unknown");
        assert_eq!(state.battery.voltage_now_uv, 0);
        assert_eq!(state.battery.current_now_ua, 0);
        assert_eq!(state.battery.time_to_full_now_seconds, None);
        assert_eq!(state.battery.technology, "Li-ion");
        assert!(!state.usb.online);
        assert_eq!(state.usb_type, "[Unknown] SDP DCP CDP");
        assert_eq!(state.usb.voltage_max_uv, DEFAULT_CHARGER_MAX_VOLTAGE_UV);
        assert!(!state.ac.online);
        assert!(!state.wireless.online);
    }

    #[::fuchsia::test]
    fn test_update_from_battery_info() {
        let mut state = PowerSupplyState::default();
        let info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Ok),
            charge_status: Some(fpower::ChargeStatus::Charging),
            charge_source: Some(fpower::ChargeSource::Usb),
            level_percent: Some(80.0),
            level_status: Some(fpower::LevelStatus::Warning),
            health: Some(fpower::HealthStatus::Warm),
            present_voltage_mv: Some(3950),
            present_charging_current_ua: Some(1500000),
            average_charging_current_ua: Some(1400000),
            remaining_charge_uah: Some(2500000),
            full_capacity_uah: Some(3000000),
            temperature_mc: Some(26500),
            battery_spec: Some(fpower::BatterySpec {
                design_capacity_uah: Some(3100000),
                max_charging_current_ua: Some(2000000),
                max_charging_voltage_uv: Some(9000000),
                ..Default::default()
            }),
            time_remaining: Some(fpower::TimeRemaining::FullCharge(1800_000_000_000)),
            ..Default::default()
        };

        state.update_from_battery_info(&info);

        assert!(state.battery.present);
        assert_eq!(state.battery.capacity, 80);
        assert_eq!(state.battery.status, "Charging");
        assert_eq!(state.battery.health, "Warm");
        assert_eq!(state.battery.capacity_level, "High");
        assert_eq!(state.battery.voltage_now_uv, 3950000);
        assert_eq!(state.battery.current_now_ua, 1500000);
        assert_eq!(state.battery.current_avg_ua, 1400000);
        assert_eq!(state.battery.charge_counter_uah, 2500000);
        assert_eq!(state.battery.charge_full_uah, 3000000);
        assert_eq!(state.battery.charge_full_design_uah, 3100000);
        assert_eq!(state.battery.temp_celsius_tenths, 265);
        assert_eq!(state.battery.time_to_full_now_seconds, Some(1800));

        assert!(state.usb.online);
        assert_eq!(state.usb_type, "Unknown [SDP] DCP CDP");
        assert_eq!(state.usb.current_max_ua, 2000000);
        assert_eq!(state.usb.voltage_max_uv, 9000000);

        assert!(!state.ac.online);
        assert!(!state.wireless.online);

        let discharging_info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::NotAvailable),
            charge_status: Some(fpower::ChargeStatus::Discharging),
            charge_source: Some(fpower::ChargeSource::None),
            level_percent: Some(40.0),
            health: Some(fpower::HealthStatus::Cool),
            time_remaining: Some(fpower::TimeRemaining::BatteryLife(3600_000_000_000)),
            ..Default::default()
        };
        state.update_from_battery_info(&discharging_info);
        assert!(state.battery.present);
        assert_eq!(state.battery.status, "Discharging");
        assert_eq!(state.battery.health, "Cool");
        assert_eq!(state.battery.capacity, 40);
        assert_eq!(state.battery.capacity_level, "Normal");
        assert!(!state.usb.online);
        assert_eq!(state.usb_type, "[Unknown] SDP DCP CDP");
        assert_eq!(state.battery.time_to_full_now_seconds, None);

        // Negative FullCharge duration should be ignored rather than underflowing.
        let negative_ttf_info = fpower::BatteryInfo {
            time_remaining: Some(fpower::TimeRemaining::FullCharge(-100)),
            ..Default::default()
        };
        state.update_from_battery_info(&negative_ttf_info);
        assert_eq!(state.battery.time_to_full_now_seconds, None);

        let wireless_info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::NotPresent),
            charge_status: Some(fpower::ChargeStatus::Full),
            charge_source: Some(fpower::ChargeSource::Wireless),
            level_percent: Some(100.0),
            ..Default::default()
        };
        state.update_from_battery_info(&wireless_info);
        assert!(!state.battery.present);
        assert!(state.wireless.online);
        assert!(!state.usb.online);
        assert!(!state.ac.online);
        assert_eq!(state.battery.time_to_full_now_seconds, None);
    }

    #[::fuchsia::test]
    async fn power_supply_registers_devices_and_attributes() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let _devices = register_power_supply_devices(kernel);

            let root = &kernel.device_registry.objects.root;

            assert!(root.lookup("class/power_supply/battery".into()).is_some());
            assert!(root.lookup("class/power_supply/usb".into()).is_some());
            assert!(root.lookup("class/power_supply/ac".into()).is_some());
            assert!(root.lookup("class/power_supply/wireless".into()).is_some());

            let battery_capacity = root
                .lookup("devices/virtual/power_supply/battery/capacity".into())
                .expect("battery capacity node should exist");
            assert_eq!(battery_capacity.info().mode, mode!(IFREG, 0o644));
            assert!(
                root.lookup("devices/virtual/power_supply/battery/capacity_level".into()).is_some()
            );
            assert!(root.lookup("devices/virtual/power_supply/battery/status".into()).is_some());
            assert!(
                root.lookup("devices/virtual/power_supply/battery/voltage_now".into()).is_some()
            );
            assert!(root.lookup("devices/virtual/power_supply/battery/temp".into()).is_some());

            let ttf_node = root
                .lookup("devices/virtual/power_supply/battery/time_to_full_now".into())
                .expect("battery time_to_full_now node should exist");
            assert_eq!(read_sysfs_node(current_task, &ttf_node), "-22\n");

            assert!(root.lookup("devices/virtual/power_supply/usb/online".into()).is_some());
            assert!(root.lookup("devices/virtual/power_supply/usb/present".into()).is_some());
            assert!(root.lookup("devices/virtual/power_supply/usb/usb_type".into()).is_some());
            assert!(root.lookup("devices/virtual/power_supply/usb/current_max".into()).is_some());

            assert!(root.lookup("devices/virtual/power_supply/ac/online".into()).is_some());
            assert!(root.lookup("devices/virtual/power_supply/ac/present".into()).is_none());

            assert!(root.lookup("devices/virtual/power_supply/wireless/online".into()).is_some());
            assert!(root.lookup("devices/virtual/power_supply/wireless/present".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn battery_capacity_write_overrides_and_dispatches_uevent() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let _devices = register_power_supply_devices(kernel);

            let registry = &kernel.device_registry;
            let root = &registry.objects.root;
            let battery_capacity = root
                .lookup("devices/virtual/power_supply/battery/capacity".into())
                .expect("battery capacity node should exist");
            let capacity_level_node = root
                .lookup("devices/virtual/power_supply/battery/capacity_level".into())
                .expect("battery capacity_level node should exist");

            let uevents = Arc::new(Mutex::new(Vec::new()));
            let uevents_clone = Arc::clone(&uevents);
            let listener_key =
                registry.register_listener(move |action, device: Device, _context| {
                    uevents_clone.lock().unwrap().push((action, device.name().to_string()));
                });

            let file_ops = battery_capacity.create_file_ops(current_task, OpenFlags::RDWR).unwrap();
            let file = anon_test_file(current_task, file_ops, OpenFlags::RDWR);
            assert_eq!(read_sysfs_node(current_task, &battery_capacity), "0\n");
            assert_eq!(read_sysfs_node(current_task, &capacity_level_node), "Unknown\n");

            let mut input = VecInputBuffer::new(b"77\n");
            assert_eq!(file.write(current_task, &mut input).unwrap(), 3);
            assert_eq!(read_sysfs_node(current_task, &battery_capacity), "77\n");
            assert_eq!(read_sysfs_node(current_task, &capacity_level_node), "Normal\n");
            assert_eq!(
                *uevents.lock().unwrap(),
                vec![(UEventAction::Change, "battery".to_string())]
            );

            // Writing the same capacity again should not emit a redundant uevent.
            let mut same_input = VecInputBuffer::new(b"77\n");
            assert_eq!(file.write(current_task, &mut same_input).unwrap(), 3);
            assert_eq!(uevents.lock().unwrap().len(), 1);

            let mut reset_input = VecInputBuffer::new(b"-1\n");
            assert_eq!(file.write(current_task, &mut reset_input).unwrap(), 3);
            assert_eq!(read_sysfs_node(current_task, &battery_capacity), "0\n");
            assert_eq!(read_sysfs_node(current_task, &capacity_level_node), "Unknown\n");
            assert_eq!(
                *uevents.lock().unwrap(),
                vec![
                    (UEventAction::Change, "battery".to_string()),
                    (UEventAction::Change, "battery".to_string()),
                ]
            );
            registry.unregister_listener(&listener_key);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn update_from_battery_info_dispatches_uevents_for_changed_devices() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let devices = register_power_supply_devices(kernel);
            let registry = &kernel.device_registry;

            let uevents = Arc::new(Mutex::new(Vec::new()));
            let uevents_clone = Arc::clone(&uevents);
            let listener_key =
                registry.register_listener(move |action, device: Device, _context| {
                    uevents_clone.lock().unwrap().push((action, device.name().to_string()));
                });

            let info = fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::Ok),
                charge_status: Some(fpower::ChargeStatus::Full),
                charge_source: Some(fpower::ChargeSource::AcAdapter),
                level_percent: Some(100.0),
                level_status: Some(fpower::LevelStatus::Ok),
                battery_spec: Some(fpower::BatterySpec {
                    max_charging_current_ua: Some(2_000_000),
                    max_charging_voltage_uv: Some(9_000_000),
                    ..Default::default()
                }),
                ..Default::default()
            };
            devices.update_from_battery_info(kernel, &info);

            {
                let state = devices.state.read();
                assert_eq!(state.battery.capacity, 100);
                assert_eq!(state.battery.capacity_level, "Full");
                assert_eq!(state.battery.status, "Full");
                assert!(state.usb.online);
                assert_eq!(state.usb_type, "Unknown SDP [DCP] CDP");
                assert!(state.ac.online);
                assert!(!state.wireless.online);
            }
            assert_eq!(
                *uevents.lock().unwrap(),
                vec![
                    (UEventAction::Change, "battery".to_string()),
                    (UEventAction::Change, "usb".to_string()),
                    (UEventAction::Change, "ac".to_string()),
                ]
            );
            registry.unregister_listener(&listener_key);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_battery_watcher_stream_handling() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let devices = register_power_supply_devices(kernel);

            let (client_end, server_end) =
                fidl::endpoints::create_endpoints::<fpower::BatteryInfoWatcherMarker>();
            let proxy = client_end.into_proxy();

            let kernel_clone = kernel.clone();
            let devices_clone = devices.clone();
            let handler_task = fuchsia_async::Task::local(async move {
                handle_battery_watcher_stream(
                    &kernel_clone,
                    &devices_clone,
                    server_end.into_stream(),
                )
                .await
            });

            let info = fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::Ok),
                charge_status: Some(fpower::ChargeStatus::Charging),
                charge_source: Some(fpower::ChargeSource::Usb),
                level_percent: Some(73.0),
                level_status: Some(fpower::LevelStatus::Ok),
                present_voltage_mv: Some(4100),
                ..Default::default()
            };

            proxy
                .on_change_battery_info(&info, None)
                .await
                .expect("on_change_battery_info should succeed");

            // Drop client to close stream and complete handler_task
            std::mem::drop(proxy);
            assert!(handler_task.await.expect("handler should complete without error"));

            let state = devices.state.read();
            assert_eq!(state.battery.capacity, 73);
            assert_eq!(state.battery.status, "Charging");
            assert!(state.usb.online);
            assert_eq!(state.battery.voltage_now_uv, 4100000);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_battery_capacity_write_propagates_to_simulator() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let devices = register_power_supply_devices(kernel);

            let (client_end, server_end) =
                fidl::endpoints::create_endpoints::<spower::BatterySimulatorMarker>();
            let simulator = LockDepMutex::new(Some(Arc::new(client_end.into_sync_proxy())));

            let file_ops = Box::new(BytesFile::new(BatteryCapacityFile {
                state: devices.state.clone(),
                device: devices.battery.clone(),
                simulator,
            }));
            let file = anon_test_file(current_task, file_ops, OpenFlags::RDWR);

            // Seed initial real battery capacity before faking.
            devices.update_from_battery_info(
                kernel,
                &fpower::BatteryInfo { level_percent: Some(50.0), ..Default::default() },
            );
            assert_eq!(devices.state.read().battery.effective_capacity(), 50);

            let mut input = VecInputBuffer::new(b"77\n");
            assert_eq!(file.write(current_task, &mut input).unwrap(), 3);
            assert_eq!(devices.state.read().battery.effective_capacity(), 77);

            // Simulator echo while `simulator_koid` is `Some` must not overwrite the underlying
            // real capacity (`50`), so resetting via `-1` immediately restores `50`.
            devices.update_from_battery_info(
                kernel,
                &fpower::BatteryInfo { level_percent: Some(77.0), ..Default::default() },
            );
            assert_eq!(devices.state.read().battery.capacity, 50);
            assert_eq!(devices.state.read().battery.effective_capacity(), 77);

            // Consecutive override (`77` -> `80`) updates percentage without calling
            // `DisconnectRealBattery` again, and duplicate write of `80` is a no-op.
            let mut second_input = VecInputBuffer::new(b"80\n");
            assert_eq!(file.write(current_task, &mut second_input).unwrap(), 3);
            assert_eq!(devices.state.read().battery.effective_capacity(), 80);

            let mut duplicate_input = VecInputBuffer::new(b"80\n");
            assert_eq!(file.write(current_task, &mut duplicate_input).unwrap(), 3);

            let mut reset_input = VecInputBuffer::new(b"-1\n");
            assert_eq!(file.write(current_task, &mut reset_input).unwrap(), 3);
            assert_eq!(devices.state.read().battery.effective_capacity(), 50);

            // Writing `-1` again when already not simulating should not send a redundant
            // `ReconnectRealBattery`.
            let mut second_reset_input = VecInputBuffer::new(b"-1\n");
            assert_eq!(file.write(current_task, &mut second_reset_input).unwrap(), 3);
            std::mem::drop(file);
            current_task.trigger_delayed_releaser();

            let mut stream = server_end.into_stream();
            assert!(matches!(
                stream.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::DisconnectRealBattery { .. })
            ));
            assert!(matches!(
                stream.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::SetBatteryPercentage { percent, .. })
                    if (percent - 77.0).abs() < f32::EPSILON
            ));
            assert!(matches!(
                stream.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::SetBatteryPercentage { percent, .. })
                    if (percent - 80.0).abs() < f32::EPSILON
            ));
            assert!(matches!(
                stream.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::ReconnectRealBattery { .. })
            ));
            assert!(stream.try_next().await.unwrap().is_none());

            // Verify closed-channel eviction and re-sending `DisconnectRealBattery` +
            // `SetBatteryPercentage` when a new `BatterySimulator` proxy connects after the
            // previous server end closed while simulating.
            let (client_end_a, server_end_a) =
                fidl::endpoints::create_endpoints::<spower::BatterySimulatorMarker>();
            let capacity_file = BatteryCapacityFile {
                state: devices.state.clone(),
                device: devices.battery.clone(),
                simulator: LockDepMutex::new(Some(Arc::new(client_end_a.into_sync_proxy()))),
            };
            BytesFileOps::write(&capacity_file, current_task, b"65\n".to_vec()).unwrap();
            assert!(devices.state.read().battery.simulator_koid.is_some());

            // Drop `server_end_a` to simulate `battery_manager` restarting and closing the channel.
            std::mem::drop(server_end_a);

            // Writing while the cached channel is closed evicts it and clears `simulator_koid`.
            BytesFileOps::write(&capacity_file, current_task, b"65\n".to_vec()).unwrap();
            assert!(capacity_file.simulator.lock().is_none());
            assert!(devices.state.read().battery.simulator_koid.is_none());

            // Installing a newly connected proxy and writing the same percentage (`65\n`) must
            // send both `DisconnectRealBattery` and `SetBatteryPercentage(65.0)` on the new channel.
            let (client_end_b, server_end_b) =
                fidl::endpoints::create_endpoints::<spower::BatterySimulatorMarker>();
            *capacity_file.simulator.lock() = Some(Arc::new(client_end_b.into_sync_proxy()));
            BytesFileOps::write(&capacity_file, current_task, b"65\n".to_vec()).unwrap();
            assert!(devices.state.read().battery.simulator_koid.is_some());
            std::mem::drop(capacity_file);

            let mut stream_b = server_end_b.into_stream();
            assert!(matches!(
                stream_b.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::DisconnectRealBattery { .. })
            ));
            assert!(matches!(
                stream_b.try_next().await.unwrap(),
                Some(spower::BatterySimulatorRequest::SetBatteryPercentage { percent, .. })
                    if (percent - 65.0).abs() < f32::EPSILON
            ));
            assert!(stream_b.try_next().await.unwrap().is_none());
        })
        .await;
    }
}
