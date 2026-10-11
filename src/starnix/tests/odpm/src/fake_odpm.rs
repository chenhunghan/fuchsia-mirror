// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Mock ODPM FIDL service implementation for Starnix integration tests.

use anyhow::Error;
use fidl_fuchsia_hardware_google_odpm as fodpm;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt as _, TryStreamExt as _};

const NANOS_PER_MILLI: i64 = 1_000_000;
const SAMPLE_TIME_MS: i64 = 1_279_315;
const RAIL0_ENERGY_UJ: i64 = 51_899_976;
const RAIL1_ENERGY_UJ: i64 = 185_487_825;

/// Converts milliseconds to nanoseconds.
fn millis_to_nanos(millis: i64) -> i64 {
    millis * NANOS_PER_MILLI
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut service_fs = ServiceFs::new_local();
    service_fs
        .dir("svc")
        .add_fidl_service_instance("rail0", |request: fodpm::ServiceRequest| {
            let fodpm::ServiceRequest::Device(stream) = request;
            (0u32, stream)
        })
        .add_fidl_service_instance("rail1", |request: fodpm::ServiceRequest| {
            let fodpm::ServiceRequest::Device(stream) = request;
            (1u32, stream)
        });
    service_fs.take_and_serve_directory_handle()?;

    service_fs
        .for_each_concurrent(
            None,
            |(channel_id, stream): (u32, fodpm::DeviceRequestStream)| async move {
                let _ = stream
                    .try_for_each(|request: fodpm::DeviceRequest| async move {
                        match request {
                            fodpm::DeviceRequest::GetRailMetadata { responder } => {
                                let metadata = if channel_id == 0 {
                                    fodpm::RailMetadata {
                                        name: Some("amb".to_string()),
                                        channel_id: Some(0),
                                        schematic_name: Some("S1M_VDD_AMB".to_string()),
                                        ..Default::default()
                                    }
                                } else {
                                    fodpm::RailMetadata {
                                        name: Some("cpu2".to_string()),
                                        channel_id: Some(1),
                                        schematic_name: Some("S2M_VDD_CPU2".to_string()),
                                        ..Default::default()
                                    }
                                };
                                responder.send(Ok(&metadata))?;
                            }
                            fodpm::DeviceRequest::GetEnergyJoules { payload, responder } => {
                                assert_eq!(
                                    payload.measurement_type,
                                    Some(fodpm::MeasurementType::Cumulative)
                                );
                                let energy_uj =
                                    if channel_id == 0 { RAIL0_ENERGY_UJ } else { RAIL1_ENERGY_UJ };
                                let reading = fodpm::EnergyReading {
                                    energy_uj: Some(energy_uj),
                                    interval: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    timestamp: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    ..Default::default()
                                };
                                responder.send(Ok(&reading))?;
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                    .await;
            },
        )
        .await;

    Ok(())
}
