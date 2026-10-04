// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Driver realm tests for the driver-lab driver.
//!
//! A DriverTestRealm boots the lab_root test driver, which adds one
//! node marked as a proxy target and offers it a fake platform device
//! with a single VMO-backed MMIO, a virtual interrupt, and fake GPIO,
//! I2C, and SPI services. The proxy under test binds to that node
//! through its real bind rules, maps the MMIO, acquires the protocol
//! backends, and serves the `fuchsia.driver.lab` wire contract, which
//! these tests exercise through real FIDL: identity, policy enforcement,
//! MMIO reads, GPIO, I2C, SPI, interrupts, and audit.

use anyhow::{Context, Result};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_driver_lab as flab;
use fidl_fuchsia_driver_test::RealmArgs;
use fuchsia_component::client::Service;
use fuchsia_component_test::{Capability, RealmBuilder};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};

// Seeded by the lab_root test driver.
const MMIO_SIZE: u64 = 0x1000;
const SEED_VALUE: u32 = 0xFEED_FACE;
const SEED_OFFSET: u64 = 0x10;

async fn start_realm() -> Result<(fuchsia_component_test::RealmInstance, flab::Proxy_Proxy)> {
    let builder = RealmBuilder::new().await?;
    builder.driver_test_realm_setup().await?;
    let dtr_exposes = vec![Capability::service::<flab::ServiceMarker>().into()];
    builder.driver_test_realm_add_dtr_exposes(&dtr_exposes).await?;
    let instance = builder.build().await?;
    instance
        .driver_test_realm_start(RealmArgs { dtr_exposes: Some(dtr_exposes), ..Default::default() })
        .await?;
    let device = Service::open_from_dir(instance.root.get_exposed_dir(), flab::ServiceMarker)
        .context("failed to open fuchsia.driver.lab service")?
        .watch_for_any()
        .await
        .context("failed to find a proxy instance")?;
    let proxy = device.connect_to_proxy()?;
    Ok((instance, proxy))
}

fn expectations(description: &flab::ProxyDescription) -> flab::Expectations {
    flab::Expectations {
        boot_id: description.boot_id.clone(),
        proxy_generation: description.proxy_generation,
        resource_digest: description.resource_digest.clone(),
        policy_digest: description.policy_digest.clone(),
        ..Default::default()
    }
}

fn run_context() -> flab::RunContext {
    flab::RunContext {
        run_id: Some("realm-run".to_string()),
        case_id: Some("realm-case".to_string()),
        plan_digest: Some("sha256:realm".to_string()),
        host_tool_version: Some("realm-test".to_string()),
        ..Default::default()
    }
}

fn read_rule(offset: u64) -> flab::AccessRule {
    flab::AccessRule { resource: 0, offset, width: 4, class: flab::AccessClass::ReadOnce }
}

async fn open_session(
    proxy: &flab::Proxy_Proxy,
    description: &flab::ProxyDescription,
    allowlist: Vec<flab::AccessRule>,
) -> Result<flab::SessionProxy> {
    let (session, server) = create_proxy::<flab::SessionMarker>();
    proxy
        .open_session(
            &run_context(),
            flab::SessionMode::ReadOnly,
            &expectations(description),
            &allowlist,
            server,
        )
        .await?
        .map_err(|error| anyhow::anyhow!("open_session rejected: {error:?}"))?;
    Ok(session)
}

async fn open_mutating_session(
    proxy: &flab::Proxy_Proxy,
    description: &flab::ProxyDescription,
    allowlist: Vec<flab::AccessRule>,
) -> Result<flab::SessionProxy> {
    let (session, server) = create_proxy::<flab::SessionMarker>();
    proxy
        .open_session(
            &run_context(),
            flab::SessionMode::Mutating,
            &expectations(description),
            &allowlist,
            server,
        )
        .await?
        .map_err(|error| anyhow::anyhow!("open_mutating_session rejected: {error:?}"))?;
    Ok(session)
}

#[fuchsia::test]
async fn describe_reports_real_identity_and_resources() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;

    assert_eq!(description.protocol_major, Some(1));
    // Real per-boot identity: 16 random bytes as hex, never a stub.
    let boot_id = description.boot_id.clone().expect("boot_id present");
    assert_eq!(boot_id.len(), 32);
    assert_ne!(boot_id, "unknown");
    assert!(description.proxy_generation.unwrap_or(0) > 0);
    assert_eq!(description.takeover, Some(flab::TakeoverState::NotActive));

    // The fake platform device offers 1 MMIO and 1 IRQ, plus fake GPIO, I2C, SPI, Clock, Reset, Serial.
    let resources = description.resources.clone().expect("resources present");
    assert_eq!(resources.len(), 8);

    let mmio = &resources[0];
    assert_eq!(mmio.id, Some(0));
    assert_eq!(mmio.name.as_deref(), Some("mmio0"));
    assert_eq!(mmio.logical_size, Some(MMIO_SIZE));
    assert_eq!(mmio.kind, Some(flab::ResourceKind::Mmio));
    assert!(mmio.digest.as_deref().unwrap_or("").starts_with("sha256:"));

    let irq = &resources[1];
    assert_eq!(irq.id, Some(1));
    assert_eq!(irq.name.as_deref(), Some("irq0"));
    assert_eq!(irq.kind, Some(flab::ResourceKind::Interrupt));

    let gpio = &resources[2];
    assert_eq!(gpio.id, Some(2));
    assert_eq!(gpio.name.as_deref(), Some("gpio0"));
    assert_eq!(gpio.kind, Some(flab::ResourceKind::Gpio));

    let i2c = &resources[3];
    assert_eq!(i2c.id, Some(3));
    assert_eq!(i2c.name.as_deref(), Some("i2c0"));
    assert_eq!(i2c.kind, Some(flab::ResourceKind::I2C));

    let spi = &resources[4];
    assert_eq!(spi.id, Some(4));
    assert_eq!(spi.name.as_deref(), Some("spi0"));
    assert_eq!(spi.kind, Some(flab::ResourceKind::Spi));

    let clock = &resources[5];
    assert_eq!(clock.id, Some(5));
    assert_eq!(clock.name.as_deref(), Some("clock0"));
    assert_eq!(clock.kind, Some(flab::ResourceKind::Clock));

    let reset = &resources[6];
    assert_eq!(reset.id, Some(6));
    assert_eq!(reset.name.as_deref(), Some("reset0"));
    assert_eq!(reset.kind, Some(flab::ResourceKind::Reset));

    let serial = &resources[7];
    assert_eq!(serial.id, Some(7));
    assert_eq!(serial.name.as_deref(), Some("serial0"));
    assert_eq!(serial.kind, Some(flab::ResourceKind::Serial));

    assert_ne!(description.resource_digest, description.policy_digest);
    Ok(())
}

#[fuchsia::test]
async fn exact_allowed_read_reaches_real_mmio() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let session = open_session(&proxy, &description, vec![read_rule(SEED_OFFSET)]).await?;

    // The allowed read returns the value lab_root seeded in the VMO.
    let (value, _audit_seq, _timestamp_ns) = session
        .read32(0, SEED_OFFSET)
        .await?
        .map_err(|error| anyhow::anyhow!("read32 denied: {error:?}"))?;
    assert_eq!(value, SEED_VALUE);

    // An offset outside the exact allowlist is denied, not read.
    let denied = session.read32(0, 0x20).await?;
    assert_eq!(denied.unwrap_err(), flab::OperationError::NotInAllowlist);

    // A read-once rule never authorizes the snapshot class.
    let snapshot =
        session.snapshot(&[flab::SnapshotItem { resource: 0, offset: SEED_OFFSET }]).await?;
    assert_eq!(snapshot.unwrap_err(), flab::OperationError::NotInAllowlist);

    // The audit trail covers the session open, the allowed read, and
    // both rejections, stamped with instance identity.
    let page = session.read_audit(0, 64).await?;
    let entries = page.0;
    let operations: Vec<String> =
        entries.iter().filter_map(|entry| entry.operation.clone()).collect();
    assert!(operations.iter().any(|operation| operation == "open_session"));
    assert!(operations.iter().any(|operation| operation == "read32"));
    let denied_count =
        entries.iter().filter(|entry| entry.decision == Some(flab::AuditDecision::Denied)).count();
    assert_eq!(denied_count, 2);
    for entry in &entries {
        assert_eq!(entry.boot_id, description.boot_id);
        assert_eq!(entry.proxy_generation, description.proxy_generation);
    }
    let open_entry = entries
        .iter()
        .find(|entry| entry.operation.as_deref() == Some("open_session"))
        .expect("open_session audit entry");
    assert_eq!(open_entry.run_id.as_deref(), Some("realm-run"));
    Ok(())
}

#[fuchsia::test]
async fn gpio_read_and_write_work() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 2, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let (value, _audit_seq, _timestamp_ns) =
        session.gpio_read(2).await?.map_err(|e| anyhow::anyhow!("gpio_read: {e:?}"))?;
    assert_eq!(value, true);

    let (audit_seq, _timestamp_ns) =
        session.gpio_write(2, false).await?.map_err(|e| anyhow::anyhow!("gpio_write: {e:?}"))?;
    assert!(audit_seq > 0);
    Ok(())
}

#[fuchsia::test]
async fn i2c_transfer_works() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 3, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let (read_data, audit_seq, _timestamp_ns) = session
        .i2c_transfer(3, &[0xAA, 0xBB], 4)
        .await?
        .map_err(|e| anyhow::anyhow!("i2c_transfer: {e:?}"))?;
    assert_eq!(read_data, vec![0x42, 0x42, 0x42, 0x42]);
    assert!(audit_seq > 0);
    Ok(())
}

#[fuchsia::test]
async fn spi_transmit_works() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 4, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let tx = vec![0x11, 0x22, 0x33, 0x44];
    let (rx_data, audit_seq, _timestamp_ns) =
        session.spi_transmit(4, &tx).await?.map_err(|e| anyhow::anyhow!("spi_transmit: {e:?}"))?;
    assert_eq!(rx_data, tx);
    assert!(audit_seq > 0);
    Ok(())
}

#[fuchsia::test]
async fn wait_for_interrupt_works() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 1, offset: 0, width: 0, class: flab::AccessClass::Interrupt };
    let session = open_session(&proxy, &description, vec![rule]).await?;

    let (resource, sequence, count, _timestamp_ns, _coalesced_count) = session
        .wait_for_interrupt(1, 0, 1_000_000_000)
        .await?
        .map_err(|e| anyhow::anyhow!("wait_for_interrupt: {e:?}"))?;
    assert_eq!(resource, 1);
    assert!(sequence >= 1);
    assert!(count >= 1);
    Ok(())
}

#[fuchsia::test]
async fn clock_enable_disable_and_rates_work() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 5, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let (enabled, _seq, _ts) = session
        .clock_is_enabled(5)
        .await?
        .map_err(|e| anyhow::anyhow!("clock_is_enabled: {e:?}"))?;
    assert_eq!(enabled, false);

    let (_seq, _ts) =
        session.clock_enable(5).await?.map_err(|e| anyhow::anyhow!("clock_enable: {e:?}"))?;

    let (enabled, _seq, _ts) = session
        .clock_is_enabled(5)
        .await?
        .map_err(|e| anyhow::anyhow!("clock_is_enabled: {e:?}"))?;
    assert_eq!(enabled, true);

    let (rate, _seq, _ts) =
        session.clock_get_rate(5).await?.map_err(|e| anyhow::anyhow!("clock_get_rate: {e:?}"))?;
    assert_eq!(rate, 24_000_000);

    let (_seq, _ts) = session
        .clock_set_rate(5, 48_000_000)
        .await?
        .map_err(|e| anyhow::anyhow!("clock_set_rate: {e:?}"))?;

    let (rate, _seq, _ts) =
        session.clock_get_rate(5).await?.map_err(|e| anyhow::anyhow!("clock_get_rate: {e:?}"))?;
    assert_eq!(rate, 48_000_000);

    let (rate_out, _seq, _ts) = session
        .clock_query_rate(5, 100_000_000)
        .await?
        .map_err(|e| anyhow::anyhow!("clock_query_rate: {e:?}"))?;
    assert_eq!(rate_out, 100_000_000);

    let (_seq, _ts) =
        session.clock_disable(5).await?.map_err(|e| anyhow::anyhow!("clock_disable: {e:?}"))?;

    let (enabled, _seq, _ts) = session
        .clock_is_enabled(5)
        .await?
        .map_err(|e| anyhow::anyhow!("clock_is_enabled: {e:?}"))?;
    assert_eq!(enabled, false);

    Ok(())
}

#[fuchsia::test]
async fn reset_assert_deassert_and_status_work() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 6, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let (asserted, _seq, _ts) =
        session.reset_status(6).await?.map_err(|e| anyhow::anyhow!("reset_status: {e:?}"))?;
    assert_eq!(asserted, false);

    let (_seq, _ts) =
        session.reset_assert(6).await?.map_err(|e| anyhow::anyhow!("reset_assert: {e:?}"))?;

    let (asserted, _seq, _ts) =
        session.reset_status(6).await?.map_err(|e| anyhow::anyhow!("reset_status: {e:?}"))?;
    assert_eq!(asserted, true);

    let (_seq, _ts) =
        session.reset_deassert(6).await?.map_err(|e| anyhow::anyhow!("reset_deassert: {e:?}"))?;

    let (asserted, _seq, _ts) =
        session.reset_status(6).await?.map_err(|e| anyhow::anyhow!("reset_status: {e:?}"))?;
    assert_eq!(asserted, false);

    let (_seq, _ts) =
        session.reset_toggle(6).await?.map_err(|e| anyhow::anyhow!("reset_toggle: {e:?}"))?;

    let (asserted, _seq, _ts) =
        session.reset_status(6).await?.map_err(|e| anyhow::anyhow!("reset_status: {e:?}"))?;
    assert_eq!(asserted, true);

    Ok(())
}

#[fuchsia::test]
async fn serial_read_and_write_work() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;
    let rule =
        flab::AccessRule { resource: 7, offset: 0, width: 0, class: flab::AccessClass::Protocol };
    let session = open_mutating_session(&proxy, &description, vec![rule]).await?;

    let (data, _seq, _ts) =
        session.serial_read(7).await?.map_err(|e| anyhow::anyhow!("serial_read: {e:?}"))?;
    assert!(data.is_empty());

    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let (_seq, _ts) = session
        .serial_write(7, &payload)
        .await?
        .map_err(|e| anyhow::anyhow!("serial_write: {e:?}"))?;

    let (data, _seq, _ts) =
        session.serial_read(7).await?.map_err(|e| anyhow::anyhow!("serial_read: {e:?}"))?;
    assert_eq!(data, payload);

    Ok(())
}

#[fuchsia::test]
async fn stale_and_reserved_expectations_are_rejected() -> Result<()> {
    let (_instance, proxy) = start_realm().await?;
    let description = proxy.describe().await?;

    // A stale boot id creates no session.
    let (_, server) = create_proxy::<flab::SessionMarker>();
    let mut stale = expectations(&description);
    stale.boot_id = Some("boot-stale".to_string());
    let result = proxy
        .open_session(&run_context(), flab::SessionMode::ReadOnly, &stale, &[], server)
        .await?;
    assert_eq!(result.unwrap_err(), flab::OpenSessionError::StaleBootId);

    // A reserved phase 2 expectation fails closed.
    let (_, server) = create_proxy::<flab::SessionMarker>();
    let mut reserved = expectations(&description);
    reserved.topology_generation = Some(7);
    let result = proxy
        .open_session(&run_context(), flab::SessionMode::ReadOnly, &reserved, &[], server)
        .await?;
    assert_eq!(result.unwrap_err(), flab::OpenSessionError::UnsupportedExpectation);

    // A rule outside the offered resources rejects the whole allowlist.
    let (_, server) = create_proxy::<flab::SessionMarker>();
    let result = proxy
        .open_session(
            &run_context(),
            flab::SessionMode::ReadOnly,
            &expectations(&description),
            &[flab::AccessRule {
                resource: 9,
                offset: 0,
                width: 4,
                class: flab::AccessClass::ReadOnce,
            }],
            server,
        )
        .await?;
    assert_eq!(result.unwrap_err(), flab::OpenSessionError::RejectedAllowlist);
    Ok(())
}
