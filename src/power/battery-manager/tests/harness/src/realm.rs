// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test realm that runs battery-manager with a fake battery driver, an optional fake charger, a
//! fake System Activity Governor, and fake time.

use crate::fake_charger::{FakeCharger, run_fake_charger};
use crate::fake_sag::{LeaseEvent, run_fake_sag};
use anyhow::Result;
use fidl::endpoints::ServiceMarker as _;
use fidl_fuchsia_component_test as ftest;
use fidl_fuchsia_driver_test as fdt;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use fidl_fuchsia_power_system as fsystem;
use fidl_test_hardwarepowercontrol as ftest_battery;
use fuchsia_component_test::{
    Capability, ChildOptions, DEFAULT_COLLECTION_NAME, DirectoryContents, RealmBuilder,
    RealmInstance, Ref, Route,
};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::channel::mpsc;
use futures::future::FutureExt as _;

/// Child name of battery-manager in the test realm.
const BATTERY_MANAGER_CHILD_NAME: &str = "battery_manager";
const BATTERY_MANAGER_URL: &str = "#meta/battery_manager_fake_time.cm";
const FAKE_CLOCK_URL: &str = "#meta/fake_clock.cm";
const PLATFORM_BUS_URL: &str = "fuchsia-boot:///platform-bus#meta/platform-bus.cm";

/// Directory capability battery-manager reads `CONFIG_FILE_NAME` from (as `/config`).
const CONFIG_DIRECTORY: &str = "config";
const CONFIG_FILE_NAME: &str = "config.json";
const SUSPEND_ENABLED_CONFIG: &str = "fuchsia.power.SuspendEnabled";

/// Lease events the fake System Activity Governor buffers before it waits for the test to read
/// them.
const LEASE_EVENT_BUFFER: usize = 10;

/// Dictates which battery FIDL protocols are routed from the DriverTestRealm
/// to the battery manager under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FidlRouteMode {
    /// Route only the new `fuchsia.hardware.power.battery` protocols.
    NewOnly,
    /// Route only the old `fuchsia.power.battery` protocols.
    OldOnly,
    /// Route both old and new battery protocols.
    Both,
}

impl FidlRouteMode {
    fn routes_new(self) -> bool {
        matches!(self, Self::NewOnly | Self::Both)
    }

    fn routes_old(self) -> bool {
        matches!(self, Self::OldOnly | Self::Both)
    }
}

/// Builds the test realm without a fake charger and with the default config.
pub async fn setup_realm(
    mode: FidlRouteMode,
    suspend_enabled: bool,
) -> Result<(RealmInstance, mpsc::Receiver<LeaseEvent>)> {
    setup_realm_with_config(mode, suspend_enabled, None, None).await
}

/// Builds the test realm with an optional `fake_charger` and the default config.
pub async fn setup_realm_with_charger(
    mode: FidlRouteMode,
    suspend_enabled: bool,
    fake_charger: Option<FakeCharger>,
) -> Result<(RealmInstance, mpsc::Receiver<LeaseEvent>)> {
    setup_realm_with_config(mode, suspend_enabled, fake_charger, None).await
}

/// Builds the test realm. If `config_json` is set, it is served to battery-manager as
/// `/config/config.json`, overriding the package's `test_config.json`.
pub async fn setup_realm_with_config(
    mode: FidlRouteMode,
    suspend_enabled: bool,
    fake_charger: Option<FakeCharger>,
    config_json: Option<String>,
) -> Result<(RealmInstance, mpsc::Receiver<LeaseEvent>)> {
    let builder = RealmBuilder::new().await?;
    builder.driver_test_realm_setup().await?;

    let (lease_events, lease_event_receiver) = mpsc::channel(LEASE_EVENT_BUFFER);
    let fake_sag = builder
        .add_local_child(
            "fake_sag",
            move |handles| run_fake_sag(handles, lease_events.clone()).boxed(),
            ChildOptions::new(),
        )
        .await?;
    let battery_manager = builder
        .add_child(BATTERY_MANAGER_CHILD_NAME, BATTERY_MANAGER_URL, ChildOptions::new())
        .await?;
    let fake_clock = builder.add_child("fake_clock", FAKE_CLOCK_URL, ChildOptions::new()).await?;

    let dtr_exposes = driver_test_realm_exposes(mode);
    builder.driver_test_realm_add_dtr_exposes(&dtr_exposes).await?;

    if let Some(config_json) = config_json {
        builder
            .read_only_directory(
                CONFIG_DIRECTORY,
                vec![&battery_manager],
                DirectoryContents::new().add_file(CONFIG_FILE_NAME, config_json),
            )
            .await?;
    }

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&battery_manager)
                .to(&fake_clock)
                .to(&fake_sag),
        )
        .await?;

    if let Some(charger) = fake_charger {
        let fake_charger = builder
            .add_local_child(
                "fake_charger",
                move |handles| run_fake_charger(handles, charger.clone()).boxed(),
                ChildOptions::new(),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                    .from(Ref::parent())
                    .to(&fake_charger),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<fcharger::ServiceMarker>())
                    .from(&fake_charger)
                    .to(&battery_manager),
            )
            .await?;
    }

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.testing.FakeClock"))
                .from(&fake_clock)
                .to(&battery_manager),
        )
        .await?;
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.testing.FakeClockControl"))
                .from(&fake_clock)
                .to(Ref::parent()),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::storage("data"))
                .capability(Capability::storage("tmp"))
                .from(Ref::parent())
                .to(&battery_manager),
        )
        .await?;

    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: SUSPEND_ENABLED_CONFIG.parse()?,
            value: suspend_enabled.into(),
        }))
        .await?;
    builder
        .add_route(
            Route::new()
                .capability(Capability::configuration(SUSPEND_ENABLED_CONFIG))
                .from(Ref::self_())
                .to(&battery_manager),
        )
        .await?;

    // Battery driver services from the DriverTestRealm to battery-manager, and its test control
    // service to the test.
    let mut battery_services = Route::new();
    if mode.routes_new() {
        battery_services =
            battery_services.capability(Capability::service::<fbattery::ServiceMarker>());
    }
    if mode.routes_old() {
        battery_services =
            battery_services.capability(Capability::service::<fpower::InfoServiceMarker>());
    }
    builder
        .add_route(
            battery_services
                .from(Ref::child(fuchsia_driver_test::COMPONENT_NAME))
                .to(&battery_manager),
        )
        .await?;
    if mode.routes_new() {
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<ftest_battery::ServiceMarker>())
                    .from(Ref::child(fuchsia_driver_test::COMPONENT_NAME))
                    .to(Ref::parent()),
            )
            .await?;
    }

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fpower::BatteryManagerMarker>())
                .capability(Capability::protocol::<spower::BatterySimulatorMarker>())
                .capability(Capability::protocol::<spower::GovernorDebugMarker>())
                .from(&battery_manager)
                .to(Ref::parent()),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fsystem::ActivityGovernorMarker>())
                .from(&fake_sag)
                .to(&battery_manager),
        )
        .await?;

    let realm = builder.build().await?;
    realm
        .driver_test_realm_start(fdt::RealmArgs {
            root_driver: Some(PLATFORM_BUS_URL.to_string()),
            dtr_exposes: Some(dtr_exposes),
            software_devices: Some(vec![fdt::SoftwareDevice {
                device_name: "fake-battery".to_string(),
                device_id: bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_FAKE_BATTERY,
            }]),
            ..Default::default()
        })
        .await?;

    Ok((realm, lease_event_receiver))
}

/// Services the DriverTestRealm exposes for `mode`.
fn driver_test_realm_exposes(mode: FidlRouteMode) -> Vec<ftest::Capability> {
    let service = |name: &str| {
        ftest::Capability::Service(ftest::Service {
            name: Some(name.to_string()),
            ..Default::default()
        })
    };
    let mut exposes = vec![];
    if mode.routes_new() {
        exposes.push(service(fbattery::ServiceMarker::SERVICE_NAME));
        exposes.push(service(ftest_battery::ServiceMarker::SERVICE_NAME));
    }
    if mode.routes_old() {
        exposes.push(service(fpower::InfoServiceMarker::SERVICE_NAME));
    }
    exposes
}

/// Returns the moniker of battery-manager in `realm`, for reading its Inspect data.
pub fn battery_manager_moniker(realm: &RealmInstance) -> String {
    format!("{DEFAULT_COLLECTION_NAME}:{}/{BATTERY_MANAGER_CHILD_NAME}", realm.root.child_name())
}
