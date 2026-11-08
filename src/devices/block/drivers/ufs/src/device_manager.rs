// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFS device initialization, descriptor/flag/attribute queries, UniPro link configuration and
//! device features.

use crate::UfsError;
use crate::constants::{
    DEVICE_INIT_POLL_INTERVAL, DEVICE_INIT_TIMEOUT, DL_AFC0_REQ_TIMEOUT_VAL,
    DL_FC0_PROTECTION_TIMEOUT_VAL, DL_TC0_REPLAY_TIMEOUT_VAL, DME_LOCAL_AFC0_REQ_TIMEOUT_VAL,
    DME_LOCAL_FC0_PROTECTION_TIMEOUT_VAL, DME_LOCAL_TC0_REPLAY_TIMEOUT_VAL,
    EXTENDED_FEATURE_WRITE_BOOSTER, FAST_PWR_MODE, GEOMETRY_SIZE_UNIT, HIGHEST_ACTIVE_ICC_LEVEL,
    HS_GEAR_ADAPT_MIN, HS_SERIES_B, INTEL_PEER_T_ACTIVATE_INCREMENT, INTEL_POST_POWER_MODE_DELAY,
    INTEL_VENDOR_ID, LINK_STARTUP_ATTEMPTS, LINK_STARTUP_RETRY_DELAY, LINK_STATE_ACTIVE,
    LU_ENABLED, LUN_SCAN_COUNT_8, LUN_SCAN_COUNT_32, MAX_NUMBER_LU_8, MAX_NUMBER_LU_32,
    MAX_TRANSFER_BYTES, MIN_LOGICAL_BLOCK_SIZE, PA_ACTIVERXDATALANES, PA_ACTIVETXDATALANES,
    PA_AVAILRXDATALANES, PA_AVAILTXDATALANES, PA_GRANULARITY, PA_HSSERIES, PA_INITIAL_ADAPT,
    PA_LOCALVERINFO, PA_MAXRXHSGEAR, PA_PWRMODEUSERDATA0, PA_PWRMODEUSERDATA1, PA_PWRMODEUSERDATA2,
    PA_PWRMODEUSERDATA3, PA_PWRMODEUSERDATA4, PA_PWRMODEUSERDATA5, PA_REMOTEVERINFO, PA_RXGEAR,
    PA_RXTERMINATION, PA_TACTIVATE, PA_TXGEAR, PA_TXHSADAPTTYPE, PA_TXTERMINATION,
    POWER_CONDITION_ACTIVE, POWER_MODE_CHANGE_TIMEOUT, QEMU_UFS_DEVICE_ID, REDHAT_VENDOR_ID,
    SEGMENT_SIZE_UNIT, TERMINATION_ON, WRITE_BOOSTER_LIFETIME_EXCEEDED,
};
use crate::descriptors::{
    Descriptor, DeviceDescriptor, GeometryDescriptor, UnitDescriptor, WriteBoosterBufferType,
};
use crate::query::{Attribute, Flag, QueryUpiu, RefClkFreq};
use crate::transfer_request_processor::TransferRequestProcessor;
use fidl_fuchsia_hardware_ufs_phy as fufs_phy;
use fuchsia_inspect::{self as inspect, Property};
use log::{error, info, warn};
use mmio::Mmio;

/// Host controller quirks, detected from the PCI vendor and device IDs or implied by the platform
/// device bus.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Quirks {
    /// Intel host: 19.2 MHz `bRefClkFreq` and the link timing workarounds.
    pub intel: bool,
    /// QEMU: no UniPro attributes, power mode change or auto-hibernate.
    pub qemu: bool,
}

impl Quirks {
    /// Returns the quirks for a platform device host.
    pub fn for_pdev() -> Self {
        Self::default()
    }

    /// Returns the quirks for a PCI function.
    pub fn from_pci_ids(vendor_id: u16, device_id: u16) -> Self {
        let qemu = vendor_id == REDHAT_VENDOR_ID && device_id == QEMU_UFS_DEVICE_ID;
        Self { intel: !qemu && vendor_id == INTEL_VENDOR_ID, qemu }
    }
}

/// UFS device power mode (`bCurrentPowerMode`, UFS 3.1/4.0 sections 13.4 and 14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UfsPowerMode {
    /// Idle.
    Idle = 0x00,
    /// Entering `Active`.
    PreActive = 0x10,
    /// Active.
    Active = 0x11,
    /// Entering `Sleep`.
    PreSleep = 0x20,
    /// Sleep.
    Sleep = 0x21,
    /// Entering `PowerDown`.
    PrePowerDown = 0x30,
    /// PowerDown.
    PowerDown = 0x33,
}

/// A Unit Descriptor read by [`DeviceManager::discover_unit_descriptors`] together with the LUN
/// it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiscoveredUnit {
    /// LUN index (`0..lun_scan_count()`).
    pub lun: u8,
    /// The LUN's Unit Descriptor.
    pub descriptor: UnitDescriptor,
}

/// Returns the number of LUNs a device may have, from Geometry `bMaxNumberLU` (UFS 3.1/4.0 section
/// 14.1.4.4: 0 means 8 logical units, 1 means 32), or `None` for a reserved value.
fn lun_scan_count_from_max_number_lu(max_number_lu: u8) -> Option<u8> {
    match max_number_lu {
        MAX_NUMBER_LU_8 => Some(LUN_SCAN_COUNT_8),
        MAX_NUMBER_LU_32 => Some(LUN_SCAN_COUNT_32),
        _ => None,
    }
}

/// Inspect nodes and properties owned by [`DeviceManager`] under `root/ufs/controller`.
#[derive(Debug, Default)]
struct DeviceInspect {
    _controller_node: inspect::Node,
    _attributes_node: inspect::Node,
    _unipro_node: inspect::Node,
    _background_op_node: inspect::Node,
    _writebooster_node: inspect::Node,
    _max_transfer_bytes: inspect::UintProperty,
    logical_unit_count: inspect::UintProperty,
    reference_clock: inspect::StringProperty,
    power_condition: inspect::UintProperty,
    link_state: inspect::UintProperty,
    b_boot_lun_en: inspect::UintProperty,
    b_current_power_mode: inspect::UintProperty,
    b_active_icc_level: inspect::UintProperty,
    remote_version: inspect::UintProperty,
    local_version: inspect::UintProperty,
    host_t_activate: inspect::UintProperty,
    device_t_activate: inspect::UintProperty,
    host_granularity: inspect::UintProperty,
    device_granularity: inspect::UintProperty,
    pa_active_tx_data_lanes: inspect::UintProperty,
    pa_active_rx_data_lanes: inspect::UintProperty,
    pa_max_rx_hs_gear: inspect::UintProperty,
    pa_tx_gear: inspect::UintProperty,
    pa_rx_gear: inspect::UintProperty,
    tx_termination: inspect::BoolProperty,
    rx_termination: inspect::BoolProperty,
    pa_hs_series: inspect::UintProperty,
    power_mode: inspect::UintProperty,
    is_background_op_enabled: inspect::BoolProperty,
    is_write_booster_enabled: inspect::BoolProperty,
    writebooster_buffer_flush_during_hibernate: inspect::BoolProperty,
    writebooster_buffer_flush_enabled: inspect::BoolProperty,
    write_booster_buffer_type: inspect::UintProperty,
    user_space_configuration_option: inspect::UintProperty,
    write_booster_dedicated_lu: inspect::UintProperty,
    write_booster_buffer_size_in_bytes: inspect::UintProperty,
}

impl DeviceInspect {
    fn new(ufs_node: &inspect::Node) -> Self {
        let controller_node = ufs_node.create_child("controller");
        let attributes_node = controller_node.create_child("attributes");
        let unipro_node = controller_node.create_child("unipro");
        let background_op_node = controller_node.create_child("background_operations");
        let writebooster_node = controller_node.create_child("writebooster");
        Self {
            _max_transfer_bytes: controller_node
                .create_uint("max_transfer_bytes", MAX_TRANSFER_BYTES),
            logical_unit_count: controller_node.create_uint("logical_unit_count", 0),
            reference_clock: controller_node.create_string("reference_clock", ""),
            power_condition: controller_node.create_uint("PowerCondition", POWER_CONDITION_ACTIVE),
            link_state: controller_node.create_uint("LinkState", LINK_STATE_ACTIVE),
            b_boot_lun_en: attributes_node.create_uint("bBootLunEn", 0),
            b_current_power_mode: attributes_node
                .create_uint("bCurrentPowerMode", UfsPowerMode::Active as u64),
            b_active_icc_level: attributes_node
                .create_uint("bActiveICCLevel", u64::from(HIGHEST_ACTIVE_ICC_LEVEL)),
            remote_version: unipro_node.create_uint("remote_version", 0),
            local_version: unipro_node.create_uint("local_version", 0),
            host_t_activate: unipro_node.create_uint("host_t_activate", 0),
            device_t_activate: unipro_node.create_uint("device_t_activate", 0),
            host_granularity: unipro_node.create_uint("host_granularity", 0),
            device_granularity: unipro_node.create_uint("device_granularity", 0),
            pa_active_tx_data_lanes: unipro_node.create_uint("PA_ActiveTxDataLanes", 0),
            pa_active_rx_data_lanes: unipro_node.create_uint("PA_ActiveRxDataLanes", 0),
            pa_max_rx_hs_gear: unipro_node.create_uint("PA_MaxRxHSGear", 0),
            pa_tx_gear: unipro_node.create_uint("PA_TxGear", 0),
            pa_rx_gear: unipro_node.create_uint("PA_RxGear", 0),
            tx_termination: unipro_node.create_bool("tx_termination", false),
            rx_termination: unipro_node.create_bool("rx_termination", false),
            pa_hs_series: unipro_node.create_uint("PA_HSSeries", 0),
            power_mode: unipro_node.create_uint("power_mode", 0),
            is_background_op_enabled: background_op_node
                .create_bool("is_background_op_enabled", false),
            is_write_booster_enabled: writebooster_node
                .create_bool("is_write_booster_enabled", false),
            writebooster_buffer_flush_during_hibernate: writebooster_node
                .create_bool("writebooster_buffer_flush_during_hibernate", false),
            writebooster_buffer_flush_enabled: writebooster_node
                .create_bool("writebooster_buffer_flush_enabled", false),
            write_booster_buffer_type: writebooster_node
                .create_uint("write_booster_buffer_type", 0),
            user_space_configuration_option: writebooster_node
                .create_uint("user_space_configuration_option", 0),
            write_booster_dedicated_lu: writebooster_node
                .create_uint("write_booster_dedicated_lu", 0),
            write_booster_buffer_size_in_bytes: writebooster_node
                .create_uint("write_booster_buffer_size_in_bytes", 0),
            _controller_node: controller_node,
            _attributes_node: attributes_node,
            _unipro_node: unipro_node,
            _background_op_node: background_op_node,
            _writebooster_node: writebooster_node,
        }
    }
}

/// UFS device state: quirks and cached Device and Geometry Descriptors.
#[derive(Debug, Default)]
pub struct DeviceManager {
    quirks: Quirks,
    /// Cached Device Descriptor (`IDN = 0x00`).
    device_descriptor: DeviceDescriptor,
    /// Cached Geometry Descriptor (`IDN = 0x07`).
    geometry_descriptor: GeometryDescriptor,
    /// Number of LUNs to scan, derived from Geometry `bMaxNumberLU`.
    lun_scan_count: u8,
    /// LUN index of the active WriteBooster buffer (`0` for shared buffers).
    write_booster_buffer_lun: u8,
    inspect: DeviceInspect,
}

impl DeviceManager {
    /// Creates a `DeviceManager` for a host with `quirks`.
    pub fn new(quirks: Quirks) -> Self {
        Self { quirks, ..Default::default() }
    }

    /// Attaches the `controller` Inspect subtree and properties under `ufs_node`.
    pub fn init_inspect(&mut self, ufs_node: &inspect::Node) {
        self.inspect = DeviceInspect::new(ufs_node);
    }

    /// Records the number of bound logical units in `controller/logical_unit_count`.
    pub fn record_logical_unit_count(&self, count: usize) {
        self.inspect.logical_unit_count.set(count as u64);
    }

    /// Returns the cached Device Descriptor.
    pub fn device_descriptor(&self) -> &DeviceDescriptor {
        &self.device_descriptor
    }

    /// Returns the cached Geometry Descriptor.
    pub fn geometry_descriptor(&self) -> &GeometryDescriptor {
        &self.geometry_descriptor
    }

    /// Installs descriptors without reading them from a device.
    #[cfg(test)]
    pub(crate) fn set_descriptors(
        &mut self,
        device: DeviceDescriptor,
        geometry: GeometryDescriptor,
    ) {
        self.device_descriptor = device;
        self.geometry_descriptor = geometry;
    }

    /// Returns `bNumberLU` from the cached Device Descriptor.
    pub fn logical_unit_count(&self) -> u8 {
        self.device_descriptor.number_lu
    }

    /// Returns the number of LUNs to scan, from Geometry `bMaxNumberLU` (`0` before
    /// [`Self::read_device_and_geometry_descriptors`]).
    pub fn lun_scan_count(&self) -> u8 {
        self.lun_scan_count
    }

    /// Sends `req` and maps a non-zero Query Response to `IoError`.
    async fn send_checked_query<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        req: &QueryUpiu,
    ) -> Result<QueryUpiu, UfsError> {
        let resp = processor.send_query_request_upiu(req).await?;
        if resp.header.response != 0 {
            warn!(
                opcode = req.opcode,
                idn = req.idn,
                index = req.index,
                selector = req.selector,
                response = resp.header.response;
                "Query request failed"
            );
            return Err(UfsError::IoError);
        }
        Ok(resp)
    }

    /// Reads a device flag.
    pub async fn read_flag<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        flag: Flag,
    ) -> Result<bool, UfsError> {
        let resp = self.send_checked_query(processor, &QueryUpiu::read_flag(flag)).await?;
        Ok(resp.flag_value())
    }

    /// Sets a device flag at index 0.
    pub async fn set_flag<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        flag: Flag,
    ) -> Result<(), UfsError> {
        self.set_flag_at_index(processor, flag, 0).await
    }

    /// Sets a device flag at `index` (LUN index for LU-dedicated WriteBooster flags).
    pub async fn set_flag_at_index<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        flag: Flag,
        index: u8,
    ) -> Result<(), UfsError> {
        self.send_checked_query(processor, &QueryUpiu::set_flag_at_index(flag, index))
            .await
            .map(|_| ())
    }

    /// Clears a device flag at index 0.
    pub async fn clear_flag<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        flag: Flag,
    ) -> Result<(), UfsError> {
        self.clear_flag_at_index(processor, flag, 0).await
    }

    /// Clears a device flag at `index` (LUN index for LU-dedicated WriteBooster flags).
    pub async fn clear_flag_at_index<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        flag: Flag,
        index: u8,
    ) -> Result<(), UfsError> {
        self.send_checked_query(processor, &QueryUpiu::clear_flag_at_index(flag, index))
            .await
            .map(|_| ())
    }

    /// Reads a device attribute.
    pub async fn read_attribute<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        attr: Attribute,
        index: u8,
    ) -> Result<u32, UfsError> {
        let req = QueryUpiu::read_attribute(attr, index);
        let val = self.send_checked_query(processor, &req).await?.value();
        if attr == Attribute::BootLunEn {
            self.inspect.b_boot_lun_en.set(u64::from(val));
        }
        Ok(val)
    }

    /// Writes a device attribute.
    pub async fn write_attribute<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        attr: Attribute,
        index: u8,
        value: u32,
    ) -> Result<(), UfsError> {
        let req = QueryUpiu::write_attribute(attr, index, value);
        self.send_checked_query(processor, &req).await.map(|_| ())
    }

    /// Reads and parses a descriptor.
    pub async fn read_descriptor<M: Mmio, D: Descriptor>(
        &self,
        processor: &TransferRequestProcessor<M>,
        index: u8,
    ) -> Result<D, UfsError> {
        let req = QueryUpiu::read_typed_descriptor::<D>(index);
        self.send_checked_query(processor, &req).await?.parse_descriptor::<D>()
    }

    /// Reads the Unit Descriptors of LUNs `0..lun_scan_count()`, skipping unreadable ones.
    pub(crate) async fn discover_unit_descriptors<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
    ) -> Vec<DiscoveredUnit> {
        let mut units = Vec::new();
        for lun in 0..self.lun_scan_count() {
            if let Ok(descriptor) = self.read_descriptor::<_, UnitDescriptor>(processor, lun).await
            {
                units.push(DiscoveredUnit { lun, descriptor });
            }
        }
        units
    }

    /// Returns the logical block size of `unit`, or `None` if `bLogicalBlockSize` is outside
    /// `[max(4096, bMinAddrBlockSize * 512), min(bMaxInBufferSize, bMaxOutBufferSize) * 512]`.
    pub fn logical_block_size(&self, unit: &UnitDescriptor) -> Option<u32> {
        let size = 1u32.checked_shl(unit.logical_block_size.into())?;
        let geometry = &self.geometry_descriptor;
        let min = MIN_LOGICAL_BLOCK_SIZE
            .max(u32::from(geometry.min_addr_block_size) * GEOMETRY_SIZE_UNIT);
        let max = u32::from(geometry.max_in_buffer_size.min(geometry.max_out_buffer_size))
            * GEOMETRY_SIZE_UNIT;
        (min..=max).contains(&size).then_some(size)
    }

    /// Sets `fDeviceInit` and polls until the device clears it (UFS 3.1/4.0 section 14.2).
    pub async fn device_init<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
    ) -> Result<(), UfsError> {
        self.set_flag(processor, Flag::DeviceInit).await?;
        let deadline = zx::MonotonicInstant::after(DEVICE_INIT_TIMEOUT);
        loop {
            if !self.read_flag(processor, Flag::DeviceInit).await? {
                return Ok(());
            }
            if zx::MonotonicInstant::get() > deadline {
                error!("Timed out waiting for fDeviceInit to clear");
                return Err(UfsError::Timeout);
            }
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(DEVICE_INIT_POLL_INTERVAL)).await;
        }
    }

    /// Ensures `bRefClkFreq` is 19.2 MHz on Intel hosts, else 38.4 MHz, skipping the persistent
    /// attribute write when the device already reports the target frequency. Best-effort: query
    /// failures are logged as warnings so devices that reject `bRefClkFreq` do not abort bring-up
    /// or `PowerDown` resume.
    pub async fn init_reference_clock<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
    ) -> Result<(), UfsError> {
        let (frequency, label) = if self.quirks.intel {
            (RefClkFreq::Freq19_2MHz, "19.2 MHz")
        } else {
            (RefClkFreq::Freq38_4MHz, "38.4 MHz")
        };
        let target = frequency as u32;
        match self.read_attribute(processor, Attribute::RefClkFreq, 0).await {
            Ok(current) if current == target => {}
            Ok(_) => {
                if let Err(e) =
                    self.write_attribute(processor, Attribute::RefClkFreq, 0, target).await
                {
                    warn!(frequency:?, err:? = e; "Failed to write bRefClkFreq");
                    return Ok(());
                }
                info!(frequency:?; "bRefClkFreq set");
            }
            Err(e) => {
                warn!(err:? = e; "Failed to read bRefClkFreq");
                return Ok(());
            }
        }
        self.inspect.reference_clock.set(label);
        Ok(())
    }

    /// Reads and caches the Device and Geometry Descriptors and derives the LUN scan bound.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` if Geometry `bMaxNumberLU` is neither 0 nor 1.
    pub async fn read_device_and_geometry_descriptors<M: Mmio>(
        &mut self,
        processor: &TransferRequestProcessor<M>,
    ) -> Result<(), UfsError> {
        let device: DeviceDescriptor = self.read_descriptor(processor, 0).await?;
        info!(
            number_lu = device.number_lu,
            boot_enable = device.boot_enable,
            spec_version = device.spec_version.get(),
            manufacturer_id = device.manufacturer_id.get();
            "DeviceDescriptor read"
        );
        self.device_descriptor = device;

        let geometry: GeometryDescriptor = self.read_descriptor(processor, 0).await?;
        info!(
            max_number_lu = geometry.max_number_lu,
            total_raw_device_capacity = geometry.total_raw_device_capacity.get();
            "GeometryDescriptor read"
        );
        self.lun_scan_count = match lun_scan_count_from_max_number_lu(geometry.max_number_lu) {
            Some(count) => count,
            None => {
                error!(value = geometry.max_number_lu; "Invalid Geometry Descriptor bMaxNumberLU");
                return Err(UfsError::InvalidParameter);
            }
        };
        self.geometry_descriptor = geometry;
        Ok(())
    }

    /// Logs the UniPro version, `PA_TActivate` and `PA_Granularity` of both link ends for
    /// diagnostics. The only side effect is on Intel hosts, where the device's `PA_TActivate` is
    /// first set to the host's value plus [`INTEL_PEER_T_ACTIVATE_INCREMENT`] granularity units.
    pub async fn init_unipro_attributes<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
    ) -> Result<(), UfsError> {
        let controller = processor.controller();
        let remote_version = controller.dme_get(PA_REMOTEVERINFO, 0).await?;
        let local_version = controller.dme_get(PA_LOCALVERINFO, 0).await?;
        let host_t_activate = controller.dme_get(PA_TACTIVATE, 0).await?;
        if self.quirks.intel {
            let t_activate = host_t_activate.saturating_add(INTEL_PEER_T_ACTIVATE_INCREMENT);
            controller.dme_peer_set(PA_TACTIVATE, 0, t_activate).await?;
        }
        let device_t_activate = controller.dme_peer_get(PA_TACTIVATE, 0).await?;
        let host_granularity = controller.dme_get(PA_GRANULARITY, 0).await?;
        let device_granularity = controller.dme_peer_get(PA_GRANULARITY, 0).await?;
        self.inspect.remote_version.set(u64::from(remote_version));
        self.inspect.local_version.set(u64::from(local_version));
        self.inspect.host_t_activate.set(u64::from(host_t_activate));
        self.inspect.device_t_activate.set(u64::from(device_t_activate));
        self.inspect.host_granularity.set(u64::from(host_granularity));
        self.inspect.device_granularity.set(u64::from(device_granularity));
        info!(
            local_version,
            remote_version,
            host_t_activate,
            device_t_activate,
            host_granularity,
            device_granularity;
            "UniPro attributes initialized"
        );
        Ok(())
    }

    /// Switches the link to HS `Fast_Mode` on all available lanes (`PA_TXGEAR = PA_RXGEAR =
    /// min(local PA_MaxRxHSGear, peer PA_MaxRxHSGear)`).
    pub async fn init_uic_power_mode<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
        phy: Option<&fufs_phy::UfsPhyProxy>,
    ) -> Result<(), UfsError> {
        let controller = processor.controller();
        let _auto_hibernate_hold =
            controller.hold_auto_hibernate(POWER_MODE_CHANGE_TIMEOUT).await?;
        if let Some(phy) = phy {
            match phy.pre_power_mode_change().await {
                Ok(Ok(())) => {}
                Ok(Err(status)) => {
                    error!(
                        status:% = zx::Status::err_from_raw(status);
                        "PHY PrePowerModeChange failed"
                    );
                    return Err(UfsError::IoError);
                }
                Err(e) => {
                    error!(err:? = e; "PHY PrePowerModeChange FIDL error");
                    return Err(UfsError::IoError);
                }
            }
        }

        let tx_lanes = controller.dme_get(PA_AVAILTXDATALANES, 0).await?;
        let rx_lanes = controller.dme_get(PA_AVAILRXDATALANES, 0).await?;
        let local_max_rx_gear = controller.dme_get(PA_MAXRXHSGEAR, 0).await?;
        let peer_max_rx_gear = controller.dme_peer_get(PA_MAXRXHSGEAR, 0).await?;
        let hs_gear = local_max_rx_gear.min(peer_max_rx_gear);
        info!(
            tx_lanes,
            rx_lanes,
            hs_gear,
            local_max_rx_gear,
            peer_max_rx_gear;
            "UIC power mode parameters"
        );

        let mut settings = vec![
            (PA_ACTIVETXDATALANES, tx_lanes),
            (PA_ACTIVERXDATALANES, rx_lanes),
            (PA_TXGEAR, hs_gear),
            (PA_RXGEAR, hs_gear),
        ];
        if hs_gear >= HS_GEAR_ADAPT_MIN {
            settings.push((PA_TXHSADAPTTYPE, PA_INITIAL_ADAPT));
        }
        settings.extend([
            (PA_TXTERMINATION, TERMINATION_ON),
            (PA_RXTERMINATION, TERMINATION_ON),
            (PA_HSSERIES, HS_SERIES_B),
            (PA_PWRMODEUSERDATA0, DL_FC0_PROTECTION_TIMEOUT_VAL),
            (PA_PWRMODEUSERDATA1, DL_TC0_REPLAY_TIMEOUT_VAL),
            (PA_PWRMODEUSERDATA2, DL_AFC0_REQ_TIMEOUT_VAL),
            (PA_PWRMODEUSERDATA3, DL_FC0_PROTECTION_TIMEOUT_VAL),
            (PA_PWRMODEUSERDATA4, DL_TC0_REPLAY_TIMEOUT_VAL),
            (PA_PWRMODEUSERDATA5, DL_AFC0_REQ_TIMEOUT_VAL),
            (DME_LOCAL_FC0_PROTECTION_TIMEOUT_VAL, DL_FC0_PROTECTION_TIMEOUT_VAL),
            (DME_LOCAL_TC0_REPLAY_TIMEOUT_VAL, DL_TC0_REPLAY_TIMEOUT_VAL),
            (DME_LOCAL_AFC0_REQ_TIMEOUT_VAL, DL_AFC0_REQ_TIMEOUT_VAL),
        ]);
        for (attribute, value) in settings {
            controller.dme_set(attribute, 0, value).await?;
        }

        for attempt in 1..=LINK_STARTUP_ATTEMPTS {
            match controller.change_power_mode(FAST_PWR_MODE, POWER_MODE_CHANGE_TIMEOUT).await {
                Ok(()) => break,
                Err(e) if attempt < LINK_STARTUP_ATTEMPTS => {
                    warn!(attempt, err:? = e; "UIC power mode change attempt failed");
                    fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                        LINK_STARTUP_RETRY_DELAY,
                    ))
                    .await;
                }
                Err(e) => {
                    error!(err:? = e; "UIC power mode change failed");
                    return Err(e);
                }
            }
        }

        if self.quirks.intel {
            // Intel workaround: settle the link and clear pending DME errors via a probe.
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(INTEL_POST_POWER_MODE_DELAY))
                .await;
            controller.dme_peer_get(PA_GRANULARITY, 0).await?;
        }
        self.inspect.pa_active_tx_data_lanes.set(u64::from(tx_lanes));
        self.inspect.pa_active_rx_data_lanes.set(u64::from(rx_lanes));
        self.inspect.pa_max_rx_hs_gear.set(u64::from(local_max_rx_gear));
        self.inspect.pa_tx_gear.set(u64::from(hs_gear));
        self.inspect.pa_rx_gear.set(u64::from(hs_gear));
        self.inspect.tx_termination.set(true);
        self.inspect.rx_termination.set(true);
        self.inspect.pa_hs_series.set(u64::from(HS_SERIES_B));
        self.inspect.power_mode.set(u64::from(FAST_PWR_MODE));
        info!(hs_gear; "UIC HS Fast_Mode enabled");
        Ok(())
    }

    /// Verifies that `bCurrentPowerMode` is `Active`, then sets `bActiveICCLevel` to the highest
    /// level.
    ///
    /// # Errors
    ///
    /// Returns `IoError` if the device is not `Active`, or the query error.
    pub async fn init_ufs_power_mode<M: Mmio>(
        &self,
        processor: &TransferRequestProcessor<M>,
    ) -> Result<(), UfsError> {
        let power_mode = self.read_attribute(processor, Attribute::CurrentPowerMode, 0).await?;
        self.inspect.b_current_power_mode.set(u64::from(power_mode));
        if power_mode != UfsPowerMode::Active as u32 {
            error!(power_mode; "Initial bCurrentPowerMode is not Active");
            return Err(UfsError::IoError);
        }
        self.write_attribute(processor, Attribute::ActiveIccLevel, 0, HIGHEST_ACTIVE_ICC_LEVEL)
            .await?;
        self.inspect.b_active_icc_level.set(u64::from(HIGHEST_ACTIVE_ICC_LEVEL));
        self.inspect.power_condition.set(POWER_CONDITION_ACTIVE);
        self.inspect.link_state.set(LINK_STATE_ACTIVE);
        Ok(())
    }

    /// Disables WriteBooster and its flush flags. Best-effort: failures are logged and ignored.
    pub async fn disable_write_booster<M: Mmio>(&self, processor: &TransferRequestProcessor<M>) {
        let lun = self.write_booster_buffer_lun;
        if let Err(e) = self.clear_flag_at_index(processor, Flag::WbBufferFlushEn, lun).await {
            warn!(lun, err:? = e; "Failed to clear fWBBufferFlushEn");
        }
        self.inspect.writebooster_buffer_flush_enabled.set(false);
        if let Err(e) =
            self.clear_flag_at_index(processor, Flag::WbBufferFlushDuringHibernate, lun).await
        {
            warn!(lun, err:? = e; "Failed to clear fWBBufferFlushDuringHibernate");
        }
        self.inspect.writebooster_buffer_flush_during_hibernate.set(false);
        if let Err(e) = self.clear_flag_at_index(processor, Flag::WriteBoosterEn, lun).await {
            warn!(lun, err:? = e; "Failed to clear fWriteBoosterEn");
        }
        self.inspect.is_write_booster_enabled.set(false);
        self.inspect.write_booster_dedicated_lu.set(0);
        self.inspect.write_booster_buffer_size_in_bytes.set(0);
    }

    /// Enables background operations and, if supported and usable, WriteBooster. `units` are the
    /// discovered Unit Descriptors (for LU-dedicated WriteBooster buffers).
    pub(crate) async fn configure_features<M: Mmio>(
        &mut self,
        processor: &TransferRequestProcessor<M>,
        units: &[DiscoveredUnit],
    ) {
        self.inspect.is_background_op_enabled.set(false);
        self.inspect.is_write_booster_enabled.set(false);
        self.inspect.writebooster_buffer_flush_during_hibernate.set(false);
        self.inspect.writebooster_buffer_flush_enabled.set(false);
        self.inspect.write_booster_dedicated_lu.set(0);
        self.inspect.write_booster_buffer_size_in_bytes.set(0);
        match self.set_flag(processor, Flag::BackgroundOpsEn).await {
            Ok(()) => {
                self.inspect.is_background_op_enabled.set(true);
                info!("Background operations enabled");
            }
            Err(e) => warn!(err:? = e; "Failed to enable fBackgroundOpsEn"),
        }

        let extended_features = self.device_descriptor.extended_ufs_features_support.get();
        if extended_features & EXTENDED_FEATURE_WRITE_BOOSTER == 0 {
            return;
        }

        let raw_buffer_type = self.device_descriptor.write_booster_buffer_type;
        self.inspect.write_booster_buffer_type.set(u64::from(raw_buffer_type));
        self.inspect
            .user_space_configuration_option
            .set(u64::from(self.device_descriptor.write_booster_buffer_preserve_user_space_en));
        let Some(buffer_type) = WriteBoosterBufferType::from_raw(raw_buffer_type) else {
            warn!(buffer_type = raw_buffer_type; "Unsupported WriteBooster buffer type, disabling");
            self.disable_write_booster(processor).await;
            return;
        };
        let (alloc_units, buffer_lun) = match buffer_type {
            WriteBoosterBufferType::Shared => {
                (self.device_descriptor.num_shared_write_booster_buffer_alloc_units.get(), 0)
            }
            WriteBoosterBufferType::LuDedicated => units
                .iter()
                .filter(|unit| unit.descriptor.lu_enable == LU_ENABLED)
                .map(|unit| {
                    (unit.descriptor.lu_num_write_booster_buffer_alloc_units.get(), unit.lun)
                })
                .find(|&(alloc_units, _)| alloc_units > 0)
                .unwrap_or((0, 0)),
        };
        self.write_booster_buffer_lun = buffer_lun;

        if alloc_units == 0 {
            warn!("WriteBooster buffer has no allocation units, disabling");
            self.disable_write_booster(processor).await;
            return;
        }

        match self.read_attribute(processor, Attribute::WbBufferLifeTimeEst, buffer_lun).await {
            Ok(WRITE_BOOSTER_LIFETIME_EXCEEDED) => {
                warn!(lun = buffer_lun; "WriteBooster buffer lifetime exceeded, disabling");
                self.disable_write_booster(processor).await;
                return;
            }
            Err(e) => {
                warn!(
                    lun = buffer_lun,
                    err:% = e;
                    "Failed to read WriteBooster lifetime, disabling",
                );
                self.disable_write_booster(processor).await;
                return;
            }
            Ok(_) => {}
        }

        if let Err(e) = self.set_flag_at_index(processor, Flag::WriteBoosterEn, buffer_lun).await {
            warn!(lun = buffer_lun, err:? = e; "Failed to enable fWriteBoosterEn");
            self.disable_write_booster(processor).await;
            return;
        }
        self.inspect.is_write_booster_enabled.set(true);
        if let Err(e) =
            self.set_flag_at_index(processor, Flag::WbBufferFlushDuringHibernate, buffer_lun).await
        {
            warn!(lun = buffer_lun, err:? = e; "Failed to enable fWBBufferFlushDuringHibernate");
            self.disable_write_booster(processor).await;
            return;
        }
        self.inspect.writebooster_buffer_flush_during_hibernate.set(true);
        if buffer_type == WriteBoosterBufferType::LuDedicated {
            self.inspect.write_booster_dedicated_lu.set(u64::from(buffer_lun));
        }

        let buffer_bytes = u64::from(alloc_units)
            .saturating_mul(self.geometry_descriptor.allocation_unit_size.into())
            .saturating_mul(self.geometry_descriptor.segment_size.get().into())
            .saturating_mul(SEGMENT_SIZE_UNIT);
        self.inspect.write_booster_buffer_size_in_bytes.set(buffer_bytes);

        info!(
            buffer_type:? = buffer_type,
            lun = buffer_lun,
            size_mib = buffer_bytes / (1024 * 1024);
            "WriteBooster enabled"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UfsError;
    use crate::constants::PA_PWRMODE;
    use crate::descriptors::DescriptorType;
    use crate::query::{QueryOpcode, QueryUpiu};
    use crate::registers::{CapabilityReg, PowerModeStatus, VersionReg};
    use crate::testing::fake_ufs::{CAP_AUTO_HIBERNATE_SUPPORT, hcs_power_mode_status};
    use crate::testing::{
        ADMIN_SLOT, OCS_SUCCESS, UicRecord, answer_queries, create_test_processor, fake_uic_device,
        fake_utp,
    };
    use crate::uic::UicCommandOpcode;
    use crate::upiu::UpiuTransactionCode;
    use mmio::Register;
    use zerocopy::IntoBytes;
    use zerocopy::byteorder::big_endian::{U16, U32, U64};

    #[fuchsia::test]
    fn test_quirks_from_pci_ids() {
        assert_eq!(
            Quirks::from_pci_ids(REDHAT_VENDOR_ID, QEMU_UFS_DEVICE_ID),
            Quirks { qemu: true, ..Default::default() }
        );
        assert_eq!(Quirks::from_pci_ids(REDHAT_VENDOR_ID, 0x0001), Quirks::default());
        assert_eq!(
            Quirks::from_pci_ids(INTEL_VENDOR_ID, 0x98FA),
            Quirks { intel: true, ..Default::default() }
        );
        assert_eq!(Quirks::from_pci_ids(0x144D, QEMU_UFS_DEVICE_ID), Quirks::default());
    }

    #[fuchsia::test]
    fn test_logical_block_size_validation() {
        let mut device_manager = DeviceManager::new(Quirks::default());
        // bMinAddrBlockSize = 4 KiB, bMaxIn/OutBufferSize = 8 KiB / 16 KiB.
        device_manager.geometry_descriptor.min_addr_block_size = 8;
        device_manager.geometry_descriptor.max_in_buffer_size = 16;
        device_manager.geometry_descriptor.max_out_buffer_size = 32;
        let unit = |shift| UnitDescriptor { logical_block_size: shift, ..Default::default() };

        assert_eq!(device_manager.logical_block_size(&unit(12)), Some(4096));
        assert_eq!(device_manager.logical_block_size(&unit(13)), Some(8192));
        // Below 4096, above min(bMaxIn, bMaxOut) and shift overflow.
        assert_eq!(device_manager.logical_block_size(&unit(9)), None);
        assert_eq!(device_manager.logical_block_size(&unit(14)), None);
        assert_eq!(device_manager.logical_block_size(&unit(32)), None);
        assert_eq!(device_manager.logical_block_size(&unit(0xFF)), None);

        // bMinAddrBlockSize raises the lower bound.
        device_manager.geometry_descriptor.min_addr_block_size = 16;
        assert_eq!(device_manager.logical_block_size(&unit(12)), None);
        assert_eq!(device_manager.logical_block_size(&unit(13)), Some(8192));
    }

    /// UIC commands issued by `init_uic_power_mode` for an HS-G4 link: 3 `DME_GET`s, 1
    /// `DME_PEER_GET`, 17 `DME_SET`s (HS-G4 adds `PA_TxHsAdaptType`) and the `PA_PWRMode` set.
    const POWER_MODE_CHANGE_UIC_COMMANDS_G4: usize = 22;
    /// As [`POWER_MODE_CHANGE_UIC_COMMANDS_G4`] for an HS-G3 peer (no `PA_TxHsAdaptType`).
    const POWER_MODE_CHANGE_UIC_COMMANDS_G3: usize = 21;
    /// UIC commands issued by `init_unipro_attributes`: 4 `DME_GET`s and 2 `DME_PEER_GET`s.
    const UNIPRO_ATTRIBUTE_UIC_COMMANDS: usize = 6;

    /// Returns the value written by the `opcode` command of `attribute` in `records`.
    fn uic_written(records: &[UicRecord], opcode: UicCommandOpcode, attribute: u16) -> Option<u32> {
        records
            .iter()
            .find(|r| r.opcode == opcode as u32 && r.attribute == attribute)
            .map(|r| r.arg3)
    }

    /// Link capabilities for [`fake_uic_device`]: 2 lanes, local HS-G`local_gear` and peer
    /// HS-G`peer_gear`.
    fn link_capabilities(
        local_gear: u32,
        peer_gear: u32,
    ) -> impl Fn(u32, u16) -> u32 + Send + 'static {
        move |opcode, attribute| match attribute {
            PA_AVAILTXDATALANES | PA_AVAILRXDATALANES => 2,
            PA_MAXRXHSGEAR if opcode == UicCommandOpcode::DmePeerGet as u32 => peer_gear,
            PA_MAXRXHSGEAR => local_gear,
            _ => 0,
        }
    }

    #[fuchsia::test]
    async fn test_init_uic_power_mode() {
        let (mmio_vmo, processor) = create_test_processor();
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal);
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G4,
            link_capabilities(4, 4),
        );
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.init_uic_power_mode(&processor, None).await, Ok(()));
        let records = hardware.join().unwrap();
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_TXGEAR), Some(4));
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_RXGEAR), Some(4));
        assert!(uic_written(&records, UicCommandOpcode::DmeSet, PA_TXHSADAPTTYPE).is_some());
        assert_eq!(
            uic_written(&records, UicCommandOpcode::DmeSet, PA_PWRMODE),
            Some(FAST_PWR_MODE)
        );
    }

    #[fuchsia::test]
    async fn test_init_uic_power_mode_negotiates_symmetric_hs_gear() {
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal);

        // Case A (local > peer): HS-G5 host + HS-G4 device -> symmetric HS-G4 + PA_TxHsAdaptType.
        let (mmio_vmo, processor) = create_test_processor();
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G4,
            link_capabilities(5, 4),
        );
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.init_uic_power_mode(&processor, None).await, Ok(()));
        let records = hardware.join().unwrap();
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_TXGEAR), Some(4));
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_RXGEAR), Some(4));
        assert_eq!(
            uic_written(&records, UicCommandOpcode::DmeSet, PA_TXHSADAPTTYPE),
            Some(PA_INITIAL_ADAPT)
        );

        // Case B (local < peer): HS-G3 host + HS-G5 device -> symmetric HS-G3, no PA_TxHsAdaptType.
        let (mmio_vmo, processor) = create_test_processor();
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G3,
            link_capabilities(3, 5),
        );
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.init_uic_power_mode(&processor, None).await, Ok(()));
        let records = hardware.join().unwrap();
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_TXGEAR), Some(3));
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_RXGEAR), Some(3));
        assert_eq!(uic_written(&records, UicCommandOpcode::DmeSet, PA_TXHSADAPTTYPE), None);
    }

    #[fuchsia::test]
    async fn test_init_uic_power_mode_requires_power_local() {
        let (mmio_vmo, processor) = create_test_processor();
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerOk);
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G4 + (LINK_STARTUP_ATTEMPTS as usize - 1),
            link_capabilities(4, 4),
        );
        let device_manager = DeviceManager::new(Quirks::default());
        assert!(matches!(
            device_manager.init_uic_power_mode(&processor, None).await,
            Err(UfsError::UicError { .. })
        ));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_init_unipro_attributes() {
        const GET: u32 = UicCommandOpcode::DmeGet as u32;
        const PEER_GET: u32 = UicCommandOpcode::DmePeerGet as u32;
        const PEER_SET: u32 = UicCommandOpcode::DmePeerSet as u32;

        let answer = |_, attribute| if attribute == PA_TACTIVATE { 5 } else { 0 };
        let (mmio_vmo, processor) = create_test_processor();
        let hardware = fake_uic_device(&mmio_vmo, 0, UNIPRO_ATTRIBUTE_UIC_COMMANDS, answer);
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.init_unipro_attributes(&processor).await, Ok(()));
        let commands: Vec<_> =
            hardware.join().unwrap().iter().map(|r| (r.opcode, r.attribute)).collect();
        assert_eq!(
            commands,
            [
                (GET, PA_REMOTEVERINFO),
                (GET, PA_LOCALVERINFO),
                (GET, PA_TACTIVATE),
                (PEER_GET, PA_TACTIVATE),
                (GET, PA_GRANULARITY),
                (PEER_GET, PA_GRANULARITY),
            ]
        );

        // Intel: the device's PA_TActivate is set to the host's plus 2 (one extra DME_PEER_SET)
        // before it is read back.
        let (mmio_vmo, processor) = create_test_processor();
        let hardware = fake_uic_device(&mmio_vmo, 0, UNIPRO_ATTRIBUTE_UIC_COMMANDS + 1, answer);
        let device_manager = DeviceManager::new(Quirks { intel: true, ..Default::default() });
        assert_eq!(device_manager.init_unipro_attributes(&processor).await, Ok(()));
        let records = hardware.join().unwrap();
        assert_eq!(records[3], UicRecord { opcode: PEER_SET, attribute: PA_TACTIVATE, arg3: 7 });
        assert_eq!((records[4].opcode, records[4].attribute), (PEER_GET, PA_TACTIVATE));
    }

    #[fuchsia::test]
    async fn test_device_manager_query_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            // SetFlag, ReadFlag, ReadDescriptor(Device), ReadDescriptor(Geometry).
            for _ in 0..4 {
                assert!(utp.wait_for_doorbell(ADMIN_SLOT), "admin query not issued");
                let request: QueryUpiu = utp.request(ADMIN_SLOT);
                let mut response = request;
                response.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
                let descriptor: Vec<u8> = match (request.opcode, request.idn) {
                    (op, idn)
                        if op == QueryOpcode::ReadFlag as u8 && idn == Flag::DeviceInit as u8 =>
                    {
                        response.value = U32::new(0);
                        Vec::new()
                    }
                    (op, idn)
                        if op == QueryOpcode::ReadDescriptor as u8
                            && idn == DescriptorType::Device as u8 =>
                    {
                        DeviceDescriptor {
                            length: std::mem::size_of::<DeviceDescriptor>() as u8,
                            descriptor_idn: DescriptorType::Device as u8,
                            number_lu: 8,
                            spec_version: U16::new(0x0310),
                            ..Default::default()
                        }
                        .as_bytes()
                        .to_vec()
                    }
                    (op, idn)
                        if op == QueryOpcode::ReadDescriptor as u8
                            && idn == DescriptorType::Geometry as u8 =>
                    {
                        GeometryDescriptor {
                            length: std::mem::size_of::<GeometryDescriptor>() as u8,
                            descriptor_idn: DescriptorType::Geometry as u8,
                            max_number_lu: 1,
                            total_raw_device_capacity: U64::new(2_000_000),
                            ..Default::default()
                        }
                        .as_bytes()
                        .to_vec()
                    }
                    _ => Vec::new(),
                };
                if !descriptor.is_empty() {
                    response.command_data[..descriptor.len()].copy_from_slice(&descriptor);
                    response.length = U16::new(descriptor.len() as u16);
                }
                utp.complete(ADMIN_SLOT, response.as_bytes(), OCS_SUCCESS);
            }
        });

        let mut device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.lun_scan_count(), 0);
        device_manager.device_init(&processor).await.expect("Failed to initialize device");
        device_manager
            .read_device_and_geometry_descriptors(&processor)
            .await
            .expect("Failed to read device descriptors");
        hardware.join().unwrap();

        assert_eq!(device_manager.logical_unit_count(), 8);
        assert_eq!(device_manager.device_descriptor().spec_version.get(), 0x0310);
        assert_eq!(device_manager.geometry_descriptor().max_number_lu, 1);
        assert_eq!(device_manager.geometry_descriptor().total_raw_device_capacity.get(), 2_000_000);
        // bMaxNumberLU 1: scan 32 LUNs regardless of bNumberLU.
        assert_eq!(device_manager.lun_scan_count(), 32);
    }

    #[fuchsia::test]
    async fn test_device_manager_checks_query_response() {
        let (mmio_vmo, processor) = create_test_processor();
        // Invalid IDN.
        let hardware =
            answer_queries(fake_utp(&mmio_vmo, &processor), 3, |r| r.header.response = 0xFD);
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(
            device_manager.read_flag(&processor, Flag::DeviceInit).await,
            Err(UfsError::IoError)
        );
        let attribute = device_manager.read_attribute(&processor, Attribute::BootLunEn, 0).await;
        assert_eq!(attribute, Err(UfsError::IoError));
        // A failed fDeviceInit read must not look like a completed initialization.
        assert_eq!(device_manager.device_init(&processor).await, Err(UfsError::IoError));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_init_reference_clock() {
        for (quirks, expected) in [
            (Quirks::default(), RefClkFreq::Freq38_4MHz),
            (Quirks { intel: true, ..Default::default() }, RefClkFreq::Freq19_2MHz),
        ] {
            let target = expected as u32;
            let (mmio_vmo, processor) = create_test_processor();
            let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 2, move |resp| {
                if resp.opcode == QueryOpcode::ReadAttribute as u8 {
                    resp.value = U32::new(target ^ 1);
                }
            });
            let device_manager = DeviceManager::new(quirks);
            assert_eq!(device_manager.init_reference_clock(&processor).await, Ok(()));
            let requests = hardware.join().unwrap();
            assert_eq!(requests[0].opcode, QueryOpcode::ReadAttribute as u8);
            assert_eq!(requests[0].idn, Attribute::RefClkFreq as u8);
            assert_eq!(requests[1].opcode, QueryOpcode::WriteAttribute as u8);
            assert_eq!(requests[1].idn, Attribute::RefClkFreq as u8);
            assert_eq!(requests[1].value(), target);

            // When `bRefClkFreq` already matches `target`, the persistent write is skipped.
            let (mmio_vmo, processor) = create_test_processor();
            let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 1, move |resp| {
                resp.value = U32::new(target);
            });
            assert_eq!(device_manager.init_reference_clock(&processor).await, Ok(()));
            let requests = hardware.join().unwrap();
            assert_eq!(requests[0].opcode, QueryOpcode::ReadAttribute as u8);
            assert_eq!(requests[0].idn, Attribute::RefClkFreq as u8);
        }
    }

    #[fuchsia::test]
    async fn test_read_descriptors_rejects_invalid_max_number_lu() {
        let (mmio_vmo, processor) = create_test_processor();
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 2, |response| {
            let descriptor = if response.idn == DescriptorType::Device as u8 {
                DeviceDescriptor {
                    length: std::mem::size_of::<DeviceDescriptor>() as u8,
                    descriptor_idn: DescriptorType::Device as u8,
                    ..Default::default()
                }
                .as_bytes()
                .to_vec()
            } else {
                GeometryDescriptor {
                    length: std::mem::size_of::<GeometryDescriptor>() as u8,
                    descriptor_idn: DescriptorType::Geometry as u8,
                    max_number_lu: 2,
                    ..Default::default()
                }
                .as_bytes()
                .to_vec()
            };
            response.command_data[..descriptor.len()].copy_from_slice(&descriptor);
            response.length = U16::new(descriptor.len() as u16);
        });
        let mut device_manager = DeviceManager::new(Quirks::default());
        let result = device_manager.read_device_and_geometry_descriptors(&processor).await;
        assert_eq!(result, Err(UfsError::InvalidParameter));
        assert_eq!(device_manager.lun_scan_count(), 0);
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_inspect_hierarchy_recording() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(VersionReg::OFFSET, 4 << 8);
            mmio.store32(
                CapabilityReg::OFFSET,
                CAP_AUTO_HIBERNATE_SUPPORT | (1 << 24) | (7 << 16) | (15 << 8) | 31,
            );
        });
        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.controller().record_inspect(&ufs_node);

        let mut device_manager = DeviceManager::new(Quirks::default());
        device_manager.init_inspect(&ufs_node);
        device_manager.record_logical_unit_count(3);

        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal);
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G4,
            link_capabilities(4, 4),
        );
        assert_eq!(device_manager.init_uic_power_mode(&processor, None).await, Ok(()));
        hardware.join().unwrap();
        inspector.root().record(ufs_node);

        diagnostics_assertions::assert_data_tree!(inspector, root: {
            ufs: {
                version: {
                    major_version_number: 4u64,
                    minor_version_number: 0u64,
                    version_suffix: 0u64,
                },
                capabilities: {
                    crypto_support: false,
                    uic_dme_test_mode_command_supported: false,
                    out_of_order_data_delivery_supported: false,
                    "64_bit_addressing_supported": true,
                    auto_hibernation_support: true,
                    number_of_utp_task_management_request_slots: 7u64,
                    number_of_outstanding_rtt_requests_supported: 15u64,
                    number_of_utp_transfer_request_slots: 31u64,
                },
                controller: {
                    max_transfer_bytes: MAX_TRANSFER_BYTES,
                    logical_unit_count: 3u64,
                    reference_clock: "",
                    PowerCondition: 1u64,
                    LinkState: 1u64,
                    attributes: {
                        bBootLunEn: 0u64,
                        bCurrentPowerMode: UfsPowerMode::Active as u64,
                        bActiveICCLevel: u64::from(HIGHEST_ACTIVE_ICC_LEVEL),
                    },
                    unipro: {
                        remote_version: 0u64,
                        local_version: 0u64,
                        host_t_activate: 0u64,
                        device_t_activate: 0u64,
                        host_granularity: 0u64,
                        device_granularity: 0u64,
                        PA_ActiveTxDataLanes: 2u64,
                        PA_ActiveRxDataLanes: 2u64,
                        PA_MaxRxHSGear: 4u64,
                        PA_TxGear: 4u64,
                        PA_RxGear: 4u64,
                        tx_termination: true,
                        rx_termination: true,
                        PA_HSSeries: u64::from(HS_SERIES_B),
                        power_mode: u64::from(FAST_PWR_MODE),
                    },
                    background_operations: {
                        is_background_op_enabled: false,
                    },
                    writebooster: {
                        is_write_booster_enabled: false,
                        writebooster_buffer_flush_during_hibernate: false,
                        writebooster_buffer_flush_enabled: false,
                        write_booster_buffer_type: 0u64,
                        user_space_configuration_option: 0u64,
                        write_booster_dedicated_lu: 0u64,
                        write_booster_buffer_size_in_bytes: 0u64,
                    },
                },
            }
        });
    }

    #[fuchsia::test]
    async fn test_device_init_polls_until_cleared() {
        let (mmio_vmo, processor) = create_test_processor();
        let poll_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poll_count_clone = std::sync::Arc::clone(&poll_count);
        // SetFlag(DeviceInit), then two ReadFlag(DeviceInit) == 1 before clearing to 0.
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 4, move |resp| {
            if resp.opcode == QueryOpcode::ReadFlag as u8 {
                let idx = poll_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                resp.value = U32::new(u32::from(idx < 2));
            }
        });
        let device_manager = DeviceManager::new(Quirks::default());
        assert_eq!(device_manager.device_init(&processor).await, Ok(()));
        assert_eq!(poll_count.load(std::sync::atomic::Ordering::SeqCst), 3);
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_init_ufs_power_mode_and_configure_features() {
        let (mmio_vmo, processor) = create_test_processor();
        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut device_manager = DeviceManager::new(Quirks::default());
        device_manager.init_inspect(&ufs_node);

        // 1. Non-Active bCurrentPowerMode fails with IoError.
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 1, |resp| {
            resp.value = U32::new(UfsPowerMode::Sleep as u32);
        });
        assert_eq!(device_manager.init_ufs_power_mode(&processor).await, Err(UfsError::IoError));
        hardware.join().unwrap();

        // 2. Active bCurrentPowerMode writes bActiveICCLevel = 0x0F.
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 2, |resp| {
            if resp.opcode == QueryOpcode::ReadAttribute as u8 {
                resp.value = U32::new(UfsPowerMode::Active as u32);
            }
        });
        assert_eq!(device_manager.init_ufs_power_mode(&processor).await, Ok(()));
        let reqs = hardware.join().unwrap();
        assert_eq!(reqs[1].idn, Attribute::ActiveIccLevel as u8);
        assert_eq!(reqs[1].value(), HIGHEST_ACTIVE_ICC_LEVEL);

        // 3. configure_features with LU-dedicated WriteBooster: disabled LUN 0 is skipped, enabled
        //    LUN 1 is selected, lifetime is valid (1), WriteBoosterEn and
        //    WbBufferFlushDuringHibernate are set, and WbBufferFlushEn is NOT set at init.
        device_manager.device_descriptor.extended_ufs_features_support =
            U32::new(EXTENDED_FEATURE_WRITE_BOOSTER);
        device_manager.device_descriptor.write_booster_buffer_type =
            WriteBoosterBufferType::LuDedicated as u8;
        device_manager.geometry_descriptor.allocation_unit_size = 1;
        device_manager.geometry_descriptor.segment_size = U32::new(1);
        let units = [
            DiscoveredUnit {
                lun: 0,
                descriptor: UnitDescriptor {
                    lu_enable: 0,
                    lu_num_write_booster_buffer_alloc_units: U32::new(4),
                    ..Default::default()
                },
            },
            DiscoveredUnit {
                lun: 1,
                descriptor: UnitDescriptor {
                    lu_enable: LU_ENABLED,
                    lu_num_write_booster_buffer_alloc_units: U32::new(2),
                    ..Default::default()
                },
            },
        ];
        // SetFlag(BackgroundOpsEn), ReadAttribute(WbBufferLifeTimeEst) -> 1,
        // SetFlag(WriteBoosterEn), SetFlag(WbBufferFlushDuringHibernate).
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 4, |resp| {
            if resp.opcode == QueryOpcode::ReadAttribute as u8 {
                resp.value = U32::new(1);
            }
        });
        device_manager.configure_features(&processor, &units).await;
        let reqs = hardware.join().unwrap();
        assert_eq!(reqs[1].opcode, QueryOpcode::ReadAttribute as u8);
        assert_eq!(reqs[1].idn, Attribute::WbBufferLifeTimeEst as u8);
        assert_eq!(reqs[1].index, 1);
        assert_eq!(reqs[2].idn, Flag::WriteBoosterEn as u8);
        assert_eq!(reqs[3].idn, Flag::WbBufferFlushDuringHibernate as u8);
        assert!(!reqs.iter().any(|r| {
            r.opcode == QueryOpcode::SetFlag as u8 && r.idn == Flag::WbBufferFlushEn as u8
        }));

        // 4. Exceeded WriteBooster lifetime disables WriteBooster without setting WriteBoosterEn.
        let hardware = answer_queries(fake_utp(&mmio_vmo, &processor), 5, |resp| {
            if resp.opcode == QueryOpcode::ReadAttribute as u8 {
                resp.value = U32::new(WRITE_BOOSTER_LIFETIME_EXCEEDED);
            }
        });
        device_manager.configure_features(&processor, &units).await;
        let reqs = hardware.join().unwrap();
        assert_eq!(reqs[1].idn, Attribute::WbBufferLifeTimeEst as u8);
        assert_eq!(reqs[2].opcode, QueryOpcode::ClearFlag as u8);
    }
}
