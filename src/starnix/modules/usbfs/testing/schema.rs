// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared data schema for Starnix `usbfs` declarative scenarios, recorded golden traces,
//! gadget control RPCs, and documented Starnix-vs-Linux divergences.

pub mod scenarios;
pub use scenarios::{DIVERGENCES_JSON, SCENARIOS};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Default loopback TCP address used by `usb_test` and `usbfs_trace_recorder` to control
/// either `fake_usb_device` (on Starnix) or `linux_ffs_daemon` (on Linux).
pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:28765";

fn default_bcd_usb() -> u16 {
    0x0200
}

fn default_bcd_device() -> u16 {
    0x0543
}

fn default_max_packet_size0() -> u8 {
    64
}

fn default_speed() -> String {
    "high".to_string()
}

fn default_config_value() -> u8 {
    1
}

fn default_bm_attributes() -> u8 {
    0x80
}

fn default_max_power_ma() -> u16 {
    100
}

fn default_interface_class() -> u8 {
    0xff
}

fn default_max_packet_size() -> u16 {
    512
}

/// Per-scenario USB device descriptor and configuration topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSpec {
    pub vendor_id: u16,
    pub product_id: u16,
    #[serde(default = "default_bcd_usb")]
    pub bcd_usb: u16,
    #[serde(default = "default_bcd_device")]
    pub bcd_device: u16,
    #[serde(default)]
    pub device_class: u8,
    #[serde(default)]
    pub device_subclass: u8,
    #[serde(default)]
    pub device_protocol: u8,
    #[serde(default = "default_max_packet_size0")]
    pub max_packet_size0: u8,
    #[serde(default = "default_speed")]
    pub speed: String,
    #[serde(default)]
    pub configs: Vec<ConfigSpec>,
}

impl Default for DeviceSpec {
    fn default() -> Self {
        Self {
            vendor_id: 0x1234,
            product_id: 0x5678,
            bcd_usb: 0x0200,
            bcd_device: 0x0543,
            device_class: 0x00,
            device_subclass: 0x00,
            device_protocol: 0x00,
            max_packet_size0: 64,
            speed: "high".to_string(),
            configs: vec![ConfigSpec {
                config_value: 1,
                bm_attributes: 0x80,
                max_power_ma: 100,
                interfaces: vec![InterfaceSpec {
                    interface_number: 0,
                    alt_setting: 0,
                    interface_class: 0xff,
                    interface_subclass: 0x00,
                    interface_protocol: 0x00,
                    endpoints: vec![
                        EndpointSpec {
                            address: 0x81,
                            xfer_type: "bulk".to_string(),
                            max_packet_size: 512,
                            interval: 0,
                        },
                        EndpointSpec {
                            address: 0x02,
                            xfer_type: "bulk".to_string(),
                            max_packet_size: 512,
                            interval: 0,
                        },
                    ],
                }],
            }],
        }
    }
}

/// USB configuration descriptor specification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSpec {
    #[serde(default = "default_config_value")]
    pub config_value: u8,
    #[serde(default = "default_bm_attributes")]
    pub bm_attributes: u8,
    #[serde(default = "default_max_power_ma")]
    pub max_power_ma: u16,
    #[serde(default)]
    pub interfaces: Vec<InterfaceSpec>,
}

/// USB interface alternate-setting specification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceSpec {
    #[serde(default)]
    pub interface_number: u8,
    #[serde(default)]
    pub alt_setting: u8,
    #[serde(default = "default_interface_class")]
    pub interface_class: u8,
    #[serde(default)]
    pub interface_subclass: u8,
    #[serde(default)]
    pub interface_protocol: u8,
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
}

/// USB endpoint descriptor specification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub address: u8,
    pub xfer_type: String,
    #[serde(default = "default_max_packet_size")]
    pub max_packet_size: u16,
    #[serde(default)]
    pub interval: u8,
}

impl EndpointSpec {
    /// Returns the `bmAttributes` transfer type bits (0=Control, 1=Iso, 2=Bulk, 3=Interrupt).
    pub fn bm_attributes(&self) -> u8 {
        match self.xfer_type.as_str() {
            "control" => 0x00,
            "iso" | "isochronous" => 0x01,
            "bulk" => 0x02,
            "interrupt" => 0x03,
            _ => 0x02,
        }
    }

    /// Serializes a 7-byte standard USB endpoint descriptor (`USB_DT_ENDPOINT = 0x05`).
    pub fn to_usb_descriptor_bytes(&self) -> [u8; 7] {
        self.to_usb_descriptor_bytes_with_mps(self.max_packet_size)
    }

    /// Serializes a 7-byte USB endpoint descriptor with an explicit `wMaxPacketSize`.
    pub fn to_usb_descriptor_bytes_with_mps(&self, mps: u16) -> [u8; 7] {
        let mps_le = mps.to_le_bytes();
        [
            7, // bLength
            5, // bDescriptorType = USB_DT_ENDPOINT
            self.address,
            self.bm_attributes(),
            mps_le[0],
            mps_le[1],
            self.interval,
        ]
    }

    /// Serializes a 7-byte FunctionFS `usb_endpoint_descriptor_no_audio` (`bLength = 7`).
    pub fn to_ffs_descriptor_no_audio_bytes(&self, mps: u16) -> [u8; 7] {
        let mps_le = mps.to_le_bytes();
        let interval = if self.bm_attributes() == 0x03 || self.bm_attributes() == 0x01 {
            self.interval.max(1)
        } else {
            self.interval
        };
        [
            7, // bLength = USB_DT_ENDPOINT_SIZE (no audio bRefresh/bSynchAddress)
            5, // bDescriptorType = USB_DT_ENDPOINT
            self.address,
            self.bm_attributes(),
            mps_le[0],
            mps_le[1],
            interval,
        ]
    }
}

impl InterfaceSpec {
    /// Serializes a 9-byte standard USB interface descriptor (`USB_DT_INTERFACE = 0x04`).
    pub fn to_interface_header_bytes(&self) -> [u8; 9] {
        [
            9, // bLength
            4, // bDescriptorType = USB_DT_INTERFACE
            self.interface_number,
            self.alt_setting,
            self.endpoints.len() as u8,
            self.interface_class,
            self.interface_subclass,
            self.interface_protocol,
            0, // iInterface
        ]
    }
}

impl DeviceSpec {
    /// Serializes the 18-byte USB 2.0 Device Descriptor (`USB_DT_DEVICE = 0x01`).
    pub fn build_device_descriptor_bytes(&self) -> [u8; 18] {
        let bcd_usb = self.bcd_usb.to_le_bytes();
        let vid = self.vendor_id.to_le_bytes();
        let pid = self.product_id.to_le_bytes();
        let bcd_dev = self.bcd_device.to_le_bytes();
        [
            18, // bLength
            1,  // bDescriptorType = USB_DT_DEVICE
            bcd_usb[0],
            bcd_usb[1],
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.max_packet_size0,
            vid[0],
            vid[1],
            pid[0],
            pid[1],
            bcd_dev[0],
            bcd_dev[1],
            1, // iManufacturer
            2, // iProduct
            3, // iSerialNumber
            self.configs.len() as u8,
        ]
    }

    /// Serializes a single USB Configuration descriptor matching `bConfigurationValue`.
    pub fn build_single_config_descriptor_bytes(&self, config_value: u8) -> Option<Vec<u8>> {
        let cfg = self.configs.iter().find(|c| c.config_value == config_value)?;
        let mut body = Vec::new();
        let mut distinct_ifaces = std::collections::BTreeSet::new();
        for iface in &cfg.interfaces {
            distinct_ifaces.insert(iface.interface_number);
            body.extend_from_slice(&iface.to_interface_header_bytes());
            for ep in &iface.endpoints {
                body.extend_from_slice(&ep.to_usb_descriptor_bytes());
            }
        }
        let total_len = (9 + body.len()) as u16;
        let total_len_le = total_len.to_le_bytes();
        let num_ifaces = distinct_ifaces.len() as u8;
        let max_power_units = (cfg.max_power_ma / 2).min(u16::from(u8::MAX)) as u8;
        let hdr = [
            9, // bLength
            2, // bDescriptorType = USB_DT_CONFIG
            total_len_le[0],
            total_len_le[1],
            num_ifaces,
            cfg.config_value,
            0, // iConfiguration
            cfg.bm_attributes,
            max_power_units,
        ];
        let mut out = Vec::with_capacity(9 + body.len());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(&body);
        Some(out)
    }

    /// Serializes the concatenated USB Configuration, Interface, and Endpoint descriptors.
    pub fn build_config_descriptor_bytes(&self) -> Vec<u8> {
        let mut all_configs = Vec::new();
        for cfg in &self.configs {
            if let Some(bytes) = self.build_single_config_descriptor_bytes(cfg.config_value) {
                all_configs.extend_from_slice(&bytes);
            }
        }
        all_configs
    }

    /// Serializes the full raw USB descriptor stream returned by `read()` on a `usbfs` device fd
    /// or `/sys/bus/usb/devices/<dev>/descriptors`.
    pub fn build_raw_descriptors(&self) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&self.build_device_descriptor_bytes());
        raw.extend_from_slice(&self.build_config_descriptor_bytes());
        raw
    }

    /// Validates that this [`DeviceSpec`] satisfies the invariants required by FunctionFS
    /// (`linux_ffs_daemon`), `fake_usb_device`, and `starnix_replayer`.
    pub fn validate_for_ffs(&self) -> Result<(), String> {
        if self.configs.len() != 1 {
            return Err(format!(
                "FunctionFS supports exactly 1 configuration, got {}",
                self.configs.len()
            ));
        }
        match self.speed.to_ascii_lowercase().as_str() {
            "low" | "full" | "high" | "super" | "super_plus" | "super-plus" => {}
            other => return Err(format!("Unsupported USB speed '{other}'")),
        }
        for iface in &self.configs[0].interfaces {
            for ep in &iface.endpoints {
                match ep.xfer_type.as_str() {
                    "bulk" => {
                        if ep.interval != 0 {
                            return Err(format!(
                                "Bulk endpoint 0x{:02x} must have interval == 0, got {}",
                                ep.address, ep.interval
                            ));
                        }
                        if self.speed.eq_ignore_ascii_case("high") && ep.max_packet_size != 512 {
                            return Err(format!(
                                "High-speed bulk endpoint 0x{:02x} must have max_packet_size == 512, got {}",
                                ep.address, ep.max_packet_size
                            ));
                        }
                    }
                    "interrupt" | "iso" | "isochronous" => {
                        if ep.interval == 0 {
                            return Err(format!(
                                "Endpoint 0x{:02x} ({}) must have interval >= 1, got 0",
                                ep.address, ep.xfer_type
                            ));
                        }
                    }
                    "control" => {}
                    other => {
                        return Err(format!(
                            "Unsupported endpoint xfer_type '{other}' on 0x{:02x}",
                            ep.address
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Serializes a Linux FunctionFS `usb_functionfs_descs_head_v2` (`magic = 3`) payload
    /// containing Full-Speed, High-Speed, and SuperSpeed descriptors for config 0, along with
    /// the ordered list of unique endpoints (`ep1..epN`).
    pub fn build_ffs_descriptors_v2_and_eps(&self) -> (Vec<u8>, Vec<EndpointSpec>) {
        const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
        const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
        const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
        const FUNCTIONFS_HAS_SS_DESC: u32 = 4;
        const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 64;
        const FUNCTIONFS_CONFIG0_SETUP: u32 = 128;

        let mut fs_descs = Vec::new();
        let mut hs_descs = Vec::new();
        let mut ss_descs = Vec::new();
        let mut fs_count: u32 = 0;
        let mut hs_count: u32 = 0;
        let mut ss_count: u32 = 0;
        let mut unique_eps: Vec<EndpointSpec> = Vec::new();

        if let Some(cfg) = self.configs.first() {
            for iface in &cfg.interfaces {
                let iface_desc = iface.to_interface_header_bytes();
                fs_descs.extend_from_slice(&iface_desc);
                hs_descs.extend_from_slice(&iface_desc);
                ss_descs.extend_from_slice(&iface_desc);
                fs_count += 1;
                hs_count += 1;
                ss_count += 1;

                for ep in &iface.endpoints {
                    if !unique_eps.iter().any(|e| e.address == ep.address) {
                        unique_eps.push(ep.clone());
                    }
                    let bm_attr = ep.bm_attributes();
                    let fs_mps = ep.max_packet_size.min(64);
                    let hs_mps = ep.max_packet_size;
                    let ss_mps = if bm_attr == 0x02 { 1024 } else { ep.max_packet_size };

                    let fs_ep = ep.to_ffs_descriptor_no_audio_bytes(fs_mps);
                    let hs_ep = ep.to_ffs_descriptor_no_audio_bytes(hs_mps);
                    let ss_ep = ep.to_ffs_descriptor_no_audio_bytes(ss_mps);
                    let ss_comp_bytes_per_interval: u16 =
                        if bm_attr == 0x01 || bm_attr == 0x03 { ss_mps } else { 0 };
                    let ss_comp_bpi_le = ss_comp_bytes_per_interval.to_le_bytes();
                    let ss_comp: [u8; 6] = [
                        6,
                        0x30, // USB_DT_SS_ENDPOINT_COMP
                        0,
                        0,
                        ss_comp_bpi_le[0],
                        ss_comp_bpi_le[1],
                    ];

                    fs_descs.extend_from_slice(&fs_ep);
                    hs_descs.extend_from_slice(&hs_ep);
                    ss_descs.extend_from_slice(&ss_ep);
                    ss_descs.extend_from_slice(&ss_comp);
                    fs_count += 1;
                    hs_count += 1;
                    ss_count += 2;
                }
            }
        }

        let flags = FUNCTIONFS_HAS_FS_DESC
            | FUNCTIONFS_HAS_HS_DESC
            | FUNCTIONFS_HAS_SS_DESC
            | FUNCTIONFS_ALL_CTRL_RECIP
            | FUNCTIONFS_CONFIG0_SETUP;
        let total_len = (24 + fs_descs.len() + hs_descs.len() + ss_descs.len()) as u32;

        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
        out.extend_from_slice(&total_len.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&fs_count.to_le_bytes());
        out.extend_from_slice(&hs_count.to_le_bytes());
        out.extend_from_slice(&ss_count.to_le_bytes());
        out.extend_from_slice(&fs_descs);
        out.extend_from_slice(&hs_descs);
        out.extend_from_slice(&ss_descs);

        (out, unique_eps)
    }

    /// Serializes a Linux FunctionFS `usb_functionfs_descs_head_v2` (`magic = 3`) payload
    /// containing Full-Speed, High-Speed, and SuperSpeed descriptors for config 0.
    pub fn build_ffs_descriptors_v2(&self) -> Vec<u8> {
        self.build_ffs_descriptors_v2_and_eps().0
    }

    /// Serializes a FunctionFS `usb_functionfs_strings_head` (`magic = 2`) with 1 English string.
    pub fn build_ffs_strings_0x0409(interface_label: &str) -> Vec<u8> {
        const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
        let str_bytes = interface_label.as_bytes();
        let total_len = (16 + 2 + str_bytes.len() + 1) as u32;
        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
        out.extend_from_slice(&total_len.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // str_count = 1
        out.extend_from_slice(&1u32.to_le_bytes()); // lang_count = 1
        out.extend_from_slice(&0x0409u16.to_le_bytes());
        out.extend_from_slice(str_bytes);
        out.push(0);
        out
    }
}

/// Declarative input scenario (`scenarios/*.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub device: DeviceSpec,
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// A single declarative host or gadget action in a [`Scenario`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(rename = "type")]
    pub op: String,
    #[serde(flatten)]
    pub params: Value,
}

/// Alias for [`Step`].
pub type TraceStep = Step;

impl Step {
    pub fn new(op: impl Into<String>, params: Value) -> Self {
        Self { op: op.into(), params }
    }

    pub fn str_param(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(Value::as_str)
    }

    pub fn u64_param(&self, key: &str) -> Option<u64> {
        self.params.get(key).and_then(Value::as_u64)
    }

    pub fn i64_param(&self, key: &str) -> Option<i64> {
        self.params.get(key).and_then(Value::as_i64)
    }

    pub fn bool_param(&self, key: &str) -> Option<bool> {
        self.params.get(key).and_then(Value::as_bool)
    }

    pub fn bytes_param(&self, key: &str) -> Option<Vec<u8>> {
        let arr = self.params.get(key)?.as_array()?;
        let mut bytes = Vec::with_capacity(arr.len());
        for item in arr {
            bytes.push(item.as_u64()? as u8);
        }
        Some(bytes)
    }

    pub fn uses_arch32(&self) -> bool {
        if self.bool_param("arch32") == Some(true) {
            return true;
        }
        if let Some(inner_val) = self.params.get("inner")
            && let Ok(inner_step) = serde_json::from_value::<Step>(inner_val.clone())
        {
            return inner_step.uses_arch32();
        }
        false
    }
}

/// Maps a numeric POSIX `errno` to its canonical symbolic name.
pub fn errno_to_name(err: i32) -> String {
    match err {
        1 => "EPERM",
        2 => "ENOENT",
        3 => "ESRCH",
        4 => "EINTR",
        5 => "EIO",
        6 => "ENXIO",
        7 => "E2BIG",
        8 => "ENOEXEC",
        9 => "EBADF",
        10 => "ECHILD",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        16 => "EBUSY",
        17 => "EEXIST",
        18 => "EXDEV",
        19 => "ENODEV",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        23 => "ENFILE",
        24 => "EMFILE",
        25 => "ENOTTY",
        26 => "ETXTBSY",
        27 => "EFBIG",
        28 => "ENOSPC",
        29 => "ESPIPE",
        30 => "EROFS",
        31 => "EMLINK",
        32 => "EPIPE",
        33 => "EDOM",
        34 => "ERANGE",
        38 => "ENOSYS",
        61 => "ENODATA",
        62 => "ETIME",
        71 => "EPROTO",
        75 => "EOVERFLOW",
        84 => "EILSEQ",
        104 => "ECONNRESET",
        105 => "ENOBUFS",
        108 => "ESHUTDOWN",
        110 => "ETIMEDOUT",
        111 => "ECONNREFUSED",
        112 => "EHOSTDOWN",
        113 => "EHOSTUNREACH",
        114 => "EALREADY",
        115 => "EINPROGRESS",
        121 => "EREMOTEIO",
        125 => "ECANCELED",
        other => return format!("ERRNO_{other}"),
    }
    .to_string()
}

/// Formats a `usbdevfs_urb.status` value (0 or `-errno`) as a canonical string.
pub fn urb_status_to_string(status: i32) -> String {
    if status == 0 {
        "0".to_string()
    } else if status < 0 {
        format!("-{}", errno_to_name(-status))
    } else {
        status.to_string()
    }
}

/// Parses `"ifno"` and `"sub_ioctl"` / `"ioctl_code"` for a `"usbdevfs_ioctl"` step.
///
/// Panics if `"ifno"` or `"ioctl_code"` is non-integer or out of 32-bit range, if `"sub_ioctl"`
/// is unrecognized, if both `"sub_ioctl"` and `"ioctl_code"` are set, or if neither is set, so
/// scenario typos fail immediately instead of silently defaulting.
pub fn parse_usbdevfs_ioctl_params(
    params: &Value,
    disconnect_code: i32,
    connect_code: i32,
) -> (i32, i32) {
    let ifno = match params.get("ifno") {
        Some(val) => val
            .as_i64()
            .map(|v| i32::try_from(v).expect("usbdevfs_ioctl ifno out of i32 range"))
            .expect("usbdevfs_ioctl ifno must be an integer"),
        None => 0,
    };
    let ioctl_code = match (params.get("sub_ioctl"), params.get("ioctl_code")) {
        (Some(sub), None) => match sub.as_str() {
            Some("disconnect") => disconnect_code,
            Some("connect") => connect_code,
            other => panic!("unknown usbdevfs_ioctl sub_ioctl: {other:?}"),
        },
        (None, Some(code)) => code
            .as_i64()
            .map(|v| {
                if v >= 0 {
                    u32::try_from(v).expect("usbdevfs_ioctl ioctl_code out of u32 range") as i32
                } else {
                    i32::try_from(v).expect("usbdevfs_ioctl ioctl_code out of i32 range")
                }
            })
            .expect("usbdevfs_ioctl ioctl_code must be an integer"),
        (Some(_), Some(_)) => {
            panic!("usbdevfs_ioctl step cannot specify both sub_ioctl and ioctl_code")
        }
        (None, None) => {
            panic!("usbdevfs_ioctl step must specify either sub_ioctl or ioctl_code")
        }
    };
    (ifno, ioctl_code)
}

/// Recorded golden trace (`generated/*.json`) produced by executing a [`Scenario`] on Linux.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedTrace {
    pub name: String,
    pub device: DeviceSpec,
    #[serde(default)]
    pub steps: Vec<RecordedStep>,
}

/// Alias for [`RecordedTrace`].
pub type GoldenTrace = RecordedTrace;

/// A single step paired with its recorded Linux outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedStep {
    pub step: Step,
    pub outcome: StepOutcome,
}

/// Recorded outcome of executing a [`Step`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
}

/// Alias for [`StepOutcome`].
pub type SyscallOutcome = StepOutcome;

impl StepOutcome {
    pub fn ok(result: Value) -> Self {
        Self { status: "ok".to_string(), errno: None, result }
    }

    pub fn error(errno: impl Into<String>) -> Self {
        Self { status: "error".to_string(), errno: Some(errno.into()), result: Value::Null }
    }
}

/// Gadget-side RPC command sent over [`DEFAULT_CONTROL_ADDR`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DeviceCommand {
    ConfigureDevice {
        device: DeviceSpec,
    },
    ScriptControl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bm_request_type: Option<u8>,
        b_request: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w_value: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w_index: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w_length: Option<u16>,
        action: Value,
    },
    ReleaseControl {
        hold: String,
        action: Value,
    },
    PushIn {
        ep: u8,
        data: Vec<u8>,
    },
    PopOut {
        ep: u8,
        timeout_ms: u64,
    },
    AssertEmpty {
        ep: u8,
    },
    Disconnect {
        #[serde(default)]
        wait_for_held_transfer: bool,
    },
    Reconnect,
}

/// Gadget-side RPC response returned over [`DEFAULT_CONTROL_ADDR`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<u8>>,
}

impl DeviceResponse {
    pub fn success() -> Self {
        Self { ok: true, error: None, data: None }
    }

    pub fn success_with_data(data: Vec<u8>) -> Self {
        Self { ok: true, error: None, data: Some(data) }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), data: None }
    }
}

/// Scoped rule allowing a documented Starnix-vs-Linux difference (`divergences.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceRule {
    pub scenario_glob: String,
    pub step_op: String,
    pub field: String,
    pub linux_expected: Value,
    pub starnix_expected: Value,
    pub bug_url: String,
    pub reason: String,
}

/// Collection of documented Starnix-vs-Linux divergence rules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceManifest {
    #[serde(default)]
    pub divergences: Vec<DivergenceRule>,
}

/// Simple wildcard glob matcher supporting `*` and `?`.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn get_field_path<'a>(root: &'a Value, field: &str) -> &'a Value {
    if field.is_empty() || field == "*" {
        return root;
    }
    let mut curr = root;
    for part in field.split('.') {
        match curr {
            Value::Object(map) => {
                if let Some(next) = map.get(part) {
                    curr = next;
                } else {
                    return &Value::Null;
                }
            }
            _ => return &Value::Null,
        }
    }
    curr
}

fn set_field_path(root: &mut Value, field: &str, new_val: Value) {
    if field.is_empty() || field == "*" {
        *root = new_val;
        return;
    }
    let parts: Vec<&str> = field.split('.').collect();
    let mut curr = root;
    for (idx, part) in parts.iter().enumerate() {
        if idx + 1 == parts.len() {
            if let Value::Object(map) = curr {
                if new_val.is_null() && (*part == "errno" || *part == "result") {
                    map.remove(*part);
                } else {
                    map.insert((*part).to_string(), new_val);
                }
            }
            return;
        }
        if !curr.is_object() {
            *curr = Value::Object(Map::new());
        }
        let map = curr.as_object_mut().unwrap();
        curr = map.entry((*part).to_string()).or_insert_with(|| Value::Object(Map::new()));
    }
}

fn normalize_outcome_json(val: &mut Value) {
    if let Value::Object(map) = val {
        if map.get("status").is_some_and(Value::is_null) {
            map.remove("status");
        }
        if map.get("errno").is_some_and(Value::is_null) {
            map.remove("errno");
        }
        if map.get("result").is_some_and(Value::is_null) {
            map.remove("result");
        }
    }
}

impl DivergenceManifest {
    /// Returns all rules matching `(scenario_name, step_op)`.
    pub fn find_matching_rules<'a>(
        &'a self,
        scenario_name: &str,
        step_op: &str,
    ) -> Vec<&'a DivergenceRule> {
        self.divergences
            .iter()
            .filter(|r| {
                glob_matches(&r.scenario_glob, scenario_name)
                    && (r.step_op == "*" || glob_matches(&r.step_op, step_op))
            })
            .collect()
    }

    /// Compares `actual` outcome against `expected` (Linux golden outcome).
    /// When `apply_divergences == false` (running on real Linux), requires an exact match.
    /// When `apply_divergences == true` (running on Starnix), allows differences explicitly
    /// listed in `self.divergences` and rejects stale rules where Starnix now matches Linux.
    pub fn matches_or_allows(
        &self,
        scenario_name: &str,
        step_op: &str,
        expected: &StepOutcome,
        actual: &StepOutcome,
        apply_divergences: bool,
    ) -> Result<(), String> {
        if !apply_divergences {
            if expected == actual {
                return Ok(());
            }
            return Err(format!(
                "Scenario '{scenario_name}' step '{step_op}' outcome mismatch (divergences disabled):\n  expected: {}\n  actual:   {}",
                serde_json::to_string(expected).unwrap_or_default(),
                serde_json::to_string(actual).unwrap_or_default()
            ));
        }

        let mut adjusted =
            serde_json::to_value(expected).unwrap_or_else(|_| Value::Object(Map::new()));
        let mut actual_val =
            serde_json::to_value(actual).unwrap_or_else(|_| Value::Object(Map::new()));
        normalize_outcome_json(&mut adjusted);
        normalize_outcome_json(&mut actual_val);

        let mut applied_rules = Vec::new();
        for rule in self.find_matching_rules(scenario_name, step_op) {
            let current_val = get_field_path(&adjusted, &rule.field);
            let matches_linux = if rule.linux_expected == Value::String("*".to_string()) {
                !current_val.is_null()
            } else {
                current_val == &rule.linux_expected
            };
            if matches_linux {
                set_field_path(&mut adjusted, &rule.field, rule.starnix_expected.clone());
                if rule.field == "status"
                    && rule.starnix_expected == Value::String("error".to_string())
                {
                    set_field_path(&mut adjusted, "result", Value::Null);
                }
                normalize_outcome_json(&mut adjusted);
                applied_rules.push((rule.field.as_str(), rule.bug_url.as_str()));
            }
        }

        if adjusted == actual_val {
            Ok(())
        } else if expected == actual && !applied_rules.is_empty() {
            Err(format!(
                "Scenario '{scenario_name}' step '{step_op}' matched Linux outcome directly, making divergence rule(s) {applied_rules:?} stale:\n  outcome:          {}\n  starnix_adjusted: {}",
                serde_json::to_string(actual).unwrap_or_default(),
                serde_json::to_string(&adjusted).unwrap_or_default()
            ))
        } else {
            Err(format!(
                "Scenario '{scenario_name}' step '{step_op}' outcome mismatch after applying divergences {applied_rules:?}:\n  linux_expected:   {}\n  starnix_adjusted: {}\n  starnix_actual:   {}",
                serde_json::to_string(expected).unwrap_or_default(),
                serde_json::to_string(&adjusted).unwrap_or_default(),
                serde_json::to_string(actual).unwrap_or_default()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_default_device_spec_descriptors_match_usb20_layout() {
        let spec = DeviceSpec::default();
        assert!(spec.validate_for_ffs().is_ok());
        let dev_desc = spec.build_device_descriptor_bytes();
        assert_eq!(dev_desc.len(), 18);
        assert_eq!(dev_desc[0], 18);
        assert_eq!(dev_desc[1], 1); // USB_DT_DEVICE
        assert_eq!(&dev_desc[8..10], &0x1234u16.to_le_bytes());
        assert_eq!(&dev_desc[10..12], &0x5678u16.to_le_bytes());
        assert_eq!(dev_desc[17], 1); // bNumConfigurations

        let cfg_desc = spec.build_config_descriptor_bytes();
        assert_eq!(cfg_desc.len(), 32);
        assert_eq!(&cfg_desc[0..6], &[9, 2, 32, 0, 1, 1]);

        let raw = spec.build_raw_descriptors();
        assert_eq!(raw.len(), 50);
        assert_eq!(raw[38], 0x81);
        assert_eq!(raw[45], 0x02);

        let mut high_power_spec = DeviceSpec::default();
        high_power_spec.configs[0].max_power_ma = 600;
        let high_power_cfg = high_power_spec.build_config_descriptor_bytes();
        assert_eq!(high_power_cfg[8], 255);
    }

    #[test]
    fn test_ffs_descriptors_v2_layout() {
        let spec = DeviceSpec::default();
        let (ffs, eps) = spec.build_ffs_descriptors_v2_and_eps();
        assert_eq!(eps.len(), 2);
        assert_eq!(ffs.len(), 105);
        assert_eq!(&ffs[0..4], &3u32.to_le_bytes());
        assert_eq!(&ffs[12..16], &3u32.to_le_bytes());
        assert_eq!(&ffs[16..20], &3u32.to_le_bytes());
        assert_eq!(&ffs[20..24], &5u32.to_le_bytes());
    }

    #[test]
    fn test_divergence_manifest_matcher() {
        let manifest = DivergenceManifest {
            divergences: vec![
                DivergenceRule {
                    scenario_glob: "discovery_*".to_string(),
                    step_op: "get_capabilities".to_string(),
                    field: "result.capabilities".to_string(),
                    linux_expected: json!(3),
                    starnix_expected: json!(0),
                    bug_url: "https://fxbug.dev/579597237".to_string(),
                    reason: "Starnix returns 0 for USBDEVFS_GET_CAPABILITIES".to_string(),
                },
                DivergenceRule {
                    scenario_glob: "*".to_string(),
                    step_op: "claim_interface".to_string(),
                    field: "errno".to_string(),
                    linux_expected: json!("ENOENT"),
                    starnix_expected: json!("EINVAL"),
                    bug_url: "https://fxbug.dev/579597237".to_string(),
                    reason: "Starnix returns EINVAL for nonexistent interface claims".to_string(),
                },
            ],
        };

        let linux_caps = StepOutcome::ok(json!({ "capabilities": 3 }));
        let starnix_caps = StepOutcome::ok(json!({ "capabilities": 0 }));

        assert!(
            manifest
                .matches_or_allows(
                    "discovery_and_descriptors",
                    "get_capabilities",
                    &linux_caps,
                    &starnix_caps,
                    false
                )
                .is_err()
        );

        assert!(
            manifest
                .matches_or_allows(
                    "discovery_and_descriptors",
                    "get_capabilities",
                    &linux_caps,
                    &starnix_caps,
                    true
                )
                .is_ok()
        );

        // Stale divergence rule: Starnix now matches Linux directly while a rule still exists.
        assert!(
            manifest
                .matches_or_allows(
                    "discovery_and_descriptors",
                    "get_capabilities",
                    &linux_caps,
                    &linux_caps,
                    true
                )
                .is_err()
        );

        let linux_err = StepOutcome::error("ENOENT");
        let starnix_err = StepOutcome::error("EINVAL");
        assert!(
            manifest
                .matches_or_allows(
                    "multi_fd_claims",
                    "claim_interface",
                    &linux_err,
                    &starnix_err,
                    true
                )
                .is_ok()
        );
        assert!(
            manifest
                .matches_or_allows(
                    "multi_fd_claims",
                    "claim_interface",
                    &linux_err,
                    &StepOutcome::error("EBUSY"),
                    true
                )
                .is_err()
        );
    }

    #[test]
    fn test_all_divergence_rules_apply_to_recorded_scenarios() {
        let manifest: DivergenceManifest =
            serde_json::from_str(DIVERGENCES_JSON).expect("valid DIVERGENCES_JSON");
        let traces: Vec<RecordedTrace> = SCENARIOS
            .iter()
            .map(|(_, raw)| serde_json::from_str(raw).expect("valid RecordedTrace JSON"))
            .collect();

        for rule in &manifest.divergences {
            let mut matched_any = false;
            for trace in &traces {
                if !glob_matches(&rule.scenario_glob, &trace.name) {
                    continue;
                }
                for recorded in &trace.steps {
                    if rule.step_op != "*" && !glob_matches(&rule.step_op, &recorded.step.op) {
                        continue;
                    }
                    let mut val = serde_json::to_value(&recorded.outcome).unwrap();
                    normalize_outcome_json(&mut val);
                    let field_val = get_field_path(&val, &rule.field);
                    let matches_linux = if rule.linux_expected == Value::String("*".to_string()) {
                        !field_val.is_null()
                    } else {
                        field_val == &rule.linux_expected
                    };
                    if matches_linux {
                        matched_any = true;
                        break;
                    }
                }
                if matched_any {
                    break;
                }
            }
            assert!(
                matched_any,
                "Divergence rule for scenario_glob='{}' step_op='{}' field='{}' ({}) did not match any recorded step in SCENARIOS",
                rule.scenario_glob, rule.step_op, rule.field, rule.bug_url
            );
        }
    }

    fn collect_step_ops(step: &Step, seen: &mut std::collections::BTreeSet<String>) {
        seen.insert(step.op.clone());
        if let Some(inner_val) = step.params.get("inner")
            && let Ok(inner_step) = serde_json::from_value::<Step>(inner_val.clone())
        {
            collect_step_ops(&inner_step, seen);
        }
    }

    #[test]
    fn test_all_step_ops_covered_by_scenarios() {
        let expected_ops = [
            "open",
            "close",
            "stat_dev",
            "read",
            "pread",
            "lseek",
            "write",
            "poll",
            "epoll_check",
            "read_sysfs",
            "get_speed",
            "get_capabilities",
            "connectinfo",
            "get_driver",
            "claim_interface",
            "release_interface",
            "disconnect_claim",
            "set_interface",
            "set_configuration",
            "clear_halt",
            "reset",
            "raw_ioctl",
            "usbdevfs_ioctl",
            "control",
            "bulk",
            "submit_urb",
            "discard_urb",
            "reap_urb_ndelay",
            "reap_urb",
            "inspect_urb",
            "start_op",
            "await_task",
            "device_script_control",
            "device_release_control",
            "device_ep_push_in",
            "device_ep_pop_out",
            "device_ep_assert_empty",
            "device_disconnect",
            "device_reconnect",
        ];

        let mut seen = std::collections::BTreeSet::new();
        for (_, raw) in SCENARIOS {
            let trace: RecordedTrace = serde_json::from_str(raw).expect("valid RecordedTrace JSON");
            assert!(trace.device.validate_for_ffs().is_ok());
            assert!(!trace.steps.is_empty(), "trace '{}' has no steps", trace.name);
            for recorded in &trace.steps {
                collect_step_ops(&recorded.step, &mut seen);
            }
        }

        for op in expected_ops {
            assert!(
                seen.contains(op),
                "Step op '{op}' is not covered by any recorded scenario in SCENARIOS"
            );
        }
    }

    #[test]
    fn test_device_command_wire_serialization() {
        let cmd = DeviceCommand::PushIn { ep: 0x81, data: vec![1, 2, 3] };
        let val = serde_json::to_value(&cmd).unwrap();
        assert_eq!(val["cmd"], "push_in");
        assert_eq!(val["ep"], 0x81);
        let roundtrip: DeviceCommand = serde_json::from_value(val).unwrap();
        assert_eq!(roundtrip, cmd);

        let script = DeviceCommand::ScriptControl {
            bm_request_type: Some(0xa1),
            b_request: 0x01,
            w_value: Some(0x0100),
            w_index: Some(0),
            w_length: Some(64),
            action: json!({ "reply": [1, 2] }),
        };
        let script_val = serde_json::to_value(&script).unwrap();
        assert_eq!(script_val["cmd"], "script_control");
        assert_eq!(script_val["w_length"], 64);
        let script_roundtrip: DeviceCommand = serde_json::from_value(script_val).unwrap();
        assert_eq!(script_roundtrip, script);

        let disc = DeviceCommand::Disconnect { wait_for_held_transfer: true };
        let disc_val = serde_json::to_value(&disc).unwrap();
        assert_eq!(disc_val["cmd"], "disconnect");
        assert_eq!(disc_val["wait_for_held_transfer"], true);
        let disc_roundtrip: DeviceCommand = serde_json::from_value(disc_val).unwrap();
        assert_eq!(disc_roundtrip, disc);

        let disc_default: DeviceCommand =
            serde_json::from_value(json!({ "cmd": "disconnect" })).unwrap();
        assert_eq!(disc_default, DeviceCommand::Disconnect { wait_for_held_transfer: false });
    }

    #[test]
    fn test_parse_usbdevfs_ioctl_params() {
        assert_eq!(
            parse_usbdevfs_ioctl_params(&json!({ "ifno": -1, "sub_ioctl": "disconnect" }), 22, 23),
            (-1, 22)
        );
        assert_eq!(
            parse_usbdevfs_ioctl_params(&json!({ "sub_ioctl": "connect" }), 22, 23),
            (0, 23)
        );
        assert_eq!(
            parse_usbdevfs_ioctl_params(&json!({ "ifno": 0, "ioctl_code": 0x1234 }), 22, 23),
            (0, 0x1234)
        );
        assert_eq!(
            parse_usbdevfs_ioctl_params(
                &json!({ "ifno": 0, "ioctl_code": 0x8004_55ffu32 }),
                22,
                23
            ),
            (0, 0x8004_55ffu32 as i32)
        );
    }

    #[test]
    #[should_panic(expected = "unknown usbdevfs_ioctl sub_ioctl")]
    fn test_parse_usbdevfs_ioctl_params_rejects_unknown_sub_ioctl() {
        let _ = parse_usbdevfs_ioctl_params(&json!({ "sub_ioctl": "typo" }), 22, 23);
    }

    #[test]
    #[should_panic(expected = "usbdevfs_ioctl ifno must be an integer")]
    fn test_parse_usbdevfs_ioctl_params_rejects_non_integer_ifno() {
        let _ =
            parse_usbdevfs_ioctl_params(&json!({ "ifno": "0", "sub_ioctl": "disconnect" }), 22, 23);
    }
}
