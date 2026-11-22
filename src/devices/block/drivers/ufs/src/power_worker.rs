// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Serialized background worker for UFS system suspend and resume commands.
//!
//! [`PowerWorker`] sends `SystemSuspend`, `SystemResume`, and `Shutdown` commands over an
//! unbounded MPSC channel to a single background [`PowerWorkerTask`]. The task acquires
//! [`PowerControl::lock`] for each command so power transitions never overlap.

use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot::{self, Sender as OneshotSender};
use mmio::Mmio;

use crate::power::{PowerControl, PowerHooks};

enum PowerCommand {
    SystemSuspend(OneshotSender<Result<(), zx::Status>>),
    SystemResume(OneshotSender<()>),
    Shutdown(OneshotSender<()>),
}

/// Handle to the serialized background power management worker task.
pub struct PowerWorker {
    tx: UnboundedSender<PowerCommand>,
}

impl fmt::Debug for PowerWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PowerWorker").finish_non_exhaustive()
    }
}

impl PowerWorker {
    pub fn spawn<M, H>(scope: &fasync::Scope, power: Arc<PowerControl<M, H>>) -> Self
    where
        M: Mmio + Send + Sync + 'static,
        H: PowerHooks + 'static,
    {
        let (tx, power_rx) = mpsc::unbounded();
        scope.spawn(async move {
            PowerWorkerTask { power, power_rx }.run().await;
        });
        Self { tx }
    }

    pub async fn suspend(&self) -> Result<(), zx::Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.tx.unbounded_send(PowerCommand::SystemSuspend(done_tx)).map_err(|_| zx::Status::IO)?;
        done_rx.await.map_err(|_| zx::Status::CANCELED)?
    }

    pub async fn resume(&self) -> Result<(), zx::Status> {
        let (done_tx, done_rx) = oneshot::channel();
        self.tx.unbounded_send(PowerCommand::SystemResume(done_tx)).map_err(|_| zx::Status::IO)?;
        done_rx.await.map_err(|_| zx::Status::CANCELED)
    }

    pub async fn shutdown(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.tx.unbounded_send(PowerCommand::Shutdown(done_tx)).is_ok() {
            let _ = done_rx.await;
        }
    }
}

struct PowerWorkerTask<M: Mmio, H: PowerHooks> {
    power: Arc<PowerControl<M, H>>,
    power_rx: UnboundedReceiver<PowerCommand>,
}

impl<M, H> PowerWorkerTask<M, H>
where
    M: Mmio + Send + Sync + 'static,
    H: PowerHooks,
{
    async fn handle_command(&mut self, cmd: PowerCommand) -> ControlFlow<()> {
        match cmd {
            PowerCommand::Shutdown(done_tx) => {
                let _ = done_tx.send(());
                ControlFlow::Break(())
            }
            PowerCommand::SystemSuspend(done_tx) => {
                let res = self.power.lock().await.suspend().await;
                let _ = done_tx.send(res.map_err(zx::Status::from));
                ControlFlow::Continue(())
            }
            PowerCommand::SystemResume(done_tx) => {
                self.power.lock().await.resume().await;
                let _ = done_tx.send(());
                ControlFlow::Continue(())
            }
        }
    }

    async fn run(mut self) {
        while let Some(cmd) = self.power_rx.next().await {
            if self.handle_command(cmd).await.is_break() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_hardware_rpmb as frpmb;
    use fidl_fuchsia_hardware_ufs as fufs;
    use fidl_fuchsia_hardware_ufs_phy as fufs_phy;
    use fidl_fuchsia_mem::Range;
    use fuchsia_inspect::{self as inspect, reader as inspect_reader};
    use futures::{TryStreamExt, poll};
    use mmio::{ReadableRegister, Register};
    use zerocopy::IntoBytes;
    use zerocopy::byteorder::big_endian::U32;

    use super::*;
    use crate::constants::{AUTO_HIBERNATE_TIMER_US, RPMB_FRAME_SIZE, RPMB_LUN};
    use crate::controller::RegisterAccess;
    use crate::device_manager::DeviceManager;
    use crate::inspect::LinkState;
    use crate::power::{DevicePowerState, PowerState};
    use crate::query::{Flag, QueryOpcode, QueryUpiu};
    use crate::quirks::HostQuirks;
    use crate::registers::{
        AutoHibernateIdleTimerReg, CapabilityReg, HostControllerEnableReg, PowerModeStatus,
    };
    use crate::rpmb::UfsRpmbServer;
    use crate::scsi::{ScsiOpcode, StartStopPowerCondition};
    use crate::server::UfsServer;
    use crate::testing::fake_ufs::{
        CAP_AUTO_HIBERNATE_SUPPORT, HCS_ACTIVE_LINK, hcs_power_mode_status, uic_hardware,
    };
    use crate::testing::{
        ADMIN_SLOT, OCS_SUCCESS, PAGE_SIZE, answer_system_suspend, complete_power_down_reinit,
        create_test_processor_with_power_domain, fake_utp, io_callback, scsi_response,
        ufs_histogram_samples, wait_async, wait_for_any_doorbell,
    };
    use crate::transfer_request_processor::DataTransfer;
    use crate::upiu::{CommandUpiu, UpiuTransactionCode};
    use crate::{Bus, PlatformPower};

    fn spawn_test_power_worker<H: PowerHooks + 'static>(
        scope: &fasync::Scope,
        power: Arc<PowerControl<crate::testing::fake_ufs::FakeMmio, H>>,
    ) -> PowerWorker {
        PowerWorker::spawn(scope, power)
    }

    #[fuchsia::test]
    async fn test_power_worker_suspend_and_resume_from_power_down_restores_clients() {
        let phy_powered = Arc::new(AtomicBool::new(true));
        let (mmio_vmo, processor) =
            create_test_processor_with_power_domain(Arc::clone(&phy_powered));
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        });
        processor.controller().configure_auto_hibernate(AUTO_HIBERNATE_TIMER_US);
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_ACTIVE_LINK;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);

        let phy_powered_for_phy = Arc::clone(&phy_powered);
        let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
        let (entered_phy_active_tx, entered_phy_active_rx) = oneshot::channel();
        let (allow_phy_active_tx, allow_phy_active_rx) = oneshot::channel();
        let phy_task = fasync::Task::spawn(async move {
            let mut entered_tx = Some(entered_phy_active_tx);
            let mut allow_rx = Some(allow_phy_active_rx);
            while let Some(req) = phy_stream.try_next().await.unwrap() {
                match req {
                    fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                        if state == fufs_phy::PowerState::Active {
                            if let Some(tx) = entered_tx.take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = allow_rx.take() {
                                let _ = rx.await;
                            }
                        }
                        phy_powered_for_phy
                            .store(state == fufs_phy::PowerState::Active, Ordering::Release);
                        responder.send(Ok(())).unwrap();
                    }
                    fufs_phy::UfsPhyRequest::Init { responder, .. } => {
                        responder.send(Ok(())).unwrap();
                    }
                    fufs_phy::UfsPhyRequest::PrePowerModeChange { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("unexpected UfsPhy request: {other:?}"),
                }
            }
        });

        let hardware = thread::spawn(move || {
            // 0. Answer RPMB Unit Descriptor read during UfsRpmbServer::new.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT), "RPMB descriptor query not issued");
            let mut q: QueryUpiu = utp.request(ADMIN_SLOT);
            q.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
            q.header.response = 1;
            utp.complete(ADMIN_SLOT, q.as_bytes(), OCS_SUCCESS);

            // 1. Suspend to PowerDown.
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);

            // 2. Full PowerDown re-initialization once PHY Active is released.
            complete_power_down_reinit(&mut utp, &mut uic_regs);

            // 3. Service the 3 paused client requests released once PowerState::Active is reached.
            let mut seen_block = false;
            let mut seen_rpmb = false;
            let mut seen_ufs_query = false;
            for _ in 0..3 {
                let slot = wait_for_any_doorbell(&utp);
                if slot == ADMIN_SLOT {
                    let mut q: QueryUpiu = utp.request(ADMIN_SLOT);
                    assert_eq!(q.opcode, QueryOpcode::ReadFlag as u8);
                    assert_eq!(q.idn, Flag::DeviceInit as u8);
                    q.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
                    q.value = U32::new(1);
                    utp.complete(ADMIN_SLOT, q.as_bytes(), OCS_SUCCESS);
                    seen_ufs_query = true;
                } else {
                    let cmd: CommandUpiu = utp.request(slot);
                    match (cmd.header.lun, cmd.cdb[0]) {
                        (0, op) if op == ScsiOpcode::Write10 as u8 => seen_block = true,
                        (RPMB_LUN, op) if op == ScsiOpcode::SecurityProtocolOut as u8 => {
                            seen_rpmb = true;
                        }
                        other => panic!("unexpected client SCSI command: {other:?}"),
                    }
                    utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
                }
            }
            assert!(seen_block && seen_rpmb && seen_ufs_query);
        });

        let scope = fasync::Scope::new();
        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.auto_hibernate_timer_us = AUTO_HIBERNATE_TIMER_US;
        mgr.init_inspect(&ufs_node);
        mgr.power_down_supported = true;
        inspector.root().record(ufs_node);

        let rpmb_server = Arc::new(UfsRpmbServer::new(&mgr, Arc::clone(&processor)).await.unwrap());
        let (rpmb_proxy, rpmb_stream) = create_proxy_and_stream::<frpmb::RpmbMarker>();
        scope.spawn(rpmb_server.serve(rpmb_stream));

        let ufs_server = Arc::new(UfsServer::new(Arc::clone(&processor), Arc::clone(mgr.flags())));
        let (ufs_proxy, ufs_stream) = create_proxy_and_stream::<fufs::UfsMarker>();
        scope.spawn(async move {
            let _ = ufs_server.serve(ufs_stream).await;
        });

        let hooks = PlatformPower::new(
            Arc::clone(processor.controller()),
            Bus::Pdev { interconnect_path: None },
            Some(phy_proxy.clone()),
            scope.to_handle(),
        );
        let power =
            Arc::new(PowerControl::new(mgr, Arc::clone(&processor), Some(phy_proxy), hooks));
        let worker = spawn_test_power_worker(&scope, Arc::clone(&power));

        assert_eq!(worker.suspend().await, Ok(()));
        {
            let p = power.lock().await;
            assert_eq!(p.power().mode, PowerState::PowerDown);
            assert_eq!(p.register_access(), RegisterAccess::Off);
            assert!(p.power().io_paused);
        }

        // Queue Block Write(10), fuchsia.hardware.ufs ReadFlag, and fuchsia.hardware.rpmb Request
        // while suspended; all three must park on paused client IO admission.
        let (block_completion, block_rx) = io_callback();
        let block_submitter = {
            let processor = Arc::clone(&processor);
            let write_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
            thread::spawn(move || {
                processor.submit_io(
                    0,
                    ScsiOpcode::build_write_10_cdb(0, 1),
                    Some(DataTransfer::write(write_vmo, 0, PAGE_SIZE as u32)),
                    block_completion,
                );
            })
        };
        let mut block_fut = pin!(block_rx);
        let flag = fufs::Flag { type_: Some(fufs::FlagType::DeviceInit), ..Default::default() };
        let mut ufs_fut = pin!(ufs_proxy.read_flag(&flag));
        let rpmb_vmo = zx::Vmo::create(RPMB_FRAME_SIZE).unwrap();
        let mut rpmb_fut = pin!(rpmb_proxy.request(frpmb::Request {
            tx_frames: Range { vmo: rpmb_vmo, offset: 0, size: RPMB_FRAME_SIZE },
            rx_frames: None,
        }));
        assert!(poll!(&mut block_fut).is_pending());
        assert!(poll!(&mut ufs_fut).is_pending());
        assert!(poll!(&mut rpmb_fut).is_pending());
        assert!(
            wait_async(|| processor.requests().blocked_on_admission() == 3).await,
            "expected block, Ufs, and Rpmb clients to park on client IO admission"
        );

        // `PowerWorker::resume()` awaits hardware bring-up before returning `Ok(())`.
        let mut resume_fut = pin!(worker.resume());
        assert!(poll!(&mut resume_fut).is_pending());
        entered_phy_active_rx.await.unwrap();
        assert!(poll!(&mut resume_fut).is_pending());
        assert!(processor.requests().power().io_paused);
        assert_data_tree!(inspector, root: {
            ufs: contains {
                power_suspended: true,
                controller: contains {
                    LinkState: LinkState::Off as u64,
                    background_operations: contains {
                        is_background_op_enabled: false,
                    },
                },
            }
        });

        // Release PHY power-up; hardware re-init completes, `resume()` returns, and clients finish.
        allow_phy_active_tx.send(()).unwrap();
        assert_eq!(resume_fut.await, Ok(()));
        assert_eq!(block_fut.await.unwrap().io_status(), Ok(()));
        block_submitter.join().unwrap();
        assert_eq!(ufs_fut.await.unwrap(), Ok(1));
        assert_eq!(rpmb_fut.await.unwrap(), Ok(()));

        assert_eq!(power.lock().await.power(), DevicePowerState::ACTIVE);
        assert_ne!(
            processor
                .controller()
                .with_mmio_unchecked(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            0
        );
        assert_data_tree!(inspector, root: {
            ufs: contains {
                power_suspended: false,
                controller: contains {
                    background_operations: contains {
                        is_background_op_enabled: true,
                    },
                },
            }
        });
        let hierarchy = inspect_reader::read(&inspector).await.unwrap();
        assert_eq!(ufs_histogram_samples(&hierarchy, "wake_latency_us"), 1);

        worker.shutdown().await;
        drop(ufs_proxy);
        drop(rpmb_proxy);
        {
            let mut guard = power.lock().await;
            let _ = guard.quiesce().await;
        }
        assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
        scope.cancel().await;
        drop(power);
        phy_task.await;
        hardware.join().unwrap();
    }
}
