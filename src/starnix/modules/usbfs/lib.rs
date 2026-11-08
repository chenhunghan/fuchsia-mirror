// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

pub(crate) mod device;
pub(crate) mod sysfs;

use crate::device::{
    USB_SPEED_FULL, USB_SPEED_HIGH, USB_SPEED_LOW, USB_SPEED_SUPER, USB_SPEED_SUPER_PLUS,
    USB_SPEED_UNKNOWN, UsbDevice, UsbDeviceFile, UsbfsMemoryBudget,
};
use crate::sysfs::build_usb_device_directory;
use fidl::endpoints::Proxy;
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fuchsia_component::client::Service;
use futures::TryStreamExt;
use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::{Bus, Device, DeviceMetadata};
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::NamespaceNode;
use starnix_logging::{log_error, log_info};
use starnix_sync::{
    LockDepMutex, UsbFsDevicesLock, UsbfsActiveHwIdsLock, UsbfsDevNumAllocatorLock,
};
use starnix_uapi::device_id::{DeviceId, USB_DEVICE_MAJOR};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, usb_device_descriptor};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub(crate) fn fuchsia_speed_to_linux_speed(speed: fusb_descriptor::UsbSpeed) -> u32 {
    match speed {
        fusb_descriptor::UsbSpeed::Full => USB_SPEED_FULL,
        fusb_descriptor::UsbSpeed::Low => USB_SPEED_LOW,
        fusb_descriptor::UsbSpeed::High => USB_SPEED_HIGH,
        fusb_descriptor::UsbSpeed::Super => USB_SPEED_SUPER,
        fusb_descriptor::UsbSpeed::EnhancedSuper => USB_SPEED_SUPER_PLUS,
        fusb_descriptor::UsbSpeed::Undefined => USB_SPEED_UNKNOWN,
    }
}

pub(crate) fn fidl_device_descriptor_to_linux(
    desc: &fusb_descriptor::UsbDeviceDescriptor,
) -> usb_device_descriptor {
    usb_device_descriptor {
        bLength: desc.b_length,
        bDescriptorType: desc.b_descriptor_type,
        bcdUSB: desc.bcd_usb.to_le(),
        bDeviceClass: desc.b_device_class,
        bDeviceSubClass: desc.b_device_sub_class,
        bDeviceProtocol: desc.b_device_protocol,
        bMaxPacketSize0: desc.b_max_packet_size0,
        idVendor: desc.id_vendor.to_le(),
        idProduct: desc.id_product.to_le(),
        bcdDevice: desc.bcd_device.to_le(),
        iManufacturer: desc.i_manufacturer,
        iProduct: desc.i_product,
        iSerialNumber: desc.i_serial_number,
        bNumConfigurations: desc.b_num_configurations,
    }
}

struct DevNumAllocator {
    allocated: HashSet<u16>,
    next: u16,
}

impl DevNumAllocator {
    fn new() -> Self {
        Self { allocated: HashSet::new(), next: 2 }
    }

    fn allocate(&mut self) -> Option<u16> {
        let start = self.next;
        for i in 0..126 {
            let num = ((start - 2 + i) % 126) + 2;
            if self.allocated.insert(num) {
                self.next = if num >= 127 { 2 } else { num + 1 };
                return Some(num);
            }
        }
        None
    }

    fn release(&mut self, dev_num: u16) {
        self.allocated.remove(&dev_num);
    }
}

#[derive(Clone)]
pub struct UsbDeviceManager {
    pub(crate) bus: Bus,
    pub(crate) devices: Arc<LockDepMutex<BTreeMap<DeviceId, Arc<UsbDevice>>, UsbFsDevicesLock>>,
    pub(crate) allocator: Arc<LockDepMutex<DevNumAllocator, UsbfsDevNumAllocatorLock>>,
    pub(crate) memory: Arc<UsbfsMemoryBudget>,
}

impl UsbDeviceManager {
    pub(crate) fn new(bus: Bus) -> Self {
        Self {
            bus,
            devices: Arc::new(LockDepMutex::new(BTreeMap::new())),
            allocator: Arc::new(LockDepMutex::new(DevNumAllocator::new())),
            memory: Arc::default(),
        }
    }

    pub(crate) fn allocate_dev_num(&self) -> Option<u16> {
        self.allocator.lock().allocate()
    }

    pub(crate) fn release_dev_num(&self, dev_num: u16) {
        self.allocator.lock().release(dev_num);
    }

    pub(crate) fn get_device(&self, id: DeviceId) -> Option<Arc<UsbDevice>> {
        self.devices.lock().get(&id).cloned()
    }

    pub(crate) fn add_device(
        &self,
        kernel: &Kernel,
        device: Arc<UsbDevice>,
    ) -> Result<Device, Errno> {
        let bus_str = format!("{:03}", device.bus_num);
        let dev_str = format!("{:03}", device.dev_num);
        let devname = format!("bus/usb/{}/{}", bus_str, dev_str);
        let sysname = format!("{}-{}", device.bus_num, device.dev_num);

        let devt = DeviceId::new(USB_DEVICE_MAJOR, device.minor());

        let desc = &device.device_descriptor;
        let id_vendor = u16::from_le(desc.idVendor);
        let id_product = u16::from_le(desc.idProduct);
        let bcd_device = u16::from_le(desc.bcdDevice);
        let b_device_class = desc.bDeviceClass;
        let b_device_sub_class = desc.bDeviceSubClass;
        let b_device_protocol = desc.bDeviceProtocol;

        let metadata =
            DeviceMetadata::new(devname.into(), devt, DeviceMode::Char).with_devtype("usb_device");

        metadata.properties.insert(
            b"PRODUCT".into(),
            format!("{:x}/{:x}/{:x}", id_vendor, id_product, bcd_device).into(),
        );
        metadata.properties.insert(
            b"TYPE".into(),
            format!("{}/{}/{}", b_device_class, b_device_sub_class, b_device_protocol).into(),
        );
        metadata.properties.insert(b"BUSNUM".into(), format!("{:03}", device.bus_num).into());
        metadata.properties.insert(b"DEVNUM".into(), format!("{:03}", device.dev_num).into());

        self.devices.lock().insert(devt, device.clone());

        let device_clone = device.clone();
        let registered_device = kernel.device_registry.add_major_device(
            kernel,
            sysname.as_bytes().into(),
            metadata,
            None,
            self.bus.clone(),
            move |device_obj, dir| {
                build_usb_device_directory(device_obj, dir, &device_clone);
            },
        );

        Ok(registered_device)
    }

    pub(crate) fn remove_device(
        &self,
        current_task: &CurrentTask,
        registered_device: Device,
    ) -> Result<(), Errno> {
        if let Some(metadata) = registered_device.metadata() {
            let removed = self.devices.lock().remove(&metadata.devt);
            if let Some(device) = removed {
                self.release_dev_num(device.dev_num);
                device.on_disconnect();
            }
        }
        current_task.kernel().device_registry.remove_device(current_task, registered_device);
        Ok(())
    }
}

async fn run_in_kernel_task_async<F, R>(
    kernel: &Kernel,
    debug_name: &'static str,
    f: F,
) -> Result<R, Errno>
where
    F: FnOnce(&CurrentTask) -> R + Send + 'static,
    R: Send + 'static,
{
    let closure = move |current_task: &CurrentTask| {
        current_task.override_creds(
            starnix_core::security::creds_start_internal_operation(current_task),
            || f(current_task),
        )
    };
    let (result_fut, req) = SpawnRequestBuilder::new()
        .with_debug_name(debug_name)
        .with_sync_closure(closure)
        .build_with_async_result();
    kernel.kthreads.spawner().spawn_from_request(req);
    result_fut.await
}

pub fn usbfs_init(kernel: &Arc<Kernel>) -> Result<UsbDeviceManager, Errno> {
    let registry = &kernel.device_registry;

    let usb_bus = registry.objects.get_or_create_bus("usb".into());
    let manager = UsbDeviceManager::new(usb_bus);
    let manager_clone = manager.clone();

    registry.register_major(
        "usb".into(),
        DeviceMode::Char,
        USB_DEVICE_MAJOR,
        move |_current_task: &CurrentTask,
              id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags|
              -> Result<Box<dyn starnix_core::vfs::FileOps>, Errno> {
            let device = manager_clone.get_device(id).ok_or_else(|| errno!(ENODEV))?;
            Ok(Box::new(UsbDeviceFile::new(device)))
        },
    )?;

    let kernel_clone = Arc::clone(kernel);
    let manager_for_watcher = manager.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            watch_usb_devices(kernel_clone, manager_for_watcher).await;
        },
        "usbfs_watcher",
    );

    Ok(manager)
}

async fn watch_usb_devices(kernel: Arc<Kernel>, manager: UsbDeviceManager) {
    log_info!("Starting Starnix usbfs USB device watcher.");

    let service = match Service::open(fhardware_usb::UsbServiceMarker) {
        Ok(service) => service,
        Err(e) => {
            log_info!("Physical USB service not available: {:?}", e);
            return;
        }
    };
    watch_usb_devices_from_service(kernel, manager, service).await;
}

async fn watch_usb_devices_from_service(
    kernel: Arc<Kernel>,
    manager: UsbDeviceManager,
    service: Service<fhardware_usb::UsbServiceMarker>,
) {
    let mut watcher = match service.watch().await {
        Ok(watcher) => watcher,
        Err(e) => {
            log_info!("Failed to create USB service watcher: {:?}", e);
            return;
        }
    };

    let active_hw_ids =
        Arc::new(LockDepMutex::<HashSet<u32>, UsbfsActiveHwIdsLock>::new(HashSet::new()));
    while let Ok(Some(instance)) = watcher.try_next().await {
        let device_proxy = match instance.connect_to_device() {
            Ok(proxy) => proxy,
            Err(e) => {
                log_error!("Failed to connect to USB device: {:?}", e);
                continue;
            }
        };

        let first_config_desc = match device_proxy.get_configuration_descriptor(1).await {
            Err(fidl::Error::ClientChannelClosed { .. }) => continue,
            Ok(Err(zx::sys::ZX_ERR_NOT_SUPPORTED)) => {
                // Child interface node from usb-composite, not parent UsbDevice; skip it.
                continue;
            }
            Ok(Ok(desc)) if !desc.is_empty() => Some(desc),
            _ => None,
        };

        let hw_device_id = match device_proxy.get_device_id().await {
            Ok(id) => id,
            Err(e) => {
                log_error!("Failed to get device ID: {:?}", e);
                continue;
            }
        };
        if !active_hw_ids.lock().insert(hw_device_id) {
            continue;
        }

        let fidl_desc = match device_proxy.get_device_descriptor().await {
            Ok(desc) => desc,
            Err(e) => {
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Failed to get device descriptor: {:?}", e);
                continue;
            }
        };

        let device_descriptor = fidl_device_descriptor_to_linux(&fidl_desc);

        let mut config_descriptors = Vec::new();
        if let Some(desc) = first_config_desc {
            config_descriptors.extend_from_slice(&desc);
        }
        for cfg in 2..=device_descriptor.bNumConfigurations {
            match device_proxy.get_configuration_descriptor(cfg).await {
                Ok(Ok(desc)) if !desc.is_empty() => {
                    config_descriptors.extend_from_slice(&desc);
                }
                _ => {}
            }
        }

        let speed = match device_proxy.get_device_speed().await {
            Ok(s) => fuchsia_speed_to_linux_speed(s),
            Err(_) => USB_SPEED_FULL,
        };

        let active_config = device_proxy.get_configuration().await.unwrap_or(1);

        let assigned_dev_num = match manager.allocate_dev_num() {
            Some(n) => n,
            None => {
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Exhausted USB device numbers (2..127) for incoming device.");
                continue;
            }
        };

        let sync_proxy = match instance.connect_to_device() {
            Ok(client_proxy) => {
                let client_end = client_proxy.into_client_end().ok();
                client_end
                    .map(|c| Arc::new(fhardware_usb::UsbSynchronousProxy::new(c.into_channel())))
            }
            Err(_) => None,
        };

        let mut usb_device =
            UsbDevice::new(1, assigned_dev_num, speed, device_descriptor, config_descriptors);
        usb_device.state.get_mut().active_config = active_config;
        usb_device.sync_proxy = sync_proxy;
        usb_device.memory = Arc::clone(&manager.memory);
        let usb_device = Arc::new(usb_device);

        let add_res = manager.add_device(&kernel, Arc::clone(&usb_device));

        match add_res {
            Ok(registered_device) => {
                log_info!(
                    "Registered USB device in Starnix usbfs (/dev/bus/usb/001/{:03}) with speed {}",
                    assigned_dev_num,
                    speed
                );

                let kernel_for_close = Arc::clone(&kernel);
                let manager_for_close = manager.clone();
                let active_hw_ids_for_close = Arc::clone(&active_hw_ids);
                let proxy_for_close = device_proxy.clone();
                kernel.kthreads.spawn_future(
                    move || async move {
                        let mut event_stream = proxy_for_close.take_event_stream();
                        let _ = event_stream.try_next().await;
                        active_hw_ids_for_close.lock().remove(&hw_device_id);
                        let _ = run_in_kernel_task_async(
                            &kernel_for_close,
                            "usbfs-device-disconnect",
                            move |current_task| {
                                if manager_for_close
                                    .remove_device(current_task, registered_device)
                                    .is_ok()
                                {
                                    log_info!(
                                        "Removed disconnected USB device from Starnix usbfs (/dev/bus/usb/001/{:03})",
                                        assigned_dev_num
                                    );
                                }
                            },
                        )
                        .await;
                    },
                    "usbfs-device-lifecycle",
                );
            }
            Err(e) => {
                manager.release_dev_num(assigned_dev_num);
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Failed to register USB device in Starnix usbfs: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use crate::device::{
        DEFAULT_CANCEL_DRAIN_TIMEOUT, INVALID_USBDEVFS_BULK_SIZE, MAX_RETIRED_VMOS,
        MAX_USBDEVFS_BULK_SIZE, MAX_USBDEVFS_CONTROL_SIZE, USBFS_XFER_MAX, UsbdevfsUrb32Header,
        UsbdevfsUrbHeader, canonicalize_ioctl_request, is_arch32_ioctl_request,
    };
    use fidl::endpoints::{ControlHandle, Responder, ServerEnd, create_endpoints};
    use fidl_fuchsia_driver_test as fdt;
    use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
    use fidl_fuchsia_hardware_usb_peripheral as fusb_peripheral;
    use fidl_fuchsia_hardware_usb_request as fusb_request;
    use fidl_fuchsia_hardware_usb_virtual_bus as fusb_vbus;
    use fidl_fuchsia_hardware_usb_virtualbustest as fvirtualbustest;
    use fuchsia_component_test::{Capability, RealmBuilder};
    use fuchsia_driver_test::{DriverTestRealmBuilder2, DriverTestRealmInstance2, Options2};
    use futures::StreamExt;
    use starnix_core::device::DeviceListener;
    use starnix_core::device::kobject::{UEventAction, UEventContext};
    use starnix_core::fs::devtmpfs::DevTmpFs;
    use starnix_core::mm::syscalls::do_mmap;
    use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt, PAGE_SIZE, TaskMemoryAccessor};
    use starnix_core::signals::{SignalInfo, send_standard_signal};
    use starnix_core::task::{KernelFeatures, TaskFlags};
    use starnix_core::testing::{spawn_kernel_and_run, spawn_kernel_with_features_and_run};
    use starnix_core::vfs::buffers::VecOutputBuffer;
    use starnix_core::vfs::{FdNumber, FileHandle, FsStr, FsString, LookupContext};
    use starnix_sync::Mutex;
    use starnix_syscalls::{SUCCESS, SyscallResult};
    use starnix_uapi::errors::Errno;
    use starnix_uapi::signals::{SIGKILL, SIGUSR1};
    #[cfg(target_arch = "aarch64")]
    use starnix_uapi::uaddr32;
    #[cfg(target_arch = "aarch64")]
    use starnix_uapi::uapi::arch32;
    use starnix_uapi::user_address::{UserAddress, UserRef};
    use starnix_uapi::vfs::FdEvents;
    use starnix_uapi::{
        MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, USB_CLASS_HID, USB_CLASS_PRINTER,
        USB_CLASS_VENDOR_SPEC, USB_DIR_IN, USB_DIR_OUT, USB_DT_CONFIG, USB_DT_CONFIG_SIZE,
        USB_DT_DEVICE, USB_DT_DEVICE_SIZE, USB_DT_ENDPOINT, USB_DT_ENDPOINT_SIZE, USB_DT_INTERFACE,
        USB_DT_INTERFACE_SIZE, USB_DT_SS_ENDPOINT_COMP, USB_DT_SS_EP_COMP_SIZE,
        USB_ENDPOINT_XFER_BULK, USB_ENDPOINT_XFER_INT, USB_ENDPOINT_XFER_ISOC, USB_RECIP_ENDPOINT,
        USB_RECIP_INTERFACE, USB_REQ_CLEAR_FEATURE, USB_REQ_GET_DESCRIPTOR, USB_REQ_SET_INTERFACE,
        USB_TYPE_CLASS, USB_TYPE_STANDARD, USB_TYPE_VENDOR, USBDEVFS_BULK, USBDEVFS_CLAIMINTERFACE,
        USBDEVFS_CLEAR_HALT, USBDEVFS_CONNECT, USBDEVFS_CONNECTINFO, USBDEVFS_CONTROL,
        USBDEVFS_DISCARDURB, USBDEVFS_DISCONNECT, USBDEVFS_DISCONNECT_CLAIM,
        USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER, USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
        USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_SPEED, USBDEVFS_GETDRIVER, USBDEVFS_IOCTL,
        USBDEVFS_REAPURB, USBDEVFS_REAPURBNDELAY, USBDEVFS_RELEASEINTERFACE, USBDEVFS_RESET,
        USBDEVFS_SETCONFIGURATION, USBDEVFS_SETINTERFACE, USBDEVFS_SUBMITURB,
        USBDEVFS_URB_BULK_CONTINUATION, USBDEVFS_URB_ISO_ASAP, USBDEVFS_URB_SHORT_NOT_OK,
        USBDEVFS_URB_TYPE_BULK, USBDEVFS_URB_TYPE_CONTROL, USBDEVFS_URB_TYPE_INTERRUPT,
        USBDEVFS_URB_TYPE_ISO, USBDEVFS_URB_ZERO_PACKET, errno, error, sigaction_t, uaddr,
        usb_config_descriptor, usb_ctrlrequest, usb_endpoint_descriptor, usb_interface_descriptor,
        usbdevfs_bulktransfer, usbdevfs_connectinfo, usbdevfs_ctrltransfer,
        usbdevfs_disconnect_claim, usbdevfs_getdriver, usbdevfs_ioctl, usbdevfs_setinterface,
    };
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::mem::{align_of, size_of};
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
    use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

    const EP1_IN: u8 = 0x81;
    const EP1_OUT: u8 = 0x01;
    const EP2_IN: u8 = 0x82;
    const EP2_OUT: u8 = 0x02;
    const EP3_IN: u8 = 0x83;

    const XFER_ISO: u8 = USB_ENDPOINT_XFER_ISOC as u8;
    const XFER_BULK: u8 = USB_ENDPOINT_XFER_BULK as u8;
    const XFER_INT: u8 = USB_ENDPOINT_XFER_INT as u8;
    const CLASS_PRINTER: u8 = USB_CLASS_PRINTER as u8;
    const CLASS_VENDOR: u8 = USB_CLASS_VENDOR_SPEC as u8;

    /// Request answered by the mock device without any side effect.
    const TEST_REQUEST: u8 = 0x42;
    const INTERFACE_OUT: u32 = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_INTERFACE;
    const ENDPOINT_OUT: u32 = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT;
    const PRINTER_GET_DEVICE_ID: u32 = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;

    /// Data returned by the mock device for an IN request when no reply is scripted.
    const DEFAULT_IN_DATA: [u8; 4] = [0xaa, 0xbb, 0xcc, 0xdd];

    /// Offset of the data buffer from the ioctl argument in the buffers of `map_test_buffers`.
    const TEST_DATA_OFFSET: usize = 256;

    /// Device number of the device registered by the `run_with_*` helpers.
    const DEV_NUM: u16 = 2;

    fn create_mock_hid_descriptors() -> (usb_device_descriptor, Vec<u8>) {
        let device_desc = usb_device_descriptor {
            bLength: USB_DT_DEVICE_SIZE as u8,
            bDescriptorType: USB_DT_DEVICE as u8,
            bcdUSB: 0x0210u16.to_le(),
            bDeviceClass: 0,
            bDeviceSubClass: 0,
            bDeviceProtocol: 0,
            bMaxPacketSize0: 64,
            idVendor: 0x1234u16.to_le(),
            idProduct: 0x5678u16.to_le(),
            bcdDevice: 0x0543u16.to_le(),
            iManufacturer: 1,
            iProduct: 2,
            iSerialNumber: 3,
            bNumConfigurations: 1,
        };

        let config_desc = usb_config_descriptor {
            bLength: USB_DT_CONFIG_SIZE as u8,
            bDescriptorType: USB_DT_CONFIG as u8,
            wTotalLength: ((9 + 9 + 7 + 7) as u16).to_le(),
            bNumInterfaces: 1,
            bConfigurationValue: 1,
            iConfiguration: 0,
            bmAttributes: 0x80, // Bus powered
            bMaxPower: 50,      // 100mA
        };

        let if_desc = usb_interface_descriptor {
            bLength: USB_DT_INTERFACE_SIZE as u8,
            bDescriptorType: USB_DT_INTERFACE as u8,
            bInterfaceNumber: 0,
            bAlternateSetting: 0,
            bNumEndpoints: 2,
            bInterfaceClass: USB_CLASS_HID as u8,
            bInterfaceSubClass: 0,
            bInterfaceProtocol: 0,
            iInterface: 0,
        };

        let ep_in_desc = usb_endpoint_descriptor {
            bLength: USB_DT_ENDPOINT_SIZE as u8,
            bDescriptorType: USB_DT_ENDPOINT as u8,
            bEndpointAddress: EP1_IN,
            bmAttributes: XFER_INT,
            wMaxPacketSize: 64u16.to_le(),
            bInterval: 5,
            bRefresh: 0,
            bSynchAddress: 0,
        };

        let ep_out_desc = usb_endpoint_descriptor { bEndpointAddress: EP1_OUT, ..ep_in_desc };

        let endpoint_size = USB_DT_ENDPOINT_SIZE as usize;
        let mut config_bytes = Vec::new();
        config_bytes.extend_from_slice(config_desc.as_bytes());
        config_bytes.extend_from_slice(if_desc.as_bytes());
        config_bytes.extend_from_slice(&ep_in_desc.as_bytes()[..endpoint_size]);
        config_bytes.extend_from_slice(&ep_out_desc.as_bytes()[..endpoint_size]);

        (device_desc, config_bytes)
    }

    fn hid_config() -> Vec<u8> {
        create_mock_hid_descriptors().1
    }

    /// Interface alternate setting used to build test configuration descriptors.
    struct TestAlt {
        interface: u8,
        alt: u8,
        class: u8,
        /// `(bEndpointAddress, bmAttributes, wMaxPacketSize)` of each endpoint.
        endpoints: Vec<(u8, u8, u16)>,
    }

    fn alt(interface: u8, alt: u8, endpoints: Vec<(u8, u8, u16)>) -> TestAlt {
        TestAlt { interface, alt, class: CLASS_VENDOR, endpoints }
    }

    fn endpoint_descriptor(address: u8, attributes: u8, max_packet_size: u16) -> Vec<u8> {
        let [lo, hi] = max_packet_size.to_le_bytes();
        vec![USB_DT_ENDPOINT_SIZE as u8, USB_DT_ENDPOINT as u8, address, attributes, lo, hi, 0]
    }

    /// Builds a configuration descriptor with value `config_value` containing `alts`.
    fn build_config(config_value: u8, alts: &[TestAlt]) -> Vec<u8> {
        let num_interfaces = alts.iter().map(|a| a.interface).collect::<HashSet<_>>().len();
        let mut bytes = usb_config_descriptor {
            bLength: USB_DT_CONFIG_SIZE as u8,
            bDescriptorType: USB_DT_CONFIG as u8,
            wTotalLength: 0,
            bNumInterfaces: num_interfaces as u8,
            bConfigurationValue: config_value,
            iConfiguration: 0,
            bmAttributes: 0x80,
            bMaxPower: 50,
        }
        .as_bytes()
        .to_vec();
        for a in alts {
            let interface = usb_interface_descriptor {
                bLength: USB_DT_INTERFACE_SIZE as u8,
                bDescriptorType: USB_DT_INTERFACE as u8,
                bInterfaceNumber: a.interface,
                bAlternateSetting: a.alt,
                bNumEndpoints: a.endpoints.len() as u8,
                bInterfaceClass: a.class,
                bInterfaceSubClass: 0,
                bInterfaceProtocol: 0,
                iInterface: 0,
            };
            bytes.extend_from_slice(interface.as_bytes());
            for &(address, attributes, max_packet_size) in &a.endpoints {
                bytes.extend(endpoint_descriptor(address, attributes, max_packet_size));
            }
        }
        let [lo, hi] = (bytes.len() as u16).to_le_bytes();
        bytes[2] = lo;
        bytes[3] = hi;
        bytes
    }

    fn open_dev_node(
        current_task: &CurrentTask,
        path: &FsStr,
        flags: OpenFlags,
    ) -> Result<FileHandle, Errno> {
        let dev_root = NamespaceNode::new_anonymous(
            DevTmpFs::from_kernel(current_task.kernel()).root().clone(),
        );
        let mut context = LookupContext::default();
        let node = current_task.lookup_path(&mut context, dev_root, path)?;
        node.open(current_task, flags)
    }

    fn open_device(current_task: &CurrentTask, dev_num: u16) -> FileHandle {
        let path = format!("bus/usb/001/{:03}", dev_num);
        open_dev_node(current_task, path.as_bytes().into(), OpenFlags::RDWR).expect("open device")
    }

    fn map_test_buffer(current_task: &CurrentTask, length: usize) -> UserAddress {
        do_mmap(
            current_task,
            UserAddress::NULL,
            length,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_PRIVATE,
            FdNumber::from_raw(-1),
            0,
        )
        .unwrap()
    }

    fn map_test_page(current_task: &CurrentTask) -> UserAddress {
        map_test_buffer(current_task, *PAGE_SIZE as usize)
    }

    /// Maps a page and returns the address of an ioctl argument and of a data buffer in it.
    fn map_test_buffers(current_task: &CurrentTask) -> (UserAddress, UserAddress) {
        let arg = map_test_page(current_task);
        (arg, (arg + TEST_DATA_OFFSET).unwrap())
    }

    /// Writes `value` at `addr` and issues ioctl `request` with `addr` as argument.
    fn ioctl_with<T: IntoBytes + Immutable>(
        current_task: &CurrentTask,
        file: &FileHandle,
        request: u32,
        addr: UserAddress,
        value: &T,
    ) -> Result<SyscallResult, Errno> {
        current_task.write_object(UserRef::new(addr), value).unwrap();
        file.ops().ioctl(file, current_task, request, addr.into())
    }

    /// Issues ioctl `request` without argument.
    fn ioctl_no_arg(
        current_task: &CurrentTask,
        file: &FileHandle,
        request: u32,
    ) -> Result<SyscallResult, Errno> {
        file.ops().ioctl(file, current_task, request, 0u64.into())
    }

    /// Issues `USBDEVFS_BULK` on raw endpoint value `ep`, using `arg` for the ioctl argument and
    /// `data` as buffer.
    fn bulk_raw(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ep: u32,
        len: u32,
        timeout: u32,
        data: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        let transfer = usbdevfs_bulktransfer {
            ep,
            len,
            timeout,
            data: uaddr { addr: data.ptr() as u64 },
            ..usbdevfs_bulktransfer::new_zeroed()
        };
        ioctl_with(current_task, file, USBDEVFS_BULK, arg, &transfer)
    }

    /// Issues `USBDEVFS_BULK` on endpoint `ep`.
    fn bulk(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ep: u8,
        len: u32,
        timeout: u32,
        data: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        bulk_raw(current_task, file, arg, u32::from(ep), len, timeout, data)
    }

    /// Reads `N` bytes at `addr`.
    fn read_bytes<const N: usize>(current_task: &CurrentTask, addr: UserAddress) -> [u8; N] {
        current_task.read_object(UserRef::new(addr)).unwrap()
    }

    fn ctrl_transfer(
        request_type: u32,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: u32,
        data: UserAddress,
    ) -> usbdevfs_ctrltransfer {
        usbdevfs_ctrltransfer {
            bRequestType: request_type as u8,
            bRequest: request,
            wValue: value,
            wIndex: index,
            wLength: length,
            timeout,
            data: uaddr { addr: data.ptr() as u64 },
            ..usbdevfs_ctrltransfer::new_zeroed()
        }
    }

    /// Issues `USBDEVFS_CONTROL` without data stage.
    fn control(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        request_type: u32,
        request: u8,
        index: u16,
        timeout: u32,
    ) -> Result<SyscallResult, Errno> {
        let ctrl = ctrl_transfer(request_type, request, 0, index, 0, timeout, UserAddress::NULL);
        ioctl_with(current_task, file, USBDEVFS_CONTROL, arg, &ctrl)
    }

    fn claim(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ifno: u32,
    ) -> Result<SyscallResult, Errno> {
        ioctl_with(current_task, file, USBDEVFS_CLAIMINTERFACE, arg, &ifno)
    }

    fn release(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ifno: u32,
    ) -> Result<SyscallResult, Errno> {
        ioctl_with(current_task, file, USBDEVFS_RELEASEINTERFACE, arg, &ifno)
    }

    fn set_interface(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        interface: u32,
        altsetting: u32,
    ) -> Result<SyscallResult, Errno> {
        let set = usbdevfs_setinterface { interface, altsetting };
        ioctl_with(current_task, file, USBDEVFS_SETINTERFACE, arg, &set)
    }

    fn set_configuration(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        config: i32,
    ) -> Result<SyscallResult, Errno> {
        ioctl_with(current_task, file, USBDEVFS_SETCONFIGURATION, arg, &config)
    }

    fn clear_halt(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ep: u32,
    ) -> Result<SyscallResult, Errno> {
        ioctl_with(current_task, file, USBDEVFS_CLEAR_HALT, arg, &ep)
    }

    fn disconnect_claim(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        interface: u32,
        flags: u32,
        driver: &[u8],
    ) -> Result<SyscallResult, Errno> {
        let mut dc = usbdevfs_disconnect_claim::default();
        dc.interface = interface;
        dc.flags = flags;
        for (dst, src) in dc.driver.iter_mut().zip(driver) {
            *dst = *src as _;
        }
        ioctl_with(current_task, file, USBDEVFS_DISCONNECT_CLAIM, arg, &dc)
    }

    fn driver_ioctl(
        current_task: &CurrentTask,
        file: &FileHandle,
        arg: UserAddress,
        ifno: i32,
        ioctl_code: u32,
    ) -> Result<SyscallResult, Errno> {
        let ctl = usbdevfs_ioctl { ifno, ioctl_code: ioctl_code as i32, data: uaddr { addr: 0 } };
        ioctl_with(current_task, file, USBDEVFS_IOCTL, arg, &ctl)
    }

    /// Queues (or removes) a SIGKILL for `current_task` without making its thread group exit, so
    /// the test can observe the interrupted wait.
    fn set_pending_sigkill(current_task: &CurrentTask, pending: bool) {
        let mut task_state = current_task.write();
        if pending {
            task_state.enqueue_signal(SignalInfo::kernel(SIGKILL));
            task_state.set_flags(TaskFlags::SIGNALS_AVAILABLE, true);
        } else {
            task_state.take_specific_signal(SignalInfo::kernel(SIGKILL));
            task_state.set_flags(TaskFlags::SIGNALS_AVAILABLE, false);
        }
    }

    /// Runs `test` against device `DEV_NUM` built by `build`.
    async fn run_with_device<B, F>(build: B, test: F)
    where
        B: FnOnce(&CurrentTask) -> UsbDevice + Send + Sync + 'static,
        F: FnOnce(&CurrentTask, &Arc<UsbDevice>) + Send + Sync + 'static,
    {
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");
                let mut device = build(current_task);
                device.memory = Arc::clone(&manager.memory);
                let device = Arc::new(device);
                let registered =
                    manager.add_device(kernel, Arc::clone(&device)).expect("add_device failed");
                test(current_task, &device);
                manager.remove_device(current_task, registered).expect("remove_device failed");
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    /// Runs `test` against device `DEV_NUM` with configuration descriptors `config_bytes` and
    /// without connection to a USB device.
    async fn run_with_unconnected_device<F>(config_bytes: Vec<u8>, test: F)
    where
        F: FnOnce(&CurrentTask) + Send + Sync + 'static,
    {
        run_with_device(
            move |_| {
                let (device_desc, _) = create_mock_hid_descriptors();
                UsbDevice::new(1, DEV_NUM, USB_SPEED_HIGH, device_desc, config_bytes)
            },
            |current_task, _| test(current_task),
        )
        .await;
    }

    /// What the mock endpoint server does with the held IN requests when it receives `CancelAll`.
    #[derive(Clone, Copy, PartialEq)]
    enum CancelBehavior {
        /// Replies to `CancelAll`, then completes the held requests with `ZX_ERR_CANCELED`.
        CompleteAfterReply,
        /// Replies to `CancelAll`, then completes the held requests successfully, as if they
        /// completed just before being cancelled.
        CompleteSuccessfullyAfterReply,
        /// Never completes the held requests.
        Drop,
    }

    /// Event signaled with `USER_0` by the mock server.
    struct MockEvent(zx::Event);

    impl Default for MockEvent {
        fn default() -> Self {
            Self(zx::Event::create())
        }
    }

    impl MockEvent {
        fn signal(&self) {
            let _ = self.0.signal(zx::Signals::NONE, zx::Signals::USER_0);
        }

        fn is_signaled(&self) -> bool {
            self.0
                .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST)
                .to_result()
                .is_ok()
        }

        fn wait_signaled(&self) {
            self.0
                .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE)
                .to_result()
                .unwrap();
        }
    }

    #[derive(Default)]
    struct MockUsbServerState {
        /// Configuration values of the device, accepted by `SetConfiguration`.
        config_values: Mutex<HashSet<u8>>,
        last_ctrl_out: Mutex<Vec<u8>>,
        ctrl_transfer_error: AtomicI32,
        ctrl_timeouts: Mutex<Vec<i64>>,
        /// `(bmRequestType, bRequest, wValue, wIndex)` of each control transfer.
        ctrl_setups: Mutex<Vec<(u8, u8, u16, u16)>>,
        hold_ctrl_transfers: AtomicBool,
        held_ctrl_responders: Mutex<Vec<fhardware_usb::UsbControlTransferResponder>>,
        /// Signaled with `USER_0` when a control transfer responder is held.
        ctrl_held: MockEvent,
        set_interface_status: AtomicI32,
        last_set_iface: Mutex<Option<(u8, u8)>>,
        set_configuration_status: AtomicI32,
        /// Configuration value of each `SetConfiguration` call.
        set_config_log: Mutex<Vec<u8>>,
        /// Endpoint address of the last `ResetEndpoint` call.
        last_reset_endpoint: AtomicU8,
        reset_endpoint_count: AtomicUsize,
        reset_endpoint_status: AtomicI32,
        /// Endpoints enabled on the mock host controller.
        enabled_endpoints: Mutex<HashSet<u8>>,
        /// `(address, enable, wMaxPacketSize)` of each successful `EnableEndpoint` call.
        enable_log: Mutex<Vec<(u8, bool, u16)>>,
        /// `(address, bMaxBurst)` of the SuperSpeed companion of each enabled endpoint.
        ss_companion_log: Mutex<Vec<(u8, u8)>>,
        connect_count: AtomicUsize,
        connect_error_once: AtomicI32,
        /// Closes the next endpoint channel before serving `RegisterVmos`.
        close_endpoint_before_register: AtomicBool,
        /// Signaled with `USER_0` when usbfs closes an endpoint channel.
        endpoint_unbound: MockEvent,
        /// `(address, VMO ID)` of each registered and unregistered VMO.
        registered_vmos: Mutex<Vec<(u8, u64)>>,
        unregistered_vmos: Mutex<Vec<(u8, u64)>>,
        /// `(address, transfer type)` of each queued request.
        request_types: Mutex<Vec<(u8, &'static str)>>,
        /// `(address, size)` of each queued request.
        request_sizes: Mutex<Vec<(u8, u64)>>,
        received_bulk_out: Mutex<Vec<Vec<u8>>>,
        bulk_out_max_transfer_size: AtomicUsize,
        bulk_in_queue_count: AtomicUsize,
        bulk_in_scripted_packets: Mutex<VecDeque<(Vec<u8>, i32)>>,
        /// Reports the full scripted reply as transferred, even if it exceeds the request size.
        overreport_in_transfer_size: AtomicBool,
        hold_in_requests: AtomicBool,
        cancel_behavior: Mutex<Option<CancelBehavior>>,
        cancel_all_status: AtomicI32,
        cancel_count: AtomicUsize,
        send_stale_completion_on_cancel: AtomicBool,
        /// Completes the held requests successfully when the next requests are queued.
        complete_held_on_next_queue: AtomicBool,
        drop_endpoint_on_next_in: AtomicBool,
        /// Closes the endpoint channel when serving `CancelAll`.
        drop_endpoint_on_cancel: AtomicBool,
        /// Closes the endpoint channel when serving `EnableEndpoint { enable: false }`.
        close_endpoint_on_disable: AtomicBool,
        endpoint_abort_senders: Mutex<
            HashMap<
                u8,
                Vec<futures::channel::oneshot::Sender<futures::channel::oneshot::Sender<()>>>,
            >,
        >,
        /// Number of `QueueRequests` calls received while `ep_addr` was not in `enabled_endpoints`.
        invalid_queue_while_disabled: AtomicUsize,
        /// Optional hook invoked when serving `ConnectToEndpoint`.
        on_connect_to_endpoint: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    fn request_type_name(request: &fusb_request::Request) -> &'static str {
        match request.information {
            Some(fusb_request::RequestInfo::Bulk(_)) => "bulk",
            Some(fusb_request::RequestInfo::Interrupt(_)) => "interrupt",
            Some(fusb_request::RequestInfo::Isochronous(_)) => "isochronous",
            Some(fusb_request::RequestInfo::Control(_)) => "control",
            _ => "unknown",
        }
    }

    /// Returns the `(VMO ID, offset, size)` of the buffer of `request`.
    fn request_region(request: &fusb_request::Request) -> (u64, u64, u64) {
        let region = request.data.as_ref().and_then(|d| d.first()).unwrap();
        let id = match region.buffer {
            Some(fusb_request::Buffer::VmoId(id)) => id,
            _ => panic!("unexpected buffer"),
        };
        (id, region.offset.unwrap_or(0), region.size.unwrap_or(0))
    }

    /// Completes `request` successfully, filling its buffer with the next scripted reply.
    fn complete_in_request(
        state: &MockUsbServerState,
        vmos: &HashMap<u64, zx::Vmo>,
        request: fusb_request::Request,
    ) -> fusb_endpoint::Completion {
        let (id, offset, size) = request_region(&request);
        let (reply, status) =
            state.bulk_in_scripted_packets.lock().pop_front().unwrap_or_else(|| {
                let len = std::cmp::min(size as usize, DEFAULT_IN_DATA.len());
                (DEFAULT_IN_DATA[..len].to_vec(), zx::sys::ZX_OK)
            });
        // Like a host controller, never write beyond the request buffer.
        let written = std::cmp::min(reply.len(), size as usize);
        if written > 0 {
            vmos[&id].write(&reply[..written], offset).unwrap();
        }
        let reported = if state.overreport_in_transfer_size.load(Ordering::SeqCst) {
            reply.len()
        } else {
            written
        };
        fusb_endpoint::Completion {
            request: Some(request),
            status: Some(status),
            transfer_size: Some(reported as u64),
            ..Default::default()
        }
    }

    fn cancelled_completion(request: fusb_request::Request) -> fusb_endpoint::Completion {
        fusb_endpoint::Completion {
            request: Some(request),
            status: Some(zx::sys::ZX_ERR_CANCELED),
            transfer_size: Some(0),
            ..Default::default()
        }
    }

    /// Serves `ep` like the xHCI endpoint server.
    ///
    /// When the server closes the channel, it disables the endpoint first. When usbfs closes the
    /// channel, the endpoint stays enabled, which models the asynchronous disable of the xHCI
    /// driver losing the race with a later enable.
    async fn serve_mock_endpoint(
        ep_addr: u8,
        ep: ServerEnd<fusb_endpoint::EndpointMarker>,
        state: Arc<MockUsbServerState>,
        mut abort_rx: futures::channel::oneshot::Receiver<futures::channel::oneshot::Sender<()>>,
    ) {
        let mut ep_stream = ep.into_stream();
        let mut vmos: HashMap<u64, zx::Vmo> = HashMap::new();
        let mut held: Vec<fusb_request::Request> = Vec::new();
        let is_in = (ep_addr & (USB_DIR_IN as u8)) != 0;
        loop {
            let next_req = match futures::future::select(ep_stream.try_next(), &mut abort_rx).await
            {
                futures::future::Either::Left((Ok(Some(ep_req)), _)) => ep_req,
                futures::future::Either::Left((_, _)) => break,
                futures::future::Either::Right((Ok(done_tx), _)) => {
                    drop(ep_stream);
                    let _ = done_tx.send(());
                    state.endpoint_unbound.signal();
                    return;
                }
                futures::future::Either::Right((Err(_), _)) => break,
            };
            match next_req {
                fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                    let mut registered = Vec::new();
                    for info in vmo_ids {
                        if let (Some(id), Some(size)) = (info.id, info.size) {
                            let vmo = zx::Vmo::create(size).unwrap();
                            let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                            vmos.insert(id, vmo);
                            state.registered_vmos.lock().push((ep_addr, id));
                            registered.push(fusb_endpoint::VmoHandle {
                                id: Some(id),
                                vmo: Some(dup),
                                ..Default::default()
                            });
                        }
                    }
                    let _ = responder.send(registered);
                }
                fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                    for id in vmo_ids {
                        vmos.remove(&id);
                        state.unregistered_vmos.lock().push((ep_addr, id));
                    }
                    let _ = responder.send(&[], &[]);
                }
                fusb_endpoint::EndpointRequest::QueueRequests { req: reqs, control_handle } => {
                    if !state.enabled_endpoints.lock().contains(&ep_addr) {
                        state.invalid_queue_while_disabled.fetch_add(1, Ordering::SeqCst);
                    }
                    if is_in && state.drop_endpoint_on_next_in.swap(false, Ordering::SeqCst) {
                        // Like xHCI, disable the endpoint before closing the channel.
                        state.enabled_endpoints.lock().remove(&ep_addr);
                        return;
                    }
                    if state.complete_held_on_next_queue.swap(false, Ordering::SeqCst) {
                        let completions: Vec<_> = held
                            .drain(..)
                            .map(|r| {
                                let (id, offset, size) = request_region(&r);
                                vmos[&id].write(&vec![0x99; size as usize], offset).unwrap();
                                fusb_endpoint::Completion {
                                    request: Some(r),
                                    status: Some(zx::sys::ZX_OK),
                                    transfer_size: Some(size),
                                    ..Default::default()
                                }
                            })
                            .collect();
                        let _ = control_handle.send_on_completion(completions);
                    }
                    let mut completions = Vec::new();
                    for r in reqs {
                        state.request_types.lock().push((ep_addr, request_type_name(&r)));
                        let (id, offset, size) = request_region(&r);
                        state.request_sizes.lock().push((ep_addr, size));
                        if is_in {
                            state.bulk_in_queue_count.fetch_add(1, Ordering::SeqCst);
                            if state.hold_in_requests.load(Ordering::SeqCst) {
                                held.push(r);
                                continue;
                            }
                            completions.push(complete_in_request(&state, &vmos, r));
                        } else {
                            let max_out = state.bulk_out_max_transfer_size.load(Ordering::Relaxed);
                            let actual_size = std::cmp::min(size as usize, max_out);
                            let mut buf = vec![0u8; actual_size];
                            if actual_size > 0 {
                                vmos[&id].read(&mut buf, offset).unwrap();
                            }
                            state.received_bulk_out.lock().push(buf);
                            completions.push(fusb_endpoint::Completion {
                                request: Some(r),
                                status: Some(zx::sys::ZX_OK),
                                transfer_size: Some(actual_size as u64),
                                ..Default::default()
                            });
                        }
                    }
                    if !completions.is_empty() {
                        let _ = control_handle.send_on_completion(completions);
                    }
                }
                fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                    state.cancel_count.fetch_add(1, Ordering::SeqCst);
                    if state.drop_endpoint_on_cancel.swap(false, Ordering::SeqCst) {
                        state.enabled_endpoints.lock().remove(&ep_addr);
                        return;
                    }
                    let status = state.cancel_all_status.load(Ordering::SeqCst);
                    if status != zx::sys::ZX_OK {
                        let _ = responder.send(Err(status));
                        continue;
                    }
                    let control_handle = responder.control_handle().clone();
                    if state.send_stale_completion_on_cancel.swap(false, Ordering::SeqCst) {
                        let _ =
                            control_handle.send_on_completion(vec![fusb_endpoint::Completion {
                                status: Some(zx::sys::ZX_ERR_CANCELED),
                                transfer_size: Some(0),
                                ..Default::default()
                            }]);
                    }
                    let _ = responder.send(Ok(()));
                    if held.is_empty() {
                        continue;
                    }
                    let behavior =
                        state.cancel_behavior.lock().unwrap_or(CancelBehavior::CompleteAfterReply);
                    let completions: Vec<_> = match behavior {
                        CancelBehavior::CompleteAfterReply => {
                            held.drain(..).map(cancelled_completion).collect()
                        }
                        CancelBehavior::CompleteSuccessfullyAfterReply => {
                            held.drain(..).map(|r| complete_in_request(&state, &vmos, r)).collect()
                        }
                        CancelBehavior::Drop => continue,
                    };
                    let _ = control_handle.send_on_completion(completions);
                }
                _ => {}
            }
        }
        state.endpoint_unbound.signal();
    }

    fn status_result(status: i32) -> Result<(), i32> {
        if status == zx::sys::ZX_OK { Ok(()) } else { Err(status) }
    }

    fn spawn_mock_usb_server(
        kernel: &Arc<Kernel>,
        config_values: HashSet<u8>,
    ) -> (Arc<fhardware_usb::UsbSynchronousProxy>, Arc<MockUsbServerState>) {
        let (client_end, server_end) = create_endpoints::<fhardware_usb::UsbMarker>();
        let sync_proxy =
            Arc::new(fhardware_usb::UsbSynchronousProxy::new(client_end.into_channel()));

        let state = Arc::new(MockUsbServerState {
            config_values: Mutex::new(config_values),
            bulk_out_max_transfer_size: AtomicUsize::new(usize::MAX),
            ..Default::default()
        });

        let state_srv = Arc::clone(&state);
        let kernel_for_srv = Arc::clone(kernel);

        kernel.kthreads.spawn_future(
            move || async move {
                let mut stream = server_end.into_stream();
                while let Ok(Some(req)) = stream.try_next().await {
                    match req {
                        fhardware_usb::UsbRequest::ControlTransfer {
                            setup,
                            timeout_ns,
                            write_data,
                            responder,
                        } => {
                            state_srv.ctrl_timeouts.lock().push(timeout_ns);
                            state_srv.ctrl_setups.lock().push((
                                setup.bm_request_type,
                                setup.b_request,
                                setup.w_value,
                                setup.w_index,
                            ));
                            let is_in = (setup.bm_request_type & (USB_DIR_IN as u8)) != 0;
                            // Like usb-bus, the OUT data must match wLength.
                            let expected_len = if is_in { 0 } else { usize::from(setup.w_length) };
                            if write_data.len() != expected_len {
                                let _ = responder.send(Err(zx::sys::ZX_ERR_INVALID_ARGS));
                                continue;
                            }
                            if state_srv.hold_ctrl_transfers.load(Ordering::SeqCst) {
                                state_srv.held_ctrl_responders.lock().push(responder);
                                state_srv.ctrl_held.signal();
                                continue;
                            }
                            let err = state_srv.ctrl_transfer_error.load(Ordering::Relaxed);
                            if err != zx::sys::ZX_OK {
                                let _ = responder.send(Err(err));
                                continue;
                            }
                            if is_in {
                                let reply =
                                    vec![setup.b_request, (setup.w_value & 0xff) as u8, 0xca, 0xfe];
                                let _ = responder.send(Ok(&reply));
                            } else {
                                *state_srv.last_ctrl_out.lock() = write_data;
                                let _ = responder.send(Ok(&[]));
                            }
                        }
                        fhardware_usb::UsbRequest::SetInterface {
                            interface_number,
                            alt_setting,
                            responder,
                        } => {
                            let status = state_srv.set_interface_status.load(Ordering::Relaxed);
                            if status == zx::sys::ZX_OK {
                                *state_srv.last_set_iface.lock() =
                                    Some((interface_number, alt_setting));
                            }
                            let _ = responder.send(status_result(status));
                        }
                        fhardware_usb::UsbRequest::SetConfiguration {
                            configuration,
                            responder,
                        } => {
                            state_srv.set_config_log.lock().push(configuration);
                            // Like usb-bus, only configurations of the device are accepted.
                            let status = if state_srv.config_values.lock().contains(&configuration)
                            {
                                state_srv.set_configuration_status.load(Ordering::Relaxed)
                            } else {
                                zx::sys::ZX_ERR_INVALID_ARGS
                            };
                            let _ = responder.send(status_result(status));
                        }
                        fhardware_usb::UsbRequest::EnableEndpoint {
                            ep_desc,
                            ss_com_desc,
                            enable,
                            responder,
                        } => {
                            // Like the xHCI driver, enabling an enabled endpoint or disabling a
                            // disabled one fails.
                            let address = ep_desc.b_endpoint_address;
                            let ok = {
                                let mut enabled = state_srv.enabled_endpoints.lock();
                                if enable {
                                    enabled.insert(address)
                                } else {
                                    enabled.remove(&address)
                                }
                            };
                            if ok {
                                state_srv.enable_log.lock().push((
                                    address,
                                    enable,
                                    ep_desc.w_max_packet_size,
                                ));
                                if enable {
                                    state_srv
                                        .ss_companion_log
                                        .lock()
                                        .push((address, ss_com_desc.b_max_burst));
                                } else if state_srv
                                    .close_endpoint_on_disable
                                    .swap(false, Ordering::SeqCst)
                                {
                                    let abort_senders = state_srv
                                        .endpoint_abort_senders
                                        .lock()
                                        .remove(&address)
                                        .unwrap_or_default();
                                    for abort_tx in abort_senders {
                                        let (done_tx, done_rx) =
                                            futures::channel::oneshot::channel();
                                        if abort_tx.send(done_tx).is_ok() {
                                            let _ = done_rx.await;
                                        }
                                    }
                                }
                                let _ = responder.send(Ok(()));
                            } else {
                                let _ = responder.send(Err(zx::sys::ZX_ERR_BAD_STATE));
                            }
                        }
                        fhardware_usb::UsbRequest::ResetEndpoint { ep_address, responder } => {
                            state_srv.last_reset_endpoint.store(ep_address, Ordering::Relaxed);
                            state_srv.reset_endpoint_count.fetch_add(1, Ordering::Relaxed);
                            let status = state_srv.reset_endpoint_status.load(Ordering::Relaxed);
                            let _ = responder.send(status_result(status));
                        }
                        fhardware_usb::UsbRequest::ConnectToEndpoint { ep_addr, ep, responder } => {
                            if let Some(hook) = state_srv.on_connect_to_endpoint.lock().take() {
                                hook();
                            }
                            let err =
                                state_srv.connect_error_once.swap(zx::sys::ZX_OK, Ordering::SeqCst);
                            if err != zx::sys::ZX_OK {
                                let _ = responder.send(Err(err));
                                continue;
                            }
                            state_srv.connect_count.fetch_add(1, Ordering::SeqCst);
                            let _ = responder.send(Ok(()));
                            if state_srv
                                .close_endpoint_before_register
                                .swap(false, Ordering::SeqCst)
                            {
                                drop(ep);
                                continue;
                            }
                            let ep_state = Arc::clone(&state_srv);
                            let (abort_tx, abort_rx) = futures::channel::oneshot::channel();
                            state_srv
                                .endpoint_abort_senders
                                .lock()
                                .entry(ep_addr)
                                .or_default()
                                .push(abort_tx);
                            kernel_for_srv.kthreads.spawn_future(
                                move || serve_mock_endpoint(ep_addr, ep, ep_state, abort_rx),
                                "mock_usb_ep_server",
                            );
                        }
                        _ => {}
                    }
                }
            },
            "mock_usb_server",
        );

        (sync_proxy, state)
    }

    /// Returns the configuration values of the configuration descriptors in `config_bytes`.
    fn config_values(config_bytes: &[u8]) -> HashSet<u8> {
        let mut values = HashSet::new();
        let mut rest = config_bytes;
        while rest.len() >= 2 && usize::from(rest[0]) >= 2 && usize::from(rest[0]) <= rest.len() {
            if u32::from(rest[1]) == USB_DT_CONFIG && rest.len() >= USB_DT_CONFIG_SIZE as usize {
                values.insert(rest[5]);
            }
            rest = &rest[usize::from(rest[0])..];
        }
        values
    }

    /// Device served by a mock USB server.
    struct MockDeviceOptions {
        config_bytes: Vec<u8>,
        speed: u32,
        active_config: u8,
        cancel_drain_timeout: zx::MonotonicDuration,
    }

    impl MockDeviceOptions {
        fn new(config_bytes: Vec<u8>) -> Self {
            Self {
                config_bytes,
                speed: USB_SPEED_HIGH,
                active_config: 1,
                cancel_drain_timeout: DEFAULT_CANCEL_DRAIN_TIMEOUT,
            }
        }
    }

    /// Runs `test` against device `DEV_NUM` served by a mock USB server.
    async fn run_with_mock_device_options<F>(options: MockDeviceOptions, test: F)
    where
        F: FnOnce(&CurrentTask, &Arc<UsbDevice>, &Arc<MockUsbServerState>) + Send + Sync + 'static,
    {
        let mock_slot: Arc<Mutex<Option<Arc<MockUsbServerState>>>> = Default::default();
        let mock_for_build = Arc::clone(&mock_slot);
        run_with_device(
            move |current_task| {
                let (sync_proxy, mock) = spawn_mock_usb_server(
                    current_task.kernel(),
                    config_values(&options.config_bytes),
                );
                *mock_for_build.lock() = Some(mock);
                let (device_desc, _) = create_mock_hid_descriptors();
                let mut device =
                    UsbDevice::new(1, DEV_NUM, options.speed, device_desc, options.config_bytes);
                device.sync_proxy = Some(sync_proxy);
                device.cancel_drain_timeout = options.cancel_drain_timeout;
                device.state.get_mut().active_config = options.active_config;
                device
            },
            move |current_task, device| {
                let mock = mock_slot.lock().take().unwrap();
                test(current_task, device, &mock);
            },
        )
        .await;
    }

    /// Runs `test` against device `DEV_NUM` served by a mock USB server with configuration
    /// descriptors `config_bytes`.
    async fn run_with_mock_device<F>(config_bytes: Vec<u8>, test: F)
    where
        F: FnOnce(&CurrentTask, &Arc<UsbDevice>, &Arc<MockUsbServerState>) + Send + Sync + 'static,
    {
        run_with_mock_device_options(MockDeviceOptions::new(config_bytes), test).await;
    }

    #[::fuchsia::test]
    async fn test_usb_device_descriptor_layout() {
        assert_eq!(std::mem::size_of::<usb_device_descriptor>(), 18);
        assert_eq!(std::mem::size_of::<usb_config_descriptor>(), 9);
        assert_eq!(std::mem::size_of::<usb_interface_descriptor>(), 9);
        assert_eq!(std::mem::size_of::<usb_endpoint_descriptor>(), 9);

        let (device_desc, config_bytes) = create_mock_hid_descriptors();
        let device = UsbDevice::new(1, 2, USB_SPEED_HIGH, device_desc, config_bytes);

        assert_eq!(device.bus_num, 1);
        assert_eq!(device.dev_num, 2);
        assert_eq!(device.minor(), 1);
        assert_eq!(device.speed, USB_SPEED_HIGH);
        assert_eq!(device.raw_descriptors.len(), 18 + 9 + 9 + 7 + 7);
    }

    #[::fuchsia::test]
    async fn test_usbfs_registration_and_devtmpfs() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let usb_dev =
                    Arc::new(UsbDevice::new(1, 2, USB_SPEED_HIGH, device_desc, config_bytes));

                let registered = manager.add_device(kernel, usb_dev).expect("add_device failed");
                let metadata = registered.metadata().expect("metadata present");
                assert_eq!(metadata.devname.as_slice(), b"bus/usb/001/002");
                assert_eq!(metadata.devt.major(), USB_DEVICE_MAJOR);
                assert_eq!(metadata.devt.minor(), 1);

                let node = open_dev_node(current_task, b"bus/usb/001/002".into(), OpenFlags::RDWR)
                    .expect("open_dev_node failed");
                assert!(node.ops().is_seekable());

                // Verify device appears under /sys/bus/usb/devices/1-2.
                assert!(
                    kernel
                        .device_registry
                        .objects
                        .root
                        .lookup("bus/usb/devices/1-2".into())
                        .is_some()
                );

                manager.remove_device(current_task, registered).expect("remove_device failed");

                // Verify device is removed from /sys/bus/usb/devices/1-2.
                assert!(
                    kernel
                        .device_registry
                        .objects
                        .root
                        .lookup("bus/usb/devices/1-2".into())
                        .is_none()
                );
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_read_descriptors() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let mut expected_raw = Vec::new();
                expected_raw.extend_from_slice(device_desc.as_bytes());
                expected_raw.extend_from_slice(&config_bytes);

                let usb_dev = Arc::new(UsbDevice::new(
                    1,
                    3,
                    USB_SPEED_FULL,
                    device_desc,
                    config_bytes.clone(),
                ));
                let registered = manager.add_device(kernel, usb_dev).unwrap();

                let file =
                    open_dev_node(current_task, b"bus/usb/001/003".into(), OpenFlags::RDONLY)
                        .expect("open_dev_node failed");

                let mut output_buf = VecOutputBuffer::new(128);
                let bytes_read = file.ops().read(&file, current_task, 0, &mut output_buf).unwrap();

                assert_eq!(bytes_read, expected_raw.len());
                assert_eq!(Vec::from(output_buf), expected_raw);

                manager.remove_device(current_task, registered).unwrap();
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    struct MockUEventListener {
        add_count: Arc<AtomicUsize>,
        remove_count: Arc<AtomicUsize>,
    }

    impl DeviceListener for MockUEventListener {
        fn on_device_event(&self, action: UEventAction, device: Device, _context: UEventContext) {
            if let Some(metadata) = device.metadata() {
                if metadata.devname.as_slice() == b"bus/usb/001/005" {
                    match action {
                        UEventAction::Add => {
                            self.add_count.fetch_add(1, Ordering::SeqCst);
                        }
                        UEventAction::Remove => {
                            self.remove_count.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    #[::fuchsia::test]
    async fn test_usbfs_uevent_emission() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");

            let add_count = Arc::new(AtomicUsize::new(0));
            let remove_count = Arc::new(AtomicUsize::new(0));
            let listener = MockUEventListener {
                add_count: add_count.clone(),
                remove_count: remove_count.clone(),
            };

            let _listener_key = kernel.device_registry.register_listener(listener);

            let (device_desc, config_bytes) = create_mock_hid_descriptors();
            let usb_dev = Arc::new(UsbDevice::new(1, 5, USB_SPEED_HIGH, device_desc, config_bytes));
            let registered = manager.add_device(kernel, usb_dev).unwrap();

            let props: HashMap<FsString, FsString> =
                registered.get_uevent_properties_list().into_iter().collect();
            assert_eq!(props.get(&FsString::from(b"SUBSYSTEM")).unwrap().as_slice(), b"usb");
            assert_eq!(props.get(&FsString::from(b"DEVTYPE")).unwrap().as_slice(), b"usb_device");
            assert_eq!(props.get(&FsString::from(b"BUSNUM")).unwrap().as_slice(), b"001");
            assert_eq!(props.get(&FsString::from(b"DEVNUM")).unwrap().as_slice(), b"005");
            assert_eq!(
                props.get(&FsString::from(b"PRODUCT")).unwrap().as_slice(),
                b"1234/5678/543"
            );

            assert_eq!(add_count.load(Ordering::SeqCst), 1);
            assert_eq!(remove_count.load(Ordering::SeqCst), 0);

            manager.remove_device(current_task, registered).unwrap();
            assert_eq!(remove_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multidevice_minor_allocation_and_cleanup() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");

            let dev1 = manager.allocate_dev_num().expect("alloc 1");
            let dev2 = manager.allocate_dev_num().expect("alloc 2");
            assert_eq!(dev1, 2);
            assert_eq!(dev2, 3);

            manager.release_dev_num(dev1);
            let dev3 = manager.allocate_dev_num().expect("alloc 3");
            assert_eq!(dev3, 4, "Monotonic device number allocation must increment");

            manager.release_dev_num(dev3);
            manager.release_dev_num(dev2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_watch_usb_devices_missing_service_handled_gracefully() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");
            assert!(manager.allocate_dev_num().is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_interface_claim_release_lifecycle() {
        run_with_unconnected_device(hid_config(), |current_task| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));
            assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));

            // Claiming non-existent interface < 64 returns ENOENT; >= 64 returns EINVAL.
            assert_eq!(claim(current_task, &file1, arg, 5), error!(ENOENT));
            assert_eq!(claim(current_task, &file1, arg, 99), error!(EINVAL));

            assert_eq!(claim(current_task, &file2, arg, 0), error!(EBUSY));

            // Releasing non-existent interface < 64 returns ENOENT; >= 64 returns EINVAL;
            // releasing an interface not claimed by this FD returns EINVAL.
            assert_eq!(release(current_task, &file1, arg, 5), error!(ENOENT));
            assert_eq!(release(current_task, &file1, arg, 99), error!(EINVAL));
            assert_eq!(release(current_task, &file2, arg, 0), error!(EINVAL));

            assert_eq!(release(current_task, &file1, arg, 0), Ok(SUCCESS));
            assert_eq!(claim(current_task, &file2, arg, 0), Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_interface_claim_release_on_file_close() {
        run_with_unconnected_device(hid_config(), |current_task| {
            let (arg, _) = map_test_buffers(current_task);
            {
                let file1 = open_device(current_task, DEV_NUM);
                assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));
            }
            current_task.kernel().delayed_releaser.apply(current_task);

            let file2 = open_device(current_task, DEV_NUM);
            assert_eq!(claim(current_task, &file2, arg, 0), Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_empty_descriptors_have_no_interfaces_or_endpoints() {
        run_with_unconnected_device(vec![], |current_task| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(claim(current_task, &file, arg, 0), error!(ENOENT));
            // Without a configuration, endpoints cannot be looked up.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 0, data), error!(ESRCH));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_control_transfer_size_limit() {
        run_with_unconnected_device(hid_config(), |current_task| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let ctrl = ctrl_transfer(
                USB_DIR_IN,
                USB_REQ_GET_DESCRIPTOR as u8,
                (USB_DT_DEVICE as u16) << 8,
                0,
                (MAX_USBDEVFS_CONTROL_SIZE + 1) as u16,
                1000,
                UserAddress::NULL,
            );
            assert_eq!(
                ioctl_with(current_task, &file, USBDEVFS_CONTROL, arg, &ctrl),
                error!(EINVAL)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_compat32_control_and_bulk_struct_layout() {
        #[cfg(target_arch = "aarch64")]
        {
            assert_eq!(std::mem::size_of::<arch32::usbdevfs_ctrltransfer>(), 16);
            assert_eq!(std::mem::size_of::<arch32::usbdevfs_bulktransfer>(), 16);
            assert_eq!(std::mem::size_of::<arch32::usbdevfs_ioctl>(), 12);

            for (arch32_request, expected) in [
                (arch32::USBDEVFS_CONTROL, USBDEVFS_CONTROL),
                (arch32::USBDEVFS_BULK, USBDEVFS_BULK),
                (arch32::USBDEVFS_SETINTERFACE, USBDEVFS_SETINTERFACE),
                (arch32::USBDEVFS_SETCONFIGURATION, USBDEVFS_SETCONFIGURATION),
                (arch32::USBDEVFS_GETDRIVER, USBDEVFS_GETDRIVER),
                (arch32::USBDEVFS_CONNECTINFO, USBDEVFS_CONNECTINFO),
                (arch32::USBDEVFS_RESET, USBDEVFS_RESET),
                (arch32::USBDEVFS_CLEAR_HALT, USBDEVFS_CLEAR_HALT),
                (arch32::USBDEVFS_RELEASEINTERFACE, USBDEVFS_RELEASEINTERFACE),
                (arch32::USBDEVFS_CLAIMINTERFACE, USBDEVFS_CLAIMINTERFACE),
                (arch32::USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_CAPABILITIES),
                (arch32::USBDEVFS_DISCONNECT_CLAIM, USBDEVFS_DISCONNECT_CLAIM),
                (arch32::USBDEVFS_GET_SPEED, USBDEVFS_GET_SPEED),
                (arch32::USBDEVFS_IOCTL, USBDEVFS_IOCTL),
            ] {
                assert_eq!(
                    canonicalize_ioctl_request(true, arch32_request),
                    expected,
                    "request {arch32_request:#x}"
                );
            }
            assert_eq!(
                canonicalize_ioctl_request(false, arch32::USBDEVFS_CONTROL),
                arch32::USBDEVFS_CONTROL
            );

            let ctrl32 = arch32::usbdevfs_ctrltransfer {
                bRequestType: USB_DIR_IN as u8,
                bRequest: USB_REQ_GET_DESCRIPTOR as u8,
                wValue: (USB_DT_DEVICE as u16) << 8,
                wIndex: 0,
                wLength: 18,
                timeout: 500,
                data: uaddr32 { addr: 0x1234_5678 },
            };
            let converted_ctrl = usbdevfs_ctrltransfer::from(ctrl32);
            assert_eq!(converted_ctrl.bRequestType, USB_DIR_IN as u8);
            assert_eq!(converted_ctrl.wLength, 18);
            assert_eq!(converted_ctrl.data.addr, 0x1234_5678);

            let bulk32 = arch32::usbdevfs_bulktransfer {
                ep: u32::from(EP1_IN),
                len: 64,
                timeout: 250,
                data: uaddr32 { addr: 0x8765_4321 },
            };
            let converted_bulk = usbdevfs_bulktransfer::from(bulk32);
            assert_eq!(converted_bulk.ep, u32::from(EP1_IN));
            assert_eq!(converted_bulk.len, 64);
            assert_eq!(converted_bulk.data.addr, 0x8765_4321);

            let ioctl32 = arch32::usbdevfs_ioctl {
                ifno: 1,
                ioctl_code: USBDEVFS_DISCONNECT as i32,
                data: uaddr32 { addr: 0xdead_beef },
            };
            let converted_ioctl = usbdevfs_ioctl::from(ioctl32);
            assert_eq!(converted_ioctl.ifno, 1);
            assert_eq!(converted_ioctl.ioctl_code, USBDEVFS_DISCONNECT as i32);
            assert_eq!(converted_ioctl.data.addr, 0xdead_beef);
        }
        assert_eq!(std::mem::size_of::<usbdevfs_ctrltransfer>(), 24);
        assert_eq!(std::mem::size_of::<usbdevfs_bulktransfer>(), 24);
        assert_eq!(std::mem::size_of::<usbdevfs_ioctl>(), 16);
        assert_eq!(canonicalize_ioctl_request(false, USBDEVFS_CONTROL), USBDEVFS_CONTROL);
    }

    #[::fuchsia::test]
    fn test_native_requests_are_not_arch32_requests() {
        // On x86_64, `uapi::arch32` aliases the native definitions, so a native request number
        // must not be mistaken for a 32-bit compat one.
        for request in [
            USBDEVFS_CONTROL,
            USBDEVFS_BULK,
            starnix_uapi::USBDEVFS_DISCSIGNAL,
            USBDEVFS_SUBMITURB,
            USBDEVFS_REAPURB,
            USBDEVFS_REAPURBNDELAY,
            starnix_uapi::USBDEVFS_IOCTL,
            USBDEVFS_CLAIMINTERFACE,
        ] {
            assert!(!is_arch32_ioctl_request(request), "request {request:#x}");
        }
        #[cfg(target_arch = "aarch64")]
        for request in [
            arch32::USBDEVFS_CONTROL,
            arch32::USBDEVFS_BULK,
            arch32::USBDEVFS_DISCSIGNAL,
            arch32::USBDEVFS_SUBMITURB,
            arch32::USBDEVFS_REAPURB,
            arch32::USBDEVFS_REAPURBNDELAY,
            arch32::USBDEVFS_IOCTL,
        ] {
            assert!(is_arch32_ioctl_request(request), "request {request:#x}");
        }
    }

    #[::fuchsia::test]
    async fn test_disconnected_device_rejects_all_ioctls_with_enodev() {
        run_with_mock_device(hid_config(), |current_task, device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            device.on_disconnect();

            for request in [
                USBDEVFS_CONTROL,
                USBDEVFS_BULK,
                USBDEVFS_CLAIMINTERFACE,
                USBDEVFS_RELEASEINTERFACE,
                USBDEVFS_SETINTERFACE,
                USBDEVFS_SETCONFIGURATION,
                USBDEVFS_CLEAR_HALT,
                USBDEVFS_RESET,
                USBDEVFS_GETDRIVER,
                USBDEVFS_GET_SPEED,
                USBDEVFS_GET_CAPABILITIES,
                USBDEVFS_CONNECTINFO,
                USBDEVFS_DISCONNECT_CLAIM,
                USBDEVFS_IOCTL,
            ] {
                assert_eq!(
                    file.ops().ioctl(&file, current_task, request, arg.into()),
                    error!(ENODEV),
                    "request {request:#x}"
                );
            }

            let mut output_buf = VecOutputBuffer::new(16);
            let res_read = file.ops().read(&file, current_task, 0, &mut output_buf);
            assert_eq!(res_read, error!(ENODEV));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_closed_usb_channel_returns_enodev_for_control_and_bulk() {
        run_with_device(
            |_| {
                let (client_end, server_end) = create_endpoints::<fhardware_usb::UsbMarker>();
                drop(server_end);
                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let mut device =
                    UsbDevice::new(1, DEV_NUM, USB_SPEED_HIGH, device_desc, config_bytes);
                device.sync_proxy = Some(Arc::new(fhardware_usb::UsbSynchronousProxy::new(
                    client_end.into_channel(),
                )));
                device
            },
            |current_task, _device| {
                let file = open_device(current_task, DEV_NUM);
                let (arg, data) = map_test_buffers(current_task);

                assert_eq!(
                    control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 1000),
                    error!(ENODEV)
                );
                assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENODEV));
            },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_invalid_and_unknown_ioctls() {
        run_with_unconnected_device(hid_config(), |current_task| {
            let file = open_device(current_task, DEV_NUM);

            assert_eq!(ioctl_no_arg(current_task, &file, 0xdeadbeef), error!(ENOTTY));
            assert_eq!(ioctl_no_arg(current_task, &file, USBDEVFS_GETDRIVER), error!(EFAULT));
            assert_eq!(
                ioctl_no_arg(current_task, &file, USBDEVFS_GET_SPEED),
                Ok((USB_SPEED_HIGH as usize).into())
            );

            // Opening with O_RDONLY rejects all USBDEVFS_* ioctls with EPERM.
            let ro_file = open_dev_node(current_task, b"bus/usb/001/002".into(), OpenFlags::RDONLY)
                .expect("open ro_file");
            assert_eq!(ioctl_no_arg(current_task, &ro_file, USBDEVFS_GET_SPEED), error!(EPERM));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_fidl_synchronous_control_transfers() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            current_task.write_object(UserRef::new(data), &[0u8; 4]).unwrap();
            let ctrl_in = ctrl_transfer(USB_DIR_IN, TEST_REQUEST, 0x0102, 0, 4, 1000, data);
            assert_eq!(
                ioctl_with(current_task, &file, USBDEVFS_CONTROL, arg, &ctrl_in),
                Ok(4.into())
            );
            assert_eq!(read_bytes::<4>(current_task, data), [TEST_REQUEST, 0x02, 0xca, 0xfe]);

            let out_payload: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
            current_task.write_object(UserRef::new(data), &out_payload).unwrap();
            let ctrl_out = ctrl_transfer(USB_DIR_OUT, TEST_REQUEST, 0, 0, 4, 1000, data);
            assert_eq!(
                ioctl_with(current_task, &file, USBDEVFS_CONTROL, arg, &ctrl_out),
                Ok(4.into())
            );
            assert_eq!(*mock.last_ctrl_out.lock(), out_payload.to_vec());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_timeout_zero_means_no_timeout() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            for timeout in [0, 1000] {
                assert_eq!(
                    control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, timeout),
                    Ok(0.into())
                );
            }
            assert_eq!(
                *mock.ctrl_timeouts.lock(),
                vec![zx::MonotonicDuration::INFINITE.into_nanos(), 1_000_000_000]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_device_errors_map_to_errno() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            for (status, expected) in [
                (zx::sys::ZX_ERR_IO_REFUSED, error!(EPIPE)),
                (zx::sys::ZX_ERR_TIMED_OUT, error!(ETIMEDOUT)),
                (zx::sys::ZX_ERR_NOT_FOUND, error!(ENOENT)),
                (zx::sys::ZX_ERR_ALREADY_BOUND, error!(EBUSY)),
                (zx::sys::ZX_ERR_IO_NOT_PRESENT, error!(ENODEV)),
                (zx::sys::ZX_ERR_PEER_CLOSED, error!(ENODEV)),
                (zx::sys::ZX_ERR_INVALID_ARGS, error!(EINVAL)),
                (zx::sys::ZX_ERR_BAD_STATE, error!(EHOSTUNREACH)),
                (zx::sys::ZX_ERR_CANCELED, error!(ECONNRESET)),
                (zx::sys::ZX_ERR_NO_MEMORY, error!(ENOMEM)),
                (zx::sys::ZX_ERR_INTERNAL, error!(EIO)),
            ] {
                mock.ctrl_transfer_error.store(status, Ordering::Relaxed);
                assert_eq!(
                    control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 1000),
                    expected,
                    "status {status}"
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_wait_ignores_maskable_signals() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            current_task.thread_group.signal_actions.set(
                SIGUSR1,
                sigaction_t { sa_handler: uaddr { addr: 0x1000 }, ..sigaction_t::default() },
            );
            send_standard_signal(current_task, SignalInfo::kernel(SIGUSR1));
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 0),
                Ok(0.into()),
                "A maskable signal must not interrupt a control transfer"
            );
            assert_eq!(mock.ctrl_timeouts.lock().len(), 1);
            assert!(current_task.read().has_signal_pending(SIGUSR1));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_wait_is_killable() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            mock.hold_ctrl_transfers.store(true, Ordering::SeqCst);

            set_pending_sigkill(current_task, true);
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 0),
                error!(EINTR),
                "SIGKILL must end a control transfer without timeout"
            );
            set_pending_sigkill(current_task, false);

            // Complete the abandoned transfer so the kernel thread running it exits.
            mock.ctrl_held.wait_signaled();
            for responder in mock.held_ctrl_responders.lock().drain(..) {
                let _ = responder.send(Err(zx::sys::ZX_ERR_CANCELED));
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_waits_for_abandoned_control_transfer() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            mock.hold_ctrl_transfers.store(true, Ordering::SeqCst);

            set_pending_sigkill(current_task, true);
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 0),
                error!(EINTR)
            );
            mock.ctrl_held.wait_signaled();

            // The abandoned transfer is still running, so the next one waits for it instead of
            // starting another one, and that wait is killable too.
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 0),
                error!(EINTR)
            );
            assert_eq!(mock.ctrl_timeouts.lock().len(), 1);
            set_pending_sigkill(current_task, false);

            // Once the abandoned transfer completes, the next transfer runs.
            mock.hold_ctrl_transfers.store(false, Ordering::SeqCst);
            for responder in mock.held_ctrl_responders.lock().drain(..) {
                let _ = responder.send(Ok(&[]));
            }
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 0),
                Ok(0.into())
            );
            assert_eq!(mock.ctrl_timeouts.lock().len(), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_recipient_requires_interface_claim() {
        let config = build_config(
            1,
            &[
                alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]),
                TestAlt { interface: 1, alt: 0, class: CLASS_PRINTER, endpoints: vec![] },
            ],
        );
        run_with_mock_device(config, |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let set_interface_request = USB_REQ_SET_INTERFACE as u8;
            let clear_feature = USB_REQ_CLEAR_FEATURE as u8;
            assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));
            assert_eq!(claim(current_task, &file1, arg, 1), Ok(SUCCESS));

            assert_eq!(
                control(current_task, &file2, arg, INTERFACE_OUT, set_interface_request, 0, 1000),
                error!(EBUSY)
            );
            let ep1_in = u16::from(EP1_IN);
            assert_eq!(
                control(current_task, &file2, arg, ENDPOINT_OUT, clear_feature, ep1_in, 1000),
                error!(EBUSY)
            );
            // The endpoint number without its direction bit also resolves to the endpoint.
            let ep1_number = u16::from(EP1_IN & !(USB_DIR_IN as u8));
            assert_eq!(
                control(current_task, &file2, arg, ENDPOINT_OUT, clear_feature, ep1_number, 1000),
                error!(EBUSY)
            );
            assert_eq!(
                control(current_task, &file2, arg, INTERFACE_OUT, set_interface_request, 64, 1000),
                error!(EINVAL)
            );
            // Endpoint 0, device, and vendor requests need no claim.
            let ep0_in = USB_DIR_IN as u16;
            assert_eq!(
                control(current_task, &file2, arg, ENDPOINT_OUT, clear_feature, ep0_in, 1000),
                Ok(0.into())
            );
            assert_eq!(
                control(current_task, &file2, arg, USB_DIR_OUT, TEST_REQUEST, 0, 1000),
                Ok(0.into())
            );
            let vendor_interface = USB_DIR_OUT | USB_TYPE_VENDOR | USB_RECIP_INTERFACE;
            assert_eq!(
                control(current_task, &file2, arg, vendor_interface, TEST_REQUEST, 0, 1000),
                Ok(0.into())
            );
            // The printer class GET_DEVICE_ID request is allowed on a claimed interface.
            assert_eq!(
                control(current_task, &file2, arg, PRINTER_GET_DEVICE_ID, 0, 1 << 8, 1000),
                Ok(0.into())
            );
            // None of these requests took the interfaces away from the first FD.
            assert_eq!(release(current_task, &file1, arg, 1), Ok(SUCCESS));

            // An interface-recipient request auto-claims the unclaimed interface.
            assert_eq!(release(current_task, &file1, arg, 0), Ok(SUCCESS));
            assert_eq!(
                control(current_task, &file2, arg, INTERFACE_OUT, set_interface_request, 0, 1000),
                Ok(0.into())
            );
            assert_eq!(claim(current_task, &file1, arg, 0), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_control_interface_recipient_on_unconfigured_device() {
        let options =
            MockDeviceOptions { active_config: 0, ..MockDeviceOptions::new(hid_config()) };
        run_with_mock_device_options(options, |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let request = USB_REQ_SET_INTERFACE as u8;

            // The configuration is checked before the interface number.
            for index in [0, 99] {
                assert_eq!(
                    control(current_task, &file, arg, INTERFACE_OUT, request, index, 1000),
                    error!(EHOSTUNREACH),
                    "interface {index}"
                );
            }
            // Requests to the device are allowed.
            assert_eq!(
                control(current_task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 1000),
                Ok(0.into())
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_only_disables_endpoints_of_interface() {
        // Interface 0 alt 0 has EP1, alt 1 has no endpoints, interface 1 has EP2.
        let config = build_config(
            1,
            &[
                alt(0, 0, vec![(EP1_IN, XFER_INT, 64), (EP1_OUT, XFER_INT, 64)]),
                alt(0, 1, vec![]),
                alt(1, 0, vec![(EP2_IN, XFER_BULK, 64)]),
            ],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, EP2_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            // Switching interface 0 to alt 1 disables EP1 IN but leaves EP2 IN alone.
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), Ok(SUCCESS));
            assert_eq!(*mock.last_set_iface.lock(), Some((0, 1)));
            assert_eq!(*mock.enabled_endpoints.lock(), HashSet::from([EP2_IN]));

            // EP2 IN keeps working without being enabled again.
            assert_eq!(bulk(current_task, &file, arg, EP2_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(
                *mock.enable_log.lock(),
                vec![(EP2_IN, true, 64), (EP1_IN, true, 64), (EP1_IN, false, 64)]
            );

            // EP1 IN is not in the active alternate setting anymore.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 2, 1000, data), error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_reenables_endpoints_after_switching_back() {
        let config = build_config(
            1,
            &[
                alt(0, 0, vec![]),
                alt(0, 1, vec![(EP1_IN, XFER_BULK, 64)]),
                alt(0, 2, vec![(EP1_IN, XFER_BULK, 512)]),
                alt(1, 0, vec![(EP2_IN, XFER_BULK, 64)]),
            ],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, EP2_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), Ok(SUCCESS));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(set_interface(current_task, &file, arg, 0, 0), Ok(SUCCESS));
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), Ok(SUCCESS));
            // Fails if EP1 IN was not disabled when it left the active alternate setting, since
            // enabling an enabled endpoint fails.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            // EP1 IN changes descriptors in alt 2, so it is enabled again with them.
            assert_eq!(set_interface(current_task, &file, arg, 0, 2), Ok(SUCCESS));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            assert_eq!(
                *mock.enable_log.lock(),
                vec![
                    (EP2_IN, true, 64),
                    (EP1_IN, true, 64),
                    (EP1_IN, false, 64),
                    (EP1_IN, true, 64),
                    (EP1_IN, false, 64),
                    (EP1_IN, true, 512),
                ]
            );
            // The endpoint channel is reused across disable and enable.
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_invalid_alt_does_not_claim() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(set_interface(current_task, &file1, arg, 0, 0x100), error!(EINVAL));
            assert_eq!(set_interface(current_task, &file1, arg, 0, 7), error!(EINVAL));
            // The interface is not claimed by the first FD.
            assert_eq!(claim(current_task, &file2, arg, 0), Ok(SUCCESS));
            assert_eq!(set_interface(current_task, &file1, arg, 0, 0), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_errno_order() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(set_interface(current_task, &file, arg, 99, 0x100), error!(EINVAL));
            // A missing interface is reported before an invalid alternate setting.
            assert_eq!(set_interface(current_task, &file, arg, 5, 0x100), error!(ENOENT));
        })
        .await;

        let options =
            MockDeviceOptions { active_config: 0, ..MockDeviceOptions::new(hid_config()) };
        run_with_mock_device_options(options, |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            // The configuration is checked first.
            assert_eq!(set_interface(current_task, &file, arg, 99, 0x100), error!(EHOSTUNREACH));
            assert_eq!(set_interface(current_task, &file, arg, 0, 0), error!(EHOSTUNREACH));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_stall_on_single_alt_setting_succeeds() {
        let config =
            build_config(1, &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64), (EP2_OUT, XFER_BULK, 64)])]);
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            mock.set_interface_status.store(zx::sys::ZX_ERR_IO_REFUSED, Ordering::Relaxed);
            assert_eq!(set_interface(current_task, &file, arg, 0, 0), Ok(SUCCESS));

            // The halt of each endpoint of the interface is cleared instead.
            let clear_halt =
                |ep: u8| (ENDPOINT_OUT as u8, USB_REQ_CLEAR_FEATURE as u8, 0, u16::from(ep));
            assert_eq!(*mock.ctrl_setups.lock(), vec![clear_halt(EP2_OUT), clear_halt(EP1_IN)]);
            // The endpoints were disabled, and are enabled again on next use.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(
                *mock.enable_log.lock(),
                vec![(EP1_IN, true, 64), (EP1_IN, false, 64), (EP1_IN, true, 64)]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_stall_on_multiple_alt_settings_fails() {
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]), alt(0, 1, vec![(EP1_IN, XFER_BULK, 64)])],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            mock.set_interface_status.store(zx::sys::ZX_ERR_IO_REFUSED, Ordering::Relaxed);
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), error!(EPIPE));
            assert!(mock.ctrl_setups.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setinterface_failure_keeps_alt_setting() {
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]), alt(0, 1, vec![(EP2_IN, XFER_BULK, 64)])],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.set_interface_status.store(zx::sys::ZX_ERR_INTERNAL, Ordering::Relaxed);
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), error!(EIO));
            // Alternate setting 0 is still active.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(bulk(current_task, &file, arg, EP2_IN, 4, 1000, data), error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setconfiguration_busy_with_any_claim() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));
            // A claim by another FD or by the caller prevents the change.
            assert_eq!(set_configuration(current_task, &file2, arg, 1), error!(EBUSY));
            assert_eq!(set_configuration(current_task, &file1, arg, 1), error!(EBUSY));
            // The claims are checked before the configuration value.
            assert_eq!(set_configuration(current_task, &file1, arg, 0x100), error!(EBUSY));
            assert!(mock.set_config_log.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setconfiguration_disables_endpoints() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(release(current_task, &file, arg, 0), Ok(SUCCESS));

            assert_eq!(set_configuration(current_task, &file, arg, 1), Ok(SUCCESS));
            assert_eq!(*mock.set_config_log.lock(), vec![1]);
            assert!(mock.enabled_endpoints.lock().is_empty());
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(
                *mock.enable_log.lock(),
                vec![(EP1_IN, true, 64), (EP1_IN, false, 64), (EP1_IN, true, 64)]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setconfiguration_rejects_invalid_values() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            for value in [0x100, -2, 5] {
                assert_eq!(
                    set_configuration(current_task, &file, arg, value),
                    error!(EINVAL),
                    "configuration {value}"
                );
            }
            assert!(mock.set_config_log.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setconfiguration_unconfigure_rejected_by_driver() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(release(current_task, &file, arg, 0), Ok(SUCCESS));

            // -1 unconfigures the device, which the USB driver does not support.
            assert_eq!(set_configuration(current_task, &file, arg, -1), error!(EINVAL));
            assert_eq!(*mock.set_config_log.lock(), vec![0]);

            // The configuration is unchanged, and the disabled endpoint is enabled again.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(*mock.enabled_endpoints.lock(), HashSet::from([EP1_IN]));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setconfiguration_failure_keeps_configuration() {
        let mut config = hid_config();
        config.extend(build_config(2, &[alt(3, 0, vec![])]));
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.set_configuration_status.store(zx::sys::ZX_ERR_IO_REFUSED, Ordering::Relaxed);
            assert_eq!(set_configuration(current_task, &file, arg, 2), error!(EPIPE));
            // Configuration 1 is still active.
            assert_eq!(claim(current_task, &file, arg, 3), error!(ENOENT));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_unconfigured_device() {
        let options =
            MockDeviceOptions { active_config: 0, ..MockDeviceOptions::new(hid_config()) };
        run_with_mock_device_options(options, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(claim(current_task, &file, arg, 0), error!(ENOENT));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ESRCH));
            assert_eq!(clear_halt(current_task, &file, arg, u32::from(EP1_IN)), error!(ESRCH));

            assert_eq!(set_configuration(current_task, &file, arg, 1), Ok(SUCCESS));
            assert_eq!(*mock.set_config_log.lock(), vec![1]);
            assert_eq!(claim(current_task, &file, arg, 0), Ok(SUCCESS));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_rejects_invalid_endpoints() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(clear_halt(current_task, &file, arg, 0x10), error!(EINVAL));
            assert_eq!(clear_halt(current_task, &file, arg, 0x181), error!(EINVAL));
            assert_eq!(clear_halt(current_task, &file, arg, 0x85), error!(ENOENT));
            assert!(mock.ctrl_setups.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_skips_reset_endpoint_when_clear_feature_fails() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            mock.ctrl_transfer_error.store(zx::sys::ZX_ERR_IO_REFUSED, Ordering::Relaxed);
            assert_eq!(clear_halt(current_task, &file, arg, u32::from(EP1_IN)), error!(EPIPE));
            assert_eq!(mock.reset_endpoint_count.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_resets_halted_endpoint() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            assert_eq!(clear_halt(current_task, &file, arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(
                *mock.ctrl_setups.lock(),
                vec![(ENDPOINT_OUT as u8, USB_REQ_CLEAR_FEATURE as u8, 0, u16::from(EP1_IN))]
            );
            assert_eq!(mock.last_reset_endpoint.load(Ordering::Relaxed), EP1_IN);
            assert_eq!(mock.reset_endpoint_count.load(Ordering::Relaxed), 1);
            // The endpoint stays enabled.
            assert_eq!(*mock.enable_log.lock(), vec![(EP1_IN, true, 64)]);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_reenables_endpoint_that_is_not_halted() {
        for status in [zx::sys::ZX_ERR_INVALID_ARGS, zx::sys::ZX_ERR_NOT_SUPPORTED] {
            run_with_mock_device(hid_config(), move |current_task, _device, mock| {
                let file = open_device(current_task, DEV_NUM);
                let (arg, data) = map_test_buffers(current_task);
                assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

                // The host controller does not reset an endpoint that is not halted, so the
                // endpoint is disabled to reset the host data toggle when it is enabled again.
                mock.reset_endpoint_status.store(status, Ordering::Relaxed);
                assert_eq!(clear_halt(current_task, &file, arg, u32::from(EP1_IN)), Ok(SUCCESS));
                assert!(mock.enabled_endpoints.lock().is_empty());
                assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
                assert_eq!(
                    *mock.enable_log.lock(),
                    vec![(EP1_IN, true, 64), (EP1_IN, false, 64), (EP1_IN, true, 64)]
                );
                // The endpoint channel stays open.
                assert_eq!(mock.connect_count.load(Ordering::SeqCst), 1);
                assert!(!mock.endpoint_unbound.is_signaled());
            })
            .await;
        }
    }

    #[::fuchsia::test]
    async fn test_clear_halt_fails_on_reset_endpoint_error() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            mock.reset_endpoint_status.store(zx::sys::ZX_ERR_INTERNAL, Ordering::Relaxed);
            assert_eq!(clear_halt(current_task, &file, arg, u32::from(EP1_IN)), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_claims_interface() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(clear_halt(current_task, &file1, arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(clear_halt(current_task, &file2, arg, u32::from(EP1_IN)), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_endpoint_lookup_searches_all_alt_settings() {
        let config =
            build_config(1, &[alt(0, 0, vec![]), alt(0, 1, vec![(EP1_IN, XFER_BULK, 64)])]);
        run_with_mock_device(config, |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // EP1 IN belongs to inactive alt 1 of interface 0, which is claimed.
            assert_eq!(clear_halt(current_task, &file1, arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(claim(current_task, &file2, arg, 0), error!(EBUSY));
            assert_eq!(bulk(current_task, &file1, arg, EP1_IN, 4, 1000, data), error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_null_buffer_returns_efault() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            let ctrl = ctrl_transfer(USB_DIR_OUT, TEST_REQUEST, 0, 0, 4, 1000, UserAddress::NULL);
            assert_eq!(
                ioctl_with(current_task, &file, USBDEVFS_CONTROL, arg, &ctrl),
                error!(EFAULT)
            );
            assert_eq!(
                bulk(current_task, &file, arg, EP1_OUT, 4, 1000, UserAddress::NULL),
                error!(EFAULT)
            );
        })
        .await;
    }

    /// Issues `USBDEVFS_CONNECTINFO` on a buffer filled with a non-zero pattern.
    fn connect_info(current_task: &CurrentTask, file: &FileHandle) -> usbdevfs_connectinfo {
        let (arg, _) = map_test_buffers(current_task);
        let pattern = [0xffu8; std::mem::size_of::<usbdevfs_connectinfo>()];
        assert_eq!(
            ioctl_with(current_task, file, USBDEVFS_CONNECTINFO, arg, &pattern),
            Ok(SUCCESS)
        );
        current_task.read_object(UserRef::new(arg)).unwrap()
    }

    #[::fuchsia::test]
    async fn test_capabilities_and_connectinfo() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            // Only the capabilities implemented by asynchronous URBs are advertised.
            assert_eq!(
                ioctl_with(current_task, &file, USBDEVFS_GET_CAPABILITIES, arg, &0xffff_ffffu32),
                Ok(SUCCESS)
            );
            let caps: u32 = current_task.read_object(UserRef::new(arg)).unwrap();
            assert_eq!(
                caps,
                starnix_uapi::USBDEVFS_CAP_ZERO_PACKET
                    | starnix_uapi::USBDEVFS_CAP_BULK_CONTINUATION
                    | starnix_uapi::USBDEVFS_CAP_NO_PACKET_SIZE_LIM
                    | starnix_uapi::USBDEVFS_CAP_REAP_AFTER_DISCONNECT
            );

            let conn_info = connect_info(current_task, &file);
            assert_eq!(conn_info.devnum, u32::from(DEV_NUM));
            assert_eq!(conn_info.slow, 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_connectinfo_reports_low_speed_device_as_slow() {
        let options =
            MockDeviceOptions { speed: USB_SPEED_LOW, ..MockDeviceOptions::new(hid_config()) };
        run_with_mock_device_options(options, |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let conn_info = connect_info(current_task, &file);
            assert_eq!(conn_info.devnum, u32::from(DEV_NUM));
            assert_eq!(conn_info.slow, 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_getdriver_reports_usbfs_for_claimed_interfaces() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            let getdriver = |interface: u32| {
                let mut gd = usbdevfs_getdriver::new_zeroed();
                gd.interface = interface;
                gd.driver.fill(0x7f);
                ioctl_with(current_task, &file, USBDEVFS_GETDRIVER, arg, &gd)
            };

            assert_eq!(getdriver(0), error!(ENODATA));
            assert_eq!(getdriver(5), error!(ENODATA));
            assert_eq!(getdriver(99), error!(ENODATA));

            assert_eq!(claim(current_task, &file, data, 0), Ok(SUCCESS));
            assert_eq!(getdriver(0), Ok(SUCCESS));
            let gd: usbdevfs_getdriver = current_task.read_object(UserRef::new(arg)).unwrap();
            assert_eq!(gd.interface, 0);
            let name: Vec<u8> =
                gd.driver.iter().map(|&c| c as u8).take_while(|&c| c != 0).collect();
            assert_eq!(name, b"usbfs");
            assert!(gd.driver[name.len()..].iter().all(|&c| c == 0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_claim_rejects_missing_interfaces() {
        // Interface 3 only exists in inactive configuration 2.
        let mut config = hid_config();
        config.extend(build_config(2, &[alt(3, 0, vec![])]));
        run_with_mock_device(config, |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(disconnect_claim(current_task, &file, arg, 99, 0, b""), error!(EINVAL));
            assert_eq!(disconnect_claim(current_task, &file, arg, 3, 0, b""), error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_claim_claims_free_interface() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(disconnect_claim(current_task, &file1, arg, 0, 0, b""), Ok(SUCCESS));
            assert_eq!(disconnect_claim(current_task, &file1, arg, 0, 0, b""), Ok(SUCCESS));
            assert_eq!(claim(current_task, &file2, arg, 0), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_claim_driver_filters() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let if_driver = USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER;
            let except_driver = USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER;
            assert_eq!(claim(current_task, &file1, arg, 0), Ok(SUCCESS));

            // The interface is bound to the "usbfs" driver.
            assert_eq!(
                disconnect_claim(current_task, &file2, arg, 0, if_driver, b"usb-storage"),
                error!(EBUSY)
            );
            assert_eq!(
                disconnect_claim(current_task, &file2, arg, 0, except_driver, b"usbfs"),
                error!(EBUSY)
            );
            assert_eq!(
                disconnect_claim(current_task, &file2, arg, 0, if_driver, b"usbfs"),
                Ok(SUCCESS)
            );
            assert_eq!(
                disconnect_claim(current_task, &file1, arg, 0, except_driver, b"usb-storage"),
                Ok(SUCCESS)
            );
            assert_eq!(claim(current_task, &file2, arg, 0), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_claim_takes_interface_from_other_fd() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file1, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            assert_eq!(disconnect_claim(current_task, &file2, arg, 0, 0, b""), Ok(SUCCESS));
            // The requests of the previous claimant are cancelled.
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
            assert_eq!(release(current_task, &file1, arg, 0), error!(EINVAL));
            assert_eq!(claim(current_task, &file1, arg, 0), error!(EBUSY));
            assert_eq!(bulk(current_task, &file2, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_driver_ioctl_disconnect_and_connect() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(
                file1.ioctl(current_task, USBDEVFS_IOCTL, UserAddress::NULL.into()),
                error!(EFAULT)
            );
            assert_eq!(
                driver_ioctl(current_task, &file1, arg, 99, USBDEVFS_DISCONNECT),
                error!(EINVAL)
            );
            assert_eq!(
                driver_ioctl(current_task, &file1, arg, -1, USBDEVFS_DISCONNECT),
                error!(EINVAL)
            );
            assert_eq!(
                driver_ioctl(current_task, &file1, arg, 0, USBDEVFS_DISCONNECT),
                error!(ENODATA)
            );
            assert_eq!(driver_ioctl(current_task, &file1, arg, 0, USBDEVFS_CONNECT), Ok(SUCCESS));
            assert_eq!(driver_ioctl(current_task, &file1, arg, 0, 0x1234), error!(ENOTTY));

            // Claiming interface 0 on file1 makes CONNECT return EBUSY and lets file2 detach file1.
            assert_eq!(bulk(current_task, &file1, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(driver_ioctl(current_task, &file2, arg, 0, USBDEVFS_CONNECT), error!(EBUSY));
            assert_eq!(
                driver_ioctl(current_task, &file2, arg, 0, USBDEVFS_DISCONNECT),
                Ok(SUCCESS)
            );
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
            assert_eq!(release(current_task, &file1, arg, 0), error!(EINVAL));
            assert_eq!(claim(current_task, &file2, arg, 0), Ok(SUCCESS));
            assert_eq!(release(current_task, &file2, arg, 0), Ok(SUCCESS));
            assert_eq!(driver_ioctl(current_task, &file2, arg, 0, USBDEVFS_CONNECT), Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reset_preserves_configuration_and_claims() {
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]), alt(0, 1, vec![(EP2_IN, XFER_BULK, 64)])],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(set_interface(current_task, &file1, arg, 0, 1), Ok(SUCCESS));

            assert_eq!(ioctl_no_arg(current_task, &file1, USBDEVFS_RESET), Ok(SUCCESS));
            assert!(mock.set_config_log.lock().is_empty());
            assert_eq!(claim(current_task, &file2, arg, 0), error!(EBUSY));
            // Alternate setting 1 is still active.
            assert_eq!(bulk(current_task, &file1, arg, EP2_IN, 4, 1000, data), Ok(4.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reset_cancels_endpoint_requests() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            assert_eq!(ioctl_no_arg(current_task, &file, USBDEVFS_RESET), Ok(SUCCESS));
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reset_stray_completion_is_not_mistaken_for_next_one() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            // A stray completion sent while no request is in flight must not be mistaken for
            // the completion of a later request.
            mock.send_stale_completion_on_cancel.store(true, Ordering::SeqCst);
            assert_eq!(ioctl_no_arg(current_task, &file, USBDEVFS_RESET), Ok(SUCCESS));
            mock.bulk_in_scripted_packets.lock().push_back((vec![1, 2, 3, 4], zx::sys::ZX_OK));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), [1, 2, 3, 4]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_rejects_invalid_endpoints() {
        let config = build_config(
            1,
            &[alt(
                0,
                0,
                vec![(EP1_IN, XFER_ISO, 64), (EP2_IN, XFER_BULK, 0), (EP3_IN, XFER_BULK, 64)],
            )],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, 0x10, 0, 1000, data), error!(EINVAL));
            assert_eq!(bulk_raw(current_task, &file, arg, 0x101, 0, 1000, data), error!(EINVAL));
            assert_eq!(bulk(current_task, &file, arg, 0x85, 0, 1000, data), error!(ENOENT));
            // Isochronous endpoints and endpoints with a zero wMaxPacketSize are rejected.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(EINVAL));
            assert_eq!(bulk(current_task, &file, arg, EP2_IN, 4, 1000, data), error!(EINVAL));
            assert!(mock.request_types.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_length_limits() {
        let config =
            build_config(1, &[alt(0, 0, vec![(EP1_IN, XFER_ISO, 64), (EP3_IN, XFER_BULK, 64)])]);
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            let too_long = (MAX_USBDEVFS_BULK_SIZE + 1) as u32;
            let invalid = INVALID_USBDEVFS_BULK_SIZE as u32;

            // usbfs limits the memory used by a transfer, including its URB.
            assert_eq!(
                bulk(current_task, &file, arg, EP3_IN, too_long, 1000, data),
                error!(ENOMEM)
            );
            // Very large lengths are invalid, which is checked before the endpoint.
            assert_eq!(bulk(current_task, &file, arg, EP3_IN, invalid, 1000, data), error!(EINVAL));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, invalid, 1000, data), error!(EINVAL));
            // The endpoint is checked before the memory limit.
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, too_long, 1000, data),
                error!(EINVAL)
            );
            assert!(mock.request_types.lock().is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_uses_endpoint_transfer_type() {
        let config =
            build_config(1, &[alt(0, 0, vec![(EP1_IN, XFER_INT, 64), (EP2_OUT, XFER_BULK, 512)])]);
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(bulk(current_task, &file, arg, EP2_OUT, 4, 1000, data), Ok(4.into()));
            assert_eq!(*mock.request_types.lock(), vec![(EP1_IN, "interrupt"), (EP2_OUT, "bulk")]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_enable_endpoint_passes_ss_companion() {
        let mut config = build_config(1, &[alt(0, 0, vec![])]);
        config.extend(endpoint_descriptor(EP1_IN, XFER_BULK, 1024));
        config.extend([USB_DT_SS_EP_COMP_SIZE as u8, USB_DT_SS_ENDPOINT_COMP as u8, 3, 0, 0, 0]);
        config[4 + USB_DT_CONFIG_SIZE as usize] = 1; // bNumEndpoints
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(*mock.ss_companion_log.lock(), vec![(EP1_IN, 3)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_out_claims_interface() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            let out_payload = [0x11u8, 0x22, 0x33, 0x44, 0x55];
            current_task.write_memory(data, &out_payload).unwrap();
            assert_eq!(bulk(current_task, &file1, arg, EP1_OUT, 5, 0, data), Ok(5.into()));
            assert_eq!(bulk(current_task, &file2, arg, EP1_OUT, 5, 0, data), error!(EBUSY));
            assert_eq!(*mock.received_bulk_out.lock(), vec![out_payload.to_vec()]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_zero_length_transfers_queue_zero_length_requests() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            assert_eq!(
                bulk(current_task, &file, arg, EP1_OUT, 0, 1000, UserAddress::NULL),
                Ok(0.into())
            );
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 0, 1000, UserAddress::NULL),
                Ok(0.into())
            );
            assert_eq!(*mock.request_sizes.lock(), vec![(EP1_OUT, 0), (EP1_IN, 0)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_out_short_write_returns_partial_count() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // If the device accepts only 3 of 5 bytes, 3 is returned.
            mock.bulk_out_max_transfer_size.store(3, Ordering::Relaxed);
            assert_eq!(bulk(current_task, &file, arg, EP1_OUT, 5, 1000, data), Ok(3.into()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_in_returns_eoverflow_when_device_sends_too_much() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            current_task.write_memory(data, &[0u8; 6]).unwrap();

            mock.overreport_in_transfer_size.store(true, Ordering::SeqCst);
            mock.bulk_in_scripted_packets.lock().push_back((vec![1, 2, 3, 4, 5, 6], 0));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 2, 1000, data), error!(EOVERFLOW));
            assert_eq!(read_bytes::<6>(current_task, data), [0; 6]);

            // Nothing is kept for the next transfer.
            mock.overreport_in_transfer_size.store(false, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_release_cancels_endpoint_requests() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            assert_eq!(release(current_task, &file, arg, 0), Ok(SUCCESS));
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_release_resets_nonzero_alternate_setting_to_zero() {
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]), alt(0, 1, vec![(EP2_IN, XFER_BULK, 64)])],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            assert_eq!(set_interface(current_task, &file, arg, 0, 1), Ok(SUCCESS));
            assert_eq!(*mock.last_set_iface.lock(), Some((0, 1)));

            assert_eq!(release(current_task, &file, arg, 0), Ok(SUCCESS));
            assert_eq!(*mock.last_set_iface.lock(), Some((0, 0)));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_close_cancels_endpoint_requests() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let (arg, data) = map_test_buffers(current_task);
            let file = open_device(current_task, DEV_NUM);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            drop(file);
            current_task.kernel().delayed_releaser.apply(current_task);
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    /// Runs `f` on a kernel thread once `count` IN requests have been queued to the mock, and
    /// returns a receiver for its result.
    fn run_when_in_requests_queued<R: Send + 'static>(
        current_task: &CurrentTask,
        mock: &Arc<MockUsbServerState>,
        count: usize,
        f: impl FnOnce(&CurrentTask) -> R + Send + 'static,
    ) -> std::sync::mpsc::Receiver<R> {
        let mock = Arc::clone(mock);
        let (sender, receiver) = std::sync::mpsc::channel();
        let closure = move |task: &CurrentTask| {
            wait_until("IN requests queued", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) >= count
            });
            let _ = sender.send(f(task));
        };
        let request = SpawnRequestBuilder::new()
            .with_debug_name("usbfs-test")
            .with_sync_closure(closure)
            .build();
        current_task.kernel().kthreads.spawner().spawn_from_request(request);
        receiver
    }

    #[::fuchsia::test]
    async fn test_reset_from_other_fd_shuts_down_in_flight_bulk() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // Another file descriptor resets the device while a transfer without timeout waits
            // for its request.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let reset = run_when_in_requests_queued(current_task, mock, 1, move |task| {
                ioctl_no_arg(task, &file2, USBDEVFS_RESET)
            });
            assert_eq!(bulk(current_task, &file1, arg, EP1_IN, 4, 0, data), error!(ESHUTDOWN));
            assert_eq!(reset.recv().unwrap(), Ok(SUCCESS));

            // The cancelled request was drained: the next transfer gets its own completion on
            // the same VMO.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file1, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(mock.registered_vmos.lock().len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_fails_in_flight_bulk_with_enodev() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let device = Arc::clone(device);
            let disconnected =
                run_when_in_requests_queued(current_task, mock, 1, move |_| device.on_disconnect());
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 0, data), error!(ENODEV));
            disconnected.recv().unwrap();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_timeout_drains_late_cancelled_completion() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // The request never completes, and its cancelled completion arrives after the
            // CancelAll reply.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(ETIMEDOUT));
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);

            // The next transfer gets its own completion, not the cancelled one.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 1);
            assert_eq!(mock.registered_vmos.lock().len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_timeout_returns_data_completed_before_cancel() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // The request completes successfully as it is cancelled after the timeout.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            *mock.cancel_behavior.lock() = Some(CancelBehavior::CompleteSuccessfullyAfterReply);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_timeout_retires_vmo_when_cancel_does_not_complete() {
        let options = MockDeviceOptions {
            cancel_drain_timeout: zx::MonotonicDuration::from_millis(50),
            ..MockDeviceOptions::new(hid_config())
        };
        run_with_mock_device_options(options, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.hold_in_requests.store(true, Ordering::SeqCst);
            *mock.cancel_behavior.lock() = Some(CancelBehavior::Drop);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(ETIMEDOUT));

            // The next transfer uses a new VMO on the same channel, without disabling the
            // endpoint.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(*mock.registered_vmos.lock(), vec![(EP1_IN, 1), (EP1_IN, 2)]);
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 1);
            assert_eq!(*mock.enable_log.lock(), vec![(EP1_IN, true, 64)]);
            assert!(!mock.endpoint_unbound.is_signaled());

            // The retired request eventually completes successfully. Its completion is not
            // mistaken for the one of the next transfer, and the retired VMO is unregistered.
            mock.complete_held_on_next_queue.store(true, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(*mock.unregistered_vmos.lock(), vec![(EP1_IN, 1)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_fails_when_too_many_vmos_are_retired() {
        let options = MockDeviceOptions {
            cancel_drain_timeout: zx::MonotonicDuration::from_millis(50),
            ..MockDeviceOptions::new(hid_config())
        };
        run_with_mock_device_options(options, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.hold_in_requests.store(true, Ordering::SeqCst);
            *mock.cancel_behavior.lock() = Some(CancelBehavior::Drop);
            for _ in 0..=MAX_RETIRED_VMOS {
                assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(ETIMEDOUT));
            }
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(EIO));
            assert_eq!(mock.registered_vmos.lock().len(), MAX_RETIRED_VMOS + 1);
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), MAX_RETIRED_VMOS + 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_wait_ignores_maskable_signals() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            current_task.thread_group.signal_actions.set(
                SIGUSR1,
                sigaction_t { sa_handler: uaddr { addr: 0x1000 }, ..sigaction_t::default() },
            );
            send_standard_signal(current_task, SignalInfo::kernel(SIGUSR1));
            // A pending maskable signal neither interrupts a transfer that completes nor one that
            // times out.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 0, data), Ok(4.into()));
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(ETIMEDOUT));
            assert!(current_task.read().has_signal_pending(SIGUSR1));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_wait_is_killable() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.hold_in_requests.store(true, Ordering::SeqCst);
            set_pending_sigkill(current_task, true);
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 4, 0, data),
                error!(EINTR),
                "SIGKILL must end a bulk transfer without timeout"
            );
            set_pending_sigkill(current_task, false);
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);

            // The cancelled request was drained: the next transfer gets its own completion on
            // the same VMO.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 1);
            assert_eq!(mock.registered_vmos.lock().len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_without_usb_connection_returns_enodev() {
        run_with_unconnected_device(hid_config(), |current_task| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);

            // ENODEV is returned before reading user memory.
            let unmapped = UserAddress::from(0x1000u64);
            assert_eq!(bulk(current_task, &file, arg, EP1_OUT, 64, 1000, unmapped), error!(ENODEV));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_cancels_endpoint_transfers() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));

            device.on_disconnect();
            assert_eq!(mock.cancel_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_endpoint_closed_before_register_vmos_disables_endpoint() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.close_endpoint_before_register.store(true, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENODEV));
            assert_eq!(*mock.enable_log.lock(), vec![(EP1_IN, true, 64), (EP1_IN, false, 64)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_endpoint_connect_failure_disables_endpoint() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // The endpoint is disabled again, so a later attempt can enable it.
            mock.connect_error_once.store(zx::sys::ZX_ERR_NOT_FOUND, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENOENT));
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(
                *mock.enable_log.lock(),
                vec![(EP1_IN, true, 64), (EP1_IN, false, 64), (EP1_IN, true, 64)]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_reconnects_after_driver_closes_endpoint() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.drop_endpoint_on_next_in.store(true, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENODEV));

            // The driver disabled the endpoint before closing the channel, so it is enabled and
            // connected again.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 2);
            assert_eq!(*mock.enable_log.lock(), vec![(EP1_IN, true, 64), (EP1_IN, true, 64)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_out_chunks_transfers_larger_than_vmo() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let large_payload: Vec<u8> = (0..70_000usize).map(|i| (i % 251) as u8).collect();
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);
            current_task.write_memory(buf_addr, &large_payload).unwrap();

            assert_eq!(
                bulk(current_task, &file, arg, EP1_OUT, large_payload.len() as u32, 2000, buf_addr),
                Ok(large_payload.len().into())
            );
            assert_eq!(mock.received_bulk_out.lock().concat(), large_payload);
            assert_eq!(*mock.request_sizes.lock(), vec![(EP1_OUT, 65_536), (EP1_OUT, 4_464)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_in_chunks_transfers_larger_than_vmo() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);

            // 70,000 bytes are received as a 65,536-byte chunk and a 4,464-byte chunk.
            let expected_in: Vec<u8> = (0..70_000usize).map(|i| (i % 239) as u8).collect();
            {
                let mut scripted = mock.bulk_in_scripted_packets.lock();
                scripted.push_back((expected_in[..65_536].to_vec(), zx::sys::ZX_OK));
                scripted.push_back((expected_in[65_536..].to_vec(), zx::sys::ZX_OK));
            }
            let len = expected_in.len() as u32;
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, len, 2000, buf_addr),
                Ok(expected_in.len().into())
            );
            let actual_in = current_task.read_memory_to_vec(buf_addr, expected_in.len()).unwrap();
            assert_eq!(actual_in, expected_in);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_in_error_in_second_chunk_discards_first_chunk() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);

            current_task.write_memory(buf_addr, &[0u8; 16]).unwrap();
            {
                let mut scripted = mock.bulk_in_scripted_packets.lock();
                scripted.push_back((vec![0x5au8; 65_536], zx::sys::ZX_OK));
                scripted.push_back((Vec::new(), zx::sys::ZX_ERR_IO_REFUSED));
            }
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 70_000, 2000, buf_addr),
                error!(EPIPE)
            );
            let untouched = current_task.read_memory_to_vec(buf_addr, 16).unwrap();
            assert_eq!(untouched, vec![0u8; 16]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_in_short_chunk_ends_transfer() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);

            mock.bulk_in_scripted_packets.lock().push_back((vec![0x11; 100], zx::sys::ZX_OK));
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 70_000, 2000, buf_addr),
                Ok(100.into())
            );
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bulk_chunks_are_multiples_of_max_packet_size() {
        // Interrupt endpoints can have a wMaxPacketSize that does not divide the VMO size.
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_OUT, XFER_INT, 1000), (EP1_IN, XFER_INT, 1000)])],
        );
        run_with_mock_device(config, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);

            assert_eq!(
                bulk(current_task, &file, arg, EP1_OUT, 70_000, 2000, buf_addr),
                Ok(70_000.into())
            );
            {
                let mut scripted = mock.bulk_in_scripted_packets.lock();
                scripted.push_back((vec![1; 65_000], zx::sys::ZX_OK));
                scripted.push_back((vec![2; 5_000], zx::sys::ZX_OK));
            }
            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 70_000, 2000, buf_addr),
                Ok(70_000.into())
            );
            assert_eq!(
                *mock.request_sizes.lock(),
                vec![(EP1_OUT, 65_000), (EP1_OUT, 5_000), (EP1_IN, 65_000), (EP1_IN, 5_000)]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reset_between_bulk_chunks_returns_eshutdown() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, _) = map_test_buffers(current_task);
            let buf_addr = map_test_buffer(current_task, 73_728);

            // Hold the first 65,536-byte chunk until the endpoint is reset, then complete the
            // first chunk successfully as it is cancelled. The transfer must fail with ESHUTDOWN
            // before the second chunk is submitted.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            *mock.cancel_behavior.lock() = Some(CancelBehavior::CompleteSuccessfullyAfterReply);
            mock.bulk_in_scripted_packets.lock().push_back((vec![0x42u8; 65_536], zx::sys::ZX_OK));
            let device_for_reset = Arc::clone(device);
            let reset = run_when_in_requests_queued(current_task, mock, 1, move |_| {
                device_for_reset.reset_endpoint_state(EP1_IN);
            });

            assert_eq!(
                bulk(current_task, &file, arg, EP1_IN, 70_000, 0, buf_addr),
                error!(ESHUTDOWN)
            );
            reset.recv().unwrap();
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_cancel_all_domain_error_retires_vmo_without_waiting_for_drain_timeout() {
        let options = MockDeviceOptions {
            // Infinite drain timeout so that ignoring an inner CancelAll error would block
            // forever instead of returning immediately.
            cancel_drain_timeout: zx::MonotonicDuration::INFINITE,
            ..MockDeviceOptions::new(hid_config())
        };
        run_with_mock_device_options(options, |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            mock.hold_in_requests.store(true, Ordering::SeqCst);
            mock.cancel_all_status.store(zx::sys::ZX_ERR_IO, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1, data), error!(ETIMEDOUT));

            // Because CancelAll failed, the next transfer retires VMO 1 and registers VMO 2.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            mock.cancel_all_status.store(zx::sys::ZX_OK, Ordering::SeqCst);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(*mock.registered_vmos.lock(), vec![(EP1_IN, 1), (EP1_IN, 2)]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_during_endpoint_connect_returns_enodev_and_leaves_no_endpoint() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            let device_for_hook = Arc::clone(device);
            *mock.on_connect_to_endpoint.lock() = Some(Box::new(move || {
                device_for_hook.on_disconnect();
            }));

            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENODEV));
            assert!(
                mock.enabled_endpoints.lock().is_empty(),
                "endpoint enabled concurrently with on_disconnect must be rolled back"
            );
        })
        .await;
    }

    fn spawn_kernel_thread<R: Send + Sync + 'static>(
        current_task: &CurrentTask,
        f: impl FnOnce(&CurrentTask) -> R + Send + Sync + 'static,
    ) -> std::sync::mpsc::Receiver<R> {
        let task_builder = current_task
            .clone_task(
                u64::from(
                    starnix_uapi::CLONE_VM
                        | starnix_uapi::CLONE_FS
                        | starnix_uapi::CLONE_FILES
                        | starnix_uapi::CLONE_THREAD
                        | starnix_uapi::CLONE_SIGHAND,
                ),
                None,
                UserRef::default(),
                UserRef::default(),
                UserRef::default(),
            )
            .expect("failed to clone task in test");
        let (sender, receiver) = std::sync::mpsc::channel();
        starnix_core::execution::execute_task(
            task_builder,
            move |worker_task| {
                let result = f(worker_task);
                worker_task.write().set_exiting(starnix_core::task::ExitStatus::Exit(0));
                let _ = sender.send(result);
                error!(EHWPOISON)
            },
            |_| {},
        )
        .expect("failed to execute worker task");
        receiver
    }

    #[::fuchsia::test]
    async fn test_reset_before_begin_transfer_returns_eshutdown() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);

            // Thread 1 holds the endpoint transfer slot while waiting on an in-flight request.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file1_first = Arc::clone(&file1);
            let first = spawn_kernel_thread(current_task, move |task| {
                let (arg1, data1) = map_test_buffers(task);
                bulk(task, &file1_first, arg1, EP1_IN, 4, 0, data1)
            });
            wait_until("the first IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });

            // Thread 2 blocks waiting for the endpoint transfer slot in `acquire_transfer`; an
            // endpoint reset between `acquire_transfer` and `begin_transfer` is caught by
            // `begin_transfer` and returns `ESHUTDOWN`.
            let device_for_hook = Arc::clone(device);
            device.set_on_before_begin_transfer(
                EP1_IN,
                Box::new(move || {
                    device_for_hook.reset_endpoint_state(EP1_IN);
                }),
            );
            let file1_second = Arc::clone(&file1);
            let second = spawn_kernel_thread(current_task, move |task| {
                let (arg2, data2) = map_test_buffers(task);
                bulk(task, &file1_second, arg2, EP1_IN, 4, 0, data2)
            });
            wait_until("second transfer waiting for slot", || {
                device.endpoint_transfer_waiters(EP1_IN) == 1
            });

            // Resetting the device aborts the in-flight Thread 1 transfer with `ESHUTDOWN`, and
            // Thread 2 wakes up, hits the reset before `begin_transfer`, and also returns
            // `ESHUTDOWN`.
            assert_eq!(ioctl_no_arg(current_task, &file2, USBDEVFS_RESET), Ok(SUCCESS));
            assert_eq!(first.recv().unwrap(), error!(ESHUTDOWN));
            assert_eq!(second.recv().unwrap(), error!(ESHUTDOWN));
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_compat32_urb_struct_layout() {
        assert_eq!(size_of::<UsbdevfsUrbHeader>(), 56);
        assert_eq!(align_of::<UsbdevfsUrbHeader>(), 8);

        assert_eq!(size_of::<UsbdevfsUrb32Header>(), 44);
        assert_eq!(align_of::<UsbdevfsUrb32Header>(), 4);
    }

    /// Size of the setup packet at the start of the buffer of a control URB.
    const SETUP_PACKET_SIZE: usize = size_of::<usb_ctrlrequest>();

    /// Offset of the `USBDEVFS_REAPURB` argument from the URB in the buffers of
    /// `map_urb_buffers`.
    const URB_REAP_ARG_OFFSET: usize = 128;

    /// Configuration with bulk endpoints `EP1_IN` and `EP2_OUT` and interrupt endpoint `EP3_IN`
    /// on interface 0, with a `wMaxPacketSize` of `URB_TEST_MAX_PACKET_SIZE`.
    fn urb_test_config() -> Vec<u8> {
        let max_packet_size = URB_TEST_MAX_PACKET_SIZE as u16;
        build_config(
            1,
            &[alt(
                0,
                0,
                vec![
                    (EP1_IN, XFER_BULK, max_packet_size),
                    (EP2_OUT, XFER_BULK, max_packet_size),
                    (EP3_IN, XFER_INT, max_packet_size),
                ],
            )],
        )
    }

    const URB_TEST_MAX_PACKET_SIZE: usize = 64;

    /// Addresses of a URB, of a `USBDEVFS_REAPURB` argument, and of a data buffer, in a page.
    struct UrbBuffers {
        urb: UserAddress,
        reap_arg: UserAddress,
        data: UserAddress,
    }

    fn urb_buffers_at(page: UserAddress) -> UrbBuffers {
        UrbBuffers {
            urb: page,
            reap_arg: (page + URB_REAP_ARG_OFFSET).unwrap(),
            data: (page + TEST_DATA_OFFSET).unwrap(),
        }
    }

    fn map_urb_buffers(current_task: &CurrentTask) -> UrbBuffers {
        urb_buffers_at(map_test_page(current_task))
    }

    /// Returns a URB of type `urb_type` on `endpoint`, with a buffer of `length` bytes at
    /// `buffer`.
    fn urb_header(
        urb_type: u32,
        endpoint: u8,
        buffer: UserAddress,
        length: usize,
    ) -> UsbdevfsUrbHeader {
        UsbdevfsUrbHeader {
            type_: urb_type as u8,
            endpoint,
            buffer: buffer.ptr() as u64,
            buffer_length: length as i32,
            ..Default::default()
        }
    }

    /// Writes `urb` at `addr` and submits it.
    fn submit_urb(
        current_task: &CurrentTask,
        file: &FileHandle,
        addr: UserAddress,
        urb: &UsbdevfsUrbHeader,
    ) -> Result<SyscallResult, Errno> {
        ioctl_with(current_task, file, USBDEVFS_SUBMITURB, addr, urb)
    }

    /// Returns a setup packet for a `TEST_REQUEST` request of type `request_type` with a data
    /// stage of `length` bytes.
    fn test_setup_packet(request_type: u32, value: u16, length: u16) -> usb_ctrlrequest {
        usb_ctrlrequest {
            bRequestType: request_type as u8,
            bRequest: TEST_REQUEST,
            wValue: value.to_le(),
            wIndex: 0,
            wLength: length.to_le(),
        }
    }

    /// Writes `setup` to `buffers.data` and submits a control URB at `buffers.urb` with a
    /// buffer of `buffer_length` bytes.
    fn submit_control_urb(
        current_task: &CurrentTask,
        file: &FileHandle,
        buffers: &UrbBuffers,
        setup: usb_ctrlrequest,
        buffer_length: usize,
    ) -> Result<SyscallResult, Errno> {
        current_task.write_object(UserRef::new(buffers.data), &setup).unwrap();
        let urb = urb_header(USBDEVFS_URB_TYPE_CONTROL, 0, buffers.data, buffer_length);
        submit_urb(current_task, file, buffers.urb, &urb)
    }

    /// Submits a control IN URB for `TEST_REQUEST` with a 3-byte data stage.
    fn submit_test_control_urb(
        current_task: &CurrentTask,
        file: &FileHandle,
        buffers: &UrbBuffers,
        value: u16,
    ) -> Result<SyscallResult, Errno> {
        let setup = test_setup_packet(USB_DIR_IN | USB_TYPE_VENDOR, value, 3);
        submit_control_urb(current_task, file, buffers, setup, SETUP_PACKET_SIZE + 3)
    }

    /// Reaps a URB with `USBDEVFS_REAPURB`, or `USBDEVFS_REAPURBNDELAY` unless `blocking`, and
    /// returns its address.
    fn reap_urb(
        current_task: &CurrentTask,
        file: &FileHandle,
        reap_arg: UserAddress,
        blocking: bool,
    ) -> Result<UserAddress, Errno> {
        let request = if blocking { USBDEVFS_REAPURB } else { USBDEVFS_REAPURBNDELAY };
        file.ops().ioctl(file, current_task, request, reap_arg.into())?;
        let addr: u64 = current_task.read_object(UserRef::new(reap_arg)).unwrap();
        Ok(UserAddress::from(addr))
    }

    fn discard_urb(
        current_task: &CurrentTask,
        file: &FileHandle,
        addr: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        file.ops().ioctl(file, current_task, USBDEVFS_DISCARDURB, addr.into())
    }

    fn read_urb(current_task: &CurrentTask, addr: UserAddress) -> UsbdevfsUrbHeader {
        current_task.read_object(UserRef::new(addr)).unwrap()
    }

    /// Returns the URB status reporting `errno`.
    fn status_of(errno: Errno) -> i32 {
        -(errno.code.error_code() as i32)
    }

    /// Polls `condition` until it holds, failing the test after 10 seconds.
    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(10));
        while !condition() {
            assert!(zx::MonotonicInstant::get() < deadline, "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Asynchronous version of `wait_until`, for tests running on an executor.
    async fn wait_until_async(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(10));
        while !condition() {
            assert!(zx::MonotonicInstant::get() < deadline, "timed out waiting for {what}");
            fuchsia_async::Timer::new(std::time::Duration::from_millis(5)).await;
        }
    }

    // Only architectures with 32-bit support have compat request numbers distinct from the native
    // ones; on x86_64 `uapi::arch32` aliases the native definitions.
    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_compat32_urb_ioctls() {
        run_with_mock_device(hid_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            // 32-bit URBs hold 32-bit pointers, so map the buffers below 4 GiB.
            let page = do_mmap(
                current_task,
                UserAddress::from(0x3000_0000u64),
                *PAGE_SIZE as usize,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS | MAP_PRIVATE,
                FdNumber::from_raw(-1),
                0,
            )
            .unwrap();
            let buffers = urb_buffers_at(page);
            let reap =
                |request| file.ops().ioctl(&file, current_task, request, buffers.reap_arg.into());

            assert_eq!(reap(arch32::USBDEVFS_REAPURBNDELAY), error!(EAGAIN));

            let setup = test_setup_packet(USB_DIR_IN | USB_TYPE_VENDOR, 0, 3);
            current_task.write_object(UserRef::new(buffers.data), &setup).unwrap();
            let urb32 = UsbdevfsUrb32Header {
                type_: USBDEVFS_URB_TYPE_CONTROL as u8,
                buffer: buffers.data.ptr() as u32,
                buffer_length: (SETUP_PACKET_SIZE + 3) as i32,
                usercontext: 0x1234,
                ..Default::default()
            };
            assert_eq!(
                ioctl_with(current_task, &file, arch32::USBDEVFS_SUBMITURB, buffers.urb, &urb32),
                Ok(SUCCESS)
            );

            assert_eq!(reap(arch32::USBDEVFS_REAPURB), Ok(SUCCESS));
            let reaped: u32 = current_task.read_object(UserRef::new(buffers.reap_arg)).unwrap();
            assert_eq!(reaped, buffers.urb.ptr() as u32);
            let urb32: UsbdevfsUrb32Header =
                current_task.read_object(UserRef::new(buffers.urb)).unwrap();
            assert_eq!(urb32.status, 0);
            assert_eq!(urb32.actual_length, 3);
            assert_eq!(urb32.usercontext, 0x1234);
            let data_addr = (buffers.data + SETUP_PACKET_SIZE).unwrap();
            assert_eq!(read_bytes::<3>(current_task, data_addr), [TEST_REQUEST, 0x00, 0xca]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_poll_events_track_completed_urbs() {
        run_with_mock_device(hid_config(), |current_task, device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let path = format!("bus/usb/001/{:03}", DEV_NUM);
            let ro_file =
                open_dev_node(current_task, path.as_bytes().into(), OpenFlags::RDONLY).unwrap();
            let buffers = map_urb_buffers(current_task);
            let events = |file: &FileHandle| file.ops().query_events(file, current_task).unwrap();
            let completed_events = FdEvents::POLLOUT | FdEvents::POLLWRNORM;
            let disconnected_events = FdEvents::POLLHUP | FdEvents::POLLERR;

            assert_eq!(events(&file), FdEvents::empty());
            assert_eq!(events(&ro_file), FdEvents::empty());

            // POLLOUT is reported while a completed URB waits to be reaped.
            assert_eq!(submit_test_control_urb(current_task, &file, &buffers, 1), Ok(SUCCESS));
            wait_until("a completed URB", || events(&file) == completed_events);
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(events(&file), FdEvents::empty());

            // Completed URBs can still be reaped after the device is disconnected.
            assert_eq!(submit_test_control_urb(current_task, &file, &buffers, 2), Ok(SUCCESS));
            wait_until("a completed URB", || events(&file) == completed_events);
            device.on_disconnect();
            assert_eq!(events(&file), completed_events | disconnected_events);
            assert_eq!(events(&ro_file), disconnected_events);
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(events(&file), disconnected_events);
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), error!(ENODEV));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), error!(ENODEV));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_blocking_reap_returns_urbs_pending_at_disconnect() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            // The blocking reap is woken up by the disconnection, and returns the URB, which
            // completes with -ESHUTDOWN.
            let device_to_disconnect = Arc::clone(device);
            let disconnect = std::thread::spawn(move || {
                wait_until("blocking reap waiter registered", || {
                    device_to_disconnect.has_reap_waiter()
                });
                device_to_disconnect.on_disconnect();
            });
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            disconnect.join().unwrap();
            let urb = read_urb(current_task, buffers.urb);
            assert_eq!(urb.status, status_of(errno!(ESHUTDOWN)));
            assert_eq!(urb.actual_length, 0);

            // Once all the URBs are reaped, reaping fails.
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), error!(ENODEV));
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_close_kills_pending_urbs() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);
            // The first URB is running, the second one waits for it.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);
            assert!(device.memory_used() > 0);

            // Closing the file cancels the running request, and the URBs release their memory.
            drop(file);
            current_task.kernel().delayed_releaser.apply(current_task);
            wait_until("the cancellation", || mock.cancel_count.load(Ordering::SeqCst) > 0);
            wait_until("the release of the URB memory", || device.memory_used() == 0);
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 1);

            // The interface was released, and the endpoint can be used again.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multi_fd_teardown_isolation() {
        run_with_mock_device(hid_config(), |current_task, device, _mock| {
            let file1 = open_device(current_task, DEV_NUM);
            let file2 = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            assert_eq!(claim(current_task, &file1, buffers.urb, 0), Ok(SUCCESS));

            // Close file2 while one of its URBs waits to be reaped.
            assert_eq!(submit_test_control_urb(current_task, &file2, &buffers, 1), Ok(SUCCESS));
            wait_until("a completed URB", || {
                file2.ops().query_events(&file2, current_task).unwrap().contains(FdEvents::POLLOUT)
            });
            drop(file2);
            current_task.kernel().delayed_releaser.apply(current_task);

            // file1 still holds its claim and its URBs work.
            let file3 = open_device(current_task, DEV_NUM);
            assert_eq!(claim(current_task, &file3, buffers.urb, 0), error!(EBUSY));
            assert_eq!(submit_test_control_urb(current_task, &file1, &buffers, 2), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file1, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, 0);
            assert_eq!(reap_urb(current_task, &file1, buffers.reap_arg, false), error!(EAGAIN));
            assert_eq!(release(current_task, &file1, buffers.urb, 0), Ok(SUCCESS));
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_concurrent_bulk_transfers_resets_alt_switches_and_errors_stress() {
        let config = build_config(
            1,
            &[
                alt(0, 0, vec![(EP1_IN, XFER_BULK, 512), (EP1_OUT, XFER_BULK, 512)]),
                alt(0, 1, vec![(EP1_IN, XFER_BULK, 512), (EP1_OUT, XFER_BULK, 512)]),
            ],
        );
        let options = MockDeviceOptions {
            cancel_drain_timeout: zx::MonotonicDuration::from_millis(20),
            ..MockDeviceOptions::new(config)
        };
        run_with_mock_device_options(options, |current_task, _device, mock| {
            const ITERS: usize = 25;
            let file_in = open_device(current_task, DEV_NUM);
            let file_out = open_device(current_task, DEV_NUM);
            let file_ctrl = open_device(current_task, DEV_NUM);
            let file_mgmt = open_device(current_task, DEV_NUM);

            // Worker 1: single-chunk and multi-chunk IN transfers with short timeouts and errors.
            let mock_in = Arc::clone(mock);
            let t_in = spawn_kernel_thread(current_task, move |task| {
                let (arg_in, _) = map_test_buffers(task);
                let buf_in = map_test_buffer(task, 73_728);
                for i in 0..ITERS {
                    if i % 5 == 0 {
                        mock_in
                            .bulk_in_scripted_packets
                            .lock()
                            .push_back((Vec::new(), zx::sys::ZX_ERR_IO_REFUSED));
                    }
                    let len = if i % 3 == 0 { 70_000 } else { 64 };
                    let _ = bulk(task, &file_in, arg_in, EP1_IN, len, 5, buf_in);
                }
            });

            // Worker 2: single-chunk and multi-chunk OUT transfers with periodic short writes.
            let mock_out = Arc::clone(mock);
            let t_out = spawn_kernel_thread(current_task, move |task| {
                let (arg_out, _) = map_test_buffers(task);
                let buf_out = map_test_buffer(task, 73_728);
                task.write_memory(buf_out, &[0x5au8; 70_000]).unwrap();
                for i in 0..ITERS {
                    if i % 5 == 0 {
                        mock_out.bulk_out_max_transfer_size.store(16, Ordering::Relaxed);
                    } else {
                        mock_out.bulk_out_max_transfer_size.store(usize::MAX, Ordering::Relaxed);
                    }
                    let len = if i % 4 == 0 { 70_000 } else { 64 };
                    let _ = bulk(task, &file_out, arg_out, EP1_OUT, len, 5, buf_out);
                }
                mock_out.bulk_out_max_transfer_size.store(usize::MAX, Ordering::Relaxed);
            });

            // Worker 3: concurrent control transfers exercising ControlTransferSlot and errors.
            let mock_ctrl = Arc::clone(mock);
            let t_ctrl = spawn_kernel_thread(current_task, move |task| {
                let (arg_ctrl, _) = map_test_buffers(task);
                for i in 0..ITERS {
                    let status =
                        if i % 6 == 0 { zx::sys::ZX_ERR_IO_REFUSED } else { zx::sys::ZX_OK };
                    mock_ctrl.ctrl_transfer_error.store(status, Ordering::Relaxed);
                    let _ = control(task, &file_ctrl, arg_ctrl, USB_DIR_OUT, TEST_REQUEST, 0, 10);
                }
                mock_ctrl.ctrl_transfer_error.store(zx::sys::ZX_OK, Ordering::Relaxed);
            });

            // Worker 4: concurrent resets, clear_halt, alternate setting switches, and
            // disconnect_claim, plus toggling hold_in_requests to exercise cancel_and_drain.
            let mock_mgmt = Arc::clone(mock);
            let t_mgmt = spawn_kernel_thread(current_task, move |task| {
                let (arg_mgmt, _) = map_test_buffers(task);
                for i in 0..ITERS {
                    mock_mgmt.hold_in_requests.store(i % 4 == 0, Ordering::SeqCst);
                    let _ = disconnect_claim(task, &file_mgmt, arg_mgmt, 0, 0, b"");
                    let _ = set_interface(task, &file_mgmt, arg_mgmt, 0, (i % 2) as u32);
                    let _ = clear_halt(task, &file_mgmt, arg_mgmt, u32::from(EP1_IN));
                    let _ = ioctl_no_arg(task, &file_mgmt, USBDEVFS_RESET);
                }
                mock_mgmt.hold_in_requests.store(false, Ordering::SeqCst);
            });

            t_in.recv().unwrap();
            t_out.recv().unwrap();
            t_ctrl.recv().unwrap();
            t_mgmt.recv().unwrap();
            mock.bulk_in_scripted_packets.lock().clear();

            assert_eq!(
                mock.invalid_queue_while_disabled.load(Ordering::SeqCst),
                0,
                "no request may be queued onto a disabled endpoint"
            );

            // After the stress run, verify the device and endpoints recover cleanly.
            let file_check = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(disconnect_claim(current_task, &file_check, arg, 0, 0, b""), Ok(SUCCESS));
            assert_eq!(bulk(current_task, &file_check, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(read_bytes::<4>(current_task, data), DEFAULT_IN_DATA);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_submiturb_validation() {
        run_with_mock_device(urb_test_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let submit =
                |urb: UsbdevfsUrbHeader| submit_urb(current_task, &file, buffers.urb, &urb);
            let bulk_in = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 4);

            // Unknown flags, and ISO_ASAP on non-isochronous URBs.
            assert_eq!(submit(UsbdevfsUrbHeader { flags: 0x1000, ..bulk_in }), error!(EINVAL));
            let iso_asap = USBDEVFS_URB_ISO_ASAP;
            assert_eq!(submit(UsbdevfsUrbHeader { flags: iso_asap, ..bulk_in }), error!(EINVAL));
            // Invalid buffers.
            assert_eq!(submit(UsbdevfsUrbHeader { buffer_length: -1, ..bulk_in }), error!(EINVAL));
            let xfer_max = USBFS_XFER_MAX as i32;
            assert_eq!(
                submit(UsbdevfsUrbHeader { buffer_length: xfer_max, ..bulk_in }),
                error!(EINVAL)
            );
            assert_eq!(submit(UsbdevfsUrbHeader { buffer: 0, ..bulk_in }), error!(EINVAL));
            // Invalid endpoints.
            assert_eq!(submit(UsbdevfsUrbHeader { endpoint: 0x10, ..bulk_in }), error!(EINVAL));
            assert_eq!(submit(UsbdevfsUrbHeader { endpoint: 0x85, ..bulk_in }), error!(ENOENT));
            assert_eq!(submit(UsbdevfsUrbHeader { endpoint: 0, ..bulk_in }), error!(ENOENT));
            // URB types that do not match the endpoint.
            for urb_type in
                [USBDEVFS_URB_TYPE_CONTROL, USBDEVFS_URB_TYPE_INTERRUPT, USBDEVFS_URB_TYPE_ISO, 7]
            {
                let urb = UsbdevfsUrbHeader { type_: urb_type as u8, ..bulk_in };
                assert_eq!(submit(urb), error!(EINVAL), "type {urb_type}");
            }
            // Completion signals are not supported.
            assert_eq!(submit(UsbdevfsUrbHeader { signr: 34, ..bulk_in }), error!(EINVAL));
            // URBs are limited by the memory available to usbfs.
            let too_large = UsbdevfsUrbHeader { buffer_length: 16 << 20, ..bulk_in };
            assert_eq!(submit(too_large), error!(ENOMEM));

            // Invalid control URBs: buffer shorter than the setup packet or the data stage.
            let setup = test_setup_packet(USB_DIR_IN | USB_TYPE_VENDOR, 0, 4);
            assert_eq!(
                submit_control_urb(current_task, &file, &buffers, setup, SETUP_PACKET_SIZE - 1),
                error!(EINVAL)
            );
            assert_eq!(
                submit_control_urb(current_task, &file, &buffers, setup, SETUP_PACKET_SIZE + 3),
                error!(EINVAL)
            );
            // Control URBs whose buffer extends past the end of user address space fail upfront
            // with EFAULT even when wLength == 0 and buffer_length > SETUP_PACKET_SIZE.
            let max_addr = current_task.maximum_valid_address().unwrap();
            let last_page = UserAddress::from((max_addr.ptr() - *PAGE_SIZE as usize) as u64);
            let mapped_last_page = do_mmap(
                current_task,
                last_page,
                *PAGE_SIZE as usize,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS | MAP_PRIVATE | starnix_uapi::MAP_FIXED,
                FdNumber::from_raw(-1),
                0,
            )
            .unwrap();
            let near_end = (mapped_last_page + (*PAGE_SIZE as usize - 16)).unwrap();
            let setup_zero = test_setup_packet(USB_DIR_OUT | USB_TYPE_VENDOR, 0, 0);
            current_task.write_object(UserRef::new(near_end), &setup_zero).unwrap();
            let overflow_ctrl = UsbdevfsUrbHeader {
                type_: USBDEVFS_URB_TYPE_CONTROL as u8,
                endpoint: 0,
                buffer: near_end.ptr() as u64,
                buffer_length: 32,
                ..Default::default()
            };
            assert_eq!(submit(overflow_ctrl), error!(EFAULT));
            assert!(mock.request_types.lock().is_empty());
            assert!(mock.ctrl_setups.lock().is_empty());

            // Unlike USBDEVFS_CONTROL, control URBs may have a data stage longer than 4096
            // bytes.
            let large = MAX_USBDEVFS_CONTROL_SIZE + 1;
            let large_buffers = urb_buffers_at(map_test_buffer(current_task, 2 * large));
            let setup = test_setup_packet(USB_DIR_IN | USB_TYPE_VENDOR, 0, large as u16);
            assert_eq!(
                submit_control_urb(current_task, &file, &large_buffers, setup, large + 8),
                Ok(SUCCESS)
            );
            let reaped = reap_urb(current_task, &file, large_buffers.reap_arg, true);
            assert_eq!(reaped, Ok(large_buffers.urb));
            let urb = read_urb(current_task, large_buffers.urb);
            assert_eq!(urb.status, 0);
            assert_eq!(urb.actual_length, 4);

            // Bulk URBs on interrupt endpoints are interrupt transfers.
            let interrupt = UsbdevfsUrbHeader { endpoint: EP3_IN, ..bulk_in };
            assert_eq!(submit(interrupt), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, 0);
            assert_eq!(*mock.request_types.lock(), vec![(EP3_IN, "interrupt")]);

            // Like Linux, a URB can be submitted again before it completes; both are reaped.
            assert_eq!(submit(bulk_in), Ok(SUCCESS));
            assert_eq!(submit(bulk_in), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(discard_urb(current_task, &file, buffers.urb), error!(EINVAL));

            // URBs on an interface claimed by another file description fail.
            assert_eq!(claim(current_task, &file, buffers.reap_arg, 0), Ok(SUCCESS));
            let file2 = open_device(current_task, DEV_NUM);
            assert_eq!(submit_urb(current_task, &file2, buffers.urb, &bulk_in), error!(EBUSY));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_urb_memory_is_shared_by_outstanding_transfers() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let large_length = 10 << 20;
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, large_length);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            // Transfers fail while the pending URB uses most of the memory, including on a
            // second device sharing the usbfs memory budget.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), error!(ENOMEM));
            assert_eq!(
                bulk(
                    current_task,
                    &file,
                    buffers.reap_arg,
                    EP2_OUT,
                    large_length as u32,
                    0,
                    buffers.data
                ),
                error!(ENOMEM)
            );
            let (device_desc, _) = create_mock_hid_descriptors();
            let mut device2 =
                UsbDevice::new(1, DEV_NUM + 1, USB_SPEED_HIGH, device_desc, urb_test_config());
            device2.sync_proxy = device.sync_proxy.clone();
            device2.memory = Arc::clone(&device.memory);
            let file2 = starnix_core::vfs::Anon::new_private_file(
                current_task,
                Box::new(UsbDeviceFile::new(Arc::new(device2))),
                OpenFlags::RDWR,
                "[usbfs]",
            );
            assert_eq!(submit_urb(current_task, &file2, buffers.urb, &urb), error!(ENOMEM));

            // The memory is available again once the URB is reaped.
            assert_eq!(discard_urb(current_task, &file, buffers.urb), Ok(SUCCESS));
            assert!(device.memory_used() > 0);
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(device.memory_used(), 0);
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_concurrent_transfers_and_device_disconnect_stress() {
        run_with_mock_device(hid_config(), |current_task, device, mock| {
            let mut workers = Vec::new();
            for i in 0..4 {
                let file = open_device(current_task, DEV_NUM);
                workers.push(spawn_kernel_thread(current_task, move |task| {
                    let (arg, data) = map_test_buffers(task);
                    for step in 0..20 {
                        let ep = if (i + step) % 2 == 0 { EP1_IN } else { EP1_OUT };
                        let _ = bulk(task, &file, arg, ep, 4, 10, data);
                        let _ = control(task, &file, arg, USB_DIR_OUT, TEST_REQUEST, 0, 10);
                        let _ = ioctl_no_arg(task, &file, USBDEVFS_RESET);
                    }
                }));
            }

            wait_until("workers started transfers", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) > 0
                    || !mock.received_bulk_out.lock().is_empty()
                    || !mock.ctrl_setups.lock().is_empty()
            });
            device.on_disconnect();

            for worker in workers {
                worker.recv().unwrap();
            }

            assert!(device.endpoints.lock().is_empty());
            assert_eq!(mock.invalid_queue_while_disabled.load(Ordering::SeqCst), 0);

            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), error!(ENODEV));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_urbs_on_endpoint_complete_in_submission_order() {
        run_with_mock_device(urb_test_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            const COUNT: usize = 8;
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = |i: usize| -> UserAddress { (page + (1024 + i * 16)).unwrap() };
            let reap_arg = (page + 2048usize).unwrap();
            let payloads: Vec<Vec<u8>> = (0..COUNT).map(|i| vec![i as u8; i + 1]).collect();
            for (i, payload) in payloads.iter().enumerate() {
                current_task.write_memory(data_addr(i), payload).unwrap();
                let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP2_OUT, data_addr(i), payload.len());
                assert_eq!(submit_urb(current_task, &file, urb_addr(i), &urb), Ok(SUCCESS));
            }
            for i in 0..COUNT {
                assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(i)));
                let urb = read_urb(current_task, urb_addr(i));
                assert_eq!(urb.status, 0);
                assert_eq!(urb.actual_length, payloads[i].len() as i32);
            }
            assert_eq!(*mock.received_bulk_out.lock(), payloads);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_discard_kills_only_the_discarded_urb() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = (page + 1024usize).unwrap();
            let reap_arg = (page + 2048usize).unwrap();
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr, 64);
            for i in 0..3 {
                assert_eq!(submit_urb(current_task, &file, urb_addr(i), &urb), Ok(SUCCESS));
            }
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            // Discarding a queued URB completes it right away.
            assert_eq!(discard_urb(current_task, &file, urb_addr(1)), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, reap_arg, false), Ok(urb_addr(1)));
            assert_eq!(read_urb(current_task, urb_addr(1)).status, status_of(errno!(ENOENT)));
            assert_eq!(discard_urb(current_task, &file, urb_addr(1)), error!(EINVAL));

            // Discarding the running URB waits for the cancellation of its request.
            assert_eq!(discard_urb(current_task, &file, urb_addr(0)), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, reap_arg, false), Ok(urb_addr(0)));
            let urb0 = read_urb(current_task, urb_addr(0));
            assert_eq!(urb0.status, status_of(errno!(ENOENT)));
            assert_eq!(urb0.actual_length, 0);

            // The last URB was not cancelled: it runs next. If its request completes when it
            // is discarded, its data is kept.
            wait_until("the next IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 2
            });
            *mock.cancel_behavior.lock() = Some(CancelBehavior::CompleteSuccessfullyAfterReply);
            assert_eq!(discard_urb(current_task, &file, urb_addr(2)), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, reap_arg, false), Ok(urb_addr(2)));
            let urb2 = read_urb(current_task, urb_addr(2));
            assert_eq!(urb2.status, 0);
            assert_eq!(urb2.actual_length, DEFAULT_IN_DATA.len() as i32);
            assert_eq!(read_bytes::<4>(current_task, data_addr), DEFAULT_IN_DATA);
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_discard_control_urb_completes_without_waiting_for_transfer() {
        run_with_mock_device(hid_config(), |current_task, _device, mock| {
            mock.hold_ctrl_transfers.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            assert_eq!(submit_test_control_urb(current_task, &file, &buffers, 1), Ok(SUCCESS));
            mock.ctrl_held.wait_signaled();

            // The control transfer cannot be cancelled, so the URB completes right away.
            assert_eq!(discard_urb(current_task, &file, buffers.urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));

            // The late result of the transfer is dropped, and the next URB runs once the
            // transfer completes.
            mock.hold_ctrl_transfers.store(false, Ordering::SeqCst);
            assert_eq!(submit_test_control_urb(current_task, &file, &buffers, 2), Ok(SUCCESS));
            for responder in mock.held_ctrl_responders.lock().drain(..) {
                responder.send(Ok(&[1, 2, 3])).unwrap();
            }
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            let urb = read_urb(current_task, buffers.urb);
            assert_eq!(urb.status, 0);
            assert_eq!(urb.actual_length, 3);
            let data_addr = (buffers.data + SETUP_PACKET_SIZE).unwrap();
            assert_eq!(read_bytes::<3>(current_task, data_addr), [TEST_REQUEST, 2, 0xca]);
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), error!(EAGAIN));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_zero_packet_urb_terminates_transfers_of_full_packets() {
        run_with_mock_device(urb_test_config(), |current_task, _device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let full = 2 * URB_TEST_MAX_PACKET_SIZE;
            let short = URB_TEST_MAX_PACKET_SIZE + 1;
            let zero_packet = USBDEVFS_URB_ZERO_PACKET;
            for (length, flags) in [(full, zero_packet), (short, zero_packet), (full, 0)] {
                let urb = UsbdevfsUrbHeader {
                    flags,
                    ..urb_header(USBDEVFS_URB_TYPE_BULK, EP2_OUT, buffers.data, length)
                };
                assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
                assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
                let urb = read_urb(current_task, buffers.urb);
                assert_eq!(urb.status, 0);
                assert_eq!(urb.actual_length, length as i32);
            }
            let sizes: Vec<usize> = mock.received_bulk_out.lock().iter().map(Vec::len).collect();
            assert_eq!(sizes, vec![full, 0, short, full]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_short_not_ok_urb_fails_and_rejects_continuations() {
        run_with_mock_device(urb_test_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);
            let submit = |flags: u32| {
                submit_urb(current_task, &file, buffers.urb, &UsbdevfsUrbHeader { flags, ..urb })
            };
            let reap = || {
                assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
                read_urb(current_task, buffers.urb)
            };

            // The device sends less data than requested, which fails the URB but keeps the
            // data.
            assert_eq!(submit(USBDEVFS_URB_SHORT_NOT_OK), Ok(SUCCESS));
            let failed = reap();
            assert_eq!(failed.status, status_of(errno!(EREMOTEIO)));
            assert_eq!(failed.actual_length, DEFAULT_IN_DATA.len() as i32);
            assert_eq!(read_bytes::<4>(current_task, buffers.data), DEFAULT_IN_DATA);

            // Continuation URBs are rejected until a new transfer starts.
            assert_eq!(submit(USBDEVFS_URB_BULK_CONTINUATION), error!(EREMOTEIO));
            assert_eq!(submit(0), Ok(SUCCESS));
            assert_eq!(reap().status, 0);
            assert_eq!(submit(USBDEVFS_URB_BULK_CONTINUATION), Ok(SUCCESS));
            assert_eq!(reap().status, 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reset_kills_pending_urbs_which_can_be_resubmitted() {
        run_with_mock_device(urb_test_config(), |current_task, _device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            assert_eq!(ioctl_no_arg(current_task, &file, USBDEVFS_RESET), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));

            // The URB is submitted again with the same address, and discarded.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 2);
            assert_eq!(discard_urb(current_task, &file, buffers.urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), error!(EAGAIN));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_release_interface_kills_its_urbs() {
        run_with_mock_device(urb_test_config(), |current_task, _device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            assert_eq!(release(current_task, &file, buffers.reap_arg, 0), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_set_interface_kills_pending_and_queued_urbs() {
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP1_IN, XFER_BULK, 64)]), alt(0, 1, vec![(EP1_IN, XFER_BULK, 64)])],
        );
        run_with_mock_device(config, |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = (page + 1024usize).unwrap();
            let reap_arg = (page + 2048usize).unwrap();
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr, 64);

            // Submit a running URB and a queued URB on EP1_IN.
            assert_eq!(submit_urb(current_task, &file, urb_addr(0), &urb), Ok(SUCCESS));
            assert_eq!(submit_urb(current_task, &file, urb_addr(1), &urb), Ok(SUCCESS));
            wait_until("the IN request", || mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1);

            // Switching alternate setting kills both the running and queued URBs with -ENOENT
            // without queuing a request for the second URB onto the disabled endpoint.
            assert_eq!(set_interface(current_task, &file, reap_arg, 0, 1), Ok(SUCCESS));
            assert_eq!(mock.invalid_queue_while_disabled.load(Ordering::SeqCst), 0);
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 1);

            let reaped0 = reap_urb(current_task, &file, reap_arg, false).unwrap();
            let reaped1 = reap_urb(current_task, &file, reap_arg, false).unwrap();
            assert!(
                (reaped0 == urb_addr(0) && reaped1 == urb_addr(1))
                    || (reaped0 == urb_addr(1) && reaped1 == urb_addr(0))
            );
            assert_eq!(read_urb(current_task, urb_addr(0)).status, status_of(errno!(ENOENT)));
            assert_eq!(read_urb(current_task, urb_addr(1)).status, status_of(errno!(ENOENT)));
            assert_eq!(reap_urb(current_task, &file, reap_arg, false), error!(EAGAIN));
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_sync_bulk_and_async_urb_contention_and_teardown() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 64);

            // Start a synchronous USBDEVFS_BULK on EP1_IN without timeout so it holds the
            // endpoint transfer slot.
            let file_for_bulk = Arc::clone(&file);
            let sync_bulk = spawn_kernel_thread(current_task, move |task| {
                let (arg, data) = map_test_buffers(task);
                bulk(task, &file_for_bulk, arg, EP1_IN, 4, 0, data)
            });
            wait_until("the sync bulk IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });

            // Submit an async URB on the same endpoint; its worker blocks waiting for the
            // endpoint transfer slot.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("async URB waiting for transfer slot", || {
                device.endpoint_transfer_waiters(EP1_IN) == 1
            });

            // 1. Discarding the async URB while the sync bulk holds the slot wakes the URB
            // worker via UrbCancellation::wake and returns promptly without cancelling the sync
            // bulk request.
            assert_eq!(discard_urb(current_task, &file, buffers.urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));

            // 2. Submit another async URB on EP1_IN and release the interface: drop_claim
            // cancels the endpoint requests before waiting for the URB worker, so neither the
            // sync bulk nor release_interface deadlocks.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("second async URB waiting for transfer slot", || {
                device.endpoint_transfer_waiters(EP1_IN) == 1
            });
            assert_eq!(release(current_task, &file, buffers.reap_arg, 0), Ok(SUCCESS));
            assert_eq!(sync_bulk.recv().unwrap(), error!(ESHUTDOWN));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, false), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_with_queued_urbs_restarts_endpoint() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            mock.reset_endpoint_status.store(zx::sys::ZX_ERR_NOT_SUPPORTED, Ordering::Relaxed);
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = (page + 1024usize).unwrap();
            let reap_arg = (page + 2048usize).unwrap();
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr, 4);

            // Submit two URBs: URB 0 is in flight, URB 1 is queued behind it.
            assert_eq!(submit_urb(current_task, &file, urb_addr(0), &urb), Ok(SUCCESS));
            assert_eq!(submit_urb(current_task, &file, urb_addr(1), &urb), Ok(SUCCESS));
            wait_until("the first IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });

            // When reset_endpoint returns ZX_ERR_NOT_SUPPORTED, clear_halt restarts the endpoint
            // because URBs are pending on it.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(clear_halt(current_task, &file, reap_arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(mock.invalid_queue_while_disabled.load(Ordering::SeqCst), 0);

            // URB 0 was aborted by the endpoint disable during clear_halt (-ESHUTDOWN), while
            // URB 1 runs on the restarted endpoint and succeeds.
            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(0)));
            assert_eq!(read_urb(current_task, urb_addr(0)).status, status_of(errno!(ESHUTDOWN)));
            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(1)));
            let urb1 = read_urb(current_task, urb_addr(1));
            assert_eq!(urb1.status, 0);
            assert_eq!(urb1.actual_length, DEFAULT_IN_DATA.len() as i32);
            assert_eq!(
                *mock.enable_log.lock(),
                vec![(EP1_IN, true, 64), (EP1_IN, false, 64), (EP1_IN, true, 64)]
            );
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_interrupt_urb_and_partial_transfer_and_error_races() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);

            // 1. Explicit USBDEVFS_URB_TYPE_INTERRUPT submission and reap on EP3_IN.
            let int_urb = urb_header(USBDEVFS_URB_TYPE_INTERRUPT, EP3_IN, buffers.data, 4);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &int_urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            let reaped_int = read_urb(current_task, buffers.urb);
            assert_eq!(reaped_int.status, 0);
            assert_eq!(reaped_int.actual_length, 4);

            // 2. Submitting an IN URB whose buffer extends past the end of user address space
            // fails upfront with EFAULT.
            let invalid_in =
                urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, UserAddress::from(u64::MAX - 8), 64);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &invalid_in), error!(EFAULT));

            // 3. Multi-chunk async IN URB where chunk 1 succeeds and chunk 2 fails with EPIPE
            // preserves actual_length and data from chunk 1.
            let large_buf = map_test_buffer(current_task, 73_728);
            {
                let mut scripted = mock.bulk_in_scripted_packets.lock();
                scripted.push_back((vec![0x33u8; 65_536], zx::sys::ZX_OK));
                scripted.push_back((Vec::new(), zx::sys::ZX_ERR_IO_REFUSED));
            }
            let multi_in = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, large_buf, 70_000);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &multi_in), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            let reaped_multi = read_urb(current_task, buffers.urb);
            assert_eq!(reaped_multi.status, status_of(errno!(EPIPE)));
            assert_eq!(reaped_multi.actual_length, 65_536);
            assert_eq!(read_bytes::<4>(current_task, large_buf), [0x33; 4]);

            // 4. Control URB on a second file description discarded while an earlier control
            // transfer holds the device control slot wakes immediately from
            // `ControlTransferSlot::acquire` without hanging behind the stuck control transfer.
            mock.hold_ctrl_transfers.store(true, Ordering::SeqCst);
            let file2 = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let ctrl0 = urb_buffers_at(page);
            let ctrl1 = UrbBuffers {
                urb: (page + 64usize).unwrap(),
                reap_arg: ctrl0.reap_arg,
                data: (page + 512usize).unwrap(),
            };
            assert_eq!(submit_test_control_urb(current_task, &file, &ctrl0, 1), Ok(SUCCESS));
            mock.ctrl_held.wait_signaled();
            assert_eq!(submit_test_control_urb(current_task, &file2, &ctrl1, 2), Ok(SUCCESS));
            assert_eq!(discard_urb(current_task, &file, ctrl0.urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, ctrl0.reap_arg, false), Ok(ctrl0.urb));
            // ctrl1's worker on `file2` is blocked in `ControlTransferSlot::acquire` waiting for
            // ctrl0's FIDL call; discarding ctrl1 wakes it via `UrbCancellation::wake`.
            wait_until("ctrl1 waiting for control transfer slot", || {
                device.control_transfer_waiters() == 1
            });
            assert_eq!(discard_urb(current_task, &file2, ctrl1.urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file2, ctrl1.reap_arg, false), Ok(ctrl1.urb));
            mock.hold_ctrl_transfers.store(false, Ordering::SeqCst);
            for responder in mock.held_ctrl_responders.lock().drain(..) {
                let _ = responder.send(Ok(&[1, 2, 3]));
            }
            wait_until("release of control URB memory", || device.memory_used() == 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_reset_endpoint_with_queued_urbs_and_bulk() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = |i: usize| -> UserAddress { (page + (1024 + i * 64)).unwrap() };
            let reap_arg = (page + 2048usize).unwrap();
            let urb1 = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr(1), 4);
            let urb2 = UsbdevfsUrbHeader {
                flags: USBDEVFS_URB_BULK_CONTINUATION,
                ..urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr(2), 4)
            };

            // A synchronous USBDEVFS_BULK is in flight on EP1_IN; URB 1 and continuation URB 2
            // are submitted behind it, so URB 1's worker waits in `acquire_transfer`.
            let file_for_bulk = Arc::clone(&file);
            let sync_bulk = spawn_kernel_thread(current_task, move |task| {
                let (arg, data) = map_test_buffers(task);
                bulk(task, &file_for_bulk, arg, EP1_IN, 4, 0, data)
            });
            wait_until("the first IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });
            assert_eq!(submit_urb(current_task, &file, urb_addr(1), &urb1), Ok(SUCCESS));
            assert_eq!(submit_urb(current_task, &file, urb_addr(2), &urb2), Ok(SUCCESS));
            wait_until("queued URB waiting for transfer slot", || {
                device.endpoint_transfer_waiters(EP1_IN) == 1
            });

            // When reset_endpoint succeeds, clear_halt cancels the in-flight request (sync_bulk
            // fails with ESHUTDOWN), while URB 1 waiting in `acquire_transfer` and continuation
            // URB 2 wait for the restart and succeed without ESHUTDOWN/ECONNRESET.
            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(clear_halt(current_task, &file, reap_arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(sync_bulk.recv().unwrap(), error!(ESHUTDOWN));

            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(1)));
            let reaped1 = read_urb(current_task, urb_addr(1));
            assert_eq!(reaped1.status, 0);
            assert_eq!(reaped1.actual_length, DEFAULT_IN_DATA.len() as i32);

            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(2)));
            let reaped2 = read_urb(current_task, urb_addr(2));
            assert_eq!(reaped2.status, 0);
            assert_eq!(reaped2.actual_length, DEFAULT_IN_DATA.len() as i32);
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_clear_halt_reconnects_closed_endpoint_for_queued_and_racing_urbs() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let page = map_test_page(current_task);
            let urb_addr = |i: usize| -> UserAddress { (page + i * 64).unwrap() };
            let data_addr = |i: usize| -> UserAddress { (page + (1024 + i * 64)).unwrap() };
            let reap_arg = (page + 2048usize).unwrap();
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, data_addr(0), 4);

            // Case 1: The driver closes the endpoint channel when CancelAll runs while URB 0 is
            // in flight and URB 1 is queued behind it.
            mock.reset_endpoint_status.store(zx::sys::ZX_ERR_NOT_SUPPORTED, Ordering::Relaxed);
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            mock.drop_endpoint_on_cancel.store(true, Ordering::SeqCst);
            assert_eq!(submit_urb(current_task, &file, urb_addr(0), &urb), Ok(SUCCESS));
            wait_until("the first IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });
            assert_eq!(submit_urb(current_task, &file, urb_addr(1), &urb), Ok(SUCCESS));

            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(clear_halt(current_task, &file, reap_arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 2);

            // URB 0 fails from the channel closure, while queued URB 1 runs on the reconnected
            // endpoint client and succeeds.
            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(0)));
            assert_ne!(read_urb(current_task, urb_addr(0)).status, 0);
            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(1)));
            let reaped1 = read_urb(current_task, urb_addr(1));
            assert_eq!(reaped1.status, 0);
            assert_eq!(reaped1.actual_length, DEFAULT_IN_DATA.len() as i32);

            // Case 2: When reset_endpoint returns ZX_ERR_NOT_SUPPORTED and the driver closes the
            // endpoint channel on enable_endpoint(false), clear_halt connects a fresh client and
            // queued URB 1 succeeds on the new client.
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            mock.reset_endpoint_status.store(zx::sys::ZX_ERR_NOT_SUPPORTED, Ordering::Relaxed);
            mock.close_endpoint_on_disable.store(true, Ordering::SeqCst);
            assert_eq!(submit_urb(current_task, &file, urb_addr(0), &urb), Ok(SUCCESS));
            wait_until("the third IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 3
            });
            assert_eq!(submit_urb(current_task, &file, urb_addr(1), &urb), Ok(SUCCESS));

            mock.hold_in_requests.store(false, Ordering::SeqCst);
            assert_eq!(clear_halt(current_task, &file, reap_arg, u32::from(EP1_IN)), Ok(SUCCESS));
            assert_eq!(mock.connect_count.load(Ordering::SeqCst), 3);

            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(0)));
            assert_eq!(read_urb(current_task, urb_addr(0)).status, status_of(errno!(ESHUTDOWN)));
            assert_eq!(reap_urb(current_task, &file, reap_arg, true), Ok(urb_addr(1)));
            let reaped1_case2 = read_urb(current_task, urb_addr(1));
            assert_eq!(reaped1_case2.status, 0);
            assert_eq!(reaped1_case2.actual_length, DEFAULT_IN_DATA.len() as i32);
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reapurb_overflow_checks_before_modifying_user_memory() {
        run_with_mock_device(urb_test_config(), |current_task, _device, _mock| {
            let file = open_device(current_task, DEV_NUM);
            let dev_file = file.downcast_file::<device::UsbDeviceFile>().unwrap();

            // Map a page above 4 GiB so the URB address does not fit in a 32-bit pointer.
            let high_page = do_mmap(
                current_task,
                UserAddress::from(0x1_0000_0000u64),
                *PAGE_SIZE as usize,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS | MAP_PRIVATE | starnix_uapi::MAP_FIXED,
                FdNumber::from_raw(-1),
                0,
            )
            .unwrap();
            let buffers = urb_buffers_at(high_page);
            current_task.write_memory(buffers.data, &[0xaau8; 4]).unwrap();
            let urb = UsbdevfsUrbHeader {
                status: -999,
                actual_length: -123,
                ..urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 4)
            };
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("completed URB ready to reap", || {
                file.ops().query_events(&file, current_task).unwrap().contains(FdEvents::POLLOUT)
            });

            // A 32-bit reap on a >32-bit URB address must fail with EOVERFLOW before modifying
            // the user data buffer or URB header.
            assert_eq!(
                dev_file.reap_urb(current_task, buffers.reap_arg.into(), true, false),
                error!(EOVERFLOW)
            );
            assert_eq!(read_bytes::<4>(current_task, buffers.data), [0xaau8; 4]);
            let untouched = read_urb(current_task, buffers.urb);
            assert_eq!(untouched.status, -999);
            assert_eq!(untouched.actual_length, -123);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_submiturb_revalidates_endpoint_across_set_interface_alt_switch() {
        // Interface 0 alt 0 has EP2_OUT as XFER_BULK; alt 1 has EP2_OUT as XFER_INT.
        let config = build_config(
            1,
            &[alt(0, 0, vec![(EP2_OUT, XFER_BULK, 64)]), alt(0, 1, vec![(EP2_OUT, XFER_INT, 64)])],
        );
        run_with_mock_device(config, |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);

            // Enable EP2_OUT in alt 0 first, then switch interface 0 to alt 1 right after
            // `prepare_urb` releases the device state lock and copies the user buffer.
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP2_OUT, buffers.data, 4);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));

            let file_for_hook = Arc::clone(&file);
            let arg_for_hook = buffers.reap_arg;
            device.state.lock().on_urb_prepared = Some(Box::new(move |task| {
                assert_eq!(set_interface(task, &file_for_hook, arg_for_hook, 0, 1), Ok(SUCCESS));
            }));

            // In alt 1, EP2_OUT is an interrupt endpoint with a new endpoint client; the bulk
            // URB prepared against alt 0's ParsedEndpoint must be rejected during post-copy
            // re-validation.
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), error!(EINVAL));
            assert_eq!(mock.invalid_queue_while_disabled.load(Ordering::SeqCst), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_submiturb_revalidates_endpoint_across_clear_halt_fallback() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 4);

            // Simulate non-halted clear_halt fallback that disables the endpoint while no URBs
            // are pending yet.
            mock.reset_endpoint_status.store(zx::sys::ZX_ERR_NOT_SUPPORTED, Ordering::Relaxed);
            let file_for_hook = Arc::clone(&file);
            let reap_arg = buffers.reap_arg;
            device.state.lock().on_urb_prepared = Some(Box::new(move |task| {
                assert_eq!(
                    clear_halt(task, &file_for_hook, reap_arg, u32::from(EP1_IN)),
                    Ok(SUCCESS)
                );
            }));

            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            assert_eq!(reap_urb(current_task, &file, buffers.reap_arg, true), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_acquire_transfer_does_not_steal_wake_from_wait_for_completion() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            mock.hold_in_requests.store(true, Ordering::SeqCst);
            mock.cancel_all_status.store(zx::sys::ZX_ERR_IO, Ordering::SeqCst);

            let file = open_device(current_task, DEV_NUM);
            let file_for_bulk = Arc::clone(&file);
            let sync_bulk = spawn_kernel_thread(current_task, move |task| {
                let (arg, data) = map_test_buffers(task);
                bulk(task, &file_for_bulk, arg, EP1_IN, 4, 0, data)
            });
            wait_until("the sync bulk IN request", || {
                mock.bulk_in_queue_count.load(Ordering::SeqCst) == 1
            });

            // Contender waits in acquire_transfer on EP1_IN.
            let buffers = map_urb_buffers(current_task);
            let urb = urb_header(USBDEVFS_URB_TYPE_BULK, EP1_IN, buffers.data, 4);
            assert_eq!(submit_urb(current_task, &file, buffers.urb, &urb), Ok(SUCCESS));
            wait_until("contender waiting in acquire_transfer", || {
                device.endpoint_transfer_waiters(EP1_IN) == 1
            });

            // Resetting the device triggers cancel_all. CancelAll returns ZX_ERR_IO without
            // completions, so the sync bulk relies on self.wake. Since acquire_transfer no longer
            // waits on or clears self.wake, sync_bulk receives the wake signal and unblocks with
            // ESHUTDOWN promptly rather than hanging forever.
            let reap_arg = (buffers.urb + 128usize).unwrap();
            assert_eq!(ioctl_no_arg(current_task, &file, USBDEVFS_RESET), Ok(SUCCESS));
            assert_eq!(sync_bulk.recv().unwrap(), error!(ESHUTDOWN));
            assert_eq!(reap_urb(current_task, &file, reap_arg, false), Ok(buffers.urb));
            assert_eq!(read_urb(current_task, buffers.urb).status, status_of(errno!(ENOENT)));
            assert_eq!(device.memory_used(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_restart_endpoint_without_proxy_does_not_leave_endpoint_restarting() {
        run_with_mock_device(urb_test_config(), |current_task, device, mock| {
            let file = open_device(current_task, DEV_NUM);
            let (arg, data) = map_test_buffers(current_task);

            // Connect EP1_IN, then test `restart_endpoint` when the device has no proxy.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(
                device.restart_endpoint_without_proxy_for_test(EP1_IN),
                (error!(ENODEV), false)
            );

            // Subsequent transfers on EP1_IN must still complete instead of hanging forever in
            // `acquire_transfer` because `state.restarting` was left set.
            assert_eq!(bulk(current_task, &file, arg, EP1_IN, 4, 1000, data), Ok(4.into()));
            assert_eq!(mock.bulk_in_queue_count.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_usb_virtual_bus_end_to_end() {
        let builder = RealmBuilder::new().await.unwrap();
        let exposes = vec![
            Capability::service::<fusb_peripheral::ServiceMarker>().into(),
            Capability::service::<fvirtualbustest::ExpectBusTestServiceMarker>().into(),
            Capability::service::<fhardware_usb::UsbServiceMarker>().into(),
        ];
        let args = fdt::RealmArgs {
            root_driver: Some("fuchsia-boot:///dtr#meta/usb-virtual-bus.cm".to_string()),
            ..Default::default()
        };
        builder
            .driver_test_realm_setup(Options2::new().driver_exposes(exposes), args)
            .await
            .unwrap();
        let realm = builder.build().await.unwrap();
        realm.wait_for_bootup().await.unwrap();

        let dev = realm.driver_test_realm_connect_to_dev().unwrap();
        let virtual_bus = device_watcher::recursive_wait_and_open::<fusb_vbus::BusMarker>(
            &dev,
            "usb-virtual-bus",
        )
        .await
        .unwrap();
        assert_eq!(virtual_bus.enable().await.unwrap(), zx::sys::ZX_OK);

        let peripheral =
            Service::open_from_dir(realm.root.get_exposed_dir(), fusb_peripheral::ServiceMarker)
                .unwrap()
                .watch_for_any()
                .await
                .unwrap()
                .connect_to_device()
                .unwrap();

        // Clear any initial peripheral functions and wait for FunctionsCleared.
        {
            let (events_client, mut events_stream) =
                fidl::endpoints::create_request_stream::<fusb_peripheral::EventsMarker>();
            peripheral.set_state_change_listener(events_client).unwrap();
            peripheral.clear_functions().await.unwrap();
            while let Some(Ok(req)) = events_stream.next().await {
                if let fusb_peripheral::EventsRequest::FunctionsCleared { control_handle } = req {
                    control_handle.shutdown();
                    break;
                }
            }
        }

        // Configure the peripheral device with virtual-bus-tester-function (USB_CLASS_VENDOR = 0xFF).
        {
            let (events_client, mut events_stream) =
                fidl::endpoints::create_request_stream::<fusb_peripheral::EventsMarker>();
            peripheral.set_state_change_listener(events_client).unwrap();
            let device_desc = fusb_peripheral::DeviceDescriptor {
                bcd_usb: 0x0200,
                b_device_class: 0,
                b_device_sub_class: 0,
                b_device_protocol: 0,
                b_max_packet_size0: 64,
                id_vendor: 0x18d1,
                id_product: 0x0002,
                bcd_device: 0x0100,
                manufacturer: "Google".to_string(),
                product: "USB Virtual Bus Virtual Device".to_string(),
                serial: "ebfd5ad49d2a".to_string(),
                b_num_configurations: 1,
            };
            let function = fusb_peripheral::FunctionDescriptor {
                interface_class: 0xff, // USB_CLASS_VENDOR
                interface_subclass: 0,
                interface_protocol: 0,
            };
            peripheral.set_configuration(&device_desc, &[vec![function]]).await.unwrap().unwrap();
            while let Some(Ok(req)) = events_stream.next().await {
                if let fusb_peripheral::EventsRequest::FunctionRegistered { responder } = req {
                    let _ = responder.send();
                    break;
                }
            }
        }

        assert_eq!(virtual_bus.connect().await.unwrap(), zx::sys::ZX_OK);

        let expect_bus_test = Service::open_from_dir(
            realm.root.get_exposed_dir(),
            fvirtualbustest::ExpectBusTestServiceMarker,
        )
        .unwrap()
        .watch_for_any()
        .await
        .unwrap()
        .connect_to_device()
        .unwrap();

        let usb_service =
            Service::open_from_dir(realm.root.get_exposed_dir(), fhardware_usb::UsbServiceMarker)
                .unwrap();

        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let kernel_for_watcher = Arc::clone(kernel);
                let manager_for_watcher = manager.clone();
                let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
                let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
                kernel.kthreads.spawn_future(
                    move || async move {
                        let _ = futures::future::Abortable::new(
                            watch_usb_devices_from_service(
                                kernel_for_watcher,
                                manager_for_watcher,
                                usb_service,
                            ),
                            abort_registration,
                        )
                        .await;
                        let _ = done_tx.send(());
                    },
                    "test_usbfs_virtual_bus_watcher",
                );

                // Wait for watch_usb_devices_from_service to discover and register /dev/bus/usb/001/002.
                let open = |path: &str| {
                    open_dev_node(current_task, path.as_bytes().into(), OpenFlags::RDWR)
                };
                wait_until_async("/dev/bus/usb/001/002", || open("bus/usb/001/002").is_ok()).await;
                let file = open("bus/usb/001/002").unwrap();

                // Verify devtmpfs node bus/usb/001/002 was created with expected major and minor
                assert_eq!(file.name.entry.node.info().rdev.major(), USB_DEVICE_MAJOR);
                assert_eq!(file.name.entry.node.info().rdev.minor(), 1);

                // 1. Read raw descriptors from usbfs and verify VID/PID + extract endpoint addresses.
                let mut output_buf = VecOutputBuffer::new(256);
                let bytes_read = file.ops().read(&file, current_task, 0, &mut output_buf).unwrap();
                let raw_desc = Vec::from(output_buf);
                assert_eq!(bytes_read, raw_desc.len());
                assert!(raw_desc.len() >= 18 + 9 + 9 + 7 + 7);
                let parsed_dev_desc =
                    usb_device_descriptor::read_from_bytes(&raw_desc[0..18]).unwrap();
                assert_eq!(u16::from_le(parsed_dev_desc.idVendor), 0x18d1);
                assert_eq!(u16::from_le(parsed_dev_desc.idProduct), 0x0002);

                let mut bulk_out_ep: Option<u8> = None;
                let mut bulk_in_ep: Option<u8> = None;
                let mut pos = USB_DT_DEVICE_SIZE as usize;
                while pos + 2 <= raw_desc.len() {
                    let length = raw_desc[pos] as usize;
                    let descriptor_type = raw_desc[pos + 1];
                    if length < 2 || pos + length > raw_desc.len() {
                        break;
                    }
                    if u32::from(descriptor_type) == USB_DT_ENDPOINT
                        && length >= USB_DT_ENDPOINT_SIZE as usize
                    {
                        let ep_addr = raw_desc[pos + 2];
                        if (u32::from(ep_addr) & USB_DIR_IN) != 0 {
                            bulk_in_ep = Some(ep_addr);
                        } else {
                            bulk_out_ep = Some(ep_addr);
                        }
                    }
                    pos += length;
                }
                let bulk_out_ep = bulk_out_ep.expect("bulk OUT endpoint descriptor present");
                let bulk_in_ep = bulk_in_ep.expect("bulk IN endpoint descriptor present");

                let page_addr = map_test_page(current_task);
                let arg_addr = page_addr;
                let data_addr = (page_addr + 256usize).unwrap();

                // 2. Claim interface 0 via USBDEVFS_DISCONNECT_CLAIM.
                let disconnect_claim_arg = usbdevfs_disconnect_claim::default();
                current_task.write_object(UserRef::new(arg_addr), &disconnect_claim_arg).unwrap();
                file.ops()
                    .ioctl(&file, current_task, USBDEVFS_DISCONNECT_CLAIM, arg_addr.into())
                    .unwrap();

                // 3. USBDEVFS_CONTROL (OUT and IN) over usb-virtual-bus to virtual-bus-tester-function.
                let expect_ctrl_out_fut = expect_bus_test.expect_control(false, &[]);
                expect_bus_test.sync().await.unwrap();
                let ctrl_out_payload = [0x10u8, 0x20, 0x30, 0x40, 0x50];
                current_task.write_memory(data_addr, &ctrl_out_payload).unwrap();
                let ctrl_out = usbdevfs_ctrltransfer {
                    bRequestType: 0x01,
                    bRequest: 0xff,
                    wValue: 0x000a,
                    wIndex: 0,
                    wLength: ctrl_out_payload.len() as u16,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &ctrl_out).unwrap();
                let result = file
                    .ops()
                    .ioctl(&file, current_task, USBDEVFS_CONTROL, arg_addr.into())
                    .unwrap();
                assert_eq!(result, ctrl_out_payload.len().into());
                let received_ctrl_out = expect_ctrl_out_fut.await.unwrap().unwrap();
                assert_eq!(received_ctrl_out, ctrl_out_payload);

                let expected_ctrl_in = vec![0xa1u8, 0xb2, 0xc3];
                let expect_ctrl_in_fut = expect_bus_test.expect_control(true, &expected_ctrl_in);
                expect_bus_test.sync().await.unwrap();
                let ctrl_in = usbdevfs_ctrltransfer {
                    bRequestType: 0x81,
                    bRequest: 0xff,
                    wValue: 0x000a,
                    wIndex: 0,
                    wLength: expected_ctrl_in.len() as u16,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &ctrl_in).unwrap();
                let result = file
                    .ops()
                    .ioctl(&file, current_task, USBDEVFS_CONTROL, arg_addr.into())
                    .unwrap();
                assert_eq!(result, expected_ctrl_in.len().into());
                assert_eq!(
                    current_task.read_memory_to_vec(data_addr, expected_ctrl_in.len()).unwrap(),
                    expected_ctrl_in
                );
                expect_ctrl_in_fut.await.unwrap().unwrap();

                // 4. USBDEVFS_BULK (OUT and IN) over ConnectToEndpoint on usb-virtual-bus.
                let expect_out_fut = expect_bus_test.expect_out();
                expect_bus_test.sync().await.unwrap();
                let bulk_out_payload = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
                current_task.write_memory(data_addr, &bulk_out_payload).unwrap();
                let bulk_out = usbdevfs_bulktransfer {
                    ep: bulk_out_ep as u32,
                    len: bulk_out_payload.len() as u32,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &bulk_out).unwrap();
                let result =
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()).unwrap();
                assert_eq!(result, bulk_out_payload.len().into());
                let received_bulk_out = expect_out_fut.await.unwrap().unwrap();
                assert_eq!(received_bulk_out, bulk_out_payload);

                // Test Zero-Length Packet (ZLP) bulk OUT transfer
                let expect_zlp_out_fut = expect_bus_test.expect_out();
                expect_bus_test.sync().await.unwrap();
                let zlp_bulk_out = usbdevfs_bulktransfer {
                    ep: bulk_out_ep as u32,
                    len: 0,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &zlp_bulk_out).unwrap();
                let result =
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()).unwrap();
                assert_eq!(result, 0.into());
                let received_zlp_out = expect_zlp_out_fut.await.unwrap().unwrap();
                assert_eq!(received_zlp_out, Vec::<u8>::new());

                let expected_bulk_in = vec![0xdeu8, 0xad, 0xbe, 0xef, 0x42];
                let expect_in_fut = expect_bus_test.expect_in(&expected_bulk_in);
                expect_bus_test.sync().await.unwrap();
                let bulk_in = usbdevfs_bulktransfer {
                    ep: bulk_in_ep as u32,
                    len: expected_bulk_in.len() as u32,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &bulk_in).unwrap();
                let result =
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()).unwrap();
                assert_eq!(result, expected_bulk_in.len().into());
                assert_eq!(
                    current_task.read_memory_to_vec(data_addr, expected_bulk_in.len()).unwrap(),
                    expected_bulk_in
                );
                let actual_in = expect_in_fut.await.unwrap().unwrap();
                assert_eq!(actual_in, expected_bulk_in.len() as u64);

                // 5. USBDEVFS_SUBMITURB + USBDEVFS_REAPURB over ConnectToEndpoint on usb-virtual-bus.
                let expect_urb_out_fut = expect_bus_test.expect_out();
                expect_bus_test.sync().await.unwrap();
                let urb_out_payload = [0xcau8, 0xfe, 0xba, 0xbe];
                current_task.write_memory(data_addr, &urb_out_payload).unwrap();
                let urb_out = UsbdevfsUrbHeader {
                    type_: USBDEVFS_URB_TYPE_BULK as u8,
                    endpoint: bulk_out_ep,
                    buffer: data_addr.ptr() as u64,
                    buffer_length: urb_out_payload.len() as i32,
                    ..Default::default()
                };
                current_task.write_object(UserRef::new(arg_addr), &urb_out).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_SUBMITURB, arg_addr.into()).unwrap();
                let reap_addr = (page_addr + 512usize).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_REAPURB, reap_addr.into()).unwrap();
                let reaped_ptr: u64 = current_task.read_object(UserRef::new(reap_addr)).unwrap();
                assert_eq!(reaped_ptr, arg_addr.ptr() as u64);
                let received_urb_out = expect_urb_out_fut.await.unwrap().unwrap();
                assert_eq!(received_urb_out, urb_out_payload);

                let expected_urb_in = vec![0x55u8, 0x66, 0x77, 0x88, 0x99, 0xaa];
                let expect_urb_in_fut = expect_bus_test.expect_in(&expected_urb_in);
                expect_bus_test.sync().await.unwrap();
                let urb_in = UsbdevfsUrbHeader {
                    type_: USBDEVFS_URB_TYPE_BULK as u8,
                    endpoint: bulk_in_ep,
                    buffer: data_addr.ptr() as u64,
                    buffer_length: expected_urb_in.len() as i32,
                    ..Default::default()
                };
                current_task.write_object(UserRef::new(arg_addr), &urb_in).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_SUBMITURB, arg_addr.into()).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_REAPURB, reap_addr.into()).unwrap();
                let updated_urb: UsbdevfsUrbHeader =
                    current_task.read_object(UserRef::new(arg_addr)).unwrap();
                assert_eq!(updated_urb.actual_length, expected_urb_in.len() as i32);
                assert_eq!(
                    current_task.read_memory_to_vec(data_addr, expected_urb_in.len()).unwrap(),
                    expected_urb_in
                );
                let actual_urb_in = expect_urb_in_fut.await.unwrap().unwrap();
                assert_eq!(actual_urb_in, expected_urb_in.len() as u64);

                // 6. USBDEVFS_DISCARDURB and USBDEVFS_RESET of in-flight bulk IN URBs, which the
                // virtual-bus-tester-function never completes, then USBDEVFS_CLEAR_HALT followed
                // immediately by USBDEVFS_BULK on both OUT and IN endpoints.
                let pending_urb_in = UsbdevfsUrbHeader {
                    type_: USBDEVFS_URB_TYPE_BULK as u8,
                    endpoint: bulk_in_ep,
                    buffer: data_addr.ptr() as u64,
                    buffer_length: 64,
                    ..Default::default()
                };
                let killed_status = -(errno!(ENOENT).code.error_code() as i32);
                current_task.write_object(UserRef::new(arg_addr), &pending_urb_in).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_SUBMITURB, arg_addr.into()).unwrap();
                file.ops()
                    .ioctl(&file, current_task, USBDEVFS_DISCARDURB, arg_addr.into())
                    .unwrap();
                assert_eq!(
                    file.ops().ioctl(&file, current_task, USBDEVFS_DISCARDURB, arg_addr.into()),
                    error!(EINVAL)
                );
                file.ops().ioctl(&file, current_task, USBDEVFS_REAPURB, reap_addr.into()).unwrap();
                let reaped_ptr: u64 = current_task.read_object(UserRef::new(reap_addr)).unwrap();
                assert_eq!(reaped_ptr, arg_addr.ptr() as u64);
                let reaped_urb: UsbdevfsUrbHeader =
                    current_task.read_object(UserRef::new(arg_addr)).unwrap();
                assert_eq!(reaped_urb.status, killed_status, "Discarded URB must be killed");

                current_task.write_object(UserRef::new(arg_addr), &pending_urb_in).unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_SUBMITURB, arg_addr.into()).unwrap();
                file.ops()
                    .ioctl(&file, current_task, USBDEVFS_RESET, UserAddress::NULL.into())
                    .unwrap();
                file.ops().ioctl(&file, current_task, USBDEVFS_REAPURB, reap_addr.into()).unwrap();
                let reaped_urb: UsbdevfsUrbHeader =
                    current_task.read_object(UserRef::new(arg_addr)).unwrap();
                assert_eq!(reaped_urb.status, killed_status, "Reset must kill pending URBs");

                let clear_halt_ep: u32 = bulk_in_ep as u32;
                current_task.write_object(UserRef::new(arg_addr), &clear_halt_ep).unwrap();
                file.ops()
                    .ioctl(&file, current_task, USBDEVFS_CLEAR_HALT, arg_addr.into())
                    .unwrap();

                let expect_post_reset_out_fut = expect_bus_test.expect_out();
                let expected_post_reset_in = vec![0xaau8, 0xbb, 0xcc, 0xdd];
                let expect_post_reset_in_fut = expect_bus_test.expect_in(&expected_post_reset_in);
                expect_bus_test.sync().await.unwrap();

                let post_reset_out_payload = [0x11u8, 0x22, 0x33, 0x44];
                current_task.write_memory(data_addr, &post_reset_out_payload).unwrap();
                let post_reset_bulk_out = usbdevfs_bulktransfer {
                    ep: bulk_out_ep as u32,
                    len: post_reset_out_payload.len() as u32,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &post_reset_bulk_out).unwrap();
                let result =
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()).unwrap();
                assert_eq!(result, post_reset_out_payload.len().into());
                let received_post_reset_out = expect_post_reset_out_fut.await.unwrap().unwrap();
                assert_eq!(received_post_reset_out, post_reset_out_payload);

                let post_reset_bulk_in = usbdevfs_bulktransfer {
                    ep: bulk_in_ep as u32,
                    len: expected_post_reset_in.len() as u32,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &post_reset_bulk_in).unwrap();
                let result =
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()).unwrap();
                assert_eq!(result, expected_post_reset_in.len().into());
                assert_eq!(
                    current_task
                        .read_memory_to_vec(data_addr, expected_post_reset_in.len())
                        .unwrap(),
                    expected_post_reset_in
                );
                let actual_post_reset_in = expect_post_reset_in_fut.await.unwrap().unwrap();
                assert_eq!(actual_post_reset_in, expected_post_reset_in.len() as u64);

                // 7. Simulate USB device unplug while usbfs file and Endpoint channels are still open.
                assert_eq!(virtual_bus.disconnect().await.unwrap(), zx::sys::ZX_OK);
                wait_until_async("removal of /dev/bus/usb/001/002", || {
                    open("bus/usb/001/002").is_err()
                })
                .await;

                // Verify ioctl on the open file handle after physical disconnect returns ENODEV.
                assert_eq!(
                    file.ops().ioctl(&file, current_task, USBDEVFS_BULK, arg_addr.into()),
                    error!(ENODEV)
                );
                drop(file);

                // 8. Simulate a second attach cycle and verify re-enumeration and bulk I/O.
                assert_eq!(virtual_bus.connect().await.unwrap(), zx::sys::ZX_OK);
                wait_until_async("/dev/bus/usb/001/003", || open("bus/usb/001/003").is_ok()).await;
                let file2 = open("bus/usb/001/003").unwrap();
                assert_eq!(file2.name.entry.node.info().rdev.major(), USB_DEVICE_MAJOR);
                assert_eq!(file2.name.entry.node.info().rdev.minor(), 2);

                current_task.write_object(UserRef::new(arg_addr), &disconnect_claim_arg).unwrap();
                file2
                    .ops()
                    .ioctl(&file2, current_task, USBDEVFS_DISCONNECT_CLAIM, arg_addr.into())
                    .unwrap();

                let expect_replug_out_fut = expect_bus_test.expect_out();
                expect_bus_test.sync().await.unwrap();
                let replug_payload = [0x99u8, 0x88, 0x77, 0x66];
                current_task.write_memory(data_addr, &replug_payload).unwrap();
                let replug_bulk_out = usbdevfs_bulktransfer {
                    ep: bulk_out_ep as u32,
                    len: replug_payload.len() as u32,
                    timeout: 5000,
                    __bindgen_padding_0: [0; 4],
                    data: uaddr { addr: data_addr.ptr() as u64 },
                };
                current_task.write_object(UserRef::new(arg_addr), &replug_bulk_out).unwrap();
                let result = file2
                    .ops()
                    .ioctl(&file2, current_task, USBDEVFS_BULK, arg_addr.into())
                    .unwrap();
                assert_eq!(result, replug_payload.len().into());
                let received_replug_out = expect_replug_out_fut.await.unwrap().unwrap();
                assert_eq!(received_replug_out, replug_payload);
                drop(file2);

                // Disconnect second attach and wait for /dev/bus/usb/001/003 removal.
                assert_eq!(virtual_bus.disconnect().await.unwrap(), zx::sys::ZX_OK);
                wait_until_async("removal of /dev/bus/usb/001/003", || {
                    open("bus/usb/001/003").is_err()
                })
                .await;

                // Cleanly tear down virtual-bus peripheral functions before realm exit.
                let (events_client, mut events_stream) =
                    fidl::endpoints::create_request_stream::<fusb_peripheral::EventsMarker>();
                peripheral.set_state_change_listener(events_client).unwrap();
                peripheral.clear_functions().await.unwrap();
                while let Some(Ok(req)) = events_stream.next().await {
                    if let fusb_peripheral::EventsRequest::FunctionsCleared { control_handle } = req
                    {
                        control_handle.shutdown();
                        break;
                    }
                }

                // Stop the device watcher task and await its completion before disabling the bus.
                abort_handle.abort();
                let _ = done_rx.await;

                assert_eq!(virtual_bus.disable().await.unwrap(), zx::sys::ZX_OK);
                realm.destroy().await.expect("Failed to destroy the test realm");
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }
}
