// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::battery_manager::BatteryManager;
use anyhow::Error;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use futures::TryStreamExt;

impl BatteryManager {
    fn set_battery_percentage(&self, level_percent: f32) {
        self.update_override(|info| info.level_percent = Some(level_percent));
    }

    fn set_present_voltage(&self, present_voltage: u32) {
        self.update_override(|info| info.present_voltage_mv = Some(present_voltage));
    }

    fn set_remaining_charge(&self, remaining_charge: u32) {
        self.update_override(|info| info.remaining_charge_uah = Some(remaining_charge));
    }

    fn set_charge_source(&self, charge_source: fpower::ChargeSource) {
        self.update_override(|info| info.charge_source = Some(charge_source));
    }

    fn set_level_status(&self, level_status: fpower::LevelStatus) {
        self.update_override(|info| info.level_status = Some(level_status));
    }

    fn set_charge_status(&self, charge_status: fpower::ChargeStatus) {
        self.update_override(|info| info.charge_status = Some(charge_status));
    }

    fn set_battery_status(&self, battery_status: fpower::BatteryStatus) {
        self.update_override(|info| info.status = Some(battery_status));
    }

    fn set_time_remaining(&self, time: i64) {
        let charge_status = self.get_battery_info_copy().charge_status;
        self.update_override(|battery_info| {
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
        });
    }

    fn set_battery_spec(&self, spec: fpower::BatterySpec) {
        self.update_override(|info| info.battery_spec = Some(spec));
    }

    pub(crate) async fn serve_simulator(
        &self,
        stream: spower::BatterySimulatorRequestStream,
    ) -> Result<(), Error> {
        stream
            .err_into()
            .try_for_each(|request| async move {
                match request {
                    spower::BatterySimulatorRequest::DisconnectRealBattery { .. } => {
                        // Start simulation with an all-`None` override table so only fields
                        // explicitly set via subsequent simulator calls override the live
                        // `BatteryInfo`.
                        self.update_simulation(Some(fpower::BatteryInfo::default()));
                    }
                    spower::BatterySimulatorRequest::ReconnectRealBattery { .. } => {
                        self.update_simulation(None);
                    }
                    spower::BatterySimulatorRequest::IsSimulating { responder, .. } => {
                        responder.send(self.is_simulating())?;
                    }
                    spower::BatterySimulatorRequest::GetBatteryInfo { responder, .. } => {
                        responder.send(&self.get_battery_info_copy())?;
                    }
                    spower::BatterySimulatorRequest::SetChargeStatus { charge_status, .. } => {
                        self.set_charge_status(charge_status);
                    }
                    spower::BatterySimulatorRequest::SetBatteryStatus {
                        battery_status, ..
                    } => {
                        self.set_battery_status(battery_status);
                    }
                    spower::BatterySimulatorRequest::SetChargeSource { charge_source, .. } => {
                        self.set_charge_source(charge_source);
                    }
                    spower::BatterySimulatorRequest::SetBatteryPercentage { percent, .. } => {
                        self.set_battery_percentage(percent);
                    }
                    spower::BatterySimulatorRequest::SetPresentVoltageMv { voltage, .. } => {
                        self.set_present_voltage(voltage);
                    }
                    spower::BatterySimulatorRequest::SetRemainingChargeUah { charge, .. } => {
                        self.set_remaining_charge(charge);
                    }
                    spower::BatterySimulatorRequest::SetLevelStatus { level_status, .. } => {
                        self.set_level_status(level_status);
                    }
                    spower::BatterySimulatorRequest::SetTimeRemaining { duration, .. } => {
                        self.set_time_remaining(duration);
                    }
                    spower::BatterySimulatorRequest::SetBatterySpec { spec, .. } => {
                        self.set_battery_spec(spec);
                    }
                }
                Ok::<(), Error>(())
            })
            .await
    }
}

// TODO(rminocha): investigate ways to make unit tests linear
#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery_info_recorders::RecorderConfig;
    use fidl::endpoints::create_request_stream;
    use futures::future::*;
    use std::sync::Arc;

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

        let request_fut = async {
            battery_manager.update_simulation(Some(battery_info.clone()));
            battery_info.level_percent = Some(60.0);
            battery_manager.update_simulation(Some(battery_info));
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

        battery_manager.update_simulation(Some(fpower::BatteryInfo::default()));
        battery_manager.set_battery_percentage(77.0);

        let updated = battery_manager.get_battery_info_copy();
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
        battery_manager.set_time_remaining(1800);
        assert_eq!(
            battery_manager.get_battery_info_copy().time_remaining,
            Some(fpower::TimeRemaining::FullCharge(1800))
        );

        // An explicit SetLevelStatus override is preserved across a subsequent SetBatteryPercentage.
        battery_manager.set_level_status(fpower::LevelStatus::Critical);
        battery_manager.set_battery_percentage(90.0);
        let with_explicit_level_status = battery_manager.get_battery_info_copy();
        assert_eq!(with_explicit_level_status.level_percent, Some(90.0));
        assert_eq!(with_explicit_level_status.level_status, Some(fpower::LevelStatus::Critical));

        // Reset overrides back to percentage-only (77%) before testing live hardware unplug.
        battery_manager.update_simulation(Some(fpower::BatteryInfo::default()));
        battery_manager.set_battery_percentage(77.0);

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
        battery_manager.set_time_remaining(3600);
        assert_eq!(
            battery_manager.get_battery_info_copy().time_remaining,
            Some(fpower::TimeRemaining::BatteryLife(3600))
        );

        // Reconnecting the real battery clears the override and restores the real level_percent.
        battery_manager.update_simulation(None);
        let reconnected = battery_manager.get_battery_info_copy();
        assert_eq!(reconnected.level_percent, Some(24.0));
        assert_eq!(reconnected.charge_status, Some(fpower::ChargeStatus::Discharging));
        assert_eq!(reconnected.charge_source, Some(fpower::ChargeSource::None));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_serve_simulator_fidl_requests() {
        let battery_manager = create_manager();
        let initial_real_info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Ok),
            charge_status: Some(fpower::ChargeStatus::Charging),
            charge_source: Some(fpower::ChargeSource::Usb),
            level_percent: Some(30.0),
            level_status: Some(fpower::LevelStatus::Low),
            health: Some(fpower::HealthStatus::Good),
            ..Default::default()
        };
        battery_manager.update_cached_battery_info(initial_real_info, None);

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<spower::BatterySimulatorMarker>();
        let bm_for_serve = battery_manager.clone();
        let serve_fut = async move {
            bm_for_serve.serve_simulator(stream).await.unwrap();
        };

        let client_fut = async move {
            assert_eq!(proxy.is_simulating().await.unwrap(), false);
            assert_eq!(proxy.get_battery_info().await.unwrap().level_percent, Some(30.0));

            proxy.disconnect_real_battery().unwrap();
            assert_eq!(proxy.is_simulating().await.unwrap(), true);

            proxy.set_battery_percentage(85.0).unwrap();
            let simulated_info = proxy.get_battery_info().await.unwrap();
            assert_eq!(simulated_info.level_percent, Some(85.0));
            assert_eq!(simulated_info.level_status, Some(fpower::LevelStatus::Ok));
            assert_eq!(simulated_info.charge_status, Some(fpower::ChargeStatus::Charging));

            proxy.reconnect_real_battery().unwrap();
            assert_eq!(proxy.is_simulating().await.unwrap(), false);
            let restored_info = proxy.get_battery_info().await.unwrap();
            assert_eq!(restored_info.level_percent, Some(30.0));
            assert_eq!(restored_info.level_status, Some(fpower::LevelStatus::Low));
        };

        join(serve_fut, client_fut).await;
    }
}
