// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the On-Device Power Monitor (ODPM) module for Starnix.
//!
//! This module registers an Industrial I/O (IIO) device at `/sys/devices/platform/cpm/cpm:ODPM/iio:device0`
//! with a symlink under `/sys/bus/iio/devices/iio:device0`, exposing power rail energy measurements
//! from the Fuchsia ODPM FIDL service (`fuchsia.hardware.google.odpm`).

#![recursion_limit = "256"]

use fidl::endpoints::Proxy;
use fidl_fuchsia_hardware_google_odpm as fodpm;
use fuchsia_async as fasync;
use fuchsia_component::client::Service;
use futures::{FutureExt, TryStreamExt};
use starnix_core::device::kobject::Device;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::BytesFile;
use starnix_core::vfs::{FsNodeOps, FsString};
use starnix_logging::{log_debug, log_error, log_warn};
use starnix_sync::{LockDepMutex, OdpmProxyLock, OdpmRailLastReadingLock};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::collections::{BTreeMap, btree_map};
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

const NANOS_PER_MILLI: i64 = 1_000_000;
const SERVICE_INITIAL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);
const SYNC_PROXY_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

fn nanos_to_millis(nanos: i64) -> i64 {
    nanos / NANOS_PER_MILLI
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct RailReading {
    timestamp_ms: i64,
    duration_ms: i64,
    energy_uj: i64,
}

/// A power rail monitored by ODPM.
#[derive(Debug)]
pub struct OdpmRail {
    name: FsString,
    channel_id: u32,
    schematic_name: FsString,
    last_reading: LockDepMutex<Option<RailReading>, OdpmRailLastReadingLock>,
}

impl OdpmRail {
    /// Creates a new [`OdpmRail`].
    pub fn new(
        name: impl Into<FsString>,
        channel_id: u32,
        schematic_name: impl Into<FsString>,
    ) -> Self {
        Self {
            name: name.into(),
            channel_id,
            schematic_name: schematic_name.into(),
            last_reading: LockDepMutex::new(None),
        }
    }
}

/// An ODPM device aggregating power rails.
pub struct OdpmDevice {
    rails: OnceLock<BTreeMap<u32, Arc<OdpmRail>>>,
    proxy: LockDepMutex<Option<Arc<fodpm::OdpmDeviceSynchronousProxy>>, OdpmProxyLock>,
}

impl std::fmt::Debug for OdpmDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OdpmDevice")
            .field("rails", &self.rails)
            .field("is_initialized", &self.rails.get().is_some())
            .finish()
    }
}

impl Default for OdpmDevice {
    fn default() -> Self {
        Self { rails: OnceLock::new(), proxy: LockDepMutex::new(None) }
    }
}

impl OdpmDevice {
    /// Creates a new, uninitialized [`OdpmDevice`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new [`OdpmDevice`] with the given power rails and proxy.
    #[cfg(test)]
    pub fn new_with_rails(
        rails: Vec<OdpmRail>,
        proxy: Option<Arc<fodpm::OdpmDeviceSynchronousProxy>>,
    ) -> Self {
        let mut map = BTreeMap::new();
        for rail in rails {
            match map.entry(rail.channel_id) {
                btree_map::Entry::Vacant(entry) => {
                    entry.insert(Arc::new(rail));
                }
                btree_map::Entry::Occupied(_) => {
                    log_warn!(
                        "ODPM rail with channel_id {} already exists; ignoring duplicate",
                        rail.channel_id
                    );
                }
            }
        }
        let rails_lock = OnceLock::new();
        let _ = rails_lock.set(map);
        Self { rails: rails_lock, proxy: LockDepMutex::new(proxy) }
    }

    /// Updates the synchronous proxy connection for this device (e.g. following a driver restart).
    pub fn update_proxy(&self, proxy: Arc<fodpm::OdpmDeviceSynchronousProxy>) {
        *self.proxy.lock() = Some(proxy);
    }

    /// Initializes this device with the discovered rails.
    pub fn init(&self, rails: BTreeMap<u32, Arc<OdpmRail>>) {
        if self.rails.set(rails).is_err() {
            log_warn!("ODPM rails are already initialized");
        }
    }

    /// Returns whether the device has completed initial rail discovery.
    pub fn is_initialized(&self) -> bool {
        self.rails.get().is_some()
    }

    /// Returns a reference to the discovered rails map, waiting for initialization if needed.
    ///
    /// Blocks the current thread until initial rail discovery has completed.
    fn rails(&self) -> &BTreeMap<u32, Arc<OdpmRail>> {
        self.rails.wait()
    }

    /// Reads energy measurements across all rails and formats them for the `energy_value` sysfs file.
    ///
    /// Returns a formatted string containing the maximum timestamp across all rail samples,
    /// followed by each rail's channel ID, sample duration, schematic name, and cumulative energy.
    /// If reading fails, cached readings or default values are used.
    pub fn energy_value(&self) -> Result<FsString, Errno> {
        let rails = self.rails();
        if rails.is_empty() {
            return Ok("t=0\n".into());
        }

        let mut max_timestamp_ms = 0i64;
        let mut readings = Vec::with_capacity(rails.len());

        let options = fodpm::Options {
            measurement_type: Some(fodpm::MeasurementType::Cumulative),
            ..Default::default()
        };

        let proxy = self.proxy.lock().clone();
        let fetched_readings = match proxy {
            Some(proxy) => match proxy
                .get_energy_joules(&options, zx::MonotonicInstant::after(SYNC_PROXY_TIMEOUT))
            {
                Ok(Ok(readings)) => Some(readings),
                Ok(Err(status)) => {
                    log_debug!("ODPM GetEnergyJoules failed: status {}", status);
                    None
                }
                Err(e) => {
                    log_debug!("ODPM GetEnergyJoules FIDL error: {:?}", e);
                    None
                }
            },
            None => None,
        };

        for rail in rails.values() {
            let reading = match fetched_readings
                .as_ref()
                .and_then(|list| list.iter().find(|r| r.channel_id == Some(rail.channel_id)))
            {
                Some(reading) => {
                    let timestamp_ms = reading.timestamp.map(nanos_to_millis).unwrap_or(0);
                    let duration_ms = reading.interval.map(nanos_to_millis).unwrap_or(timestamp_ms);
                    let energy_uj = reading.energy_uj.unwrap_or(0);
                    let rail_reading = RailReading { timestamp_ms, duration_ms, energy_uj };
                    *rail.last_reading.lock() = Some(rail_reading);
                    rail_reading
                }
                None => (*rail.last_reading.lock()).unwrap_or_default(),
            };

            if max_timestamp_ms < reading.timestamp_ms {
                max_timestamp_ms = reading.timestamp_ms;
            }
            readings.push((
                rail.channel_id,
                reading.duration_ms,
                &rail.schematic_name,
                reading.energy_uj,
            ));
        }

        let mut output = String::with_capacity(32 + readings.len() * 64);
        let _ = write!(output, "t={max_timestamp_ms}\n");
        for (channel_id, duration_ms, schematic_name, energy_uj) in readings {
            let _ =
                write!(output, "CH{channel_id}(T={duration_ms})[{schematic_name}], {energy_uj}\n");
        }

        Ok(output.into())
    }

    /// Formats rail configuration for the `enabled_rails` sysfs file.
    pub fn enabled_rails(&self) -> FsString {
        let rails = self.rails();
        let mut output = String::with_capacity(rails.len() * 40);
        for rail in rails.values() {
            let _ =
                write!(output, "CH{}[{}]:{}\n", rail.channel_id, rail.schematic_name, rail.name);
        }
        output.into()
    }
}

#[derive(Clone, Debug)]
struct EnergyValueFile {
    device: Arc<OdpmDevice>,
}

impl EnergyValueFile {
    fn new_node(device: Arc<OdpmDevice>) -> impl FsNodeOps {
        DynamicFile::new_node(Self { device })
    }
}

impl DynamicFileSource for EnergyValueFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let content = self.device.energy_value()?;
        sink.write(&content);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct EnabledRailsFile {
    device: Arc<OdpmDevice>,
}

impl EnabledRailsFile {
    fn new_node(device: Arc<OdpmDevice>) -> impl FsNodeOps {
        DynamicFile::new_node(Self { device })
    }
}

impl DynamicFileSource for EnabledRailsFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let content = self.device.enabled_rails();
        sink.write(&content);
        Ok(())
    }
}

fn build_odpm_directory(dir: &SimpleDirectoryMutator, odpm_device: Arc<OdpmDevice>) {
    dir.entry("name", BytesFile::new_node(b"cpm:ODPM\n".to_vec()), mode!(IFREG, 0o444));
    dir.entry(
        "enabled_rails",
        EnabledRailsFile::new_node(odpm_device.clone()),
        mode!(IFREG, 0o444),
    );
    dir.entry("energy_value", EnergyValueFile::new_node(odpm_device), mode!(IFREG, 0o444));
}

async fn watch_odpm_rails(odpm_device: Arc<OdpmDevice>) {
    let service = match Service::open(fodpm::ServiceMarker) {
        Ok(service) => service,
        Err(e) => {
            log_warn!("Failed to open ODPM service: {:?}", e);
            odpm_device.init(BTreeMap::new());
            return;
        }
    };
    let mut watcher = match service.watch().await {
        Ok(watcher) => watcher,
        Err(e) => {
            log_warn!("Failed to watch ODPM service: {:?}", e);
            odpm_device.init(BTreeMap::new());
            return;
        }
    };

    let mut timeout = std::pin::pin!(
        fasync::Timer::new(zx::MonotonicInstant::after(SERVICE_INITIAL_TIMEOUT)).fuse()
    );

    loop {
        futures::select! {
            entry = watcher.try_next().fuse() => {
                match entry {
                    Ok(Some(service_proxy)) => {
                        let device_proxy = match service_proxy.connect_to_device() {
                            Ok(proxy) => proxy,
                            Err(e) => {
                                log_warn!("Failed to connect to ODPM device: {:?}", e);
                                continue;
                            }
                        };

                        match device_proxy.get_rail_metadata().await {
                            Ok(Ok(metadata_list)) => {
                                let client_end = match device_proxy.into_client_end() {
                                    Ok(client_end) => client_end,
                                    Err(_) => {
                                        log_warn!("Failed to get client end from ODPM device proxy");
                                        continue;
                                    }
                                };
                                let sync_proxy = Arc::new(fodpm::OdpmDeviceSynchronousProxy::new(
                                    client_end.into_channel(),
                                ));
                                odpm_device.update_proxy(sync_proxy);

                                if !odpm_device.is_initialized() {
                                    let mut rails = BTreeMap::new();
                                    for metadata in metadata_list {
                                        let name = match metadata.name {
                                            Some(name) => name,
                                            None => {
                                                log_warn!("ODPM rail missing name");
                                                continue;
                                            }
                                        };
                                        let channel_id = match metadata.channel_id {
                                            Some(channel_id) => channel_id,
                                            None => {
                                                log_warn!("ODPM rail '{name}' missing channel_id");
                                                continue;
                                            }
                                        };
                                        let schematic_name = match metadata.schematic_name {
                                            Some(schematic_name) => schematic_name,
                                            None => {
                                                log_warn!("ODPM rail '{name}' missing schematic_name");
                                                continue;
                                            }
                                        };

                                        match rails.entry(channel_id) {
                                            btree_map::Entry::Vacant(entry) => {
                                                entry.insert(Arc::new(OdpmRail::new(
                                                    name,
                                                    channel_id,
                                                    schematic_name,
                                                )));
                                            }
                                            btree_map::Entry::Occupied(_) => {
                                                log_warn!(
                                                    "Duplicate ODPM rail '{name}' (channel_id {channel_id}); ignoring"
                                                );
                                            }
                                        }
                                    }
                                    odpm_device.init(rails);
                                    timeout.set(futures::future::Fuse::terminated());
                                } else {
                                    log_debug!("Updated ODPM device proxy with new service instance");
                                }
                            }
                            Ok(Err(status)) => {
                                log_warn!("ODPM GetRailMetadata failed with status: {:?}", status);
                            }
                            Err(e) => {
                                log_warn!("ODPM GetRailMetadata FIDL error: {:?}", e);
                            }
                        }
                    }
                    Ok(None) => {
                        log_error!("ODPM service watcher stream ended");
                        break;
                    }
                    Err(e) => {
                        log_error!("ODPM service watcher error: {:?}", e);
                        break;
                    }
                }
            }
            _ = timeout => {
                log_warn!("ODPM service watcher timed out waiting for initial service");
                odpm_device.init(BTreeMap::new());
                break;
            }
        }
    }

    if !odpm_device.is_initialized() {
        odpm_device.init(BTreeMap::new());
    }
}

/// Registers the ODPM sysfs device hierarchy and spawns the background rail watcher.
///
/// Returns an error (such as `EEXIST`), rolls back any partial sysfs entries, and does not spawn
/// the background watcher if a conflicting device is already registered.
pub fn odpm_device_init(kernel: &Kernel) -> Result<(), Errno> {
    let odpm_device = Arc::new(OdpmDevice::new());
    register_odpm_device(kernel, Arc::clone(&odpm_device))?;

    let odpm_device_clone = Arc::clone(&odpm_device);
    kernel.kthreads.spawn_future(
        move || async move {
            watch_odpm_rails(odpm_device_clone).await;
        },
        "odpm_device_watcher",
    );
    Ok(())
}

struct StagedDevices<'a> {
    kernel: &'a Kernel,
    devices: Vec<Device>,
}

impl Drop for StagedDevices<'_> {
    fn drop(&mut self) {
        for device in self.devices.drain(..).rev() {
            self.kernel.device_registry.remove_device_with_kernel(self.kernel, device);
        }
    }
}

/// Registers the `cpm` and `cpm:ODPM` platform bus devices and the `iio:device0` IIO bus device
/// at `/sys/devices/platform/cpm/cpm:ODPM/iio:device0`.
///
/// Returns an error and rolls back any devices added by this call if a conflicting sysfs entry
/// already exists.
fn register_odpm_device(kernel: &Kernel, odpm_device: Arc<OdpmDevice>) -> Result<(), Errno> {
    let registry = &kernel.device_registry;
    let platform = registry.objects.platform_device();
    let platform_bus = registry.objects.platform_bus();
    let mut staged = StagedDevices { kernel, devices: Vec::with_capacity(2) };
    let cpm = registry.add_bus_device(
        "cpm".into(),
        Some(platform),
        platform_bus.clone(),
        build_device_directory,
    )?;
    staged.devices.push(cpm.clone());
    let odpm = registry.add_bus_device(
        "cpm:ODPM".into(),
        Some(cpm),
        platform_bus,
        build_device_directory,
    )?;
    staged.devices.push(odpm.clone());
    let iio_bus = registry.objects.get_or_create_bus("iio".into());
    registry.add_bus_device("iio:device0".into(), Some(odpm), iio_bus, |device, dir| {
        build_device_directory(device, dir);
        build_odpm_directory(dir, odpm_device);
    })?;
    staged.devices.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use starnix_core::device::DeviceRegistry;
    use starnix_core::device::kobject::{UEventAction, UEventContext};
    use starnix_core::fs::sysfs::get_sysfs;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::buffers::VecOutputBuffer;
    use starnix_core::vfs::{LookupContext, NamespaceNode, SymlinkTarget};
    use starnix_sync::Mutex;
    use starnix_uapi::errno;
    use starnix_uapi::open_flags::OpenFlags;

    fn record_device_events(
        registry: &DeviceRegistry,
    ) -> Arc<Mutex<Vec<(UEventAction, FsString)>>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let _ = registry.register_listener(
            move |action: UEventAction, device: Device, _context: UEventContext| {
                recorded.lock().push((action, device.name().to_owned()));
            },
        );
        events
    }

    fn millis_to_nanos(millis: i64) -> i64 {
        millis * NANOS_PER_MILLI
    }

    fn spawn_fake_odpm_server(
        readings: Vec<fodpm::EnergyReading>,
    ) -> Arc<fodpm::OdpmDeviceSynchronousProxy> {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fodpm::OdpmDeviceMarker>();
        let proxy = fodpm::OdpmDeviceSynchronousProxy::new(client_end.into_channel());
        std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutorBuilder::new().build();
            let mut stream = server_end.into_stream();
            executor.run_singlethreaded(async move {
                while let Some(Ok(request)) = stream.next().await {
                    if let fodpm::OdpmDeviceRequest::GetEnergyJoules { payload: _, responder } =
                        request
                    {
                        let _ = responder.send(Ok(&readings));
                    }
                }
            });
        });
        Arc::new(proxy)
    }

    #[::fuchsia::test]
    async fn test_odpm_energy_value_output() {
        let proxy = spawn_fake_odpm_server(vec![
            fodpm::EnergyReading {
                channel_id: Some(0),
                energy_uj: Some(51899976),
                interval: Some(millis_to_nanos(1279315)),
                timestamp: Some(millis_to_nanos(1279315)),
                ..Default::default()
            },
            fodpm::EnergyReading {
                channel_id: Some(1),
                energy_uj: Some(185487825),
                interval: Some(millis_to_nanos(1279315)),
                timestamp: Some(millis_to_nanos(1279315)),
                ..Default::default()
            },
        ]);

        let odpm_device = OdpmDevice::new_with_rails(
            vec![OdpmRail::new("amb", 0, "S1M_VDD_AMB"), OdpmRail::new("cpu2", 1, "S2M_VDD_CPU2")],
            Some(proxy),
        );

        let energy_val = odpm_device.energy_value().expect("energy_value failed");
        assert_eq!(
            energy_val,
            "t=1279315\nCH0(T=1279315)[S1M_VDD_AMB], 51899976\nCH1(T=1279315)[S2M_VDD_CPU2], 185487825\n"
        );
    }

    #[::fuchsia::test]
    async fn test_odpm_energy_value_preserves_reading_on_error() {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fodpm::OdpmDeviceMarker>();
        let proxy = Arc::new(fodpm::OdpmDeviceSynchronousProxy::new(client_end.into_channel()));
        std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutorBuilder::new().build();
            let mut stream = server_end.into_stream();
            executor.run_singlethreaded(async move {
                let mut first_call = true;
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fodpm::OdpmDeviceRequest::GetEnergyJoules { payload: _, responder } => {
                            if first_call {
                                first_call = false;
                                let readings = [fodpm::EnergyReading {
                                    channel_id: Some(0),
                                    energy_uj: Some(100),
                                    interval: Some(millis_to_nanos(1000)),
                                    timestamp: Some(millis_to_nanos(1000)),
                                    ..Default::default()
                                }];
                                let _ = responder.send(Ok(&readings));
                            } else {
                                // Second call returns an error.
                                let _ = responder.send(Err(zx::sys::ZX_ERR_INTERNAL));
                            }
                        }
                        _ => {}
                    }
                }
            });
        });

        let odpm_device =
            OdpmDevice::new_with_rails(vec![OdpmRail::new("amb", 0, "S1M_VDD_AMB")], Some(proxy));

        // First read succeeds.
        let energy_val1 = odpm_device.energy_value().expect("first read should succeed");
        assert_eq!(energy_val1, "t=1000\nCH0(T=1000)[S1M_VDD_AMB], 100\n");

        // Second read encounters error, but reuses prior reading.
        let energy_val2 =
            odpm_device.energy_value().expect("second read should preserve prior reading");
        assert_eq!(energy_val2, "t=1000\nCH0(T=1000)[S1M_VDD_AMB], 100\n");
    }

    #[::fuchsia::test]
    async fn test_odpm_enabled_rails() {
        let odpm_device = OdpmDevice::new_with_rails(
            vec![OdpmRail::new("cpu2", 1, "S2M_VDD_CPU2"), OdpmRail::new("amb", 0, "S1M_VDD_AMB")],
            None,
        );

        assert_eq!(odpm_device.enabled_rails(), "CH0[S1M_VDD_AMB]:amb\nCH1[S2M_VDD_CPU2]:cpu2\n");
    }

    #[::fuchsia::test]
    async fn test_odpm_duplicate_rail_ignored() {
        let odpm_device = OdpmDevice::new_with_rails(
            vec![
                OdpmRail::new("cpu2", 1, "S2M_VDD_CPU2"),
                OdpmRail::new("cpu2_dup", 1, "S2M_VDD_CPU2_DUP"),
            ],
            None,
        );
        assert_eq!(odpm_device.enabled_rails(), "CH1[S2M_VDD_CPU2]:cpu2\n");
    }

    #[::fuchsia::test]
    async fn test_odpm_device_update_proxy() {
        let proxy1 = spawn_fake_odpm_server(vec![fodpm::EnergyReading {
            channel_id: Some(0),
            energy_uj: Some(100),
            interval: Some(millis_to_nanos(1000)),
            timestamp: Some(millis_to_nanos(1000)),
            ..Default::default()
        }]);
        let proxy2 = spawn_fake_odpm_server(vec![fodpm::EnergyReading {
            channel_id: Some(0),
            energy_uj: Some(200),
            interval: Some(millis_to_nanos(2000)),
            timestamp: Some(millis_to_nanos(2000)),
            ..Default::default()
        }]);

        let odpm_device =
            OdpmDevice::new_with_rails(vec![OdpmRail::new("amb", 0, "S1M_VDD_AMB")], Some(proxy1));

        let val1 = odpm_device.energy_value().expect("read with proxy1");
        assert_eq!(val1, "t=1000\nCH0(T=1000)[S1M_VDD_AMB], 100\n");

        odpm_device.update_proxy(proxy2);

        let val2 = odpm_device.energy_value().expect("read with proxy2");
        assert_eq!(val2, "t=2000\nCH0(T=2000)[S1M_VDD_AMB], 200\n");
    }

    #[::fuchsia::test]
    async fn test_odpm_energy_value_defaults_to_zero_on_initial_failure() {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fodpm::OdpmDeviceMarker>();
        let proxy = Arc::new(fodpm::OdpmDeviceSynchronousProxy::new(client_end.into_channel()));
        std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutorBuilder::new().build();
            let mut stream = server_end.into_stream();
            executor.run_singlethreaded(async move {
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fodpm::OdpmDeviceRequest::GetEnergyJoules { payload: _, responder } => {
                            let _ = responder.send(Err(zx::sys::ZX_ERR_INTERNAL));
                        }
                        _ => {}
                    }
                }
            });
        });

        let odpm_device =
            OdpmDevice::new_with_rails(vec![OdpmRail::new("amb", 0, "S1M_VDD_AMB")], Some(proxy));

        let energy_val =
            odpm_device.energy_value().expect("energy_value should succeed with default 0");
        assert_eq!(energy_val, "t=0\nCH0(T=0)[S1M_VDD_AMB], 0\n");
    }

    #[::fuchsia::test]
    async fn test_odpm_energy_value_empty_rails_skips_rpc() {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fodpm::OdpmDeviceMarker>();
        let proxy = Arc::new(fodpm::OdpmDeviceSynchronousProxy::new(client_end.into_channel()));
        let server_thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutorBuilder::new().build();
            let mut stream = server_end.into_stream();
            executor.run_singlethreaded(async move {
                if let Some(request) = stream.next().await {
                    panic!("Unexpected RPC when rails are empty: {:?}", request);
                }
            });
        });

        let odpm_device = OdpmDevice::new_with_rails(vec![], Some(proxy));
        let energy_val = odpm_device.energy_value().expect("energy_value should succeed");
        assert_eq!(energy_val, "t=0\n");

        drop(odpm_device);
        server_thread.join().expect("server thread should finish cleanly");
    }

    #[::fuchsia::test]
    async fn test_register_odpm_sysfs_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let events = record_device_events(&kernel.device_registry);

            let odpm_device = Arc::new(OdpmDevice::new_with_rails(vec![], None));
            register_odpm_device(kernel, odpm_device).expect("register_odpm_device should succeed");

            assert_eq!(
                *events.lock(),
                vec![
                    (UEventAction::Add, FsString::from("cpm")),
                    (UEventAction::Add, FsString::from("cpm:ODPM")),
                    (UEventAction::Add, FsString::from("iio:device0")),
                ]
            );

            let root = &kernel.device_registry.objects.root;
            let assert_symlink_target = |path: &str, expected: &str| {
                let node =
                    root.lookup(path.into()).unwrap_or_else(|| panic!("{path} should exist"));
                match node.readlink(&current_task).expect("readlink should succeed") {
                    SymlinkTarget::Path(target) => {
                        assert_eq!(target, expected, "unexpected target for {path}");
                    }
                    other => panic!("expected SymlinkTarget::Path for {path}, got {other:?}"),
                }
            };

            assert_symlink_target("bus/platform/devices/cpm", "../../../devices/platform/cpm");
            assert_symlink_target(
                "bus/platform/devices/cpm:ODPM",
                "../../../devices/platform/cpm/cpm:ODPM",
            );
            assert_symlink_target(
                "bus/iio/devices/iio:device0",
                "../../../devices/platform/cpm/cpm:ODPM/iio:device0",
            );
            assert_symlink_target("devices/platform/cpm/subsystem", "../../../bus/platform");
            assert_symlink_target(
                "devices/platform/cpm/cpm:ODPM/subsystem",
                "../../../../bus/platform",
            );
            assert_symlink_target(
                "devices/platform/cpm/cpm:ODPM/iio:device0/subsystem",
                "../../../../../bus/iio",
            );

            assert!(root.lookup("devices/platform/cpm/cpm:ODPM/iio:device0/name".into()).is_some());
            assert!(
                root.lookup("devices/platform/cpm/cpm:ODPM/iio:device0/enabled_rails".into())
                    .is_some()
            );
            assert!(
                root.lookup("devices/platform/cpm/cpm:ODPM/iio:device0/energy_value".into())
                    .is_some()
            );

            let sysfs_root = NamespaceNode::new_anonymous(get_sysfs(kernel).root().clone());
            for uevent_path in [
                "devices/platform/cpm/uevent",
                "devices/platform/cpm/cpm:ODPM/uevent",
                "devices/platform/cpm/cpm:ODPM/iio:device0/uevent",
            ] {
                let node = current_task
                    .lookup_path(
                        &mut LookupContext::default(),
                        sysfs_root.clone(),
                        uevent_path.into(),
                    )
                    .unwrap_or_else(|e| panic!("lookup {uevent_path}: {e:?}"));
                let file = node.open(&current_task, OpenFlags::RDONLY).expect("open uevent");
                let mut buf = VecOutputBuffer::new(64);
                let n = file.read(&current_task, &mut buf).expect("read uevent");
                assert_eq!(n, 0, "expected empty uevent file at {uevent_path}");
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_odpm_rolls_back_on_conflict() {
        // Conflict on `cpm`: preserves the pre-existing `cpm` device without creating `bus/iio`.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            let platform = registry.objects.platform_device();
            let platform_bus = registry.objects.platform_bus();
            registry
                .add_bus_device("cpm".into(), Some(platform), platform_bus, build_device_directory)
                .expect("pre-register conflicting cpm");

            let events = record_device_events(registry);

            let odpm_device = Arc::new(OdpmDevice::new_with_rails(vec![], None));
            assert_eq!(register_odpm_device(kernel, odpm_device), Err(errno!(EEXIST)));
            assert_eq!(*events.lock(), vec![]);

            let root = &registry.objects.root;
            assert!(root.lookup("devices/platform/cpm".into()).is_some());
            assert!(root.lookup("bus/platform/devices/cpm".into()).is_some());
            assert!(root.lookup("devices/platform/cpm/cpm:ODPM".into()).is_none());
            assert!(root.lookup("bus/iio".into()).is_none());
        })
        .await;

        // Conflict on `cpm:ODPM`: rolls back `cpm` without creating `bus/iio` while preserving
        // the pre-existing `cpm:ODPM` bus symlink.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            let platform_bus = registry.objects.platform_bus();
            registry
                .add_bus_device("cpm:ODPM".into(), None, platform_bus, build_device_directory)
                .expect("pre-register conflicting cpm:ODPM");

            let events = record_device_events(registry);

            let odpm_device = Arc::new(OdpmDevice::new_with_rails(vec![], None));
            assert_eq!(register_odpm_device(kernel, odpm_device), Err(errno!(EEXIST)));
            assert_eq!(
                *events.lock(),
                vec![
                    (UEventAction::Add, FsString::from("cpm")),
                    (UEventAction::Remove, FsString::from("cpm")),
                ]
            );

            let root = &registry.objects.root;
            assert!(root.lookup("devices/platform/cpm".into()).is_none());
            assert!(root.lookup("bus/platform/devices/cpm".into()).is_none());
            assert!(root.lookup("devices/cpm:ODPM".into()).is_some());
            assert!(root.lookup("bus/platform/devices/cpm:ODPM".into()).is_some());
            assert!(root.lookup("bus/iio".into()).is_none());
        })
        .await;

        // Conflict on `iio:device0`: rolls back `cpm:ODPM` before `cpm` and preserves the
        // pre-existing `iio:device0` device.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            let iio_bus = registry.objects.get_or_create_bus("iio".into());
            registry
                .add_bus_device("iio:device0".into(), None, iio_bus, build_device_directory)
                .expect("pre-register conflicting iio:device0");

            let events = record_device_events(registry);

            let odpm_device = Arc::new(OdpmDevice::new_with_rails(vec![], None));
            assert_eq!(register_odpm_device(kernel, odpm_device), Err(errno!(EEXIST)));
            assert_eq!(
                *events.lock(),
                vec![
                    (UEventAction::Add, FsString::from("cpm")),
                    (UEventAction::Add, FsString::from("cpm:ODPM")),
                    (UEventAction::Remove, FsString::from("cpm:ODPM")),
                    (UEventAction::Remove, FsString::from("cpm")),
                ]
            );

            let root = &registry.objects.root;
            assert!(root.lookup("devices/platform/cpm".into()).is_none());
            assert!(root.lookup("bus/platform/devices/cpm".into()).is_none());
            assert!(root.lookup("bus/platform/devices/cpm:ODPM".into()).is_none());
            assert!(root.lookup("bus/iio/devices/iio:device0".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_nanos_to_millis() {
        assert_eq!(nanos_to_millis(1_000_000), 1);
        assert_eq!(nanos_to_millis(2_500_000), 2);
        assert_eq!(nanos_to_millis(0), 0);
        assert_eq!(millis_to_nanos(5), 5_000_000);
    }
}
