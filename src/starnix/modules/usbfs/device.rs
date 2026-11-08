// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{SynchronousProxy, create_endpoints};
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt, TaskMemoryAccessor};
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{
    CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, TaskFlags, ThreadLockupDetector,
    WaitCanceler, WaitQueue, Waiter, WaiterOptions,
};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{FileObject, FileOps, fileops_impl_noop_sync, fileops_impl_seekable};
use starnix_logging::track_stub;
use starnix_sync::{
    LockDepMutex, UsbfsControlSlotLock, UsbfsDeviceStateLock, UsbfsEndpointStateLock,
    UsbfsEndpointTransferLock, UsbfsEndpointsLock, UsbfsFileUrbsLock, UsbfsOpenFilesLock,
};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::errors::{ECONNRESET, EINTR, ENODEV, ENOENT, ESHUTDOWN, ETIMEDOUT, Errno};
use starnix_uapi::signals::SigSet;
use starnix_uapi::user_address::{ArchSpecific, MultiArchUserRef, UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    _IOC_DIRSHIFT, _IOC_SIZEMASK, _IOC_SIZESHIFT, _IOC_WRITE, USB_CLASS_PRINTER, USB_DIR_IN,
    USB_DIR_OUT, USB_DT_CONFIG, USB_DT_ENDPOINT, USB_DT_ENDPOINT_SIZE, USB_DT_INTERFACE,
    USB_DT_INTERFACE_ASSOCIATION, USB_DT_SS_ENDPOINT_COMP, USB_ENDPOINT_HALT,
    USB_ENDPOINT_MAXP_MASK, USB_ENDPOINT_NUMBER_MASK, USB_ENDPOINT_XFER_BULK,
    USB_ENDPOINT_XFER_CONTROL, USB_ENDPOINT_XFER_INT, USB_ENDPOINT_XFER_ISOC,
    USB_ENDPOINT_XFERTYPE_MASK, USB_RECIP_ENDPOINT, USB_RECIP_INTERFACE, USB_RECIP_MASK,
    USB_REQ_CLEAR_FEATURE, USB_TYPE_CLASS, USB_TYPE_MASK, USB_TYPE_STANDARD, USB_TYPE_VENDOR,
    USBDEVFS_BULK, USBDEVFS_CAP_BULK_CONTINUATION, USBDEVFS_CAP_NO_PACKET_SIZE_LIM,
    USBDEVFS_CAP_REAP_AFTER_DISCONNECT, USBDEVFS_CAP_ZERO_PACKET, USBDEVFS_CLAIMINTERFACE,
    USBDEVFS_CLEAR_HALT, USBDEVFS_CONNECT, USBDEVFS_CONNECTINFO, USBDEVFS_CONTROL,
    USBDEVFS_DISCARDURB, USBDEVFS_DISCONNECT, USBDEVFS_DISCONNECT_CLAIM,
    USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER, USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
    USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_SPEED, USBDEVFS_GETDRIVER, USBDEVFS_IOCTL,
    USBDEVFS_REAPURB, USBDEVFS_REAPURBNDELAY, USBDEVFS_RELEASEINTERFACE, USBDEVFS_RESET,
    USBDEVFS_SETCONFIGURATION, USBDEVFS_SETINTERFACE, USBDEVFS_SUBMITURB,
    USBDEVFS_URB_BULK_CONTINUATION, USBDEVFS_URB_ISO_ASAP, USBDEVFS_URB_NO_FSBR,
    USBDEVFS_URB_NO_INTERRUPT, USBDEVFS_URB_SHORT_NOT_OK, USBDEVFS_URB_TYPE_BULK,
    USBDEVFS_URB_TYPE_CONTROL, USBDEVFS_URB_TYPE_INTERRUPT, USBDEVFS_URB_TYPE_ISO,
    USBDEVFS_URB_ZERO_PACKET, errno, error, uapi, usb_config_descriptor, usb_ctrlrequest,
    usb_device_descriptor, usb_device_speed_USB_SPEED_FULL, usb_device_speed_USB_SPEED_HIGH,
    usb_device_speed_USB_SPEED_LOW, usb_device_speed_USB_SPEED_SUPER,
    usb_device_speed_USB_SPEED_SUPER_PLUS, usb_device_speed_USB_SPEED_UNKNOWN,
    usb_device_speed_USB_SPEED_WIRELESS, usb_interface_descriptor, usb_ss_ep_comp_descriptor,
    usbdevfs_bulktransfer, usbdevfs_connectinfo, usbdevfs_ctrltransfer, usbdevfs_disconnect_claim,
    usbdevfs_getdriver, usbdevfs_ioctl, usbdevfs_setinterface,
};
use std::cmp::min;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::mem::{offset_of, size_of};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

type UsbdevfsCtrltransferPtr =
    MultiArchUserRef<usbdevfs_ctrltransfer, uapi::arch32::usbdevfs_ctrltransfer>;
type UsbdevfsBulktransferPtr =
    MultiArchUserRef<usbdevfs_bulktransfer, uapi::arch32::usbdevfs_bulktransfer>;
type UsbdevfsIoctlPtr = MultiArchUserRef<usbdevfs_ioctl, uapi::arch32::usbdevfs_ioctl>;

/// 32-bit compat USBDEVFS ioctl request numbers and their native equivalents. On architectures
/// without 32-bit support (e.g. x86_64), `uapi::arch32` aliases the native definitions, so each
/// entry maps a request number to itself.
const ARCH32_IOCTL_REQUESTS: [(u32, u32); 7] = [
    (uapi::arch32::USBDEVFS_CONTROL, uapi::USBDEVFS_CONTROL),
    (uapi::arch32::USBDEVFS_BULK, uapi::USBDEVFS_BULK),
    (uapi::arch32::USBDEVFS_DISCSIGNAL, uapi::USBDEVFS_DISCSIGNAL),
    (uapi::arch32::USBDEVFS_SUBMITURB, uapi::USBDEVFS_SUBMITURB),
    (uapi::arch32::USBDEVFS_REAPURB, uapi::USBDEVFS_REAPURB),
    (uapi::arch32::USBDEVFS_REAPURBNDELAY, uapi::USBDEVFS_REAPURBNDELAY),
    (uapi::arch32::USBDEVFS_IOCTL, uapi::USBDEVFS_IOCTL),
];

/// Returns the native request number of `request`, which is a 32-bit compat request number if
/// `is_arch32` is set.
pub(crate) fn canonicalize_ioctl_request(is_arch32: bool, request: u32) -> u32 {
    if !is_arch32 {
        return request;
    }
    ARCH32_IOCTL_REQUESTS
        .iter()
        .find(|&&(compat, _)| compat == request)
        .map_or(request, |&(_, native)| native)
}

/// Returns whether `request` is the number of a 32-bit compat USBDEVFS ioctl. Like Linux, these
/// are honored for 64-bit tasks too. On architectures without 32-bit support (e.g. x86_64),
/// `uapi::arch32` aliases the native definitions, so no request number is a compat request number
/// there.
pub(crate) fn is_arch32_ioctl_request(request: u32) -> bool {
    canonicalize_ioctl_request(true, request) != request
}

/// Header of the 64-bit `struct usbdevfs_urb`, without the trailing isochronous packet
/// descriptors.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct UsbdevfsUrbHeader {
    pub(crate) type_: u8,
    pub(crate) endpoint: u8,
    pub(crate) _pad0: [u8; 2],
    pub(crate) status: i32,
    pub(crate) flags: u32,
    pub(crate) _pad1: [u8; 4],
    pub(crate) buffer: u64,
    pub(crate) buffer_length: i32,
    pub(crate) actual_length: i32,
    pub(crate) start_frame: i32,
    pub(crate) number_of_packets_or_stream_id: u32,
    pub(crate) error_count: i32,
    pub(crate) signr: u32,
    pub(crate) usercontext: u64,
}

/// Header of the 32-bit compat `struct usbdevfs_urb`, without the trailing isochronous packet
/// descriptors.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct UsbdevfsUrb32Header {
    pub(crate) type_: u8,
    pub(crate) endpoint: u8,
    pub(crate) _pad0: [u8; 2],
    pub(crate) status: i32,
    pub(crate) flags: u32,
    pub(crate) buffer: u32,
    pub(crate) buffer_length: i32,
    pub(crate) actual_length: i32,
    pub(crate) start_frame: i32,
    pub(crate) number_of_packets_or_stream_id: u32,
    pub(crate) error_count: i32,
    pub(crate) signr: u32,
    pub(crate) usercontext: u32,
}

/// Fields of a `struct usbdevfs_urb` used to submit it, from either layout.
struct UrbHeader {
    type_: u8,
    endpoint: u8,
    flags: u32,
    buffer: UserAddress,
    buffer_length: i32,
    signr: u32,
}

impl From<UsbdevfsUrbHeader> for UrbHeader {
    fn from(urb: UsbdevfsUrbHeader) -> Self {
        Self {
            type_: urb.type_,
            endpoint: urb.endpoint,
            flags: urb.flags,
            buffer: UserAddress::from(urb.buffer),
            buffer_length: urb.buffer_length,
            signr: urb.signr,
        }
    }
}

impl From<UsbdevfsUrb32Header> for UrbHeader {
    fn from(urb: UsbdevfsUrb32Header) -> Self {
        Self {
            type_: urb.type_,
            endpoint: urb.endpoint,
            flags: urb.flags,
            buffer: UserAddress::from(u64::from(urb.buffer)),
            buffer_length: urb.buffer_length,
            signr: urb.signr,
        }
    }
}

/// Reads the header of the `struct usbdevfs_urb` at `addr`, in the 32-bit layout if `is_32bit`.
fn read_urb_header(
    current_task: &CurrentTask,
    addr: UserAddress,
    is_32bit: bool,
) -> Result<UrbHeader, Errno> {
    Ok(if is_32bit {
        current_task.read_object::<UsbdevfsUrb32Header>(UserRef::new(addr))?.into()
    } else {
        current_task.read_object::<UsbdevfsUrbHeader>(UserRef::new(addr))?.into()
    })
}

/// Writes the result of a completed URB to its `struct usbdevfs_urb` at `addr`. Like Linux, only
/// `status`, `actual_length` and `error_count` are written, so concurrent changes to the other
/// fields by userspace are preserved.
fn write_urb_result(
    current_task: &CurrentTask,
    addr: UserAddress,
    is_32bit: bool,
    status: i32,
    actual_length: i32,
) -> Result<(), Errno> {
    let (status_offset, actual_length_offset, error_count_offset) = if is_32bit {
        (
            offset_of!(UsbdevfsUrb32Header, status),
            offset_of!(UsbdevfsUrb32Header, actual_length),
            offset_of!(UsbdevfsUrb32Header, error_count),
        )
    } else {
        (
            offset_of!(UsbdevfsUrbHeader, status),
            offset_of!(UsbdevfsUrbHeader, actual_length),
            offset_of!(UsbdevfsUrbHeader, error_count),
        )
    };
    let write_field = |offset: usize, value: i32| -> Result<(), Errno> {
        current_task.write_object(UserRef::<i32>::new((addr + offset)?), &value)?;
        Ok(())
    };
    write_field(status_offset, status)?;
    write_field(actual_length_offset, actual_length)?;
    // Isochronous URBs are not supported, so no packet ever fails.
    write_field(error_count_offset, 0)
}

/// Returns the negated errno code reported in the `status` of a completed URB.
fn urb_status(errno: &Errno) -> i32 {
    -(errno.code.error_code() as i32)
}

pub(crate) const USB_SPEED_UNKNOWN: u32 = usb_device_speed_USB_SPEED_UNKNOWN;
pub(crate) const USB_SPEED_LOW: u32 = usb_device_speed_USB_SPEED_LOW;
pub(crate) const USB_SPEED_FULL: u32 = usb_device_speed_USB_SPEED_FULL;
pub(crate) const USB_SPEED_HIGH: u32 = usb_device_speed_USB_SPEED_HIGH;
pub(crate) const USB_SPEED_WIRELESS: u32 = usb_device_speed_USB_SPEED_WIRELESS;
pub(crate) const USB_SPEED_SUPER: u32 = usb_device_speed_USB_SPEED_SUPER;
pub(crate) const USB_SPEED_SUPER_PLUS: u32 = usb_device_speed_USB_SPEED_SUPER_PLUS;

pub(crate) const MAX_USBDEVFS_CONTROL_SIZE: usize = 4096;

/// Size of the setup packet at the start of the buffer of a control URB.
const USB_SETUP_PACKET_SIZE: usize = size_of::<usb_ctrlrequest>();

/// Approximate size of the Linux `struct urb` on 64-bit architectures (184 bytes without vendor
/// padding). usbfs charges it to the memory used by each transfer, on top of the data buffer.
const URB_SIZE: usize = 184;

/// Memory available to the outstanding transfers of usbfs (the default `usbfs_memory_mb` of
/// 16 MiB).
const USBFS_MEMORY_LIMIT: usize = 16 * 1024 * 1024;

/// Largest data length of a transfer that fits in `USBFS_MEMORY_LIMIT`.
#[cfg(test)]
pub(crate) const MAX_USBDEVFS_BULK_SIZE: usize = USBFS_MEMORY_LIMIT - URB_SIZE;

/// Data lengths of `USBDEVFS_BULK` transfers from this value on are invalid (`EINVAL`), before
/// the memory limit is checked.
pub(crate) const INVALID_USBDEVFS_BULK_SIZE: usize = i32::MAX as usize - URB_SIZE;

/// Buffer lengths of URBs from this value on are invalid (`EINVAL`), before the memory limit is
/// checked.
pub(crate) const USBFS_XFER_MAX: usize = (u32::MAX / 2 - 1_000_000) as usize;

/// URB flags accepted for all URB types. Only `USBDEVFS_URB_SHORT_NOT_OK`,
/// `USBDEVFS_URB_BULK_CONTINUATION` and `USBDEVFS_URB_ZERO_PACKET` change the behavior of usbfs;
/// the others are scheduling hints.
const URB_FLAGS: u32 = USBDEVFS_URB_SHORT_NOT_OK
    | USBDEVFS_URB_BULK_CONTINUATION
    | USBDEVFS_URB_NO_FSBR
    | USBDEVFS_URB_ZERO_PACKET
    | USBDEVFS_URB_NO_INTERRUPT;

const ENDPOINT_VMO_SIZE: u64 = 65536;

/// Maximum number of retired VMOs of an endpoint, i.e. VMOs whose cancelled requests did not
/// complete in time. Transfers on the endpoint fail with `EIO` while this many VMOs still wait
/// for completions.
pub(crate) const MAX_RETIRED_VMOS: usize = 4;

/// Maximum number of interfaces supported by the `u64` interface claim bitset.
const MAX_USB_INTERFACES: u32 = u64::BITS;

/// Default upper bound on the time spent waiting for the completions of cancelled requests
/// before the endpoint VMO is retired.
pub(crate) const DEFAULT_CANCEL_DRAIN_TIMEOUT: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(1);

/// Timeout of the `CLEAR_FEATURE(ENDPOINT_HALT)` control transfers issued by usbfs.
const CLEAR_HALT_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(5000);

/// Driver name reported for interfaces claimed through usbfs. Starnix never binds kernel drivers
/// to USB interfaces, so usbfs is the only possible driver.
const USBFS_DRIVER_NAME: &[u8] = b"usbfs";

/// `bmRequestType` and `bRequest` of the printer class GET_DEVICE_ID request.
const USB_PRINTER_GET_DEVICE_ID_REQUEST_TYPE: u32 =
    USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
const USB_PRINTER_GET_DEVICE_ID: u8 = 0;

/// Offsets of the fields of a standard endpoint descriptor (USB 2.0 Table 9-13). The uapi
/// `usb_endpoint_descriptor` has the 9-byte audio layout, so the 7-byte descriptor is read field
/// by field.
const ENDPOINT_DESC_ADDRESS: usize = 2;
const ENDPOINT_DESC_ATTRIBUTES: usize = 3;
const ENDPOINT_DESC_MAX_PACKET_SIZE: usize = 4;
const ENDPOINT_DESC_INTERVAL: usize = 6;

/// Validated USB interface number in `0..MAX_USB_INTERFACES` (`0..64`),
/// suitable for indexing into a `u64` interface claim bitset.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Ifno(u8);

impl Ifno {
    fn as_u8(self) -> u8 {
        self.0
    }
}

impl TryFrom<u32> for Ifno {
    type Error = Errno;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value >= MAX_USB_INTERFACES { error!(EINVAL) } else { Ok(Self(value as u8)) }
    }
}

impl From<Ifno> for u32 {
    fn from(ifno: Ifno) -> Self {
        u32::from(ifno.0)
    }
}

/// SuperSpeed endpoint companion of an endpoint without one.
const NO_SS_COMPANION: fusb_descriptor::UsbSsEpCompDescriptor =
    fusb_descriptor::UsbSsEpCompDescriptor {
        b_length: 0,
        b_descriptor_type: 0,
        b_max_burst: 0,
        bm_attributes: 0,
        w_bytes_per_interval: 0,
    };

#[derive(Clone, Debug, PartialEq)]
struct ParsedEndpoint {
    descriptor: fusb_descriptor::UsbEndpointDescriptor,
    ss_companion: fusb_descriptor::UsbSsEpCompDescriptor,
}

impl ParsedEndpoint {
    fn address(&self) -> u8 {
        self.descriptor.b_endpoint_address
    }

    fn transfer_type(&self) -> u32 {
        u32::from(self.descriptor.bm_attributes) & USB_ENDPOINT_XFERTYPE_MASK
    }

    fn max_packet_size(&self) -> u32 {
        u32::from(self.descriptor.w_max_packet_size) & USB_ENDPOINT_MAXP_MASK
    }
}

#[derive(Clone, Debug)]
struct ParsedInterfaceAlt {
    interface_number: u8,
    alt_setting: u8,
    interface_class: u8,
    endpoints: Vec<ParsedEndpoint>,
}

#[derive(Clone, Debug)]
struct ParsedConfig {
    config_value: u8,
    interfaces: Vec<ParsedInterfaceAlt>,
}

/// Returns an iterator over the descriptors in `data`.
///
/// Per USB 2.0 Section 9.5, every USB descriptor begins with a 2-byte `[bLength, bDescriptorType]`
/// header followed by `bLength - 2` descriptor-specific bytes. Iteration stops at the first
/// descriptor that is truncated or has a `bLength` smaller than the 2-byte header.
fn iter_descriptors(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let length = usize::from(*rest.first()?);
        if length < 2 || length > rest.len() {
            return None;
        }
        let (descriptor, tail) = rest.split_at(length);
        rest = tail;
        Some(descriptor)
    })
}

fn parse_endpoint_descriptor(desc: &[u8]) -> Option<ParsedEndpoint> {
    if desc.len() < USB_DT_ENDPOINT_SIZE as usize {
        return None;
    }
    let descriptor = fusb_descriptor::UsbEndpointDescriptor {
        b_length: desc[0],
        b_descriptor_type: desc[1],
        b_endpoint_address: desc[ENDPOINT_DESC_ADDRESS],
        bm_attributes: desc[ENDPOINT_DESC_ATTRIBUTES],
        w_max_packet_size: u16::from_le_bytes([
            desc[ENDPOINT_DESC_MAX_PACKET_SIZE],
            desc[ENDPOINT_DESC_MAX_PACKET_SIZE + 1],
        ]),
        b_interval: desc[ENDPOINT_DESC_INTERVAL],
    };
    Some(ParsedEndpoint { descriptor, ss_companion: NO_SS_COMPANION })
}

fn parse_ss_companion_descriptor(desc: &[u8]) -> Option<fusb_descriptor::UsbSsEpCompDescriptor> {
    let (comp, _) = usb_ss_ep_comp_descriptor::read_from_prefix(desc).ok()?;
    Some(fusb_descriptor::UsbSsEpCompDescriptor {
        b_length: comp.bLength,
        b_descriptor_type: comp.bDescriptorType,
        b_max_burst: comp.bMaxBurst,
        bm_attributes: comp.bmAttributes,
        w_bytes_per_interval: u16::from_le(comp.wBytesPerInterval),
    })
}

/// Parses the configurations, interfaces, and endpoints of the configuration descriptors in
/// `data`.
///
/// Per USB 2.0 Section 9.4.3 (`GET_DESCRIPTOR` for `CONFIGURATION`) and USB 3.2 Section 9.6.7, a
/// configuration descriptor blob is a flat byte stream of concatenated `[bLength, bDescriptorType,
/// ...]` descriptors ordered hierarchically:
/// - Configuration descriptor (`USB_DT_CONFIG`)
///   - Interface alternate setting descriptors (`USB_DT_INTERFACE`)
///     - Endpoint descriptors (`USB_DT_ENDPOINT`)
///       - Optional class-specific endpoint descriptors
///       - Optional SuperSpeed Endpoint Companion descriptor (`USB_DT_SS_ENDPOINT_COMP`)
///
/// `data` concatenates the blobs of all configurations of the device. Descriptors that are too
/// short are ignored, as are interfaces before the first configuration and endpoints before the
/// first interface.
fn parse_config_descriptors(data: &[u8]) -> Vec<ParsedConfig> {
    let mut configs: Vec<ParsedConfig> = Vec::new();
    // Whether the last descriptor was an endpoint that may be followed by its SuperSpeed
    // endpoint companion descriptor.
    let mut in_endpoint_scope = false;

    for desc in iter_descriptors(data) {
        let descriptor_type = u32::from(desc[1]);
        let mut is_endpoint = false;
        match descriptor_type {
            USB_DT_CONFIG => {
                if let Ok((config, _)) = usb_config_descriptor::read_from_prefix(desc) {
                    configs.push(ParsedConfig {
                        config_value: config.bConfigurationValue,
                        interfaces: Vec::new(),
                    });
                }
            }
            USB_DT_INTERFACE => {
                let interface = usb_interface_descriptor::read_from_prefix(desc).ok();
                if let (Some((interface, _)), Some(config)) = (interface, configs.last_mut()) {
                    config.interfaces.push(ParsedInterfaceAlt {
                        interface_number: interface.bInterfaceNumber,
                        alt_setting: interface.bAlternateSetting,
                        interface_class: interface.bInterfaceClass,
                        endpoints: Vec::new(),
                    });
                }
            }
            USB_DT_ENDPOINT => {
                let interface = configs.last_mut().and_then(|c| c.interfaces.last_mut());
                if let (Some(endpoint), Some(interface)) =
                    (parse_endpoint_descriptor(desc), interface)
                {
                    interface.endpoints.push(endpoint);
                    is_endpoint = true;
                }
            }
            USB_DT_SS_ENDPOINT_COMP if in_endpoint_scope => {
                let endpoint = configs
                    .last_mut()
                    .and_then(|c| c.interfaces.last_mut())
                    .and_then(|i| i.endpoints.last_mut());
                if let (Some(companion), Some(endpoint)) =
                    (parse_ss_companion_descriptor(desc), endpoint)
                {
                    endpoint.ss_companion = companion;
                }
            }
            USB_DT_INTERFACE_ASSOCIATION => {}
            // Class-specific descriptors may follow an endpoint before its companion.
            _ => is_endpoint = in_endpoint_scope,
        }
        in_endpoint_scope = is_endpoint;
    }
    configs
}

fn zx_status_to_errno(status: i32) -> Errno {
    match status {
        zx::sys::ZX_ERR_IO_REFUSED => errno!(EPIPE),
        zx::sys::ZX_ERR_TIMED_OUT => errno!(ETIMEDOUT),
        zx::sys::ZX_ERR_NOT_FOUND => errno!(ENOENT),
        zx::sys::ZX_ERR_ALREADY_BOUND => errno!(EBUSY),
        zx::sys::ZX_ERR_IO_NOT_PRESENT | zx::sys::ZX_ERR_PEER_CLOSED => errno!(ENODEV),
        zx::sys::ZX_ERR_INVALID_ARGS => errno!(EINVAL),
        zx::sys::ZX_ERR_BAD_STATE => errno!(EHOSTUNREACH),
        zx::sys::ZX_ERR_CANCELED => errno!(ECONNRESET),
        zx::sys::ZX_ERR_NO_MEMORY => errno!(ENOMEM),
        _ => errno!(EIO),
    }
}

/// Maps a FIDL transport error to an errno: `ENODEV` if the channel is closed (the device is
/// gone), `EIO` otherwise.
fn fidl_error_to_errno(error: &fidl::Error) -> Errno {
    if error.is_closed() { errno!(ENODEV) } else { errno!(EIO) }
}

/// Maps the two-layer result of a FIDL method with an `error zx.Status` clause to an errno.
fn fidl_result_to_errno<T>(result: Result<Result<T, i32>, fidl::Error>) -> Result<T, Errno> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(status)) => Err(zx_status_to_errno(status)),
        Err(e) => Err(fidl_error_to_errno(&e)),
    }
}

/// Converts an optional timeout (`None` meaning no timeout) into an absolute deadline.
fn deadline_after(timeout: Option<zx::MonotonicDuration>) -> zx::MonotonicInstant {
    timeout.map_or(zx::MonotonicInstant::INFINITE, zx::MonotonicInstant::after)
}

/// Converts a usbfs timeout in milliseconds into an optional timeout. A timeout of 0 means no
/// timeout.
fn usbfs_timeout(timeout_ms: u32) -> Option<zx::MonotonicDuration> {
    (timeout_ms != 0).then(|| zx::MonotonicDuration::from_millis(i64::from(timeout_ms)))
}

/// Blocks the current task until any of the signals in `waits` is asserted on its handle, or
/// `deadline` passes.
///
/// The wait is killable, like Linux usbfs transfers: only unblockable signals (SIGKILL, SIGSTOP)
/// interrupt it, in which case `EINTR` is returned. Other signals stay pending and are delivered
/// when the syscall returns. Returns `ETIMEDOUT` when the deadline passes.
fn wait_for_signals(
    current_task: &CurrentTask,
    waits: &[(&dyn zx::AsHandleRef, zx::Signals)],
    deadline: zx::MonotonicInstant,
) -> Result<(), Errno> {
    // Block all the maskable signals for the duration of the wait, unless a temporary signal mask
    // is already in place.
    //
    // TODO(https://fxbug.dev/573607334): On Linux, an unblocked signal whose default action
    // terminates the process (e.g. SIGTERM with SIG_DFL, but not core-dumping signals, and not
    // when ptraced) is promoted to SIGKILL on delivery, which ends a killable wait. This mask
    // prevents that for single-threaded processes waiting without timeout (multi-threaded ones
    // still exit through another thread). Fixing it requires a killable wait primitive in the
    // Starnix core, since a core promotion honoring the current mask would not see through this
    // temporary one.
    let mask_signals = !current_task.flags().contains(TaskFlags::TEMPORARY_SIGNAL_MASK);
    if mask_signals {
        current_task.write().set_temporary_signal_mask(!SigSet::default());
    }
    let result = wait_for_signals_unmasked(current_task, waits, deadline);
    if mask_signals {
        // Signals received during the wait are delivered with the original mask.
        current_task.write().restore_signal_mask();
    }
    result
}

fn wait_for_signals_unmasked(
    current_task: &CurrentTask,
    waits: &[(&dyn zx::AsHandleRef, zx::Signals)],
    deadline: zx::MonotonicInstant,
) -> Result<(), Errno> {
    loop {
        // `PortWaiter::wait_until` automatically invokes `current_task.trigger_delayed_releaser()`
        // unless `WaiterOptions::UNSAFE_CALLSTACK` is set. Callers of `wait_for_signals` hold
        // `UsbfsEndpointTransferLock`, and flushing a closed `UsbDeviceFile` queued in the
        // thread-local delayed releaser runs `UsbDeviceFile::drop`, which acquires
        // `UsbfsDeviceStateLock` (ordered before `UsbfsEndpointTransferLock`).
        let waiter = Waiter::with_options(WaiterOptions::UNSAFE_CALLSTACK);
        let mut cancelers = Vec::with_capacity(waits.len());
        for &(handle, signals) in waits {
            let handler = SignalHandler {
                inner: SignalHandlerInner::None,
                event_handler: EventHandler::None,
                err_code: None,
            };
            cancelers.push(
                waiter.wake_on_zircon_signals(handle, signals, handler).map_err(|_| errno!(EIO))?,
            );
        }
        // The wait is edge-triggered, so check for signals asserted before it was registered.
        let asserted = waits.iter().any(|&(handle, signals)| {
            handle
                .as_handle_ref()
                .wait_one(signals, zx::MonotonicInstant::INFINITE_PAST)
                .to_result()
                .is_ok()
        });
        let result = if asserted { Ok(()) } else { waiter.wait_until(current_task, deadline) };
        for canceler in cancelers {
            canceler.cancel();
        }
        match result {
            // Spurious wakeup without a pending signal.
            Err(e)
                if e.code == EINTR
                    && !current_task.is_interrupt_pending()
                    && !current_task.is_exitted() =>
            {
                continue;
            }
            result => return result,
        }
    }
}

/// Kind of transfer issued by `USBDEVFS_BULK` on an endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferType {
    Bulk,
    Interrupt,
}

impl TransferType {
    fn request_info(self) -> fusb_request::RequestInfo {
        match self {
            Self::Bulk => fusb_request::RequestInfo::Bulk(fusb_request::BulkRequestInfo::default()),
            Self::Interrupt => {
                fusb_request::RequestInfo::Interrupt(fusb_request::InterruptRequestInfo::default())
            }
        }
    }
}

/// How `USBDEVFS_BULK` transfers are issued on an endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EndpointTransferParams {
    transfer_type: TransferType,
    /// `wMaxPacketSize` of the endpoint.
    max_packet_size: usize,
    /// Size of the requests a transfer is split into: the largest multiple of the endpoint's
    /// `wMaxPacketSize` that fits in the endpoint VMO, so that only the last request of a
    /// transfer can end with a short packet.
    chunk_size: usize,
}

impl EndpointTransferParams {
    /// Returns the parameters of `endpoint`, or `EINVAL` if the endpoint is neither a bulk nor
    /// an interrupt endpoint, or has a zero `wMaxPacketSize`.
    fn for_endpoint(endpoint: &ParsedEndpoint) -> Result<Self, Errno> {
        let max_packet_size = endpoint.max_packet_size() as usize;
        if max_packet_size == 0 {
            return error!(EINVAL);
        }
        let transfer_type = match endpoint.transfer_type() {
            USB_ENDPOINT_XFER_BULK => TransferType::Bulk,
            USB_ENDPOINT_XFER_INT => TransferType::Interrupt,
            _ => return error!(EINVAL),
        };
        let vmo_size = ENDPOINT_VMO_SIZE as usize;
        Ok(Self {
            transfer_type,
            max_packet_size,
            chunk_size: vmo_size - vmo_size % max_packet_size,
        })
    }
}

/// Outcome of a transfer that may have moved some data before failing.
#[derive(Debug)]
struct PartialTransfer<T> {
    /// Data or number of bytes transferred, even if the transfer failed.
    value: T,
    error: Option<Errno>,
}

impl<T> PartialTransfer<T> {
    fn ok(value: T) -> Self {
        Self { value, error: None }
    }

    fn failed(value: T, error: Errno) -> Self {
        Self { value, error: Some(error) }
    }

    /// Returns the value of a successful transfer, or the error of a failed one. Like Linux
    /// `USBDEVFS_BULK`, the data transferred before a failure is discarded.
    fn into_result(self) -> Result<T, Errno> {
        match self.error {
            None => Ok(self.value),
            Some(error) => Err(error),
        }
    }
}

/// Returns the setup packet of a `CLEAR_FEATURE(ENDPOINT_HALT)` request for endpoint `ep_addr`.
fn clear_halt_setup(ep_addr: u8) -> fusb_descriptor::UsbSetup {
    fusb_descriptor::UsbSetup {
        bm_request_type: (USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT) as u8,
        b_request: USB_REQ_CLEAR_FEATURE as u8,
        w_value: USB_ENDPOINT_HALT as u16,
        w_index: u16::from(ep_addr),
        w_length: 0,
    }
}

/// Returns the ID of the VMO used by the request of `completion`, if known.
fn completion_vmo_id(completion: &fusb_endpoint::Completion) -> Option<u64> {
    match completion.request.as_ref()?.data.as_ref()?.first()?.buffer {
        Some(fusb_request::Buffer::VmoId(id)) => Some(id),
        _ => None,
    }
}

/// State of an endpoint protected by its transfer lock.
struct EndpointTransferState {
    /// VMO used by the requests, at offset 0. At most one request is in flight at a time.
    vmo: zx::Vmo,
    vmo_id: u64,
    /// Number of requests queued with `vmo_id` whose completion has not been received yet.
    in_flight_count: usize,
    /// Number of requests whose completion has not been received yet, for each retired VMO.
    ///
    /// When the completion of a cancelled request does not arrive in time, its VMO is retired
    /// and replaced by a new one, since the request may still complete and write to it later.
    /// The completions of the requests of retired VMOs are discarded, and a retired VMO is
    /// unregistered once all of them have been received.
    retired_vmos: HashMap<u64, usize>,
    next_vmo_id: u64,
}

/// Lifecycle and reset state of an endpoint, protected by `UsbEndpointClient::state`.
///
/// Locking this state around both request submission (`queue_requests`) and endpoint disabling or
/// reset (`cancel_all`) ensures a transfer cannot queue a new request onto an endpoint after
/// `cancel_all` has already run for that disable or reset.
struct EndpointClientState {
    /// Whether the endpoint is enabled and new requests may be queued. Cleared before the
    /// in-flight requests are cancelled when the endpoint is disabled.
    usable: bool,
    /// Whether the endpoint is currently being disabled and immediately re-enabled by `clear_halt`
    /// while URBs are pending on it. Subsequent transfers wait in `acquire_transfer` until
    /// restarting finishes rather than failing with `ESHUTDOWN`.
    restarting: bool,
    /// Whether the driver closed the channel. The driver disables the endpoint before closing
    /// the channel, so a new connection can be made right away.
    closed: bool,
    /// Replacement client connected when this client's channel was closed while restarting the
    /// endpoint. Transfers waiting in `acquire_transfer` switch to this client.
    replaced_by: Option<Arc<UsbEndpointClient>>,
    /// Generation count incremented on each reset or cancellation of the endpoint.
    reset_count: u64,
    /// Whether a transfer currently holds the endpoint's transfer slot.
    transfer_busy: bool,
    /// Number of transfers waiting in `acquire_transfer`.
    #[cfg(test)]
    transfer_waiters: usize,
    /// Optional hook invoked right before `begin_transfer` in tests.
    #[cfg(test)]
    on_before_begin_transfer: Option<Box<dyn FnOnce() + Send>>,
}

/// Connection to an endpoint of the device.
///
/// The channel stays open for the lifetime of the device: closing it makes the host controller
/// cancel the requests and disable the endpoint asynchronously, which would race with a later
/// enable of the endpoint. Only the driver closes it, after disabling the endpoint.
pub(crate) struct UsbEndpointClient {
    proxy: fusb_endpoint::EndpointSynchronousProxy,
    /// Protects the endpoint's VMO and completion accounting for the active transfer. Locked only
    /// after claiming `EndpointClientState::transfer_busy` via `acquire_transfer`.
    transfer: LockDepMutex<EndpointTransferState, UsbfsEndpointTransferLock>,
    /// Protects the endpoint's enabled/closed status, reset generation, and transfer slot across
    /// request submission and cancellation. Ordered after `UninterruptibleLock`,
    /// `UsbfsDeviceStateLock`, `UsbfsEndpointTransferLock`, and `UsbfsEndpointsLock`.
    state: LockDepMutex<EndpointClientState, UsbfsEndpointStateLock>,
    /// Signaled with `USER_0` when `EndpointClientState::transfer_busy` becomes false.
    transfer_released: zx::Event,
    /// Signaled with `USER_0` to make a transfer waiting for a completion check whether it
    /// should stop, when the endpoint is disabled or reset.
    wake: zx::Event,
    /// Upper bound on the time spent draining cancelled requests.
    cancel_drain_timeout: zx::MonotonicDuration,
}

struct EndpointTransferSlotGuard<'a> {
    client: &'a UsbEndpointClient,
}

impl Drop for EndpointTransferSlotGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.client.state.lock();
        state.transfer_busy = false;
        let _ = self.client.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }
}

enum AcquiredTransfer<'a> {
    Slot(EndpointTransferSlotGuard<'a>, u64),
    Replaced(Arc<UsbEndpointClient>),
}

#[cfg(test)]
struct TransferWaiterCountGuard<'a>(&'a UsbEndpointClient);

#[cfg(test)]
impl Drop for TransferWaiterCountGuard<'_> {
    fn drop(&mut self) {
        self.0.state.lock().transfer_waiters -= 1;
    }
}

#[cfg(test)]
impl UsbEndpointClient {
    fn count_transfer_waiter<'a>(
        &'a self,
        state: &mut EndpointClientState,
    ) -> TransferWaiterCountGuard<'a> {
        state.transfer_waiters += 1;
        TransferWaiterCountGuard(self)
    }
}

#[cfg(not(test))]
impl UsbEndpointClient {
    #[inline(always)]
    fn count_transfer_waiter(&self, _state: &mut EndpointClientState) {}
}

impl UsbEndpointClient {
    /// Connects to endpoint `ep_addr` of the device behind `proxy` and registers its VMO.
    fn connect(
        proxy: &fhardware_usb::UsbSynchronousProxy,
        ep_addr: u8,
        cancel_drain_timeout: zx::MonotonicDuration,
    ) -> Result<Arc<Self>, Errno> {
        let (ep_client_end, ep_server_end) = create_endpoints::<fusb_endpoint::EndpointMarker>();
        fidl_result_to_errno(proxy.connect_to_endpoint(
            ep_addr,
            ep_server_end,
            zx::MonotonicInstant::INFINITE,
        ))?;
        let ep_proxy = fusb_endpoint::EndpointSynchronousProxy::new(ep_client_end.into_channel());
        let vmo_id = 1;
        let vmo = Self::register_vmo(&ep_proxy, vmo_id)?;
        Ok(Arc::new(Self {
            proxy: ep_proxy,
            transfer: LockDepMutex::new(EndpointTransferState {
                vmo,
                vmo_id,
                in_flight_count: 0,
                retired_vmos: HashMap::new(),
                next_vmo_id: vmo_id + 1,
            }),
            state: LockDepMutex::new(EndpointClientState {
                usable: false,
                restarting: false,
                closed: false,
                replaced_by: None,
                reset_count: 0,
                transfer_busy: false,
                #[cfg(test)]
                transfer_waiters: 0,
                #[cfg(test)]
                on_before_begin_transfer: None,
            }),
            transfer_released: zx::Event::create(),
            wake: zx::Event::create(),
            cancel_drain_timeout,
        }))
    }

    fn register_vmo(
        proxy: &fusb_endpoint::EndpointSynchronousProxy,
        vmo_id: u64,
    ) -> Result<zx::Vmo, Errno> {
        let vmo_infos = [fusb_endpoint::VmoInfo {
            id: Some(vmo_id),
            size: Some(ENDPOINT_VMO_SIZE),
            ..Default::default()
        }];
        let mut vmos = proxy
            .register_vmos(&vmo_infos, zx::MonotonicInstant::INFINITE)
            .map_err(|e| fidl_error_to_errno(&e))?;
        vmos.pop().and_then(|h| h.vmo).ok_or_else(|| errno!(EIO))
    }

    fn is_closed(&self) -> bool {
        let mut state = self.state.lock();
        if !state.closed
            && self
                .proxy
                .as_channel()
                .wait_one(zx::Signals::CHANNEL_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
                .to_result()
                .is_ok()
        {
            state.usable = false;
            state.closed = true;
            let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
        }
        state.closed
    }

    fn enable(&self) {
        let mut state = self.state.lock();
        state.usable = true;
        state.restarting = false;
        let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    /// Makes a transfer waiting for a completion on this endpoint check whether it should stop.
    fn wake_transfer(&self) {
        let _ = self.wake.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    /// Marks the endpoint as disabled and cancels its in-flight requests under the state lock, so
    /// no concurrent transfer can queue a request after `cancel_all` runs.
    fn disable(&self) {
        let mut state = self.state.lock();
        state.usable = false;
        state.restarting = false;
        state.reset_count += 1;
        let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
        self.wake_transfer();
        if let Err(e) =
            self.proxy.cancel_all(zx::MonotonicInstant::after(self.cancel_drain_timeout))
        {
            if e.is_closed() {
                state.closed = true;
            }
        }
    }

    /// Cancels in-flight requests and holds new transfers in `acquire_transfer` while the endpoint
    /// is disabled and immediately re-enabled on the host controller.
    fn begin_restart(&self) {
        let mut state = self.state.lock();
        state.restarting = true;
        state.reset_count += 1;
        self.wake_transfer();
        if let Err(e) =
            self.proxy.cancel_all(zx::MonotonicInstant::after(self.cancel_drain_timeout))
        {
            if e.is_closed() {
                state.usable = false;
                state.closed = true;
                let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
            }
        }
    }

    /// Releases transfers waiting in `acquire_transfer` after the endpoint has been re-enabled on
    /// the host controller.
    fn finish_restart(&self) {
        let mut state = self.state.lock();
        state.restarting = false;
        let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    /// Replaces this closed client with `replacement` and wakes transfers waiting in
    /// `acquire_transfer` so they switch to `replacement`.
    fn replace_with(&self, replacement: Arc<UsbEndpointClient>) {
        let mut state = self.state.lock();
        state.usable = false;
        state.restarting = false;
        state.closed = true;
        state.replaced_by = Some(replacement);
        state.reset_count += 1;
        let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
        self.wake_transfer();
    }

    /// Cancels the in-flight requests under the state lock. The transfer that queued them drains
    /// their completions.
    fn cancel_all(&self) {
        let mut state = self.state.lock();
        state.reset_count += 1;
        self.wake_transfer();
        if let Err(e) =
            self.proxy.cancel_all(zx::MonotonicInstant::after(self.cancel_drain_timeout))
        {
            if e.is_closed() {
                state.usable = false;
                state.closed = true;
                let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
            }
        }
    }

    /// Returns the error of a transfer stopped before or while waiting, if any: `ENOENT` if
    /// `cancel` is set, like a killed URB, or `ESHUTDOWN` if usbfs disabled or reset the
    /// endpoint.
    fn cancellation_error_locked(
        state: &EndpointClientState,
        cancel: Option<&UrbCancellation>,
        expected_reset: u64,
    ) -> Option<Errno> {
        if cancel.is_some_and(UrbCancellation::is_cancelled) {
            Some(errno!(ENOENT))
        } else if !state.usable || state.reset_count != expected_reset {
            Some(errno!(ESHUTDOWN))
        } else {
            None
        }
    }

    fn cancellation_error(
        &self,
        cancel: Option<&UrbCancellation>,
        expected_reset: u64,
    ) -> Option<Errno> {
        let state = self.state.lock();
        let error = Self::cancellation_error_locked(&state, cancel, expected_reset);
        if !state.usable || state.reset_count != expected_reset {
            // Re-assert `wake` so any other transfer waiting on this endpoint also observes the
            // disable or reset.
            self.wake_transfer();
        }
        error
    }

    /// Acquires exclusive ownership of the endpoint's transfer slot and records the endpoint's
    /// current `reset_count`, waiting until any in-flight transfer or endpoint restart finishes,
    /// the deadline expires, the calling task is interrupted by a fatal signal, `cancel` is
    /// triggered, or the endpoint is disabled.
    fn acquire_transfer(
        &self,
        current_task: &CurrentTask,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
    ) -> Result<AcquiredTransfer<'_>, Errno> {
        loop {
            let _waiter_guard = {
                let mut state = self.state.lock();
                if cancel.is_some_and(UrbCancellation::is_cancelled) {
                    return error!(ENOENT);
                }
                if !state.restarting {
                    if let Some(replacement) = state.replaced_by.as_ref().cloned() {
                        return Ok(AcquiredTransfer::Replaced(replacement));
                    }
                    if state.closed {
                        return error!(ENODEV);
                    }
                    if !state.usable {
                        return error!(ESHUTDOWN);
                    }
                    if !state.transfer_busy {
                        state.transfer_busy = true;
                        return Ok(AcquiredTransfer::Slot(
                            EndpointTransferSlotGuard { client: self },
                            state.reset_count,
                        ));
                    }
                }
                let _ = self.transfer_released.signal(zx::Signals::USER_0, zx::Signals::NONE);
                self.count_transfer_waiter(&mut state)
            };
            if cancel.is_some_and(UrbCancellation::is_cancelled) {
                return error!(ENOENT);
            }
            match cancel {
                Some(cancel) => wait_for_signals(
                    current_task,
                    &[
                        (&self.transfer_released, zx::Signals::USER_0),
                        (&cancel.wake, zx::Signals::USER_0),
                    ],
                    deadline,
                )?,
                None => wait_for_signals(
                    current_task,
                    &[(&self.transfer_released, zx::Signals::USER_0)],
                    deadline,
                )?,
            };
        }
    }

    fn mark_closed(&self) {
        let mut state = self.state.lock();
        state.usable = false;
        state.closed = true;
        let _ = self.transfer_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    /// Maps a channel error to an errno, recording whether the driver closed the channel.
    fn channel_error(&self, error: &fidl::Error) -> Errno {
        if error.is_closed() {
            self.mark_closed();
        }
        fidl_error_to_errno(error)
    }

    /// Accounts for `completion`, returning it if it is the completion of the request in flight
    /// on the current VMO.
    fn account_completion(
        &self,
        transfer: &mut EndpointTransferState,
        completion: fusb_endpoint::Completion,
    ) -> Option<fusb_endpoint::Completion> {
        match completion_vmo_id(&completion) {
            Some(id) if id != transfer.vmo_id => {
                if let Some(count) = transfer.retired_vmos.get_mut(&id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        transfer.retired_vmos.remove(&id);
                        let _ = self.proxy.unregister_vmos(&[id], zx::MonotonicInstant::INFINITE);
                    }
                }
                None
            }
            // Completions received while no request is in flight are stale.
            _ if transfer.in_flight_count == 0 => None,
            _ => {
                transfer.in_flight_count -= 1;
                Some(completion)
            }
        }
    }

    /// Accounts for the completions of the event `event`, returning the completion of the
    /// request in flight on the current VMO, if any.
    fn account_event(
        &self,
        transfer: &mut EndpointTransferState,
        event: fusb_endpoint::EndpointEvent,
    ) -> Option<fusb_endpoint::Completion> {
        let fusb_endpoint::EndpointEvent::OnCompletion { completion } = event;
        let mut current = None;
        for completion in completion {
            if let Some(completion) = self.account_completion(transfer, completion) {
                current = Some(completion);
            }
        }
        current
    }

    /// Accounts for the completions already received, without blocking.
    fn process_pending_events(&self, transfer: &mut EndpointTransferState) -> Result<(), Errno> {
        loop {
            match self.proxy.wait_for_event(zx::MonotonicInstant::INFINITE_PAST) {
                Ok(event) => {
                    self.account_event(transfer, event);
                }
                Err(fidl::Error::ClientEvent(zx::Status::TIMED_OUT)) => return Ok(()),
                Err(fidl::Error::UnexpectedSyncResponse) => continue,
                Err(e) => return Err(self.channel_error(&e)),
            }
        }
    }

    /// Replaces the VMO by a new one if requests are still in flight on it.
    fn retire_vmo_if_busy(&self, transfer: &mut EndpointTransferState) -> Result<(), Errno> {
        if transfer.in_flight_count == 0 {
            return Ok(());
        }
        if transfer.retired_vmos.len() >= MAX_RETIRED_VMOS {
            return error!(EIO);
        }
        let vmo_id = transfer.next_vmo_id;
        let vmo = Self::register_vmo(&self.proxy, vmo_id).inspect_err(|e| {
            if e.code == ENODEV {
                self.mark_closed();
            }
        })?;
        transfer.next_vmo_id += 1;
        transfer.retired_vmos.insert(transfer.vmo_id, transfer.in_flight_count);
        transfer.vmo = vmo;
        transfer.vmo_id = vmo_id;
        transfer.in_flight_count = 0;
        Ok(())
    }

    /// Checks that a transfer can start, and makes sure that no request is in flight on the
    /// VMO.
    fn begin_transfer(
        &self,
        transfer: &mut EndpointTransferState,
        cancel: Option<&UrbCancellation>,
        expected_reset: u64,
    ) -> Result<(), Errno> {
        {
            let state = self.state.lock();
            if state.closed {
                return error!(ENODEV);
            }
            if let Some(error) = Self::cancellation_error_locked(&state, cancel, expected_reset) {
                return Err(error);
            }
        }
        self.process_pending_events(transfer)?;
        self.retire_vmo_if_busy(transfer)
    }

    /// Waits for the completion of the request in flight on the current VMO.
    ///
    /// Returns the error of `cancellation_error` if the transfer is stopped while waiting, in
    /// which case the request is still in flight.
    fn wait_for_completion(
        &self,
        current_task: &CurrentTask,
        transfer: &mut EndpointTransferState,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
        expected_reset: u64,
    ) -> Result<fusb_endpoint::Completion, Errno> {
        loop {
            match self.proxy.wait_for_event(zx::MonotonicInstant::INFINITE_PAST) {
                Ok(event) => {
                    if let Some(completion) = self.account_event(transfer, event) {
                        return Ok(completion);
                    }
                    continue;
                }
                Err(fidl::Error::ClientEvent(zx::Status::TIMED_OUT)) => {}
                Err(fidl::Error::UnexpectedSyncResponse) => continue,
                Err(e) => return Err(self.channel_error(&e)),
            }
            // Clear the wake signal before checking the conditions it reports, so that a later
            // change is not missed.
            let _ = self.wake.signal(zx::Signals::USER_0, zx::Signals::NONE);
            if let Some(error) = self.cancellation_error(cancel, expected_reset) {
                return Err(error);
            }
            let ch_signals = zx::Signals::CHANNEL_READABLE | zx::Signals::CHANNEL_PEER_CLOSED;
            match cancel {
                Some(cancel) => wait_for_signals(
                    current_task,
                    &[
                        (self.proxy.as_channel(), ch_signals),
                        (&self.wake, zx::Signals::USER_0),
                        (&cancel.wake, zx::Signals::USER_0),
                    ],
                    deadline,
                )?,
                None => wait_for_signals(
                    current_task,
                    &[(self.proxy.as_channel(), ch_signals), (&self.wake, zx::Signals::USER_0)],
                    deadline,
                )?,
            };
        }
    }

    /// Cancels the request in flight and waits for its completion, which is returned if it
    /// arrives in time. Otherwise, the VMO is retired before the next transfer.
    fn cancel_and_drain(
        &self,
        transfer: &mut EndpointTransferState,
    ) -> Option<fusb_endpoint::Completion> {
        let deadline = zx::MonotonicInstant::after(self.cancel_drain_timeout);
        match self.proxy.cancel_all(deadline) {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return None,
            Err(e) => {
                let _ = self.channel_error(&e);
                return None;
            }
        }
        let mut current = None;
        while transfer.in_flight_count > 0 {
            match self.proxy.wait_for_event(deadline) {
                Ok(event) => {
                    if let Some(completion) = self.account_event(transfer, event) {
                        current = Some(completion);
                    }
                }
                Err(fidl::Error::UnexpectedSyncResponse) => continue,
                Err(e) => {
                    let _ = self.channel_error(&e);
                    break;
                }
            }
        }
        current
    }

    /// Queues one request of `len` bytes at offset 0 of the endpoint VMO and waits for its
    /// completion. Returns the number of bytes transferred, which never exceeds `len`.
    ///
    /// On timeout, interruption, or cancellation, the request is cancelled. If it nevertheless
    /// completed successfully, its result is returned. Otherwise, `ETIMEDOUT`, `EINTR`, or the
    /// error of `cancellation_error` is returned along with the number of bytes transferred
    /// before the cancellation. If the completion reports more than `len` bytes, `EOVERFLOW` is
    /// returned with `len` bytes.
    fn transfer_chunk(
        &self,
        current_task: &CurrentTask,
        transfer: &mut EndpointTransferState,
        transfer_type: TransferType,
        len: usize,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
        expected_reset: u64,
    ) -> PartialTransfer<usize> {
        let request = fusb_request::Request {
            data: Some(vec![fusb_request::BufferRegion {
                buffer: Some(fusb_request::Buffer::VmoId(transfer.vmo_id)),
                offset: Some(0),
                size: Some(len as u64),
                ..Default::default()
            }]),
            defer_completion: Some(false),
            information: Some(transfer_type.request_info()),
            ..Default::default()
        };
        // Hold the endpoint state lock while checking for cancellation and queueing the request so
        // that `disable` or `cancel_all` cannot run before `queue_requests` and leave the newly
        // queued request uncancelled on a disabled or reset endpoint.
        {
            let state = self.state.lock();
            if let Some(error) = Self::cancellation_error_locked(&state, cancel, expected_reset) {
                return PartialTransfer::failed(0, error);
            }
            if let Err(e) = self.proxy.queue_requests(vec![request]) {
                drop(state);
                return PartialTransfer::failed(0, self.channel_error(&e));
            }
        }
        transfer.in_flight_count += 1;
        let (completion, cancel_error) = match self.wait_for_completion(
            current_task,
            transfer,
            deadline,
            cancel,
            expected_reset,
        ) {
            Ok(completion) => (completion, None),
            Err(e)
                if e.code == ETIMEDOUT
                    || e.code == EINTR
                    || e.code == ENOENT
                    || e.code == ESHUTDOWN =>
            {
                match self.cancel_and_drain(transfer) {
                    // The request completed in hardware before `CancelAll` took effect: keep its
                    // actual completion status (whether `ZX_OK` or a hardware error like `EPIPE`).
                    Some(completion) if completion.status != Some(zx::sys::ZX_ERR_CANCELED) => {
                        (completion, None)
                    }
                    Some(completion) => (completion, Some(e)),
                    None => return PartialTransfer::failed(0, e),
                }
            }
            Err(e) => return PartialTransfer::failed(0, e),
        };
        let transferred = completion.transfer_size.unwrap_or(0);
        let Some(transferred) = usize::try_from(transferred).ok().filter(|&t| t <= len) else {
            // Like a babble error on Linux: the device sent more data than requested.
            return PartialTransfer::failed(len, errno!(EOVERFLOW));
        };
        if let Some(e) = cancel_error {
            return PartialTransfer::failed(transferred, e);
        }
        match completion.status.unwrap_or(zx::sys::ZX_ERR_IO) {
            zx::sys::ZX_OK => PartialTransfer::ok(transferred),
            // The cancelled completion of a request may be received before the transfer notices
            // why it was cancelled.
            zx::sys::ZX_ERR_CANCELED => PartialTransfer::failed(
                transferred,
                self.cancellation_error(cancel, expected_reset)
                    .unwrap_or_else(|| zx_status_to_errno(zx::sys::ZX_ERR_CANCELED)),
            ),
            status => PartialTransfer::failed(transferred, zx_status_to_errno(status)),
        }
    }

    /// Sends `data`, in requests of `params.chunk_size` bytes. If `zero_packet` is set and the
    /// length of `data` is a non-zero multiple of the endpoint's `wMaxPacketSize`, the transfer
    /// is terminated by a zero-length packet, without letting another transfer use the endpoint
    /// in between.
    ///
    /// Returns the number of bytes sent, which is short if the device accepted less than a full
    /// request. On failure, the number of bytes sent before the failure is returned with the
    /// error.
    fn bulk_out(
        &self,
        current_task: &CurrentTask,
        params: EndpointTransferParams,
        data: &[u8],
        zero_packet: bool,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
    ) -> PartialTransfer<usize> {
        // All transfers on this endpoint are serialized by `acquire_transfer` + `self.transfer`,
        // and `begin_transfer` / `cancel_and_drain` ensure no earlier request is still using
        // `transfer.vmo` (replacing it with a fresh VMO if a cancelled request did not drain in
        // time).
        let (_slot, expected_reset) = match self.acquire_transfer(current_task, deadline, cancel) {
            Ok(AcquiredTransfer::Slot(slot, expected_reset)) => (slot, expected_reset),
            Ok(AcquiredTransfer::Replaced(replacement)) => {
                return replacement.bulk_out(
                    current_task,
                    params,
                    data,
                    zero_packet,
                    deadline,
                    cancel,
                );
            }
            Err(e) => return PartialTransfer::failed(0, e),
        };
        #[cfg(test)]
        {
            let hook = self.state.lock().on_before_begin_transfer.take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut transfer = self.transfer.lock();
        if let Err(e) = self.begin_transfer(&mut transfer, cancel, expected_reset) {
            return PartialTransfer::failed(0, e);
        }
        let send_chunk = |transfer: &mut EndpointTransferState, len: usize| {
            self.transfer_chunk(
                current_task,
                transfer,
                params.transfer_type,
                len,
                deadline,
                cancel,
                expected_reset,
            )
        };

        if data.is_empty() {
            let result = send_chunk(&mut transfer, 0);
            return PartialTransfer { value: 0, error: result.error };
        }

        let mut offset = 0;
        while offset < data.len() {
            if let Some(error) = self.cancellation_error(cancel, expected_reset) {
                return PartialTransfer::failed(offset, error);
            }
            let chunk_len = min(data.len() - offset, params.chunk_size);
            if transfer.vmo.write(&data[offset..offset + chunk_len], 0).is_err() {
                return PartialTransfer::failed(offset, errno!(EIO));
            }
            let _ = transfer.vmo.op_range(zx::VmoOp::CACHE_CLEAN, 0, chunk_len as u64);
            let result = send_chunk(&mut transfer, chunk_len);
            offset += result.value;
            if let Some(e) = result.error {
                return PartialTransfer::failed(offset, e);
            }
            if result.value < chunk_len {
                // The device did not accept the whole transfer, so no zero-length packet is
                // needed to terminate it.
                return PartialTransfer::ok(offset);
            }
        }
        if zero_packet && offset % params.max_packet_size == 0 {
            if let Some(e) = send_chunk(&mut transfer, 0).error {
                return PartialTransfer::failed(offset, e);
            }
        }
        PartialTransfer::ok(offset)
    }

    /// Receives up to `max_len` bytes, in requests of at most `params.chunk_size` bytes.
    ///
    /// The transfer ends early on a short request. On failure, the data received before the
    /// failure is returned with the error.
    fn bulk_in(
        &self,
        current_task: &CurrentTask,
        params: EndpointTransferParams,
        max_len: usize,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
    ) -> PartialTransfer<Vec<u8>> {
        // All transfers on this endpoint are serialized by `acquire_transfer` + `self.transfer`,
        // and `begin_transfer` / `cancel_and_drain` ensure no earlier request is still using
        // `transfer.vmo` (replacing it with a fresh VMO if a cancelled request did not drain in
        // time).
        let (_slot, expected_reset) = match self.acquire_transfer(current_task, deadline, cancel) {
            Ok(AcquiredTransfer::Slot(slot, expected_reset)) => (slot, expected_reset),
            Ok(AcquiredTransfer::Replaced(replacement)) => {
                return replacement.bulk_in(current_task, params, max_len, deadline, cancel);
            }
            Err(e) => return PartialTransfer::failed(Vec::new(), e),
        };
        #[cfg(test)]
        {
            let hook = self.state.lock().on_before_begin_transfer.take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut transfer = self.transfer.lock();
        if let Err(e) = self.begin_transfer(&mut transfer, cancel, expected_reset) {
            return PartialTransfer::failed(Vec::new(), e);
        }

        if max_len == 0 {
            let result = self.transfer_chunk(
                current_task,
                &mut transfer,
                params.transfer_type,
                0,
                deadline,
                cancel,
                expected_reset,
            );
            return PartialTransfer { value: Vec::new(), error: result.error };
        }

        let mut data = Vec::new();
        while data.len() < max_len {
            if let Some(error) = self.cancellation_error(cancel, expected_reset) {
                return PartialTransfer::failed(data, error);
            }
            let requested = min(max_len - data.len(), params.chunk_size);
            let _ = transfer.vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, 0, requested as u64);
            let result = self.transfer_chunk(
                current_task,
                &mut transfer,
                params.transfer_type,
                requested,
                deadline,
                cancel,
                expected_reset,
            );
            if result.value > 0 {
                let _ = transfer.vmo.op_range(
                    zx::VmoOp::CACHE_CLEAN_INVALIDATE,
                    0,
                    result.value as u64,
                );
                let start = data.len();
                data.resize(start + result.value, 0);
                if transfer.vmo.read(&mut data[start..], 0).is_err() {
                    data.truncate(start);
                    return PartialTransfer::failed(data, errno!(EIO));
                }
            }
            if let Some(e) = result.error {
                return PartialTransfer::failed(data, e);
            }
            if result.value < requested {
                // Short packet: the device has no more data for this transfer.
                break;
            }
        }
        PartialTransfer::ok(data)
    }
}

/// Serializes the synchronous control transfers of a device.
///
/// The `Usb` protocol cannot cancel a control transfer, so a transfer whose caller is killed
/// keeps running on its kernel thread until it completes, which may be never if the caller gave
/// no timeout. Later transfers wait for it rather than piling up blocked kernel threads.
struct ControlTransferSlot {
    /// Whether a control transfer is currently in flight. Protected by a mutex rather than an
    /// atomic so that clearing/signaling the `released` event and updating `busy` are serialized
    /// without lock-free ordering subtleties.
    busy: LockDepMutex<bool, UsbfsControlSlotLock>,
    /// `USER_0` is asserted when the slot is released.
    released: zx::Event,
    /// Number of transfers waiting in `acquire`.
    #[cfg(test)]
    waiters: AtomicUsize,
}

#[cfg(test)]
struct ControlWaiterCountGuard<'a>(&'a ControlTransferSlot);

#[cfg(test)]
impl Drop for ControlWaiterCountGuard<'_> {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ControlTransferSlot {
    #[cfg(test)]
    fn count_control_waiter<'a>(&'a self) -> ControlWaiterCountGuard<'a> {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        ControlWaiterCountGuard(self)
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn count_control_waiter(&self) {}

    fn new() -> Self {
        Self {
            busy: LockDepMutex::new(false),
            released: zx::Event::create(),
            #[cfg(test)]
            waiters: AtomicUsize::new(0),
        }
    }

    /// Takes the slot, waiting for the transfer holding it to complete. The wait is killable, and
    /// is also interrupted if `cancel` is signalled.
    fn acquire(
        &self,
        current_task: &CurrentTask,
        cancel: Option<&UrbCancellation>,
    ) -> Result<(), Errno> {
        loop {
            let _waiter_guard = {
                let mut busy = self.busy.lock();
                if cancel.is_some_and(UrbCancellation::is_cancelled) {
                    return error!(ENOENT);
                }
                if !*busy {
                    *busy = true;
                    return Ok(());
                }
                // Clear the release signal while holding `busy` so a concurrent `release` cannot
                // signal `USER_0` between the check and the clear.
                let _ = self.released.signal(zx::Signals::USER_0, zx::Signals::NONE);
                self.count_control_waiter()
            };
            if cancel.is_some_and(UrbCancellation::is_cancelled) {
                return error!(ENOENT);
            }
            match cancel {
                Some(cancel) => wait_for_signals(
                    current_task,
                    &[(&self.released, zx::Signals::USER_0), (&cancel.wake, zx::Signals::USER_0)],
                    zx::MonotonicInstant::INFINITE,
                )?,
                None => wait_for_signals(
                    current_task,
                    &[(&self.released, zx::Signals::USER_0)],
                    zx::MonotonicInstant::INFINITE,
                )?,
            };
        }
    }

    fn release(&self) {
        let mut busy = self.busy.lock();
        *busy = false;
        let _ = self.released.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }
}

/// Runs a control transfer on the calling thread, which blocks until it completes. A `timeout`
/// of `None` means no timeout.
fn run_control_transfer(
    proxy: &fhardware_usb::UsbSynchronousProxy,
    setup: &fusb_descriptor::UsbSetup,
    timeout: Option<zx::MonotonicDuration>,
    out_data: &[u8],
) -> Result<Vec<u8>, Errno> {
    // The Usb protocol interprets a timeout of 0 as its default timeout, so request an infinite
    // one explicitly.
    let timeout_ns = timeout.unwrap_or(zx::MonotonicDuration::INFINITE).into_nanos();
    let _waiting_guard = ThreadLockupDetector::pause_tracking();
    fidl_result_to_errno(proxy.control_transfer(
        setup,
        timeout_ns,
        out_data,
        zx::MonotonicInstant::INFINITE,
    ))
}

/// Memory used by the outstanding transfers of usbfs, limited to `USBFS_MEMORY_LIMIT`.
#[derive(Debug, Default)]
pub(crate) struct UsbfsMemoryBudget {
    /// Bytes charged to outstanding transfers. Updated with `AcqRel`/`Acquire` orderings so a
    /// thread charging memory after another thread drops its `UsbfsMemoryCharge` observes the
    /// released budget.
    used: AtomicUsize,
}

impl UsbfsMemoryBudget {
    /// Charges `amount` bytes to the budget, or returns `ENOMEM` if they do not fit.
    fn charge(self: &Arc<Self>, amount: usize) -> Result<UsbfsMemoryCharge, Errno> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(amount).filter(|&total| total <= USBFS_MEMORY_LIMIT)
            })
            .map_err(|_| errno!(ENOMEM))?;
        Ok(UsbfsMemoryCharge { budget: Arc::clone(self), amount })
    }
}

/// Memory charged to a transfer, returned to the budget when dropped.
#[derive(Debug)]
struct UsbfsMemoryCharge {
    budget: Arc<UsbfsMemoryBudget>,
    amount: usize,
}

impl Drop for UsbfsMemoryCharge {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.amount, Ordering::AcqRel);
    }
}

/// Configuration state of a device, shared by all its file descriptors.
pub(crate) struct DeviceState {
    /// `bConfigurationValue` of the active configuration (0 if unconfigured).
    pub(crate) active_config: u8,
    /// Active alternate setting of each interface (absent means 0).
    alt_settings: HashMap<Ifno, u8>,
    /// File descriptor (`UsbDeviceFile::id`) owning each claimed interface.
    claims: HashMap<Ifno, u64>,
    /// Endpoints enabled on the host controller, with the descriptors they were enabled with.
    enabled_endpoints: HashMap<u8, ParsedEndpoint>,
    /// Optional hook invoked in `submit_urb` after `prepare_urb` releases the device state lock.
    #[cfg(test)]
    pub(crate) on_urb_prepared: Option<Box<dyn FnOnce(&CurrentTask) + Send>>,
}

pub(crate) struct UsbDevice {
    pub(crate) bus_num: u16,
    pub(crate) dev_num: u16,
    pub(crate) speed: u32,
    pub(crate) device_descriptor: usb_device_descriptor,
    parsed_configs: Vec<ParsedConfig>,
    pub(crate) raw_descriptors: Vec<u8>,
    pub(crate) sync_proxy: Option<Arc<fhardware_usb::UsbSynchronousProxy>>,
    /// Serializes the ioctls that read or change the configuration, interface claims, and
    /// endpoint lifecycle. Held across synchronous FIDL calls to the USB driver, so it is ordered
    /// before `UninterruptibleLock`.
    pub(crate) state: LockDepMutex<DeviceState, UsbfsDeviceStateLock>,
    pub(crate) endpoints: LockDepMutex<HashMap<u8, Arc<UsbEndpointClient>>, UsbfsEndpointsLock>,
    /// Set once the device has been removed from usbfs. Stored with `Release` ordering (while
    /// holding `endpoints` in `on_disconnect`) and loaded with `Acquire` ordering so that any
    /// thread observing `disconnected == false` under `endpoints` is guaranteed that
    /// `on_disconnect` has not yet drained the endpoint map, and any thread observing
    /// `disconnected == true` after a transfer error sees the disconnect state established by
    /// `on_disconnect`.
    disconnected: AtomicBool,
    /// Upper bound on the time spent draining cancelled requests before retiring an endpoint
    /// VMO.
    pub(crate) cancel_drain_timeout: zx::MonotonicDuration,
    control_slot: Arc<ControlTransferSlot>,
    pub(crate) memory: Arc<UsbfsMemoryBudget>,
    /// Monotonically increasing counter assigning a unique ID to each `UsbDeviceFile`. Accessed
    /// with `Relaxed` ordering because the atomic `fetch_add` itself guarantees uniqueness and no
    /// other memory state is published through the ID.
    next_file_id: AtomicU64,
    /// URB state of each open file description, by `UsbDeviceFile::id`.
    open_files: LockDepMutex<HashMap<u64, Arc<UsbfsFileUrbs>>, UsbfsOpenFilesLock>,
}

impl UsbDevice {
    pub(crate) fn new(
        bus_num: u16,
        dev_num: u16,
        speed: u32,
        device_descriptor: usb_device_descriptor,
        config_descriptors: Vec<u8>,
    ) -> Self {
        let mut raw_descriptors = Vec::new();
        let desc_bytes = zerocopy::IntoBytes::as_bytes(&device_descriptor);
        raw_descriptors.extend_from_slice(desc_bytes);
        raw_descriptors.extend_from_slice(&config_descriptors);
        let parsed_configs = parse_config_descriptors(&config_descriptors);

        Self {
            bus_num,
            dev_num,
            speed,
            device_descriptor,
            parsed_configs,
            raw_descriptors,
            sync_proxy: None,
            state: LockDepMutex::new(DeviceState {
                active_config: 1,
                alt_settings: HashMap::new(),
                claims: HashMap::new(),
                enabled_endpoints: HashMap::new(),
                #[cfg(test)]
                on_urb_prepared: None,
            }),
            endpoints: LockDepMutex::new(HashMap::new()),
            disconnected: AtomicBool::new(false),
            cancel_drain_timeout: DEFAULT_CANCEL_DRAIN_TIMEOUT,
            control_slot: Arc::new(ControlTransferSlot::new()),
            memory: Arc::default(),
            next_file_id: AtomicU64::new(1),
            open_files: LockDepMutex::new(HashMap::new()),
        }
    }

    /// Computes the Linux USB device minor number: `(bus - 1) * 128 + (dev_num - 1)`.
    pub(crate) fn minor(&self) -> u32 {
        debug_assert!(self.bus_num >= 1 && self.dev_num >= 1);
        (u32::from(self.bus_num).saturating_sub(1) * 128)
            + u32::from(self.dev_num).saturating_sub(1)
    }

    pub(crate) fn speed_string(&self) -> &'static str {
        match self.speed {
            USB_SPEED_LOW => "1.5\n",
            USB_SPEED_FULL => "12\n",
            USB_SPEED_HIGH => "480\n",
            USB_SPEED_WIRELESS => "480\n",
            USB_SPEED_SUPER => "5000\n",
            USB_SPEED_SUPER_PLUS => "10000\n",
            _ => "unknown\n",
        }
    }

    /// Returns the proxy to the USB device, or `ENODEV` if there is none.
    fn proxy(&self) -> Result<&Arc<fhardware_usb::UsbSynchronousProxy>, Errno> {
        self.sync_proxy.as_ref().ok_or_else(|| errno!(ENODEV))
    }

    /// Returns the memory charged to the outstanding transfers of the device.
    #[cfg(test)]
    pub(crate) fn memory_used(&self) -> usize {
        self.memory.used.load(Ordering::Acquire)
    }

    /// Returns the number of transfers waiting in `acquire_transfer` on `ep_addr`.
    #[cfg(test)]
    pub(crate) fn endpoint_transfer_waiters(&self, ep_addr: u8) -> usize {
        self.endpoints.lock().get(&ep_addr).map_or(0, |client| client.state.lock().transfer_waiters)
    }

    /// Installs a one-shot hook invoked right before `begin_transfer` on `ep_addr`.
    #[cfg(test)]
    pub(crate) fn set_on_before_begin_transfer(
        &self,
        ep_addr: u8,
        hook: impl FnOnce() + Send + 'static,
    ) {
        let client = self.endpoints.lock().get(&ep_addr).cloned().expect("endpoint connected");
        client.state.lock().on_before_begin_transfer = Some(Box::new(hook));
    }

    /// Returns the number of control transfers waiting in `ControlTransferSlot::acquire`.
    #[cfg(test)]
    pub(crate) fn control_transfer_waiters(&self) -> usize {
        self.control_slot.waiters.load(Ordering::SeqCst)
    }

    /// Calls `restart_endpoint` on `ep_addr` on a shallow copy of this device that has no
    /// `sync_proxy`, returning its result while sharing the live `endpoints` map, and returns
    /// whether the endpoint client was left in `restarting`.
    #[cfg(test)]
    pub(crate) fn restart_endpoint_without_proxy_for_test(
        &self,
        ep_addr: u8,
    ) -> (Result<(), Errno>, bool) {
        let mut state = self.state.lock();
        let enabled = state.enabled_endpoints.clone();
        let unproxied = Self {
            bus_num: self.bus_num,
            dev_num: self.dev_num,
            speed: self.speed,
            device_descriptor: self.device_descriptor,
            parsed_configs: self.parsed_configs.clone(),
            raw_descriptors: self.raw_descriptors.clone(),
            sync_proxy: None,
            state: LockDepMutex::new(DeviceState {
                active_config: state.active_config,
                alt_settings: state.alt_settings.clone(),
                claims: state.claims.clone(),
                enabled_endpoints: enabled,
                on_urb_prepared: None,
            }),
            endpoints: LockDepMutex::new(self.endpoints.lock().clone()),
            disconnected: AtomicBool::new(false),
            cancel_drain_timeout: self.cancel_drain_timeout,
            control_slot: Arc::clone(&self.control_slot),
            memory: Arc::clone(&self.memory),
            next_file_id: AtomicU64::new(1),
            open_files: LockDepMutex::new(HashMap::new()),
        };
        let result = unproxied.restart_endpoint(&mut state, ep_addr);
        let endpoints = unproxied.endpoints.into_inner();
        let restarting =
            endpoints.get(&ep_addr).is_some_and(|client| client.state.lock().restarting);
        *self.endpoints.lock() = endpoints;
        (result, restarting)
    }

    /// Returns whether any open file description has a waiter registered on `UsbfsFileUrbs`.
    #[cfg(test)]
    pub(crate) fn has_reap_waiter(&self) -> bool {
        self.open_files().iter().any(|file| !file.waiters.is_empty())
    }

    /// Returns the active configuration, or `None` if the device is unconfigured.
    fn active_config(&self, state: &DeviceState) -> Option<&ParsedConfig> {
        self.parsed_configs.iter().find(|c| c.config_value == state.active_config)
    }

    fn has_interface(&self, state: &DeviceState, ifno: Ifno) -> bool {
        self.active_config(state)
            .is_some_and(|c| c.interfaces.iter().any(|i| i.interface_number == ifno.as_u8()))
    }

    /// Returns the alternate settings of interface `ifno` in the active configuration.
    fn interface_alts<'a>(
        &'a self,
        state: &DeviceState,
        ifno: Ifno,
    ) -> impl Iterator<Item = &'a ParsedInterfaceAlt> {
        self.active_config(state)
            .into_iter()
            .flat_map(|c| c.interfaces.iter())
            .filter(move |i| i.interface_number == ifno.as_u8())
    }

    fn find_interface_alt(
        &self,
        state: &DeviceState,
        interface_number: u8,
        alt: u8,
    ) -> Option<&ParsedInterfaceAlt> {
        self.active_config(state)?
            .interfaces
            .iter()
            .find(|i| i.interface_number == interface_number && i.alt_setting == alt)
    }

    /// Returns the addresses of the endpoints of interface `ifno` in all its alternate settings
    /// of the active configuration.
    fn interface_endpoint_addresses(&self, state: &DeviceState, ifno: Ifno) -> Vec<u8> {
        let mut addresses: Vec<u8> = self
            .interface_alts(state, ifno)
            .flat_map(|i| i.endpoints.iter().map(ParsedEndpoint::address))
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        addresses
    }

    /// Finds the interface number owning endpoint `ep` in any alternate setting of the active
    /// configuration.
    /// Returns `EINVAL` if bits outside `USB_DIR_IN | USB_ENDPOINT_NUMBER_MASK` are set, `ESRCH`
    /// if the device is unconfigured, and `ENOENT` if no interface has the endpoint.
    fn find_interface_for_endpoint(&self, state: &DeviceState, ep: u32) -> Result<Ifno, Errno> {
        if (ep & !(USB_DIR_IN | USB_ENDPOINT_NUMBER_MASK)) != 0 {
            return error!(EINVAL);
        }
        let config = self.active_config(state).ok_or_else(|| errno!(ESRCH))?;
        config
            .interfaces
            .iter()
            .find(|i| i.endpoints.iter().any(|e| u32::from(e.address()) == ep))
            .map_or_else(|| error!(ENOENT), |i| Ifno::try_from(u32::from(i.interface_number)))
    }

    /// Returns the descriptors of endpoint `ep_addr` in the active alternate settings, if any.
    fn active_endpoint(&self, state: &DeviceState, ep_addr: u8) -> Option<&ParsedEndpoint> {
        self.active_config(state)?
            .interfaces
            .iter()
            .filter(|i| {
                let ifno = Ifno(i.interface_number);
                i.alt_setting == state.alt_settings.get(&ifno).copied().unwrap_or(0)
            })
            .flat_map(|i| i.endpoints.iter())
            .find(|e| e.address() == ep_addr)
    }

    /// Returns the URB state of the open file descriptions of the device.
    fn open_files(&self) -> Vec<Arc<UsbfsFileUrbs>> {
        self.open_files.lock().values().cloned().collect()
    }

    /// Returns whether any open file description has a pending URB on `ep_addr`.
    fn has_pending_urbs(&self, ep_addr: u8) -> bool {
        self.open_files()
            .iter()
            .any(|file| file.book.lock().pending.values().any(|urb| urb.endpoint == ep_addr))
    }

    pub(crate) fn on_disconnect(&self) {
        // Set `disconnected` and drain `endpoints` before `start_kill_urbs` locks `book` so any
        // concurrent `submit_urb` that locks `book` after `start_kill_urbs` sees
        // `disconnected == true` and fails with `ENODEV`, without `on_disconnect` acquiring
        // `state` (which may be held by a synchronous FIDL call to the disconnecting driver).
        // Mark the URBs before calling `client.disable()` so URBs whose requests are cancelled
        // complete with `-ESHUTDOWN` and any concurrent synchronous `USBDEVFS_BULK` holding an
        // endpoint transfer slot is woken up before `finish_kill_urbs` waits for URB workers.
        let endpoints: Vec<_> = {
            let mut endpoints = self.endpoints.lock();
            self.disconnected.store(true, Ordering::Release);
            endpoints.drain().collect()
        };
        let files_and_running: Vec<_> = self
            .open_files()
            .into_iter()
            .map(|file| {
                let running = file.start_kill_urbs(UrbCancelReason::Shutdown, |_| true);
                (file, running)
            })
            .collect();
        for (_ep_addr, client) in endpoints {
            client.disable();
        }
        for (file, running) in files_and_running {
            file.finish_kill_urbs(running);
            file.waiters.notify_fd_events(FdEvents::POLLHUP | FdEvents::POLLERR);
        }
    }

    /// Returns a client for endpoint `endpoint` of the active alternate settings, enabling the
    /// endpoint on the host controller and connecting to it if needed.
    fn get_or_connect_endpoint(
        &self,
        state: &mut DeviceState,
        endpoint: &ParsedEndpoint,
    ) -> Result<Arc<UsbEndpointClient>, Errno> {
        let proxy = self.proxy()?;
        let ep_addr = endpoint.address();

        let existing = {
            let mut endpoints = self.endpoints.lock();
            if self.disconnected.load(Ordering::Acquire) {
                return error!(ENODEV);
            }
            match endpoints.get(&ep_addr).cloned() {
                Some(client) if client.is_closed() => {
                    // The driver closes the channel only after disabling the endpoint, so the
                    // endpoint can be enabled and connected again right away.
                    endpoints.remove(&ep_addr);
                    state.enabled_endpoints.remove(&ep_addr);
                    None
                }
                existing => existing,
            }
        };
        if let Some(client) = &existing {
            if state.enabled_endpoints.get(&ep_addr) == Some(endpoint) {
                return Ok(Arc::clone(client));
            }
        }
        // If the endpoint is still enabled with other descriptors, disable it first. The
        // lifecycle code disables endpoints whose interface changes, so this is a safety net.
        self.disable_endpoint(state, ep_addr)?;

        fidl_result_to_errno(proxy.enable_endpoint(
            &endpoint.descriptor,
            &endpoint.ss_companion,
            true,
            zx::MonotonicInstant::INFINITE,
        ))?;
        self.connect_enabled_endpoint(state, endpoint, existing)
    }

    /// Connects (or re-enables `existing`) and registers the client for `endpoint`, which has
    /// just been enabled on the host controller. Rolls back `enable_endpoint` on failure.
    fn connect_enabled_endpoint(
        &self,
        state: &mut DeviceState,
        endpoint: &ParsedEndpoint,
        existing: Option<Arc<UsbEndpointClient>>,
    ) -> Result<Arc<UsbEndpointClient>, Errno> {
        let proxy = self.proxy()?;
        let ep_addr = endpoint.address();
        let client = match existing {
            Some(client) => client,
            None => match UsbEndpointClient::connect(proxy, ep_addr, self.cancel_drain_timeout) {
                Ok(client) => client,
                Err(e) => {
                    // Roll back the enable so a later attempt does not fail on an endpoint that
                    // is already enabled.
                    let _ = proxy.enable_endpoint(
                        &endpoint.descriptor,
                        &endpoint.ss_companion,
                        false,
                        zx::MonotonicInstant::INFINITE,
                    );
                    return Err(e);
                }
            },
        };
        {
            let mut endpoints = self.endpoints.lock();
            // `on_disconnect` may have run while `enable_endpoint` or `connect` was in flight
            // without the `endpoints` lock held. Re-check `disconnected` under `endpoints` before
            // publishing the enabled client so we never leave an active endpoint in `endpoints`
            // after disconnect.
            if self.disconnected.load(Ordering::Acquire) {
                drop(endpoints);
                client.disable();
                let _ = proxy.enable_endpoint(
                    &endpoint.descriptor,
                    &endpoint.ss_companion,
                    false,
                    zx::MonotonicInstant::INFINITE,
                );
                return error!(ENODEV);
            }
            client.enable();
            endpoints.insert(ep_addr, Arc::clone(&client));
        }
        state.enabled_endpoints.insert(ep_addr, endpoint.clone());
        Ok(client)
    }

    /// Updates the endpoint client of pending URBs on `ep_addr` across all open file
    /// descriptions so `kill_locked` wakes the replacement client after a reconnect.
    fn replace_pending_urb_clients(&self, ep_addr: u8, new_client: &Arc<UsbEndpointClient>) {
        for file in self.open_files() {
            let mut book = file.book.lock();
            for urb in book.pending.values_mut() {
                if urb.endpoint == ep_addr && urb.client.is_some() {
                    urb.client = Some(Arc::clone(new_client));
                }
            }
        }
    }

    /// Disables endpoint `ep_addr` on the host controller if it is enabled, after cancelling its
    /// in-flight requests.
    ///
    /// This does not wait for the transfer that queued the requests: it is woken up and drains
    /// their completions itself, within the cancel drain timeout.
    fn disable_endpoint(&self, state: &mut DeviceState, ep_addr: u8) -> Result<(), Errno> {
        let Some(endpoint) = state.enabled_endpoints.remove(&ep_addr) else {
            return Ok(());
        };
        let client = self.endpoints.lock().get(&ep_addr).cloned();
        if let Some(client) = client {
            client.disable();
            if client.is_closed() {
                // The driver disabled the endpoint before closing the channel.
                self.endpoints.lock().remove(&ep_addr);
                return Ok(());
            }
        }
        fidl_result_to_errno(self.proxy()?.enable_endpoint(
            &endpoint.descriptor,
            &endpoint.ss_companion,
            false,
            zx::MonotonicInstant::INFINITE,
        ))
    }

    /// Disables all the enabled endpoints.
    fn disable_all_endpoints(&self, state: &mut DeviceState) -> Result<(), Errno> {
        let addresses: Vec<u8> = state.enabled_endpoints.keys().copied().collect();
        for ep_addr in addresses {
            self.disable_endpoint(state, ep_addr)?;
        }
        Ok(())
    }

    /// Disables and immediately re-enables `ep_addr` on the host controller while holding back
    /// queued transfers in `acquire_transfer` so they run on the restarted endpoint instead of
    /// failing with `ESHUTDOWN`.
    fn restart_endpoint(&self, state: &mut DeviceState, ep_addr: u8) -> Result<(), Errno> {
        let proxy = self.proxy()?;
        if let Some(endpoint) = state.enabled_endpoints.get(&ep_addr).cloned() {
            let client = self.endpoints.lock().get(&ep_addr).cloned();
            if let Some(client) = client {
                client.begin_restart();
                if !client.is_closed() {
                    let toggled = fidl_result_to_errno(proxy.enable_endpoint(
                        &endpoint.descriptor,
                        &endpoint.ss_companion,
                        false,
                        zx::MonotonicInstant::INFINITE,
                    ))
                    .and_then(|()| {
                        fidl_result_to_errno(proxy.enable_endpoint(
                            &endpoint.descriptor,
                            &endpoint.ss_companion,
                            true,
                            zx::MonotonicInstant::INFINITE,
                        ))
                    });
                    return match toggled {
                        Ok(()) if !client.is_closed() => {
                            client.finish_restart();
                            Ok(())
                        }
                        Ok(()) => {
                            // The driver closed the endpoint channel when disabling the
                            // endpoint; connect a new client for the re-enabled endpoint and
                            // redirect any transfer waiting on the old client.
                            self.endpoints.lock().remove(&ep_addr);
                            state.enabled_endpoints.remove(&ep_addr);
                            match self.connect_enabled_endpoint(state, &endpoint, None) {
                                Ok(new_client) => {
                                    self.replace_pending_urb_clients(ep_addr, &new_client);
                                    client.replace_with(new_client);
                                    Ok(())
                                }
                                Err(e) => {
                                    client.disable();
                                    Err(e)
                                }
                            }
                        }
                        Err(e) => {
                            state.enabled_endpoints.remove(&ep_addr);
                            client.disable();
                            Err(e)
                        }
                    };
                }
                self.endpoints.lock().remove(&ep_addr);
                state.enabled_endpoints.remove(&ep_addr);
                return match self.active_endpoint(state, ep_addr).cloned() {
                    Some(active) => match self.get_or_connect_endpoint(state, &active) {
                        Ok(new_client) => {
                            self.replace_pending_urb_clients(ep_addr, &new_client);
                            client.replace_with(new_client);
                            Ok(())
                        }
                        Err(e) => {
                            client.disable();
                            Err(e)
                        }
                    },
                    None => {
                        client.disable();
                        Ok(())
                    }
                };
            }
        }
        if let Some(endpoint) = self.active_endpoint(state, ep_addr).cloned() {
            let new_client = self.get_or_connect_endpoint(state, &endpoint)?;
            self.replace_pending_urb_clients(ep_addr, &new_client);
        }
        Ok(())
    }

    /// Cancels the in-flight requests of endpoint `ep_addr`.
    pub(crate) fn reset_endpoint_state(&self, ep_addr: u8) {
        let client = self.endpoints.lock().get(&ep_addr).cloned();
        if let Some(client) = client {
            client.cancel_all();
        }
    }

    /// Releases the claim on interface `ifno`, killing the URBs of its owner on the interface's
    /// endpoints and cancelling their in-flight requests. If a non-zero alternate setting was
    /// active, restores alternate setting 0.
    fn drop_claim(&self, state: &mut DeviceState, ifno: Ifno) {
        let owner = state.claims.remove(&ifno);
        let endpoints = self.interface_endpoint_addresses(state, ifno);
        // Mark and wake the owner's URBs before cancelling or disabling the interface's endpoints
        // so any concurrent synchronous transfer holding an endpoint slot is also aborted, then
        // wait for running URBs before restoring alternate setting 0.
        let owner_urbs = owner.and_then(|id| self.open_files.lock().get(&id).cloned());
        let running = owner_urbs.as_ref().map(|urbs| {
            urbs.start_kill_urbs(UrbCancelReason::Killed, |urb| endpoints.contains(&urb.endpoint))
        });
        let alt = state.alt_settings.remove(&ifno).unwrap_or(0);
        let restore_alt0 = alt != 0 && !self.disconnected.load(Ordering::Acquire);
        if restore_alt0 {
            for &ep_addr in &endpoints {
                let _ = self.disable_endpoint(state, ep_addr);
            }
        } else {
            for &ep_addr in &endpoints {
                self.reset_endpoint_state(ep_addr);
            }
        }
        if let (Some(urbs), Some(running)) = (&owner_urbs, running) {
            urbs.finish_kill_urbs(running);
        }
        if restore_alt0 {
            if let Ok(proxy) = self.proxy() {
                let _ = proxy.set_interface(ifno.as_u8(), 0, zx::MonotonicInstant::INFINITE);
            }
        }
    }

    /// Maps the error of a transfer to `ENODEV` if the device was disconnected.
    fn transfer_error(&self, error: Errno) -> Errno {
        if self.disconnected.load(Ordering::Acquire) { errno!(ENODEV) } else { error }
    }

    /// Sends `data` to OUT endpoint `client`. See `UsbEndpointClient::bulk_out`.
    fn endpoint_bulk_out(
        &self,
        current_task: &CurrentTask,
        client: &UsbEndpointClient,
        params: EndpointTransferParams,
        data: &[u8],
        zero_packet: bool,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
    ) -> PartialTransfer<usize> {
        if self.disconnected.load(Ordering::Acquire) {
            return PartialTransfer::failed(0, errno!(ENODEV));
        }
        let result = client.bulk_out(current_task, params, data, zero_packet, deadline, cancel);
        PartialTransfer { value: result.value, error: result.error.map(|e| self.transfer_error(e)) }
    }

    /// Receives up to `max_len` bytes from IN endpoint `client`. See
    /// `UsbEndpointClient::bulk_in`.
    fn endpoint_bulk_in(
        &self,
        current_task: &CurrentTask,
        client: &UsbEndpointClient,
        params: EndpointTransferParams,
        max_len: usize,
        deadline: zx::MonotonicInstant,
        cancel: Option<&UrbCancellation>,
    ) -> PartialTransfer<Vec<u8>> {
        if self.disconnected.load(Ordering::Acquire) {
            return PartialTransfer::failed(Vec::new(), errno!(ENODEV));
        }
        let result = client.bulk_in(current_task, params, max_len, deadline, cancel);
        PartialTransfer { value: result.value, error: result.error.map(|e| self.transfer_error(e)) }
    }

    /// Issues a control transfer on endpoint 0 and returns the data read, if any.
    ///
    /// The transfer runs on a kernel thread so that the calling task can be killed while it
    /// waits. If it is, `EINTR` is returned and the transfer keeps running in the background
    /// until it completes, which never happens if `timeout` is `None` and the device does not
    /// answer. Control transfers are serialized by the control slot, so at most one such
    /// transfer runs per device.
    fn control_transfer(
        &self,
        current_task: &CurrentTask,
        setup: fusb_descriptor::UsbSetup,
        timeout: Option<zx::MonotonicDuration>,
        out_data: Vec<u8>,
    ) -> Result<Vec<u8>, Errno> {
        let proxy = Arc::clone(self.proxy()?);
        self.control_slot.acquire(current_task, None)?;
        let done = Arc::new(zx::Event::create());
        let (sender, receiver) = sync_channel(1);
        let done_for_transfer = Arc::clone(&done);
        let slot = Arc::clone(&self.control_slot);
        let closure = move |_: &CurrentTask| {
            let result = run_control_transfer(&proxy, &setup, timeout, &out_data);
            slot.release();
            let _ = sender.send(result);
            let _ = done_for_transfer.signal(zx::Signals::NONE, zx::Signals::USER_0);
        };
        let request = SpawnRequestBuilder::new()
            .with_debug_name("usbfs-control-transfer")
            .with_sync_closure(closure)
            .build();
        current_task.kernel().kthreads.spawner().spawn_from_request(request);
        wait_for_signals(
            current_task,
            &[(done.as_ref(), zx::Signals::USER_0)],
            zx::MonotonicInstant::INFINITE,
        )?;
        receiver.recv().map_err(|_| errno!(EIO))?
    }
}

/// Why a URB was cancelled, which determines its completion status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum UrbCancelReason {
    /// Killed by `USBDEVFS_DISCARDURB`, `USBDEVFS_RESET`, the release of its interface, or the
    /// close of its file description.
    Killed = 1,
    /// Unlinked because an earlier URB of the same bulk transfer failed.
    Unlinked = 2,
    /// The device was disconnected.
    Shutdown = 3,
}

impl UrbCancelReason {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Killed),
            2 => Some(Self::Unlinked),
            3 => Some(Self::Shutdown),
            _ => None,
        }
    }

    /// Returns the `status` of a URB cancelled for this reason.
    fn status(self) -> i32 {
        match self {
            Self::Killed => urb_status(&errno!(ENOENT)),
            Self::Unlinked => urb_status(&errno!(ECONNRESET)),
            Self::Shutdown => urb_status(&errno!(ESHUTDOWN)),
        }
    }
}

/// Cancellation state of a URB, shared with the worker processing it.
struct UrbCancellation {
    /// Set once the URB is cancelled. Synchronized with `Release`/`Acquire` orderings so a
    /// thread observing `cancelled == true` also sees `reason`.
    cancelled: AtomicBool,
    /// `UrbCancelReason` of the first cancellation, or 0. Updated with `AcqRel`/`Acquire` so the
    /// first reason is published to readers observing `cancelled`.
    reason: AtomicU8,
    /// Signaled with `USER_0` when the URB is cancelled, waking any thread waiting to acquire
    /// an endpoint transfer slot or the control transfer slot.
    wake: zx::Event,
}

impl Default for UrbCancellation {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            reason: AtomicU8::new(0),
            wake: zx::Event::create(),
        }
    }
}

impl UrbCancellation {
    /// Cancels the URB for `reason`. Returns whether it was not cancelled already.
    fn cancel(&self, reason: UrbCancelReason) -> bool {
        let first = self
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        self.cancelled.store(true, Ordering::Release);
        let _ = self.wake.signal(zx::Signals::NONE, zx::Signals::USER_0);
        first
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn reason(&self) -> Option<UrbCancelReason> {
        UrbCancelReason::from_u8(self.reason.load(Ordering::Acquire))
    }
}

/// Transfer of a URB.
enum UrbTransfer {
    /// Control transfer on endpoint 0.
    Control { setup: fusb_descriptor::UsbSetup, out_data: Vec<u8> },
    /// Bulk or interrupt transfer on another endpoint.
    Endpoint {
        client: Arc<UsbEndpointClient>,
        params: EndpointTransferParams,
        out_data: Vec<u8>,
        zero_packet: bool,
    },
}

/// A URB waiting in the queue of its endpoint, or being processed by the worker of the queue.
struct QueuedUrb {
    id: u64,
    transfer: UrbTransfer,
    is_in: bool,
    /// Length of the data stage, excluding the setup packet of control URBs.
    length: usize,
    /// Whether a short IN transfer fails with `-EREMOTEIO` (`USBDEVFS_URB_SHORT_NOT_OK`).
    short_not_ok: bool,
    /// Whether the URB targets a bulk endpoint, whose failures unlink the continuation URBs
    /// queued after it.
    is_bulk: bool,
    /// Whether the URB continues the transfer of the previous URB of its bulk endpoint
    /// (`USBDEVFS_URB_BULK_CONTINUATION`).
    bulk_continuation: bool,
    cancellation: Arc<UrbCancellation>,
    /// Shared with `PendingUrb`/`CompletedUrb` so the memory budget stays charged until both the
    /// worker thread and reaper have finished with the URB (even when a cancelled control URB
    /// completes in the book while its worker thread is still running).
    _charge: Arc<UsbfsMemoryCharge>,
}

/// A submitted URB that has not completed yet.
struct PendingUrb {
    /// Address of the `struct usbdevfs_urb` of the URB, which identifies it to userspace.
    user_addr: UserAddress,
    is_32bit: bool,
    /// Address of the endpoint of the URB, 0 for control URBs.
    endpoint: u8,
    /// Address of the data stage buffer, after the setup packet of control URBs.
    data_addr: UserAddress,
    cancellation: Arc<UrbCancellation>,
    /// Client of the endpoint of a bulk or interrupt URB, woken up to cancel it.
    client: Option<Arc<UsbEndpointClient>>,
    /// Signaled with `USER_0` when the URB completes.
    completed: Arc<zx::Event>,
    charge: Arc<UsbfsMemoryCharge>,
}

/// Result of a URB.
struct UrbResult {
    status: i32,
    actual_length: usize,
    /// Data received by an IN URB.
    in_data: Vec<u8>,
}

impl UrbResult {
    fn cancelled(reason: UrbCancelReason) -> Self {
        Self { status: reason.status(), actual_length: 0, in_data: Vec::new() }
    }
}

/// A completed URB waiting to be reaped.
struct CompletedUrb {
    user_addr: UserAddress,
    is_32bit: bool,
    data_addr: UserAddress,
    result: UrbResult,
    /// Released when the URB is reaped (and its worker has finished).
    _charge: Arc<UsbfsMemoryCharge>,
}

/// URBs of an endpoint, processed in submission order by at most one worker kernel thread, like
/// the endpoint queues of a host controller.
#[derive(Default)]
struct EndpointUrbQueue {
    queued: VecDeque<QueuedUrb>,
    /// ID of the URB being processed by the worker.
    running: Option<u64>,
    /// Whether a worker processes the queue.
    has_worker: bool,
}

#[derive(Default)]
struct UrbBook {
    next_id: u64,
    /// Submitted URBs that have not completed, by ID, i.e. in submission order.
    pending: BTreeMap<u64, PendingUrb>,
    /// Completed URBs waiting to be reaped, in completion order.
    completed: VecDeque<CompletedUrb>,
    /// URB queue of each endpoint address, 0 for control URBs.
    queues: HashMap<u8, EndpointUrbQueue>,
    /// Bulk endpoints on which continuation URBs are rejected because a transfer failed.
    disabled_bulk_endpoints: HashSet<u8>,
}

/// URBs of a usbfs file description, shared with the workers processing them and with its
/// device.
#[derive(Default)]
struct UsbfsFileUrbs {
    book: LockDepMutex<UrbBook, UsbfsFileUrbsLock>,
    /// Notified when URBs complete and when the device is disconnected.
    waiters: WaitQueue,
}

impl UsbfsFileUrbs {
    fn notify_completion(&self) {
        self.waiters.notify_fd_events(FdEvents::POLLOUT | FdEvents::POLLWRNORM);
    }

    /// Moves pending URB `id` to the completed URBs with `result`. Returns false if the URB is
    /// not pending, e.g. because it completed when it was cancelled.
    fn complete_locked(book: &mut UrbBook, id: u64, result: UrbResult) -> bool {
        let Some(urb) = book.pending.remove(&id) else {
            return false;
        };
        let _ = urb.completed.signal(zx::Signals::NONE, zx::Signals::USER_0);
        book.completed.push_back(CompletedUrb {
            user_addr: urb.user_addr,
            is_32bit: urb.is_32bit,
            data_addr: urb.data_addr,
            result,
            _charge: urb.charge,
        });
        true
    }

    /// Cancels pending URB `id` for `reason`.
    ///
    /// URBs still queued never reached the device, and control transfers cannot be cancelled,
    /// so both complete right away; the late result of a running control transfer is dropped.
    /// A running bulk or interrupt URB is woken up, and completes once its request is
    /// cancelled: the event signaled on its completion is returned.
    fn kill_locked(book: &mut UrbBook, id: u64, reason: UrbCancelReason) -> Option<Arc<zx::Event>> {
        let urb = book.pending.get(&id)?;
        let completed = Arc::clone(&urb.completed);
        if !urb.cancellation.cancel(reason) {
            // Another cancellation is completing the URB.
            return Some(completed);
        }
        let client = urb.client.clone();
        let queue = book.queues.entry(urb.endpoint).or_default();
        if queue.running == Some(id) {
            if let Some(client) = client {
                client.wake_transfer();
                return Some(completed);
            }
            track_stub!(
                TODO("https://fxbug.dev/571954283"),
                "usbfs cancel in-flight control transfer"
            );
        } else {
            queue.queued.retain(|queued| queued.id != id);
        }
        Self::complete_locked(book, id, UrbResult::cancelled(reason));
        None
    }

    /// Marks the pending URBs selected by `filter` as cancelled for `reason`, completing queued
    /// and control URBs immediately and waking running endpoint URBs. Returns the completion
    /// events of the running endpoint URBs so the caller can wait for them via
    /// `finish_kill_urbs`.
    fn start_kill_urbs(
        &self,
        reason: UrbCancelReason,
        filter: impl Fn(&PendingUrb) -> bool,
    ) -> Vec<Arc<zx::Event>> {
        let mut book = self.book.lock();
        let ids: Vec<u64> =
            book.pending.iter().filter(|(_, urb)| filter(urb)).map(|(&id, _)| id).collect();
        let mut running = Vec::new();
        for id in ids {
            if let Some(completed) = Self::kill_locked(&mut book, id, reason) {
                running.push(completed);
            }
        }
        running
    }

    /// Waits for the running URBs returned by `start_kill_urbs` to complete and notifies
    /// completion waiters.
    fn finish_kill_urbs(&self, running: Vec<Arc<zx::Event>>) {
        for completed in running {
            let _ = completed.wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE);
        }
        self.notify_completion();
    }

    /// Cancels the pending URBs selected by `filter` for `reason`, waiting for any running
    /// ones to complete.
    fn kill_urbs(&self, reason: UrbCancelReason, filter: impl Fn(&PendingUrb) -> bool) {
        let running = self.start_kill_urbs(reason, filter);
        self.finish_kill_urbs(running);
    }

    /// Unlinks the continuation URBs queued on bulk endpoint `endpoint` after a URB of the
    /// endpoint failed. Unless a new transfer is queued, continuation URBs are rejected until
    /// one is submitted.
    fn unlink_bulk_continuations_locked(book: &mut UrbBook, endpoint: u8) {
        let queue = book.queues.entry(endpoint).or_default();
        let mut unlinked = Vec::new();
        let mut new_transfer_queued = false;
        while let Some(urb) = queue.queued.front() {
            if !urb.bulk_continuation {
                new_transfer_queued = true;
                break;
            }
            if let Some(urb) = queue.queued.pop_front() {
                urb.cancellation.cancel(UrbCancelReason::Unlinked);
                unlinked.push(urb.id);
            }
        }
        if !new_transfer_queued {
            book.disabled_bulk_endpoints.insert(endpoint);
        }
        for id in unlinked {
            Self::complete_locked(book, id, UrbResult::cancelled(UrbCancelReason::Unlinked));
        }
    }

    /// Processes the URBs queued on `endpoint` in order, until the queue is empty.
    fn run_worker(&self, device: &UsbDevice, endpoint: u8, current_task: &CurrentTask) {
        loop {
            let urb = {
                let mut book = self.book.lock();
                let queue = book.queues.entry(endpoint).or_default();
                let Some(urb) = queue.queued.pop_front() else {
                    queue.has_worker = false;
                    return;
                };
                queue.running = Some(urb.id);
                urb
            };
            let urb_id = urb.id;
            let is_bulk = urb.is_bulk;
            let result = device.run_urb(current_task, &urb);
            // Drop `urb` (and its `Arc<UsbfsMemoryCharge>`) before signaling `urb.completed` in
            // `complete_locked` so a caller waiting on `discard_urb` sees the memory released as
            // soon as the URB is reaped.
            drop(urb);
            let mut book = self.book.lock();
            if let Some(queue) = book.queues.get_mut(&endpoint) {
                queue.running = None;
            }
            if let Some(result) = result {
                let status = result.status;
                let failed = status < 0
                    && status != UrbCancelReason::Killed.status()
                    && status != UrbCancelReason::Unlinked.status();
                if Self::complete_locked(&mut book, urb_id, result) && is_bulk && failed {
                    Self::unlink_bulk_continuations_locked(&mut book, endpoint);
                }
            }
            drop(book);
            self.notify_completion();
        }
    }
}

/// Endpoint of a validated URB.
enum UrbTarget {
    /// Endpoint 0, with the setup packet of the URB.
    Control { setup: fusb_descriptor::UsbSetup },
    /// A bulk or interrupt endpoint.
    Endpoint { endpoint: ParsedEndpoint, params: EndpointTransferParams, is_bulk: bool },
}

/// Reads and validates the setup packet of control URB `urb`, whose buffer is `buffer_length`
/// bytes long.
fn read_control_urb_setup(
    current_task: &CurrentTask,
    urb: &UrbHeader,
    buffer_length: usize,
) -> Result<fusb_descriptor::UsbSetup, Errno> {
    if buffer_length < USB_SETUP_PACKET_SIZE {
        return error!(EINVAL);
    }
    let setup: usb_ctrlrequest = current_task.read_object(UserRef::new(urb.buffer))?;
    let w_length = u16::from_le(setup.wLength);
    if buffer_length < USB_SETUP_PACKET_SIZE + usize::from(w_length) {
        return error!(EINVAL);
    }
    // Unlike Linux, the Usb protocol limits the length of the data stage.
    if u64::from(w_length) > fhardware_usb::MAX_CONTROL_REQUEST_LEN {
        return error!(EINVAL);
    }
    Ok(fusb_descriptor::UsbSetup {
        bm_request_type: setup.bRequestType,
        b_request: setup.bRequest,
        w_value: u16::from_le(setup.wValue),
        w_index: u16::from_le(setup.wIndex),
        w_length,
    })
}

/// Rejects URBs requesting a signal on completion, which is not supported.
fn check_urb_signal(urb: &UrbHeader) -> Result<(), Errno> {
    if urb.signr != 0 {
        track_stub!(TODO("https://fxbug.dev/571954283"), "usbfs URB completion signal");
        return error!(EINVAL);
    }
    Ok(())
}

impl UsbDevice {
    /// Runs `urb` on the calling worker thread and returns its result, or `None` if it already
    /// completed when it was cancelled.
    fn run_urb(&self, current_task: &CurrentTask, urb: &QueuedUrb) -> Option<UrbResult> {
        let cancel = Some(urb.cancellation.as_ref());
        let outcome = match &urb.transfer {
            UrbTransfer::Control { setup, out_data } => {
                self.run_control_urb(current_task, urb, setup, out_data)?
            }
            UrbTransfer::Endpoint { client, params, out_data, zero_packet } => {
                let deadline = zx::MonotonicInstant::INFINITE;
                if urb.is_in {
                    let result = self.endpoint_bulk_in(
                        current_task,
                        client,
                        *params,
                        urb.length,
                        deadline,
                        cancel,
                    );
                    PartialTransfer {
                        value: (result.value.len(), result.value),
                        error: result.error,
                    }
                } else {
                    let result = self.endpoint_bulk_out(
                        current_task,
                        client,
                        *params,
                        out_data,
                        *zero_packet,
                        deadline,
                        cancel,
                    );
                    PartialTransfer { value: (result.value, Vec::new()), error: result.error }
                }
            }
        };
        let (actual_length, in_data) = outcome.value;
        let status = match outcome.error {
            // A cancelled URB reports why it was cancelled when its transfer was aborted by
            // cancellation, endpoint reset, or disconnect, while genuine hardware or transport
            // errors (such as EPIPE or EPROTO) that completed before a late cancellation are
            // preserved.
            Some(e) => match e.code {
                ENOENT | ESHUTDOWN | ECONNRESET | EINTR => {
                    urb.cancellation.reason().map_or_else(|| urb_status(&e), |r| r.status())
                }
                ENODEV => urb.cancellation.reason().unwrap_or(UrbCancelReason::Shutdown).status(),
                _ => urb_status(&e),
            },
            None if urb.short_not_ok && urb.is_in && actual_length < urb.length => {
                urb_status(&errno!(EREMOTEIO))
            }
            None => 0,
        };
        Some(UrbResult { status, actual_length, in_data })
    }

    /// Runs control URB `urb`, holding the control slot like synchronous control transfers.
    /// Returns `None` if the URB was cancelled, in which case it already completed.
    fn run_control_urb(
        &self,
        current_task: &CurrentTask,
        urb: &QueuedUrb,
        setup: &fusb_descriptor::UsbSetup,
        out_data: &[u8],
    ) -> Option<PartialTransfer<(usize, Vec<u8>)>> {
        let failed = |e: Errno| PartialTransfer::failed((0, Vec::new()), e);
        let proxy = match self.proxy() {
            Ok(proxy) => Arc::clone(proxy),
            Err(e) => return Some(failed(e)),
        };
        if let Err(e) = self.control_slot.acquire(current_task, Some(&urb.cancellation)) {
            if urb.cancellation.is_cancelled() {
                return None;
            }
            return Some(failed(e));
        }
        if urb.cancellation.is_cancelled() {
            self.control_slot.release();
            return None;
        }
        // Like Linux, asynchronous URBs have no timeout.
        let result = run_control_transfer(&proxy, setup, None, out_data);
        self.control_slot.release();
        Some(match result {
            Ok(mut data) if urb.is_in => {
                data.truncate(urb.length);
                PartialTransfer::ok((data.len(), data))
            }
            // The Usb protocol does not report the number of bytes sent by an OUT control
            // transfer: on success, the device accepted all of them.
            Ok(_) => PartialTransfer::ok((urb.length, Vec::new())),
            Err(e) => failed(self.transfer_error(e)),
        })
    }
}

pub(crate) struct UsbDeviceFile {
    device: Arc<UsbDevice>,
    /// Identifies this file descriptor in `DeviceState::claims` and `UsbDevice::open_files`.
    id: u64,
    urbs: Arc<UsbfsFileUrbs>,
}

/// Reads the interface number argument of `USBDEVFS_CLAIMINTERFACE` and
/// `USBDEVFS_RELEASEINTERFACE`.
fn read_ifno(current_task: &CurrentTask, arg: SyscallArg) -> Result<Ifno, Errno> {
    let raw_ifno: u32 = current_task.read_object(UserRef::new(UserAddress::from(arg)))?;
    Ifno::try_from(raw_ifno)
}

impl UsbDeviceFile {
    pub(crate) fn new(device: Arc<UsbDevice>) -> Self {
        let id = device.next_file_id.fetch_add(1, Ordering::Relaxed);
        let urbs = Arc::new(UsbfsFileUrbs::default());
        device.open_files.lock().insert(id, Arc::clone(&urbs));
        Self { device, id, urbs }
    }

    /// Writes completed URB `urb` back to userspace, and its address to the `USBDEVFS_REAPURB`
    /// argument at `reap_arg_addr`, as a 32-bit pointer if `is_reap_32`.
    fn write_reaped_urb(
        &self,
        current_task: &CurrentTask,
        reap_arg_addr: UserAddress,
        urb: CompletedUrb,
        is_reap_32: bool,
    ) -> Result<SyscallResult, Errno> {
        let result = &urb.result;
        let actual_length = i32::try_from(result.actual_length).map_err(|_| errno!(EOVERFLOW))?;
        let ptr_32 = if is_reap_32 {
            Some(u32::try_from(urb.user_addr.ptr()).map_err(|_| errno!(EOVERFLOW))?)
        } else {
            None
        };
        let copied = min(result.in_data.len(), result.actual_length);
        if copied > 0 {
            current_task.write_memory(urb.data_addr, &result.in_data[..copied])?;
        }
        write_urb_result(current_task, urb.user_addr, urb.is_32bit, result.status, actual_length)?;
        if let Some(ptr) = ptr_32 {
            current_task.write_object(UserRef::new(reap_arg_addr), &ptr)?;
        } else {
            let ptr = urb.user_addr.ptr() as u64;
            current_task.write_object(UserRef::new(reap_arg_addr), &ptr)?;
        }
        Ok(SUCCESS)
    }

    /// Validates `urb`, charges its memory, and connects to its endpoint. `control_setup` is the
    /// setup packet of a URB on endpoint 0.
    fn prepare_urb(
        &self,
        state: &mut DeviceState,
        urb: &UrbHeader,
        buffer_length: usize,
        control_setup: Option<fusb_descriptor::UsbSetup>,
    ) -> Result<(UrbTarget, Option<Ifno>, Arc<UsbfsMemoryCharge>), Errno> {
        if let Some(setup) = control_setup {
            let claimed_ifno = self.check_control_recipient(
                state,
                setup.bm_request_type,
                setup.b_request,
                setup.w_index,
            )?;
            check_urb_signal(urb)?;
            let charge = Arc::new(
                self.device
                    .memory
                    .charge(URB_SIZE + USB_SETUP_PACKET_SIZE + usize::from(setup.w_length))?,
            );
            return Ok((UrbTarget::Control { setup }, claimed_ifno, charge));
        }
        let ifno = self.device.find_interface_for_endpoint(state, u32::from(urb.endpoint))?;
        self.check_interface(state, ifno.into())?;
        let endpoint = self
            .device
            .active_endpoint(state, urb.endpoint)
            .cloned()
            .ok_or_else(|| errno!(ENOENT))?;
        let endpoint_type = endpoint.transfer_type();
        match (u32::from(urb.type_), endpoint_type) {
            // Bulk URBs on interrupt endpoints are single-shot interrupt transfers.
            (USBDEVFS_URB_TYPE_BULK, USB_ENDPOINT_XFER_BULK | USB_ENDPOINT_XFER_INT)
            | (USBDEVFS_URB_TYPE_INTERRUPT, USB_ENDPOINT_XFER_INT) => {}
            (USBDEVFS_URB_TYPE_CONTROL, USB_ENDPOINT_XFER_CONTROL) => {
                track_stub!(
                    TODO("https://fxbug.dev/571954283"),
                    "usbfs control URB on a non-default control endpoint"
                );
                return error!(EINVAL);
            }
            (USBDEVFS_URB_TYPE_ISO, USB_ENDPOINT_XFER_ISOC) => {
                track_stub!(TODO("https://fxbug.dev/571954283"), "usbfs isochronous URB");
                return error!(EINVAL);
            }
            _ => return error!(EINVAL),
        }
        let params = EndpointTransferParams::for_endpoint(&endpoint)?;
        check_urb_signal(urb)?;
        let charge = Arc::new(self.device.memory.charge(URB_SIZE + buffer_length)?);
        self.device.get_or_connect_endpoint(state, &endpoint)?;
        let is_bulk = endpoint_type == USB_ENDPOINT_XFER_BULK;
        Ok((UrbTarget::Endpoint { endpoint, params, is_bulk }, Some(ifno), charge))
    }

    fn submit_urb(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
        is_32bit: bool,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        let urb = read_urb_header(current_task, user_addr, is_32bit)?;
        let urb_type = u32::from(urb.type_);
        let allowed_flags = if urb_type == USBDEVFS_URB_TYPE_ISO {
            URB_FLAGS | USBDEVFS_URB_ISO_ASAP
        } else {
            URB_FLAGS
        };
        if urb.flags & !allowed_flags != 0 {
            return error!(EINVAL);
        }
        let buffer_length = usize::try_from(urb.buffer_length)
            .ok()
            .filter(|&len| len < USBFS_XFER_MAX)
            .ok_or_else(|| errno!(EINVAL))?;
        if buffer_length > 0 {
            if urb.buffer.is_null() {
                return error!(EINVAL);
            }
            let end_addr = (urb.buffer + buffer_length).map_err(|_| errno!(EFAULT))?;
            if current_task.maximum_valid_address().is_some_and(|max| end_addr > max) {
                return error!(EFAULT);
            }
        }
        let is_default_control =
            urb_type == USBDEVFS_URB_TYPE_CONTROL && (u32::from(urb.endpoint) & !USB_DIR_IN) == 0;
        // The setup packet is read before taking the state lock, which must not be held while
        // accessing user memory.
        let control_setup = if is_default_control {
            Some(read_control_urb_setup(current_task, &urb, buffer_length)?)
        } else {
            None
        };
        #[cfg(test)]
        let on_urb_prepared;
        let (target, claimed_ifno, charge) = {
            let mut state = self.device.state.lock();
            #[cfg(test)]
            {
                on_urb_prepared = state.on_urb_prepared.take();
            }
            self.prepare_urb(&mut state, &urb, buffer_length, control_setup)?
        };
        #[cfg(test)]
        if let Some(hook) = on_urb_prepared {
            hook(current_task);
        }

        let (endpoint, data_addr, length, is_in) = match &target {
            UrbTarget::Control { setup } => (
                0,
                (urb.buffer + USB_SETUP_PACKET_SIZE)?,
                usize::from(setup.w_length),
                // Like Linux, a control URB without data stage is an OUT transfer.
                (u32::from(setup.bm_request_type) & USB_DIR_IN) != 0 && setup.w_length > 0,
            ),
            UrbTarget::Endpoint { .. } => (
                urb.endpoint,
                urb.buffer,
                buffer_length,
                (u32::from(urb.endpoint) & USB_DIR_IN) != 0,
            ),
        };
        let out_data = if !is_in && length > 0 {
            current_task.read_memory_to_vec(data_addr, length)?
        } else {
            Vec::new()
        };
        let cancellation = Arc::new(UrbCancellation::default());

        let start_worker = {
            // Re-verify under `state` that the device was not disconnected, the interface claim
            // was not released, and the endpoint was not disabled or reconfigured while reading
            // user memory.
            let mut state = self.device.state.lock();
            if self.device.disconnected.load(Ordering::Acquire) {
                return error!(ENODEV);
            }
            if let Some(ifno) = claimed_ifno {
                if state.claims.get(&ifno) != Some(&self.id) {
                    return error!(EINVAL);
                }
            }
            let (transfer, client, is_bulk) = match target {
                UrbTarget::Control { setup } => {
                    (UrbTransfer::Control { setup, out_data }, None, false)
                }
                UrbTarget::Endpoint { endpoint: prepared_endpoint, params, is_bulk } => {
                    match self.device.active_endpoint(&state, endpoint) {
                        Some(active) if *active == prepared_endpoint => {}
                        Some(_) => return error!(EINVAL),
                        None => return error!(ENOENT),
                    }
                    let client =
                        self.device.get_or_connect_endpoint(&mut state, &prepared_endpoint)?;
                    let zero_packet = !is_in && (urb.flags & USBDEVFS_URB_ZERO_PACKET) != 0;
                    let transfer = UrbTransfer::Endpoint {
                        client: Arc::clone(&client),
                        params,
                        out_data,
                        zero_packet,
                    };
                    (transfer, Some(client), is_bulk)
                }
            };
            let bulk_continuation = is_bulk && (urb.flags & USBDEVFS_URB_BULK_CONTINUATION) != 0;
            let mut book = self.urbs.book.lock();
            if self.device.disconnected.load(Ordering::Acquire) {
                return error!(ENODEV);
            }
            if is_bulk {
                if !bulk_continuation {
                    // A new transfer starts.
                    book.disabled_bulk_endpoints.remove(&endpoint);
                } else if book.disabled_bulk_endpoints.contains(&endpoint) {
                    // An earlier URB of the transfer failed.
                    return error!(EREMOTEIO);
                }
            }
            let id = book.next_id;
            book.next_id += 1;
            book.pending.insert(
                id,
                PendingUrb {
                    user_addr,
                    is_32bit,
                    endpoint,
                    data_addr,
                    cancellation: Arc::clone(&cancellation),
                    client,
                    completed: Arc::new(zx::Event::create()),
                    charge: Arc::clone(&charge),
                },
            );
            let queue = book.queues.entry(endpoint).or_default();
            queue.queued.push_back(QueuedUrb {
                id,
                transfer,
                is_in,
                length,
                short_not_ok: is_in && (urb.flags & USBDEVFS_URB_SHORT_NOT_OK) != 0,
                is_bulk,
                bulk_continuation,
                cancellation,
                _charge: charge,
            });
            !std::mem::replace(&mut queue.has_worker, true)
        };
        if start_worker {
            let urbs = Arc::clone(&self.urbs);
            let device = Arc::clone(&self.device);
            let closure = move |current_task: &CurrentTask| {
                urbs.run_worker(&device, endpoint, current_task);
            };
            let request = SpawnRequestBuilder::new()
                .with_sync_closure(closure)
                .with_debug_name("usbfs-urb-worker")
                .build();
            current_task.kernel().kthreads.spawner().spawn_from_request(request);
        }
        Ok(SUCCESS)
    }

    fn discard_urb(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        let completed = {
            let mut book = self.urbs.book.lock();
            // Like Linux, the first pending URB with this address is discarded.
            let id = book
                .pending
                .iter()
                .find(|(_, urb)| urb.user_addr == user_addr)
                .map(|(&id, _)| id)
                .ok_or_else(|| errno!(EINVAL))?;
            UsbfsFileUrbs::kill_locked(&mut book, id, UrbCancelReason::Killed)
        };
        // Wait for the URB to complete. Its cancelled request completes within the cancel drain
        // timeout, and only fatal signals interrupt the wait.
        if let Some(completed) = completed {
            wait_for_signals(
                current_task,
                &[(completed.as_ref(), zx::Signals::USER_0)],
                zx::MonotonicInstant::INFINITE,
            )?;
        } else {
            self.urbs.notify_completion();
        }
        Ok(SUCCESS)
    }

    pub(crate) fn reap_urb(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
        is_32bit: bool,
        blocking: bool,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        loop {
            let waiter = Waiter::new();
            if blocking {
                self.urbs.waiters.wait_async(&waiter);
            }
            {
                let mut book = self.urbs.book.lock();
                if let Some(urb) = book.completed.pop_front() {
                    drop(book);
                    return self.write_reaped_urb(current_task, user_addr, urb, is_32bit);
                }
                // After a disconnection, the pending URBs complete shortly with `-ESHUTDOWN`
                // and can still be reaped.
                if self.device.disconnected.load(Ordering::Acquire) && book.pending.is_empty() {
                    return error!(ENODEV);
                }
            }
            if !blocking {
                return error!(EAGAIN);
            }
            waiter.wait(current_task)?;
        }
    }

    /// Claims interface `ifno` for this file descriptor:
    /// - Already claimed by this FD -> `Ok(())`
    /// - Interface does not exist in active configuration -> `ENOENT`
    /// - Claimed by another FD -> `EBUSY`
    fn claim_interface(&self, state: &mut DeviceState, ifno: Ifno) -> Result<(), Errno> {
        match state.claims.get(&ifno) {
            Some(&owner) if owner == self.id => return Ok(()),
            Some(_) => return error!(EBUSY),
            None => {}
        }
        if !self.device.has_interface(state, ifno) {
            return error!(ENOENT);
        }
        state.claims.insert(ifno, self.id);
        Ok(())
    }

    /// Releases interface `ifno` from this file descriptor:
    /// - Interface does not exist in active configuration -> `ENOENT`
    /// - Not claimed by this FD -> `EINVAL`
    fn release_interface(&self, state: &mut DeviceState, ifno: Ifno) -> Result<(), Errno> {
        if !self.device.has_interface(state, ifno) {
            return error!(ENOENT);
        }
        if state.claims.get(&ifno) != Some(&self.id) {
            return error!(EINVAL);
        }
        self.device.drop_claim(state, ifno);
        Ok(())
    }

    /// Ensures interface `ifnum` is claimed by this file descriptor, auto-claiming it if
    /// currently unclaimed. Returns `EHOSTUNREACH` if the device is unconfigured, then `EINVAL`
    /// if `ifnum` is out of range.
    fn check_interface(&self, state: &mut DeviceState, ifnum: u32) -> Result<Ifno, Errno> {
        if self.device.active_config(state).is_none() {
            return error!(EHOSTUNREACH);
        }
        let ifno = Ifno::try_from(ifnum)?;
        self.claim_interface(state, ifno)?;
        Ok(ifno)
    }

    /// Checks that this file descriptor may issue a control transfer to the recipient selected
    /// by `request_type` and `index`, auto-claiming the target interface if needed and returning
    /// its interface number when claimed.
    fn check_control_recipient(
        &self,
        state: &mut DeviceState,
        request_type: u8,
        request: u8,
        index: u16,
    ) -> Result<Option<Ifno>, Errno> {
        let request_type = u32::from(request_type);
        if (request_type & USB_TYPE_MASK) == USB_TYPE_VENDOR {
            return Ok(None);
        }
        let [index_high, index_low] = index.to_be_bytes();
        // The printer class GET_DEVICE_ID request is always allowed: it is used to query
        // things like ink levels.
        if request_type == USB_PRINTER_GET_DEVICE_ID_REQUEST_TYPE
            && request == USB_PRINTER_GET_DEVICE_ID
            && self
                .device
                .find_interface_alt(state, index_high, index_low)
                .is_some_and(|i| u32::from(i.interface_class) == USB_CLASS_PRINTER)
        {
            return Ok(None);
        }
        let index = u32::from(index_low);
        match request_type & USB_RECIP_MASK {
            USB_RECIP_ENDPOINT => {
                if (index & !USB_DIR_IN) == 0 {
                    return Ok(None);
                }
                // Some applications pass the endpoint number without the direction bit.
                let ifno = self.device.find_interface_for_endpoint(state, index).or_else(|_| {
                    self.device.find_interface_for_endpoint(state, index ^ USB_DIR_IN)
                })?;
                self.check_interface(state, ifno.into()).map(Some)
            }
            USB_RECIP_INTERFACE => self.check_interface(state, index).map(Some),
            _ => Ok(None),
        }
    }

    fn control(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
        is_arch32: bool,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        let ctrl_ref = if is_arch32 {
            UsbdevfsCtrltransferPtr::from_32(UserRef::new(user_addr))
        } else {
            UsbdevfsCtrltransferPtr::new(current_task, user_addr)
        };
        let ctrl: usbdevfs_ctrltransfer = current_task.read_multi_arch_object(ctrl_ref)?;
        self.check_control_recipient(
            &mut self.device.state.lock(),
            ctrl.bRequestType,
            ctrl.bRequest,
            ctrl.wIndex,
        )?;

        let length = usize::from(ctrl.wLength);
        if length > MAX_USBDEVFS_CONTROL_SIZE {
            return error!(EINVAL);
        }
        let data_addr = UserAddress::from(ctrl.data);
        if length > 0 && data_addr.is_null() {
            return error!(EFAULT);
        }
        let is_in = (u32::from(ctrl.bRequestType) & USB_DIR_IN) != 0;
        let setup = fusb_descriptor::UsbSetup {
            bm_request_type: ctrl.bRequestType,
            b_request: ctrl.bRequest,
            w_value: ctrl.wValue,
            w_index: ctrl.wIndex,
            w_length: ctrl.wLength,
        };
        let timeout = usbfs_timeout(ctrl.timeout);

        if is_in {
            let read_data =
                self.device.control_transfer(current_task, setup, timeout, Vec::new())?;
            let len = min(read_data.len(), length);
            if len > 0 {
                current_task.write_memory(data_addr, &read_data[..len])?;
            }
            Ok(len.into())
        } else {
            let out_data = if length > 0 {
                current_task.read_memory_to_vec(data_addr, length)?
            } else {
                Vec::new()
            };
            self.device.control_transfer(current_task, setup, timeout, out_data)?;
            // The Usb protocol does not report the number of bytes sent by an OUT control
            // transfer: on success, the device accepted all of them.
            Ok(length.into())
        }
    }

    fn bulk(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
        is_arch32: bool,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        let bulk_ref = if is_arch32 {
            UsbdevfsBulktransferPtr::from_32(UserRef::new(user_addr))
        } else {
            UsbdevfsBulktransferPtr::new(current_task, user_addr)
        };
        let bulk: usbdevfs_bulktransfer = current_task.read_multi_arch_object(bulk_ref)?;
        let len = bulk.len as usize;
        let data_addr = UserAddress::from(bulk.data);

        let (client, params, _charge) = {
            let mut state = self.device.state.lock();
            let ifno = self.device.find_interface_for_endpoint(&state, bulk.ep)?;
            self.check_interface(&mut state, ifno.into())?;
            if len >= INVALID_USBDEVFS_BULK_SIZE {
                return error!(EINVAL);
            }
            let endpoint = self
                .device
                .active_endpoint(&state, bulk.ep as u8)
                .cloned()
                .ok_or_else(|| errno!(EINVAL))?;
            let params = EndpointTransferParams::for_endpoint(&endpoint)?;
            // usbfs limits the memory used by outstanding transfers.
            let charge = self.device.memory.charge(URB_SIZE + len)?;
            if len > 0 && data_addr.is_null() {
                return error!(EFAULT);
            }
            let client = self.device.get_or_connect_endpoint(&mut state, &endpoint)?;
            (client, params, charge)
        };
        let deadline = deadline_after(usbfs_timeout(bulk.timeout));

        if (bulk.ep & USB_DIR_IN) != 0 {
            let data = self
                .device
                .endpoint_bulk_in(current_task, &client, params, len, deadline, None)
                .into_result()?;
            if !data.is_empty() {
                current_task.write_memory(data_addr, &data)?;
            }
            Ok(data.len().into())
        } else {
            let out_data =
                if len > 0 { current_task.read_memory_to_vec(data_addr, len)? } else { Vec::new() };
            let written = self
                .device
                .endpoint_bulk_out(current_task, &client, params, &out_data, false, deadline, None)
                .into_result()?;
            Ok(written.into())
        }
    }

    fn set_interface(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let set_iface: usbdevfs_setinterface =
            current_task.read_object(UserRef::new(UserAddress::from(arg)))?;

        let mut state = self.device.state.lock();
        if self.device.active_config(&state).is_none() {
            return error!(EHOSTUNREACH);
        }
        let ifno = Ifno::try_from(set_iface.interface)?;
        if !self.device.has_interface(&state, ifno) {
            return error!(ENOENT);
        }
        // Validate the alternate setting before auto-claiming the interface.
        let alt = u8::try_from(set_iface.altsetting)
            .ok()
            .filter(|&alt| self.device.find_interface_alt(&state, ifno.as_u8(), alt).is_some())
            .ok_or_else(|| errno!(EINVAL))?;
        self.claim_interface(&mut state, ifno)?;

        // Kill the pending URBs on the interface's endpoints before changing the alternate
        // setting. Only endpoints of the interface being changed are affected; all of them are
        // disabled, even those also present in the new alternate setting, because the device
        // resets their data toggles. They are enabled again with the new descriptors on next use.
        let endpoints = self.device.interface_endpoint_addresses(&state, ifno);
        let running = self
            .urbs
            .start_kill_urbs(UrbCancelReason::Killed, |urb| endpoints.contains(&urb.endpoint));
        let mut disable_result = Ok(());
        for &ep_addr in &endpoints {
            if let Err(e) = self.device.disable_endpoint(&mut state, ep_addr) {
                if disable_result.is_ok() {
                    disable_result = Err(e);
                }
            }
        }
        self.urbs.finish_kill_urbs(running);
        disable_result?;
        let proxy = self.device.proxy()?;
        match proxy.set_interface(ifno.as_u8(), alt, zx::MonotonicInstant::INFINITE) {
            // Devices may STALL SET_INTERFACE on an interface with a single alternate setting
            // (USB 2.0 section 9.4.10). Like Linux, treat it as a success and clear the halt of
            // each endpoint of the interface instead, which resets their data toggles.
            Ok(Err(zx::sys::ZX_ERR_IO_REFUSED))
                if self.device.interface_alts(&state, ifno).count() == 1 =>
            {
                for ep_addr in endpoints {
                    let _ = proxy.control_transfer(
                        &clear_halt_setup(ep_addr),
                        CLEAR_HALT_TIMEOUT.into_nanos(),
                        &[],
                        zx::MonotonicInstant::INFINITE,
                    );
                }
            }
            result => fidl_result_to_errno(result)?,
        }
        state.alt_settings.insert(ifno, alt);
        Ok(SUCCESS)
    }

    fn set_configuration(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let raw_config: i32 = current_task.read_object(UserRef::new(UserAddress::from(arg)))?;

        let mut state = self.device.state.lock();
        // Claims are taken under the same lock, so no interface can be claimed concurrently.
        if !state.claims.is_empty() {
            return error!(EBUSY);
        }
        // -1 unconfigures the device, which USB represents as configuration value 0.
        let config = match raw_config {
            -1 => 0,
            value => u8::try_from(value).map_err(|_| errno!(EINVAL))?,
        };
        // Unknown configurations are rejected before touching the device. Whether the device
        // can be unconfigured is up to the USB driver.
        if config != 0 && !self.device.parsed_configs.iter().any(|c| c.config_value == config) {
            return error!(EINVAL);
        }
        self.device.disable_all_endpoints(&mut state)?;
        fidl_result_to_errno(
            self.device.proxy()?.set_configuration(config, zx::MonotonicInstant::INFINITE),
        )?;
        state.active_config = config;
        state.alt_settings.clear();
        Ok(SUCCESS)
    }

    fn clear_halt(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let ep: u32 = current_task.read_object(UserRef::new(UserAddress::from(arg)))?;
        let mut state = self.device.state.lock();
        let ifno = self.device.find_interface_for_endpoint(&state, ep)?;
        self.check_interface(&mut state, ifno.into())?;
        let ep_addr = ep as u8;
        // The request is not killable: once the device reset its data toggle, the host must reset
        // its own too.
        let proxy = self.device.proxy()?;
        fidl_result_to_errno(proxy.control_transfer(
            &clear_halt_setup(ep_addr),
            CLEAR_HALT_TIMEOUT.into_nanos(),
            &[],
            zx::MonotonicInstant::INFINITE,
        ))?;
        match proxy.reset_endpoint(ep_addr, zx::MonotonicInstant::INFINITE) {
            Ok(Ok(())) => self.device.reset_endpoint_state(ep_addr),
            // The host controller only resets halted endpoints, so it kept the data toggle of
            // this endpoint while the device reset its own. Disable the endpoint so that enabling
            // it again resets the host data toggle too; if asynchronous URBs are already pending
            // on the endpoint, restart it right away so their worker does not fail with
            // ESHUTDOWN.
            Ok(Err(zx::sys::ZX_ERR_NOT_SUPPORTED)) | Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)) => {
                if self.device.has_pending_urbs(ep_addr) {
                    self.device.restart_endpoint(&mut state, ep_addr)?;
                } else {
                    self.device.disable_endpoint(&mut state, ep_addr)?;
                }
            }
            result => fidl_result_to_errno(result)?,
        }
        Ok(SUCCESS)
    }

    fn get_driver(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_ref = UserRef::<usbdevfs_getdriver>::new(UserAddress::from(arg));
        let mut getdriver = current_task.read_object(user_ref)?;
        let claimed = {
            let state = self.device.state.lock();
            Ifno::try_from(getdriver.interface).is_ok_and(|ifno| {
                self.device.has_interface(&state, ifno) && state.claims.contains_key(&ifno)
            })
        };
        if !claimed {
            return error!(ENODATA);
        }
        getdriver.driver.fill(0);
        for (dst, src) in getdriver.driver.iter_mut().zip(USBFS_DRIVER_NAME) {
            *dst = *src as _;
        }
        current_task.write_object(user_ref, &getdriver)?;
        Ok(SUCCESS)
    }

    fn disconnect_claim(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let disconnect: usbdevfs_disconnect_claim =
            current_task.read_object(UserRef::new(UserAddress::from(arg)))?;
        let mut state = self.device.state.lock();
        let ifno = Ifno::try_from(disconnect.interface)?;
        if !self.device.has_interface(&state, ifno) {
            return error!(EINVAL);
        }
        // Interfaces claimed through usbfs are bound to the "usbfs" driver.
        if state.claims.contains_key(&ifno) {
            let driver: Vec<u8> =
                disconnect.driver.iter().map(|&c| c as u8).take_while(|&c| c != 0).collect();
            let is_usbfs = driver == USBFS_DRIVER_NAME;
            if (disconnect.flags & USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER) != 0 && !is_usbfs {
                return error!(EBUSY);
            }
            if (disconnect.flags & USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER) != 0 && is_usbfs {
                return error!(EBUSY);
            }
            // Disconnect the current claimant.
            self.device.drop_claim(&mut state, ifno);
        }
        self.claim_interface(&mut state, ifno)?;
        Ok(SUCCESS)
    }

    fn driver_ioctl(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
        is_arch32: bool,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        let ctl_ref = if is_arch32 {
            UsbdevfsIoctlPtr::from_32(UserRef::new(user_addr))
        } else {
            UsbdevfsIoctlPtr::new(current_task, user_addr)
        };
        let ctl: usbdevfs_ioctl = current_task.read_multi_arch_object(ctl_ref)?;
        let code = ctl.ioctl_code as u32;
        let size = ((code >> _IOC_SIZESHIFT) & _IOC_SIZEMASK) as usize;
        if size > 0 && ((code >> _IOC_DIRSHIFT) & _IOC_WRITE) != 0 {
            current_task.read_memory_to_vec(UserAddress::from(ctl.data), size)?;
        }
        let mut state = self.device.state.lock();
        if self.device.active_config(&state).is_none() {
            return error!(EHOSTUNREACH);
        }
        let ifno = u32::try_from(ctl.ifno).map_err(|_| errno!(EINVAL)).and_then(Ifno::try_from)?;
        if !self.device.has_interface(&state, ifno) {
            return error!(EINVAL);
        }
        match code {
            USBDEVFS_DISCONNECT => {
                if state.claims.contains_key(&ifno) {
                    self.device.drop_claim(&mut state, ifno);
                    Ok(SUCCESS)
                } else {
                    error!(ENODATA)
                }
            }
            USBDEVFS_CONNECT => {
                if state.claims.contains_key(&ifno) {
                    error!(EBUSY)
                } else {
                    // Starnix has no in-kernel USB interface drivers to bind, so return 0 as when
                    // no driver matches.
                    Ok(SUCCESS)
                }
            }
            _ => error!(ENOTTY),
        }
    }
}

impl Drop for UsbDeviceFile {
    fn drop(&mut self) {
        self.device.open_files.lock().remove(&self.id);
        // Kill and wait for the pending URBs of this file description before clearing any
        // un-reaped completions.
        self.urbs.kill_urbs(UrbCancelReason::Killed, |_| true);
        self.urbs.book.lock().completed.clear();

        let mut state = self.device.state.lock();
        let released: Vec<Ifno> = state
            .claims
            .iter()
            .filter(|&(_, &owner)| owner == self.id)
            .map(|(&ifno, _)| ifno)
            .collect();
        for ifno in released {
            self.device.drop_claim(&mut state, ifno);
        }
    }
}

impl FileOps for UsbDeviceFile {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.urbs.waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let mut events = FdEvents::empty();
        let has_completed = !self.urbs.book.lock().completed.is_empty();
        if file.can_write() && has_completed {
            events |= FdEvents::POLLOUT | FdEvents::POLLWRNORM;
        }
        if self.device.disconnected.load(Ordering::Acquire) {
            events |= FdEvents::POLLHUP | FdEvents::POLLERR;
        }
        Ok(events)
    }

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        if self.device.disconnected.load(Ordering::Acquire) {
            return error!(ENODEV);
        }
        let descriptors = &self.device.raw_descriptors;
        if offset >= descriptors.len() {
            return Ok(0);
        }
        data.write(&descriptors[offset..])
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }

    fn ioctl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        // All USBDEVFS_* ioctls require write permission on the open file description.
        if !file.can_write() {
            return error!(EPERM);
        }

        let is_arch32_call = current_task.is_arch32() || is_arch32_ioctl_request(request);
        let request = canonicalize_ioctl_request(is_arch32_call, request);

        // Completed URBs can still be reaped after the device is disconnected.
        if self.device.disconnected.load(Ordering::Acquire)
            && request != USBDEVFS_REAPURB
            && request != USBDEVFS_REAPURBNDELAY
        {
            return error!(ENODEV);
        }

        match request {
            USBDEVFS_CONTROL => self.control(current_task, arg, is_arch32_call),
            USBDEVFS_BULK => self.bulk(current_task, arg, is_arch32_call),
            USBDEVFS_CLAIMINTERFACE => {
                let ifno = read_ifno(current_task, arg)?;
                self.claim_interface(&mut self.device.state.lock(), ifno)?;
                Ok(SUCCESS)
            }
            USBDEVFS_RELEASEINTERFACE => {
                let ifno = read_ifno(current_task, arg)?;
                self.release_interface(&mut self.device.state.lock(), ifno)?;
                Ok(SUCCESS)
            }
            USBDEVFS_SETINTERFACE => self.set_interface(current_task, arg),
            USBDEVFS_SETCONFIGURATION => self.set_configuration(current_task, arg),
            USBDEVFS_CLEAR_HALT => self.clear_halt(current_task, arg),
            USBDEVFS_RESET => {
                // The Usb protocol has no method to reset the physical device. On real Linux, a
                // device reset force-unbinds usbfs interfaces and drops claims because usbfs lacks
                // reset callbacks. Since Fuchsia does not perform a hardware reset here, Starnix
                // preserves configuration and claims as a software stub, cancelling in-flight
                // requests.
                track_stub!(
                    TODO("https://fxbug.dev/571954283"),
                    "usbfs USBDEVFS_RESET device reset"
                );
                // Like a device reset, abort the URBs of all the file descriptions under the
                // state lock so no concurrent submission slips in, cancel in-flight endpoint
                // requests so both async URB workers and synchronous USBDEVFS_BULK callers wake
                // up, and wait for running URBs after releasing the state lock.
                let files_and_running: Vec<_> = {
                    let _state = self.device.state.lock();
                    let files_and_running: Vec<_> = self
                        .device
                        .open_files()
                        .into_iter()
                        .map(|urbs| {
                            let running = urbs.start_kill_urbs(UrbCancelReason::Killed, |_| true);
                            (urbs, running)
                        })
                        .collect();
                    let addresses: Vec<u8> = self.device.endpoints.lock().keys().copied().collect();
                    for ep_addr in addresses {
                        self.device.reset_endpoint_state(ep_addr);
                    }
                    files_and_running
                };
                for (urbs, running) in files_and_running {
                    urbs.finish_kill_urbs(running);
                }
                Ok(SUCCESS)
            }
            USBDEVFS_GETDRIVER => self.get_driver(current_task, arg),
            USBDEVFS_GET_SPEED => Ok((self.device.speed as usize).into()),
            USBDEVFS_GET_CAPABILITIES => {
                let caps: u32 = USBDEVFS_CAP_ZERO_PACKET
                    | USBDEVFS_CAP_BULK_CONTINUATION
                    | USBDEVFS_CAP_NO_PACKET_SIZE_LIM
                    | USBDEVFS_CAP_REAP_AFTER_DISCONNECT;
                current_task.write_object(UserRef::new(UserAddress::from(arg)), &caps)?;
                Ok(SUCCESS)
            }
            USBDEVFS_CONNECTINFO => {
                let info = usbdevfs_connectinfo {
                    devnum: u32::from(self.device.dev_num),
                    slow: u8::from(self.device.speed == USB_SPEED_LOW),
                    ..Default::default()
                };
                current_task.write_object(UserRef::new(UserAddress::from(arg)), &info)?;
                Ok(SUCCESS)
            }
            USBDEVFS_DISCONNECT_CLAIM => self.disconnect_claim(current_task, arg),
            USBDEVFS_IOCTL => self.driver_ioctl(current_task, arg, is_arch32_call),
            USBDEVFS_SUBMITURB => self.submit_urb(current_task, arg, is_arch32_call),
            USBDEVFS_DISCARDURB => self.discard_urb(current_task, arg),
            USBDEVFS_REAPURBNDELAY => self.reap_urb(current_task, arg, is_arch32_call, false),
            USBDEVFS_REAPURB => self.reap_urb(current_task, arg, is_arch32_call, true),
            _ => {
                track_stub!(TODO("https://fxbug.dev/571954283"), "usbfs ioctl", request);
                error!(ENOTTY)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_uapi::{
        USB_DT_CONFIG_SIZE, USB_DT_INTERFACE_ASSOCIATION_SIZE, USB_DT_INTERFACE_SIZE,
        USB_DT_SS_EP_COMP_SIZE,
    };

    const CONFIG: [u8; USB_DT_CONFIG_SIZE as usize] =
        [USB_DT_CONFIG_SIZE as u8, USB_DT_CONFIG as u8, 0, 0, 1, 7, 0, 0x80, 50];

    fn interface(number: u8, alt: u8) -> [u8; USB_DT_INTERFACE_SIZE as usize] {
        [USB_DT_INTERFACE_SIZE as u8, USB_DT_INTERFACE as u8, number, alt, 1, 0xff, 0, 0, 0]
    }

    fn endpoint(address: u8) -> [u8; USB_DT_ENDPOINT_SIZE as usize] {
        [
            USB_DT_ENDPOINT_SIZE as u8,
            USB_DT_ENDPOINT as u8,
            address,
            USB_ENDPOINT_XFER_BULK as u8,
            0x00,
            0x04,
            0,
        ]
    }

    fn ss_companion(max_burst: u8) -> [u8; USB_DT_SS_EP_COMP_SIZE as usize] {
        [USB_DT_SS_EP_COMP_SIZE as u8, USB_DT_SS_ENDPOINT_COMP as u8, max_burst, 0, 0, 0]
    }

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[::fuchsia::test]
    fn test_parse_configuration_interfaces_and_endpoints() {
        let data = concat(&[&CONFIG, &interface(2, 1), &endpoint(0x81)]);
        let configs = parse_config_descriptors(&data);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].config_value, 7);
        let interface = &configs[0].interfaces[0];
        assert_eq!((interface.interface_number, interface.alt_setting), (2, 1));
        assert_eq!(interface.interface_class, 0xff);
        let endpoint = &interface.endpoints[0];
        assert_eq!(endpoint.address(), 0x81);
        assert_eq!(endpoint.max_packet_size(), 1024);
        assert_eq!(endpoint.ss_companion, NO_SS_COMPANION);
    }

    #[::fuchsia::test]
    fn test_parse_ss_companion_attaches_to_preceding_endpoint() {
        // A class-specific descriptor between an endpoint and its companion is skipped.
        let class_specific = [3, 0x25, 0];
        let data = concat(&[
            &CONFIG,
            &interface(0, 0),
            &endpoint(0x81),
            &class_specific,
            &ss_companion(3),
            &endpoint(0x02),
            &ss_companion(5),
        ]);
        let configs = parse_config_descriptors(&data);
        let endpoints = &configs[0].interfaces[0].endpoints;
        assert_eq!(endpoints[0].ss_companion.b_max_burst, 3);
        assert_eq!(endpoints[0].ss_companion.b_length, USB_DT_SS_EP_COMP_SIZE as u8);
        assert_eq!(endpoints[1].ss_companion.b_max_burst, 5);
    }

    #[::fuchsia::test]
    fn test_parse_ss_companion_ignored_outside_endpoint_scope() {
        let iad = [USB_DT_INTERFACE_ASSOCIATION_SIZE as u8, USB_DT_INTERFACE_ASSOCIATION as u8];
        let iad = concat(&[&iad, &[0; USB_DT_INTERFACE_ASSOCIATION_SIZE as usize - 2]]);
        // A companion after an interface association descriptor, or after an interface
        // descriptor, does not belong to the previous endpoint.
        let data = concat(&[
            &CONFIG,
            &interface(0, 0),
            &endpoint(0x81),
            &iad,
            &ss_companion(3),
            &interface(1, 0),
            &ss_companion(4),
        ]);
        let configs = parse_config_descriptors(&data);
        assert_eq!(configs[0].interfaces.len(), 2);
        let endpoint = &configs[0].interfaces[0].endpoints[0];
        assert_eq!(endpoint.ss_companion, NO_SS_COMPANION);
    }

    #[::fuchsia::test]
    fn test_parse_ignores_descriptors_without_parent() {
        // An interface before any configuration and an endpoint before any interface.
        let data = concat(&[&interface(0, 0), &endpoint(0x81), &CONFIG, &endpoint(0x82)]);
        let configs = parse_config_descriptors(&data);
        assert_eq!(configs.len(), 1);
        assert!(configs[0].interfaces.is_empty());
    }

    #[::fuchsia::test]
    fn test_parse_stops_at_malformed_descriptors() {
        // A descriptor with a bLength smaller than its header ends the parsing.
        let data = concat(&[&CONFIG, &[1, USB_DT_INTERFACE as u8], &interface(0, 0)]);
        assert!(parse_config_descriptors(&data)[0].interfaces.is_empty());

        // So does a truncated descriptor.
        let data = concat(&[&CONFIG, &interface(0, 0)[..5]]);
        assert!(parse_config_descriptors(&data)[0].interfaces.is_empty());
        assert!(parse_config_descriptors(&CONFIG[..1]).is_empty());

        // Descriptors shorter than their standard size are skipped.
        let short_interface = [5, USB_DT_INTERFACE as u8, 0, 0, 0];
        let short_endpoint = [5, USB_DT_ENDPOINT as u8, 0x81, 2, 0];
        let data = concat(&[&CONFIG, &short_interface, &interface(1, 0), &short_endpoint]);
        let configs = parse_config_descriptors(&data);
        assert_eq!(configs[0].interfaces.len(), 1);
        assert_eq!(configs[0].interfaces[0].interface_number, 1);
        assert!(configs[0].interfaces[0].endpoints.is_empty());
    }

    #[::fuchsia::test]
    fn test_memory_budget_limits_outstanding_charges() {
        let budget = Arc::new(UsbfsMemoryBudget::default());
        let first = budget.charge(USBFS_MEMORY_LIMIT - 1).unwrap();
        assert_eq!(budget.charge(2).unwrap_err(), errno!(ENOMEM));
        let second = budget.charge(1).unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), USBFS_MEMORY_LIMIT);
        drop(first);
        drop(second);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert_eq!(budget.charge(USBFS_MEMORY_LIMIT + 1).unwrap_err(), errno!(ENOMEM));
    }

    /// Adds a pending URB queued on endpoint `EP` to `book`.
    fn queue_test_urb(
        book: &mut UrbBook,
        budget: &Arc<UsbfsMemoryBudget>,
        continuation: bool,
    ) -> u64 {
        const EP: u8 = 0x81;
        let id = book.next_id;
        book.next_id += 1;
        let cancellation = Arc::new(UrbCancellation::default());
        let charge = Arc::new(budget.charge(URB_SIZE).unwrap());
        book.pending.insert(
            id,
            PendingUrb {
                user_addr: UserAddress::from(0x1000 + id * 0x100),
                is_32bit: false,
                endpoint: EP,
                data_addr: UserAddress::NULL,
                cancellation: Arc::clone(&cancellation),
                client: None,
                completed: Arc::new(zx::Event::create()),
                charge: Arc::clone(&charge),
            },
        );
        book.queues.entry(EP).or_default().queued.push_back(QueuedUrb {
            id,
            transfer: UrbTransfer::Control {
                setup: fusb_descriptor::UsbSetup {
                    bm_request_type: 0,
                    b_request: 0,
                    w_value: 0,
                    w_index: 0,
                    w_length: 0,
                },
                out_data: Vec::new(),
            },
            is_in: true,
            length: 0,
            short_not_ok: false,
            is_bulk: true,
            bulk_continuation: continuation,
            cancellation,
            _charge: charge,
        });
        id
    }

    #[::fuchsia::test]
    fn test_bulk_failure_unlinks_queued_continuation_urbs() {
        const EP: u8 = 0x81;
        let budget = Arc::new(UsbfsMemoryBudget::default());
        let unlinked_status = UrbCancelReason::Unlinked.status();

        // The continuations of the failed transfer are unlinked, up to the next transfer.
        let mut book = UrbBook::default();
        let continuations =
            [queue_test_urb(&mut book, &budget, true), queue_test_urb(&mut book, &budget, true)];
        let next_transfer = queue_test_urb(&mut book, &budget, false);
        let next_continuation = queue_test_urb(&mut book, &budget, true);
        UsbfsFileUrbs::unlink_bulk_continuations_locked(&mut book, EP);
        let completed: Vec<(UserAddress, i32)> =
            book.completed.iter().map(|urb| (urb.user_addr, urb.result.status)).collect();
        assert_eq!(
            completed,
            continuations
                .iter()
                .map(|&id| (UserAddress::from(0x1000 + id * 0x100), unlinked_status))
                .collect::<Vec<_>>()
        );
        let queued: Vec<u64> = book.queues[&EP].queued.iter().map(|urb| urb.id).collect();
        assert_eq!(queued, vec![next_transfer, next_continuation]);
        assert!(!book.disabled_bulk_endpoints.contains(&EP));

        // Without a new transfer, later continuations are rejected.
        let mut book = UrbBook::default();
        queue_test_urb(&mut book, &budget, true);
        UsbfsFileUrbs::unlink_bulk_continuations_locked(&mut book, EP);
        assert_eq!(book.completed.len(), 1);
        assert!(book.queues[&EP].queued.is_empty());
        assert!(book.disabled_bulk_endpoints.contains(&EP));
    }
}
