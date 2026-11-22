// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake `fuchsia.hardware.power.battery.Service` implementation for Starnix integration tests.

use anyhow::Error;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt as _, TryStreamExt as _};

const DESIGN_CAPACITY_UAH: u32 = 4_200_000;
const DESIGN_VOLTAGE_UV: u32 = 4_400_000;
const DEFAULT_LEVEL_PERCENT: f32 = 50.0;
const DEFAULT_VOLTAGE_UV: u32 = 4_100_000;
const DEFAULT_CURRENT_UA: i32 = 250_000;
const DEFAULT_REMAINING_CAPACITY_UAH: u32 = 2_000_000;
const DEFAULT_FULL_CHARGE_CAPACITY_UAH: u32 = 4_000_000;
const DEFAULT_TEMP_CELSIUS: f32 = 25.0;

fn default_spec() -> fbattery::Spec {
    fbattery::Spec {
        design_capacity_uah: Some(DESIGN_CAPACITY_UAH),
        design_voltage_uv: Some(DESIGN_VOLTAGE_UV),
        ..Default::default()
    }
}

fn default_status() -> fbattery::Status {
    fbattery::Status {
        present: Some(true),
        level_percent: Some(DEFAULT_LEVEL_PERCENT),
        voltage_uv: Some(DEFAULT_VOLTAGE_UV),
        current_ua: Some(DEFAULT_CURRENT_UA),
        remaining_capacity_uah: Some(DEFAULT_REMAINING_CAPACITY_UAH),
        full_charge_capacity_uah: Some(DEFAULT_FULL_CHARGE_CAPACITY_UAH),
        temp_celsius: Some(DEFAULT_TEMP_CELSIUS),
        health: Some(fbattery::HealthStatus::Good),
        ..Default::default()
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service_instance("default", |request| request);
    service_fs.take_and_serve_directory_handle()?;

    service_fs
        .for_each_concurrent(None, |request: fbattery::ServiceRequest| async move {
            #[allow(unreachable_patterns)]
            match request {
                fbattery::ServiceRequest::Battery(mut stream) => {
                    let mut first_watch = true;
                    let mut pending_watches = Vec::new();
                    while let Ok(Some(request)) = stream.try_next().await {
                        match request {
                            fbattery::BatteryRequest::GetSpec { responder } => {
                                let _ = responder.send(Ok(&default_spec()));
                            }
                            fbattery::BatteryRequest::GetStatus { responder } => {
                                let _ = responder.send(Ok(&default_status()));
                            }
                            fbattery::BatteryRequest::ConfigureWatch { options, responder } => {
                                let _ = responder.send(Ok(&options));
                            }
                            fbattery::BatteryRequest::Watch { responder, .. } => {
                                if first_watch {
                                    first_watch = false;
                                    let _ = responder.send(Ok((&default_status(), None)));
                                } else {
                                    pending_watches.push(responder);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        })
        .await;

    Ok(())
}
