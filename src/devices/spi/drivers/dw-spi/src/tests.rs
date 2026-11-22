// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::DwSpiDriver;
use super::spi_device::{
    DIAG_LAST_POWER_UP_NS, DIAG_MAX_POWER_UP_NS, DIAG_POWER_DOWN_COUNT, DIAG_POWER_DOWN_FAILURES,
    DIAG_POWER_UP_COUNT, DIAG_POWER_UP_FAILURES, DIAG_POWER_UP_WAITS, DIAG_POWERED, DIAG_REASON,
    DIAG_TOTAL_POWER_UP_NS, DwSpiDevice, INTERRUPT_TIMEOUT, REASON_POWER_RELEASE_FAILED,
    REASON_POWER_UP_FAILED, registers,
};
use fake_clock::FakeClock;
use fake_gpio::FakeGpio;
use fake_pdev::FakePDev;
use fake_pin::FakePinStates;
use fake_powerdomain::FakePowerDomain;
use fake_reset::FakeReset;
use fdf_component::testing::harness::{DriverUnderTest, TestHarness};
use fdf_component::{
    Driver, HealthAlert, HealthCheckError, HealthCheckRequest, HealthCheckResponse,
    HealthCondition, HealthDiagnosticItem, HealthDiagnosticValue, HealthStatus, ServiceInstance,
};
use fdf_fidl;
use fidl::Serializable;
use fidl_fuchsia_driver_framework as fdf;
use fidl_fuchsia_driver_metadata as fmetadata;
use fidl_fuchsia_hardware_spi_businfo as fspi_businfo;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_clock::{self as fclock, ClockServerHandler};
use fidl_next_fuchsia_hardware_gpio as fgpio;
use fidl_next_fuchsia_hardware_platform_device as fdevice;
use fidl_next_fuchsia_hardware_powerdomain::{self as fpowerdomain, DomainServerHandler};
use fidl_next_fuchsia_hardware_sharedmemory as fsharedmemory;
use fidl_next_fuchsia_hardware_sharedmemory::natural::SharedVmoRight;
use fidl_next_fuchsia_hardware_spiimpl as fspiimpl;
use fidl_next_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_async::TimeoutExt;
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use fuchsia_inspect::hierarchy::Property as InspectProperty;
use fuchsia_sync::Mutex;
use futures::future::{self, Either};
use mmio::Register;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use zx::Vmo;

#[fuchsia::test]
async fn test_init() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let entries = vec![fmetadata::DictionaryEntry {
        key: "dw_spi_rx_sample_delay_ns".to_string(),
        value: fmetadata::DictionaryValue::Int64(25),
    }];
    let dict = fmetadata::Dictionary { entries: Some(entries), ..Default::default() };
    let serialized_config = fidl::persist(&dict).expect("Failed to serialize config");
    pdev.add_metadata("fuchsia.driver.metadata.Dictionary", serialized_config);

    let metadata = fspi_businfo::SpiBusMetadata {
        channels: Some(vec![fspi_businfo::SpiChannel {
            cs: Some(0),
            max_frequency_hz: Some(20_000_000),
            ..Default::default()
        }]),
        bus_id: Some(0),
        ..Default::default()
    };
    let serialized_metadata = fidl::persist(&metadata).expect("Failed to serialize metadata");
    pdev.add_metadata(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME, serialized_metadata);

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.use_fake_irq = true;
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();

    let clock_bus = FakeClock::new();
    clock_bus.set_rate(200_000_000);
    let clock_regs = FakeClock::new();

    let mut reset = FakeReset::new();

    let pin_states_0 = FakePinStates::new();
    let pin_states_1 = FakePinStates::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(pin_states_0.serve(&mut service_fs, scope.to_handle(), "pin-states-0"))
        .add_offer(pin_states_1.serve(&mut service_fs, scope.to_handle(), "pin-states-1"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    assert!(powerdomain.enabled());
    assert!(clock_bus.enabled());
    assert!(clock_regs.enabled());
    assert!(reset.take_toggled());
    assert_eq!(pin_states_0.current_state(), "default");
    assert_eq!(pin_states_1.current_state(), "default");

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    let ctrlr0 = read_u32(0x0);
    assert_eq!(ctrlr0, 7);

    let ssi_enr = read_u32(0x8);
    assert_eq!(ssi_enr, 1);

    let baudr = read_u32(0x14);
    assert_eq!(baudr, 10);

    let rx_sample_dly = read_u32(0xf0);
    assert_eq!(rx_sample_dly, 5);

    // Stopping the driver releases every vote.
    started_driver.stop_driver().await;
    assert!(!powerdomain.enabled());
    assert!(!clock_bus.enabled());
    assert!(!clock_regs.enabled());
}

#[fuchsia::test]
async fn test_dictionary_spi_bus_metadata() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let spi_dict = fmetadata::Dictionary {
        entries: Some(vec![
            fmetadata::DictionaryEntry {
                key: "provider".to_string(),
                value: fmetadata::DictionaryValue::Int64(2),
            },
            fmetadata::DictionaryEntry {
                key: "channels._count".to_string(),
                value: fmetadata::DictionaryValue::Int64(2),
            },
            fmetadata::DictionaryEntry {
                key: "channels.0.cs".to_string(),
                value: fmetadata::DictionaryValue::Int64(0),
            },
            fmetadata::DictionaryEntry {
                key: "channels.0.id".to_string(),
                value: fmetadata::DictionaryValue::Int64(10),
            },
            fmetadata::DictionaryEntry {
                key: "channels.0.max_frequency_hz".to_string(),
                value: fmetadata::DictionaryValue::Int64(40_000_000),
            },
            fmetadata::DictionaryEntry {
                key: "channels.1.cs".to_string(),
                value: fmetadata::DictionaryValue::Int64(1),
            },
            fmetadata::DictionaryEntry {
                key: "channels.1.id".to_string(),
                value: fmetadata::DictionaryValue::Int64(11),
            },
            fmetadata::DictionaryEntry {
                key: "channels.1.max_frequency_hz".to_string(),
                value: fmetadata::DictionaryValue::Int64(20_000_000),
            },
        ]),
        ..Default::default()
    };
    let serialized_metadata = fidl::persist(&spi_dict).expect("Failed to serialize metadata");
    pdev.add_metadata(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME, serialized_metadata);

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.use_fake_irq = true;
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    clock_bus.set_rate(200_000_000);
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    // Min max_frequency_hz across channels is 20 MHz, bus clock is 200 MHz -> baudr = 10.
    let baudr = read_u32(0x14);
    assert_eq!(baudr, 10);

    // Verify the Dictionary-encoded SpiBusMetadata was forwarded intact to the child node.
    let metadata_proxy = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
        fmetadata::MetadataMarker,
    >(
        started_driver.driver_outgoing(),
        &format!("svc/{}/default/metadata", fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME),
    )
    .expect("Failed to connect to forwarded SPI bus metadata");
    let persisted = metadata_proxy
        .get_persisted_metadata()
        .await
        .expect("FIDL call failed")
        .expect("GetPersistedMetadata failed");
    let forwarded_dict: fmetadata::Dictionary =
        fidl::unpersist(&persisted).expect("Failed to unpersist forwarded dictionary");
    assert_eq!(forwarded_dict, spi_dict);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_exchange_vector() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();

    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();

    let reset = FakeReset::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    // Set TXFLR and ISR.TXEIS to indicate TX FIFO empty, then trigger the interrupt.
    write_u32(0x20, 0x0);
    write_u32(0x30, 0x01);

    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    // Call exchange_vector(), but don't await on it yet.
    let txdata = vec![1u8, 2, 3, 4, 5];
    let exchange_future = client.exchange_vector(0, &txdata);
    let exchange_future = std::pin::pin!(exchange_future);

    // The driver should handle the interrupt then ack it. Wait for this to happen before starting
    // the RX portion of the transfer.
    let untriggered_future =
        fasync::OnSignals::new(&irq_dup, zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED);
    let untriggered_future = std::pin::pin!(untriggered_future);

    // The actual exchange_vector() FIDL call is made the first time its future is polled, so the
    // two futures must be selected here. The untriggered future will always complete first.
    let exchange_future = match future::select(exchange_future, untriggered_future).await {
        Either::Left((res, _)) => {
            panic!("exchange_vector completed prematurely: {:?}", res);
        }
        Either::Right((res, remaining_exchange_future)) => {
            res.expect("Failed to wait for untriggered signal");
            remaining_exchange_future
        }
    };

    // Verify register values after the first interrupt (TX empty) is handled:
    // - TXFTLR: Remains at the default 128.
    // - RXFTLR: Set to 4 (calculated as rx_remaining (5) - 1).
    // - IMR: Only RXFIFO interrupt remains unmasked (and error interrupts). TX FIFO empty interrupt
    //        is masked since TX is done.
    assert_eq!(read_u32(0x18), 128);
    assert_eq!(read_u32(0x1c), 4);
    assert_eq!(read_u32(0x2c), 0x1e);

    // Set RXFLR and ISR.RXFIS to indicate that 5 bytes can be read from the RX FIFO, then trigger
    // the interrupt again.
    write_u32(0x24, 0x5);
    write_u32(0x30, 0x10);

    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    // The driver should be able to complete the request now.
    let response = exchange_future.await;
    assert!(response.is_ok());

    // Verify register values after the transfer is complete:
    // - TXFTLR: Remains at the default 128.
    // - RXFTLR: Set to 0 (calculated as rx_remaining.max(1) - 1, which is 1 - 1 = 0 when
    //           rx_remaining is 0).
    // - IMR: All interrupts are masked.
    assert_eq!(read_u32(0x18), 128);
    assert_eq!(read_u32(0x1c), 0);
    assert_eq!(read_u32(0x2c), 0);

    let rxdata = response.unwrap().unwrap().rxdata;
    assert_eq!(rxdata.len(), txdata.len());

    // Check the last value that was written to the FIFO.
    assert_eq!(read_u32(0x60), 5);
    // The RX path should have read the last value that was written to the FIFO.
    assert_eq!(rxdata, vec![5, 5, 5, 5, 5]);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_vmo_registration() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(vmo) };

    let mut pdev_config: fake_pdev::Config = Default::default();
    pdev_config.mmios.insert(0, mmio);
    pdev_config.use_fake_irq = true;

    let pdev = FakePDev::new();
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    // Create a VMO and register it.
    let vmo = Vmo::create(4096).unwrap();
    let dup_vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let range = fmem::natural::Range { vmo: vmo, offset: 0, size: 4096 };
    client
        .register_vmo(0, 1, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Registering the same VMO ID again should fail.
    let vmo = Vmo::create(4096).unwrap();
    let range = fmem::natural::Range { vmo: vmo, offset: 0, size: 4096 };
    let res =
        client.register_vmo(0, 1, range, SharedVmoRight::READ).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::ALREADY_EXISTS);

    // Unregister the VMO and make sure it's the same one we registered.
    let unreg_vmo = client
        .unregister_vmo(0, 1)
        .await
        .expect("FIDL call failed")
        .expect("Unregister VMO failed")
        .vmo;
    assert_eq!(dup_vmo.koid().unwrap(), unreg_vmo.koid().unwrap());

    // Unregistering again should fail.
    let res = client.unregister_vmo(0, 1).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::NOT_FOUND);

    let vmo = Vmo::create(4096).unwrap();
    let range = fmem::natural::Range { vmo: vmo, offset: 0, size: 4096 };
    client
        .register_vmo(0, 2, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    let vmo = Vmo::create(4096).unwrap();
    let range = fmem::natural::Range { vmo: vmo, offset: 0, size: 4096 };
    client
        .register_vmo(0, 3, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Release VMOs, and check that unregistering fails.
    client.release_registered_vmos(0).await.unwrap();

    let res = client.unregister_vmo(0, 2).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::NOT_FOUND);

    let res = client.unregister_vmo(0, 3).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::NOT_FOUND);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_vmo_registration_loopback() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(vmo) };

    let mut pdev_config: fake_pdev::Config = Default::default();
    pdev_config.mmios.insert(0, mmio);
    pdev_config.use_fake_irq = true;

    let pdev = FakePDev::new();
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    // Create a VMO and register it on loopback.
    let vmo1 = Vmo::create(4096).unwrap();
    let dup_vmo1 = vmo1.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let range = fmem::natural::Range { vmo: vmo1, offset: 0, size: 4096 };
    client
        .register_vmo(fspiimpl::LOOPBACK_CHIP_SELECT, 1, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Registering the same VMO ID on normal CS should succeed (they are separate).
    let vmo2 = Vmo::create(4096).unwrap();
    let dup_vmo2 = vmo2.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let range = fmem::natural::Range { vmo: vmo2, offset: 0, size: 4096 };
    client
        .register_vmo(0, 1, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Unregister loopback VMO and verify it's vmo1.
    let unreg_vmo1 = client
        .unregister_vmo(fspiimpl::LOOPBACK_CHIP_SELECT, 1)
        .await
        .expect("FIDL call failed")
        .expect("Unregister VMO failed")
        .vmo;
    assert_eq!(dup_vmo1.koid().unwrap(), unreg_vmo1.koid().unwrap());

    // Unregister normal CS VMO and verify it's vmo2.
    let unreg_vmo2 = client
        .unregister_vmo(0, 1)
        .await
        .expect("FIDL call failed")
        .expect("Unregister VMO failed")
        .vmo;
    assert_eq!(dup_vmo2.koid().unwrap(), unreg_vmo2.koid().unwrap());

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_transmit_vmo() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    let tx_vmo = Vmo::create(4096).unwrap();
    let txdata = vec![1u8, 2, 3, 4, 5];
    tx_vmo.write(&txdata, 0).unwrap();
    let range = fmem::natural::Range { vmo: tx_vmo, offset: 0, size: 4096 };
    client
        .register_vmo(0, 1, range, SharedVmoRight::READ)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Set TXFLR and ISR.TXEIS to indicate TX FIFO empty, then trigger the interrupt.
    write_u32(0x20, 0x0);
    write_u32(0x30, 0x01);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 1, offset: 0, size: 5 };
    let transmit_future = client.transmit_vmo(0, buffer);
    let transmit_future = std::pin::pin!(transmit_future);

    let untriggered_future =
        fasync::OnSignals::new(&irq_dup, zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED);
    let untriggered_future = std::pin::pin!(untriggered_future);

    let transmit_future = match future::select(transmit_future, untriggered_future).await {
        Either::Left((res, _)) => {
            panic!("transmit_vmo completed prematurely: {:?}", res);
        }
        Either::Right((res, remaining_future)) => {
            res.expect("Failed to wait for untriggered signal");
            remaining_future
        }
    };

    // Set RXFLR and ISR.RXFIS to indicate that 5 bytes can be read from the RX FIFO, then trigger
    // the interrupt again.
    write_u32(0x24, 0x5);
    write_u32(0x30, 0x10);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let response = transmit_future.await;
    assert!(response.is_ok());
    assert!(response.unwrap().is_ok());

    // Verify what was written to the TX FIFO.
    assert_eq!(read_u32(0x60), 5);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_receive_vmo() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    let rx_vmo = Vmo::create(4096).unwrap();
    let dup_rx_vmo = rx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let range = fmem::natural::Range { vmo: rx_vmo, offset: 0, size: 4096 };
    client
        .register_vmo(0, 1, range, SharedVmoRight::WRITE)
        .await
        .expect("FIDL call failed")
        .expect("Register VMO failed");

    // Set TXFLR and ISR.TXEIS to indicate TX FIFO empty, then trigger the interrupt.
    write_u32(0x20, 0x0);
    write_u32(0x30, 0x01);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 1, offset: 0, size: 5 };
    let receive_future = client.receive_vmo(0, buffer);
    let receive_future = std::pin::pin!(receive_future);

    let untriggered_future =
        fasync::OnSignals::new(&irq_dup, zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED);
    let untriggered_future = std::pin::pin!(untriggered_future);

    let receive_future = match future::select(receive_future, untriggered_future).await {
        Either::Left((res, _)) => {
            panic!("receive_vmo completed prematurely: {:?}", res);
        }
        Either::Right((res, remaining_future)) => {
            res.expect("Failed to wait for untriggered signal");
            remaining_future
        }
    };

    // Set RXFLR and ISR.RXFIS to indicate that 5 bytes can be read from the RX FIFO, then trigger
    // the interrupt again.
    write_u32(0x24, 0x5);
    write_u32(0x30, 0x10);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let response = receive_future.await;
    assert!(response.is_ok());
    assert!(response.unwrap().is_ok());

    // Verify what was written to the VMO (0xFF because txdata was empty).
    let mut rx_data = vec![0u8; 5];
    dup_rx_vmo.read(&mut rx_data, 0).unwrap();
    assert_eq!(rx_data, vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_exchange_vmo() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    // Create TX VMO and write [1, 2, 3, 4, 5].
    let tx_vmo = Vmo::create(4096).unwrap();
    tx_vmo.write(&[1u8, 2, 3, 4, 5], 0).unwrap();
    let tx_range = fmem::natural::Range { vmo: tx_vmo, offset: 0, size: 4096 };
    client.register_vmo(0, 1, tx_range, SharedVmoRight::READ).await.unwrap().unwrap();

    // Create RX VMO.
    let rx_vmo = Vmo::create(4096).unwrap();
    let dup_rx_vmo = rx_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let rx_range = fmem::natural::Range { vmo: rx_vmo, offset: 0, size: 4096 };
    client.register_vmo(0, 2, rx_range, SharedVmoRight::WRITE).await.unwrap().unwrap();

    // Set TXFLR and ISR.TXEIS to indicate TX FIFO empty, then trigger the interrupt.
    write_u32(0x20, 0x0);
    write_u32(0x30, 0x01);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let tx_buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 1, offset: 0, size: 5 };
    let rx_buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 2, offset: 0, size: 5 };
    let exchange_future = client.exchange_vmo(0, tx_buffer, rx_buffer);
    let exchange_future = std::pin::pin!(exchange_future);

    let untriggered_future =
        fasync::OnSignals::new(&irq_dup, zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED);
    let untriggered_future = std::pin::pin!(untriggered_future);

    let exchange_future = match future::select(exchange_future, untriggered_future).await {
        Either::Left((res, _)) => {
            panic!("exchange_vmo completed prematurely: {:?}", res);
        }
        Either::Right((res, remaining_future)) => {
            res.expect("Failed to wait for untriggered signal");
            remaining_future
        }
    };

    // Set RXFLR and ISR.RXFIS to indicate that 5 bytes can be read from the RX FIFO, then trigger
    // the interrupt again.
    write_u32(0x24, 0x5);
    write_u32(0x30, 0x10);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let response = exchange_future.await;
    assert!(response.is_ok());
    assert!(response.unwrap().is_ok());

    // Check RX VMO content. Last written byte was 5.
    let mut rx_data = vec![0u8; 5];
    dup_rx_vmo.read(&mut rx_data, 0).unwrap();
    assert_eq!(rx_data, vec![5, 5, 5, 5, 5]);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_vmo_validation() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    let vmo = Vmo::create(4096).unwrap();
    let range = fmem::natural::Range { vmo: vmo, offset: 0, size: 1024 }; // only register 1024 bytes

    client.register_vmo(0, 1, range, SharedVmoRight::READ).await.unwrap().unwrap();

    // Invalid VMO ID
    let buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 99, offset: 0, size: 5 };
    let res = client.transmit_vmo(0, buffer).await.unwrap();
    assert_eq!(res.unwrap_err(), zx::Status::NOT_FOUND);

    // Out of range (offset + size > registered size)
    let buffer = fsharedmemory::natural::SharedVmoBuffer { vmo_id: 1, offset: 1020, size: 10 };
    let res = client.transmit_vmo(0, buffer).await.unwrap();
    assert_eq!(res.unwrap_err(), zx::Status::OUT_OF_RANGE);

    // Out of range (overflow)
    let buffer =
        fsharedmemory::natural::SharedVmoBuffer { vmo_id: 1, offset: u64::MAX - 2, size: 10 };
    let res = client.transmit_vmo(0, buffer).await.unwrap();
    assert_eq!(res.unwrap_err(), zx::Status::OUT_OF_RANGE);

    started_driver.stop_driver().await;
}

#[fuchsia::test]
async fn test_loopback_mode() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test");

    let pdev = FakePDev::new();

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    let clock_regs = FakeClock::new();
    let reset = FakeReset::new();

    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");

    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();

    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    // 1. Normal transfer (CS 0)
    perform_transfer(&client, &irq_dup, &gpio, &mapping, 0).await;

    // 2. Loopback transfer (CS LOOPBACK_CHIP_SELECT)
    perform_transfer(&client, &irq_dup, &gpio, &mapping, fspiimpl::LOOPBACK_CHIP_SELECT).await;

    // 3. Normal transfer (CS 0)
    perform_transfer(&client, &irq_dup, &gpio, &mapping, 0).await;

    started_driver.stop_driver().await;
}

async fn perform_transfer(
    client: &fidl_next::Client<fspiimpl::SpiImpl, fdf_fidl::DriverChannel>,
    irq_dup: &zx::VirtualInterrupt,
    gpio: &FakeGpio,
    mapping: &mapped_vmo::Mapping,
    cs: u32,
) {
    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };
    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };

    write_u32(0x24, 0x0);
    write_u32(0x20, 0x0);
    write_u32(0x30, 0x01);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let exchange_future = client.exchange_vector(cs, vec![1u8, 2, 3, 4, 5]);
    let exchange_future = std::pin::pin!(exchange_future);

    let untriggered_future =
        fasync::OnSignals::new(irq_dup, zx::Signals::VIRTUAL_INTERRUPT_UNTRIGGERED);
    let untriggered_future = std::pin::pin!(untriggered_future);

    let exchange_future = match future::select(exchange_future, untriggered_future).await {
        Either::Left((res, _)) => {
            panic!("exchange_vector completed prematurely: {:?}", res);
        }
        Either::Right((res, remaining_exchange_future)) => {
            res.expect("Failed to wait for untriggered signal");
            remaining_exchange_future
        }
    };

    // During transfer:
    // - SRL should be 1 if loopback, 0 otherwise
    // - CS should be High (deasserted) if loopback, Low (asserted) otherwise
    let ctrlr0 = read_u32(0x0);
    let expected_srl = cs == fspiimpl::LOOPBACK_CHIP_SELECT;
    assert_eq!(registers::CtrlR0::from_raw(ctrlr0).srl(), expected_srl);

    let expected_buffer_mode = if cs == fspiimpl::LOOPBACK_CHIP_SELECT {
        fgpio::BufferMode::OutputHigh
    } else {
        fgpio::BufferMode::OutputLow
    };
    assert_eq!(gpio.buffer_mode(), expected_buffer_mode);

    write_u32(0x24, 0x5);
    write_u32(0x30, 0x10);
    irq_dup.trigger(zx::BootInstant::from_nanos(1)).expect("Failed to trigger interrupt");

    let _ = exchange_future.await.expect("exchange_vector failed");

    // After transfer:
    // - SRL should be the same as before
    // - CS should be High (deasserted)
    let ctrlr0 = read_u32(0x0);
    assert_eq!(registers::CtrlR0::from_raw(ctrlr0).srl(), expected_srl);
    assert_eq!(gpio.buffer_mode(), fgpio::BufferMode::OutputHigh);
}

#[fuchsia::test]
async fn test_suspend_and_resume() {
    let mut service_fs = ServiceFs::new();
    let scope = fasync::Scope::new_with_name("test_suspend_resume");

    let pdev = FakePDev::new();

    let entries = vec![fmetadata::DictionaryEntry {
        key: "dw_spi_rx_sample_delay_ns".to_string(),
        value: fmetadata::DictionaryValue::Int64(25),
    }];
    let dict = fmetadata::Dictionary { entries: Some(entries), ..Default::default() };
    let serialized_config = fidl::persist(&dict).expect("Failed to serialize config");
    pdev.add_metadata("fuchsia.driver.metadata.Dictionary", serialized_config);

    let metadata = fspi_businfo::SpiBusMetadata {
        channels: Some(vec![fspi_businfo::SpiChannel {
            cs: Some(0),
            max_frequency_hz: Some(20_000_000),
            ..Default::default()
        }]),
        bus_id: Some(0),
        ..Default::default()
    };
    let serialized_metadata = fidl::persist(&metadata).expect("Failed to serialize metadata");
    pdev.add_metadata(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME, serialized_metadata);

    let vmo = Vmo::create(0x100).expect("Failed to create VMO");
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        0x100,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )
    .expect("Failed to map VMO");

    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
    let irq = zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
    let irq_dup =
        irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

    let mut pdev_config: fake_pdev::Config = Default::default();
    let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
    pdev_config.mmios.insert(0, mmio);
    pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
    pdev.set_config(pdev_config);

    let powerdomain = FakePowerDomain::new();
    let clock_bus = FakeClock::new();
    clock_bus.set_rate(200_000_000);
    let clock_regs = FakeClock::new();
    let mut reset = FakeReset::new();
    let pin_states = FakePinStates::new();
    let gpio = FakeGpio::default();

    let mut harness = TestHarness::<DwSpiDriver>::new()
        .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
        .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
        .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
        .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
        .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
        .add_offer(pin_states.serve(&mut service_fs, scope.to_handle(), "pin-states-0"))
        .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"))
        .set_driver_incoming(service_fs);

    let dispatcher = fdf_fidl::FidlExecutor::from(harness.dispatcher().clone());
    let started_driver = harness.start_driver().await.expect("Failed to start driver");
    let driver = started_driver.get_driver().expect("Failed to get driver instance");

    let read_u32 = |offset: usize| -> u32 {
        let mut bytes = [0u8; 4];
        mapping.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    };
    let write_u32 = |offset: usize, value: u32| {
        mapping.write_at(offset, &value.to_le_bytes());
    };

    // Verify startup resource state and registers.
    assert!(powerdomain.enabled());
    assert!(clock_bus.enabled());
    assert!(clock_regs.enabled());
    assert!(reset.take_toggled());
    assert!(!reset.asserted());
    assert_eq!(pin_states.current_state(), "default");
    assert_eq!(read_u32(0x00), 0x7); // CTRLR0
    assert_eq!(read_u32(0x14), 0xa); // BAUDR
    assert_eq!(read_u32(0xf0), 0x5); // RX_SAMPLE_DLY
    assert_eq!(read_u32(0x2c), 0x0); // IMR
    assert_eq!(read_u32(0x18), 0x80); // TXFTLR
    assert_eq!(read_u32(0x08), 0x1); // SSIENR

    // Trigger system_suspend() and verify resource states and register values.
    driver.system_suspend().await.expect("system_suspend failed");

    assert!(!powerdomain.enabled());
    assert!(!clock_bus.enabled());
    assert!(!clock_regs.enabled());
    assert!(!reset.take_toggled());
    assert!(!reset.asserted());
    assert_eq!(pin_states.current_state(), "sleep");
    assert_eq!(read_u32(0x08), 0x0); // SSIENR disabled
    assert_eq!(read_u32(0x14), 0x0); // BAUDR cleared

    // Simulate volatile register state loss when power domain is turned off.
    write_u32(0x00, 0x0); // CTRLR0
    write_u32(0x14, 0x0); // BAUDR
    write_u32(0x18, 0x0); // TXFTLR
    write_u32(0xf0, 0x0); // RX_SAMPLE_DLY
    write_u32(0x08, 0x0); // SSIENR

    // Trigger system_resume(). Resume is lazy, so nothing is powered until the next transfer.
    driver.system_resume(None).await.expect("system_resume failed");

    assert!(!powerdomain.enabled());
    assert!(!clock_bus.enabled());
    assert!(!clock_regs.enabled());
    assert!(!reset.take_toggled());
    assert_eq!(pin_states.current_state(), "sleep");
    assert_eq!(read_u32(0x00), 0x0); // CTRLR0

    // A transfer powers the controller up and reinitializes the registers.
    let spi_service: fdf_component::ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();
    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    let client = client_end.spawn_on(&dispatcher);

    perform_transfer(&client, &irq_dup, &gpio, &mapping, 0).await;

    assert!(powerdomain.enabled());
    assert!(clock_bus.enabled());
    assert!(clock_regs.enabled());
    assert!(reset.take_toggled());
    assert_eq!(pin_states.current_state(), "default");

    assert_eq!(read_u32(0x00), 0x7); // CTRLR0
    assert_eq!(read_u32(0x14), 0xa); // BAUDR
    assert_eq!(read_u32(0xf0), 0x5); // RX_SAMPLE_DLY
    assert_eq!(read_u32(0x2c), 0x0); // IMR
    assert_eq!(read_u32(0x18), 0x80); // TXFTLR
    assert_eq!(read_u32(0x08), 0x1); // SSIENR

    started_driver.stop_driver().await;
}

/// A log of power domain calls shared between fakes, used to verify cross-resource ordering.
type CallLog = Arc<Mutex<Vec<String>>>;

/// Thin wrapper around [`FakeClock`] that checks the hardware invariant that the clock is never
/// disabled while its power domain is off.
#[derive(Clone, Default)]
struct TestClock {
    inner: FakeClock,
    domain: Arc<Mutex<Option<FakePowerDomain>>>,
    disables_without_domain: Arc<AtomicU32>,
}

impl Deref for TestClock {
    type Target = FakeClock;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl TestClock {
    fn new() -> Self {
        Self::default()
    }

    fn set_domain(&self, domain: &LoggedPowerDomain) {
        *self.domain.lock() = Some(domain.inner.clone());
    }

    fn disables_without_domain(&self) -> u32 {
        self.disables_without_domain.load(Ordering::Relaxed)
    }

    fn serve<O: ServiceObjTrait>(
        &self,
        service_fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        fdf_component::ServiceOffer::<fclock::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                TestClockService { clock: self.clone(), scope },
            )
            .build_zircon_offer_next()
    }
}

struct TestClockService {
    clock: TestClock,
    scope: fasync::ScopeHandle,
}

impl fclock::ServiceHandler for TestClockService {
    fn clock(&self, server_end: fidl_next::ServerEnd<fclock::Clock>) {
        let client = self.clock.inner.connect(&self.scope);
        server_end.spawn_on(TestClockServer { clock: self.clock.clone(), client }, &self.scope);
    }
}

struct TestClockServer {
    clock: TestClock,
    client: fidl_next::Client<fclock::Clock>,
}

impl ClockServerHandler for TestClockServer {
    async fn enable(&mut self, responder: Responder<fclock::clock::Enable>) {
        let res = self.client.enable().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn disable(&mut self, responder: Responder<fclock::clock::Disable>) {
        if self.clock.domain.lock().as_ref().is_some_and(|domain| !domain.enabled()) {
            self.clock.disables_without_domain.fetch_add(1, Ordering::Relaxed);
        }
        let res = self.client.disable().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn is_enabled(&mut self, responder: Responder<fclock::clock::IsEnabled>) {
        let res = self.client.is_enabled().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn set_rate(
        &mut self,
        request: Request<fclock::clock::SetRate>,
        responder: Responder<fclock::clock::SetRate>,
    ) {
        let res = self.client.set_rate(request.payload().hz).await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn query_supported_rate(
        &mut self,
        request: Request<fclock::clock::QuerySupportedRate>,
        responder: Responder<fclock::clock::QuerySupportedRate>,
    ) {
        let res = self.client.query_supported_rate(request.payload().hz_in).await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn get_rate(&mut self, responder: Responder<fclock::clock::GetRate>) {
        let res = self.client.get_rate().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn set_input(
        &mut self,
        request: Request<fclock::clock::SetInput>,
        responder: Responder<fclock::clock::SetInput>,
    ) {
        let res = self.client.set_input(request.payload().idx).await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn get_num_inputs(&mut self, responder: Responder<fclock::clock::GetNumInputs>) {
        let res = self.client.get_num_inputs().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn get_input(&mut self, responder: Responder<fclock::clock::GetInput>) {
        let res = self.client.get_input().await.unwrap();
        let _ = responder.respond_with(res).await;
    }

    async fn get_properties(&mut self, responder: Responder<fclock::clock::GetProperties>) {
        let res = self.client.get_properties().await.unwrap();
        let _ = responder.respond(res).await;
    }
}

/// Thin wrapper around [`FakePowerDomain`] that records successful Enable/Disable calls in a
/// shared cross-resource log, noting whether any clock was enabled at the time.
#[derive(Clone)]
struct LoggedPowerDomain {
    inner: FakePowerDomain,
    name: &'static str,
    log: CallLog,
    clocks: Vec<FakeClock>,
}

impl Deref for LoggedPowerDomain {
    type Target = FakePowerDomain;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl LoggedPowerDomain {
    fn new(name: &'static str, log: CallLog, clocks: Vec<FakeClock>) -> Self {
        Self { inner: FakePowerDomain::new(), name, log, clocks }
    }

    fn record(&self, op: &str) {
        let clocks_on = self.clocks.iter().any(|clock| clock.enabled());
        let suffix = if clocks_on { " (clocks on)" } else { "" };
        self.log.lock().push(format!("{}.{op}{suffix}", self.name));
    }

    fn serve<O: ServiceObjTrait>(
        &self,
        service_fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        fdf_component::ServiceOffer::<fpowerdomain::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                LoggedPowerDomainService { domain: self.clone(), scope },
            )
            .build_zircon_offer_next()
    }
}

struct LoggedPowerDomainService {
    domain: LoggedPowerDomain,
    scope: fasync::ScopeHandle,
}

impl fpowerdomain::ServiceHandler for LoggedPowerDomainService {
    fn domain(&self, server_end: fidl_next::ServerEnd<fpowerdomain::Domain>) {
        let client = self.domain.inner.connect(&self.scope);
        server_end
            .spawn_on(LoggedPowerDomainServer { domain: self.domain.clone(), client }, &self.scope);
    }
}

struct LoggedPowerDomainServer {
    domain: LoggedPowerDomain,
    client: fidl_next::Client<fpowerdomain::Domain>,
}

impl DomainServerHandler for LoggedPowerDomainServer {
    async fn enable(&mut self, responder: Responder<fpowerdomain::domain::Enable>) {
        let res = self.client.enable().await.unwrap();
        if res.is_ok() {
            self.domain.record("enable");
        }
        let _ = responder.respond_with(res).await;
    }

    async fn disable(&mut self, responder: Responder<fpowerdomain::domain::Disable>) {
        let res = self.client.disable().await.unwrap();
        if res.is_ok() {
            self.domain.record("disable");
        }
        let _ = responder.respond_with(res).await;
    }

    async fn is_enabled(&mut self, responder: Responder<fpowerdomain::domain::IsEnabled>) {
        let res = self.client.is_enabled().await.unwrap();
        let _ = responder.respond_with(res).await;
    }
}

/// Whether the driver has an interrupt controller parent offering a power domain vote.
#[derive(Clone, Copy, PartialEq)]
enum InterruptControllerParent {
    Served,
    Absent,
}

struct RuntimePmFakes {
    _scope: fasync::Scope,
    _pdev: FakePDev,
    mapping: mapped_vmo::Mapping,
    irq: zx::VirtualInterrupt,
    gpio: FakeGpio,
    powerdomain: LoggedPowerDomain,
    interrupt_controller: LoggedPowerDomain,
    clock_bus: TestClock,
    clock_regs: TestClock,
    reset: FakeReset,
    // "pin-states-0" has no sleep state; "pin-states-1" accepts any state.
    pin_states_no_sleep: FakePinStates,
    pin_states: FakePinStates,
    log: CallLog,
}

impl RuntimePmFakes {
    #[track_caller]
    fn assert_votes_released(&self) {
        assert_eq!(self.powerdomain.ref_count(), 0);
        assert_eq!(self.clock_bus.ref_count(), 0);
        assert_eq!(self.clock_regs.ref_count(), 0);
    }
}

struct RuntimePmEnv {
    harness: TestHarness<DwSpiDriver>,
    fakes: RuntimePmFakes,
}

impl Deref for RuntimePmEnv {
    type Target = RuntimePmFakes;

    fn deref(&self) -> &Self::Target {
        &self.fakes
    }
}

impl DerefMut for RuntimePmEnv {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fakes
    }
}

struct StartedRuntimePm<'a> {
    env: &'a mut RuntimePmFakes,
    started_driver: DriverUnderTest<'a, DwSpiDriver>,
    client: fidl_next::Client<fspiimpl::SpiImpl, fdf_fidl::DriverChannel>,
}

impl Deref for StartedRuntimePm<'_> {
    type Target = RuntimePmFakes;

    fn deref(&self) -> &Self::Target {
        self.env
    }
}

impl DerefMut for StartedRuntimePm<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.env
    }
}

impl StartedRuntimePm<'_> {
    fn driver(&self) -> &DwSpiDriver {
        self.started_driver.get_driver().expect("Failed to get driver instance")
    }

    fn device(&self) -> &DwSpiDevice {
        &self.driver().device
    }

    async fn transfer(&self) {
        perform_transfer(&self.client, &self.env.irq, &self.env.gpio, &self.env.mapping, 0).await;
    }

    async fn stop(self) {
        self.started_driver.stop_driver().await;
    }
}

impl RuntimePmEnv {
    fn new(interrupt_controller_parent: InterruptControllerParent) -> Self {
        Self::with_max_frequency_hz(interrupt_controller_parent, Some(20_000_000))
    }

    /// Like `new()`, with the given maximum bus clock in the SPI bus metadata. `None` leaves it
    /// out, so that the driver keeps the baud rate divider it finds programmed at start.
    fn with_max_frequency_hz(
        interrupt_controller_parent: InterruptControllerParent,
        max_frequency_hz: Option<u32>,
    ) -> Self {
        let mut service_fs = ServiceFs::new();
        let scope = fasync::Scope::new_with_name("test_runtime_pm");

        let pdev = FakePDev::new();

        let entries = vec![fmetadata::DictionaryEntry {
            key: "dw_spi_rx_sample_delay_ns".to_string(),
            value: fmetadata::DictionaryValue::Int64(25),
        }];
        let dict = fmetadata::Dictionary { entries: Some(entries), ..Default::default() };
        let serialized_config = fidl::persist(&dict).expect("Failed to serialize config");
        pdev.add_metadata("fuchsia.driver.metadata.Dictionary", serialized_config);

        let metadata = fspi_businfo::SpiBusMetadata {
            channels: Some(vec![fspi_businfo::SpiChannel {
                cs: Some(0),
                max_frequency_hz,
                ..Default::default()
            }]),
            bus_id: Some(0),
            ..Default::default()
        };
        let serialized_metadata = fidl::persist(&metadata).expect("Failed to serialize metadata");
        pdev.add_metadata(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME, serialized_metadata);

        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice).expect("Failed to set cache policy");
        let mapping = mapped_vmo::Mapping::create_from_vmo(
            &vmo,
            0x100,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
        )
        .expect("Failed to map VMO");

        let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO");
        let irq =
            zx::VirtualInterrupt::create_virtual().expect("Failed to create virtual interrupt");
        let irq_dup =
            irq.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate interrupt");

        let mut pdev_config: fake_pdev::Config = Default::default();
        let mmio = fdevice::natural::Mmio { offset: Some(0), size: Some(0x100), vmo: Some(dup) };
        pdev_config.mmios.insert(0, mmio);
        pdev_config.irqs.insert(0, zx::Interrupt::from(irq.into_handle()));
        pdev.set_config(pdev_config);

        let clock_bus = TestClock::new();
        clock_bus.set_rate(200_000_000);
        let clock_regs = TestClock::new();

        let log: CallLog = Arc::new(Mutex::new(vec![]));
        let clocks = vec![clock_bus.inner.clone(), clock_regs.inner.clone()];
        let powerdomain = LoggedPowerDomain::new("pd", log.clone(), clocks.clone());
        clock_bus.set_domain(&powerdomain);
        clock_regs.set_domain(&powerdomain);
        let interrupt_controller = LoggedPowerDomain::new("irq", log.clone(), clocks);

        let reset = FakeReset::new();
        let pin_states_no_sleep = FakePinStates::with_states(["default"]);
        let pin_states = FakePinStates::new();
        let gpio = FakeGpio::default();

        let mut harness = TestHarness::<DwSpiDriver>::new()
            .add_offer(pdev.serve(&mut service_fs, scope.to_handle(), "pdev"))
            .add_offer(powerdomain.serve(&mut service_fs, scope.to_handle(), "power-domain"))
            .add_offer(clock_bus.serve(&mut service_fs, scope.to_handle(), "bus"))
            .add_offer(clock_regs.serve(&mut service_fs, scope.to_handle(), "registers"))
            .add_offer(reset.serve(&mut service_fs, scope.to_handle(), "reset"))
            .add_offer(pin_states_no_sleep.serve(
                &mut service_fs,
                scope.to_handle(),
                "pin-states-0",
            ))
            .add_offer(pin_states.serve(&mut service_fs, scope.to_handle(), "pin-states-1"))
            .add_offer(gpio.serve(&mut service_fs, scope.to_handle(), "cs-0"));
        if interrupt_controller_parent == InterruptControllerParent::Served {
            harness = harness.add_offer(interrupt_controller.serve(
                &mut service_fs,
                scope.to_handle(),
                "interrupt-controller-0",
            ));
        }
        let harness = harness.set_driver_incoming(service_fs);

        Self {
            harness,
            fakes: RuntimePmFakes {
                _scope: scope,
                _pdev: pdev,
                mapping,
                irq: irq_dup,
                gpio,
                powerdomain,
                interrupt_controller,
                clock_bus,
                clock_regs,
                reset,
                pin_states_no_sleep,
                pin_states,
                log,
            },
        }
    }

    async fn start(&mut self) -> StartedRuntimePm<'_> {
        let dispatcher = fdf_fidl::FidlExecutor::from(self.harness.dispatcher().clone());
        let started_driver = self.harness.start_driver().await.expect("Failed to start driver");
        let client = connect_spi(&started_driver, &dispatcher);
        StartedRuntimePm { env: &mut self.fakes, started_driver, client }
    }
}

fn read_reg(mapping: &mapped_vmo::Mapping, offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    mapping.read_at(offset, &mut bytes);
    u32::from_le_bytes(bytes)
}

fn write_reg(mapping: &mapped_vmo::Mapping, offset: usize, value: u32) {
    mapping.write_at(offset, &value.to_le_bytes());
}

fn take_log(log: &CallLog) -> Vec<String> {
    std::mem::take(&mut *log.lock())
}

/// Connects a client to the driver's SPI impl service.
fn connect_spi<E: fidl_next::Executor>(
    started_driver: &DriverUnderTest<'_, DwSpiDriver>,
    executor: &E,
) -> fidl_next::Client<fspiimpl::SpiImpl, fdf_fidl::DriverChannel> {
    let spi_service: ServiceInstance<fspiimpl::Service> =
        started_driver.driver_outgoing().service().connect_next().unwrap();
    let (client_end, server_end) = fdf_fidl::create_channel();
    spi_service.device(server_end).unwrap();
    client_end.spawn_on(executor)
}

/// Waits for the device to runtime suspend, failing the test if it does not within 5 s.
async fn wait_for_runtime_suspend(device: &DwSpiDevice) {
    async {
        while device.is_powered() {
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(1),
            ))
            .await;
        }
    }
    .on_timeout(zx::MonotonicDuration::from_seconds(5), || {
        panic!("Timed out waiting for the device to runtime suspend")
    })
    .await;
}

/// Enables autosuspend with a short delay and waits for the device to runtime suspend, then
/// disables autosuspend again so that the rest of the test is not timing sensitive.
async fn force_runtime_suspend(device: &DwSpiDevice) {
    device.set_autosuspend_delay(Some(zx::MonotonicDuration::from_millis(1)));
    wait_for_runtime_suspend(device).await;
    device.set_autosuspend_delay(None);
}

#[fuchsia::test]
async fn test_runtime_suspend_after_idle_following_start() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    assert!(drv.device().is_powered());
    assert!(drv.powerdomain.enabled());
    assert!(drv.clock_bus.enabled());
    assert!(drv.clock_regs.enabled());
    assert_eq!(drv.pin_states.current_state(), "default");

    force_runtime_suspend(drv.device()).await;

    // No votes are held once the bus has been idle for the autosuspend delay, and the controller
    // was quiesced before power was removed.
    drv.assert_votes_released();
    assert_eq!(drv.pin_states.current_state(), "sleep");
    assert_eq!(read_reg(&drv.mapping, 0x08), 0x0); // SSIENR
    assert_eq!(read_reg(&drv.mapping, 0x2c), 0x0); // IMR

    // Stopping while runtime suspended must not release any vote again.
    drv.stop().await;
    assert_eq!(env.powerdomain.enable_calls(), 1);
    assert_eq!(env.powerdomain.disable_calls(), 1);
}

#[fuchsia::test]
async fn test_transfer_powers_up_and_reinitializes_after_runtime_suspend() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let mut drv = env.start().await;
    assert!(drv.reset.take_toggled());

    force_runtime_suspend(drv.device()).await;
    assert!(!drv.powerdomain.enabled());

    // Simulate volatile register state loss while the power domain is off.
    for offset in [0x00, 0x08, 0x14, 0x18, 0xf0] {
        write_reg(&drv.mapping, offset, 0x0);
    }

    drv.transfer().await;

    assert!(drv.device().is_powered());
    assert_eq!(drv.powerdomain.enable_calls(), 2);
    assert!(drv.powerdomain.enabled());
    assert!(drv.clock_bus.enabled());
    assert!(drv.clock_regs.enabled());
    assert!(drv.reset.take_toggled());
    assert_eq!(drv.pin_states.current_state(), "default");
    assert_eq!(read_reg(&drv.mapping, 0x00), 0x7); // CTRLR0
    assert_eq!(read_reg(&drv.mapping, 0x14), 0xa); // BAUDR
    assert_eq!(read_reg(&drv.mapping, 0xf0), 0x5); // RX_SAMPLE_DLY
    assert_eq!(read_reg(&drv.mapping, 0x18), 0x80); // TXFTLR
    assert_eq!(read_reg(&drv.mapping, 0x08), 0x1); // SSIENR

    // The controller runtime suspends again once the bus is idle.
    force_runtime_suspend(drv.device()).await;
    assert!(!drv.powerdomain.enabled());
    assert_eq!(drv.powerdomain.disable_calls(), 2);

    drv.stop().await;
    assert_eq!(env.powerdomain.disable_calls(), 2);
}

#[fuchsia::test]
async fn test_back_to_back_transfers_do_not_power_cycle() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    // Long enough that the timer never fires between the transfers below.
    drv.device().set_autosuspend_delay(Some(zx::MonotonicDuration::from_seconds(60)));

    for _ in 0..3 {
        drv.transfer().await;
    }

    assert_eq!(drv.powerdomain.enable_calls(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 0);
    assert!(drv.clock_bus.enabled());
    assert!(drv.clock_regs.enabled());

    drv.stop().await;
    assert_eq!(env.powerdomain.disable_calls(), 1);
    assert!(!env.clock_bus.enabled());
    assert!(!env.clock_regs.enabled());
}

#[fuchsia::test]
async fn test_system_suspend_while_runtime_suspended() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    force_runtime_suspend(drv.device()).await;

    drv.driver().system_suspend().await.expect("system_suspend failed");
    drv.driver().system_resume(None).await.expect("system_resume failed");

    // Neither system suspend nor resume touched any vote.
    assert!(!drv.device().is_powered());
    assert_eq!(drv.powerdomain.enable_calls(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 1);
    assert!(!drv.clock_bus.enabled());
    assert!(!drv.clock_regs.enabled());

    drv.stop().await;
    assert_eq!(env.powerdomain.disable_calls(), 1);
}

#[fuchsia::test]
async fn test_system_suspend_while_runtime_powered_cancels_autosuspend() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    // Arm the autosuspend timer, then system suspend before it fires.
    drv.device().set_autosuspend_delay(Some(zx::MonotonicDuration::from_millis(20)));
    drv.driver().system_suspend().await.expect("system_suspend failed");
    assert!(!drv.device().is_powered());
    assert!(!drv.powerdomain.enabled());

    // The cancelled autosuspend timer must not release the votes a second time.
    fasync::Timer::new(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_millis(50)))
        .await;
    assert_eq!(drv.powerdomain.disable_calls(), 1);

    drv.stop().await;
    assert_eq!(env.powerdomain.disable_calls(), 1);
}

#[fuchsia::test]
async fn test_interrupt_controller_vote_ordering() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let drv = env.start().await;

    // The interrupt controller vote is taken after the power domain and before the clocks.
    assert_eq!(take_log(&drv.log), vec!["pd.enable", "irq.enable"]);
    assert!(drv.interrupt_controller.enabled());

    // It is released after the clocks and before the power domain.
    force_runtime_suspend(drv.device()).await;
    assert_eq!(take_log(&drv.log), vec!["irq.disable", "pd.disable"]);
    assert!(!drv.interrupt_controller.enabled());

    // A transfer takes the vote again, in the same order.
    drv.transfer().await;
    assert_eq!(take_log(&drv.log), vec!["pd.enable", "irq.enable"]);

    drv.stop().await;
    assert_eq!(take_log(&env.log), vec!["irq.disable", "pd.disable"]);
    assert!(!env.interrupt_controller.enabled());
    assert!(!env.powerdomain.enabled());
    assert_eq!(env.interrupt_controller.enable_calls(), 2);
    assert_eq!(env.interrupt_controller.disable_calls(), 2);
}

#[fuchsia::test]
async fn test_power_up_failure_rolls_back() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let drv = env.start().await;

    force_runtime_suspend(drv.device()).await;
    take_log(&drv.log);

    // Make the interrupt controller vote fail. The transfer fails and the power domain is
    // released again.
    drv.interrupt_controller.set_enable_error(Some(zx::Status::BAD_STATE));
    let result = drv.client.exchange_vector(0, vec![1u8, 2, 3]).await.expect("FIDL call failed");
    assert_eq!(result.unwrap_err(), zx::Status::BAD_STATE);

    assert!(!drv.device().is_powered());
    assert_eq!(take_log(&drv.log), vec!["pd.enable", "pd.disable"]);
    drv.assert_votes_released();

    // The next transfer powers up successfully.
    drv.interrupt_controller.set_enable_error(None);
    drv.transfer().await;
    assert!(drv.device().is_powered());
    assert_eq!(take_log(&drv.log), vec!["pd.enable", "irq.enable"]);

    drv.stop().await;
    assert!(!env.powerdomain.enabled());
    assert!(!env.interrupt_controller.enabled());
}

#[fuchsia::test]
async fn test_transfer_rejected_while_system_suspended() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    drv.driver().system_suspend().await.expect("system_suspend failed");
    assert!(!drv.powerdomain.enabled());

    // A request that reaches the driver after system suspend (e.g. one that was read before the
    // dispatcher was paused) must not take the power votes across suspend.
    let result = drv.client.exchange_vector(0, vec![1u8, 2, 3]).await.expect("FIDL call failed");
    assert_eq!(result.unwrap_err(), zx::Status::BAD_STATE);
    assert!(!drv.device().is_powered());
    assert!(!drv.powerdomain.enabled());
    assert_eq!(drv.powerdomain.enable_calls(), 1);

    drv.driver().system_resume(None).await.expect("system_resume failed");
    drv.transfer().await;
    assert!(drv.device().is_powered());

    drv.stop().await;
}

fn find_condition<'a>(
    conditions: &'a Option<Vec<HealthCondition>>,
    reason_code: &str,
) -> Option<&'a HealthCondition> {
    conditions.as_deref()?.iter().find(|c| c.reason_code.as_deref() == Some(reason_code))
}

async fn expect_health(
    started_driver: &DriverUnderTest<'_, DwSpiDriver>,
    request: HealthCheckRequest,
    expected_status: HealthStatus,
    expected_reasons: &[&str],
) -> HealthCheckResponse {
    let resp = started_driver.health_check(request).await.expect("health_check failed");
    let conditions = resp.conditions.as_deref().unwrap_or(&[]);
    if expected_reasons.is_empty() {
        assert_eq!(expected_status, HealthStatus::Ok);
        assert!(conditions.is_empty(), "Unexpected conditions: {conditions:?}");
    } else {
        for reason in expected_reasons {
            let cond = find_condition(&resp.conditions, reason)
                .unwrap_or_else(|| panic!("Missing condition {reason} in {conditions:?}"));
            assert_eq!(cond.status, Some(expected_status));
        }
        assert_eq!(conditions.len(), expected_reasons.len(), "Conditions: {conditions:?}");
    }
    resp
}

#[track_caller]
fn expect_condition(
    resp: &HealthCheckResponse,
    reason_code: &str,
    expected_count: u64,
    expected_reason: &str,
) {
    let cond = find_condition(&resp.conditions, reason_code)
        .unwrap_or_else(|| panic!("Missing condition {reason_code}"));
    assert_eq!(cond.occurrence_count, Some(expected_count));
    expect_diag_str(&cond.diagnostics, DIAG_REASON, expected_reason);
}

#[track_caller]
fn expect_diag_uint(resp: &HealthCheckResponse, key: &str, expected: u64) {
    let actual = resp.diagnostics.as_deref().and_then(|items| {
        items.iter().find_map(|item| match (&*item.key, &item.value) {
            (k, HealthDiagnosticValue::UintValue(v)) if k == key => Some(*v),
            _ => None,
        })
    });
    assert_eq!(actual, Some(expected), "diagnostic {key}");
}

#[track_caller]
fn expect_diag_bool(resp: &HealthCheckResponse, key: &str, expected: bool) {
    let actual = resp.diagnostics.as_deref().and_then(|items| {
        items.iter().find_map(|item| match (&*item.key, &item.value) {
            (k, HealthDiagnosticValue::BoolValue(v)) if k == key => Some(*v),
            _ => None,
        })
    });
    assert_eq!(actual, Some(expected), "diagnostic {key}");
}

#[track_caller]
fn expect_diag_str(diagnostics: &Option<Vec<HealthDiagnosticItem>>, key: &str, expected: &str) {
    let actual = diagnostics.as_deref().and_then(|items| {
        items.iter().find_map(|item| match (&*item.key, &item.value) {
            (k, HealthDiagnosticValue::StringValue(v)) if k == key => Some(v.as_str()),
            _ => None,
        })
    });
    assert_eq!(actual, Some(expected), "diagnostic {key}");
}

#[fuchsia::test]
async fn test_power_down_failure_forgets_vote_skips_mmio_and_latches_health() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let mut drv = env.start().await;

    // Make power domain Disable fail: the provider keeps the vote, and the driver treats the
    // controller as unpowered, never retries Disable, and latches SPI_POWER_RELEASE_FAILED.
    drv.powerdomain.set_disable_error(Some(zx::Status::TIMED_OUT));
    take_log(&drv.log);

    force_runtime_suspend(drv.device()).await;
    assert!(!drv.device().is_powered());
    assert!(!drv.clock_regs.enabled());
    assert!(!drv.clock_bus.enabled());
    assert!(!drv.interrupt_controller.enabled());
    assert!(drv.powerdomain.enabled());
    assert_eq!(drv.powerdomain.ref_count(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 1);
    assert_eq!(take_log(&drv.log), vec!["irq.disable"]);
    drv.powerdomain.set_disable_error(None);

    // Poison MMIO registers after power-down to verify nothing touches MMIO until the next
    // successful power-up.
    let sentinel = 0xdead_beef;
    for offset in [0x00, 0x08, 0x14, 0x18, 0x2c, 0xf0] {
        write_reg(&drv.mapping, offset, sentinel);
    }

    let alert = drv
        .started_driver
        .next_health_alert()
        .on_timeout(zx::MonotonicDuration::from_seconds(5), || {
            panic!("Timed out waiting for SelfFault health alert")
        })
        .await
        .expect("Expected SelfFault health alert");
    match alert {
        HealthAlert::SelfFault(resp) => {
            let cond = find_condition(&resp.conditions, REASON_POWER_RELEASE_FAILED)
                .expect("Missing SPI_POWER_RELEASE_FAILED in alert");
            assert_eq!(cond.status, Some(HealthStatus::Unhealthy));
            expect_diag_str(&cond.diagnostics, DIAG_REASON, "disable power domain");
        }
        other => panic!("Unexpected health alert: {other:?}"),
    }

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_RELEASE_FAILED, 1, "disable power domain");
    expect_diag_bool(&resp, DIAG_POWERED, false);
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 1);

    // Neither system_suspend, a failed power-up, nor health_check retries Disable or touches MMIO.
    drv.driver().system_suspend().await.expect("system_suspend failed");
    drv.driver().system_resume(None).await.expect("system_resume failed");
    assert_eq!(drv.powerdomain.disable_calls(), 1);

    drv.clock_bus.set_enable_error(Some(zx::Status::IO));
    let result = drv.client.exchange_vector(0, vec![1u8]).await.expect("FIDL call failed");
    assert_eq!(result.unwrap_err(), zx::Status::IO);
    drv.clock_bus.set_enable_error(None);
    // Rollback released only the newly acquired votes (leaving the leaked domain vote at 1), and
    // never wrote MMIO.
    assert_eq!(drv.powerdomain.ref_count(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 2);
    for offset in [0x00, 0x08, 0x14, 0x18, 0x2c, 0xf0] {
        assert_eq!(read_reg(&drv.mapping, offset), sentinel, "MMIO offset {offset:#x} modified");
    }

    // Next successful transfer takes fresh votes without retrying the earlier failed Disable.
    take_log(&drv.log);
    drv.transfer().await;
    assert_eq!(take_log(&drv.log), vec!["pd.enable", "irq.enable"]);
    assert_eq!(drv.powerdomain.disable_calls(), 2);
    assert_eq!(drv.powerdomain.ref_count(), 2);
    assert_eq!(drv.clock_regs.ref_count(), 1);
    assert_eq!(drv.clock_bus.ref_count(), 1);

    // Subsequent clean runtime suspend and stop release only the new vote; the leaked vote stays at
    // 1 and SPI_POWER_RELEASE_FAILED remains latched while SPI_POWER_UP_FAILED was cleared.
    force_runtime_suspend(drv.device()).await;
    assert_eq!(drv.powerdomain.ref_count(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 3);
    expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;

    drv.stop().await;
    assert_eq!(env.powerdomain.ref_count(), 1);
    assert_eq!(env.powerdomain.disable_calls(), 3);
}

async fn read_runtime_pm_property(driver: &DwSpiDriver, name: &str) -> InspectProperty {
    let hierarchy =
        fuchsia_inspect::reader::read(&driver.inspector).await.expect("Failed to read inspect");
    hierarchy
        .get_property_by_path(&["runtime_pm", name])
        .unwrap_or_else(|| panic!("Missing runtime_pm/{name}"))
        .clone()
}

async fn read_runtime_pm_uint(driver: &DwSpiDriver, name: &str) -> u64 {
    match read_runtime_pm_property(driver, name).await {
        InspectProperty::Uint(_, v) => v,
        other => panic!("Unexpected runtime_pm/{name}: {other:?}"),
    }
}

#[fuchsia::test]
async fn test_inspect_runtime_pm() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let drv = env.start().await;

    assert!(matches!(
        read_runtime_pm_property(drv.driver(), "powered").await,
        InspectProperty::Bool(_, true)
    ));
    assert!(matches!(
        read_runtime_pm_property(drv.driver(), "irq_controller_vote").await,
        InspectProperty::Bool(_, true)
    ));
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_up_count").await, 1);
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_down_count").await, 0);

    force_runtime_suspend(drv.device()).await;
    assert!(matches!(
        read_runtime_pm_property(drv.driver(), "powered").await,
        InspectProperty::Bool(_, false)
    ));
    assert!(matches!(
        read_runtime_pm_property(drv.driver(), "irq_controller_vote").await,
        InspectProperty::Bool(_, false)
    ));
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_down_count").await, 1);

    drv.interrupt_controller.set_enable_error(Some(zx::Status::BAD_STATE));
    let result = drv.client.exchange_vector(0, vec![1u8]).await.expect("FIDL call failed");
    assert_eq!(result.unwrap_err(), zx::Status::BAD_STATE);
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_up_failures").await, 1);

    drv.stop().await;
}

#[fuchsia::test]
async fn test_missing_sleep_state_is_only_requested_once() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    for _ in 0..3 {
        force_runtime_suspend(drv.device()).await;
        assert_eq!(drv.pin_states.current_state(), "sleep");
        assert_eq!(drv.pin_states_no_sleep.current_state(), "default");
        drv.transfer().await;
        assert_eq!(drv.pin_states_no_sleep.current_state(), "default");
    }
    force_runtime_suspend(drv.device()).await;

    // "pin-states-0" was only asked for its missing sleep state once; "pin-states-1" still gets it
    // on every power-down.
    assert_eq!(drv.pin_states_no_sleep.state_select_calls("sleep"), 1);
    assert_eq!(drv.pin_states.current_state(), "sleep");
    assert_eq!(drv.powerdomain.disable_calls(), 4);

    drv.stop().await;
}

#[fuchsia::test]
async fn test_system_suspend_release_failure_is_not_fatal() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;

    // A failed release does not fail suspend (that would make the driver host stop the driver).
    // Because a clock Disable failed, the power domain Disable is skipped so the domain stays on
    // while the clock remains prepared.
    drv.clock_regs.set_disable_error(Some(zx::Status::IO));
    take_log(&drv.log);
    drv.driver().system_suspend().await.expect("system_suspend failed");
    assert!(!drv.device().is_powered());
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_down_failures").await, 1);
    assert!(drv.powerdomain.enabled());
    assert_eq!(drv.powerdomain.disable_calls(), 0);
    assert!(take_log(&drv.log).is_empty());
    assert!(!drv.clock_bus.enabled());
    assert!(drv.clock_regs.enabled()); // The provider kept the vote.

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_RELEASE_FAILED, 1, "disable registers clock");
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 1);

    // The next transfer after resume takes fresh votes without retrying the failed Disable.
    drv.clock_regs.set_disable_error(None);
    drv.driver().system_resume(None).await.expect("system_resume failed");
    drv.transfer().await;
    // The leaked registers-clock vote is still counted by the provider (along with the kept domain
    // vote).
    assert_eq!(take_log(&drv.log), vec!["pd.enable (clocks on)"]);
    assert_eq!(drv.clock_regs.disable_calls(), 1);
    assert_eq!(drv.clock_regs.enable_calls(), 2);
    assert_eq!(drv.clock_regs.ref_count(), 2);
    assert_eq!(drv.powerdomain.ref_count(), 2);
    assert_eq!(drv.clock_regs.disables_without_domain(), 0);
    assert_eq!(drv.clock_bus.disables_without_domain(), 0);

    drv.stop().await;
    // Stopping releases only the second set of votes; the leaked clock and domain votes stay at 1.
    assert_eq!(env.clock_regs.ref_count(), 1);
    assert_eq!(env.powerdomain.ref_count(), 1);
}

#[fuchsia::test]
async fn test_initial_divider_restored_after_runtime_suspend() {
    // Without a maximum bus clock, the driver keeps the divider it finds programmed at start (left
    // by the bootloader, unless the reset toggle cleared it).
    let mut env = RuntimePmEnv::with_max_frequency_hz(InterruptControllerParent::Absent, None);
    write_reg(&env.mapping, 0x14, 0x8); // BAUDR
    write_reg(&env.mapping, 0xf0, 0x3); // RX_SAMPLE_DLY
    let drv = env.start().await;
    assert_eq!(read_reg(&drv.mapping, 0x14), 0x8); // BAUDR

    // Power-down clears BAUDR, and the register contents are lost with the power domain.
    force_runtime_suspend(drv.device()).await;
    assert_eq!(read_reg(&drv.mapping, 0x14), 0x0); // BAUDR
    write_reg(&drv.mapping, 0xf0, 0x0); // RX_SAMPLE_DLY

    // The next power-up restores the initial divider and RX sample delay.
    drv.transfer().await;
    assert_eq!(read_reg(&drv.mapping, 0x14), 0x8); // BAUDR
    assert_eq!(read_reg(&drv.mapping, 0xf0), 0x3); // RX_SAMPLE_DLY

    drv.stop().await;
}

#[fuchsia::test]
async fn test_interrupt_timeout_aborts_transfer() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;
    drv.device().set_interrupt_timeout(zx::MonotonicDuration::from_millis(10));

    // The interrupt never fires (for example because the interrupt controller lost its state),
    // so the transfer fails instead of waiting forever.
    write_reg(&drv.mapping, 0x20, 0x0); // TXFLR
    write_reg(&drv.mapping, 0x24, 0x0); // RXFLR
    let result = drv.client.exchange_vector(0, vec![1u8, 2, 3]).await.expect("FIDL call failed");
    assert_eq!(result.unwrap_err(), zx::Status::TIMED_OUT);

    // Interrupts are masked, the controller was re-enabled (flushing the FIFOs), the target is
    // deselected and chip select is deasserted.
    assert_eq!(read_reg(&drv.mapping, 0x2c), 0x0); // IMR
    assert_eq!(read_reg(&drv.mapping, 0x08), 0x1); // SSIENR
    assert_eq!(read_reg(&drv.mapping, 0x10), 0x0); // SER
    assert_eq!(drv.gpio.buffer_mode(), fgpio::BufferMode::OutputHigh);
    assert!(drv.device().is_powered());

    // The next transfer works.
    drv.device().set_interrupt_timeout(INTERRUPT_TIMEOUT);
    drv.transfer().await;

    // The controller still runtime suspends once the bus is idle.
    force_runtime_suspend(drv.device()).await;
    assert!(!drv.powerdomain.enabled());

    drv.stop().await;
}

#[fuchsia::test]
async fn test_clock_disable_failure_keeps_power_domain_and_never_retries() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    let drv = env.start().await;
    take_log(&drv.log);

    // Case 1: registers clock Disable fails on runtime suspend. PowerDomain::Disable is skipped so
    // the domain never powers off under a still-prepared clock, and system_suspend() while already
    // unpowered does not retry either release.
    drv.clock_regs.set_disable_error(Some(zx::Status::TIMED_OUT));
    force_runtime_suspend(drv.device()).await;
    assert!(!drv.device().is_powered());
    assert!(drv.powerdomain.enabled());
    assert_eq!(drv.powerdomain.ref_count(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 0);
    assert_eq!(drv.clock_regs.disable_calls(), 1);
    assert_eq!(drv.clock_regs.ref_count(), 1);
    assert!(!drv.clock_bus.enabled());
    assert!(take_log(&drv.log).is_empty());

    drv.clock_regs.set_disable_error(None);
    drv.driver().system_suspend().await.expect("system_suspend failed");
    drv.driver().system_resume(None).await.expect("system_resume failed");
    assert!(take_log(&drv.log).is_empty());
    assert_eq!(drv.clock_regs.disable_calls(), 1);
    assert_eq!(drv.powerdomain.disable_calls(), 0);

    // Case 2: next transfer takes fresh votes, then bus clock Disable fails on runtime suspend,
    // which also skips PowerDomain::Disable and updates SPI_POWER_RELEASE_FAILED.
    drv.transfer().await;
    // The leaked registers-clock vote is still counted by the provider (along with the kept domain
    // vote).
    assert_eq!(take_log(&drv.log), vec!["pd.enable (clocks on)"]);
    assert_eq!(drv.powerdomain.ref_count(), 2);
    assert_eq!(drv.clock_regs.ref_count(), 2);
    assert_eq!(drv.clock_bus.ref_count(), 1);

    drv.clock_bus.set_disable_error(Some(zx::Status::TIMED_OUT));
    force_runtime_suspend(drv.device()).await;
    assert!(!drv.device().is_powered());
    assert_eq!(drv.powerdomain.disable_calls(), 0);
    assert_eq!(drv.powerdomain.ref_count(), 2);
    assert_eq!(drv.clock_regs.ref_count(), 1);
    assert_eq!(drv.clock_bus.ref_count(), 1);
    assert!(take_log(&drv.log).is_empty());
    drv.clock_bus.set_disable_error(None);

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_RELEASE_FAILED, 2, "disable bus clock");

    // Stopping while unpowered does not retry any leaked vote, and clocks were never disabled
    // without the power domain enabled.
    drv.stop().await;
    assert_eq!(env.powerdomain.disable_calls(), 0);
    assert_eq!(env.clock_regs.disables_without_domain(), 0);
    assert_eq!(env.clock_bus.disables_without_domain(), 0);
}

#[fuchsia::test]
async fn test_health_check_power_up_rollback_reset_and_pin_state_failures() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let drv = env.start().await;

    // 1. Healthy Ping at start; non-Ping requests report NotSupported.
    let resp =
        expect_health(&drv.started_driver, HealthCheckRequest::Ping, HealthStatus::Ok, &[]).await;
    expect_diag_bool(&resp, DIAG_POWERED, true);
    expect_diag_uint(&resp, DIAG_POWER_UP_COUNT, 1);
    expect_diag_uint(&resp, DIAG_POWER_DOWN_COUNT, 0);
    expect_diag_uint(&resp, DIAG_POWER_UP_FAILURES, 0);
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 0);
    assert_eq!(
        drv.started_driver.health_check(HealthCheckRequest::Deep).await,
        Err(HealthCheckError::NotSupported)
    );

    force_runtime_suspend(drv.device()).await;

    // 2. Power-up failure on registers clock Enable with clean rollback latches only
    // SPI_POWER_UP_FAILED.
    drv.clock_regs.set_enable_error(Some(zx::Status::IO));
    let res = drv.client.exchange_vector(0, vec![1u8]).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::IO);
    drv.clock_regs.set_enable_error(None);
    assert_eq!(drv.powerdomain.ref_count(), 0);
    assert_eq!(drv.clock_bus.ref_count(), 0);

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_UP_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_UP_FAILED, 1, "enable registers clock");
    expect_diag_uint(&resp, DIAG_POWER_UP_FAILURES, 1);

    // 3. Reset toggle failure also rolls back votes and increments SPI_POWER_UP_FAILED.
    drv.reset.set_toggle_error(Some(zx::Status::BAD_STATE));
    let res = drv.client.exchange_vector(0, vec![1u8]).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::BAD_STATE);
    drv.reset.set_toggle_error(None);
    assert_eq!(drv.powerdomain.ref_count(), 0);
    assert_eq!(drv.clock_regs.ref_count(), 0);
    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_UP_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_UP_FAILED, 2, "toggle reset");
    expect_diag_uint(&resp, DIAG_POWER_UP_FAILURES, 2);

    // 4. Default pin-state failure combined with a rollback Disable failure latches both
    // SPI_POWER_UP_FAILED and SPI_POWER_RELEASE_FAILED.
    drv.pin_states_no_sleep.set_state_error("default", Some(zx::Status::IO));
    drv.powerdomain.set_disable_error(Some(zx::Status::TIMED_OUT));
    let res = drv.client.exchange_vector(0, vec![1u8]).await.expect("FIDL call failed");
    assert_eq!(res.unwrap_err(), zx::Status::IO);
    drv.pin_states_no_sleep.set_state_error("default", None);
    drv.powerdomain.set_disable_error(None);
    assert_eq!(drv.powerdomain.ref_count(), 1);

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_UP_FAILED, REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_UP_FAILED, 3, "select default pin state");
    expect_condition(&resp, REASON_POWER_RELEASE_FAILED, 1, "disable power domain");
    expect_diag_uint(&resp, DIAG_POWER_UP_FAILURES, 3);
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 1);

    // 5. A successful transfer clears SPI_POWER_UP_FAILED while keeping SPI_POWER_RELEASE_FAILED.
    drv.transfer().await;
    expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;

    // 6. Failing to release the interrupt controller power domain vote on power-down also updates
    // SPI_POWER_RELEASE_FAILED while still disabling the controller's own power domain.
    drv.interrupt_controller.set_disable_error(Some(zx::Status::IO));
    force_runtime_suspend(drv.device()).await;
    drv.interrupt_controller.set_disable_error(None);
    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(
        &resp,
        REASON_POWER_RELEASE_FAILED,
        2,
        "disable interrupt controller power domain",
    );
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 2);

    drv.stop().await;
}

#[fuchsia::test]
async fn test_sleep_pin_state_error_latches_power_release_failed() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Absent);
    env.pin_states_no_sleep.set_state_error("sleep", Some(zx::Status::IO));
    let drv = env.start().await;

    force_runtime_suspend(drv.device()).await;
    // Votes were still released despite the sleep pin-state failure.
    drv.assert_votes_released();

    let resp = expect_health(
        &drv.started_driver,
        HealthCheckRequest::Ping,
        HealthStatus::Unhealthy,
        &[REASON_POWER_RELEASE_FAILED],
    )
    .await;
    expect_condition(&resp, REASON_POWER_RELEASE_FAILED, 1, "select sleep pin state");
    expect_diag_uint(&resp, DIAG_POWER_DOWN_FAILURES, 1);

    drv.stop().await;
}

#[fuchsia::test]
async fn test_power_up_latency_stats() {
    let mut env = RuntimePmEnv::new(InterruptControllerParent::Served);
    let drv = env.start().await;

    // Initial power-up at start records latency without counting as a transfer wait.
    let last_1 = read_runtime_pm_uint(drv.driver(), "last_power_up_ns").await;
    let max_1 = read_runtime_pm_uint(drv.driver(), "max_power_up_ns").await;
    let total_1 = read_runtime_pm_uint(drv.driver(), "total_power_up_ns").await;
    assert!(last_1 > 0);
    assert_eq!(max_1, last_1);
    assert_eq!(total_1, last_1);
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_up_waits").await, 0);

    let resp =
        expect_health(&drv.started_driver, HealthCheckRequest::Ping, HealthStatus::Ok, &[]).await;
    expect_diag_uint(&resp, DIAG_POWER_UP_WAITS, 0);
    expect_diag_uint(&resp, DIAG_LAST_POWER_UP_NS, last_1);
    expect_diag_uint(&resp, DIAG_MAX_POWER_UP_NS, max_1);
    expect_diag_uint(&resp, DIAG_TOTAL_POWER_UP_NS, total_1);

    // A transfer while already powered does not wait for or record another power-up.
    drv.transfer().await;
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_up_waits").await, 0);
    assert_eq!(read_runtime_pm_uint(drv.driver(), "last_power_up_ns").await, last_1);
    assert_eq!(read_runtime_pm_uint(drv.driver(), "total_power_up_ns").await, total_1);

    // After runtime suspend, the first transfer waits for a power-up and updates latency stats.
    force_runtime_suspend(drv.device()).await;
    drv.transfer().await;

    let last_2 = read_runtime_pm_uint(drv.driver(), "last_power_up_ns").await;
    let max_2 = read_runtime_pm_uint(drv.driver(), "max_power_up_ns").await;
    let total_2 = read_runtime_pm_uint(drv.driver(), "total_power_up_ns").await;
    assert_eq!(read_runtime_pm_uint(drv.driver(), "power_up_waits").await, 1);
    assert!(last_2 > 0);
    assert!(max_2 >= last_2);
    assert_eq!(max_2, std::cmp::max(max_1, last_2));
    assert_eq!(total_2, total_1 + last_2);

    let resp =
        expect_health(&drv.started_driver, HealthCheckRequest::Ping, HealthStatus::Ok, &[]).await;
    expect_diag_uint(&resp, DIAG_POWER_UP_WAITS, 1);
    expect_diag_uint(&resp, DIAG_LAST_POWER_UP_NS, last_2);
    expect_diag_uint(&resp, DIAG_MAX_POWER_UP_NS, max_2);
    expect_diag_uint(&resp, DIAG_TOTAL_POWER_UP_NS, total_2);

    drv.stop().await;
}
