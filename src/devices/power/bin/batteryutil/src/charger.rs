// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Charger Client and Controller (supporting both
//! fuchsia.hardware.power.charger and fuchsia.power.battery.Charger).

use crate::common::{
    MEMBER_CHARGER, MEMBER_DEBUG, MEMBER_DEVICE, MicroUnit, append_member_suffix, select_instance,
};
use crate::spmi::{self, PowerSource};
use anyhow::{Context, Result, anyhow};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpowerbattery;
use fuchsia_component::client::connect_to_protocol_at_path;
use std::fmt;

struct DisplayOperatingMode(fcharger::OperatingMode);

impl fmt::Display for DisplayOperatingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::OperatingMode::Charging => "Charging",
            fcharger::OperatingMode::Passthrough => "Passthrough",
            fcharger::OperatingMode::Discharging => "Discharging",
            fcharger::OperatingMode::Otg => "OTG",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargePhase(fcharger::ChargePhase);

impl fmt::Display for DisplayChargePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::ChargePhase::None => "None",
            fcharger::ChargePhase::Trickle => "Trickle",
            fcharger::ChargePhase::Fast => "Fast",
            fcharger::ChargePhase::Taper => "Taper",
            fcharger::ChargePhase::TopOff => "Top-Off",
            fcharger::ChargePhase::Done => "Done",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplaySourceType(fcharger::SourceType);

impl fmt::Display for DisplaySourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::SourceType::Ac => "AC",
            fcharger::SourceType::Usb => "USB",
            fcharger::SourceType::Wireless => "Wireless",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargerHealth(fcharger::Health);

impl fmt::Display for DisplayChargerHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::Health::Good => "Good",
            fcharger::Health::InputOverVoltage => "Input Over-Voltage",
            fcharger::Health::InputUnderVoltage => "Input Under-Voltage",
            fcharger::Health::ThermalWarning => "Thermal Warning",
            fcharger::Health::ThermalShutdown => "Thermal Shutdown",
            fcharger::Health::SafetyTimerExpired => "Safety Timer Expired",
            fcharger::Health::BatteryOverVoltage => "Battery Over-Voltage",
            fcharger::Health::WatchdogTimerExpired => "Watchdog Timer Expired",
            fcharger::Health::UnspecifiedFailure => "Unspecified Failure",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargerSpec<'a>(&'a fcharger::Spec);

impl fmt::Display for DisplayChargerSpec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = self.0;
        if let Some(model) = &spec.model {
            writeln!(f, "Charger Model: {model}")?;
        }
        if let Some(v) = spec.max_charge_current_ua {
            writeln!(f, "Max Charge Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.max_charge_voltage_uv {
            writeln!(f, "Max Charge Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.max_input_current_ua {
            writeln!(f, "Max Input Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.precharge_current_ua {
            writeln!(f, "Precharge Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.charge_term_current_ua {
            writeln!(f, "Spec Charge Term Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.recharge_voltage_uv {
            writeln!(f, "Recharge Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.min_input_voltage_uv {
            writeln!(f, "Min Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.max_input_voltage_uv {
            writeln!(f, "Max Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(modes) = &spec.supported_modes {
            let list: Vec<String> =
                modes.iter().map(|m| DisplayOperatingMode(*m).to_string()).collect();
            writeln!(
                f,
                "Supported Operating Modes: {}",
                if list.is_empty() { "None".to_string() } else { list.join(", ") }
            )?;
        }
        if let Some(opts) = &spec.supported_options {
            let format_trigger_list = |status_mask: &Option<fcharger::Status>| -> String {
                let Some(s) = status_mask else {
                    return "None".to_string();
                };
                let mut triggers = Vec::new();
                if s.online.is_some() {
                    triggers.push("online");
                }
                if s.source_type.is_some() {
                    triggers.push("source_type");
                }
                if s.operating_mode.is_some() {
                    triggers.push("operating_mode");
                }
                if s.charge_phase.is_some() {
                    triggers.push("charge_phase");
                }
                if s.health.is_some() {
                    triggers.push("health");
                }
                if s.input_voltage_uv.is_some() {
                    triggers.push("input_voltage");
                }
                if s.input_current_ua.is_some() {
                    triggers.push("input_current");
                }
                if s.input_current_limit_ua.is_some() {
                    triggers.push("input_current_limit");
                }
                if s.input_voltage_limit_uv.is_some() {
                    triggers.push("input_voltage_limit");
                }
                if s.charge_current_limit_ua.is_some() {
                    triggers.push("charge_current_limit");
                }
                if s.float_voltage_uv.is_some() {
                    triggers.push("float_voltage");
                }
                if s.charge_term_current_ua.is_some() {
                    triggers.push("charge_term_current");
                }
                if triggers.is_empty() { "None".to_string() } else { triggers.join(", ") }
            };

            writeln!(f, "Supported Triggers: {}", format_trigger_list(&opts.interest))?;
            writeln!(f, "Supported Wake Triggers: {}", format_trigger_list(&opts.wake_on))?;
        }
        Ok(())
    }
}

pub struct DisplayChargerStatus<'a>(pub &'a fcharger::Status);

impl fmt::Display for DisplayChargerStatus<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        if let Some(online) = s.online {
            writeln!(f, "Online: {online}")?;
        }
        if let Some(src) = s.source_type {
            writeln!(f, "Source Type: {}", DisplaySourceType(src))?;
        }
        if let Some(mode) = s.operating_mode {
            writeln!(f, "Operating Mode: {}", DisplayOperatingMode(mode))?;
        }
        if let Some(phase) = s.charge_phase {
            writeln!(f, "Charge Phase: {}", DisplayChargePhase(phase))?;
        }
        if let Some(health) = s.health {
            writeln!(f, "Health: {}", DisplayChargerHealth(health))?;
        }
        if let Some(v) = s.input_voltage_uv {
            writeln!(f, "Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.input_current_ua {
            writeln!(f, "Input Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.input_current_limit_ua {
            writeln!(f, "Input Current Limit: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.input_voltage_limit_uv {
            writeln!(f, "Input Voltage Limit: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.charge_current_limit_ua {
            writeln!(f, "Charge Current Limit: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.float_voltage_uv {
            writeln!(f, "Float Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.charge_term_current_ua {
            writeln!(f, "Charge Term Current: {}", MicroUnit(v, "A"))?;
        }
        Ok(())
    }
}

pub(crate) async fn get_modern_charger_info_at(path: &str) -> Result<()> {
    let charger_path = append_member_suffix(path, MEMBER_CHARGER);
    let charger_path_str = charger_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fcharger::ChargerMarker>(charger_path_str)
        .with_context(|| format!("Failed to connect to Charger at {charger_path_str}"))?;

    if let Ok(Ok(spec)) = proxy.get_spec().await {
        print!("{}", DisplayChargerSpec(&spec));
    }

    let status = proxy
        .get_status()
        .await
        .context("Charger GetStatus call failed")?
        .map_err(|e| anyhow!("Charger GetStatus returned domain error: {:?}", e))?;

    print!("{}", DisplayChargerStatus(&status));
    Ok(())
}

pub fn charger_watch_stream(
    proxy: fcharger::ChargerProxy,
) -> futures::stream::BoxStream<'static, Result<fcharger::Status>> {
    use futures::stream;
    Box::pin(stream::unfold(Some(proxy), |proxy_opt| async move {
        let proxy = proxy_opt?;
        match proxy.watch(None).await {
            Ok(Ok((status, _wake_lease))) => Some((Ok(status), Some(proxy))),
            Ok(Err(e)) => Some((Err(anyhow!("Charger Watch domain error: {:?}", e)), None)),
            Err(e) => Some((Err(anyhow!("Charger Watch FIDL error: {:#}", e)), None)),
        }
    }))
}

pub(crate) async fn watch_charger_at(path: &str) -> Result<()> {
    use futures::StreamExt;
    let target_path = append_member_suffix(path, MEMBER_CHARGER);
    let target_path_str = target_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fcharger::ChargerMarker>(target_path_str)
        .with_context(|| format!("Failed to connect to Charger at {target_path_str}"))?;

    println!("Watching charger events on {} (press Ctrl+C to exit)...\n", target_path_str);

    let mut stream = charger_watch_stream(proxy);
    let mut received_any = false;
    let mut last_err = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(status) => {
                received_any = true;
                println!("=== Charger Telemetry Update ===");
                print!("{}", DisplayChargerStatus(&status));
                println!();
            }
            Err(e) => {
                eprintln!("Warning: received charger watch error: {:#}", e);
                last_err = Some(e);
            }
        }
    }
    if !received_any {
        return Err(last_err.unwrap_or_else(|| anyhow!("Charger watch stream closed immediately")));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargerModeArg(pub fcharger::OperatingMode);

impl std::str::FromStr for ChargerModeArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "charging" | "charge" | "usb" => Ok(Self(fcharger::OperatingMode::Charging)),
            "passthrough" => Ok(Self(fcharger::OperatingMode::Passthrough)),
            "discharging" | "discharge" | "battery" | "batt" => {
                Ok(Self(fcharger::OperatingMode::Discharging))
            }
            "otg" => Ok(Self(fcharger::OperatingMode::Otg)),
            _ => Err(format!(
                "Invalid charger mode '{s}'. Use charging/usb, passthrough, discharging/battery, \
                 or otg."
            )),
        }
    }
}

pub async fn set_charger_mode(path: Option<&str>, mode: fcharger::OperatingMode) -> Result<()> {
    let legacy_enable = match mode {
        fcharger::OperatingMode::Charging => Some(true),
        fcharger::OperatingMode::Passthrough => Some(false),
        _ => None,
    };

    let target = match path {
        Some(p) => set_charger_mode_at_path(p, mode, legacy_enable).await?,
        None => set_default_charger_mode(mode, legacy_enable).await?,
    };
    println!(
        "Successfully set charger operating mode to {} via {target}",
        DisplayOperatingMode(mode)
    );
    Ok(())
}

fn build_mode_control_options(mode: fcharger::OperatingMode) -> fcharger::ControlOptions {
    fcharger::ControlOptions { operating_mode: Some(mode), ..Default::default() }
}

async fn set_charger_mode_at_path(
    p: &str,
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<String> {
    if p.contains(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
        let enable =
            legacy_enable.context("legacy ChargerService does not support power source mode")?;
        return set_legacy_charger_enable(p, enable).await;
    }
    // Never fall back to `Controller`: it is single-client and reverts on close, so a mode set
    // through it would be undone as soon as batteryutil exits. Any charger `Service` or
    // `DebugService` path (instance or member, including `/controller`) maps to `DebugService`.
    if p.contains(fcharger::ServiceMarker::SERVICE_NAME)
        || p.contains(fcharger::DebugServiceMarker::SERVICE_NAME)
        || p.ends_with("/debug")
    {
        return set_debug_charger_mode(&debug_service_instance(p)?, mode).await;
    }
    // Other paths: Debug, then legacy enable.
    match set_debug_charger_mode(p, mode).await {
        Ok(target) => Ok(target),
        Err(debug_err) => match legacy_enable {
            Some(enable) => set_legacy_charger_enable(p, enable)
                .await
                .with_context(|| format!("Debug also failed: {debug_err:#}")),
            None => Err(debug_err),
        },
    }
}

async fn set_default_charger_mode(
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<String> {
    let mut last_err = None;
    if let Ok(p) = select_instance(fcharger::DebugServiceMarker::SERVICE_NAME) {
        match set_debug_charger_mode(&p, mode).await {
            Ok(target) => return Ok(target),
            Err(e) => last_err = Some(e),
        }
    }
    match set_sorrel_fallback_mode(mode, legacy_enable).await {
        Ok(Some(target)) => return Ok(target),
        Ok(None) => {}
        Err(e) => last_err = Some(e),
    }
    Err(last_err.unwrap_or_else(|| anyhow!("No charger control service found under /svc")))
}

/// Status label for the Sorrel USB input suspend control.
const SPMI_USB_SUSPEND_TARGET: &str = "SPMI 0x2954 (USB_IN_SUSPEND)";

/// Applies Sorrel fallback overrides where USB input suspend (`SPMI 0x2954`) and battery charge
/// enable (`fuchsia.power.battery.ChargerService`) are two independent controls.
async fn set_sorrel_fallback_mode(
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<Option<String>> {
    // Quiet presence check: `set_spmi_power_source` selects (and reports) the instance itself.
    let spmi_present = !crate::common::discover_instances(
        fidl_fuchsia_hardware_spmi::DebugServiceMarker::SERVICE_NAME,
    )
    .is_empty();
    match mode {
        fcharger::OperatingMode::Discharging => {
            if spmi_present {
                spmi::set_spmi_power_source(PowerSource::Battery).await?;
                return Ok(Some(SPMI_USB_SUSPEND_TARGET.to_string()));
            }
            Ok(None)
        }
        fcharger::OperatingMode::Charging | fcharger::OperatingMode::Passthrough => {
            let mut results = Vec::new();
            if spmi_present {
                results.push(
                    spmi::set_spmi_power_source(PowerSource::Usb)
                        .await
                        .map(|()| SPMI_USB_SUSPEND_TARGET.to_string()),
                );
            }
            if let Some(enable) = legacy_enable
                && let Ok(p) = select_instance(fpowerbattery::ChargerServiceMarker::SERVICE_NAME)
            {
                results.push(set_legacy_charger_enable(&p, enable).await);
            }
            merge_fallback_results(results)
        }
        _ => Ok(None),
    }
}

/// Merges the outcomes of independent fallback controls. Succeeds with the applied targets if any
/// control was applied, or with `None` if no control was attempted. Otherwise returns the last
/// failure. Every failure that is not returned is printed as a warning, so none is lost.
fn merge_fallback_results(results: Vec<Result<String>>) -> Result<Option<String>> {
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(target) => applied.push(target),
            Err(e) => errors.push(e),
        }
    }
    let returned_err = if applied.is_empty() { errors.pop() } else { None };
    for e in &errors {
        eprintln!("Warning: fallback control not applied: {e:#}");
    }
    match returned_err {
        Some(e) => Err(e),
        None if applied.is_empty() => Ok(None),
        None => Ok(Some(applied.join(" + "))),
    }
}

/// Connects to the `Debug` member of a charger `DebugService` instance, also returning a
/// description of the target for status messages.
fn connect_debug(path: &str) -> Result<(fcharger::DebugProxy, String)> {
    let debug_path = append_member_suffix(path, MEMBER_DEBUG);
    let debug_path_str = debug_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fcharger::DebugMarker>(debug_path_str).with_context(
        || format!("Failed to connect to Debug charger protocol at {debug_path_str}"),
    )?;
    Ok((proxy, format!("fuchsia.hardware.power.charger.Debug ({debug_path_str})")))
}

async fn set_debug_charger_mode(path: &str, mode: fcharger::OperatingMode) -> Result<String> {
    let (proxy, target) = connect_debug(path)?;
    set_debug_charger_mode_with_proxy(&proxy, mode).await?;
    Ok(target)
}

async fn set_debug_charger_mode_with_proxy(
    proxy: &fcharger::DebugProxy,
    mode: fcharger::OperatingMode,
) -> Result<()> {
    let options = build_mode_control_options(mode);
    proxy
        .set_control(&options)
        .await
        .context("Debug SetControl call failed")?
        .map_err(|status| anyhow!("Debug SetControl rejected with error: {:?}", status))
}

async fn set_legacy_charger_enable(path: &str, enable: bool) -> Result<String> {
    let legacy_path = append_member_suffix(path, MEMBER_DEVICE);
    let legacy_path_str = legacy_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fpowerbattery::ChargerMarker>(legacy_path_str)
        .with_context(|| {
            format!("Failed to connect to legacy Charger protocol at {legacy_path_str}")
        })?;

    proxy
        .enable(enable)
        .await
        .context("Enable call failed")?
        .map_err(|e| anyhow!("Enable rejected with domain error: {:?}", e))?;
    Ok(format!("fuchsia.power.battery.Charger ({legacy_path_str})"))
}

/// Argument for `batteryutil mode`: an operating mode to apply, or `auto` to clear all overrides
/// applied through the charger `DebugService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeArg {
    /// Applies an operating mode: as a sticky `Debug` override where the charger `DebugService`
    /// is available, otherwise through the legacy or Sorrel controls.
    Set(ChargerModeArg),
    /// Clears every override applied through the charger `DebugService`.
    Auto,
}

impl std::str::FromStr for ModeArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse().map(Self::Set).map_err(|_| {
            format!(
                "Invalid mode '{s}'. Use charging/usb, passthrough, discharging/battery, otg, \
                 or auto."
            )
        })
    }
}

/// Withdraws all sticky overrides applied through the charger `DebugService`, handing control back
/// to the production policy client or the charger's autonomous defaults.
pub(crate) async fn clear_charger_overrides(path: Option<&str>) -> Result<()> {
    let instance = match path {
        Some(p) => debug_service_instance(p)?,
        None => select_instance(fcharger::DebugServiceMarker::SERVICE_NAME)
            .context("'mode auto' requires fuchsia.hardware.power.charger.DebugService")?,
    };
    let target = clear_debug_overrides(&instance).await?;
    println!("Successfully cleared charger overrides via {target}");
    Ok(())
}

/// Maps a charger service instance path to the matching `DebugService` instance, since overrides
/// can only be cleared through `Debug`.
fn debug_service_instance(path: &str) -> Result<String> {
    if path.contains(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
        return Err(anyhow!(
            "legacy ChargerService has no overrides to clear; use 'mode charging' instead"
        ));
    }
    Ok(path
        .replace(fcharger::ServiceMarker::SERVICE_NAME, fcharger::DebugServiceMarker::SERVICE_NAME))
}

async fn clear_debug_overrides(path: &str) -> Result<String> {
    let (proxy, target) = connect_debug(path)?;
    clear_debug_overrides_with_proxy(&proxy).await?;
    Ok(target)
}

async fn clear_debug_overrides_with_proxy(proxy: &fcharger::DebugProxy) -> Result<()> {
    proxy
        .clear_overrides()
        .await
        .context("Debug ClearOverrides call failed")?
        .map_err(|status| anyhow!("Debug ClearOverrides rejected with error: {:?}", status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn test_parse_and_build_charger_mode_options() {
        for s in ["charging", "charge", "usb"] {
            let parsed = s.parse::<ChargerModeArg>().unwrap();
            assert_eq!(parsed, ChargerModeArg(fcharger::OperatingMode::Charging));
            assert_eq!(
                build_mode_control_options(parsed.0).operating_mode,
                Some(fcharger::OperatingMode::Charging)
            );
        }
        let parsed = "passthrough".parse::<ChargerModeArg>().unwrap();
        assert_eq!(parsed, ChargerModeArg(fcharger::OperatingMode::Passthrough));
        assert_eq!(
            build_mode_control_options(parsed.0).operating_mode,
            Some(fcharger::OperatingMode::Passthrough)
        );
        for s in ["battery", "batt", "discharging", "discharge"] {
            let parsed = s.parse::<ChargerModeArg>().unwrap();
            assert_eq!(parsed, ChargerModeArg(fcharger::OperatingMode::Discharging));
            assert_eq!(
                build_mode_control_options(parsed.0).operating_mode,
                Some(fcharger::OperatingMode::Discharging)
            );
        }
        assert_eq!(
            "otg".parse::<ChargerModeArg>(),
            Ok(ChargerModeArg(fcharger::OperatingMode::Otg))
        );
        for invalid in ["1", "0", "true", "false", "on", "off", "invalid"] {
            assert!(invalid.parse::<ChargerModeArg>().is_err());
        }
    }

    #[test]
    fn test_display_charger_spec_and_status() {
        let spec = fcharger::Spec {
            model: Some("MAX77779".to_string()),
            max_charge_current_ua: Some(4_500_000),
            max_charge_voltage_uv: Some(4_400_000),
            max_input_current_ua: Some(3_200_000),
            supported_modes: Some(vec![
                fcharger::OperatingMode::Charging,
                fcharger::OperatingMode::Passthrough,
                fcharger::OperatingMode::Discharging,
                fcharger::OperatingMode::Otg,
            ]),
            supported_options: Some(fcharger::WatchOptions {
                interest: Some(fcharger::Status {
                    online: Some(false),
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    input_current_limit_ua: Some(0),
                    ..Default::default()
                }),
                wake_on: Some(fcharger::Status { online: Some(false), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec_out = DisplayChargerSpec(&spec).to_string();
        assert!(spec_out.contains("Charger Model: MAX77779"));
        assert!(spec_out.contains("Max Charge Current: 4.500 A"));
        assert!(spec_out.contains("Max Charge Voltage: 4.400 V"));
        assert!(spec_out.contains("Max Input Current: 3.200 A"));
        assert!(
            spec_out.contains("Supported Operating Modes: Charging, Passthrough, Discharging, OTG")
        );
        assert!(
            spec_out.contains("Supported Triggers: online, operating_mode, input_current_limit")
        );
        assert!(spec_out.contains("Supported Wake Triggers: online"));

        let status = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            health: Some(fcharger::Health::Good),
            input_current_limit_ua: Some(1_500_000),
            input_current_ua: Some(1_200_000),
            ..Default::default()
        };
        let status_out = DisplayChargerStatus(&status).to_string();
        assert!(status_out.contains("Online: true"));
        assert!(status_out.contains("Source Type: USB"));
        assert!(status_out.contains("Operating Mode: Charging"));
        assert!(status_out.contains("Charge Phase: Fast"));
        assert!(status_out.contains("Health: Good"));
        assert!(status_out.contains("Input Current Limit: 1.500 A"));
        assert!(status_out.contains("Input Current: 1.200 A"));
    }

    #[fuchsia::test]
    async fn test_charger_watch_stream_stops_on_domain_error() {
        let (charger_proxy, mut charger_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        let server_task = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::ChargerRequest::Watch { responder, .. })) =
                charger_stream.next().await
            else {
                panic!("expected a first Watch request");
            };
            responder
                .send(Ok((
                    &fcharger::Status {
                        online: Some(true),
                        operating_mode: Some(fcharger::OperatingMode::Passthrough),
                        ..Default::default()
                    },
                    None,
                )))
                .expect("send Watch status");
            let Some(Ok(fcharger::ChargerRequest::Watch { responder, .. })) =
                charger_stream.next().await
            else {
                panic!("expected a second Watch request");
            };
            responder.send(Err(fcharger::Error::Io)).expect("send Watch error");
        });

        let mut watch_stream = charger_watch_stream(charger_proxy);
        let first = watch_stream.next().await.expect("first item").expect("ok status");
        assert_eq!(first.online, Some(true));
        assert_eq!(first.operating_mode, Some(fcharger::OperatingMode::Passthrough));

        let second = watch_stream.next().await.expect("error item");
        assert!(second.is_err());
        assert!(watch_stream.next().await.is_none());
        server_task.await;
    }

    #[fuchsia::test]
    async fn test_set_debug_charger_mode_sends_operating_mode() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::SetControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected a SetControl request");
            };
            assert_eq!(options.operating_mode, Some(fcharger::OperatingMode::Discharging));
            responder.send(Ok(())).expect("send SetControl response");
        });
        set_debug_charger_mode_with_proxy(&debug_proxy, fcharger::OperatingMode::Discharging)
            .await
            .expect("set_debug_charger_mode_with_proxy succeeded");
        debug_server.await;
    }

    #[test]
    fn test_parse_mode_arg() {
        for s in ["auto", "AUTO", "Auto"] {
            assert_eq!(s.parse::<ModeArg>(), Ok(ModeArg::Auto));
        }
        assert_eq!(
            "usb".parse::<ModeArg>(),
            Ok(ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Charging)))
        );
        assert_eq!(
            "otg".parse::<ModeArg>(),
            Ok(ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Otg)))
        );
        let err = "automatic".parse::<ModeArg>().unwrap_err();
        assert!(err.contains("or auto"), "unexpected error: {err}");
    }

    #[test]
    fn test_debug_service_instance_mapping() {
        let service = format!("/svc/{}/default", fcharger::ServiceMarker::SERVICE_NAME);
        let debug = format!("/svc/{}/default", fcharger::DebugServiceMarker::SERVICE_NAME);
        assert_eq!(debug_service_instance(&service).unwrap(), debug);
        assert_eq!(debug_service_instance(&debug).unwrap(), debug);

        // A `Controller` member path resolves to the `Debug` member, never to `Controller`.
        let controller = debug_service_instance(&format!("{service}/controller")).unwrap();
        assert_eq!(
            append_member_suffix(controller, MEMBER_DEBUG),
            std::path::PathBuf::from(format!("{debug}/debug"))
        );

        let legacy = format!("/svc/{}/default", fpowerbattery::ChargerServiceMarker::SERVICE_NAME);
        assert!(debug_service_instance(&legacy).is_err());
    }

    #[fuchsia::test]
    async fn test_clear_debug_overrides_sends_clear_overrides() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::ClearOverrides { responder })) =
                debug_stream.next().await
            else {
                panic!("expected a ClearOverrides request");
            };
            responder.send(Ok(())).expect("send ClearOverrides response");
        });
        clear_debug_overrides_with_proxy(&debug_proxy)
            .await
            .expect("clear_debug_overrides_with_proxy succeeded");
        debug_server.await;
    }

    #[fuchsia::test]
    async fn test_clear_debug_overrides_propagates_driver_error() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::ClearOverrides { responder })) =
                debug_stream.next().await
            else {
                panic!("expected a ClearOverrides request");
            };
            responder.send(Err(fcharger::Error::Io)).expect("send ClearOverrides error");
        });
        let err = clear_debug_overrides_with_proxy(&debug_proxy).await.unwrap_err();
        assert!(format!("{err:#}").contains("Io"), "unexpected error: {err:#}");
        debug_server.await;
    }

    #[test]
    fn test_merge_fallback_results() {
        assert!(merge_fallback_results(vec![]).unwrap().is_none());
        assert_eq!(
            merge_fallback_results(vec![Ok("spmi".to_string()), Ok("legacy".to_string())])
                .unwrap()
                .as_deref(),
            Some("spmi + legacy")
        );
        // A partial success still succeeds; the failure is only reported as a warning.
        assert_eq!(
            merge_fallback_results(vec![Ok("spmi".to_string()), Err(anyhow!("legacy failed"))])
                .unwrap()
                .as_deref(),
            Some("spmi")
        );
        // With nothing applied, the last failure is returned and earlier ones become warnings.
        let err =
            merge_fallback_results(vec![Err(anyhow!("spmi failed")), Err(anyhow!("no enable"))])
                .unwrap_err();
        assert_eq!(err.to_string(), "no enable");
    }
}
