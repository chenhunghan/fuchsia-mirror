// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Charger Client and Controller (supporting both
//! fuchsia.hardware.power.charger and fuchsia.power.battery.Charger).

use crate::common::{MEMBER_CONTROLLER, MEMBER_DEVICE, append_member_suffix, select_instance};
use anyhow::{Context, Result, anyhow};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpowerbattery;
use fuchsia_component::client::connect_to_protocol_at_path;

pub async fn enable_charger(path: Option<&str>, enable: bool) -> Result<()> {
    let target = if let Some(p) = path {
        if p.contains(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
            set_legacy_charger_enable(p, enable).await?
        } else if p.contains(fcharger::ServiceMarker::SERVICE_NAME) {
            set_modern_charger_enable(p, enable).await?
        } else {
            match set_modern_charger_enable(p, enable).await {
                Ok(target) => target,
                Err(modern_err) => set_legacy_charger_enable(p, enable)
                    .await
                    .with_context(|| format!("modern Controller also failed: {modern_err:#}"))?,
            }
        }
    } else if let Ok(resolved) = select_instance(fcharger::ServiceMarker::SERVICE_NAME) {
        match set_modern_charger_enable(&resolved, enable).await {
            Ok(target) => target,
            Err(modern_err) => {
                match select_instance(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
                    Ok(legacy_resolved) => {
                        set_legacy_charger_enable(&legacy_resolved, enable).await.with_context(
                            || format!("modern Controller also failed: {modern_err:#}"),
                        )?
                    }
                    Err(_) => return Err(modern_err),
                }
            }
        }
    } else {
        let legacy_resolved = select_instance(fpowerbattery::ChargerServiceMarker::SERVICE_NAME)?;
        set_legacy_charger_enable(&legacy_resolved, enable).await?
    };

    println!("Successfully {} charging via {target}", if enable { "enabled" } else { "disabled" });
    Ok(())
}

fn build_modern_control_options(enable: bool) -> fcharger::ControlOptions {
    fcharger::ControlOptions {
        operating_mode: Some(if enable {
            fcharger::OperatingMode::Charging
        } else {
            fcharger::OperatingMode::Passthrough
        }),
        ..Default::default()
    }
}

async fn set_modern_charger_enable(path: &str, enable: bool) -> Result<String> {
    let charger_path = append_member_suffix(path, MEMBER_CONTROLLER);
    let charger_path_str = charger_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fcharger::ControllerMarker>(charger_path_str)
        .with_context(|| format!("Failed to connect to Controller at {charger_path_str}"))?;

    let options = build_modern_control_options(enable);

    proxy
        .set_control(&options)
        .await
        .context("SetControl call failed")?
        .map_err(|status| anyhow!("SetControl rejected with error: {:?}", status))?;

    Ok(format!("fuchsia.hardware.power.charger.Controller ({charger_path_str})"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_modern_control_options_enable_and_disable() {
        let enabled = build_modern_control_options(true);
        assert_eq!(enabled.operating_mode, Some(fcharger::OperatingMode::Charging));

        let disabled = build_modern_control_options(false);
        assert_eq!(disabled.operating_mode, Some(fcharger::OperatingMode::Passthrough));
    }
}
