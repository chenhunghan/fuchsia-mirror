// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Platform-device and peripheral resource provider.
//!
//! Discovers and maps every MMIO region, IRQ, GPIO, I2C, and SPI resource
//! the bound node offers, packaging them into a [`ProvidedResources`] bundle.
//! A node that offers no resources yields an empty bundle: the control plane
//! still serves, but nothing is reachable. A node that offers resources the
//! driver cannot acquire fails acquisition -- a partially acquired device
//! would misrepresent the resource identity the digests promise.

#[cfg(feature = "fdf_driver")]
use crate::fuchsia_backends::{
    FuchsiaClock, FuchsiaGpio, FuchsiaI2c, FuchsiaReset, FuchsiaSerial, FuchsiaSpi, LiveBackend,
};
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_clock as fclock;
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_gpio as fgpio;
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_i2c as fi2c;
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_reset as freset;
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_serial as fserial;
#[cfg(feature = "fdf_driver")]
use fidl_fuchsia_hardware_spi as fspi;
#[cfg(feature = "fdf_driver")]
use fidl_next_fuchsia_hardware_platform_device as fpdev;
#[cfg(feature = "fdf_driver")]
use lab_proxy_core::access_policy::{
    MmioResource, ProtocolCeiling, ResourceCeiling, ResourceId, ResourceKind,
};
use lab_proxy_core::hardware_backend::{BackendError, MmioBackend};
#[cfg(feature = "fdf_driver")]
use lab_proxy_core::provider::ProvidedResources;
#[cfg(feature = "fdf_driver")]
use log::info;
#[cfg(feature = "fdf_driver")]
use mmio::Mmio as _;
#[cfg(feature = "fdf_driver")]
use mmio::region::MmioRegion;
#[cfg(feature = "fdf_driver")]
use mmio::vmo::{VmoMapping, VmoMemory};
#[cfg(feature = "fdf_driver")]
use pdev::PlatformDevice;

/// Provider kind recorded in resource digests.
pub const PROVIDER: &str = "platform";

#[cfg(not(feature = "fdf_driver"))]
struct RawVmoMapping {
    map_addr: usize,
    map_size: usize,
    base_addr: usize,
    len: usize,
}

#[cfg(not(feature = "fdf_driver"))]
impl RawVmoMapping {
    fn map(offset: usize, size: usize, vmo: zx::Vmo) -> Result<Self, zx::Status> {
        if size == 0 || size > isize::MAX as usize {
            return Err(zx::Status::OUT_OF_RANGE);
        }
        let page_size = zx::system_get_page_size() as usize;
        let page_offset = offset % page_size;
        let aligned_offset = (offset - page_offset) as u64;
        let map_size = size
            .checked_add(page_offset)
            .ok_or(zx::Status::OUT_OF_RANGE)?
            .next_multiple_of(page_size);

        let info = vmo.info()?;
        if aligned_offset.saturating_add(map_size as u64) > info.size_bytes {
            return Err(zx::Status::OUT_OF_RANGE);
        }

        let root_self = fuchsia_runtime::vmar_root_self();
        let map_addr = root_self.map(
            0,
            &vmo,
            aligned_offset,
            map_size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE | zx::VmarFlags::MAP_RANGE,
        )?;
        Ok(Self { map_addr, map_size, base_addr: map_addr + page_offset, len: size })
    }

    fn len(&self) -> usize {
        self.len
    }

    fn try_load32(&self, offset: usize) -> Result<u32, BackendError> {
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|end| end > self.len) {
            return Err(BackendError::Fault);
        }
        let ptr = std::ptr::with_exposed_provenance::<u32>(self.base_addr + offset);
        // SAFETY: `offset + 4 <= self.len` and `offset` is 4-byte aligned within the VMAR mapping.
        Ok(unsafe { std::ptr::read_volatile(ptr) })
    }

    fn try_store32(&mut self, offset: usize, value: u32) -> Result<(), BackendError> {
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|end| end > self.len) {
            return Err(BackendError::Fault);
        }
        let ptr = std::ptr::with_exposed_provenance_mut::<u32>(self.base_addr + offset);
        // SAFETY: `offset + 4 <= self.len` and `offset` is 4-byte aligned within the VMAR mapping.
        unsafe { std::ptr::write_volatile(ptr, value) };
        Ok(())
    }
}

#[cfg(not(feature = "fdf_driver"))]
impl Drop for RawVmoMapping {
    fn drop(&mut self) {
        let root_self = fuchsia_runtime::vmar_root_self();
        // SAFETY: `self.map_addr` and `self.map_size` were returned by `root_self.map` and are
        // only accessed while `self` is alive.
        let _ = unsafe { root_self.unmap(self.map_addr, self.map_size) };
    }
}

/// Volatile access to one locally mapped MMIO region.
pub struct MappedMmio {
    #[cfg(feature = "fdf_driver")]
    region: MmioRegion<VmoMemory>,
    #[cfg(not(feature = "fdf_driver"))]
    region: RawVmoMapping,
}

impl MappedMmio {
    /// Wraps an already-mapped [`MmioRegion`].
    #[cfg(feature = "fdf_driver")]
    pub fn from_region(region: MmioRegion<VmoMemory>) -> Self {
        Self { region }
    }

    /// Creates an independent local MMIO mapping by duplicating an existing
    /// driver MMIO VMO handle (`zx::Rights::SAME_RIGHTS`) and mapping it into
    /// the current driver host process address space (Spec Phase 2 Section 2.1).
    ///
    /// Preserves the VMO's existing cache policy so already-mapped VMOs can be
    /// mapped a second time without failing on `zx_vmo_set_cache_policy`.
    pub fn from_vmo(vmo: &zx::Vmo, offset: usize, size: usize) -> Result<Self, zx::Status> {
        let dup_vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        #[cfg(feature = "fdf_driver")]
        {
            let cache_policy = dup_vmo.info()?.cache_policy();
            let region = VmoMapping::map_with_cache_policy(offset, size, dup_vmo, cache_policy)?;
            Ok(Self { region })
        }
        #[cfg(not(feature = "fdf_driver"))]
        {
            let region = RawVmoMapping::map(offset, size, dup_vmo)?;
            Ok(Self { region })
        }
    }

    /// Returns the mapped byte length of the region.
    pub fn len(&self) -> usize {
        self.region.len()
    }

    /// Returns whether the mapped region has zero length.
    pub fn is_empty(&self) -> bool {
        self.region.len() == 0
    }
}

impl std::fmt::Debug for MappedMmio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedMmio").field("len", &self.region.len()).finish()
    }
}

impl MmioBackend for MappedMmio {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        let offset = usize::try_from(offset).map_err(|_| BackendError::Fault)?;
        self.region.try_load32(offset).map_err(|_| BackendError::Fault)
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        let offset = usize::try_from(offset).map_err(|_| BackendError::Fault)?;
        self.region.try_store32(offset, value).map_err(|_| BackendError::Fault)
    }

    fn barrier(&mut self) {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl lab_proxy_core::protocol_resource_adapter::ResourceBackend for MappedMmio {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        MmioBackend::read32(self, offset)
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        MmioBackend::write32(self, offset, value)
    }

    fn barrier(&mut self) {
        MmioBackend::barrier(self);
    }
}

/// Why acquisition failed. Driver start fails on any of these: serving
/// a partially acquired device would break resource-identity promises.
#[cfg(feature = "fdf_driver")]
#[derive(Debug)]
pub enum AcquireError {
    /// The device reported an MMIO the driver could not fetch or map.
    Mmio {
        /// The MMIO index that failed.
        id: u32,
    },
    /// The device reported an IRQ the driver could not fetch.
    Irq {
        /// The IRQ index that failed.
        id: u32,
    },
}

#[cfg(feature = "fdf_driver")]
impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Mmio { id } => write!(f, "MMIO {id} could not be fetched or mapped"),
            AcquireError::Irq { id } => write!(f, "IRQ {id} could not be fetched"),
        }
    }
}

#[cfg(feature = "fdf_driver")]
fn connect_instance(
    context: &fdf_component::DriverContext,
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

/// Acquires the bound node's platform-device and protocol resources.
///
/// Returns an empty bundle when the node offers no reachable resources.
#[cfg(feature = "fdf_driver")]
pub async fn acquire(
    context: &fdf_component::DriverContext,
    node_identity: &str,
) -> Result<(ProvidedResources<LiveBackend>, Vec<(ResourceId, zx::Interrupt)>), AcquireError> {
    let mut bundle = ProvidedResources::empty(PROVIDER, node_identity);
    let mut acquired_irqs = Vec::new();
    let mut next_id: ResourceId = 0;

    // 1. Probe platform device for MMIOs and IRQs
    let mut connected_pdev = None;
    for instance in ["pdev", "default"] {
        let Some(pdev) = connect_instance(context, instance) else { continue };
        match pdev.get_node_device_info().await {
            Ok(Ok(device_info)) => {
                connected_pdev = Some((
                    pdev,
                    device_info.mmio_count.unwrap_or(0),
                    device_info.irq_count.unwrap_or(0),
                ));
                break;
            }
            Ok(Err(status)) => {
                info!("get_node_device_info on instance {instance} returned {status:?}");
            }
            Err(error) => {
                info!("platform device instance {instance} not served ({error:?})");
            }
        }
    }

    if let Some((pdev, mmio_count, irq_count)) = connected_pdev {
        for id in 0..mmio_count {
            let region = pdev.map_mmio_by_id(id).await.map_err(|error| {
                info!("mapping MMIO {id} failed: {error:?}");
                AcquireError::Mmio { id }
            })?;
            let size = region.len() as u64;
            let res_id = next_id;
            next_id += 1;
            bundle.resources.insert(res_id, MmioResource::mmio(format!("mmio{id}"), size, size));
            bundle.ceiling.insert(
                res_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: true,
                    allow_poll: true,
                    writable_registers: vec![],
                    protocol: None,
                    allow_interrupt: false,
                },
            );
            bundle.backends.insert(res_id, LiveBackend::Mmio(MappedMmio { region }));
        }

        for id in 0..irq_count {
            let irq = pdev
                .get_interrupt_by_id(id, 0)
                .await
                .map_err(|error| {
                    info!("getting IRQ {id} failed: {error:?}");
                    AcquireError::Irq { id }
                })?
                .map_err(|status| {
                    info!("getting IRQ {id} returned status {status:?}");
                    AcquireError::Irq { id }
                })?
                .irq;
            let res_id = next_id;
            next_id += 1;
            bundle.resources.insert(res_id, MmioResource::interrupt(format!("irq{id}")));
            bundle.ceiling.insert(
                res_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: false,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: Some(ProtocolCeiling::default_for(ResourceKind::Interrupt)),
                    allow_interrupt: true,
                },
            );
            bundle.backends.insert(res_id, LiveBackend::Interrupt);
            acquired_irqs.push((res_id, irq));
        }
    }

    // 2. Probe GPIO services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(fgpio::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_device_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                match proxy.read(probe_deadline) {
                    Ok(_) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "gpio0".to_string()
                        } else {
                            format!("gpio-{instance}")
                        };
                        info!("acquired GPIO resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::gpio(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Gpio)),
                                allow_interrupt: false,
                            },
                        );
                        bundle.backends.insert(res_id, LiveBackend::Gpio(FuchsiaGpio::new(proxy)));
                    }
                    Err(e) => {
                        info!("GPIO instance {instance} probe failed: {e:?}");
                    }
                }
            }
        }
    }

    // 3. Probe I2C services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(fi2c::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_device_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                // An empty transfer triggers no bus activity and succeeds with INVALID_ARGS if the server is active.
                match proxy.transfer(&[], probe_deadline) {
                    Ok(Err(status)) if status == zx::Status::INVALID_ARGS.into_raw() => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "i2c0".to_string()
                        } else {
                            format!("i2c-{instance}")
                        };
                        info!("acquired I2C resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::i2c(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::I2c)),
                                allow_interrupt: false,
                            },
                        );
                        bundle.backends.insert(res_id, LiveBackend::I2c(FuchsiaI2c::new(proxy)));
                    }
                    Ok(Ok(_)) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "i2c0".to_string()
                        } else {
                            format!("i2c-{instance}")
                        };
                        info!("acquired I2C resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::i2c(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::I2c)),
                                allow_interrupt: false,
                            },
                        );
                        bundle.backends.insert(res_id, LiveBackend::I2c(FuchsiaI2c::new(proxy)));
                    }
                    res => {
                        info!("I2C instance {instance} probe returned: {res:?}");
                    }
                }
            }
        }
    }

    // 4. Probe SPI services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(fspi::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_device_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                // CanAssertCs is a pure software query that verifies server liveness without bus transmission.
                match proxy.can_assert_cs(probe_deadline) {
                    Ok(_) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "spi0".to_string()
                        } else {
                            format!("spi-{instance}")
                        };
                        info!("acquired SPI resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::spi(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Spi)),
                                allow_interrupt: false,
                            },
                        );
                        bundle.backends.insert(res_id, LiveBackend::Spi(FuchsiaSpi::new(proxy)));
                    }
                    Err(e) => {
                        info!("SPI instance {instance} probe returned: {e:?}");
                    }
                }
            }
        }
    }

    // 5. Probe Clock services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(fclock::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_clock_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                match proxy.is_enabled(probe_deadline) {
                    Ok(_) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "clock0".to_string()
                        } else {
                            format!("clock-{instance}")
                        };
                        info!("acquired Clock resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::clock(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Clock)),
                                allow_interrupt: false,
                            },
                        );
                        bundle
                            .backends
                            .insert(res_id, LiveBackend::Clock(FuchsiaClock::new(proxy)));
                    }
                    Err(e) => {
                        info!("Clock instance {instance} probe returned: {e:?}");
                    }
                }
            }
        }
    }

    // 6. Probe Reset services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(freset::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_reset_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                match proxy.status(probe_deadline) {
                    Ok(_) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "reset0".to_string()
                        } else {
                            format!("reset-{instance}")
                        };
                        info!("acquired Reset resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::reset(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Reset)),
                                allow_interrupt: false,
                            },
                        );
                        bundle
                            .backends
                            .insert(res_id, LiveBackend::Reset(FuchsiaReset::new(proxy)));
                    }
                    Err(e) => {
                        info!("Reset instance {instance} probe returned: {e:?}");
                    }
                }
            }
        }
    }

    // 7. Probe Serial services
    for instance in ["default"] {
        if let Ok(service) =
            context.incoming.service_marker(fserial::ServiceMarker).instance(&instance).connect()
        {
            if let Ok(proxy) = service.connect_to_device_sync() {
                let probe_deadline = zx::MonotonicInstant::after(zx::Duration::from_millis(50));
                match proxy.get_class(probe_deadline) {
                    Ok(_) => {
                        let res_id = next_id;
                        next_id += 1;
                        let name = if instance == "default" {
                            "serial0".to_string()
                        } else {
                            format!("serial-{instance}")
                        };
                        info!("acquired Serial resource {name} (id {res_id})");
                        bundle.resources.insert(res_id, MmioResource::serial(name));
                        bundle.ceiling.insert(
                            res_id,
                            ResourceCeiling {
                                hard_denied: vec![],
                                allow_unknown_reads: false,
                                allow_poll: false,
                                writable_registers: vec![],
                                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Serial)),
                                allow_interrupt: false,
                            },
                        );
                        bundle
                            .backends
                            .insert(res_id, LiveBackend::Serial(FuchsiaSerial::new(proxy)));
                    }
                    Err(e) => {
                        info!("Serial instance {instance} probe returned: {e:?}");
                    }
                }
            }
        }
    }

    if bundle.resources.is_empty() {
        info!("no hardware resources offered; serving zero resources");
        return Ok((ProvidedResources::empty("none", node_identity), acquired_irqs));
    }

    info!("acquired {} total hardware resource(s)", bundle.resources.len());
    Ok((bundle, acquired_irqs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zx::CachePolicy;

    #[test]
    fn mapped_mmio_from_vmo_shares_memory_independently_of_driver_mapping() {
        const SIZE: usize = 4096;
        let vmo = zx::Vmo::create(SIZE as u64).unwrap();
        let driver_vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        // Simulate the active driver mapping its own MMIO region first.
        let mut driver_region =
            VmoMapping::map_with_cache_policy(0, SIZE, driver_vmo, CachePolicy::Cached).unwrap();
        assert_eq!(vmo.info().unwrap().num_mappings, 1);

        // Driver initializes a register at 0x10.
        driver_region.try_store32(0x10, 0xCAFE_BABE).unwrap();

        // Embedded library duplicates the VMO and creates an independent local mapping.
        let mut lab_mmio = MappedMmio::from_vmo(&vmo, 0, SIZE).unwrap();
        assert_eq!(lab_mmio.len(), SIZE);
        assert!(!lab_mmio.is_empty());
        assert_eq!(vmo.info().unwrap().num_mappings, 2);

        // Library observes live register value written by the driver.
        assert_eq!(MmioBackend::read32(&mut lab_mmio, 0x10), Ok(0xCAFE_BABE));

        // Library writes a value at 0x20; driver observes it in its own mapping.
        assert_eq!(MmioBackend::write32(&mut lab_mmio, 0x20, 0x1234_5678), Ok(()));
        assert_eq!(driver_region.try_load32(0x20), Ok(0x1234_5678));

        // Dropping the library mapping leaves the driver's mapping intact.
        drop(lab_mmio);
        assert_eq!(vmo.info().unwrap().num_mappings, 1);
        assert_eq!(driver_region.try_load32(0x10), Ok(0xCAFE_BABE));
        assert_eq!(driver_region.try_load32(0x20), Ok(0x1234_5678));
    }

    #[test]
    fn mapped_mmio_from_vmo_rejects_out_of_range_size() {
        let vmo = zx::Vmo::create(4096).unwrap();
        let err = MappedMmio::from_vmo(&vmo, 0, 8192).unwrap_err();
        assert_eq!(err, zx::Status::OUT_OF_RANGE);
    }
}
