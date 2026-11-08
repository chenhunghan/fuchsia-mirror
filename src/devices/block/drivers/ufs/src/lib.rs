// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Universal Flash Storage (UFS) host controller driver entry point and node topology.
//!
//! The same driver binary backs two components: `ufs-rust-pdev` (platform device composite with
//! `pdev`, `phy` and optional `ufs-interconnect` parents) and `ufs-rust-pci` (PCI composite with
//! a `pci` parent).

pub mod block_device;
pub mod constants;
pub mod controller;
pub mod descriptors;
pub mod device_manager;
pub mod dma_buffer;
pub mod query;
pub mod registers;
pub mod request_list;
pub(crate) mod rpmb;
pub mod scsi;
pub(crate) mod server;
pub mod transfer;
pub mod transfer_request_processor;
pub mod uic;
pub mod upiu;

use crate::block_device::{BlockServerCompleter, LunBlockServer, UfsBlockDevice};
use crate::constants::{
    AUTO_HIBERNATE_TIMER_US, COMMAND_TIMEOUT, INTERCONNECT_AVG_BANDWIDTH_BPS,
    INTERCONNECT_INSTANCE, INTERCONNECT_PEAK_BANDWIDTH_BPS, INTERCONNECT_TAG_UFS,
    IRQ_SCHEDULER_ROLE, LU_ENABLED, MIN_MMIO_SIZE, PCI_INSTANCE, PDEV_INSTANCE, PHY_INSTANCE,
    TEST_UNIT_READY_ATTEMPTS, WORKER_SCHEDULER_ROLE,
};
use crate::controller::UfsController;
use crate::device_manager::{DeviceManager, DiscoveredUnit, Quirks};
use crate::query::Attribute;
use crate::rpmb::UfsRpmbServer;
use crate::scsi::ScsiOpcode;
use crate::server::UfsServer;
use crate::transfer_request_processor::TransferRequestProcessor;
use fdf_component::{Driver, DriverContext, DriverError};
use fidl::endpoints::RequestStream;
use fidl_fuchsia_device_fs as fdevfs;
use fidl_fuchsia_driver_framework::{NodeAddArgs, NodeControllerMarker, NodeMarker};
use fidl_fuchsia_hardware_block_volume as fvolume;
use fidl_fuchsia_hardware_interconnect as finterconnect;
use fidl_fuchsia_hardware_pci as fpci;
use fidl_fuchsia_hardware_platform_device as fpdev;
use fidl_fuchsia_hardware_rpmb as frpmb;
use fidl_fuchsia_hardware_ufs_phy::{self as fufs_phy, UfshciRequest};
use fidl_fuchsia_scheduler as fscheduler;
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceObj};
use fuchsia_sync::Mutex;
use futures::future::FusedFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use log::{error, info, warn};
use mmio::Mmio;
use mmio::region::MmioRegion;
use mmio::vmo::{VmoMapping, VmoMemory};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Register access type of the production driver: the mapped MMIO VMO of the host controller.
pub(crate) type DriverMmio = MmioRegion<VmoMemory>;

/// Block server of one LUN; sessions run on worker threads (`block_server::callback_interface`).
type DriverBlockServer = LunBlockServer<DriverMmio>;

/// Outgoing FIDL services exposed to child nodes and clients (`Volume` is indexed by LUN).
pub(crate) enum OutgoingService {
    /// Administrative `fuchsia.hardware.ufs.Service` protocol.
    Ufs(fidl_fuchsia_hardware_ufs::ServiceRequest),
    /// Per-LUN `fuchsia.hardware.block.volume.Service` protocol indexed by LUN.
    Volume(u8, fvolume::ServiceRequest),
    /// Replay Protected Memory Block (`fuchsia.hardware.rpmb.Service`) protocol.
    Rpmb(frpmb::ServiceRequest),
}

/// DFv2 child nodes of a UFS Logical Unit, kept alive while the driver runs.
struct LogicalUnit {
    /// Intermediate `scsi-block-device-0-<lun>` node offering the block volume service.
    _scsi_node: fdf_component::Node,
    _scsi_controller: fidl::endpoints::ClientEnd<NodeControllerMarker>,
    /// Leaf `block` node exported to devfs.
    _block_node: fidl::endpoints::ClientEnd<NodeMarker>,
    _block_controller: fidl::endpoints::ClientEnd<NodeControllerMarker>,
}

/// Client ends of the `ufs.rpmb` child node, kept alive while the driver runs.
struct RpmbNode {
    _node: fidl::endpoints::ClientEnd<NodeMarker>,
    _controller: fidl::endpoints::ClientEnd<NodeControllerMarker>,
}

/// Child nodes, block servers, and devfs connector server ends created by
/// [`UfsDriver::register_logical_units`].
struct RegisteredLuns {
    logical_units: Vec<LogicalUnit>,
    block_servers: HashMap<u8, Arc<DriverBlockServer>>,
    connector_servers: Vec<(u8, fidl::endpoints::ServerEnd<fdevfs::ConnectorMarker>)>,
}

/// Everything [`UfsDriver::bring_up_device`] creates once the device is initialized.
struct BoundDevice {
    inspector: fuchsia_inspect::Inspector,
    parent_node: fdf_component::Node,
    logical_units: Vec<LogicalUnit>,
    rpmb_node: Option<RpmbNode>,
    block_servers: Arc<HashMap<u8, Arc<DriverBlockServer>>>,
}

/// Parent bus of the host controller and the state to release on stop.
#[derive(Debug, Clone)]
enum Bus {
    /// Platform device; holds the interconnect path if a bandwidth vote was cast.
    Pdev { interconnect_path: Option<finterconnect::PathProxy> },
    /// PCI function.
    Pci { device: fpci::DeviceProxy },
}

/// Core UFS DFv2 driver state and owned node resources.
///
/// Teardown order matters. [`Driver::stop`] closes new submissions, terminates the block sessions,
/// drains remaining in-flight transfers, and quiesces the controller (`HCE = 0`, no more DMA)
/// before stopping the IRQ thread, cancelling the async scope (stopping the I/O stall watchdog and
/// FIDL/devfs servers), and releasing the parent bus, in that order. The field order below then
/// governs the drop order: IRQ workers before the async scope, and both before the nodes and the
/// processor that own the DMA buffers.
pub struct UfsDriver {
    /// Dedicated OS thread(s) servicing hardware interrupts. Stopped after `quiesce()` in `stop()`.
    interrupt_workers: InterruptWorkers,
    /// Hosts the I/O stall watchdog, devfs connector, PHY `Ufshci`, and FIDL server tasks. Declared
    /// after `interrupt_workers` so the IRQ thread is joined before it drops.
    scope: fasync::Scope,
    _inspector: fuchsia_inspect::Inspector,
    _parent_node: fdf_component::Node,
    _logical_units: Vec<LogicalUnit>,
    _rpmb_node: Option<RpmbNode>,
    bus: Bus,
    /// Quiesced in `stop()`.
    processor: Arc<TransferRequestProcessor<DriverMmio>>,
    /// Per-LUN block servers; their sessions are terminated in `stop()`.
    block_servers: Arc<HashMap<u8, Arc<DriverBlockServer>>>,
}

/// Hardware resources acquired from either a Platform Device (SoC) or PCI parent node.
struct HardwareResources {
    mmio: DriverMmio,
    interrupt: zx::Interrupt,
    bti: zx::Bti,
    phy_proxy: Option<fufs_phy::UfsPhyProxy>,
    bus: Bus,
    quirks: Quirks,
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
/// Unlike the crate-wide `From<UfsError> for zx::Status`, a failed UIC command (`UicError`) is
/// reported as `IO` rather than `INTERNAL`.
fn ufs_error_to_raw_status(err: UfsError) -> i32 {
    match err {
        UfsError::Timeout => zx::Status::TIMED_OUT.into_raw(),
        UfsError::UicError { .. } => zx::Status::IO.into_raw(),
        UfsError::InvalidSlot(_)
        | UfsError::CheckCondition
        | UfsError::IoError
        | UfsError::InvalidParameter => zx::Status::INTERNAL.into_raw(),
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

/// Sends TEST UNIT READY to `lun`, retrying up to [`TEST_UNIT_READY_ATTEMPTS`] times to clear
/// any initial Unit Attention condition after reset.
pub(crate) async fn test_unit_ready<M: mmio::Mmio>(
    processor: &TransferRequestProcessor<M>,
    lun: u8,
) -> Result<(), UfsError> {
    let mut result = Ok(());
    for attempt in 1..=TEST_UNIT_READY_ATTEMPTS {
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        result = processor.send_scsi_command(lun, cdb, None).await;
        match &result {
            Ok(()) => break,
            Err(e) => info!(
                lun,
                attempt,
                max_attempts = TEST_UNIT_READY_ATTEMPTS,
                err:% = e;
                "TEST UNIT READY attempt failed"
            ),
        }
    }
    result
}

/// Serves the PHY driver's `Ufshci` callback stream: the `DME_SET`/`DME_GET` requests it issues
/// against the host controller while `UfsPhy.Init` or `PrePowerModeChange` is in progress.
async fn handle_ufshci_stream<M: mmio::Mmio + Send + Sync + 'static>(
    mut stream: fufs_phy::UfshciRequestStream,
    controller: Arc<UfsController<M>>,
) -> Result<(), fidl::Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            UfshciRequest::DmeSet { mib_attribute, gen_selector_index, value, responder } => {
                let res = controller.dme_set(mib_attribute, gen_selector_index, value);
                responder.send(res.await.map_err(ufs_error_to_raw_status))?;
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
            quirks: Quirks::for_pdev(),
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
        let quirks = Quirks::from_pci_ids(info.vendor_id, info.device_id);
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

    /// Resets and enables the UFSHCI controller, initializes any attached M-PHY, and brings up the
    /// UniPro link (see [`UfsController::link_startup`] for the retry policy). Disables `HCE` if
    /// PHY initialization or link startup fails.
    async fn init_controller_and_link(
        mmio: DriverMmio,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        scope: &fasync::Scope,
    ) -> Result<Arc<UfsController<DriverMmio>>, DriverError> {
        let controller = Arc::new(UfsController::new(mmio));
        check_step("Host controller enable", controller.reset_and_enable().await)?;
        if let Err(err) = Self::init_phy_and_link(&controller, phy_proxy, scope).await {
            Self::disable_host_controller_or_log(&controller).await;
            return Err(err);
        }
        Ok(controller)
    }

    /// Runs `UfsPhy.Init` (serving its `Ufshci` callbacks on `scope`) if a PHY is present, then
    /// `DME_LINKSTARTUP`.
    async fn init_phy_and_link(
        controller: &Arc<UfsController<DriverMmio>>,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        scope: &fasync::Scope,
    ) -> Result<(), DriverError> {
        if let Some(phy) = phy_proxy {
            info!("UFS PHY detected, performing PHY initialization...");
            let (client_end, server_end) =
                fidl::endpoints::create_endpoints::<fufs_phy::UfshciMarker>();
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
        check_step("UFS link startup", controller.link_startup().await)
    }

    /// [`UfsController::disable_host_controller`] for failure paths, where the error can only be
    /// logged.
    async fn disable_host_controller_or_log(controller: &UfsController<DriverMmio>) {
        if let Err(e) = controller.disable_host_controller().await {
            error!(err:% = e; "Failed to disable host controller");
        }
    }

    /// Registers DFv2 `scsi-block-device-0-<lun>` and `block` child nodes for all enabled LUNs
    /// with a supported logical block size.
    async fn register_logical_units<'a>(
        device_manager: &DeviceManager,
        units: &[DiscoveredUnit],
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
        parent_node: &fdf_component::Node,
        service_fs: &mut ServiceFs<ServiceObj<'a, OutgoingService>>,
    ) -> Result<RegisteredLuns, DriverError> {
        let mut logical_units = Vec::new();
        let mut block_servers = HashMap::new();
        let mut connector_servers = Vec::new();

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

            if let Err(e) = test_unit_ready(processor, lun).await {
                warn!(lun, err:% = e; "TEST UNIT READY failed");
            }

            let device_flags = UfsBlockDevice::<DriverMmio>::device_flags(unit);
            let block_server =
                UfsBlockDevice::new(lun, block_size, unit, device_flags, Arc::clone(processor))
                    .into_server();
            block_servers.insert(lun, Arc::clone(&block_server));

            let device_name = format!("scsi-block-device-0-{lun}");
            let offer = fdf_component::ServiceOffer::<fvolume::ServiceMarker>::new()
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

            let (connector, connector_server) =
                fidl::endpoints::create_endpoints::<fdevfs::ConnectorMarker>();
            connector_servers.push((lun, connector_server));

            let devfs_args = fidl_fuchsia_driver_framework::DevfsAddArgs {
                connector: Some(connector),
                class_name: Some("block".to_string()),
                ..Default::default()
            };
            let block_args = NodeAddArgs {
                name: Some("block".to_string()),
                devfs_args: Some(devfs_args),
                ..Default::default()
            };

            let scsi_node = fdf_component::Node::from(scsi_node.into_proxy());
            let (block_node, block_controller) =
                scsi_node.add_owned_child(block_args).await.map_err(|status| {
                    error!(lun, status:%; "Failed to add block node");
                    DriverError::from(status)
                })?;

            logical_units.push(LogicalUnit {
                _scsi_node: scsi_node,
                _scsi_controller: scsi_controller,
                _block_node: block_node,
                _block_controller: block_controller,
            });
        }

        if logical_units.is_empty() {
            error!("No usable logical units found");
            return Err(zx::Status::BAD_STATE.into());
        }
        // Completions of block requests are routed to their LUN's server from the interrupt path.
        processor.set_io_completer(Arc::new(BlockServerCompleter::new(
            block_servers.iter().map(|(&lun, server)| (lun, server)),
        )));

        Ok(RegisteredLuns { logical_units, block_servers, connector_servers })
    }

    /// Probes the RPMB Well-Known LUN ([`crate::constants::RPMB_LUN`]) and, if it is ready,
    /// creates its server and the `ufs.rpmb` child node offering `fuchsia.hardware.rpmb.Service`.
    ///
    /// If `add_owned_child` fails, a warning is logged and `(Some(rpmb_server), None)` is still
    /// returned so `OutgoingService::Rpmb` remains served on the driver's outgoing directory.
    async fn init_rpmb(
        parent_node: &fdf_component::Node,
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

        let rpmb_offer = fdf_component::ServiceOffer::<frpmb::ServiceMarker>::new()
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
        connector_servers: Vec<(u8, fidl::endpoints::ServerEnd<fdevfs::ConnectorMarker>)>,
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
                    let stream = fidl_fuchsia_storage_block::BlockRequestStream::from_channel(
                        fasync::Channel::from_channel(channel),
                    );
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
                            OutgoingService::Ufs(
                                fidl_fuchsia_hardware_ufs::ServiceRequest::Device(stream),
                            ) => {
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

    /// Releases the parent bus after the controller is stopped: drops the interconnect vote
    /// (pdev) or disables bus mastering (PCI).
    async fn release_bus(bus: &Bus) {
        match bus {
            Bus::Pdev { interconnect_path: Some(path) } => {
                let _ =
                    check_fidl("SetBandwidth", path.set_bandwidth(&bandwidth_request(0, 0)).await);
            }
            Bus::Pdev { interconnect_path: None } => {}
            Bus::Pci { device } => {
                let _ = check_fidl("SetBusMastering", device.set_bus_mastering(false).await);
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
                    enable_auto_hibernate: true,
                    auto_hibernate_timer_us: AUTO_HIBERNATE_TIMER_US,
                }
            }
        }
    }

    /// Brings up the controller, link and device, then registers the child nodes. Stops the
    /// controller on failure.
    async fn initialize_with_resources(
        context: &mut DriverContext,
        resources: HardwareResources,
        config: &ufs_rust_config::Config,
    ) -> Result<Self, DriverError> {
        let HardwareResources { mmio, interrupt, bti, phy_proxy, bus, quirks } = resources;
        let scope = fasync::Scope::new();

        let controller =
            match Self::init_controller_and_link(mmio, phy_proxy.as_ref(), &scope).await {
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
                Self::disable_host_controller_or_log(&controller).await;
                return Err(status.into());
            }
        };

        // From here on the controller may DMA, so every failure must go through the single
        // cleanup block below.
        let mut interrupt_workers = InterruptWorkers::default();
        let result = Self::start_processor_and_bring_up(
            context,
            &scope,
            &mut interrupt_workers,
            interrupt,
            quirks,
            config,
            phy_proxy.as_ref(),
            &controller,
            &processor,
        )
        .await;
        let bound = match result {
            Ok(bound) => bound,
            Err(err) => {
                interrupt_workers.stop().await;
                // Cancel background tasks before quiescing the controller and releasing the
                // processor's buffers.
                scope.cancel().await;
                let _ = processor.quiesce().await;
                return Err(err);
            }
        };

        info!(luns = bound.logical_units.len(); "Bind Success");
        Ok(Self {
            interrupt_workers,
            scope,
            _inspector: bound.inspector,
            _parent_node: bound.parent_node,
            _logical_units: bound.logical_units,
            _rpmb_node: bound.rpmb_node,
            bus,
            processor,
            block_servers: bound.block_servers,
        })
    }

    /// Programs the request list, enables interrupts, starts the IRQ thread (stored in
    /// `interrupt_workers` even on failure so the caller can stop it) and the I/O timeout task,
    /// then runs [`Self::bring_up_device`]. The caller cleans up on error.
    async fn start_processor_and_bring_up(
        context: &mut DriverContext,
        scope: &fasync::Scope,
        interrupt_workers: &mut InterruptWorkers,
        interrupt: zx::Interrupt,
        quirks: Quirks,
        config: &ufs_rust_config::Config,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        controller: &UfsController<DriverMmio>,
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
    ) -> Result<BoundDevice, DriverError> {
        check_step("Transfer request processor init", processor.init())?;
        controller.enable_interrupts();
        *interrupt_workers = spawn_interrupt_thread(interrupt, Arc::clone(processor))?;
        spawn_io_timeout_task(scope, Arc::clone(processor));
        Self::bring_up_device(context, quirks, config, phy_proxy, controller, processor, scope)
            .await
    }

    /// UTP bring-up, device configuration, and child node registration. QEMU skips the UniPro,
    /// power mode, and auto-hibernate steps.
    async fn bring_up_device(
        context: &mut DriverContext,
        quirks: Quirks,
        config: &ufs_rust_config::Config,
        phy_proxy: Option<&fufs_phy::UfsPhyProxy>,
        controller: &UfsController<DriverMmio>,
        processor: &Arc<TransferRequestProcessor<DriverMmio>>,
        scope: &fasync::Scope,
    ) -> Result<BoundDevice, DriverError> {
        let inspector = fuchsia_inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        if let Err(status) = context.publish_inspect(&inspector, scope.to_handle()) {
            warn!(status:%; "Failed to publish UFS Inspect tree");
        }
        ufs_node.record_child("config", |node| config.record_inspect(node));
        controller.record_inspect(&ufs_node);
        processor.record_inspect(&ufs_node);

        info!("Sending NOP OUT UPIU...");
        check_step("NOP OUT", processor.send_nop_out_upiu().await)?;

        let mut device_manager = DeviceManager::new(quirks);
        device_manager.init_inspect(&ufs_node);
        check_step("fDeviceInit", device_manager.device_init(processor).await)?;
        check_step("bRefClkFreq", device_manager.init_reference_clock(processor).await)?;
        if !quirks.qemu {
            check_step(
                "UniPro attributes",
                device_manager.init_unipro_attributes(processor).await,
            )?;
            // Stay usable in PWM if the HS switch fails.
            if let Err(e) = device_manager.init_uic_power_mode(processor, phy_proxy).await {
                warn!(err:% = e; "Failed to switch to HS Fast_Mode, continuing in PWM");
            }
            check_step("UFS power mode", device_manager.init_ufs_power_mode(processor).await)?;
        }
        match device_manager.read_attribute(processor, Attribute::BootLunEn, 0).await {
            Ok(boot_lun) => info!(boot_lun; "bBootLunEn read"),
            Err(e) => warn!(err:% = e; "Failed to read bBootLunEn"),
        }
        check_step(
            "Descriptor read",
            device_manager.read_device_and_geometry_descriptors(processor).await,
        )?;

        let units = device_manager.discover_unit_descriptors(processor).await;
        device_manager.configure_features(processor, &units).await;
        if !quirks.qemu {
            let ah8_timer_us =
                if config.enable_auto_hibernate { config.auto_hibernate_timer_us } else { 0 };
            controller.configure_auto_hibernate(ah8_timer_us);
        }

        // Register LUN block devices, initialize RPMB and serve the outgoing FIDL protocols.
        let mut service_fs = ServiceFs::new();
        let parent_node = context.take_node().map_err(|err| {
            error!(err:?; "Failed to take DFv2 parent node");
            err
        })?;
        let RegisteredLuns { logical_units, block_servers, connector_servers } =
            Self::register_logical_units(
                &device_manager,
                &units,
                processor,
                &parent_node,
                &mut service_fs,
            )
            .await?;
        device_manager.record_logical_unit_count(logical_units.len());
        inspector.root().record(ufs_node);
        let block_servers = Arc::new(block_servers);

        let (rpmb_server, rpmb_node) =
            Self::init_rpmb(&parent_node, &mut service_fs, &device_manager, processor).await;
        let ufs_server = Arc::new(UfsServer::new(Arc::clone(processor)));
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
        let bus = resources.bus.clone();
        match Self::initialize_with_resources(&mut context, resources, &config).await {
            Ok(driver) => Ok(driver),
            Err(err) => {
                Self::release_bus(&bus).await;
                Err(err)
            }
        }
    }

    async fn stop(&self) {
        // Close new submissions, terminate all block sessions (stopping FIFO loops immediately and
        // waiting up to `drain_deadline` for their active requests to complete while the IRQ thread
        // is running), drain any remaining in-flight transfers, and quiesce the controller
        // (`HCE = 0`, stopping DMA and failing any wedged slots so `terminate` is guaranteed to
        // unblock) before tearing down the IRQ thread, async scope, and parent bus.
        self.processor.close_submissions();
        let drain_deadline = zx::MonotonicInstant::after(COMMAND_TIMEOUT);
        let mut terminate_sessions = std::pin::pin!(
            futures::future::join_all(self.block_servers.values().map(|server| {
                let server = Arc::clone(server);
                fasync::unblock(move || server.session_manager().terminate())
            }))
            .fuse()
        );
        let mut drain_timer = std::pin::pin!(fasync::Timer::new(drain_deadline).fuse());
        futures::select! {
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
        if let Err(e) = self.processor.quiesce().await {
            error!(err:? = e; "Failed to quiesce UFS controller");
        }
        if !terminate_sessions.is_terminated() {
            terminate_sessions.await;
        }
        self.interrupt_workers.stop().await;
        self.scope.to_handle().cancel().await;
        Self::release_bus(&self.bus).await;
    }
}

fdf_component::driver_register!(UfsDriver);

/// Spawns [`TransferRequestProcessor::run_io_timeout_task`] on `scope`: the stall-logging
/// watchdog that warns on long-running UTP slots while DMA remains live.
fn spawn_io_timeout_task<M>(scope: &fasync::Scope, processor: Arc<TransferRequestProcessor<M>>)
where
    M: Mmio + Send + Sync + 'static,
{
    scope.spawn(async move { processor.run_io_timeout_task().await });
}

/// Dedicated interrupt worker threads that wait on hardware interrupts via `zx::Interrupt::wait`
/// without hopping through the driver's async executor.
#[derive(Default)]
struct InterruptWorkers {
    inner: Mutex<InterruptWorkersInner>,
    /// Set by [`InterruptWorkers::take_and_destroy`] to signal worker threads to exit cleanly.
    stop_requested: Arc<AtomicBool>,
}

#[derive(Default)]
struct InterruptWorkersInner {
    /// Duplicate interrupt handles destroyed by [`InterruptWorkers::take_and_destroy`] to unblock
    /// each worker thread's `zx::Interrupt::wait` with `zx::Status::CANCELED`.
    interrupts: Vec<zx::Interrupt>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl InterruptWorkers {
    /// Destroys the interrupt objects so any blocked `wait()` returns `zx::Status::CANCELED` and
    /// returns the thread join handles.
    fn take_and_destroy(&self) -> Vec<std::thread::JoinHandle<()>> {
        self.stop_requested.store(true, Ordering::Release);
        let mut inner = self.inner.lock();
        for irq in inner.interrupts.drain(..) {
            if let Err(status) = irq.destroy() {
                warn!(status:%; "Failed to destroy UFS interrupt on stop");
            }
        }
        std::mem::take(&mut inner.threads)
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
    let stop_requested = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_requested);
    let handle = std::thread::Builder::new()
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
        inner: Mutex::new(InterruptWorkersInner {
            interrupts: vec![irq_for_stop],
            threads: vec![handle],
        }),
        stop_requested,
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

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registers::{HostControllerEnableReg, InterruptStatusReg};
    use crate::testing::{
        OCS_SUCCESS, PAGE_SIZE, create_test_processor, fake_utp, io_callback, scsi_response,
        slot_pinned, wait_for_any_doorbell,
    };
    use crate::transfer_request_processor::DataTransfer;
    use mmio::{ReadableRegister, Register};
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
        assert_eq!(ufs_error_to_raw_status(UfsError::IoError), zx::Status::INTERNAL.into_raw());
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

        let hardware = std::thread::spawn(move || {
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

        assert_eq!(test_unit_ready(&processor, 0).await, Ok(()));
        assert_eq!(test_unit_ready(&processor, 0).await, Err(UfsError::IoError));
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
        mmio::WritableRegister::write(&is, &mut utp.regs);
        irq_trigger.trigger(zx::BootInstant::get()).unwrap();

        assert_eq!(rx.await.unwrap().into_io_status(), Ok(()));
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

        // Step 1 of stop(): close new submissions while in-flight completions remain active.
        processor.close_submissions();
        let (late_completion, late_rx) = io_callback();
        processor.submit_io(0, ScsiOpcode::build_test_unit_ready_cdb(), None, late_completion);
        assert_eq!(late_rx.await.unwrap().into_io_status(), Err(zx::Status::IO));

        // Complete the in-flight write on the dedicated IRQ worker while drain_inflight awaits.
        let hardware = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            utp.write_completion(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
            let mut is = InterruptStatusReg::from_raw(0);
            is.set_utp_transfer_request_completion_status(true);
            mmio::WritableRegister::write(&is, &mut utp.regs);
            irq_trigger.trigger(zx::BootInstant::get()).unwrap();
            utp
        });

        assert_eq!(processor.drain_inflight(COMMAND_TIMEOUT).await, Ok(()));
        let _utp = hardware.join().unwrap();
        assert_eq!(rx.await.unwrap().into_io_status(), Ok(()));
        assert_eq!(processor.busy_slot_count(), 0);
        assert!(!slot_pinned(&processor, 0));

        assert_eq!(processor.quiesce().await, Ok(()));
        workers.stop().await;
    }

    #[fuchsia::test]
    async fn test_stop_quiesce_aborts_unresponsive_inflight_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let mut hce = HostControllerEnableReg::from_raw(0);
        hce.set_host_controller_enable(true);
        mmio::WritableRegister::write(&hce, &mut utp.regs);

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

        assert_eq!(processor.quiesce().await, Ok(()));
        assert!(!HostControllerEnableReg::read(&utp.regs).host_controller_enable());
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(rx.await.unwrap().into_io_status(), Err(zx::Status::IO));
    }
}
