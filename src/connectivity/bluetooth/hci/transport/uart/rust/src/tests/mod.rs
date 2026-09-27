// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod fake_serial;

use fdf_component::testing::harness::TestHarness;
use fdf_component::{ServiceInstance, ServiceOffer};
use fidl_fuchsia_driver_framework::{NodeProperty2, NodePropertyValue};
use fidl_next_fuchsia_hardware_serial as serial;
use fidl_next_fuchsia_hardware_serialimpl as serialimpl;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use std::sync::Arc;

use crate::{BtTransportUart, CHILD_NODE_NAME, HCI_SERVICE_NAME};
use fake_serial::{FAKE_SERIAL_PID, FakeSerialService, FakeSerialState};

fn setup_test_harness(state: Arc<Mutex<FakeSerialState>>) -> TestHarness<BtTransportUart> {
    let mut driver_incoming = ServiceFs::new();
    let harness = TestHarness::<BtTransportUart>::new();
    let offer = ServiceOffer::<serialimpl::Service>::new_next()
        .add_default_named_next(
            &mut driver_incoming,
            "default",
            FakeSerialService::new(harness.dispatcher(), state),
        )
        .build_driver_offer();

    harness.add_offer(offer).set_driver_incoming(driver_incoming)
}

#[fuchsia::test]
async fn test_driver_lifecycle_start_and_stop() {
    let state = Arc::new(Mutex::new(FakeSerialState::default()));
    let mut harness = setup_test_harness(state.clone());

    let started_driver = harness.start_driver().await.expect("driver should start successfully");

    {
        let state_guard = state.lock();
        assert!(state_guard.enabled, "device should be enabled");
        assert_eq!(state_guard.cancel_all_count, 0, "cancel_all should not be called yet");
    }

    let driver = started_driver.get_driver().expect("driver instance");
    assert_eq!(driver.serial_pid(), FAKE_SERIAL_PID);

    started_driver.stop_driver().await;

    {
        let state_guard = state.lock();
        assert_eq!(state_guard.cancel_all_count, 1, "stop should trigger cancel_all exactly once");
    }
}

#[fuchsia::test]
async fn test_driver_get_info_error() {
    let state = Arc::new(Mutex::new(FakeSerialState {
        info_result: Err(zx::Status::INTERNAL),
        ..Default::default()
    }));
    let mut harness = setup_test_harness(state);

    assert_eq!(harness.start_driver().await.err(), Some(zx::Status::INTERNAL));
}

#[fuchsia::test]
async fn test_driver_wrong_serial_class() {
    let state = Arc::new(Mutex::new(FakeSerialState {
        info_result: Ok(serial::SerialPortInfo {
            serial_class: serial::Class::Generic,
            serial_vid: 0,
            serial_pid: 0,
        }),
        ..Default::default()
    }));
    let mut harness = setup_test_harness(state);

    assert_eq!(harness.start_driver().await.err(), Some(zx::Status::INTERNAL));
}

#[fuchsia::test]
async fn test_driver_enable_error() {
    let state = Arc::new(Mutex::new(FakeSerialState {
        enable_result: Err(zx::Status::NOT_SUPPORTED),
        ..Default::default()
    }));
    let mut harness = setup_test_harness(state.clone());

    assert_eq!(harness.start_driver().await.err(), Some(zx::Status::NOT_SUPPORTED));
    assert!(!state.lock().enabled, "device should not be marked enabled if enable() failed");
}

#[fuchsia::test]
async fn test_child_nodes_and_properties() {
    let state = Arc::new(Mutex::new(FakeSerialState::default()));
    let mut harness = setup_test_harness(state);

    let started_driver = harness.start_driver().await.expect("driver should start successfully");
    let children = started_driver.node().children();
    assert_eq!(children.len(), 1, "driver should publish one child node");

    let transport_child = children.get(CHILD_NODE_NAME).expect("bt-transport-uart child node");
    assert_eq!(
        transport_child.properties(),
        vec![NodeProperty2 {
            key: bind_fuchsia::SERVICE.to_string(),
            value: NodePropertyValue::StringValue(HCI_SERVICE_NAME.to_string()),
        }]
    );

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_outgoing_serialimpl_service() {
    let state = Arc::new(Mutex::new(FakeSerialState::default()));
    let mut harness = setup_test_harness(state.clone());

    let started_driver = harness.start_driver().await.expect("driver should start successfully");

    let service_proxy: ServiceInstance<serialimpl::Service> = started_driver
        .driver_outgoing()
        .service()
        .connect_next()
        .expect("connect to outgoing serialimpl::Service");

    let (client_chan, server_chan) = fdf::Channel::create();
    let client_end = fidl_next::ClientEnd::<serialimpl::Device, _>::from_untyped(
        fdf_fidl::DriverChannel::new_with_dispatcher(
            started_driver.harness().dispatcher(),
            client_chan,
        ),
    );
    let server_end = fidl_next::ServerEnd::<serialimpl::Device, _>::from_untyped(
        fdf_fidl::DriverChannel::new(server_chan),
    );
    service_proxy.device(server_end).expect("connect to serialimpl device member");
    let (client, client_task) = client_end.spawn_full();

    // 1. Verify get_info returns cached PID and BluetoothHci class.
    let info_resp = client.get_info().await.expect("get_info FIDL call");
    let info = info_resp.as_ref().expect("get_info should succeed").info;
    assert_eq!(info.serial_class, serial::Class::BluetoothHci);
    assert_eq!(info.serial_pid, FAKE_SERIAL_PID);

    // 2. Verify config delegates to parent serial device.
    const TEST_BAUD_RATE: u32 = 115_200;
    let config_resp = client
        .config(TEST_BAUD_RATE, serialimpl::SERIAL_SET_BAUD_RATE_ONLY)
        .await
        .expect("config FIDL call");
    assert!(config_resp.as_ref().is_ok());
    assert_eq!(
        state.lock().last_config,
        Some((TEST_BAUD_RATE, serialimpl::SERIAL_SET_BAUD_RATE_ONLY))
    );

    // Verify config error propagation from parent.
    state.lock().config_result = Err(zx::Status::INVALID_ARGS);
    let config_err_resp = client.config(9600, 0).await.expect("config FIDL call");
    assert_eq!(config_err_resp.as_ref().err(), Some(&Err(zx::Status::INVALID_ARGS)));

    // 3. Verify enable, read, and write return NOT_SUPPORTED.
    let enable_resp = client.enable(false).await.expect("enable FIDL call");
    assert_eq!(enable_resp.as_ref().err(), Some(&Err(zx::Status::NOT_SUPPORTED)));

    let read_resp = client.read().await.expect("read FIDL call");
    assert_eq!(read_resp.as_ref().err(), Some(&Err(zx::Status::NOT_SUPPORTED)));

    let write_resp = client.write([0x01, 0x02]).await.expect("write FIDL call");
    assert_eq!(write_resp.as_ref().err(), Some(&Err(zx::Status::NOT_SUPPORTED)));

    // 4. Verify cancel_all returns Ok(()).
    client.cancel_all().await.expect("cancel_all FIDL call");
    assert_eq!(state.lock().cancel_all_count, 0, "proxy cancel_all does not cancel parent");

    drop(client);
    let _ = client_task.await;
    started_driver.stop_driver().await;
}
