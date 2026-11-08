// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod fake_governor_votes;
mod fake_telemetry_governor_votes;

use anyhow::Result;
use battery_manager_test_harness::{
    DEFAULT_CHARGER_MAX_CURRENT_UA, DEFAULT_CHARGER_MAX_VOLTAGE_UV, FakeCharger, FidlRouteMode,
    LeaseEvent, battery_manager_moniker, setup_realm, setup_realm_with_charger,
    setup_realm_with_config, wait_for_battery_info, wait_for_inspect_match,
    wait_for_matching_battery_info,
};
use diagnostics_assertions::{TreeAssertion, tree_assertion};
use fake_governor_votes::{FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA, FAKE_GOVERNOR_OPERATING_MODE};
use fake_telemetry_governor_votes::{
    MAX_INTERVAL_MILLIS, SAMPLED_RESPONSES, charge_current_limit_ua_for_level,
    charge_term_current_ua_for_phase, input_current_limit_ua_for_responses,
};
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use fidl_fuchsia_testing as ftesting;
use fidl_test_hardwarepowercontrol as ftest_battery;
use futures::StreamExt as _;
use futures::channel::mpsc;
use test_case::test_case;
use test_util::assert_gt;

const REFRESHED_CHARGING_CURRENT_UA: i32 = 1_500_000;
const REFRESHED_DISCHARGING_CURRENT_UA: i32 = -400_000;
const STARTUP_WITH_CHARGER_LEASE_EVENT_COUNT: usize = 5;

/// Config bundled in the test package, used by battery-manager when `/config` is not routed.
const PKG_TEST_CONFIG_PATH: &str = "/pkg/config/test_config.json";
const CONFIG_KEY_GOVERNOR_URLS: &str = "governor_urls";

const FAKE_GOVERNOR_URL: &str = "#meta/fake_governor.cm";
/// Child (and arbiter voter) name battery-manager derives from `FAKE_GOVERNOR_URL`.
const FAKE_GOVERNOR_NAME: &str = "fake_governor";
const FAKE_TELEMETRY_GOVERNOR_URL: &str = "#meta/fake_telemetry_governor.cm";
/// Child (and arbiter voter) name battery-manager derives from `FAKE_TELEMETRY_GOVERNOR_URL`.
const FAKE_TELEMETRY_GOVERNOR_NAME: &str = "fake_telemetry_governor";
/// Battery level the fake battery reports after the governor received the default status.
const TELEMETRY_UPDATED_LEVEL_PERCENT: f32 = 42.0;

fn assert_default_battery_info(info: &fpower::BatteryInfo) {
    assert_eq!(info.level_percent, Some(ftest_battery::DEFAULT_ROUNDED_LEVEL_PERCENT as f32));
    assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(info.charge_source, Some(fpower::ChargeSource::AcAdapter));
    assert_eq!(info.present_voltage_mv, Some(ftest_battery::DEFAULT_PRESENT_VOLTAGE_MV));
    assert_eq!(info.remaining_charge_uah, Some(ftest_battery::DEFAULT_REMAINING_CHARGE_UAH));
    assert!(info.timestamp.is_some());
}

async fn update_and_check(
    control: &ftest_battery::ControlProxy,
    fake_clock_control: &ftesting::FakeClockControlProxy,
    watcher_stream: &mut fpower::BatteryInfoWatcherRequestStream,
    advance_time: zx::MonotonicDuration,
    raw_level: f32,
    expected_scaled: f32,
) -> Result<()> {
    fake_clock_control
        .advance(&ftesting::Increment::Determined(advance_time.into_nanos()))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;

    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(raw_level),
            current_ua: Some(250_000),
            ..Default::default()
        })
        .await?;

    let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
        info, responder, ..
    })) = watcher_stream.next().await
    else {
        return Err(anyhow::anyhow!("Watcher stream ended prematurely"));
    };
    responder.send()?;
    let level = info.level_percent.expect("level_percent missing");
    assert!(
        (level - expected_scaled).abs() < f32::EPSILON,
        "expected level_percent {expected_scaled}, got {level}"
    );
    Ok(())
}

/// Verifies that `Watch` and `GetBatteryInfo` report the fake battery's default info for each
/// FIDL route mode.
#[test_case(FidlRouteMode::NewOnly; "new_only")]
#[test_case(FidlRouteMode::OldOnly; "old_only")]
#[test_case(FidlRouteMode::Both; "both")]
#[fuchsia::test]
async fn test_get_battery_info(mode: FidlRouteMode) -> Result<()> {
    let (realm, _lease_events) = setup_realm(mode, false).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (info, _stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);

    let get_info = battery_mgr.get_battery_info().await?;
    assert_default_battery_info(&get_info);
    Ok(())
}

/// Verifies that a `BatteryInfoWatcher` receives the fake battery's default info.
#[fuchsia::test]
async fn test_watcher() -> Result<()> {
    let (realm, _lease_events) = setup_realm(FidlRouteMode::Both, false).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (info, _stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);
    Ok(())
}

/// Verifies that after `DisconnectRealBattery`, watchers receive the level and charge status
/// set through `BatterySimulator`.
#[fuchsia::test]
async fn test_simulator() -> Result<()> {
    let (realm, _lease_events) = setup_realm(FidlRouteMode::Both, false).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let simulator: spower::BatterySimulatorProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from DTR driver first
    let (initial_info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&initial_info);

    // Now disconnect real battery to trigger simulation mode
    simulator.disconnect_real_battery()?;

    // Wait for the simulation mode update to be propagated
    if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo { responder, .. })) =
        watcher_stream.next().await
    {
        responder.send()?;
    }

    // Setting only the simulated battery percentage should preserve existing real driver fields
    // while updating level_percent and level_status.
    simulator.set_battery_percentage(77.0)?;
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.level_percent == Some(77.0) {
                assert_eq!(info.level_status, Some(fpower::LevelStatus::Warning));
                assert_eq!(info.charge_status, initial_info.charge_status);
                assert_eq!(info.charge_source, initial_info.charge_source);
                assert_eq!(info.present_voltage_mv, initial_info.present_voltage_mv);
                assert_eq!(info.remaining_charge_uah, initial_info.remaining_charge_uah);
                break;
            }
        } else {
            panic!("Watcher stream ended before receiving simulated 77% update");
        }
    }

    // Now push simulated updates
    simulator.set_battery_percentage(50.0)?;
    simulator.set_charge_status(fpower::ChargeStatus::Discharging)?;

    // We should receive the updated battery info callback
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.level_percent == Some(50.0)
                && info.charge_status == Some(fpower::ChargeStatus::Discharging)
            {
                break;
            }
        } else {
            panic!("Watcher stream ended before receiving expected simulated updates");
        }
    }

    // Reconnecting the real battery should restore the real driver's BatteryInfo.
    simulator.reconnect_real_battery()?;
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.level_percent == initial_info.level_percent
                && info.charge_status == initial_info.charge_status
            {
                assert_default_battery_info(&info);
                break;
            }
        } else {
            panic!("Watcher stream ended before receiving reconnected real battery info");
        }
    }
    Ok(())
}

/// Verifies that a driver status update reaches watchers scaled, with a later timestamp.
#[fuchsia::test]
async fn test_watch_dynamic_updates() -> Result<()> {
    let (realm, _lease_events) = setup_realm(FidlRouteMode::NewOnly, false).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from driver
    let (info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);
    let t0 = info.timestamp.expect("timestamp missing from default info");

    let fake_clock_control =
        realm.root.connect_to_protocol_at_exposed_dir::<ftesting::FakeClockControlProxy>()?;

    fake_clock_control.pause().await?;

    // Advance fake time by 10 seconds.
    fake_clock_control
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_seconds(10).into_nanos(),
        ))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;

    // Now update fake battery using driver Control.
    // Raw level 99.1% maps to 100.0% scaled level after processing.
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(99.1),
            current_ua: Some(250_000),
            ..Default::default()
        })
        .await?;

    // Wait for the update on watcher stream
    let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
        info, responder, ..
    })) = watcher_stream.next().await
    else {
        panic!("Watcher stream ended before receiving expected driver updates");
    };
    responder.send()?;
    let level = info.level_percent.expect("level_percent missing");
    assert!((level - 100.0).abs() < f32::EPSILON, "expected 100.0, got {level}");
    assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Charging));
    let t1 = info.timestamp.expect("timestamp missing from first update");
    assert_gt!(t1, t0);

    Ok(())
}

/// Verifies that raw levels are scaled above the shutdown offset and capped at 100%.
#[fuchsia::test]
async fn test_shutdown_offset_scaling() -> Result<()> {
    let (realm, _lease_events) = setup_realm(FidlRouteMode::NewOnly, false).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from driver
    let (info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);

    let fake_clock_control =
        realm.root.connect_to_protocol_at_exposed_dir::<ftesting::FakeClockControlProxy>()?;
    fake_clock_control.pause().await?;

    let advance_1000s = zx::MonotonicDuration::from_seconds(1000);

    // Test cases:
    // 1. Raw level at or below offset (3.0%) -> Scaled level 0.0%
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 4.0, 2.0)
        .await?;
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 3.1, 1.0)
        .await?;
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 3.0, 0.0)
        .await?;
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 2.0, 0.0)
        .await?;

    // 2. Raw level in middle (51.5%) -> Scaled level 50.0%
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 51.5, 50.0)
        .await?;

    // 3. Raw level 99.0% -> Scaled level 99.0%
    update_and_check(&control, &fake_clock_control, &mut watcher_stream, advance_1000s, 99.0, 99.0)
        .await?;

    // 4. Raw level 100% -> Scaled level 100.0%
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        advance_1000s,
        100.0,
        100.0,
    )
    .await?;

    // 5. Raw level above 100% (101.0%) -> Scaled level capped at 100.0%
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        advance_1000s,
        101.0,
        100.0,
    )
    .await?;

    Ok(())
}

/// Verifies that the startup wake lease is released after the first update, and that charging
/// holds the suspend-blocking lease until the battery discharges.
#[fuchsia::test]
async fn test_charging_wake_lease() -> Result<()> {
    // 1. Setup realm with suspend_enabled = true
    let (realm, mut lease_events) = setup_realm(FidlRouteMode::NewOnly, true).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    // 2. The battery-manager starts watching driver updates.
    // It should immediately connect to fake_sag and acquire the startup lease "battery_manager".
    // Wait for the first LeaseEvent::Acquired("battery_manager")
    let event1 = lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(event1, LeaseEvent::Acquired("battery_manager".to_string()));

    // 3. Connect a watcher client to battery_manager
    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from driver first
    let (_info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;

    // The startup lease should be dropped now
    let event_drop =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(event_drop, LeaseEvent::Dropped("battery_manager".to_string()));

    // 4. Inject a charging status update via Driver Control (AcAdapter plugged in)
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(50.0),
            current_ua: Some(250_000),
            ..Default::default()
        })
        .await?;

    // Wait for the watcher stream to propagate the Charging status
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.charge_status == Some(fpower::ChargeStatus::Charging)
                && info.charge_source == Some(fpower::ChargeSource::AcAdapter)
            {
                break;
            }
        } else {
            return Err(anyhow::anyhow!("Watcher stream ended prematurely"));
        }
    }

    // 5. Verify that fake_sag receives AcquireUnmonitoredWakeLease("charging_block_suspension")
    let event2 = lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(event2, LeaseEvent::Acquired("charging_block_suspension".to_string()));

    // 6. Unplug the charger (Discharging, no source)
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(50.0),
            current_ua: Some(-250_000),
            ..Default::default()
        })
        .await?;

    // Wait for the watcher stream to propagate the Discharging status
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.charge_status == Some(fpower::ChargeStatus::Discharging) {
                break;
            }
        } else {
            return Err(anyhow::anyhow!("Watcher stream ended prematurely"));
        }
    }

    // 7. Verify that the wake lease was dropped (Fake SAG detects PEER_CLOSED)
    let event3 = lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(event3, LeaseEvent::Dropped("charging_block_suspension".to_string()));

    Ok(())
}

/// Verifies that the rate limiter moves the reported level toward a jump by at most 2% per 15s.
#[fuchsia::test]
async fn test_rate_limiter() -> Result<()> {
    // 1. Setup realm with suspend_enabled = false
    let (realm, _lease_events) = setup_realm(FidlRouteMode::NewOnly, false).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let fake_clock_control =
        realm.root.connect_to_protocol_at_exposed_dir::<ftesting::FakeClockControlProxy>()?;
    fake_clock_control.pause().await?;

    // Wait for the initial update from driver (which is scaled 98.7% -> 99.0%)
    let (info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_eq!(info.level_percent, Some(99.0));

    // 2. Set raw level to 51.5% -> maps to 50.0% scaled level.
    // Advance time by 1000s to let it settle immediately (bypass the rate limiter for the setup).
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        zx::MonotonicDuration::from_seconds(1000),
        51.5,
        50.0,
    )
    .await?;

    // 3. Suddenly update the raw level to 61.2% -> maps to 60.0% scaled level.
    // Step 1: Advance clock by 15 seconds. Max delta is 2.0%.
    // So the level should increase to 52.0%.
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        zx::MonotonicDuration::from_seconds(15),
        61.2,
        52.0,
    )
    .await?;

    // Step 2: Advance clock by another 15 seconds.
    // The level should increase to 54.0%.
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        zx::MonotonicDuration::from_seconds(15),
        61.2,
        54.0,
    )
    .await?;

    // Step 3: Advance clock by 100 seconds to let it fully reach 60.0%.
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        zx::MonotonicDuration::from_seconds(100),
        61.2,
        60.0,
    )
    .await?;

    Ok(())
}

async fn collect_lease_events(
    lease_events: &mut mpsc::Receiver<LeaseEvent>,
    count: usize,
) -> Result<Vec<LeaseEvent>> {
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        let event =
            lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
        events.push(event);
    }
    Ok(events)
}

/// Verifies that charger telemetry and spec are merged into battery info, and that source type
/// changes are reflected.
#[fuchsia::test]
async fn test_charger_discovery_and_merged_telemetry() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait until both battery and charger services have been discovered and merged.
    let (info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.battery_spec.as_ref().is_some_and(|spec| {
                spec.design_capacity_uah.is_some() && spec.max_charging_current_ua.is_some()
            })
    })
    .await?;

    assert_eq!(info.charge_source, Some(fpower::ChargeSource::Usb));
    assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(info.level_percent, Some(ftest_battery::DEFAULT_ROUNDED_LEVEL_PERCENT));
    assert_eq!(info.present_voltage_mv, Some(ftest_battery::DEFAULT_PRESENT_VOLTAGE_MV));
    assert_eq!(info.remaining_charge_uah, Some(ftest_battery::DEFAULT_REMAINING_CHARGE_UAH));

    let expected_spec = fpower::BatterySpec {
        design_capacity_uah: Some(ftest_battery::DEFAULT_FULL_CAPACITY_UAH as i32),
        max_charging_current_ua: Some(DEFAULT_CHARGER_MAX_CURRENT_UA as i32),
        max_charging_voltage_uv: Some(DEFAULT_CHARGER_MAX_VOLTAGE_UV as i32),
        ..Default::default()
    };
    assert_eq!(info.battery_spec, Some(expected_spec.clone()));

    let polled_info = battery_mgr.get_battery_info().await?;
    assert_eq!(polled_info.charge_source, Some(fpower::ChargeSource::Usb));
    assert_eq!(polled_info.battery_spec, Some(expected_spec.clone()));

    // Switch charger source type to Wireless and verify override updates dynamically.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Wireless),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (wireless_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Wireless)
    })
    .await?;
    assert_eq!(wireless_info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(wireless_info.battery_spec, Some(expected_spec));

    Ok(())
}

/// Verifies that the suspend-blocking wake lease follows the charger's operating mode: held while
/// charging, dropped in passthrough and OTG.
#[fuchsia::test]
async fn test_charger_operating_modes_and_wake_leases() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, mut lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, true, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (_initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;

    // Both battery and charger watch loops acquire and release their startup wake leases,
    // and the active charging state acquires the suspension-blocking wake lease.
    let startup_events =
        collect_lease_events(&mut lease_events, STARTUP_WITH_CHARGER_LEASE_EVENT_COUNT).await?;
    assert!(startup_events.contains(&LeaseEvent::Acquired("battery_manager".to_string())));
    assert!(startup_events.contains(&LeaseEvent::Dropped("battery_manager".to_string())));
    assert!(startup_events.contains(&LeaseEvent::Acquired("battery_manager_charger".to_string())));
    assert!(startup_events.contains(&LeaseEvent::Dropped("battery_manager_charger".to_string())));
    assert!(
        startup_events.contains(&LeaseEvent::Acquired("charging_block_suspension".to_string()))
    );

    // Transition charger to Passthrough (NotCharging) -> wake lease must be dropped.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Passthrough),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });

    let (passthrough_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::NotCharging)
    })
    .await?;
    assert_eq!(passthrough_info.charge_source, Some(fpower::ChargeSource::Usb));

    let drop_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(drop_event, LeaseEvent::Dropped("charging_block_suspension".to_string()));

    // Transition charger back to Charging -> wake lease must be re-acquired.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (resumed_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;
    assert_eq!(resumed_info.charge_source, Some(fpower::ChargeSource::Usb));

    let reacquire_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(reacquire_event, LeaseEvent::Acquired("charging_block_suspension".to_string()));

    // Transition charger to OTG (Discharging) -> wake lease must be dropped again.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Otg),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });

    let (otg_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(otg_info.charge_source, Some(fpower::ChargeSource::None));

    let otg_drop_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(otg_drop_event, LeaseEvent::Dropped("charging_block_suspension".to_string()));

    Ok(())
}

/// Verifies that when the charger disconnects, battery-manager falls back to battery-only
/// telemetry and drops the suspend-blocking wake lease.
#[fuchsia::test]
async fn test_charger_disconnect_fallback() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, mut lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, true, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for initial merged state where charger overrides battery telemetry.
    let (_initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
            && info.battery_spec.as_ref().is_some_and(|spec| {
                spec.design_capacity_uah.is_some() && spec.max_charging_current_ua.is_some()
            })
    })
    .await?;

    let startup_events =
        collect_lease_events(&mut lease_events, STARTUP_WITH_CHARGER_LEASE_EVENT_COUNT).await?;
    assert!(
        startup_events.contains(&LeaseEvent::Acquired("charging_block_suspension".to_string()))
    );

    // Put the underlying battery driver into a discharging state while the charger remains active.
    // While the charger channel is open, charger telemetry continues to override charge_source
    // and charge_status.
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(51.5),
            current_ua: Some(REFRESHED_DISCHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;

    let (overridden_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.present_charging_current_ua == Some(REFRESHED_DISCHARGING_CURRENT_UA)
    })
    .await?;
    assert_eq!(overridden_info.charge_source, Some(fpower::ChargeSource::Usb));
    assert_eq!(overridden_info.charge_status, Some(fpower::ChargeStatus::Charging));

    // Close the charger FIDL channel (ChargerGone).
    fake_charger.disconnect();

    // Verify battery_manager clears cached charger state/spec, falls back to battery-only
    // telemetry, and drops the "charging_block_suspension" wake lease.
    let (fallback_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(fallback_info.charge_source, Some(fpower::ChargeSource::None));
    assert_eq!(
        fallback_info.battery_spec,
        Some(fpower::BatterySpec {
            design_capacity_uah: Some(ftest_battery::DEFAULT_FULL_CAPACITY_UAH as i32),
            max_charging_current_ua: None,
            max_charging_voltage_uv: None,
            ..Default::default()
        })
    );

    let drop_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(drop_event, LeaseEvent::Dropped("charging_block_suspension".to_string()));

    Ok(())
}

/// Verifies that plugging in or unplugging the charger refreshes the battery current from
/// `Battery.GetStatus`.
#[fuchsia::test]
async fn test_charger_update_refreshes_battery_status() -> Result<()> {
    // Start with the charger offline.
    let fake_charger = FakeCharger::new(
        fcharger::Spec {
            max_charge_current_ua: Some(DEFAULT_CHARGER_MAX_CURRENT_UA),
            max_charge_voltage_uv: Some(DEFAULT_CHARGER_MAX_VOLTAGE_UV),
            ..Default::default()
        },
        fcharger::Status {
            online: Some(false),
            operating_mode: Some(fcharger::OperatingMode::Discharging),
            charge_phase: Some(fcharger::ChargePhase::None),
            ..Default::default()
        },
    );
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial merged state so the initial `ChargerStatus` (and its on-demand
    // `Battery::GetStatus` refresh) has already been processed before we synchronize the
    // `Battery::Watch` pipeline.
    wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.remaining_charge_uah == Some(ftest_battery::DEFAULT_REMAINING_CHARGE_UAH)
            && info.charge_source == Some(fpower::ChargeSource::None)
            && info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;

    // Synchronize the battery watch pipeline with an explicit `remaining_capacity_uah` update
    // (alongside `level_percent`, which is in `supported_options.interest`) so the initial
    // `Battery::Watch` response is guaranteed drained before testing passive `current_ua` reads.
    const SYNC_REMAINING_CAPACITY_UAH: u32 = 2_800_000;
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(ftest_battery::DEFAULT_LEVEL_PERCENT),
            remaining_capacity_uah: Some(SYNC_REMAINING_CAPACITY_UAH),
            ..Default::default()
        })
        .await?;

    let (initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.remaining_charge_uah == Some(SYNC_REMAINING_CAPACITY_UAH)
            && info.charge_source == Some(fpower::ChargeSource::None)
            && info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(
        initial_info.present_charging_current_ua,
        Some(ftest_battery::DEFAULT_CHARGING_CURRENT_UA)
    );

    // Update only `current_ua` on the battery driver. Because `current_ua` is not in the fake
    // battery driver's `supported_options.interest`, this updates `Battery::GetStatus` without
    // triggering a `Battery::Watch` notification.
    control
        .set_battery_status(&fbattery::Status {
            current_ua: Some(REFRESHED_CHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;

    let cached_before_plug = battery_mgr.get_battery_info().await?;
    assert_eq!(
        cached_before_plug.present_charging_current_ua,
        Some(ftest_battery::DEFAULT_CHARGING_CURRENT_UA)
    );

    // Trigger a charger online transition; battery_manager should proactively poll
    // `Battery::GetStatus` and publish the refreshed battery current alongside the charger state.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (plugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;
    assert_eq!(plugged_info.present_charging_current_ua, Some(REFRESHED_CHARGING_CURRENT_UA));

    // Update `current_ua` again without a battery Watch notification, then unplug the charger.
    control
        .set_battery_status(&fbattery::Status {
            current_ua: Some(REFRESHED_DISCHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;

    fake_charger.set_status(fcharger::Status {
        online: Some(false),
        operating_mode: Some(fcharger::OperatingMode::Discharging),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });

    let (unplugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::None)
            && info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(unplugged_info.present_charging_current_ua, Some(REFRESHED_DISCHARGING_CURRENT_UA));

    Ok(())
}

/// Verifies that the charger's phase resolves to Charging, NotCharging, or Full with the matching
/// time remaining.
#[fuchsia::test]
async fn test_charger_phase_and_full_resolution() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for initial merged state: default battery is at 99.0% scaled (< 100%) with positive
    // current (+250 mA), and charger is in `ChargePhase::Fast`, producing a positive `FullCharge`.
    let (charging_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;
    match charging_info.time_remaining {
        Some(fpower::TimeRemaining::FullCharge(nanos)) => assert_gt!(nanos, 0),
        other => panic!("Expected positive FullCharge time_remaining, got {:?}", other),
    }

    // Charger reports ChargePhase::None while online in Charging mode (e.g. fault/inhibit) ->
    // resolves to ChargeStatus::NotCharging and Indeterminate(0).
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });

    let (inhibited_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::NotCharging)
    })
    .await?;
    assert_eq!(inhibited_info.charge_source, Some(fpower::ChargeSource::Usb));
    assert_eq!(inhibited_info.time_remaining, Some(fpower::TimeRemaining::Indeterminate(0)));

    // Charger reports ChargePhase::Done at the default (full) level -> resolves to
    // ChargeStatus::Full and FullCharge(0).
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Done),
        ..Default::default()
    });

    let (done_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Full)
    })
    .await?;
    assert_eq!(done_info.charge_source, Some(fpower::ChargeSource::Usb));
    assert_eq!(done_info.time_remaining, Some(fpower::TimeRemaining::FullCharge(0)));

    // Termination well below a full pack (e.g. a float voltage below the pack's full-charge
    // voltage) -> resolves to ChargeStatus::NotCharging rather than Full.
    control
        .set_battery_status(&fbattery::Status { level_percent: Some(88.5), ..Default::default() })
        .await?;

    let (early_done_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::NotCharging)
    })
    .await?;
    assert_eq!(early_done_info.charge_source, Some(fpower::ChargeSource::Usb));

    Ok(())
}

/// Verifies that a charger wake lease is forwarded to watchers and handed back to the charger on
/// its next `Watch`.
#[fuchsia::test]
async fn test_charger_wake_lease_handoff() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (_initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
    })
    .await?;

    // Inject a charger update carrying a wake_lease token.
    let (local_lease, remote_lease) = zx::EventPair::create();
    let expected_koid = local_lease.basic_info()?.koid;

    fake_charger.set_status_with_wake_lease(
        fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Ac),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            ..Default::default()
        },
        Some(remote_lease),
    );

    let (_ac_info, watcher_wake_lease) =
        wait_for_matching_battery_info(&mut watcher_stream, |info| {
            info.charge_source == Some(fpower::ChargeSource::AcAdapter)
        })
        .await?;

    // Verify the duplicated wake lease was forwarded to the BatteryInfoWatcher client.
    let watcher_wake_lease =
        watcher_wake_lease.expect("Expected wake_lease forwarded to BatteryInfoWatcher");
    assert_eq!(watcher_wake_lease.basic_info()?.related_koid, expected_koid);

    // Trigger one more charger update so we know battery_manager has issued the subsequent
    // `Charger::Watch(lease)` call handing back the previous wake lease.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Wireless),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (_wireless_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Wireless)
    })
    .await?;

    assert_eq!(fake_charger.last_received_watch_lease_related_koid(), Some(expected_koid));

    Ok(())
}

/// Verifies that battery-manager discovers a charger instance published after startup, falls back
/// when it disconnects, and reconnects to a newly published instance.
#[fuchsia::test]
async fn test_charger_late_discovery_and_reconnection() -> Result<()> {
    // Start with the charger service directory routed, but with no published service instances yet.
    let fake_charger = FakeCharger::default_usb_charging_unpublished();
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Initially, only the battery driver is active, so `charge_source` is inferred as `AcAdapter`
    // and charger spec fields are `None`.
    let (battery_only_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::AcAdapter)
    })
    .await?;
    assert_eq!(
        battery_only_info.battery_spec,
        Some(fpower::BatterySpec {
            design_capacity_uah: Some(ftest_battery::DEFAULT_FULL_CAPACITY_UAH as i32),
            max_charging_current_ua: None,
            max_charging_voltage_uv: None,
            ..Default::default()
        })
    );

    // Dynamically publish the "default" charger service instance; `main.rs`'s `Service::watch()`
    // loop should discover it and merge charger telemetry and spec.
    fake_charger.publish_instance("default");

    let (discovered_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Usb)
    })
    .await?;
    assert_eq!(discovered_info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(
        discovered_info.battery_spec,
        Some(fpower::BatterySpec {
            design_capacity_uah: Some(ftest_battery::DEFAULT_FULL_CAPACITY_UAH as i32),
            max_charging_current_ua: Some(DEFAULT_CHARGER_MAX_CURRENT_UA as i32),
            max_charging_voltage_uv: Some(DEFAULT_CHARGER_MAX_VOLTAGE_UV as i32),
            ..Default::default()
        })
    );

    // Disconnect the charger instance (`ChargerGone`) and verify fallback to battery-only state.
    fake_charger.disconnect();

    let (fallback_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::AcAdapter)
    })
    .await?;
    assert_eq!(fallback_info.battery_spec.and_then(|s| s.max_charging_current_ua), None);

    // Publish a new charger service instance ("reconnected") reporting Wireless charging and
    // verify `battery_manager` reconnects and resumes charger-merged telemetry.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Wireless),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });
    fake_charger.publish_instance("reconnected");

    let (reconnected_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Wireless)
    })
    .await?;
    assert_eq!(reconnected_info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(
        reconnected_info.battery_spec.and_then(|s| s.max_charging_current_ua),
        Some(DEFAULT_CHARGER_MAX_CURRENT_UA as i32)
    );

    Ok(())
}

/// Verifies that plugging in the charger resets the average current behind time-to-full.
#[fuchsia::test]
async fn test_charger_plug_resets_average_current() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, false, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (_initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.remaining_charge_uah == Some(ftest_battery::DEFAULT_REMAINING_CHARGE_UAH)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;

    let fake_clock_control =
        realm.root.connect_to_protocol_at_exposed_dir::<ftesting::FakeClockControlProxy>()?;
    fake_clock_control.pause().await?;

    // Accumulate > 60s of negative battery current while `ChargeStatus::Charging` so
    // `Polisher`'s internal `ChargeTimeEstimator` computes a negative average current and
    // reports `TimeRemaining::Indeterminate(0)`.
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(ftest_battery::DEFAULT_LEVEL_PERCENT),
            remaining_capacity_uah: Some(2_800_000),
            current_ua: Some(REFRESHED_DISCHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;
    wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.remaining_charge_uah == Some(2_800_000)
    })
    .await?;

    fake_clock_control
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_seconds(100).into_nanos(),
        ))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;

    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(ftest_battery::DEFAULT_LEVEL_PERCENT),
            remaining_capacity_uah: Some(2_750_000),
            current_ua: Some(REFRESHED_DISCHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;
    let (neg_avg_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.remaining_charge_uah == Some(2_750_000)
    })
    .await?;
    assert_eq!(neg_avg_info.time_remaining, Some(fpower::TimeRemaining::Indeterminate(0)));

    // Without unplugging, a 1s positive current sample is outweighed by the 100s of negative
    // current in the estimator's accumulator, so `time_remaining` remains `Indeterminate(0)`.
    fake_clock_control
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_seconds(1).into_nanos(),
        ))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;

    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(ftest_battery::DEFAULT_LEVEL_PERCENT),
            remaining_capacity_uah: Some(2_760_000),
            current_ua: Some(REFRESHED_CHARGING_CURRENT_UA),
            ..Default::default()
        })
        .await?;
    let (still_indeterminate_info, _) =
        wait_for_matching_battery_info(&mut watcher_stream, |info| {
            info.remaining_charge_uah == Some(2_760_000)
        })
        .await?;
    assert_eq!(
        still_indeterminate_info.time_remaining,
        Some(fpower::TimeRemaining::Indeterminate(0))
    );

    // Unplug the charger (`is_plugged_in: true -> false`), then plug it back in
    // (`!old_is_plugged_in && new_is_plugged_in`).
    fake_charger.set_status(fcharger::Status {
        online: Some(false),
        operating_mode: Some(fcharger::OperatingMode::Discharging),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });
    wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;

    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Ac),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    // The plug-in edge resets `Polisher`'s average current accumulator, so `time_remaining`
    // immediately computes a positive `FullCharge` from the current positive `current_ua`.
    let (replugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::AcAdapter)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;
    assert_eq!(replugged_info.present_charging_current_ua, Some(REFRESHED_CHARGING_CURRENT_UA));
    match replugged_info.time_remaining {
        Some(fpower::TimeRemaining::FullCharge(nanos)) => assert_gt!(nanos, 0),
        other => {
            panic!("Expected positive FullCharge after plug-in accumulator reset, got {:?}", other)
        }
    }

    Ok(())
}

/// Verifies that the suspend-blocking wake lease stays held across Charging -> Full -> Charging.
#[fuchsia::test]
async fn test_charger_full_retains_wake_lease() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, mut lease_events) =
        setup_realm_with_charger(FidlRouteMode::NewOnly, true, Some(fake_charger.clone())).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (_initial_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.status == Some(fpower::BatteryStatus::Ok)
            && info.charge_source == Some(fpower::ChargeSource::Usb)
            && info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;

    let startup_events =
        collect_lease_events(&mut lease_events, STARTUP_WITH_CHARGER_LEASE_EVENT_COUNT).await?;
    assert!(
        startup_events.contains(&LeaseEvent::Acquired("charging_block_suspension".to_string()))
    );

    // Transition Charging -> Full -> Charging while still plugged in.
    // The "charging_block_suspension" wake lease must remain held across both transitions
    // without being dropped or re-acquired.
    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Done),
        ..Default::default()
    });

    let (full_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Full)
    })
    .await?;
    assert_eq!(full_info.charge_source, Some(fpower::ChargeSource::Usb));

    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (_charging_again_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;

    // Unplug the charger (Discharging) and then plug back in (Charging).
    // If Full had dropped the lease, `lease_events` would have queued an extra Dropped + Acquired
    // pair before this unplug; instead, the next two lease events must be the single Dropped from
    // unplugging followed by the Acquired from re-plugging.
    fake_charger.set_status(fcharger::Status {
        online: Some(false),
        operating_mode: Some(fcharger::OperatingMode::Discharging),
        charge_phase: Some(fcharger::ChargePhase::None),
        ..Default::default()
    });

    let (unplugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(unplugged_info.charge_source, Some(fpower::ChargeSource::None));

    let drop_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(drop_event, LeaseEvent::Dropped("charging_block_suspension".to_string()));

    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Fast),
        ..Default::default()
    });

    let (_replugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Charging)
    })
    .await?;

    let reacquire_event =
        lease_events.next().await.ok_or_else(|| anyhow::anyhow!("lease_events ended"))?;
    assert_eq!(reacquire_event, LeaseEvent::Acquired("charging_block_suspension".to_string()));

    Ok(())
}

/// Verifies that unplugging while the reported level rounds to 100% holds 100% until the battery
/// drops `ssoc_delta` below the unplug level.
#[fuchsia::test]
async fn test_unplug_when_rounded_to_100_holds_full_for_ssoc_delta() -> Result<()> {
    let (realm, _lease_events) = setup_realm(FidlRouteMode::NewOnly, false).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;
    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;
    let fake_clock_control =
        realm.root.connect_to_protocol_at_exposed_dir::<ftesting::FakeClockControlProxy>()?;
    fake_clock_control.pause().await?;

    // 1. Initial driver state is 98.7% raw -> 99.0% scaled/rate-limited while Charging.
    let (info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_eq!(info.level_percent, Some(99.0));
    assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Charging));

    // 2. Advance clock by 6s. With `rl_max_delta_soc = 2.0` and `rl_max_time_s = 15.0`,
    // the rate limiter allows a +0.8% step (99.0% -> 99.8% internal `rate_limited_soc`) when
    // the raw level reaches 99.1% (100.0% scaled).
    // `Polisher::round_reported_level(99.8)` rounds the published `level_percent` to 100.0%
    // and `calculate_time_to_full` reports `FullCharge(0)`.
    fake_clock_control
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_seconds(6).into_nanos(),
        ))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(99.1),
            current_ua: Some(250_000),
            ..Default::default()
        })
        .await?;
    let (full_ui_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.level_percent == Some(100.0)
    })
    .await?;
    assert_eq!(full_ui_info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(full_ui_info.time_remaining, Some(fpower::TimeRemaining::FullCharge(0)));

    // 3. Unplug the charger (negative current -> Discharging) at 98.0% raw (98.0% scaled).
    // Because the UI was displaying 100% (`FullCharge(0)`), disconnecting should splice the
    // discharge curve midpoint at `scaled_real_soc - ssoc_delta = 98.0 - 2.0 = 96.0` with
    // `ui = 100.0`, holding the reported UI level at 100% down to 96.0% scaled.
    fake_clock_control
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_seconds(1000).into_nanos(),
        ))
        .await?
        .map_err(|e| anyhow::anyhow!("failed to advance fake clock: {:?}", e))?;
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(98.0),
            current_ua: Some(-250_000),
            ..Default::default()
        })
        .await?;
    let (unplugged_info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_status == Some(fpower::ChargeStatus::Discharging)
    })
    .await?;
    assert_eq!(unplugged_info.level_percent, Some(100.0));

    // 4. Discharge to 96.1% raw -> `((96.1 - 3.0) * 100.0 / 97.0).ceil() = 96.0%` scaled,
    // which is still within the 2.0% `ssoc_delta` hold window `[96.0%, 98.0%]`.
    // The reported UI level should remain at 100.0% (whereas if `update_curve_state` checks
    // unrounded `99.8 >= 100.0`, it skips the `ssoc_delta` hold and drops to 98.0%).
    update_and_check(
        &control,
        &fake_clock_control,
        &mut watcher_stream,
        zx::MonotonicDuration::from_seconds(1000),
        96.1,
        100.0,
    )
    .await?;
    Ok(())
}

/// Returns the config to route to battery-manager as `/config/config.json`: the package's
/// `test_config.json` (what battery-manager reads when `/config` is not routed) with
/// `governor_urls` replaced by `governor_url`.
fn governor_config_json(governor_url: &str) -> Result<String> {
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(PKG_TEST_CONFIG_PATH)?)?;
    config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{PKG_TEST_CONFIG_PATH} is not a JSON object"))?
        .insert(CONFIG_KEY_GOVERNOR_URLS.to_string(), serde_json::json!([governor_url]));
    Ok(serde_json::to_string(&config)?)
}

/// Verifies that battery-manager launches a configured governor into `#governors`, hands it a
/// charger `Controller` through `CreateChildArgs.additional_inputs`, and that the arbiter
/// attributes the governor's votes to its child name. No charger driver is routed, so the arbiter
/// only caches the votes.
#[fuchsia::test]
async fn test_governor_votes_attributed_by_name() -> Result<()> {
    let (realm, _lease_events) = setup_realm_with_config(
        FidlRouteMode::NewOnly,
        false,
        None,
        Some(governor_config_json(FAKE_GOVERNOR_URL)?),
    )
    .await?;

    // Connecting starts battery-manager, which then launches its configured governors.
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let _ = battery_mgr.get_battery_info().await?;

    let moniker = battery_manager_moniker(&realm);
    let expected_limit = u64::from(FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA);
    let expected_mode = format!("{FAKE_GOVERNOR_OPERATING_MODE:?}");
    let assertion = tree_assertion!(root: contains {
        governors: {
            var FAKE_GOVERNOR_NAME: {
                url: FAKE_GOVERNOR_URL,
                state: "running",
                restarts: 0u64,
            },
        },
        charge_limit_arbiter: {
            elections: contains {
                charge_current_limit_ua: {
                    result: expected_limit,
                    overridden: false,
                    votes: {
                        var FAKE_GOVERNOR_NAME: expected_limit,
                    },
                },
                operating_mode: {
                    result: expected_mode.clone(),
                    overridden: false,
                    votes: {
                        var FAKE_GOVERNOR_NAME: expected_mode,
                    },
                },
            },
        },
    });
    wait_for_inspect_match(&moniker, &assertion).await
}

/// Verifies that battery-manager applies a governor's votes to the charger driver's `Controller`
/// and replays them when a new `Controller` instance appears.
#[fuchsia::test]
async fn test_governor_votes_reach_charger_driver() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) = setup_realm_with_config(
        FidlRouteMode::NewOnly,
        false,
        Some(fake_charger.clone()),
        Some(governor_config_json(FAKE_GOVERNOR_URL)?),
    )
    .await?;

    // Connecting starts battery-manager, which then launches its configured governors.
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let (watcher_client, mut watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;
    let _ = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::Usb)
    })
    .await?;

    let expected = fcharger::ControlOptions {
        operating_mode: Some(FAKE_GOVERNOR_OPERATING_MODE),
        charge_current_limit_ua: Some(FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA),
        ..Default::default()
    };
    fake_charger.wait_for_controls(&expected).await;

    // Disconnecting resets FakeCharger's recorded controls; once battery-manager observes the
    // disconnection, publishing a new instance verifies that it rebinds `Controller` and replays
    // the active governor votes.
    fake_charger.disconnect();
    let _ = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.charge_source == Some(fpower::ChargeSource::AcAdapter)
    })
    .await?;
    fake_charger.publish_instance("reconnected");
    fake_charger.wait_for_controls(&expected).await;
    Ok(())
}

/// Returns an assertion that the fake telemetry governor's vote reflects `level_percent`.
fn telemetry_vote_assertion(level_percent: f32) -> TreeAssertion {
    let expected_limit = u64::from(charge_current_limit_ua_for_level(level_percent));
    tree_assertion!(root: contains {
        charge_limit_arbiter: {
            elections: contains {
                charge_current_limit_ua: contains {
                    votes: {
                        var FAKE_TELEMETRY_GOVERNOR_NAME: expected_limit,
                    },
                },
            },
        },
    })
}

/// Verifies that a governor watching `fuchsia.hardware.power.battery.Battery` through
/// battery-manager receives the fake battery's telemetry: first the current status, then a pushed
/// update. The governor votes a value derived from each status, observed in Inspect.
#[fuchsia::test]
async fn test_governor_receives_battery_telemetry() -> Result<()> {
    let (realm, _lease_events) = setup_realm_with_config(
        FidlRouteMode::NewOnly,
        false,
        None,
        Some(governor_config_json(FAKE_TELEMETRY_GOVERNOR_URL)?),
    )
    .await?;

    // Connecting starts battery-manager, which then launches its configured governors.
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let _ = battery_mgr.get_battery_info().await?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let control = service.watch_for_any().await?.connect_to_control()?;

    let moniker = battery_manager_moniker(&realm);
    wait_for_inspect_match(
        &moniker,
        &telemetry_vote_assertion(ftest_battery::DEFAULT_LEVEL_PERCENT),
    )
    .await?;

    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(TELEMETRY_UPDATED_LEVEL_PERCENT),
            ..Default::default()
        })
        .await?;
    wait_for_inspect_match(&moniker, &telemetry_vote_assertion(TELEMETRY_UPDATED_LEVEL_PERCENT))
        .await
}

/// Returns an assertion that the fake telemetry governor's vote reflects `charge_phase`.
fn charge_phase_vote_assertion(charge_phase: fcharger::ChargePhase) -> TreeAssertion {
    let expected_current = u64::from(charge_term_current_ua_for_phase(charge_phase));
    tree_assertion!(root: contains {
        charge_limit_arbiter: {
            elections: contains {
                charge_term_current_ua: contains {
                    votes: {
                        var FAKE_TELEMETRY_GOVERNOR_NAME: expected_current,
                    },
                },
            },
        },
    })
}

/// Verifies that a governor watching `fuchsia.hardware.power.charger.Charger` through
/// battery-manager receives the fake charger's status: first the current status, then a pushed
/// charge phase change. The governor votes a value derived from each, observed in Inspect.
#[fuchsia::test]
async fn test_governor_receives_charger_status() -> Result<()> {
    let fake_charger = FakeCharger::default_usb_charging();
    let (realm, _lease_events) = setup_realm_with_config(
        FidlRouteMode::NewOnly,
        false,
        Some(fake_charger.clone()),
        Some(governor_config_json(FAKE_TELEMETRY_GOVERNOR_URL)?),
    )
    .await?;

    // Connecting starts battery-manager, which then launches its configured governors.
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let _ = battery_mgr.get_battery_info().await?;

    let moniker = battery_manager_moniker(&realm);
    wait_for_inspect_match(&moniker, &charge_phase_vote_assertion(fcharger::ChargePhase::Fast))
        .await?;

    fake_charger.set_status(fcharger::Status {
        online: Some(true),
        source_type: Some(fcharger::SourceType::Usb),
        operating_mode: Some(fcharger::OperatingMode::Charging),
        charge_phase: Some(fcharger::ChargePhase::Taper),
        ..Default::default()
    });
    wait_for_inspect_match(&moniker, &charge_phase_vote_assertion(fcharger::ChargePhase::Taper))
        .await
}

/// Verifies that a governor that configures `max_interval` on its `Battery` connection votes for
/// that sample interval, and that its `Watch` keeps completing with periodic samples while the
/// battery status does not change.
#[fuchsia::test]
async fn test_governor_receives_periodic_samples() -> Result<()> {
    let (realm, _lease_events) = setup_realm_with_config(
        FidlRouteMode::NewOnly,
        false,
        None,
        Some(governor_config_json(FAKE_TELEMETRY_GOVERNOR_URL)?),
    )
    .await?;

    // Connecting starts battery-manager, which then launches its configured governors.
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let _ = battery_mgr.get_battery_info().await?;

    let moniker = battery_manager_moniker(&realm);
    // Battery-manager wakes with timers on the fake clock here, as wake alarms are not routed to
    // it; once the governor has received `SAMPLED_RESPONSES` responses (which only periodic
    // samples provide while the battery status does not change), its `input_current_limit_ua` vote
    // saturates at `SAMPLED_RESPONSES`.
    let expected_limit = u64::from(input_current_limit_ua_for_responses(SAMPLED_RESPONSES));
    let sampled = tree_assertion!(root: contains {
        governor_telemetry: contains {
            battery_sample_interval: {
                result: MAX_INTERVAL_MILLIS,
                overridden: false,
                votes: {
                    var FAKE_TELEMETRY_GOVERNOR_NAME: MAX_INTERVAL_MILLIS,
                },
            },
        },
        charge_limit_arbiter: {
            elections: contains {
                input_current_limit_ua: {
                    result: expected_limit,
                    overridden: false,
                    votes: {
                        var FAKE_TELEMETRY_GOVERNOR_NAME: expected_limit,
                    },
                },
            },
        },
    });
    wait_for_inspect_match(&moniker, &sampled).await
}
