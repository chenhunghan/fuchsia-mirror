// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Driver realm integration tests for `lab_sample_driver` (CS28).
//!
//! Boots a `DriverTestRealm` containing `lab_root` and `lab_sample_driver`
//! (without `lab_proxy`), connects to `fuchsia.driver.lab.Service` exposed
//! directly by the active `lab_sample_driver` component, and verifies:
//! - In-situ `Describe` identity and shared resources (`mmio0` + `irq0`)
//! - Safe concurrent reads (`REG_DEVICE_ID`, `REG_STATUS`)
//! - Rejection of `hard_denied` destructive FIFO register (`REG_FIFO_DATA`)
//! - Cooperative quiesce engagement (`STATUS_QUIESCED` bit in `REG_STATUS` +
//!   audit records) during `Mutating` sessions and release on close
//! - Interrupt event tapping via `WaitForInterrupt`

use anyhow::{Context, Result};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_driver_lab as flab;
use fidl_fuchsia_driver_test::RealmArgs;
use fuchsia_component::client::Service;
use fuchsia_component_test::{Capability, RealmBuilder};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};

const REG_DEVICE_ID: u64 = 0x00;
const SAMPLE_DEVICE_ID: u32 = 0x5341_4D50;
const REG_STATUS: u64 = 0x04;
const STATUS_READY: u32 = 0x01;
const STATUS_QUIESCED: u32 = 0x10;
const REG_CONTROL: u64 = 0x08;
const REG_FIFO_DATA: u64 = 0x40;

const STATE_BUSY_FLAG: u64 = 0x00;
const STATE_INVARIANT_VIOLATIONS: u64 = 0x04;
const KNOB_RACE_DELAY_US: u64 = 0x08;
const KNOB_FIX_ENABLED: u64 = 0x0C;
const TRIGGER_CONCURRENT_OP: u64 = 0x10;

async fn start_sample_realm() -> Result<(fuchsia_component_test::RealmInstance, flab::Proxy_Proxy)>
{
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
        .context("failed to find sample driver lab service instance")?;
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
        run_id: Some("sample-realm-run".to_string()),
        case_id: Some("sample-realm-case".to_string()),
        plan_digest: Some("sha256:sample-realm".to_string()),
        host_tool_version: Some("sample-realm-test".to_string()),
        ..Default::default()
    }
}

#[fuchsia::test]
async fn sample_driver_in_situ_describe_quiesce_and_interrupt_tap() -> Result<()> {
    let (_instance, proxy) = start_sample_realm().await?;
    let description = proxy.describe().await?;

    assert_eq!(description.protocol_major, Some(1));
    let resources = description.resources.clone().expect("resources present");
    assert_eq!(resources.len(), 3);
    assert_eq!(resources[0].name.as_deref(), Some("mmio0"));
    assert_eq!(resources[0].kind, Some(flab::ResourceKind::Mmio));
    assert_eq!(resources[1].name.as_deref(), Some("irq0"));
    assert_eq!(resources[1].kind, Some(flab::ResourceKind::Interrupt));
    assert_eq!(resources[2].name.as_deref(), Some("state0"));
    assert_eq!(resources[2].kind, Some(flab::ResourceKind::Mmio));
    assert_eq!(resources[2].logical_size, Some(0x20));

    // 1. Destructive FIFO register (0x40) is hard_denied in the ceiling.
    let (_denied_session, denied_server) = create_proxy::<flab::SessionMarker>();
    let denied_res = proxy
        .open_session(
            &run_context(),
            flab::SessionMode::ReadOnly,
            &expectations(&description),
            &[flab::AccessRule {
                resource: 0,
                offset: REG_FIFO_DATA,
                width: 4,
                class: flab::AccessClass::ReadOnce,
            }],
            denied_server,
        )
        .await?;
    assert_eq!(denied_res, Err(flab::OpenSessionError::RejectedAllowlist));

    // 1b. Read-only state slot (STATE_BUSY_FLAG) on state0 (resource 2) rejects write allowlist.
    let (_denied_state_session, denied_state_server) = create_proxy::<flab::SessionMarker>();
    let denied_state_res = proxy
        .open_session(
            &run_context(),
            flab::SessionMode::Mutating,
            &expectations(&description),
            &[flab::AccessRule {
                resource: 2,
                offset: STATE_BUSY_FLAG,
                width: 4,
                class: flab::AccessClass::Write,
            }],
            denied_state_server,
        )
        .await?;
    assert_eq!(denied_state_res, Err(flab::OpenSessionError::RejectedAllowlist));

    // 2. Read-only session reads DEVICE_ID and STATUS without quiescing.
    let (ro_session, ro_server) = create_proxy::<flab::SessionMarker>();
    proxy
        .open_session(
            &run_context(),
            flab::SessionMode::ReadOnly,
            &expectations(&description),
            &[
                flab::AccessRule {
                    resource: 0,
                    offset: REG_DEVICE_ID,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                },
                flab::AccessRule {
                    resource: 0,
                    offset: REG_STATUS,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                },
                flab::AccessRule {
                    resource: 1,
                    offset: 0,
                    width: 0,
                    class: flab::AccessClass::Interrupt,
                },
            ],
            ro_server,
        )
        .await?
        .expect("read-only session should open");

    let (dev_id, _, _) = ro_session.read32(0, REG_DEVICE_ID).await?.expect("read DEVICE_ID");
    assert_eq!(dev_id, SAMPLE_DEVICE_ID);

    let (status_ro, _, _) = ro_session.read32(0, REG_STATUS).await?.expect("read STATUS");
    assert_eq!(status_ro & STATUS_READY, STATUS_READY);
    assert_eq!(status_ro & STATUS_QUIESCED, 0);

    // 3. WaitForInterrupt receives tapped ISR event from lab_sample_driver.
    let (irq_res, irq_seq, _, _, _) =
        ro_session.wait_for_interrupt(1, 0, 1_000_000_000).await?.expect("WaitForInterrupt");
    assert_eq!(irq_res, 1);
    assert!(irq_seq >= 1);

    drop(ro_session);

    // 4. Mutating session engages quiesce hook (sets STATUS_QUIESCED), writes REG_CONTROL,
    // and exercises state0 knobs + trigger in a single boot.
    let (mut_session, mut_server) = create_proxy::<flab::SessionMarker>();
    proxy
        .open_session(
            &run_context(),
            flab::SessionMode::Mutating,
            &expectations(&description),
            &[
                flab::AccessRule {
                    resource: 0,
                    offset: REG_STATUS,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                },
                flab::AccessRule {
                    resource: 0,
                    offset: REG_CONTROL,
                    width: 4,
                    class: flab::AccessClass::Write,
                },
                flab::AccessRule {
                    resource: 2,
                    offset: STATE_BUSY_FLAG,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                },
                flab::AccessRule {
                    resource: 2,
                    offset: STATE_INVARIANT_VIOLATIONS,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                },
                flab::AccessRule {
                    resource: 2,
                    offset: KNOB_RACE_DELAY_US,
                    width: 4,
                    class: flab::AccessClass::Write,
                },
                flab::AccessRule {
                    resource: 2,
                    offset: KNOB_FIX_ENABLED,
                    width: 4,
                    class: flab::AccessClass::Write,
                },
                flab::AccessRule {
                    resource: 2,
                    offset: TRIGGER_CONCURRENT_OP,
                    width: 4,
                    class: flab::AccessClass::Write,
                },
            ],
            mut_server,
        )
        .await?
        .expect("mutating session should open");

    let (status_mut, _, _) =
        mut_session.read32(0, REG_STATUS).await?.expect("read STATUS while mutating");
    assert_eq!(status_mut & STATUS_QUIESCED, STATUS_QUIESCED);

    let (readback_val, _, _) = mut_session
        .write32(0, REG_CONTROL, 0xABCD_1234, 0xFFFF_FFFF, None, true)
        .await?
        .expect("write32 REG_CONTROL");
    assert_eq!(readback_val, 0xABCD_1234);

    // 4b. Reproduce race condition with KNOB_RACE_DELAY_US = 50, KNOB_FIX_ENABLED = 0:
    let (delay_rb, _, _) = mut_session
        .write32(2, KNOB_RACE_DELAY_US, 50, 0xFFFF_FFFF, None, true)
        .await?
        .expect("write KNOB_RACE_DELAY_US");
    assert_eq!(delay_rb, 50);
    let (trig_bug_rb, _, _) = mut_session
        .write32(2, TRIGGER_CONCURRENT_OP, 1, 0xFFFF_FFFF, None, true)
        .await?
        .expect("trigger concurrent op with fix disabled");
    assert_eq!(trig_bug_rb, 1);
    let (violations_before, _, _) = mut_session
        .read32(2, STATE_INVARIANT_VIOLATIONS)
        .await?
        .expect("read STATE_INVARIANT_VIOLATIONS");
    assert_eq!(violations_before, 1);

    // 4c. Enable fix via KNOB_FIX_ENABLED = 1 and re-trigger: no new violation!
    let (fix_rb, _, _) = mut_session
        .write32(2, KNOB_FIX_ENABLED, 1, 0xFFFF_FFFF, None, true)
        .await?
        .expect("write KNOB_FIX_ENABLED");
    assert_eq!(fix_rb, 1);
    let (trig_fixed_rb, _, _) = mut_session
        .write32(2, TRIGGER_CONCURRENT_OP, 1, 0xFFFF_FFFF, None, true)
        .await?
        .expect("trigger concurrent op with fix enabled");
    assert_eq!(trig_fixed_rb, 0);
    let (violations_after, _, _) = mut_session
        .read32(2, STATE_INVARIANT_VIOLATIONS)
        .await?
        .expect("read STATE_INVARIANT_VIOLATIONS after fix");
    assert_eq!(violations_after, 1);
    let (busy_val, _, _) =
        mut_session.read32(2, STATE_BUSY_FLAG).await?.expect("read STATE_BUSY_FLAG");
    assert_eq!(busy_val, 0);

    drop(mut_session);

    // 5. After closing mutating session, STATUS_QUIESCED is cleared and audit records exist.
    let (verify_session, verify_server) = create_proxy::<flab::SessionMarker>();
    proxy
        .open_session(
            &run_context(),
            flab::SessionMode::ReadOnly,
            &expectations(&description),
            &[flab::AccessRule {
                resource: 0,
                offset: REG_STATUS,
                width: 4,
                class: flab::AccessClass::ReadOnce,
            }],
            verify_server,
        )
        .await?
        .expect("verify session should open");

    let mut status_after = STATUS_QUIESCED;
    for _ in 0..100 {
        let (val, _, _) =
            verify_session.read32(0, REG_STATUS).await?.expect("read STATUS after close");
        status_after = val;
        if status_after & STATUS_QUIESCED == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(status_after & STATUS_QUIESCED, 0);

    let mut ops = Vec::new();
    let mut cursor = 0;
    for _ in 0..16 {
        let (entries, _, _, next_cursor) = verify_session.read_audit(cursor, 64).await?;
        if entries.is_empty() {
            break;
        }
        ops.extend(entries.into_iter().filter_map(|r| r.operation));
        if next_cursor <= cursor {
            break;
        }
        cursor = next_cursor;
    }
    assert!(ops.iter().any(|op| op == "quiesce_engaged"));
    assert!(ops.iter().any(|op| op == "quiesce_released"));
    assert!(ops.iter().any(|op| op == "interrupt_tap"));

    Ok(())
}
