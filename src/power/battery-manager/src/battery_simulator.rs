// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::battery_manager::BatterySimulationStateObserver;
use anyhow::{Error, format_err};
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use futures::lock::Mutex;
use std::sync::{Arc, Weak};

fn get_current_time() -> Option<i64> {
    Some(zx::BootInstant::get().into_nanos())
}

// SimulatedBatteryInfoSource stores per-field BatteryInfo overrides modified
// through BatterySimulator FIDL calls. Only fields explicitly set via simulator
// calls are populated (`Some(...)`); unset fields remain `None` so
// BatteryManager keeps the existing live hardware values for those fields.
pub(crate) struct SimulatedBatteryInfoSource {
    battery_info: Arc<Mutex<fpower::BatteryInfo>>,
    observer: Weak<dyn BatterySimulationStateObserver>,
}

impl SimulatedBatteryInfoSource {
    pub fn new(
        battery_info: fpower::BatteryInfo,
        observer: Weak<dyn BatterySimulationStateObserver>,
    ) -> Self {
        Self { battery_info: Arc::new(Mutex::new(battery_info)), observer }
    }

    pub async fn get_battery_info_copy(&self) -> fpower::BatteryInfo {
        if let Some(observer) = self.observer.upgrade() {
            observer.get_battery_info_copy()
        } else {
            let battery_info = self.battery_info.lock().await;
            (*battery_info).clone()
        }
    }

    async fn update_override(&self, f: impl FnOnce(&mut fpower::BatteryInfo)) -> Result<(), Error> {
        {
            let mut battery_info = self.battery_info.lock().await;
            f(&mut battery_info);
            battery_info.timestamp = get_current_time();
        }
        self.notify_battery_info_changed().await
    }

    async fn set_battery_percentage(&self, level_percent: f32) -> Result<(), Error> {
        self.update_override(|info| info.level_percent = Some(level_percent)).await
    }

    async fn set_present_voltage(&self, present_voltage: u32) -> Result<(), Error> {
        self.update_override(|info| info.present_voltage_mv = Some(present_voltage)).await
    }

    async fn set_remaining_charge(&self, remaining_charge: u32) -> Result<(), Error> {
        self.update_override(|info| info.remaining_charge_uah = Some(remaining_charge)).await
    }

    async fn set_charge_source(&self, charge_source: fpower::ChargeSource) -> Result<(), Error> {
        self.update_override(|info| info.charge_source = Some(charge_source)).await
    }

    async fn set_level_status(&self, level_status: fpower::LevelStatus) -> Result<(), Error> {
        self.update_override(|info| info.level_status = Some(level_status)).await
    }

    async fn set_charge_status(&self, charge_status: fpower::ChargeStatus) -> Result<(), Error> {
        self.update_override(|info| info.charge_status = Some(charge_status)).await
    }

    async fn set_battery_status(&self, battery_status: fpower::BatteryStatus) -> Result<(), Error> {
        self.update_override(|info| info.status = Some(battery_status)).await
    }

    async fn set_time_remaining(&self, time: i64) -> Result<(), Error> {
        self.update_override(|battery_info| {
            let charge_status = battery_info.charge_status.or_else(|| {
                self.observer.upgrade().and_then(|o| o.get_battery_info_copy().charge_status)
            });
            battery_info.time_remaining =
                Some(if charge_status == Some(fpower::ChargeStatus::Charging) {
                    fpower::TimeRemaining::FullCharge(time)
                } else if matches!(
                    charge_status,
                    Some(fpower::ChargeStatus::Discharging | fpower::ChargeStatus::NotCharging)
                ) {
                    fpower::TimeRemaining::BatteryLife(time)
                } else {
                    fpower::TimeRemaining::Indeterminate(time)
                });
        })
        .await
    }

    async fn set_battery_spec(&self, spec: fpower::BatterySpec) -> Result<(), Error> {
        self.update_override(|info| info.battery_spec = Some(spec)).await
    }

    // Updates the simulated_battery_info in BatteryManager
    async fn notify_battery_info_changed(&self) -> Result<(), Error> {
        let observer = self.observer.upgrade().ok_or_else(|| format_err!("Observer not found"))?;
        observer.update_simulated_battery_info((*self.battery_info.lock().await).clone());
        Ok(())
    }

    // Updates BatteryManager as well as store the simulated override BatteryInfo
    pub async fn update_simulation(
        &self,
        is_simulating: bool,
        battery_info: fpower::BatteryInfo,
    ) -> Result<(), Error> {
        let mut overrides = self.battery_info.lock().await;
        *overrides = battery_info;
        drop(overrides);

        let observer = self.observer.upgrade().ok_or_else(|| format_err!("Observer not found"))?;
        observer.update_simulation(is_simulating);
        self.notify_battery_info_changed().await?;
        Ok(())
    }

    pub(crate) async fn handle_request(
        &self,
        request: spower::BatterySimulatorRequest,
    ) -> Result<(), Error> {
        async move {
            match request {
                spower::BatterySimulatorRequest::GetBatteryInfo { responder, .. } => {
                    let info = self.get_battery_info_copy();
                    responder.send(&info.await).map_err(|e| format_err!("{}", e))?;
                }
                spower::BatterySimulatorRequest::SetChargeStatus { charge_status, .. } => {
                    self.set_charge_status(charge_status).await?;
                }
                spower::BatterySimulatorRequest::SetBatteryStatus { battery_status, .. } => {
                    self.set_battery_status(battery_status).await?;
                }
                spower::BatterySimulatorRequest::SetChargeSource { charge_source, .. } => {
                    self.set_charge_source(charge_source).await?;
                }
                spower::BatterySimulatorRequest::SetBatteryPercentage { percent, .. } => {
                    self.set_battery_percentage(percent).await?;
                }
                spower::BatterySimulatorRequest::SetPresentVoltageMv { voltage, .. } => {
                    self.set_present_voltage(voltage).await?;
                }
                spower::BatterySimulatorRequest::SetRemainingChargeUah { charge, .. } => {
                    self.set_remaining_charge(charge).await?;
                }
                spower::BatterySimulatorRequest::SetLevelStatus { level_status, .. } => {
                    self.set_level_status(level_status).await?;
                }
                spower::BatterySimulatorRequest::SetTimeRemaining { duration, .. } => {
                    self.set_time_remaining(duration).await?;
                }
                spower::BatterySimulatorRequest::DisconnectRealBattery { .. } => {
                    Err(format_err!("Unexpected Disconnect request called"))?
                }
                spower::BatterySimulatorRequest::ReconnectRealBattery { .. } => {
                    Err(format_err!("Unexpected  Reconnect request called"))?
                }
                spower::BatterySimulatorRequest::IsSimulating { responder: _, .. } => {
                    Err(format_err!("Unexpected Simulating requested called"))?
                }
                spower::BatterySimulatorRequest::SetBatterySpec { spec, .. } => {
                    self.set_battery_spec(spec).await?;
                }
            }
            Ok::<(), Error>(())
        }
        .await?;

        Ok(())
    }
}

// TODO(rminocha): investigate ways to make unit tests linear
#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery_info_recorders::RecorderConfig;
    use crate::battery_manager::BatteryManager;
    use fidl::endpoints::create_request_stream;
    use futures::TryStreamExt;
    use futures::future::*;

    pub fn create_manager() -> Arc<BatteryManager> {
        Arc::new(BatteryManager::new(RecorderConfig::default()))
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_observer_relationship() {
        let (watcher_client_end, mut stream) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher = watcher_client_end.into_proxy();

        let battery_manager = create_manager();
        battery_manager.add_watcher(watcher);

        let mut battery_info: fpower::BatteryInfo = battery_manager.get_battery_info_copy();
        battery_info.level_percent = Some(50.0);

        let simulated_battery_info_source = SimulatedBatteryInfoSource::new(
            battery_manager.get_battery_info_copy(),
            Arc::downgrade(&battery_manager) as Weak<dyn BatterySimulationStateObserver>,
        );

        let serve_fut = async move {
            let request = stream.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: _,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received");
            };

            let request = stream.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: _,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 60);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received");
            };
        };

        let request_fut = async move {
            // Update_simulation requires a battery_info since that holds the updated simulated
            // battery info object.
            let res =
                simulated_battery_info_source.update_simulation(true, battery_info.clone()).await;
            assert!(!res.is_err(), "Update simulation failed");
            battery_info.level_percent = Some(60.0);
            let res =
                simulated_battery_info_source.update_simulation(true, battery_info.clone()).await;
            assert!(!res.is_err(), "Update simulation failed");
        };

        join(serve_fut, request_fut).await;
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_set_battery_percentage_preserves_and_propagates_real_fields() {
        let battery_manager = create_manager();
        let initial_real_info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::NotAvailable),
            charge_status: Some(fpower::ChargeStatus::Charging),
            charge_source: Some(fpower::ChargeSource::Usb),
            level_percent: Some(25.0),
            level_status: Some(fpower::LevelStatus::Low),
            health: Some(fpower::HealthStatus::Unknown),
            present_voltage_mv: Some(4100),
            temperature_mc: Some(25000),
            ..Default::default()
        };
        battery_manager.update_cached_battery_info(initial_real_info, None);

        let simulated = SimulatedBatteryInfoSource::new(
            fpower::BatteryInfo::default(),
            Arc::downgrade(&battery_manager) as Weak<dyn BatterySimulationStateObserver>,
        );

        simulated.update_simulation(true, fpower::BatteryInfo::default()).await.unwrap();
        simulated.set_battery_percentage(77.0).await.unwrap();

        let updated = simulated.get_battery_info_copy().await;
        assert_eq!(updated.level_percent, Some(77.0));
        assert_eq!(updated.level_status, Some(fpower::LevelStatus::Warning));
        assert_eq!(updated.status, Some(fpower::BatteryStatus::Ok));
        assert_eq!(updated.health, Some(fpower::HealthStatus::Good));
        assert_eq!(updated.charge_status, Some(fpower::ChargeStatus::Charging));
        assert_eq!(updated.charge_source, Some(fpower::ChargeSource::Usb));
        assert_eq!(updated.present_voltage_mv, Some(4100));
        assert_eq!(updated.temperature_mc, Some(25000));

        // Setting time_remaining without overriding charge_status uses the live charge_status
        // (`Charging` -> `FullCharge`).
        simulated.set_time_remaining(1800).await.unwrap();
        assert_eq!(
            battery_manager.get_battery_info_copy().time_remaining,
            Some(fpower::TimeRemaining::FullCharge(1800))
        );

        // An explicit SetLevelStatus override is preserved across a subsequent SetBatteryPercentage.
        simulated.set_level_status(fpower::LevelStatus::Critical).await.unwrap();
        simulated.set_battery_percentage(90.0).await.unwrap();
        let with_explicit_level_status = battery_manager.get_battery_info_copy();
        assert_eq!(with_explicit_level_status.level_percent, Some(90.0));
        assert_eq!(with_explicit_level_status.level_status, Some(fpower::LevelStatus::Critical));

        // Reset overrides back to percentage-only (77%) before testing live hardware unplug.
        simulated.update_simulation(true, fpower::BatteryInfo::default()).await.unwrap();
        simulated.set_battery_percentage(77.0).await.unwrap();

        // When a real hardware update arrives while simulating (e.g. USB unplugged),
        // non-overridden fields update while the overridden level_percent remains 77%.
        let unplugged_real_info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Ok),
            charge_status: Some(fpower::ChargeStatus::Discharging),
            charge_source: Some(fpower::ChargeSource::None),
            level_percent: Some(24.0),
            level_status: Some(fpower::LevelStatus::Low),
            health: Some(fpower::HealthStatus::Good),
            present_voltage_mv: Some(3800),
            temperature_mc: Some(26000),
            ..Default::default()
        };
        battery_manager.update_cached_battery_info(unplugged_real_info, None);

        let after_unplug = battery_manager.get_battery_info_copy();
        assert_eq!(after_unplug.level_percent, Some(77.0));
        assert_eq!(after_unplug.charge_status, Some(fpower::ChargeStatus::Discharging));
        assert_eq!(after_unplug.charge_source, Some(fpower::ChargeSource::None));
        assert_eq!(after_unplug.present_voltage_mv, Some(3800));
        assert_eq!(after_unplug.temperature_mc, Some(26000));

        // Setting time_remaining after unplug uses the updated live charge_status
        // (`Discharging` -> `BatteryLife`).
        simulated.set_time_remaining(3600).await.unwrap();
        assert_eq!(
            battery_manager.get_battery_info_copy().time_remaining,
            Some(fpower::TimeRemaining::BatteryLife(3600))
        );

        // Reconnecting the real battery clears the override and restores the real level_percent.
        simulated.update_simulation(false, fpower::BatteryInfo::default()).await.unwrap();
        let reconnected = battery_manager.get_battery_info_copy();
        assert_eq!(reconnected.level_percent, Some(24.0));
        assert_eq!(reconnected.charge_status, Some(fpower::ChargeStatus::Discharging));
        assert_eq!(reconnected.charge_source, Some(fpower::ChargeSource::None));
    }
}
