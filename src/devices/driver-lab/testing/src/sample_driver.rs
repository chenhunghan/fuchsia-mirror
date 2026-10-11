// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reference synthetic platform driver demonstrating `driver_lab_rust` Phase 2
//! in-situ integration (Spec Phase 2 Section 3.1 / CS28).
//!
//! Binds to the `sample-device` node added by `lab_root`, maps its platform
//! MMIO and interrupt for normal operation, shares a duplicated VMO handle with
//! [`DriverLabBuilder`], registers a cooperative quiesce hook, taps its own
//! interrupt service routine via [`EmbeddedLabServer::notify_interrupt_with_timestamp`],
//! and publishes `fuchsia.driver.lab.Service` alongside normal execution.

use driver_lab_rust::embedded::{DriverLabBuilder, EmbeddedLabServer, StateBank};
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_next_fuchsia_hardware_platform_device as fpdev;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use log::{info, warn};
use std::sync::{Arc, Mutex};

/// Read-only device identification register (`0x00`).
pub const REG_DEVICE_ID: u64 = 0x00;
/// Expected value of [`REG_DEVICE_ID`] (`"SAMP"`).
pub const SAMPLE_DEVICE_ID: u32 = 0x5341_4D50;

/// Status register (`0x04`): bit 0 (`0x01`) = ready, bit 4 (`0x10`) = quiesced.
pub const REG_STATUS: u64 = 0x04;
/// Bit set in [`REG_STATUS`] when the driver is initialized and ready.
pub const STATUS_READY: u32 = 0x01;
/// Bit set in [`REG_STATUS`] while the driver's background loop is quiesced.
pub const STATUS_QUIESCED: u32 = 0x10;

/// Writable control register (`0x08`).
pub const REG_CONTROL: u64 = 0x08;

/// Interrupt counter register (`0x0C`) updated by the driver ISR when not quiesced.
pub const REG_IRQ_COUNT: u64 = 0x0C;

/// Destructive clear-on-read FIFO register (`0x40`) marked `hard_denied` in the ceiling.
pub const REG_FIFO_DATA: u64 = 0x40;
/// Seeded value in [`REG_FIFO_DATA`].
pub const FIFO_SEED_VALUE: u32 = 0xCAFE_BABE;

/// Logical size of the `"state0"` software state bank (`0x20` bytes).
pub const STATE_BANK_SIZE: u64 = 0x20;
/// Read-only state slot (`0x00`): simulated busy/in-flight state flag.
pub const STATE_BUSY_FLAG: u64 = 0x00;
/// Read-only state slot (`0x04`): count of observed concurrency invariant violations.
pub const STATE_INVARIANT_VIOLATIONS: u64 = 0x04;
/// Writable runtime knob (`0x08`): simulated race-window delay in microseconds.
pub const KNOB_RACE_DELAY_US: u64 = 0x08;
/// Writable runtime knob (`0x0C`): candidate fix toggle (`0` = unguarded, `1` = guarded).
pub const KNOB_FIX_ENABLED: u64 = 0x0C;
/// Writable trigger offset (`0x10`): triggers a simulated concurrent state transition.
pub const TRIGGER_CONCURRENT_OP: u64 = 0x10;

struct LabSampleDriver {
    _node: Node,
    _scope: fasync::Scope,
    _mapping: Arc<mapped_vmo::Mapping>,
    _lab: EmbeddedLabServer,
    irq_canceller: Mutex<Option<zx::Interrupt>>,
}

driver_register!(LabSampleDriver);

fn connect_pdev(
    context: &DriverContext,
    instance: &str,
) -> Option<fidl_next::Client<fpdev::Device>> {
    let service = context
        .incoming
        .service::<fdf_component::ServiceInstance<fpdev::Service>>()
        .instance(instance)
        .connect_next()
        .ok()?;
    let (client_end, server_end) = fidl_next::fuchsia::create_channel();
    service.device(server_end).ok()?;
    Some(client_end.spawn())
}

fn read_u32(mapping: &mapped_vmo::Mapping, offset: u64) -> u32 {
    let mut bytes = [0u8; 4];
    mapping.read_at(offset as usize, &mut bytes);
    u32::from_le_bytes(bytes)
}

fn write_u32(mapping: &mapped_vmo::Mapping, offset: u64, value: u32) {
    mapping.write_at(offset as usize, &value.to_le_bytes());
}

impl Driver for LabSampleDriver {
    const NAME: &str = "lab_sample_driver";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;

        let mut connected_pdev = None;
        for instance in ["sample-pdev", "pdev", "default"] {
            if let Some(pdev) = connect_pdev(&context, instance) {
                if let Ok(Ok(_)) = pdev.get_node_device_info().await {
                    connected_pdev = Some(pdev);
                    break;
                }
            }
        }
        let pdev = connected_pdev.ok_or_else(|| {
            warn!("lab_sample_driver failed to connect to platform device");
            DriverError::Status(zx::Status::NOT_FOUND)
        })?;

        let mmio = pdev
            .get_mmio_by_id(0)
            .await
            .map_err(|_| DriverError::Status(zx::Status::IO))?
            .map_err(DriverError::Status)?;
        let vmo = mmio.vmo.ok_or(DriverError::Status(zx::Status::INVALID_ARGS))?;
        let offset = mmio.offset.unwrap_or(0) as usize;
        let size = mmio.size.unwrap_or(0x1000) as usize;

        let irq = pdev
            .get_interrupt_by_id(0, 0)
            .await
            .map_err(|_| DriverError::Status(zx::Status::IO))?
            .map_err(DriverError::Status)?
            .irq;

        // Map the VMO for the driver's own state machine and seed registers.
        let mapping = Arc::new(
            mapped_vmo::Mapping::create_from_vmo(
                &vmo,
                size,
                zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
            )
            .map_err(DriverError::Status)?,
        );
        write_u32(&mapping, REG_DEVICE_ID, SAMPLE_DEVICE_ID);
        write_u32(&mapping, REG_STATUS, STATUS_READY);
        write_u32(&mapping, REG_CONTROL, 0);
        write_u32(&mapping, REG_IRQ_COUNT, 0);
        write_u32(&mapping, REG_FIFO_DATA, FIFO_SEED_VALUE);

        // Configure the embedded driver-lab server by duplicating the VMO handle.
        let mut builder = DriverLabBuilder::new(Self::NAME).with_enabled(true);
        let mmio_id =
            builder.with_mmio("mmio0", &vmo, offset, size).map_err(DriverError::Status)?;
        builder.with_writable_registers(mmio_id, vec![REG_CONTROL]);
        builder.with_hard_denied_ranges(mmio_id, vec![(REG_FIFO_DATA, REG_FIFO_DATA + 4)]);
        let irq_id = builder.with_interrupt("irq0");

        let mut state_bank = StateBank::new(STATE_BANK_SIZE);
        let busy_slot = state_bank.define_state_slot(STATE_BUSY_FLAG, 0);
        let violations_slot = state_bank.define_state_slot(STATE_INVARIANT_VIOLATIONS, 0);
        let race_delay_knob = state_bank.define_knob(KNOB_RACE_DELAY_US, 0);
        let fix_enabled_knob = state_bank.define_knob(KNOB_FIX_ENABLED, 0);
        let busy_for_trigger = busy_slot.clone();
        let violations_for_trigger = violations_slot.clone();
        let delay_for_trigger = race_delay_knob.clone();
        let fix_for_trigger = fix_enabled_knob.clone();
        state_bank.define_trigger(TRIGGER_CONCURRENT_OP, move |arg| {
            busy_for_trigger.store(arg & 1);
            let violated = delay_for_trigger.load() > 0 && fix_for_trigger.load() == 0;
            if violated {
                violations_for_trigger.fetch_add(1);
                busy_for_trigger.store(0);
                Ok(1)
            } else {
                busy_for_trigger.store(0);
                Ok(0)
            }
        });
        builder.with_state_bank("state0", state_bank);

        let hook_mapping = mapping.clone();
        builder.with_quiesce_hook(move |paused| {
            let current = read_u32(&hook_mapping, REG_STATUS);
            let next = if paused { current | STATUS_QUIESCED } else { current & !STATUS_QUIESCED };
            write_u32(&hook_mapping, REG_STATUS, next);
        });

        let lab = builder.build().map_err(|_| DriverError::Status(zx::Status::INTERNAL))?;

        // Retain exclusive driver ownership of the zx::Interrupt and tap events into driver-lab.
        let irq_canceller = irq.duplicate_handle(zx::Rights::SAME_RIGHTS).ok();
        let isr_mapping = mapping.clone();
        let isr_lab = lab.clone();
        std::thread::Builder::new()
            .name("lab-sample-driver-isr".to_string())
            .spawn(move || {
                loop {
                    match irq.wait() {
                        Ok(timestamp) => {
                            if !isr_lab.is_quiesced() {
                                let count = read_u32(&isr_mapping, REG_IRQ_COUNT);
                                write_u32(&isr_mapping, REG_IRQ_COUNT, count.wrapping_add(1));
                            }
                            let _ = isr_lab
                                .notify_interrupt_with_timestamp(irq_id, timestamp.into_nanos());
                            let _ = irq.ack();
                        }
                        Err(zx::Status::CANCELED) => break,
                        Err(e) => {
                            warn!("lab_sample_driver IRQ wait failed: {e:?}");
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn lab_sample_driver ISR thread");

        let scope = fasync::Scope::new_with_name(Self::NAME);
        let mut outgoing = ServiceFs::new();
        lab.publish(&mut outgoing, scope.to_handle());
        context.serve_outgoing(&mut outgoing)?;
        scope.spawn(async move {
            outgoing.collect::<()>().await;
        });

        info!("lab_sample_driver started with embedded fuchsia.driver.lab.Service");
        Ok(Self {
            _node: node,
            _scope: scope,
            _mapping: mapping,
            _lab: lab,
            irq_canceller: Mutex::new(irq_canceller),
        })
    }

    async fn stop(&self) {
        if let Ok(mut guard) = self.irq_canceller.lock() {
            if let Some(irq) = guard.take() {
                let _ = irq.destroy();
            }
        }
    }
}
