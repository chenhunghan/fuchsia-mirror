// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(clippy::await_holding_lock)]
#![cfg_attr(test, recursion_limit = "256")]

//! Universal Flash Storage (UFS) host controller driver entry point and node topology.
//!
//! The same driver binary backs two components: `ufs-rust-pdev` (platform device composite with
//! `pdev`, `phy` and optional `ufs-interconnect` parents) and `ufs-rust-pci` (PCI composite with
//! a `pci` parent).
//!
//! # Module Layout
//!
//! - `controller`: `UfsController` owns the register block. No other module holds the MMIO
//!   region, so every register read and write in the driver goes through a `UfsController`
//!   method. It also runs UIC (link) commands and keeps `RegisterAccess`, which records whether
//!   the registers may be touched at all.
//! - `request_list`: the transfer slots, their DMA descriptors, and the driver's power state.
//!   Data only: it takes no lock and touches no register.
//! - `transfer_request_processor`: `TransferRequestProcessor` owns the request list behind a
//!   lock. It is the only code that reserves slots, rings the doorbell, and completes requests,
//!   and it decides whether a client request is admitted or must wait.
//! - `device_manager`: `DeviceManager` knows the device: initialization, descriptors, flags,
//!   attributes, and link configuration. It runs query, SCSI, and UIC operations through the
//!   processor and controller, and never holds MMIO registers.
//! - `power`, `power_worker`: `PowerControl` in `power` owns the `DeviceManager` and runs every
//!   power transition. `PowerWorker` in `power_worker` is the background task that asks for
//!   them.
//! - `block_device`, `server`, `rpmb`: where clients come in (block IO per logical unit, the
//!   `fuchsia.hardware.ufs` protocol, and RPMB). They submit through the processor.
//!
//! # Threads and Tasks
//!
//! - Block server session and mapper threads submit IO synchronously. They block while no slot
//!   is free or while client IO is paused.
//! - One interrupt thread (`ufs-irq-0`) waits on the controller interrupt. It calls
//!   `TransferRequestProcessor::service_interrupt`, which reads and clears the interrupt status
//!   and completes finished requests on that thread.
//! - On the driver's async executor: the IO timeout task, the power worker, the FIDL servers,
//!   and the stream that serves the PHY driver's DME requests.
//!
//! # Register Access Rules
//!
//! The registers cannot be reached while the PHY has gated the controller's clocks
//! (`HibernateSleep`) or while the host controller is powered down. Each kind of caller is kept
//! off them in its own way:
//!
//! - `RegisterAccess` (`Reachable`, `Blocked`, `Off`, `OffUnconfirmed`) lives under the
//!   controller lock. The register accessors assert `Reachable` in debug builds. The operations
//!   that block or restore access for a power transition live on `PowerGuard`.
//! - Client requests: a transition pauses client IO and drains in-flight transfers before it
//!   touches the hardware (`IoGuard`). A request that arrives meanwhile waits. It is never
//!   failed because of a power transition: a failed write makes the filesystem read-only.
//! - The interrupt thread: a transition pauses the `InterruptGate` before the registers go away.
//!   The thread parks in `InterruptGate::enter` and services the interrupt after resume, so no
//!   interrupt is dropped and no handler runs without clocks.
//! - UIC commands and the PHY's DME requests fail with an error while access is not
//!   `Reachable`. The timeout task has nothing to do then, because active requests drained
//!   first and a quarantined admin slot prevents suspend.
//!
//! DMA follows the same state. A request's pages are unpinned when the controller reports the
//! request complete. Requests the controller still owns when it is stopped stay pinned until
//! `HCE` reads 0. If that cannot be confirmed (`OffUnconfirmed`), their pages are quarantined
//! and never unpinned.
//!
//! # Guards and Proofs
//!
//! - `PowerControl::lock()` returns a `PowerGuard`: the right to run a power transition or to
//!   rely on the power state. One holder at a time. Operations that block, restore, or power off
//!   register access, plus power-state updates, live on `PowerGuard`.
//! - `PowerGuard::pause_client_io()` returns an `IoGuard`: client IO is paused while it lives,
//!   and dropping it sets admission to what the recorded state calls for. Operations that require
//!   client IO to be paused live on it.
//! - `TransferRequestProcessor::requests()` returns `Requests`, a locked view of the request
//!   list. One decision reads one view.
//! - `CountGuard` counts a waiter or a client transaction for as long as it lives.
//!   `ClientTransactionGuard` holds one across a multi-command client sequence, so a transition
//!   does not start between its commands. Client queries and SCSI commands are sent through it,
//!   so a client path cannot skip admission.
//! - `ReservedSlot` owns a reserved transfer slot until it is scheduled. `AdminSlotGuard` adds
//!   the lock that allows one administrative command at a time.
//! - `UicBusyGuard` owns the UIC command registers for one command. `AutoHibernateHoldGuard`
//!   keeps hardware auto-hibernate off across a multi-command link sequence.
//! - `InterruptGate::enter()` returns an `InterruptGuard` for one interrupt pass. `pause()`
//!   waits until no pass is in progress.
//!
//! # Lock Order
//!
//! Take locks in this order, and never take an earlier lock while holding a later one.
//!
//! 1. `PowerControl` or `UfsServer::write_lock` (async, never held together): power state
//!    transitions, or one client flag/attribute write and its cache update.
//! 2. `admin_lock` (async): one command at a time on the admin slot.
//! 3. `request_list` (sync): transfer slots, their DMA descriptors, and the device power state.
//! 4. `controller.state` (sync): host controller registers.
//!
//! Never call `InterruptGate::pause()` while holding lock 3 or 4: it waits for the interrupt
//! thread, which takes both. The RPMB `staging_vmo` lock (async) comes before `request_list`
//! and is never held with 1 or 2. The interrupt worker lock is always taken alone. Sync locks
//! are also taken on the IO and interrupt threads, so never hold one across `.await`.
//!
//! Spec references in this driver are to UFS 4.0 (JESD220F) and UFSHCI 4.0 (JESD223E).

// JEDEC UFS, UFSHCI, and SCSI protocol definitions.
pub mod constants;
pub mod descriptors;
pub mod query;
pub mod registers;
pub mod scsi;
pub mod uic;
pub mod upiu;

mod block_device;
mod controller;
mod device_manager;
mod dma_buffer;
mod inspect;
mod power;
mod power_worker;
mod quirks;
mod request_list;
mod rpmb;
mod server;
mod transfer;
mod transfer_request_processor;

use crate::block_device::{BlockServerCompleter, LunBlockServer, UfsBlockDevice};
use crate::constants::{
    AUTO_HIBERNATE_TIMER_US, COMMAND_TIMEOUT, DRAIN_INFLIGHT_MARGIN,
    INTERCONNECT_AVG_BANDWIDTH_BPS, INTERCONNECT_INSTANCE, INTERCONNECT_PEAK_BANDWIDTH_BPS,
    INTERCONNECT_TAG_UFS, IRQ_SCHEDULER_ROLE, LU_ENABLED, MIN_MMIO_SIZE, PCI_INSTANCE,
    PDEV_INSTANCE, PHY_INSTANCE, POWER_LEVEL_ON, RPMB_LUN, WORKER_SCHEDULER_ROLE,
};
use crate::controller::{RegisterAccess, UfsController};
use crate::device_manager::{DeviceManager, DiscoveredUnit};
use crate::power::{PowerControl, PowerGuard, PowerHooks, PowerStep};
use crate::power_worker::PowerWorker;
use crate::query::{Attribute, Flag};
use crate::quirks::HostQuirks;
use crate::rpmb::UfsRpmbServer;
use crate::server::UfsServer;
use crate::transfer_request_processor::TransferRequestProcessor;
use fdf_component::{Driver, DriverContext, DriverError, Node, ServiceOffer, driver_register};
use fdf_power::{Suspendable, SuspendableDriver};
use fidl::endpoints::{ClientEnd, RequestStream, ServerEnd, create_endpoints};
use fidl_fuchsia_device_fs as fdevfs;
use fidl_fuchsia_driver_framework::{DevfsAddArgs, NodeAddArgs, NodeControllerMarker, NodeMarker};
use fidl_fuchsia_hardware_block_volume as fvolume;
use fidl_fuchsia_hardware_interconnect as finterconnect;
use fidl_fuchsia_hardware_pci as fpci;
use fidl_fuchsia_hardware_platform_device as fpdev;
use fidl_fuchsia_hardware_rpmb as frpmb;
use fidl_fuchsia_hardware_ufs as fufs;
use fidl_fuchsia_hardware_ufs_phy::{self as fufs_phy, UfshciRequest};
use fidl_fuchsia_power_system as fpower_system;
use fidl_fuchsia_scheduler as fscheduler;
use fidl_fuchsia_storage_block::BlockRequestStream;
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceObj};
use fuchsia_inspect::Inspector;
use futures::future::{FusedFuture, join_all};
use futures::{FutureExt, StreamExt, TryStreamExt, select};
use log::{debug, error, info, warn};
use mmio::Mmio;
use mmio::region::MmioRegion;
use mmio::vmo::{VmoMapping, VmoMemory};
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::mem::take;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{Builder, JoinHandle};

/// Register access type of the production driver: the mapped MMIO VMO of the host controller.
pub type DriverMmio = MmioRegion<VmoMemory>;

/// Block server of one LUN; sessions run on worker threads (`block_server::callback_interface`).
type DriverBlockServer = LunBlockServer<DriverMmio>;

/// Outgoing FIDL services exposed to child nodes and clients (`Volume` is indexed by LUN).
pub enum OutgoingService {
    /// Administrative `fuchsia.hardware.ufs.Service` protocol.
    Ufs(fufs::ServiceRequest),
    /// Per-LUN `fuchsia.hardware.block.volume.Service` protocol indexed by LUN.
    Volume(u8, fvolume::ServiceRequest),
    /// Replay Protected Memory Block (`fuchsia.hardware.rpmb.Service`) protocol.
    Rpmb(frpmb::ServiceRequest),
}

/// DFv2 child nodes of a UFS Logical Unit, kept alive while the driver runs.
struct LogicalUnit {
    /// Intermediate `scsi-block-device-0-<lun>` node offering the block volume service.
    _scsi_node: Node,
    _scsi_controller: ClientEnd<NodeControllerMarker>,
    /// Leaf `block` node exported to devfs.
    _block_node: ClientEnd<NodeMarker>,
    _block_controller: ClientEnd<NodeControllerMarker>,
}

/// Client ends of the `ufs.rpmb` child node, kept alive while the driver runs.
struct RpmbNode {
    _node: ClientEnd<NodeMarker>,
    _controller: ClientEnd<NodeControllerMarker>,
}

/// Per-LUN state created during bring-up before FIDL service registration.
struct RegisteredLun {
    lun: u8,
    logical_unit: LogicalUnit,
    block_server: Arc<DriverBlockServer>,
    connector_server: ServerEnd<fdevfs::ConnectorMarker>,
}

/// Everything [`UfsDriver::bring_up_device`] creates once the device is initialized.
struct BoundDevice {
    inspector: Inspector,
    parent_node: Node,
    logical_units: Vec<LogicalUnit>,
    rpmb_node: Option<RpmbNode>,
    block_servers: Arc<HashMap<u8, Arc<DriverBlockServer>>>,
}

/// Parent bus of the host controller and the state to release on stop.
#[derive(Debug, Clone)]
pub enum Bus {
    /// Platform device; holds the interconnect path if a bandwidth vote was cast.
    Pdev { interconnect_path: Option<finterconnect::PathProxy> },
    /// PCI function.
    Pci { device: fpci::DeviceProxy },
}

/// Core UFS driver holding the device state and all owned resources.
///
/// [`Driver::stop`] tears down active work before `Self` is dropped. Field order governs drop
/// order: `power_worker` and `interrupt_workers` drop before `scope` so worker tasks and threads
/// finish first, and all three drop before the child nodes and `processor` that own the DMA
/// buffers.
pub struct UfsDriver {
    /// Whether the framework created a power element (`start_args.power_element_args`). Tracked
    /// separately from `power_worker` because an unserved runner pins the element at level 1 and
    /// blocks system suspend even when storage power management is disabled.
    framework_suspend_enabled: bool,
    power_worker: Option<PowerWorker>,
    /// Dedicated OS thread(s) servicing hardware interrupts. Stopped after `quiesce()` in `stop()`.
    interrupt_workers: InterruptWorkers,
    /// Hosts the I/O stall watchdog, devfs connector, PHY `Ufshci`, and FIDL server tasks. Declared
    /// after `interrupt_workers` so the IRQ thread is joined before it drops.
    scope: fasync::Scope,
    _inspector: Inspector,
    _parent_node: Node,
    _logical_units: Vec<LogicalUnit>,
    _rpmb_node: Option<RpmbNode>,
    /// Quiesced in `stop()`.
    processor: Arc<TransferRequestProcessor<DriverMmio>>,
    /// Per-LUN block servers; their sessions are terminated in `stop()`.
    block_servers: Arc<HashMap<u8, Arc<DriverBlockServer>>>,
    /// Device, PHY, bus, and power state.
    power: Arc<PowerControl<DriverMmio, PlatformPower<DriverMmio>>>,
}

/// Hardware resources acquired from either a Platform Device (SoC) or PCI parent node.
struct HardwareResources {
    mmio: DriverMmio,
    interrupt: zx::Interrupt,
    bti: zx::Bti,
    phy_proxy: Option<fufs_phy::UfsPhyProxy>,
    bus: Bus,
    quirks: HostQuirks,
}

/// Forwards every `Connect` request on a devfs connector `stream` to `connect_cb`.
async fn run_connector(
    mut stream: fdevfs::ConnectorRequestStream,
    mut connect_cb: impl FnMut(zx::Channel) + Send + 'static,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fdevfs::ConnectorRequest::Connect { server, .. } => {
                connect_cb(server);
            }
        }
    }
}

/// Maps a UIC command error to the raw `zx.Status` returned on the PHY's `Ufshci` protocol.
///
/// Reports `BAD_STATE` when controller registers are unreachable (`IoError`, per
/// `fuchsia.hardware.ufs.phy/PowerState`), and `IO` rather than `INTERNAL` on a failed UIC command
/// (`UicError`).
fn ufs_error_to_raw_status(err: UfsError) -> i32 {
    match err {
        UfsError::Timeout => zx::Status::TIMED_OUT.into_raw(),
        UfsError::UicError { .. } => zx::Status::IO.into_raw(),
        UfsError::IoError => zx::Status::BAD_STATE.into_raw(),
        UfsError::InvalidSlot(_) | UfsError::CheckCondition | UfsError::InvalidParameter => {
            zx::Status::INTERNAL.into_raw()
        }
    }
}

/// Logs a failed bring-up step and converts its error.
fn check_step<T>(step: &str, result: Result<T, UfsError>) -> Result<T, DriverError> {
    result.map_err(|e| {
        error!(step, err:% = e; "UFS bring-up step failed");
        DriverError::from(zx::Status::from(e))
    })
}

/// Logs a failed FIDL call returning `error zx.Status` and converts its error.
fn check_fidl<T>(
    call: &str,
    result: Result<Result<T, i32>, fidl::Error>,
) -> Result<T, DriverError> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(raw)) => {
            let status = zx::Status::err_from_raw(raw);
            error!(call, status:%; "FIDL call returned error");
            Err(status.into())
        }
        Err(e) => {
            error!(call, err:? = e; "FIDL call failed");
            Err(e.into())
        }
    }
}

/// Maps `size` bytes of the register VMO starting at `offset`.
fn map_mmio(vmo: zx::Vmo, offset: u64, size: u64) -> Result<DriverMmio, DriverError> {
    if size < MIN_MMIO_SIZE {
        error!(size, min_size = MIN_MMIO_SIZE; "MMIO region is smaller than minimum");
        return Err(zx::Status::OUT_OF_RANGE.into());
    }
    let offset = usize::try_from(offset).map_err(|_| zx::Status::OUT_OF_RANGE)?;
    let size = usize::try_from(size).map_err(|_| zx::Status::OUT_OF_RANGE)?;
    VmoMapping::map(offset, size, vmo).map_err(|status| {
        error!(status:%; "Failed to map MMIO");
        status.into()
    })
}

fn bandwidth_request(avg_bps: u64, peak_bps: u64) -> finterconnect::BandwidthRequest {
    finterconnect::BandwidthRequest {
        average_bandwidth_bps: Some(avg_bps),
        peak_bandwidth_bps: Some(peak_bps),
        tag: Some(INTERCONNECT_TAG_UFS),
        ..Default::default()
    }
}

/// Requests [`WORKER_SCHEDULER_ROLE`] via `fuchsia.scheduler.RoleManager` for the calling thread.
///
/// Applied to the driver executor and worker threads. Failures are logged and ignored.
async fn apply_worker_scheduler_role(context: &DriverContext) {
    let role_manager: fscheduler::RoleManagerProxy = match context.incoming.connect_protocol() {
        Ok(proxy) => proxy,
        Err(status) => {
            warn!(status:%; "No RoleManager, executor thread keeps default role");
            return;
        }
    };
    let thread = match fuchsia_runtime::with_thread_self(|thread| {
        thread.duplicate_handle(zx::Rights::SAME_RIGHTS)
    }) {
        Ok(thread) => thread,
        Err(status) => {
            warn!(status:%; "Failed to duplicate executor thread handle");
            return;
        }
    };
    let request = fscheduler::RoleManagerSetRoleRequest {
        target: Some(fscheduler::RoleTarget::Thread(thread)),
        role: Some(fscheduler::RoleName { role: WORKER_SCHEDULER_ROLE.to_string() }),
        ..Default::default()
    };
    match role_manager.set_role(request).await {
        Ok(Ok(_)) => info!(role = WORKER_SCHEDULER_ROLE; "Executor thread scheduler role set"),
        Ok(Err(raw)) => {
            warn!(
                status:% = zx::Status::err_from_raw(raw);
                "Failed to set executor thread scheduler role"
            );
        }
        Err(e) => warn!(err:? = e; "Failed to set executor thread scheduler role"),
    }
}

/// Serves DME get/set requests from the PHY driver.
///
/// The PHY configures itself through the host controller, so it calls back into this driver
/// while its own `Init` or `PrePowerModeChange` call is still in progress. On resume, the caller
/// already holds [`PowerGuard`], so requests go straight to `controller`; taking the lock here
/// would deadlock.
async fn handle_ufshci_stream<M: Mmio + Send + Sync + 'static>(
    mut stream: fufs_phy::UfshciRequestStream,
    controller: Arc<UfsController<M>>,
) -> Result<(), fidl::Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            UfshciRequest::DmeSet { mib_attribute, gen_selector_index, value, responder } => {
                let res = controller.dme_set(mib_attribute, gen_selector_index, value).await;
                responder.send(res.map_err(ufs_error_to_raw_status))?;
            }
            UfshciRequest::DmeGet { mib_attribute, gen_selector_index, responder } => {
                let res = controller.dme_get(mib_attribute, gen_selector_index).await;
                responder.send(res.map_err(ufs_error_to_raw_status))?;
            }
            UfshciRequest::_UnknownMethod { ordinal, .. } => {
                warn!(ordinal; "Unknown Ufshci ordinal");
            }
        }
    }
    Ok(())
}

impl UfsDriver {
    /// Acquires MMIO, BTI, IRQ, PHY and interconnect resources from the `pdev` parent.
    ///
    /// Returns `Ok(None)` if there is no platform device (the first call fails at the transport
    /// level), so the caller can try PCI. Any later failure is an error.
    async fn try_acquire_pdev_resources(
        context: &DriverContext,
    ) -> Result<Option<HardwareResources>, DriverError> {
        let pdev = match context
            .incoming
            .service::<fpdev::ServiceProxy>()
            .instance(PDEV_INSTANCE)
            .connect()
            .and_then(|service| service.connect_to_device().map_err(|_| zx::Status::NOT_FOUND))
        {
            Ok(pdev) => pdev,
            Err(status) => {
                info!(status:%; "No platform device, trying PCI");
                return Ok(None);
            }
        };
        let mmio = match pdev.get_mmio_by_id(0).await {
            Err(e) => {
                info!(err:? = e; "Platform device GetMmioById failed, trying PCI");
                return Ok(None);
            }
            result => check_fidl("GetMmioById", result)?,
        };
        info!("UfsDriver binding to Platform Device...");
        let (Some(vmo), Some(offset), Some(size)) = (mmio.vmo, mmio.offset, mmio.size) else {
            error!("Platform Device MMIO 0 lacks a VMO, offset or size");
            return Err(zx::Status::INTERNAL.into());
        };
        let mmio = map_mmio(vmo, offset, size)?;
        let bti = check_fidl("GetBtiById", pdev.get_bti_by_id(0).await)?;
        let interrupt = check_fidl("GetInterruptById", pdev.get_interrupt_by_id(0, 0).await)?;

        let phy_proxy = context
            .incoming
            .service::<fufs_phy::ServiceProxy>()
            .instance(PHY_INSTANCE)
            .connect()
            .ok()
            .and_then(|service| service.connect_to_phy().ok());
        if phy_proxy.is_none() {
            warn!("Could not connect to the UFS PHY service");
        }

        // Last, so that no earlier failure leaks the vote.
        let interconnect_path = Self::vote_interconnect_bandwidth(context).await?;
        Ok(Some(HardwareResources {
            mmio,
            interrupt,
            bti,
            phy_proxy,
            bus: Bus::Pdev { interconnect_path },
            quirks: HostQuirks::for_pdev(),
        }))
    }

    /// Votes for the operational interconnect bandwidth. The `ufs-interconnect` parent is
    /// optional: a transport error means it is absent and returns `Ok(None)`.
    async fn vote_interconnect_bandwidth(
        context: &DriverContext,
    ) -> Result<Option<finterconnect::PathProxy>, DriverError> {
        let Ok(path) = context
            .incoming
            .service::<finterconnect::PathServiceProxy>()
            .instance(INTERCONNECT_INSTANCE)
            .connect()
            .and_then(|service| service.connect_to_path().map_err(|_| zx::Status::NOT_FOUND))
        else {
            info!("No interconnect path, skipping the bandwidth vote");
            return Ok(None);
        };
        let req =
            bandwidth_request(INTERCONNECT_AVG_BANDWIDTH_BPS, INTERCONNECT_PEAK_BANDWIDTH_BPS);
        match path.set_bandwidth(&req).await {
            Err(e) => {
                info!(err:? = e; "Interconnect SetBandwidth failed, continuing without a vote");
                Ok(None)
            }
            result => {
                check_fidl("SetBandwidth", result)?;
                Ok(Some(path))
            }
        }
    }

    /// Acquires BAR 0, the IRQ (MSI-X, MSI or legacy) and the BTI from the `pci` parent, detects
    /// quirks and enables bus mastering.
    async fn acquire_pci_resources(
        context: &DriverContext,
    ) -> Result<HardwareResources, DriverError> {
        info!("UfsDriver binding to PCI...");
        let pci_service = context
            .incoming
            .service::<fpci::ServiceProxy>()
            .instance(PCI_INSTANCE)
            .connect()
            .map_err(|status| {
                error!(status:%; "Failed to connect to PCI device service");
                DriverError::from(status)
            })?;
        let pci = pci_service.connect_to_device().map_err(|e| {
            error!(err:? = e; "Failed to connect to PCI device");
            DriverError::from(zx::Status::NOT_FOUND)
        })?;

        let info = pci.get_device_info().await.map_err(|e| {
            error!(err:? = e; "Failed to get PCI device info");
            DriverError::from(e)
        })?;
        let quirks = HostQuirks::from_pci_ids(info.vendor_id, info.device_id);
        info!(
            vendor_id = info.vendor_id,
            device_id = info.device_id,
            quirks:?;
            "PCI device info"
        );

        let bar = check_fidl("GetBar", pci.get_bar(0).await)?;
        let fpci::BarResult::Vmo(vmo) = bar.result else {
            error!("PCI BAR 0 is not an MMIO BAR");
            return Err(zx::Status::WRONG_TYPE.into());
        };
        let mmio = map_mmio(vmo, 0, bar.size)?;

        let modes = pci.get_interrupt_modes().await.map_err(|e| {
            error!(err:? = e; "Failed to get PCI interrupt modes");
            DriverError::from(e)
        })?;
        let irq_mode = if modes.msix_count > 0 {
            fpci::InterruptMode::MsiX
        } else if modes.msi_count > 0 {
            fpci::InterruptMode::Msi
        } else if modes.has_legacy {
            fpci::InterruptMode::Legacy
        } else {
            error!("No interrupt modes are supported");
            return Err(zx::Status::NOT_SUPPORTED.into());
        };
        check_fidl("SetInterruptMode", pci.set_interrupt_mode(irq_mode, 1).await)?;
        let interrupt = check_fidl("MapInterrupt", pci.map_interrupt(0).await)?;
        let bti = check_fidl("GetBti", pci.get_bti(0).await)?;

        // Last, so that no earlier failure leaves bus mastering on.
        check_fidl("SetBusMastering", pci.set_bus_mastering(true).await)?;
        Ok(HardwareResources {
            mmio,
            interrupt,
            bti,
            phy_proxy: None,
            bus: Bus::Pci { device: pci },
            quirks,
        })
    }

    /// Acquires hardware resources from the platform device, or from PCI if there is none.
    async fn acquire_hardware_resources(
        context: &DriverContext,
    ) -> Result<HardwareResources, DriverError> {
        match Self::try_acquire_pdev_resources(context).await? {
            Some(resources) => Ok(resources),
            None => Self::acquire_pci_resources(context).await,
        }
    }

    /// Powers up and enables the host controller, then brings up the link.
    ///
    /// 1. Request `Active` power from the PHY driver. It owns the controller's power domain and
    ///    clocks, so this must come before any register access.
    /// 2. Reset and enable the host controller. PHY setup uses DME commands, which need an
    ///    enabled controller.
    /// 3. Initialize the PHY.
    /// 4. Start the link (see [`UfsController::link_startup`] for the retry policy).
    ///
    /// Disables the controller again if step 3 or 4 fails.
    async fn init_controller_and_link<M: Mmio + Send + Sync + 'static>(
        device_manager: &DeviceManager,
        mmio: M,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        scope: &fasync::Scope,
    ) -> Result<Arc<UfsController<M>>, DriverError> {
        device_manager
            .set_phy_power_state(phy_proxy, fufs_phy::PowerState::Active, PowerStep::PhyActive)
            .await
            .map_err(|err| DriverError::Status(zx::Status::from(err)))?;
        let controller = Arc::new(UfsController::new(mmio));
        check_step("Host controller enable", controller.reset_and_enable().await)?;
        if let Err(err) =
            Self::init_phy_and_link(device_manager, &controller, phy_proxy, scope).await
        {
            Self::disable_controller_or_log(&controller).await;
            return Err(err);
        }
        Ok(controller)
    }

    /// Initializes the attached PHY, if any, and spawns its DME callback server on `scope`.
    ///
    /// The callback server outlives `Init`: the PHY keeps the channel for `PrePowerModeChange`
    /// and closes it only on the next `Init` or on `PowerDown`, which ends the task. All
    /// remaining tasks are cancelled by [`Driver::stop`].
    pub async fn init_phy<M: Mmio + Send + Sync + 'static>(
        controller: &Arc<UfsController<M>>,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        scope: &fasync::ScopeHandle,
    ) -> Result<(), DriverError> {
        if let Some(phy) = phy_proxy {
            info!("UFS PHY detected, performing PHY initialization...");
            let (client_end, server_end) = create_endpoints::<fufs_phy::UfshciMarker>();
            let controller_for_phy = Arc::clone(controller);
            let stream = server_end.into_stream();
            scope.spawn(async move {
                if let Err(e) = handle_ufshci_stream(stream, controller_for_phy).await {
                    warn!(err:? = e; "UFSHCI stream closed");
                }
            });
            check_fidl("UfsPhy.Init", phy.init(client_end).await)?;
            info!("UFS PHY initialization successful");
        }
        Ok(())
    }

    /// Initializes the attached PHY, if any, and starts the link.
    async fn init_phy_and_link<M: Mmio + Send + Sync + 'static>(
        device_manager: &DeviceManager,
        controller: &Arc<UfsController<M>>,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        scope: &fasync::Scope,
    ) -> Result<(), DriverError> {
        device_manager
            .run_power_step(
                PowerStep::PhyInit,
                Self::init_phy(controller, phy_proxy, scope.as_handle()),
            )
            .await?;
        check_step("UFS link startup", controller.link_startup().await)
    }

    /// [`UfsController::disable_host_controller`] for failure paths, where the error can only be
    /// logged.
    async fn disable_controller_or_log<M: Mmio>(controller: &UfsController<M>) {
        if let Err(e) = controller.disable_host_controller().await {
            error!(err:% = e; "Failed to disable host controller");
        }
    }

    /// Ties the driver's power element to the CPU execution state in Power Broker.
    ///
    /// Keeps storage at [`POWER_LEVEL_ON`] while the CPU is active so system suspend runs only
    /// after the CPU enters suspend.
    async fn configure_power_dependency(context: &DriverContext) -> Result<(), zx::Status> {
        let token = context
            .start_args
            .power_element_args
            .as_ref()
            .and_then(|args| args.token.as_ref())
            .ok_or(zx::Status::NOT_FOUND)?
            .duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let cpu_mgr: fpower_system::CpuElementManagerProxy = context.incoming.connect_protocol()?;
        let req = fpower_system::CpuElementManagerAddExecutionStateDependencyRequest {
            dependency_token: Some(token),
            power_level: Some(POWER_LEVEL_ON),
            ..Default::default()
        };
        match cpu_mgr.add_execution_state_dependency(req).await {
            Ok(Ok(())) => {
                debug!("Registered CPU execution-state power dependency");
                Ok(())
            }
            Ok(Err(err)) => {
                error!(err:?; "CPU power manager rejected execution-state dependency");
                Err(zx::Status::INTERNAL)
            }
            Err(err) => {
                error!(err:%; "Failed to register CPU execution-state dependency");
                Err(zx::Status::INTERNAL)
            }
        }
    }

    /// Registers DFv2 `scsi-block-device-0-<lun>` and `block` child nodes for all enabled LUNs
    /// with a supported logical block size.
    async fn register_logical_units<'a>(
        device_manager: &DeviceManager,
        units: &[DiscoveredUnit],
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
        parent_node: &Node,
        service_fs: &mut ServiceFs<ServiceObj<'a, OutgoingService>>,
    ) -> Result<Vec<RegisteredLun>, DriverError> {
        let mut registered = Vec::new();

        for &DiscoveredUnit { lun, descriptor: ref unit } in units {
            if unit.lu_enable != LU_ENABLED {
                continue;
            }
            let Some(block_size) = device_manager.logical_block_size(unit) else {
                error!(
                    lun,
                    logical_block_size = unit.logical_block_size;
                    "Unsupported bLogicalBlockSize, skipping LUN"
                );
                continue;
            };
            info!(
                lun,
                block_size,
                block_count = unit.logical_block_count.get();
                "Discovered LUN"
            );

            if let Err(e) = processor.send_test_unit_ready(lun).await {
                warn!(lun, err:% = e; "TEST UNIT READY failed");
            }

            let device_flags = UfsBlockDevice::<DriverMmio>::device_flags(unit);
            let block_server =
                UfsBlockDevice::new(lun, block_size, unit, device_flags, Arc::clone(processor))
                    .into_server();

            let device_name = format!("scsi-block-device-0-{lun}");
            let offer = ServiceOffer::<fvolume::ServiceMarker>::new()
                .add_default_named(service_fs, device_name.clone(), move |req| {
                    OutgoingService::Volume(lun, req)
                })
                .build_zircon_offer();

            let scsi_args = NodeAddArgs {
                name: Some(device_name),
                offers2: Some(vec![offer]),
                ..Default::default()
            };
            let (scsi_node, scsi_controller) =
                parent_node.add_owned_child(scsi_args).await.map_err(|status| {
                    error!(lun, status:%; "Failed to add scsi node");
                    DriverError::from(status)
                })?;

            let (connector, connector_server) = create_endpoints::<fdevfs::ConnectorMarker>();

            let devfs_args = DevfsAddArgs {
                connector: Some(connector),
                class_name: Some("block".to_string()),
                ..Default::default()
            };
            let block_args = NodeAddArgs {
                name: Some("block".to_string()),
                devfs_args: Some(devfs_args),
                ..Default::default()
            };

            let scsi_node = Node::from(scsi_node.into_proxy());
            let (block_node, block_controller) =
                scsi_node.add_owned_child(block_args).await.map_err(|status| {
                    error!(lun, status:%; "Failed to add block node");
                    DriverError::from(status)
                })?;

            let logical_unit = LogicalUnit {
                _scsi_node: scsi_node,
                _scsi_controller: scsi_controller,
                _block_node: block_node,
                _block_controller: block_controller,
            };
            registered.push(RegisteredLun { lun, logical_unit, block_server, connector_server });
        }

        if registered.is_empty() {
            error!("No usable logical units found");
            return Err(zx::Status::BAD_STATE.into());
        }
        // Completions of block requests are routed to their LUN's server from the interrupt path.
        processor.set_io_completer(Arc::new(BlockServerCompleter::new(
            registered.iter().map(|item| (item.lun, &item.block_server)),
        )));

        Ok(registered)
    }

    /// Probes the RPMB Well-Known LUN ([`crate::constants::RPMB_LUN`]) and, if it is ready,
    /// creates its server and the `ufs.rpmb` child node offering `fuchsia.hardware.rpmb.Service`.
    ///
    /// If `add_owned_child` fails, a warning is logged and `(Some(rpmb_server), None)` is still
    /// returned so `OutgoingService::Rpmb` remains served on the driver's outgoing directory.
    async fn init_rpmb(
        parent_node: &Node,
        service_fs: &mut ServiceFs<ServiceObj<'static, OutgoingService>>,
        device_manager: &DeviceManager,
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
    ) -> (Option<Arc<UfsRpmbServer>>, Option<RpmbNode>) {
        if !UfsRpmbServer::probe_rpmb_lun(processor).await {
            return (None, None);
        }
        let rpmb_server = match UfsRpmbServer::new(device_manager, Arc::clone(processor)).await {
            Ok(server) => Arc::new(server),
            Err(status) => {
                warn!(status:%; "Failed to create RPMB server");
                return (None, None);
            }
        };

        let rpmb_offer = ServiceOffer::<frpmb::ServiceMarker>::new()
            .add_default_named(service_fs, "default", OutgoingService::Rpmb)
            .build_zircon_offer();
        let rpmb_args = NodeAddArgs {
            name: Some("ufs.rpmb".to_string()),
            offers2: Some(vec![rpmb_offer]),
            ..Default::default()
        };
        let rpmb_node = match parent_node.add_owned_child(rpmb_args).await {
            Ok((node, controller)) => {
                info!("Registered ufs.rpmb child node and fuchsia.hardware.rpmb.Service");
                Some(RpmbNode { _node: node, _controller: controller })
            }
            Err(status) => {
                warn!(status:%; "Failed to add ufs.rpmb node");
                None
            }
        };
        (Some(rpmb_server), rpmb_node)
    }

    /// Publishes the outgoing directory and serves the `ufs`, per-LUN `volume`, `rpmb`, and devfs
    /// `block` connector services on `scope`.
    fn serve_outgoing_services(
        context: &mut DriverContext,
        scope: &fasync::Scope,
        mut service_fs: ServiceFs<ServiceObj<'static, OutgoingService>>,
        ufs_server: Arc<UfsServer>,
        rpmb_server: Option<Arc<UfsRpmbServer>>,
        block_servers: Arc<HashMap<u8, Arc<DriverBlockServer>>>,
        connector_servers: Vec<(u8, ServerEnd<fdevfs::ConnectorMarker>)>,
    ) -> Result<(), DriverError> {
        service_fs.dir("svc").add_fidl_service_instance("default", OutgoingService::Ufs);
        context.serve_outgoing(&mut service_fs).map_err(|err| {
            error!(err:?; "Failed to serve outgoing directory");
            err
        })?;

        for (lun, connector_server) in connector_servers {
            if let Some(server) = block_servers.get(&lun).cloned() {
                let scope_handle = scope.to_handle();
                let stream = connector_server.into_stream();
                scope.spawn(run_connector(stream, move |channel| {
                    let stream =
                        BlockRequestStream::from_channel(fasync::Channel::from_channel(channel));
                    let server = Arc::clone(&server);
                    scope_handle.spawn(async move {
                        let _ = server.handle_requests(stream).await;
                    });
                }));
            }
        }

        scope.spawn(async move {
            service_fs
                .for_each_concurrent(None, move |request| {
                    let ufs_server = Arc::clone(&ufs_server);
                    let block_servers = Arc::clone(&block_servers);
                    let rpmb_server = rpmb_server.clone();
                    async move {
                        match request {
                            OutgoingService::Ufs(fufs::ServiceRequest::Device(stream)) => {
                                let _ = ufs_server.serve(stream).await;
                            }
                            OutgoingService::Volume(lun, volume_request) => {
                                if let fvolume::ServiceRequest::Volume(stream) = volume_request
                                    && let Some(block_server) = block_servers.get(&lun)
                                {
                                    let _ = block_server.handle_requests(stream).await;
                                }
                            }
                            OutgoingService::Rpmb(frpmb::ServiceRequest::Device(stream)) => {
                                if let Some(rpmb_server) = rpmb_server {
                                    rpmb_server.serve(stream).await;
                                }
                            }
                        }
                    }
                })
                .await;
        });
        Ok(())
    }

    /// Requests bus interconnect bandwidth on platform devices that provide a path.
    async fn set_bus_bandwidth(bus: &Bus, avg_bps: u64, peak_bps: u64) -> Result<(), UfsError> {
        if let Bus::Pdev { interconnect_path: Some(path) } = bus {
            check_fidl(
                "SetBandwidth",
                path.set_bandwidth(&bandwidth_request(avg_bps, peak_bps)).await,
            )
            .map_err(|_| UfsError::IoError)?;
        }
        Ok(())
    }

    pub async fn restore_bus_bandwidth(bus: &Bus) -> Result<(), UfsError> {
        Self::set_bus_bandwidth(
            bus,
            INTERCONNECT_AVG_BANDWIDTH_BPS,
            INTERCONNECT_PEAK_BANDWIDTH_BPS,
        )
        .await
    }

    pub async fn drop_bus_bandwidth(bus: &Bus) -> Result<(), UfsError> {
        Self::set_bus_bandwidth(bus, 0, 0).await
    }

    /// Releases the parent bus after the controller is stopped: drops the interconnect vote
    /// (pdev) or disables bus mastering (PCI).
    async fn release_bus(bus: &Bus) {
        match bus {
            Bus::Pdev { .. } => {
                let _ = Self::drop_bus_bandwidth(bus).await;
            }
            Bus::Pci { device } => {
                let _ = check_fidl("SetBusMastering", device.set_bus_mastering(false).await);
            }
        }
    }

    /// Starts teardown: shuts down the power worker, locks [`PowerControl`], and closes new
    /// submissions.
    ///
    /// Returns the [`PowerGuard`], taken before submissions close so they never close under a
    /// running power transition. `stop()` holds it until the scope is cancelled, so no transition
    /// starts afterwards.
    async fn close_for_stop<'a, M: Mmio, H: PowerHooks>(
        power_worker: Option<&PowerWorker>,
        power: &'a PowerControl<M, H>,
    ) -> PowerGuard<'a, M, H> {
        if let Some(worker) = power_worker {
            worker.shutdown().await;
        }
        let guard = power.lock().await;
        power.processor.close_submissions();
        guard
    }

    /// Wakes a sleeping PHY before driver teardown disables the host controller.
    ///
    /// When register access is [`RegisterAccess::Blocked`], the PHY is in `HibernateSleep` while
    /// `HCE = 1` still owns DMA, so PHY clocks and the bus vote must return to `Active` before
    /// `quiesce` touches controller registers. Other `RegisterAccess` states leave the PHY alone so
    /// teardown neither wakes `PowerDown` rails nor overwrites `Off` with `OffUnconfirmed`.
    ///
    /// Takes the [`PowerGuard`] from [`Self::close_for_stop`]: holding it keeps the power worker
    /// from changing the power state for the rest of teardown.
    async fn prepare_phy_for_stop<M: Mmio, H: PowerHooks>(power: &mut PowerGuard<'_, M, H>) {
        if power.register_access() == RegisterAccess::Blocked {
            // Best-effort: quiesce only needs register access, whereas waking PHY clocks below is
            // required for MMIO.
            let _ = power.restore_bus_bandwidth().await;
            let wake_res =
                power.set_phy_power_state(fufs_phy::PowerState::Active, PowerStep::PhyActive).await;
            match wake_res {
                Ok(()) => power.allow_register_access_for_reset(),
                Err(err) => {
                    error!(
                        err:%;
                        "Failed to wake PHY before controller teardown; quarantining DMA memory"
                    );
                    power.mark_unreachable_unquiesced();
                }
            }
        }
    }

    /// Reads the driver's structured configuration, falling back to safe defaults if no config
    /// VMO was provided.
    fn take_config(context: &mut DriverContext) -> ufs_rust_config::Config {
        match context.take_config::<ufs_rust_config::Config>() {
            Ok(config) => config,
            Err(status) => {
                info!(status:%; "Could not read UFS structured config, using defaults");
                ufs_rust_config::Config {
                    suspend_enabled: false,
                    storage_power_management_enabled: false,
                    enable_auto_hibernate: true,
                    auto_hibernate_timer_us: AUTO_HIBERNATE_TIMER_US,
                    command_timeout_sec: COMMAND_TIMEOUT.into_seconds() as u32,
                }
            }
        }
    }

    fn apply_timeout_config<M: Mmio>(
        processor: &TransferRequestProcessor<M>,
        device_manager: &mut DeviceManager,
        config: &ufs_rust_config::Config,
    ) {
        if config.command_timeout_sec > 0 {
            processor.set_timeout(zx::MonotonicDuration::from_seconds(i64::from(
                config.command_timeout_sec,
            )));
        }
        device_manager.drain_inflight_timeout = processor.command_timeout() + DRAIN_INFLIGHT_MARGIN;
    }

    /// Brings up the controller, link and device, then registers the child nodes. Stops the
    /// controller on failure.
    async fn initialize_with_resources(
        context: &mut DriverContext,
        resources: HardwareResources,
        config: &ufs_rust_config::Config,
        power_enabled: bool,
    ) -> Result<Self, DriverError> {
        let framework_suspend_enabled = context.start_args.power_element_args.is_some();
        let HardwareResources { mmio, interrupt, bti, phy_proxy, bus, quirks } = resources;
        let scope = fasync::Scope::new();

        let mut device_manager = DeviceManager::new(quirks);
        let controller =
            match Self::init_controller_and_link(&device_manager, mmio, phy_proxy.as_ref(), &scope)
                .await
            {
                Ok(controller) => controller,
                Err(err) => {
                    scope.cancel().await;
                    return Err(err);
                }
            };
        let processor = match TransferRequestProcessor::new(&bti, Arc::clone(&controller)) {
            Ok(processor) => Arc::new(processor),
            Err(status) => {
                error!(status:%; "Failed to create transfer request processor");
                scope.cancel().await;
                Self::disable_controller_or_log(&controller).await;
                return Err(status.into());
            }
        };
        Self::apply_timeout_config(&processor, &mut device_manager, config);
        let power = Arc::new(PowerControl::new(
            device_manager,
            Arc::clone(&processor),
            phy_proxy.clone(),
            PlatformPower::new(Arc::clone(&controller), bus, phy_proxy.clone(), scope.to_handle()),
        ));

        // From here on the controller may DMA, so every failure must go through the single
        // cleanup block below.
        let mut guard = power.lock().await;
        let mut interrupt_workers = InterruptWorkers::default();
        let result = async {
            check_step("Transfer request processor init", processor.init())?;
            guard.enable_interrupts();
            interrupt_workers = spawn_interrupt_thread(interrupt, Arc::clone(&processor))?;
            let processor_for_timeout = Arc::clone(&processor);
            scope.spawn(async move { processor_for_timeout.run_io_timeout_task().await });
            Self::bring_up_device(
                context,
                &mut guard,
                config,
                phy_proxy.as_ref(),
                &controller,
                &processor,
                &scope,
            )
            .await
        }
        .await;
        let bound = match result {
            Ok(bound) => bound,
            Err(err) => {
                interrupt_workers.stop().await;
                // Stop DMA before cancelling background tasks and releasing the processor's
                // buffers.
                let _ = guard.quiesce().await;
                drop(guard);
                scope.cancel().await;
                return Err(err);
            }
        };
        drop(guard);

        let power_worker = power_enabled.then(|| PowerWorker::spawn(&scope, Arc::clone(&power)));

        info!(luns = bound.logical_units.len(); "Bind Success");
        Ok(Self {
            framework_suspend_enabled,
            power_worker,
            interrupt_workers,
            scope,
            _inspector: bound.inspector,
            _parent_node: bound.parent_node,
            _logical_units: bound.logical_units,
            _rpmb_node: bound.rpmb_node,
            processor,
            block_servers: bound.block_servers,
            power,
        })
    }

    /// UTP bring-up, device configuration, and child node registration. QEMU skips the UniPro,
    /// power mode, and auto-hibernate steps.
    async fn bring_up_device(
        context: &mut DriverContext,
        power: &mut PowerGuard<'_, DriverMmio, PlatformPower<DriverMmio>>,
        config: &ufs_rust_config::Config,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        controller: &UfsController<DriverMmio>,
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
        scope: &fasync::Scope,
    ) -> Result<BoundDevice, DriverError> {
        let quirks = power.quirks;
        let inspector = Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        if let Err(status) =
            inspect::publish_ufs_inspect(&context.incoming, &inspector, scope.to_handle())
        {
            warn!(status:%; "Failed to publish UFS Inspect tree");
        }
        ufs_node.record_child("config", |node| config.record_inspect(node));
        processor.controller().record_inspect(&ufs_node);
        processor.record_inspect(&ufs_node);

        info!("Sending NOP OUT UPIU...");
        check_step("NOP OUT", processor.send_nop_out().await)?;

        let ah8_timer_us =
            if config.enable_auto_hibernate { config.auto_hibernate_timer_us } else { 0 };
        power.auto_hibernate_timer_us = ah8_timer_us;
        power.init_inspect(&ufs_node);
        if let Some(phy) = phy_proxy {
            match phy.get_properties().await {
                Ok(props) => {
                    let supports_suspend_resume = props.supports_suspend_resume.unwrap_or(false);
                    // UfsPhy.supports_suspend_resume gates full rail/controller PowerDown;
                    // link-hibernate SystemSleep works without it.
                    power.power_down_supported = supports_suspend_resume;
                }
                Err(e) => {
                    warn!(
                        err:% = e;
                        "Failed to query PHY power capabilities; defaulting to link sleep"
                    );
                }
            }
        }
        check_step("fDeviceInit", power.device_init(processor).await)?;
        // Record `fPowerOnWPEn` so that `restore_config` can set it again after a hardware
        // reset clears it.
        // TODO(https://fxbug.dev/586599452): On boards whose PHY resets the device at bring-up,
        // the flag reads 0 here, so there is nothing to restore later. Decide who sets it after
        // bring-up.
        let wp_en =
            check_step("fPowerOnWPEn", power.read_flag(processor, Flag::PowerOnWpEn).await)?;
        power.flags.set(fufs::FlagType::PowerOnWpEn, wp_en);
        power.init_reference_clock(processor).await;
        if !quirks.qemu {
            check_step("UniPro attributes", power.init_unipro_attributes(processor).await)?;
            // Stay usable in PWM if the HS switch fails.
            if let Err(e) = power.init_uic_power_mode(processor, phy_proxy).await {
                warn!(err:% = e; "Failed to switch to HS Fast_Mode, continuing in PWM");
            }
            check_step("UFS power mode", power.init_ufs_power_mode(processor).await)?;
        }
        match power.read_attribute(processor, Attribute::BootLunEn, 0).await {
            Ok(boot_lun) => info!(boot_lun; "bBootLunEn read"),
            Err(e) => warn!(err:% = e; "Failed to read bBootLunEn"),
        }
        check_step("Descriptor read", power.read_descriptors(processor).await)?;

        let units = power.discover_unit_descriptors(processor).await;
        power.configure_features(processor, &units).await;
        if !quirks.qemu {
            controller.configure_auto_hibernate(ah8_timer_us);
        }

        // Register LUN block devices, initialize RPMB and serve the outgoing FIDL protocols.
        let mut service_fs = ServiceFs::new();
        let parent_node = context.take_node().map_err(|err| {
            error!(err:?; "Failed to take DFv2 parent node");
            err
        })?;
        let registered =
            Self::register_logical_units(power, &units, processor, &parent_node, &mut service_fs)
                .await?;
        let mut logical_units = Vec::with_capacity(registered.len());
        let mut block_servers = HashMap::with_capacity(registered.len());
        let mut connector_servers = Vec::with_capacity(registered.len());
        let mut bound_luns = Vec::with_capacity(registered.len() + 1);
        for item in registered {
            logical_units.push(item.logical_unit);
            block_servers.insert(item.lun, item.block_server);
            connector_servers.push((item.lun, item.connector_server));
            bound_luns.push(item.lun);
        }
        power.record_logical_unit_count(logical_units.len());
        inspector.root().record(ufs_node);
        let block_servers = Arc::new(block_servers);

        let (rpmb_server, rpmb_node) =
            Self::init_rpmb(&parent_node, &mut service_fs, power, processor).await;
        if rpmb_server.is_some() {
            bound_luns.push(RPMB_LUN);
        }
        power.bound_luns = bound_luns;
        let ufs_server = Arc::new(UfsServer::new(Arc::clone(processor), Arc::clone(power.flags())));
        Self::serve_outgoing_services(
            context,
            scope,
            service_fs,
            ufs_server,
            rpmb_server,
            Arc::clone(&block_servers),
            connector_servers,
        )?;
        Ok(BoundDevice { inspector, parent_node, logical_units, rpmb_node, block_servers })
    }
}

impl Driver for UfsDriver {
    const NAME: &'static str = "ufs";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("UfsDriver starting...");
        apply_worker_scheduler_role(&context).await;
        let config = Self::take_config(&mut context);
        let resources = Self::acquire_hardware_resources(&context).await?;
        // QEMU's emulated controller supports no power management (see `HostQuirks::qemu`).
        let power_enabled = config.suspend_enabled
            && config.storage_power_management_enabled
            && !resources.quirks.qemu;
        let bus = resources.bus.clone();
        match Self::initialize_with_resources(&mut context, resources, &config, power_enabled).await
        {
            Ok(driver) => {
                // `driver_manager` registration is one-shot, so run it only after bring-up
                // succeeds; fail `start()` on error because `driver_manager` waits for it.
                if power_enabled
                    && let Err(status) = Self::configure_power_dependency(&context).await
                {
                    error!(status:%; "Failed to register CPU power dependency");
                    driver.stop().await;
                    return Err(status.into());
                }
                Ok(driver)
            }
            Err(err) => {
                Self::release_bus(&bus).await;
                Err(err)
            }
        }
    }

    async fn stop(self) {
        // Shut down in order:
        // 1. Shut down the power worker so suspend and resume transitions stop, and lock `power`
        //    for the rest of teardown.
        // 2. Close new submissions so callers fail fast while active slots finish. Clients parked
        //    on client IO admission fail here too.
        // 3. Terminate block sessions up to `drain_deadline` so FIFO loops stop reading requests.
        // 4. Drain remaining in-flight transfers while interrupts are still running.
        // 5. Quiesce the controller (`HCE = 0`) so DMA stops and stuck slots fail, unblocking any
        //    session still terminating.
        // 6. Stop the interrupt thread now that no more completions can arrive.
        // 7. Cancel the async scope to stop the timeout task and FIDL/devfs servers.
        // 8. Release the parent bus (interconnect vote or PCI bus mastering).
        let mut power = Self::close_for_stop(self.power_worker.as_ref(), &self.power).await;
        let drain_deadline = zx::MonotonicInstant::after(self.processor.command_timeout());
        let mut terminate_sessions = pin!(
            join_all(self.block_servers.values().map(|server| {
                let server = Arc::clone(server);
                fasync::unblock(move || server.session_manager().terminate())
            }))
            .fuse()
        );
        let mut drain_timer = pin!(fasync::Timer::new(drain_deadline).fuse());
        select! {
            _ = terminate_sessions => {}
            () = drain_timer => {
                warn!("Timed out waiting for block sessions to terminate before quiesce");
            }
        }
        let remaining =
            (drain_deadline - zx::MonotonicInstant::get()).max(zx::MonotonicDuration::ZERO);
        if let Err(err) = self.processor.drain_inflight(remaining).await {
            warn!(err:?; "Timed out draining in-flight UFS requests before quiesce");
        }
        Self::prepare_phy_for_stop(&mut power).await;
        if let Err(e) = power.quiesce().await {
            error!(err:? = e; "Failed to quiesce UFS controller");
        }
        // Join any session whose active slot was wedged until `quiesce()` failed it.
        if !terminate_sessions.is_terminated() {
            terminate_sessions.await;
        }
        self.interrupt_workers.stop().await;
        self.scope.to_handle().cancel().await;
        // Catch any session spawned after the first terminate() whose scope task was cancelled
        // before signalling its FIFO thread.
        for server in self.block_servers.values() {
            server.session_manager().terminate();
        }
        power.run_power_step(PowerStep::BusDrop, Self::release_bus(&power.hooks().bus)).await;
    }
}

impl SuspendableDriver for UfsDriver {
    async fn suspend(&self) {
        if let Some(worker) = &self.power_worker {
            // The framework cannot act on a failed suspend, and the power worker already logs the
            // failure with its cause.
            let _ = worker.suspend().await;
        }
    }

    async fn resume(&self) {
        if let Some(worker) = &self.power_worker {
            // The framework cannot act on a failed resume; hardware reset failures panic inside
            // the worker, and `Err` means the power worker has already stopped.
            let _ = worker.resume().await;
        }
    }

    fn suspend_enabled(&self) -> bool {
        self.framework_suspend_enabled
    }
}

driver_register!(Suspendable<UfsDriver>);

/// Dedicated interrupt worker threads that wait on hardware interrupts via `zx::Interrupt::wait`
/// without hopping through the driver's async executor.
#[derive(Default)]
struct InterruptWorkers {
    /// Interrupt handles and worker thread join handles.
    inner: Cell<InterruptWorkersInner>,
    /// Set by [`InterruptWorkers::take_and_destroy`] to signal worker threads to exit cleanly.
    stop_requested: Arc<AtomicBool>,
    /// Interrupt gate stopped by [`InterruptWorkers::take_and_destroy`] so the parked interrupt
    /// thread exits cleanly.
    interrupt_gate: Option<Arc<crate::controller::InterruptGate>>,
}

#[derive(Default)]
struct InterruptWorkersInner {
    /// Duplicate interrupt handles destroyed by [`InterruptWorkers::take_and_destroy`] to unblock
    /// each worker thread's `zx::Interrupt::wait` with `zx::Status::CANCELED`.
    interrupts: Vec<zx::Interrupt>,
    threads: Vec<JoinHandle<()>>,
}

impl InterruptWorkers {
    /// Destroys the interrupt objects so any blocked `wait()` returns `zx::Status::CANCELED`,
    /// stops the interrupt gate so the parked thread unparks, and returns the thread join handles.
    fn take_and_destroy(&self) -> Vec<JoinHandle<()>> {
        self.stop_requested.store(true, Ordering::Release);
        if let Some(interrupt_gate) = &self.interrupt_gate {
            interrupt_gate.stop();
        }
        let mut inner = self.inner.take();
        for irq in inner.interrupts.drain(..) {
            if let Err(status) = irq.destroy() {
                warn!(status:%; "Failed to destroy UFS interrupt on stop");
            }
        }
        take(&mut inner.threads)
    }

    /// Destroys all interrupts and joins the worker threads off the async executor.
    async fn stop(&self) {
        let threads = self.take_and_destroy();
        if !threads.is_empty() {
            fasync::unblock(move || {
                for handle in threads {
                    let _ = handle.join();
                }
            })
            .await;
        }
    }
}

impl Drop for InterruptWorkers {
    fn drop(&mut self) {
        for handle in self.take_and_destroy() {
            let _ = handle.join();
        }
    }
}

/// Spawns a dedicated OS thread that waits on `interrupt` and services completions directly
/// without hopping through the driver's async executor.
fn spawn_interrupt_thread<M, K>(
    interrupt: zx::Interrupt<K>,
    processor: Arc<TransferRequestProcessor<M>>,
) -> Result<InterruptWorkers, DriverError>
where
    M: Mmio + Send + Sync + 'static,
    K: zx::InterruptKind + Send + 'static,
{
    let irq_for_stop = interrupt.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(|status| {
        error!(status:%; "Failed to duplicate UFS interrupt handle");
        DriverError::from(status)
    })?;
    let irq_for_stop = zx::Interrupt::from(zx::NullableHandle::from(irq_for_stop));
    let interrupt_gate = Arc::clone(processor.controller().interrupt_gate());
    let stop_requested = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_requested);
    let handle = Builder::new()
        .name("ufs-irq-0".to_string())
        .spawn(move || {
            if let Err(e) = fuchsia_scheduler::set_role_for_this_thread(IRQ_SCHEDULER_ROLE) {
                warn!(err:? = e; "Failed to set UFS irq thread scheduler role");
            }
            loop {
                match interrupt.wait() {
                    Ok(_) => {}
                    Err(zx::Status::CANCELED) => return,
                    Err(status) => {
                        if !worker_stop.load(Ordering::Acquire) {
                            error!(status:%; "UFS irq wait failed");
                        }
                        break;
                    }
                }
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                let Some(_interrupt_guard) = processor.controller().interrupt_gate().enter() else {
                    return;
                };
                let _ = processor.service_interrupt();
            }
            if !worker_stop.load(Ordering::Acquire) {
                error!("UFS irq loop exited; in-flight requests will time out");
            }
        })
        .map_err(|e| {
            error!(err:% = e; "Failed to spawn UFS irq thread");
            DriverError::from(zx::Status::NO_RESOURCES)
        })?;
    Ok(InterruptWorkers {
        inner: Cell::new(InterruptWorkersInner {
            interrupts: vec![irq_for_stop],
            threads: vec![handle],
        }),
        stop_requested,
        interrupt_gate: Some(interrupt_gate),
    })
}

/// UFS driver error mapping to `zx::Status` (for `UicError`, `code` is the `UICCMDARG2` result or
/// `HCS.UPMCRS` status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UfsError {
    /// Requested transfer slot index is out of bounds.
    #[error("Invalid transfer slot: {0}")]
    InvalidSlot(u32),
    /// Controller, UIC, or UTP operation timed out.
    #[error("UFS operation timed out")]
    Timeout,
    /// UIC command failed with the given opcode and result code.
    #[error("UIC command {opcode:#x} failed with code {code:#x}")]
    UicError {
        /// UIC command opcode (`DME_*`).
        opcode: u8,
        /// Hardware result or power mode status code.
        code: u32,
    },
    /// Target device returned SCSI `CHECK CONDITION`.
    #[error("SCSI Check Condition")]
    CheckCondition,
    /// General hardware or transport I/O error.
    #[error("UFS I/O error")]
    IoError,
    /// Caller supplied an out-of-range or malformed parameter.
    #[error("Invalid parameter")]
    InvalidParameter,
}

impl From<UfsError> for zx::Status {
    fn from(err: UfsError) -> Self {
        match err {
            UfsError::InvalidSlot(_) => zx::Status::OUT_OF_RANGE,
            UfsError::Timeout => zx::Status::TIMED_OUT,
            UfsError::UicError { .. } => zx::Status::INTERNAL,
            UfsError::CheckCondition => zx::Status::IO_REFUSED,
            UfsError::IoError => zx::Status::IO,
            UfsError::InvalidParameter => zx::Status::INVALID_ARGS,
        }
    }
}

/// Platform-level power hooks (bus interconnect votes and `UfsPhy` init) owned by
/// [`PowerControl`].
pub struct PlatformPower<M: Mmio> {
    controller: Arc<UfsController<M>>,
    pub bus: Bus,
    phy_proxy: Option<fufs_phy::UfsPhyProxy>,
    scope: fasync::ScopeHandle,
}

impl<M: Mmio> PlatformPower<M> {
    pub fn new(
        controller: Arc<UfsController<M>>,
        bus: Bus,
        phy_proxy: Option<fufs_phy::UfsPhyProxy>,
        scope: fasync::ScopeHandle,
    ) -> Self {
        Self { controller, bus, phy_proxy, scope }
    }
}

impl<M: Mmio + Send + Sync + 'static> PowerHooks for PlatformPower<M> {
    fn init_phy(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        async {
            UfsDriver::init_phy(&self.controller, self.phy_proxy.as_ref(), &self.scope)
                .await
                .map_err(|_| UfsError::IoError)
        }
    }

    fn drop_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        UfsDriver::drop_bus_bandwidth(&self.bus)
    }

    fn restore_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        UfsDriver::restore_bus_bandwidth(&self.bus)
    }
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::{PowerControl, PowerState};
    use crate::registers::{
        HostControllerEnableReg, InterruptEnableReg, InterruptStatusReg, PowerModeStatus,
    };
    use crate::scsi::{ScsiOpcode, StartStopPowerCondition};
    use crate::testing::fake_ufs::{
        HCS_ACTIVE_LINK, HCS_DEVICE_PRESENT, complete_uic_command, hcs_power_mode_status,
        uic_hardware, uic_opcode, wait_until,
    };
    use crate::testing::{
        ADMIN_SLOT, OCS_SUCCESS, PAGE_SIZE, answer_system_suspend, create_test_processor, fake_utp,
        io_callback, scsi_response, slot_pinned, wait_async, wait_for_any_doorbell,
    };
    use crate::transfer_request_processor::DataTransfer;
    use crate::uic::UicCommandOpcode;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use mmio::{ReadableRegister, Register, WritableRegister};
    use std::sync::mpsc as sync_mpsc;
    use std::thread;
    use std::time::Duration;
    use zerocopy::IntoBytes;

    #[fuchsia::test]
    fn test_ufs_error_to_status() {
        assert_eq!(zx::Status::from(UfsError::InvalidSlot(40)), zx::Status::OUT_OF_RANGE);
        assert_eq!(zx::Status::from(UfsError::Timeout), zx::Status::TIMED_OUT);
        assert_eq!(
            zx::Status::from(UfsError::UicError { opcode: 0x16, code: 1 }),
            zx::Status::INTERNAL
        );
        assert_eq!(zx::Status::from(UfsError::CheckCondition), zx::Status::IO_REFUSED);
        assert_eq!(zx::Status::from(UfsError::IoError), zx::Status::IO);
        assert_eq!(zx::Status::from(UfsError::InvalidParameter), zx::Status::INVALID_ARGS);

        assert_eq!(ufs_error_to_raw_status(UfsError::Timeout), zx::Status::TIMED_OUT.into_raw());
        assert_eq!(
            ufs_error_to_raw_status(UfsError::UicError { opcode: 0x01, code: 1 }),
            zx::Status::IO.into_raw()
        );
        assert_eq!(ufs_error_to_raw_status(UfsError::IoError), zx::Status::BAD_STATE.into_raw());
        assert_eq!(
            ufs_error_to_raw_status(UfsError::InvalidParameter),
            zx::Status::INTERNAL.into_raw()
        );
    }

    #[fuchsia::test]
    fn test_ufs_error_display() {
        assert_eq!(UfsError::InvalidSlot(7).to_string(), "Invalid transfer slot: 7");
        assert_eq!(
            UfsError::UicError { opcode: 0x16, code: 0x1 }.to_string(),
            "UIC command 0x16 failed with code 0x1"
        );
    }

    #[fuchsia::test]
    async fn test_test_unit_ready_retries() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            // First call: one target failure, then success.
            // Second call: target failure on every attempt.
            for succeed in [false, true, false, false, false] {
                let slot = wait_for_any_doorbell(&utp);
                let mut response = scsi_response(slot, 0);
                response.header.response = if succeed { 0 } else { 1 };
                utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            }
            utp
        });

        assert_eq!(processor.send_test_unit_ready(0).await, Ok(()));
        assert_eq!(processor.send_test_unit_ready(0).await, Err(UfsError::IoError));
        let utp = hardware.join().unwrap();
        assert_eq!(utp.doorbell(), 0);
    }

    #[fuchsia::test]
    async fn test_dedicated_interrupt_worker_completes_io_and_stops() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let irq = zx::VirtualInterrupt::create_virtual().unwrap();
        let irq_trigger = irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let workers = spawn_interrupt_thread(irq, Arc::clone(&processor)).unwrap();

        let (completion, rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        processor.submit_io(0, cdb, None, completion);
        assert_eq!(utp.doorbell(), 1);

        // Complete the request without the fake's interrupt hook: the dedicated worker must pick
        // it up from the real (virtual) interrupt.
        utp.write_completion(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_utp_transfer_request_completion_status(true);
        WritableRegister::write(&is, &mut utp.regs);
        irq_trigger.trigger(zx::BootInstant::get()).unwrap();

        assert_eq!(rx.await.unwrap().io_status(), Ok(()));
        workers.stop().await;
    }

    #[fuchsia::test]
    async fn test_interrupt_during_hibernate_sleep_parks_and_services_on_resume() {
        let bti = fake_bti::FakeBti::create().unwrap();
        let mmio_vmo = zx::Vmo::create(crate::testing::MMIO_SIZE as u64).unwrap();
        let phy_powered = Arc::new(AtomicBool::new(true));
        let mmio = crate::testing::fake_ufs::FakeMmio::new(&mmio_vmo, crate::testing::MMIO_SIZE)
            .with_power_domain(Arc::clone(&phy_powered));
        let controller = Arc::new(UfsController::new(mmio));
        let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
        processor.init().unwrap();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let irq = zx::VirtualInterrupt::create_virtual().unwrap();
        let irq_trigger = irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let proc = fuchsia_runtime::process_self();
        let existing_koids: std::collections::HashSet<_> =
            proc.threads().unwrap().into_iter().collect();
        let workers = spawn_interrupt_thread(irq, Arc::clone(&processor)).unwrap();
        let mut interrupt_thread = None;
        assert!(
            wait_until(|| {
                if interrupt_thread.is_none() {
                    interrupt_thread = proc.threads().unwrap().into_iter().find_map(|koid| {
                        (!existing_koids.contains(&koid))
                            .then(|| {
                                proc.get_child(&koid, zx::Rights::SAME_RIGHTS)
                                    .ok()
                                    .filter(|th| th.get_name().is_ok_and(|n| n == "ufs-irq-0"))
                            })
                            .flatten()
                    });
                }
                interrupt_thread.as_ref().is_some_and(|th| {
                    th.info().is_ok_and(|i| {
                        i.state == zx::ThreadState::Blocked(zx::ThreadBlockType::Interrupt)
                    })
                })
            }),
            "interrupt thread did not enter interrupt.wait()"
        );
        let interrupt_thread = interrupt_thread.unwrap();

        let (completion, rx) = io_callback();
        processor.submit_io(0, ScsiOpcode::build_test_unit_ready_cdb(), None, completion);
        assert_eq!(utp.doorbell(), 1);

        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;

        // Simulate HibernateSleep blocking MMIO while a completion interrupt arrives concurrently.
        guard.block_register_access();
        phy_powered.store(false, Ordering::Release);

        utp.write_completion(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_utp_transfer_request_completion_status(true);
        WritableRegister::write(&is, &mut utp.regs);
        irq_trigger.trigger(zx::BootInstant::get()).unwrap();

        assert!(
            wait_until(|| {
                interrupt_thread
                    .info()
                    .is_ok_and(|i| i.state == zx::ThreadState::Blocked(zx::ThreadBlockType::Futex))
            }),
            "interrupt thread did not park on InterruptGate while registers are unreachable"
        );
        assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
        assert!(InterruptStatusReg::read(&utp.regs).utp_transfer_request_completion_status());

        // Waking from HibernateSleep must unpark the worker and deliver the pending completion
        // without needing a second interrupt trigger.
        phy_powered.store(true, Ordering::Release);
        guard.allow_register_access();
        assert_eq!(rx.await.unwrap().io_status(), Ok(()));
        assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
        workers.stop().await;
    }

    #[fuchsia::test]
    async fn test_stop_drains_inflight_write_before_quiesce() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let irq = zx::VirtualInterrupt::create_virtual().unwrap();
        let irq_trigger = irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let workers = spawn_interrupt_thread(irq, Arc::clone(&processor)).unwrap();

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let (completion, rx) = io_callback();
        let cdb = ScsiOpcode::build_write_10_cdb(0, 1);
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::write(Arc::clone(&data_vmo), 0, PAGE_SIZE as u32)),
            completion,
        );
        assert_eq!(processor.busy_slot_count(), 1);
        assert!(slot_pinned(&processor, 0));

        // Step 2 of stop(): close new submissions while in-flight completions remain active.
        processor.close_submissions();
        let (late_completion, late_rx) = io_callback();
        processor.submit_io(0, ScsiOpcode::build_test_unit_ready_cdb(), None, late_completion);
        assert_eq!(late_rx.await.unwrap().io_status(), Err(zx::Status::IO));

        // Complete the in-flight write on the dedicated IRQ worker while drain_inflight awaits.
        let hardware = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            utp.write_completion(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
            let mut is = InterruptStatusReg::from_raw(0);
            is.set_utp_transfer_request_completion_status(true);
            WritableRegister::write(&is, &mut utp.regs);
            irq_trigger.trigger(zx::BootInstant::get()).unwrap();
            utp
        });

        assert_eq!(processor.drain_inflight(COMMAND_TIMEOUT).await, Ok(()));
        let _utp = hardware.join().unwrap();
        assert_eq!(rx.await.unwrap().io_status(), Ok(()));
        assert_eq!(processor.busy_slot_count(), 0);
        assert!(!slot_pinned(&processor, 0));

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        workers.stop().await;
    }

    #[fuchsia::test]
    async fn test_close_for_stop_waits_for_suspend_then_fails_parked_clients() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| mmio.store32(HostControllerEnableReg::OFFSET, 1));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);

        // The fake hardware holds the first suspend command until teardown is waiting.
        let (suspend_started_tx, suspend_started_rx) = oneshot::channel();
        let (finish_suspend_tx, finish_suspend_rx) = sync_mpsc::channel();
        let hardware = thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT), "suspend did not start");
            suspend_started_tx.send(()).unwrap();
            finish_suspend_rx.recv().unwrap();
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
        });

        let scope = fasync::Scope::new();
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = true;
        let power = Arc::new(PowerControl::new(mgr, Arc::clone(&processor), None, ()));
        let worker = PowerWorker::spawn(&scope, Arc::clone(&power));
        let mut suspend_fut = pin!(worker.suspend());
        assert!(futures::poll!(suspend_fut.as_mut()).is_pending());
        suspend_started_rx.await.unwrap();

        // A block client parks on client IO admission when suspend pauses it.
        let (completion, rx) = io_callback();
        let submitter = {
            let processor = Arc::clone(&processor);
            thread::spawn(move || {
                processor.submit_io(0, ScsiOpcode::build_test_unit_ready_cdb(), None, completion);
            })
        };
        assert!(wait_async(|| processor.requests().blocked_on_admission() == 1).await);

        // Teardown waits for the suspend and leaves submissions open, so the suspend's remaining
        // commands still run.
        let mut close_fut = pin!(UfsDriver::close_for_stop(Some(&worker), &power));
        assert!(futures::poll!(close_fut.as_mut()).is_pending());
        assert!(!processor.request_list().lock().is_closing());
        finish_suspend_tx.send(()).unwrap();
        assert_eq!(suspend_fut.await, Ok(()));
        hardware.join().unwrap();

        let guard = close_fut.await;
        let state = guard.power();
        assert_eq!(state.mode, PowerState::PowerDown);
        assert!(!state.needs_reset);
        // Closing submissions fails the parked client instead of leaving it waiting.
        assert_eq!(rx.await.unwrap().io_status(), Err(zx::Status::IO));
        submitter.join().unwrap();

        // The power worker has shut down, so no later transition can run.
        assert_eq!(worker.resume().await, Err(zx::Status::IO));
        let mut guard = guard;
        assert_eq!(guard.quiesce().await, Ok(()));
        drop(guard);
        scope.cancel().await;
    }

    #[fuchsia::test]
    async fn test_stop_quiesce_aborts_unresponsive_inflight_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let mut hce = HostControllerEnableReg::from_raw(0);
        hce.set_host_controller_enable(true);
        WritableRegister::write(&hce, &mut utp.regs);

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let (completion, rx) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_write_10_cdb(0, 1),
            Some(DataTransfer::write(data_vmo, 0, PAGE_SIZE as u32)),
            completion,
        );
        assert!(slot_pinned(&processor, 0));

        processor.close_submissions();
        assert_eq!(
            processor.drain_inflight(zx::MonotonicDuration::from_millis(5)).await,
            Err(UfsError::Timeout)
        );
        // DMA buffer remains pinned until HCE=0 quiesce disables DMA.
        assert!(slot_pinned(&processor, 0));

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        assert!(!HostControllerEnableReg::read(&utp.regs).host_controller_enable());
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(rx.await.unwrap().io_status(), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_ufshci_stream_forwards_dme_and_fails_when_powered_off() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut uic_regs = uic_hardware(&mmio_vmo, HCS_ACTIVE_LINK);
        let (ufshci_proxy, ufshci_stream) = create_proxy_and_stream::<fufs_phy::UfshciMarker>();
        let controller = Arc::clone(processor.controller());
        let stream_task = fasync::Task::spawn(async move {
            handle_ufshci_stream(ufshci_stream, controller).await.unwrap();
        });

        let hardware = thread::spawn(move || {
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeGet as u32),
                "DME_GET not forwarded"
            );
            complete_uic_command(&mut uic_regs, 0, 0x1234_5678, |_| {});

            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeSet as u32),
                "DME_SET not forwarded"
            );
            complete_uic_command(&mut uic_regs, 0, 0, |_| {});

            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeGet as u32),
                "failing DME_GET not forwarded"
            );
            complete_uic_command(&mut uic_regs, 1, 0, |_| {});
        });

        assert_eq!(ufshci_proxy.dme_get(0x1571, 0).await.unwrap(), Ok(0x1234_5678));
        assert_eq!(ufshci_proxy.dme_set(0x1568, 0, 2).await.unwrap(), Ok(()));
        assert_eq!(ufshci_proxy.dme_get(0x1571, 0).await.unwrap(), Err(zx::Status::IO.into_raw()));
        hardware.join().unwrap();

        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;
        guard.block_register_access();
        assert_eq!(
            ufshci_proxy.dme_get(0x1571, 0).await.unwrap(),
            Err(zx::Status::BAD_STATE.into_raw())
        );
        guard.allow_register_access();

        guard.power_off_after(|_| {});
        assert_eq!(
            ufshci_proxy.dme_get(0x1571, 0).await.unwrap(),
            Err(zx::Status::BAD_STATE.into_raw())
        );
        assert_eq!(
            ufshci_proxy.dme_set(0x1568, 0, 2).await.unwrap(),
            Err(zx::Status::BAD_STATE.into_raw())
        );

        drop(ufshci_proxy);
        stream_task.await;
    }

    #[fuchsia::test]
    async fn test_inflight_dme_fails_with_bad_state_when_unreachable() {
        let bti = fake_bti::FakeBti::create().unwrap();
        let phy_powered = Arc::new(AtomicBool::new(true));
        let mmio_vmo = zx::Vmo::create(crate::testing::MMIO_SIZE as u64).unwrap();
        let mmio = crate::testing::fake_ufs::FakeMmio::new(&mmio_vmo, crate::testing::MMIO_SIZE)
            .with_power_domain(Arc::clone(&phy_powered));
        let controller = Arc::new(UfsController::new(mmio));
        let processor =
            Arc::new(TransferRequestProcessor::new(&bti, Arc::clone(&controller)).unwrap());
        let power =
            PowerControl::new(DeviceManager::new(HostQuirks::default()), processor, None, ());
        let mut guard = power.lock().await;
        let _uic_regs = uic_hardware(&mmio_vmo, HCS_ACTIVE_LINK);
        let (ufshci_proxy, ufshci_stream) = create_proxy_and_stream::<fufs_phy::UfshciMarker>();
        let stream_controller = Arc::clone(&controller);
        let stream_task = fasync::Task::spawn(async move {
            handle_ufshci_stream(ufshci_stream, stream_controller).await.unwrap();
        });

        let mut dme_fut = pin!(ufshci_proxy.dme_get(0x1571, 0));
        assert!(futures::poll!(dme_fut.as_mut()).is_pending());
        assert!(
            wait_async(|| controller.uic_busy()).await,
            "DME_GET did not claim uic_busy before completion"
        );

        guard.mark_unreachable_unquiesced();
        phy_powered.store(false, Ordering::Release);
        assert_eq!(dme_fut.await.unwrap(), Err(zx::Status::BAD_STATE.into_raw()));
        assert_eq!(controller.with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
        assert!(!controller.uic_busy());

        drop(ufshci_proxy);
        stream_task.await;
    }

    #[fuchsia::test]
    fn test_apply_timeout_config_updates_processor_and_preserves_default_on_zero() {
        let (_mmio_vmo, processor) = create_test_processor();
        let mut device_manager = DeviceManager::new(HostQuirks::default());
        assert_eq!(processor.command_timeout(), COMMAND_TIMEOUT);
        assert_eq!(device_manager.drain_inflight_timeout, COMMAND_TIMEOUT + DRAIN_INFLIGHT_MARGIN);

        let custom = ufs_rust_config::Config {
            suspend_enabled: false,
            storage_power_management_enabled: false,
            enable_auto_hibernate: true,
            auto_hibernate_timer_us: AUTO_HIBERNATE_TIMER_US,
            command_timeout_sec: 15,
        };
        UfsDriver::apply_timeout_config(&processor, &mut device_manager, &custom);
        let fifteen_sec = zx::MonotonicDuration::from_seconds(15);
        assert_eq!(processor.command_timeout(), fifteen_sec);
        assert_eq!(device_manager.drain_inflight_timeout, fifteen_sec + DRAIN_INFLIGHT_MARGIN);

        // A zero `command_timeout_sec` preserves the existing timeout rather than arming a 0s
        // deadline.
        let zero_sec = ufs_rust_config::Config { command_timeout_sec: 0, ..custom };
        UfsDriver::apply_timeout_config(&processor, &mut device_manager, &zero_sec);
        assert_eq!(processor.command_timeout(), fifteen_sec);
        assert_eq!(device_manager.drain_inflight_timeout, fifteen_sec + DRAIN_INFLIGHT_MARGIN);
    }

    #[fuchsia::test]
    async fn test_init_controller_and_link_wakes_phy_before_hce_touch() {
        let mmio_vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let mut uic_regs = uic_hardware(&mmio_vmo, HCS_ACTIVE_LINK);
        let phy_powered = Arc::new(AtomicBool::new(false));
        let phy_powered_for_server = Arc::clone(&phy_powered);
        let phy_events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let phy_events_for_server = Arc::clone(&phy_events);

        let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
        let phy_task = fasync::Task::local(async move {
            while let Some(Ok(request)) = phy_stream.next().await {
                match request {
                    fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                        let is_active = state == fufs_phy::PowerState::Active;
                        phy_powered_for_server.store(is_active, Ordering::Release);
                        phy_events_for_server.lock().push("set_active");
                        responder.send(Ok(())).unwrap();
                    }
                    fufs_phy::UfsPhyRequest::Init { responder, .. } => {
                        phy_events_for_server.lock().push("init");
                        if phy_powered_for_server.load(Ordering::Acquire) {
                            responder.send(Ok(())).unwrap();
                        } else {
                            responder.send(Err(zx::Status::BAD_STATE.into_raw())).unwrap();
                        }
                    }
                    other => panic!("Unexpected UfsPhy request {other:?}"),
                }
            }
        });

        let hardware = thread::spawn(move || {
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeLinkStartup as u32),
                "DME_LINKSTARTUP not issued"
            );
            complete_uic_command(&mut uic_regs, 0, 0, |_| {});
        });

        let mmio = crate::testing::fake_ufs::FakeMmio::new(&mmio_vmo, PAGE_SIZE as usize)
            .with_power_domain(Arc::clone(&phy_powered));
        let scope = fasync::Scope::new();
        let device_manager = DeviceManager::new(HostQuirks::default());
        let controller =
            UfsDriver::init_controller_and_link(&device_manager, mmio, Some(&phy_proxy), &scope)
                .await
                .expect("init_controller_and_link must wake PHY before touching HCE or Init");
        assert_eq!(controller.with_mmio(|m| m.unpowered_accesses()), 0);
        assert_eq!(&*phy_events.lock(), &["set_active", "init"]);

        hardware.join().unwrap();
        scope.cancel().await;
        drop(phy_proxy);
        phy_task.await;
    }

    #[fuchsia::test]
    async fn test_prepare_phy_for_stop_wakes_sleep_and_quarantines_on_phy_failure() {
        // 1. SystemSleep + working PHY: wakes PHY to Active before quiesce touches UFSHCI MMIO.
        {
            let bti = fake_bti::FakeBti::create().unwrap();
            let mmio_vmo = zx::Vmo::create(crate::testing::MMIO_SIZE as u64).unwrap();
            mmio_vmo
                .write(
                    &HCS_ACTIVE_LINK.to_le_bytes(),
                    crate::registers::HostControllerStatusReg::OFFSET as u64,
                )
                .unwrap();
            mmio_vmo.write(&1u32.to_le_bytes(), HostControllerEnableReg::OFFSET as u64).unwrap();
            let phy_powered = Arc::new(AtomicBool::new(true));
            let mmio =
                crate::testing::fake_ufs::FakeMmio::new(&mmio_vmo, crate::testing::MMIO_SIZE)
                    .with_power_domain(Arc::clone(&phy_powered));
            let controller = Arc::new(UfsController::new(mmio));
            let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
            processor.init().unwrap();
            let utp = fake_utp(&mmio_vmo, &processor);

            let phy_powered_srv = Arc::clone(&phy_powered);
            let phy_calls = Arc::new(Mutex::new(Vec::new()));
            let phy_calls_srv = Arc::clone(&phy_calls);
            let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
            let phy_task = fasync::Task::local(async move {
                while let Some(Ok(req)) = phy_stream.next().await {
                    match req {
                        fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                            phy_calls_srv.lock().push(state);
                            phy_powered_srv
                                .store(state == fufs_phy::PowerState::Active, Ordering::Release);
                            responder.send(Ok(())).unwrap();
                        }
                        other => panic!("Unexpected UfsPhy request {other:?}"),
                    }
                }
            });

            let power = PowerControl::new(
                DeviceManager::new(HostQuirks::default()),
                Arc::clone(&processor),
                Some(phy_proxy),
                (),
            );
            let mut guard = power.lock().await;
            // Suspend blocks register access before the PHY powers down, as `enter_sleep` does.
            guard.block_register_access();
            phy_powered.store(false, Ordering::Release);
            guard.set_state(PowerState::SystemSleep, false);
            UfsDriver::prepare_phy_for_stop(&mut guard).await;
            assert_eq!(&*phy_calls.lock(), &[fufs_phy::PowerState::Active]);
            // `prepare_phy_for_stop` must keep interrupts masked so status bits latched across
            // sleep cannot fire on the interrupt thread before `quiesce` disables `HCE`.
            assert_eq!(InterruptEnableReg::read(&utp.regs).0, 0);
            assert_eq!(guard.quiesce().await, Ok(()));
            assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
            assert!(!HostControllerEnableReg::read(&utp.regs).host_controller_enable());
            assert_eq!(processor.quarantined_pmt_count(), 0);
            drop(guard);
            drop(power);
            phy_task.await;
        }

        // 2. SystemSleep + failing PHY: zero unclocked MMIO accesses and PMT quarantined.
        {
            let bti = fake_bti::FakeBti::create().unwrap();
            let mmio_vmo = zx::Vmo::create(crate::testing::MMIO_SIZE as u64).unwrap();
            mmio_vmo
                .write(
                    &HCS_ACTIVE_LINK.to_le_bytes(),
                    crate::registers::HostControllerStatusReg::OFFSET as u64,
                )
                .unwrap();
            mmio_vmo.write(&1u32.to_le_bytes(), HostControllerEnableReg::OFFSET as u64).unwrap();
            let phy_powered = Arc::new(AtomicBool::new(true));
            let mmio =
                crate::testing::fake_ufs::FakeMmio::new(&mmio_vmo, crate::testing::MMIO_SIZE)
                    .with_power_domain(Arc::clone(&phy_powered));
            let controller = Arc::new(UfsController::new(mmio));
            let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
            processor.init().unwrap();

            let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
            let (completion, rx) = io_callback();
            processor.submit_io(
                0,
                ScsiOpcode::build_write_10_cdb(0, 1),
                Some(DataTransfer::write(data_vmo, 0, PAGE_SIZE as u32)),
                completion,
            );
            assert!(slot_pinned(&processor, 0));
            processor.close_submissions();

            let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
            let phy_task = fasync::Task::local(async move {
                while let Some(Ok(req)) = phy_stream.next().await {
                    match req {
                        fufs_phy::UfsPhyRequest::SetPowerState { responder, .. } => {
                            responder.send(Err(zx::Status::IO.into_raw())).unwrap();
                        }
                        other => panic!("Unexpected UfsPhy request {other:?}"),
                    }
                }
            });

            let power = PowerControl::new(
                DeviceManager::new(HostQuirks::default()),
                Arc::clone(&processor),
                Some(phy_proxy),
                (),
            );
            let mut guard = power.lock().await;
            guard.block_register_access();
            phy_powered.store(false, Ordering::Release);
            guard.set_state(PowerState::SystemSleep, false);
            UfsDriver::prepare_phy_for_stop(&mut guard).await;
            assert_eq!(guard.quiesce().await, Err(UfsError::Timeout));
            assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);
            assert!(!slot_pinned(&processor, 0));
            assert_eq!(processor.quarantined_pmt_count(), 1);
            assert_eq!(rx.await.unwrap().io_status(), Err(zx::Status::IO));
            drop(guard);
            drop(power);
            phy_task.await;
        }

        // 3. PowerDown: leaves PHY untouched (rails stay off) and touches zero MMIO.
        {
            let (_mmio_vmo, processor) = create_test_processor();
            let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
            let phy_task = fasync::Task::local(async move {
                if let Some(req) = phy_stream.next().await {
                    panic!("PowerDown stop must not wake UfsPhy: {req:?}");
                }
            });
            let power = PowerControl::new(
                DeviceManager::new(HostQuirks::default()),
                Arc::clone(&processor),
                Some(phy_proxy),
                (),
            );
            let mut guard = power.lock().await;
            guard.power_off_after(|m| HostControllerEnableReg::from_raw(0).write(m));
            guard.set_state(PowerState::PowerDown, false);
            UfsDriver::prepare_phy_for_stop(&mut guard).await;
            assert_eq!(guard.quiesce().await, Ok(()));
            drop(guard);
            drop(power);
            phy_task.await;
        }

        // 4. Failed SystemSleep that stopped HCE before blocking access: leaves PHY untouched and
        // unpins cleanly without quarantining.
        {
            let (_mmio_vmo, processor) = create_test_processor();
            let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
            let (completion, rx) = io_callback();
            processor.submit_io(
                0,
                ScsiOpcode::build_write_10_cdb(0, 1),
                Some(DataTransfer::write(data_vmo, 0, PAGE_SIZE as u32)),
                completion,
            );
            processor.close_submissions();
            let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
            let phy_task = fasync::Task::local(async move {
                if let Some(req) = phy_stream.next().await {
                    panic!("Stopped controller must not wake UfsPhy: {req:?}");
                }
            });
            let power = PowerControl::new(
                DeviceManager::new(HostQuirks::default()),
                Arc::clone(&processor),
                Some(phy_proxy),
                (),
            );
            let mut guard = power.lock().await;
            guard.power_off_after(|m| HostControllerEnableReg::from_raw(0).write(m));
            guard.set_state(PowerState::SystemSleep, false);
            UfsDriver::prepare_phy_for_stop(&mut guard).await;
            assert_eq!(guard.quiesce().await, Ok(()));
            assert!(!slot_pinned(&processor, 0));
            assert_eq!(processor.quarantined_pmt_count(), 0);
            assert_eq!(rx.await.unwrap().io_status(), Err(zx::Status::IO));
            drop(guard);
            drop(power);
            phy_task.await;
        }
    }
}
