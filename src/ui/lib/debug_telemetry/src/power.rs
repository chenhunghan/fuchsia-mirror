// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Battery state, fuel-gauge power flow, and system suspend residency telemetry via
//! `fuchsia.power.battery.BatteryManager`, `fuchsia.hardware.power.battery.Service` fallback, and
//! Zircon monotonic/boot clocks.

use fidl_fuchsia_hardware_power_battery as fhwbattery;
use fidl_fuchsia_power_battery as fbattery;
use fuchsia_component::client::{Service, connect_to_protocol};
use std::fmt;

const MILLIDEGREES_PER_DEGREE_CELSIUS: f32 = 1000.0;
const MICROVOLTS_PER_MILLIVOLT: u32 = 1000;
const MICROAMPERES_PER_MILLIAMPERE: i32 = 1000;
const NANOWATTS_PER_MILLIWATT: i64 = 1_000_000;
const PICOWATTS_PER_MILLIWATT: i128 = 1_000_000_000;
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const FULL_PERCENT: f64 = 100.0;
const HW_CURRENT_EMA_ALPHA: f32 = 1.0 / 3.0;
const INITIAL_HW_DISCOVERY_BACKOFF_NS: i64 = 1_000_000_000;
const MAX_HW_DISCOVERY_BACKOFF_NS: i64 = 60_000_000_000;

/// Samples `BootInstant` before `MonotonicInstant` so scheduler preemption between the two reads
/// cannot inflate `(boot - monotonic)` into phantom suspend residency.
pub(crate) fn sample_clocks() -> (zx::MonotonicInstant, zx::BootInstant) {
    let boot = zx::BootInstant::get();
    let monotonic = zx::MonotonicInstant::get();
    (monotonic, boot)
}

/// Battery charge state reported by `fuchsia.power.battery.BatteryManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum ChargeStatus {
    /// Battery is actively charging.
    Charging,
    /// Battery is discharging.
    Discharging,
    /// Battery is full.
    Full,
    /// External power is present or managed without actively charging the battery.
    NotCharging,
    /// Charge state is unknown.
    Unknown,
}

impl ChargeStatus {
    /// Returns a compact human-readable label for this charge state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "Charging",
            Self::Discharging => "Discharging",
            Self::Full => "Full",
            Self::NotCharging => "NotCharging",
            Self::Unknown => "Unknown",
        }
    }

    fn from_fidl(status: fbattery::ChargeStatus) -> Self {
        match status {
            fbattery::ChargeStatus::Charging => Self::Charging,
            fbattery::ChargeStatus::Discharging => Self::Discharging,
            fbattery::ChargeStatus::Full => Self::Full,
            fbattery::ChargeStatus::NotCharging => Self::NotCharging,
            fbattery::ChargeStatus::Unknown => Self::Unknown,
        }
    }
}

impl fmt::Display for ChargeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// External power source reported by `fuchsia.power.battery.BatteryManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum ChargeSource {
    /// USB power source.
    Usb,
    /// AC wall adapter power source.
    AcAdapter,
    /// Inductive/wireless power source.
    Wireless,
    /// No external power source attached.
    None,
    /// Power source is unknown.
    Unknown,
}

impl ChargeSource {
    /// Returns a compact human-readable label for this power source.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::AcAdapter => "AC",
            Self::Wireless => "Wireless",
            Self::None => "None",
            Self::Unknown => "Unknown",
        }
    }

    fn from_fidl(source: fbattery::ChargeSource) -> Self {
        match source {
            fbattery::ChargeSource::Usb => Self::Usb,
            fbattery::ChargeSource::AcAdapter => Self::AcAdapter,
            fbattery::ChargeSource::Wireless => Self::Wireless,
            fbattery::ChargeSource::None => Self::None,
            fbattery::ChargeSource::Unknown => Self::Unknown,
        }
    }
}

impl fmt::Display for ChargeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshot of battery charge state, temperature, fuel-gauge power flow, active/boot uptime, and
/// cumulative suspend residency.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct PowerMetrics {
    /// Battery charge level percentage (`0.0..=100.0`).
    pub battery_percent: Option<f32>,
    /// Battery charge status, or `None` when `BatteryManager` is unavailable.
    pub charge_status: Option<ChargeStatus>,
    /// External power source, or `None` when `BatteryManager` is unavailable.
    pub charge_source: Option<ChargeSource>,
    /// Battery temperature in degrees Celsius.
    pub battery_temp_c: Option<f32>,
    /// Present battery terminal voltage in millivolts.
    pub voltage_mv: Option<u32>,
    /// Present battery current in milliamperes (**positive when charging**, **negative when
    /// discharging**).
    pub current_ma: Option<i32>,
    /// Rolling average battery current in milliamperes (**positive when charging**, **negative when
    /// discharging**).
    pub avg_current_ma: Option<i32>,
    /// Present battery power flow in milliwatts (**positive when charging**, **negative when
    /// discharging**), clamped to `i32::MIN..=i32::MAX`.
    pub power_mw: Option<i32>,
    /// Active (non-suspended) system uptime in seconds (`zx::MonotonicInstant`).
    pub mono_uptime_secs: u64,
    /// Total wall-clock system uptime since boot in seconds (`zx::BootInstant`, including suspend).
    pub boot_uptime_secs: u64,
    /// Cumulative percentage of total boot uptime spent in system suspend (`0.0..=100.0`).
    pub suspend_residency_pct: f32,
}

/// Queries battery fuel-gauge metrics from `fuchsia.power.battery.BatteryManager` with automatic
/// fallback to `fuchsia.hardware.power.battery.Service` on minimal builds, alongside active vs.
/// boot uptime and cumulative suspend residency from Zircon clocks.
#[derive(Debug)]
pub struct PowerCollector {
    manager_proxy: Option<fbattery::BatteryManagerProxy>,
    hw_proxy: Option<fhwbattery::BatteryProxy>,
    auto_discover_hw: bool,
    hw_dir_missing: bool,
    next_hw_discovery_ns: Option<i64>,
    hw_discovery_backoff_ns: i64,
    smoothed_hw_current_ua: Option<f32>,
}

impl PowerCollector {
    /// Connects to `fuchsia.power.battery.BatteryManager` in the incoming namespace if available,
    /// falling back to `fuchsia.hardware.power.battery.Service` if `BatteryManager` is absent.
    pub fn from_namespace() -> Self {
        Self {
            manager_proxy: connect_to_protocol::<fbattery::BatteryManagerMarker>().ok(),
            hw_proxy: None,
            auto_discover_hw: true,
            hw_dir_missing: false,
            next_hw_discovery_ns: None,
            hw_discovery_backoff_ns: INITIAL_HW_DISCOVERY_BACKOFF_NS,
            smoothed_hw_current_ua: None,
        }
    }

    /// Creates an inert collector with all battery protocol/service connections disabled.
    pub fn disconnected() -> Self {
        Self::with_proxies(None, None)
    }

    /// Creates a collector backed by an explicit `BatteryManagerProxy` without hardware service
    /// discovery.
    pub fn with_proxy(proxy: Option<fbattery::BatteryManagerProxy>) -> Self {
        Self::with_proxies(proxy, None)
    }

    /// Creates a collector backed by an explicit `fuchsia.hardware.power.battery.BatteryProxy`
    /// without `BatteryManager` or filesystem service discovery.
    pub fn with_hardware_proxy(hw_proxy: Option<fhwbattery::BatteryProxy>) -> Self {
        Self::with_proxies(None, hw_proxy)
    }

    /// Creates a collector backed by explicit `BatteryManager` and hardware `Battery` proxies,
    /// falling back to `hw_proxy` if `manager_proxy` is `None` or closes.
    pub fn with_proxies(
        manager_proxy: Option<fbattery::BatteryManagerProxy>,
        hw_proxy: Option<fhwbattery::BatteryProxy>,
    ) -> Self {
        Self {
            manager_proxy,
            hw_proxy,
            auto_discover_hw: false,
            hw_dir_missing: false,
            next_hw_discovery_ns: None,
            hw_discovery_backoff_ns: INITIAL_HW_DISCOVERY_BACKOFF_NS,
            smoothed_hw_current_ua: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.manager_proxy.is_some() || self.hw_proxy.is_some() || self.auto_discover_hw
    }

    async fn ensure_hw_battery(&mut self, clamped_mono_ns: i64) {
        if self.hw_proxy.is_some() || !self.auto_discover_hw || self.hw_dir_missing {
            return;
        }
        if self.next_hw_discovery_ns.is_some_and(|next_ns| clamped_mono_ns < next_ns) {
            return;
        }

        let Ok(service) = Service::open(fhwbattery::ServiceMarker) else {
            self.hw_dir_missing = true;
            return;
        };
        let Ok(instances) = service.enumerate().await else {
            self.hw_dir_missing = true;
            return;
        };

        for instance in instances {
            if let Ok(proxy) = instance.connect_to_battery() {
                self.hw_proxy = Some(proxy);
                self.next_hw_discovery_ns = None;
                self.hw_discovery_backoff_ns = INITIAL_HW_DISCOVERY_BACKOFF_NS;
                return;
            }
        }

        self.next_hw_discovery_ns =
            Some(clamped_mono_ns.saturating_add(self.hw_discovery_backoff_ns));
        self.hw_discovery_backoff_ns =
            self.hw_discovery_backoff_ns.saturating_mul(2).min(MAX_HW_DISCOVERY_BACKOFF_NS);
    }

    fn apply_battery_manager_info(metrics: &mut PowerMetrics, info: fbattery::BatteryInfo) {
        metrics.battery_percent =
            info.level_percent.filter(|pct| pct.is_finite()).map(|pct| pct.clamp(0.0, 100.0));
        metrics.charge_status = info.charge_status.map(ChargeStatus::from_fidl);
        metrics.charge_source = info.charge_source.map(ChargeSource::from_fidl);
        metrics.battery_temp_c =
            info.temperature_mc.map(|temp_mc| temp_mc as f32 / MILLIDEGREES_PER_DEGREE_CELSIUS);
        metrics.voltage_mv = info.present_voltage_mv;
        if let Some(current_ua) = info.present_charging_current_ua {
            metrics.current_ma = Some(current_ua / MICROAMPERES_PER_MILLIAMPERE);
            if let Some(mv) = metrics.voltage_mv {
                let power_mw = (mv as i64 * current_ua as i64) / NANOWATTS_PER_MILLIWATT;
                metrics.power_mw = Some(power_mw.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            }
        }
        metrics.avg_current_ma =
            info.average_charging_current_ua.map(|avg_ua| avg_ua / MICROAMPERES_PER_MILLIAMPERE);
    }

    fn apply_hw_battery_status(&mut self, metrics: &mut PowerMetrics, status: fhwbattery::Status) {
        if status.present == Some(false) {
            self.smoothed_hw_current_ua = None;
            return;
        }
        metrics.battery_percent =
            status.level_percent.filter(|pct| pct.is_finite()).map(|pct| pct.clamp(0.0, 100.0));
        metrics.battery_temp_c = status.temp_celsius.filter(|temp| temp.is_finite());
        metrics.voltage_mv = status.voltage_uv.map(|uv| uv / MICROVOLTS_PER_MILLIVOLT);
        if let Some(current_ua) = status.current_ua {
            metrics.current_ma = Some(current_ua / MICROAMPERES_PER_MILLIAMPERE);
            let avg_ua = match self.smoothed_hw_current_ua {
                Some(prev) => prev + HW_CURRENT_EMA_ALPHA * (current_ua as f32 - prev),
                None => current_ua as f32,
            };
            self.smoothed_hw_current_ua = Some(avg_ua);
            metrics.avg_current_ma = Some((avg_ua.round() as i32) / MICROAMPERES_PER_MILLIAMPERE);
            if let Some(uv) = status.voltage_uv {
                let power_mw = (uv as i128 * current_ua as i128) / PICOWATTS_PER_MILLIWATT;
                metrics.power_mw = Some(power_mw.clamp(i32::MIN as i128, i32::MAX as i128) as i32);
            }
        }
    }

    /// Collects [`PowerMetrics`] using current Zircon boot and monotonic timestamps.
    pub async fn collect(&mut self) -> PowerMetrics {
        let (monotonic, boot) = sample_clocks();
        self.collect_at(monotonic, boot).await
    }

    /// Collects [`PowerMetrics`] at explicit `monotonic` and `boot` instants.
    pub async fn collect_at(
        &mut self,
        monotonic: zx::MonotonicInstant,
        boot: zx::BootInstant,
    ) -> PowerMetrics {
        let clamped_mono_ns = monotonic.into_nanos().max(0);
        let clamped_boot_ns = boot.into_nanos().max(clamped_mono_ns);
        let suspend_ns = clamped_boot_ns.saturating_sub(clamped_mono_ns);
        let suspend_residency_pct = if clamped_boot_ns > 0 {
            ((suspend_ns as f64 / clamped_boot_ns as f64) * FULL_PERCENT) as f32
        } else {
            0.0
        };

        let mut metrics = PowerMetrics {
            mono_uptime_secs: (clamped_mono_ns / NANOSECONDS_PER_SECOND) as u64,
            boot_uptime_secs: (clamped_boot_ns / NANOSECONDS_PER_SECOND) as u64,
            suspend_residency_pct,
            ..Default::default()
        };

        if let Some(proxy) = self.manager_proxy.as_ref() {
            match proxy.get_battery_info().await {
                Ok(info) => {
                    Self::apply_battery_manager_info(&mut metrics, info);
                    return metrics;
                }
                Err(err) if err.is_closed() => {
                    self.manager_proxy = None;
                }
                Err(_) => return metrics,
            }
        }

        self.ensure_hw_battery(clamped_mono_ns).await;
        let Some(hw_proxy) = self.hw_proxy.as_ref() else {
            return metrics;
        };
        match hw_proxy.get_status().await {
            Ok(Ok(status)) => {
                self.apply_hw_battery_status(&mut metrics, status);
            }
            Ok(Err(_)) => {}
            Err(err) => {
                if err.is_closed() {
                    self.hw_proxy = None;
                    self.smoothed_hw_current_ua = None;
                    if self.auto_discover_hw {
                        self.next_hw_discovery_ns =
                            Some(clamped_mono_ns.saturating_add(self.hw_discovery_backoff_ns));
                        self.hw_discovery_backoff_ns = self
                            .hw_discovery_backoff_ns
                            .saturating_mul(2)
                            .min(MAX_HW_DISCOVERY_BACKOFF_NS);
                    }
                }
            }
        }

        metrics
    }
}
