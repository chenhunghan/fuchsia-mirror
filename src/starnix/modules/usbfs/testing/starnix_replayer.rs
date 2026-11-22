// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-process Starnix `usbfs` golden-trace replayer for `starnix-usbfs-tests`.

use crate::device::{
    USB_SPEED_FULL, USB_SPEED_HIGH, USB_SPEED_LOW, USB_SPEED_SUPER, USB_SPEED_SUPER_PLUS,
    USB_SPEED_UNKNOWN, UsbDevice, UsbdevfsUrb32Header, UsbdevfsUrbHeader,
};
use crate::{UsbDeviceManager, usbfs_init};
use fidl::endpoints::{ServerEnd, create_endpoints};
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use futures::TryStreamExt;
use serde_json::{Value, json};
use starnix_core::device::kobject::Device;
use starnix_core::fs::devtmpfs::DevTmpFs;
use starnix_core::fs::sysfs::get_sysfs;
use starnix_core::mm::syscalls::do_mmap;
use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt, PAGE_SIZE};
use starnix_core::task::{CurrentTask, Kernel, KernelFeatures};
use starnix_core::testing::spawn_kernel_with_features_and_run;
use starnix_core::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
use starnix_core::vfs::{FdNumber, FileHandle, LookupContext, NamespaceNode, SeekTarget};
#[allow(clippy::disallowed_types)]
use starnix_sync::Mutex;
use starnix_syscalls::SyscallResult;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
#[cfg(target_arch = "aarch64")]
use starnix_uapi::uaddr32;
#[cfg(target_arch = "aarch64")]
use starnix_uapi::uapi::arch32;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, USB_DIR_IN, USBDEVFS_BULK,
    USBDEVFS_CLAIMINTERFACE, USBDEVFS_CLEAR_HALT, USBDEVFS_CONNECT, USBDEVFS_CONNECTINFO,
    USBDEVFS_CONTROL, USBDEVFS_DISCARDURB, USBDEVFS_DISCONNECT, USBDEVFS_DISCONNECT_CLAIM,
    USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER, USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
    USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_SPEED, USBDEVFS_GETDRIVER, USBDEVFS_IOCTL,
    USBDEVFS_REAPURB, USBDEVFS_REAPURBNDELAY, USBDEVFS_RELEASEINTERFACE, USBDEVFS_RESET,
    USBDEVFS_SETCONFIGURATION, USBDEVFS_SETINTERFACE, USBDEVFS_SUBMITURB,
    USBDEVFS_URB_BULK_CONTINUATION, USBDEVFS_URB_ISO_ASAP, USBDEVFS_URB_NO_FSBR,
    USBDEVFS_URB_NO_INTERRUPT, USBDEVFS_URB_SHORT_NOT_OK, USBDEVFS_URB_TYPE_BULK,
    USBDEVFS_URB_TYPE_CONTROL, USBDEVFS_URB_TYPE_INTERRUPT, USBDEVFS_URB_TYPE_ISO,
    USBDEVFS_URB_ZERO_PACKET, uaddr, usb_ctrlrequest, usb_device_descriptor, usbdevfs_bulktransfer,
    usbdevfs_connectinfo, usbdevfs_ctrltransfer, usbdevfs_disconnect_claim, usbdevfs_getdriver,
    usbdevfs_ioctl, usbdevfs_setinterface,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use usbfs_trace_schema::{
    DIVERGENCES_JSON, DeviceSpec, DivergenceManifest, RecordedStep, RecordedTrace, Step,
    StepOutcome, errno_to_name, parse_usbdevfs_ioctl_params, urb_status_to_string,
};
use zerocopy::{FromBytes, IntoBytes};

const DEV_NUM: u16 = 2;
const SETUP_PACKET_SIZE: usize = 8;
const CANARY_PAD: usize = 64;
const CANARY_BYTE: u8 = 0xa5;
const IN_INIT_BYTE: u8 = 0x5a;

#[derive(Clone, Debug)]
enum ControlScriptAction {
    Reply(Vec<u8>),
    Stall,
    Hold(String),
}

impl ControlScriptAction {
    fn from_value(v: &Value) -> Option<Self> {
        if v.get("stall").and_then(Value::as_bool) == Some(true) {
            return Some(Self::Stall);
        }
        if let Some(hold_id) = v.get("hold").and_then(Value::as_str) {
            return Some(Self::Hold(hold_id.to_string()));
        }
        if let Some(arr) = v.get("reply").and_then(Value::as_array) {
            let bytes: Vec<u8> = arr.iter().filter_map(|x| x.as_u64().map(|b| b as u8)).collect();
            return Some(Self::Reply(bytes));
        }
        None
    }
}

#[derive(Clone, Debug)]
struct ScriptedControlRule {
    bm_request_type: Option<u8>,
    b_request: u8,
    w_value: Option<u16>,
    w_index: Option<u16>,
    w_length: Option<u16>,
    action: ControlScriptAction,
}

impl ScriptedControlRule {
    fn matches(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
    ) -> bool {
        if self.b_request != b_request {
            return false;
        }
        match self.bm_request_type {
            Some(expected_bm) if expected_bm != bm_request_type => return false,
            None if (bm_request_type & 0x60) == 0 => return false,
            _ => {}
        }
        if self.w_value.is_some_and(|v| v != w_value) {
            return false;
        }
        if self.w_index.is_some_and(|v| v != w_index) {
            return false;
        }
        if self.w_length.is_some_and(|v| v != w_length) {
            return false;
        }
        true
    }
}

struct HeldControlTransfer {
    hold_id: String,
    setup: fusb_descriptor::UsbSetup,
    write_data: Vec<u8>,
    responder: fhardware_usb::UsbControlTransferResponder,
}

#[derive(Default)]
struct EndpointQueueState {
    in_packets: VecDeque<Vec<u8>>,
    out_packets: VecDeque<Vec<u8>>,
    held_in: VecDeque<(fusb_request::Request, fusb_endpoint::EndpointControlHandle)>,
    vmos: HashMap<u64, zx::Vmo>,
}

// `starnix_sync::Mutex` is banned in production Starnix code in favor of ordered locks,
// but is used here solely for in-process mock FIDL server state in unit tests.
#[allow(clippy::disallowed_types)]
struct ReplayerUsbServerState {
    device_descriptor_bytes: Vec<u8>,
    config_descriptors: Vec<Vec<u8>>,
    auto_echo: AtomicBool,
    first_in_ep: u8,
    scripted_controls: Mutex<Vec<ScriptedControlRule>>,
    held_controls: Mutex<VecDeque<HeldControlTransfer>>,
    released_controls: Mutex<HashMap<String, ControlScriptAction>>,
    last_ctrl_out: Mutex<Vec<u8>>,
    endpoints: Mutex<HashMap<u8, EndpointQueueState>>,
}

impl ReplayerUsbServerState {
    #[allow(clippy::disallowed_types)]
    fn new(spec: &DeviceSpec) -> Arc<Self> {
        spec.validate_for_ffs().expect("invalid DeviceSpec in starnix_replayer");
        let mut endpoints = HashMap::new();
        let mut in_eps = Vec::new();
        for cfg in &spec.configs {
            for iface in &cfg.interfaces {
                for ep in &iface.endpoints {
                    endpoints.entry(ep.address).or_insert_with(EndpointQueueState::default);
                    if (ep.address & (USB_DIR_IN as u8)) != 0 && !in_eps.contains(&ep.address) {
                        in_eps.push(ep.address);
                    }
                }
            }
        }
        let first_in_ep =
            if in_eps.contains(&0x81) { 0x81 } else { in_eps.first().copied().unwrap_or(0x81) };
        Arc::new(Self {
            device_descriptor_bytes: spec.build_device_descriptor_bytes().to_vec(),
            config_descriptors: spec
                .configs
                .iter()
                .filter_map(|c| spec.build_single_config_descriptor_bytes(c.config_value))
                .collect(),
            auto_echo: AtomicBool::new(false),
            first_in_ep,
            scripted_controls: Mutex::new(Vec::new()),
            held_controls: Mutex::new(VecDeque::new()),
            released_controls: Mutex::new(HashMap::new()),
            last_ctrl_out: Mutex::new(Vec::new()),
            endpoints: Mutex::new(endpoints),
        })
    }

    fn script_control(
        &self,
        bm_request_type: Option<u8>,
        b_request: u8,
        w_value: Option<u16>,
        w_index: Option<u16>,
        w_length: Option<u16>,
        action: &Value,
    ) {
        if let Some(auto_echo) = action.get("auto_echo").and_then(Value::as_bool) {
            self.auto_echo.store(auto_echo, Ordering::SeqCst);
            if auto_echo {
                self.scripted_controls.lock().clear();
                self.released_controls.lock().clear();
                self.last_ctrl_out.lock().clear();
                for ep in self.endpoints.lock().values_mut() {
                    ep.in_packets.clear();
                    ep.out_packets.clear();
                }
            }
            return;
        }
        let act = ControlScriptAction::from_value(action)
            .unwrap_or_else(|| panic!("invalid ScriptControl action: {action}"));
        self.scripted_controls.lock().push(ScriptedControlRule {
            bm_request_type,
            b_request,
            w_value,
            w_index,
            w_length,
            action: act,
        });
    }

    fn respond_to_control(
        &self,
        setup: &fusb_descriptor::UsbSetup,
        write_data: Vec<u8>,
        responder: fhardware_usb::UsbControlTransferResponder,
        action: ControlScriptAction,
    ) {
        let is_in = (setup.bm_request_type & (USB_DIR_IN as u8)) != 0;
        let is_vendor = (setup.bm_request_type & 0x60) == 0x40;
        let w_len = usize::from(setup.w_length);
        match action {
            ControlScriptAction::Stall => {
                let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
            }
            ControlScriptAction::Reply(mut reply_bytes) => {
                if is_in {
                    if reply_bytes.len() > w_len {
                        reply_bytes.truncate(w_len);
                    }
                    let _ = responder.send(Ok(&reply_bytes));
                } else {
                    if is_vendor && setup.b_request == 0x01 {
                        *self.last_ctrl_out.lock() = write_data;
                    }
                    let _ = responder.send(Ok(&[]));
                }
            }
            ControlScriptAction::Hold(_) => {
                if is_in {
                    let default_reply = vec![0x42u8; w_len];
                    let _ = responder.send(Ok(&default_reply));
                } else {
                    let _ = responder.send(Ok(&[]));
                }
            }
        }
    }

    fn release_control(&self, hold_id: &str, action: &Value) {
        let resolved = ControlScriptAction::from_value(action)
            .unwrap_or_else(|| panic!("invalid ReleaseControl action: {action}"));
        let held_opt = {
            let mut held_list = self.held_controls.lock();
            held_list
                .iter()
                .position(|h| h.hold_id == hold_id)
                .and_then(|pos| held_list.remove(pos))
        };
        if let Some(held) = held_opt {
            self.respond_to_control(&held.setup, held.write_data, held.responder, resolved);
        } else {
            self.released_controls.lock().insert(hold_id.to_string(), resolved);
        }
    }

    fn push_in(&self, endpoint: u8, mut data: Vec<u8>) {
        assert!(
            (endpoint & (USB_DIR_IN as u8)) != 0,
            "push_in requires IN endpoint, got {endpoint:#x}"
        );
        let mut eps = self.endpoints.lock();
        let ep_state =
            eps.get_mut(&endpoint).unwrap_or_else(|| panic!("unknown IN endpoint {endpoint:#x}"));
        let mut delivered = false;
        while let Some((request, control_handle)) = ep_state.held_in.pop_front() {
            let (id, offset, size) = request_region(&request);
            let written = std::cmp::min(data.len(), size as usize);
            if written > 0 {
                if let Some(vmo) = ep_state.vmos.get(&id) {
                    vmo.write(&data[..written], offset).expect("write VMO");
                }
            }
            let completion = fusb_endpoint::Completion {
                request: Some(request),
                status: Some(zx::sys::ZX_OK),
                transfer_size: Some(written as u64),
                ..Default::default()
            };
            if control_handle.send_on_completion(vec![completion]).is_ok() {
                data.drain(..written);
                if data.is_empty() {
                    delivered = true;
                    break;
                }
            }
        }
        if !delivered {
            ep_state.in_packets.push_back(data);
        }
    }

    fn pop_out(&self, endpoint: u8, timeout_ms: u64) -> Option<Vec<u8>> {
        assert!(
            (endpoint & (USB_DIR_IN as u8)) == 0,
            "pop_out requires OUT endpoint, got {endpoint:#x}"
        );
        assert!(
            self.endpoints.lock().contains_key(&endpoint),
            "unknown OUT endpoint {endpoint:#x}"
        );
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(pkt) =
                self.endpoints.lock().get_mut(&endpoint).unwrap().out_packets.pop_front()
            {
                return Some(pkt);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn is_ep_empty(&self, endpoint: u8) -> bool {
        assert!(
            (endpoint & (USB_DIR_IN as u8)) == 0,
            "is_ep_empty requires OUT endpoint, got {endpoint:#x}"
        );
        let eps = self.endpoints.lock();
        let ep_state =
            eps.get(&endpoint).unwrap_or_else(|| panic!("unknown OUT endpoint {endpoint:#x}"));
        ep_state.out_packets.is_empty()
    }

    fn has_held_transfer(&self) -> bool {
        if !self.held_controls.lock().is_empty() {
            return true;
        }
        self.endpoints.lock().values().any(|ep| !ep.held_in.is_empty())
    }
}

fn request_region(request: &fusb_request::Request) -> (u64, u64, u64) {
    let region = request.data.as_ref().and_then(|d| d.first()).expect("request region");
    let id = match region.buffer {
        Some(fusb_request::Buffer::VmoId(id)) => id,
        _ => panic!("unexpected buffer"),
    };
    (id, region.offset.unwrap_or(0), region.size.unwrap_or(0))
}

async fn serve_replayer_endpoint(
    ep_addr: u8,
    ep: ServerEnd<fusb_endpoint::EndpointMarker>,
    state: Arc<ReplayerUsbServerState>,
) {
    let mut ep_stream = ep.into_stream();
    let is_in = (ep_addr & (USB_DIR_IN as u8)) != 0;
    while let Ok(Some(ep_req)) = ep_stream.try_next().await {
        match ep_req {
            fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                let mut registered = Vec::new();
                let mut eps = state.endpoints.lock();
                let ep_state = eps.entry(ep_addr).or_default();
                for info in vmo_ids {
                    if let (Some(id), Some(size)) = (info.id, info.size) {
                        let vmo = zx::Vmo::create(size).expect("create VMO");
                        let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup VMO");
                        ep_state.vmos.insert(id, vmo);
                        registered.push(fusb_endpoint::VmoHandle {
                            id: Some(id),
                            vmo: Some(dup),
                            ..Default::default()
                        });
                    }
                }
                drop(eps);
                let _ = responder.send(registered);
            }
            fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                let mut eps = state.endpoints.lock();
                let ep_state = eps.entry(ep_addr).or_default();
                for id in vmo_ids {
                    ep_state.vmos.remove(&id);
                }
                drop(eps);
                let _ = responder.send(&[], &[]);
            }
            fusb_endpoint::EndpointRequest::QueueRequests { req: reqs, control_handle } => {
                let mut completions = Vec::new();
                let mut echoed_packets = Vec::new();
                let auto_echo = state.auto_echo.load(Ordering::SeqCst);
                let mut eps = state.endpoints.lock();
                let ep_state = eps.entry(ep_addr).or_default();
                for r in reqs {
                    let (id, offset, size) = request_region(&r);
                    if is_in {
                        if let Some(mut pkt) = ep_state.in_packets.pop_front() {
                            let written = std::cmp::min(pkt.len(), size as usize);
                            if written > 0 {
                                if let Some(vmo) = ep_state.vmos.get(&id) {
                                    vmo.write(&pkt[..written], offset).expect("write VMO");
                                }
                            }
                            if written < pkt.len() {
                                ep_state.in_packets.push_front(pkt.split_off(written));
                            }
                            completions.push(fusb_endpoint::Completion {
                                request: Some(r),
                                status: Some(zx::sys::ZX_OK),
                                transfer_size: Some(written as u64),
                                ..Default::default()
                            });
                        } else {
                            ep_state.held_in.push_back((r, control_handle.clone()));
                        }
                    } else {
                        let actual_size = size as usize;
                        let mut buf = vec![0u8; actual_size];
                        if actual_size > 0 {
                            if let Some(vmo) = ep_state.vmos.get(&id) {
                                vmo.read(&mut buf, offset).expect("read VMO");
                            }
                        }
                        if auto_echo {
                            if !buf.is_empty() {
                                echoed_packets.push(buf);
                            }
                        } else {
                            ep_state.out_packets.push_back(buf);
                        }
                        completions.push(fusb_endpoint::Completion {
                            request: Some(r),
                            status: Some(zx::sys::ZX_OK),
                            transfer_size: Some(actual_size as u64),
                            ..Default::default()
                        });
                    }
                }
                drop(eps);
                for pkt in echoed_packets {
                    state.push_in(state.first_in_ep, pkt);
                }
                if !completions.is_empty() {
                    let _ = control_handle.send_on_completion(completions);
                }
            }
            fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                let drained: Vec<_> = {
                    let mut eps = state.endpoints.lock();
                    eps.entry(ep_addr).or_default().held_in.drain(..).collect()
                };
                for (req, ch) in drained {
                    let _ = ch.send_on_completion(vec![fusb_endpoint::Completion {
                        request: Some(req),
                        status: Some(zx::sys::ZX_ERR_CANCELED),
                        transfer_size: Some(0),
                        ..Default::default()
                    }]);
                }
                let _ = responder.send(Ok(()));
            }
            _ => {}
        }
    }
}

fn spawn_replayer_usb_server(
    kernel: &Arc<Kernel>,
    spec: &DeviceSpec,
) -> (Arc<fhardware_usb::UsbSynchronousProxy>, Arc<ReplayerUsbServerState>) {
    let (client_end, server_end) = create_endpoints::<fhardware_usb::UsbMarker>();
    let sync_proxy = Arc::new(fhardware_usb::UsbSynchronousProxy::new(client_end.into_channel()));
    let state = ReplayerUsbServerState::new(spec);
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
                        let is_in = (setup.bm_request_type & (USB_DIR_IN as u8)) != 0;
                        let is_vendor = (setup.bm_request_type & 0x60) == 0x40;
                        let w_len = usize::from(setup.w_length);
                        let b_req = setup.b_request;

                        let scripted = state_srv
                            .scripted_controls
                            .lock()
                            .iter()
                            .rev()
                            .find(|r| {
                                r.matches(
                                    setup.bm_request_type,
                                    b_req,
                                    setup.w_value,
                                    setup.w_index,
                                    setup.w_length,
                                )
                            })
                            .map(|r| r.action.clone());
                        match scripted {
                            Some(ControlScriptAction::Stall) => {
                                let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                            }
                            Some(ControlScriptAction::Reply(mut reply_bytes)) => {
                                if is_in {
                                    if reply_bytes.len() > w_len {
                                        reply_bytes.truncate(w_len);
                                    }
                                    let _ = responder.send(Ok(&reply_bytes));
                                } else {
                                    if is_vendor && b_req == 0x01 {
                                        *state_srv.last_ctrl_out.lock() = write_data;
                                    }
                                    let _ = responder.send(Ok(&[]));
                                }
                            }
                            Some(ControlScriptAction::Hold(hold_id)) => {
                                let pre_released =
                                    state_srv.released_controls.lock().remove(&hold_id);
                                if let Some(released_action) = pre_released {
                                    state_srv.respond_to_control(
                                        &setup,
                                        write_data,
                                        responder,
                                        released_action,
                                    );
                                } else {
                                    let infinite_ns = zx::MonotonicDuration::INFINITE.into_nanos();
                                    if timeout_ns > 0 && timeout_ns < infinite_ns {
                                        let wait_ms = (timeout_ns / 1_000_000).clamp(1, 500) as u64;
                                        fuchsia_async::Timer::new(Duration::from_millis(wait_ms))
                                            .await;
                                        let _ = responder.send(Err(zx::sys::ZX_ERR_TIMED_OUT));
                                    } else {
                                        state_srv.held_controls.lock().push_back(
                                            HeldControlTransfer {
                                                hold_id,
                                                setup,
                                                write_data,
                                                responder,
                                            },
                                        );
                                    }
                                }
                            }
                            None => {
                                if setup.bm_request_type == 0x80
                                    && b_req == 0x06
                                    && (setup.w_value >> 8) == 0x01
                                {
                                    let len = std::cmp::min(
                                        state_srv.device_descriptor_bytes.len(),
                                        w_len,
                                    );
                                    let _ = responder
                                        .send(Ok(&state_srv.device_descriptor_bytes[..len]));
                                } else if setup.bm_request_type == 0x80
                                    && b_req == 0x06
                                    && (setup.w_value >> 8) == 0x02
                                {
                                    let cfg_idx = (setup.w_value & 0xff) as usize;
                                    if let Some(cfg_bytes) =
                                        state_srv.config_descriptors.get(cfg_idx)
                                    {
                                        let len = std::cmp::min(cfg_bytes.len(), w_len);
                                        let _ = responder.send(Ok(&cfg_bytes[..len]));
                                    } else {
                                        let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                                    }
                                } else if is_vendor && b_req == 0xff {
                                    let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                                } else if is_in {
                                    let mut resp = if is_vendor && b_req == 0x01 {
                                        let stored = state_srv.last_ctrl_out.lock().clone();
                                        if stored.is_empty() {
                                            vec![0x11, 0x22, 0x33, 0x44]
                                        } else {
                                            stored
                                        }
                                    } else {
                                        vec![0x42; w_len]
                                    };
                                    resp.resize(w_len, 0);
                                    let _ = responder.send(Ok(&resp));
                                } else {
                                    if is_vendor && b_req == 0x01 {
                                        *state_srv.last_ctrl_out.lock() = write_data;
                                    }
                                    let _ = responder.send(Ok(&[]));
                                }
                            }
                        }
                    }
                    fhardware_usb::UsbRequest::SetInterface { responder, .. } => {
                        let _ = responder.send(Ok(()));
                    }
                    fhardware_usb::UsbRequest::SetConfiguration { responder, .. } => {
                        let _ = responder.send(Ok(()));
                    }
                    fhardware_usb::UsbRequest::EnableEndpoint { responder, .. } => {
                        let _ = responder.send(Ok(()));
                    }
                    fhardware_usb::UsbRequest::ResetEndpoint { responder, .. } => {
                        let _ = responder.send(Ok(()));
                    }
                    fhardware_usb::UsbRequest::ConnectToEndpoint { ep_addr, ep, responder } => {
                        let _ = responder.send(Ok(()));
                        let ep_state = Arc::clone(&state_srv);
                        kernel_for_srv.kthreads.spawn_future(
                            move || serve_replayer_endpoint(ep_addr, ep, ep_state),
                            "replayer_usb_ep_server",
                        );
                    }
                    _ => {}
                }
            }
        },
        "replayer_usb_server",
    );

    (sync_proxy, state)
}

fn build_device_descriptor(spec: &DeviceSpec) -> usb_device_descriptor {
    let bytes = spec.build_device_descriptor_bytes();
    usb_device_descriptor::read_from_bytes(&bytes).expect("valid 18-byte usb_device_descriptor")
}

fn speed_from_spec(spec: &DeviceSpec) -> u32 {
    match spec.speed.to_ascii_lowercase().as_str() {
        "low" => USB_SPEED_LOW,
        "full" => USB_SPEED_FULL,
        "high" => USB_SPEED_HIGH,
        "super" => USB_SPEED_SUPER,
        "super_plus" | "super-plus" => USB_SPEED_SUPER_PLUS,
        _ => USB_SPEED_UNKNOWN,
    }
}

fn create_and_register_device(
    current_task: &CurrentTask,
    manager: &UsbDeviceManager,
    spec: &DeviceSpec,
) -> (Arc<UsbDevice>, Device, Arc<ReplayerUsbServerState>) {
    let kernel = current_task.kernel();
    let (sync_proxy, server_state) = spawn_replayer_usb_server(kernel, spec);
    let dev_num = manager.allocate_dev_num().unwrap_or(DEV_NUM);
    let mut usb_device = UsbDevice::new(
        1,
        dev_num,
        speed_from_spec(spec),
        build_device_descriptor(spec),
        spec.build_config_descriptor_bytes(),
    );
    usb_device.sync_proxy = Some(sync_proxy);
    usb_device.memory = Arc::clone(&manager.memory);
    let usb_device = Arc::new(usb_device);
    let registered =
        manager.add_device(kernel, Arc::clone(&usb_device)).expect("add_device failed");
    (usb_device, registered, server_state)
}

#[derive(Clone)]
struct CanaryUserBuffer {
    base_addr: UserAddress,
    user_addr: UserAddress,
    user_len: usize,
}

impl CanaryUserBuffer {
    fn new(
        ctx: &ReplayerContext,
        current_task: &CurrentTask,
        user_len: usize,
        is_in: bool,
        arch32: bool,
    ) -> Self {
        let total_len = CANARY_PAD + user_len + CANARY_PAD;
        let base_addr = ctx.map_rw_buffer(current_task, total_len, arch32);
        let user_addr = (base_addr + CANARY_PAD).expect("user_addr offset");
        let mut init_bytes = vec![CANARY_BYTE; total_len];
        let fill = if is_in { IN_INIT_BYTE } else { 0u8 };
        for b in &mut init_bytes[CANARY_PAD..CANARY_PAD + user_len] {
            *b = fill;
        }
        current_task.write_memory(base_addr, &init_bytes).expect("init canary buffer");
        Self { base_addr, user_addr, user_len }
    }

    fn write_at(&self, current_task: &CurrentTask, offset: usize, bytes: &[u8]) {
        if offset + bytes.len() <= self.user_len && !bytes.is_empty() {
            let dst = (self.user_addr + offset).expect("write_at offset");
            current_task.write_memory(dst, bytes).expect("write_at user memory");
        }
    }

    fn read_range(&self, current_task: &CurrentTask, start: usize, end: usize) -> Vec<u8> {
        let clamped_end = end.min(self.user_len);
        let clamped_start = start.min(clamped_end);
        let len = clamped_end - clamped_start;
        if len == 0 {
            return Vec::new();
        }
        let src = (self.user_addr + clamped_start).expect("read_range offset");
        current_task.read_memory_to_vec(src, len).expect("read_range user memory")
    }

    fn canary_intact(&self, current_task: &CurrentTask) -> bool {
        let leading =
            current_task.read_memory_to_vec(self.base_addr, CANARY_PAD).expect("read leading");
        let trailing_addr = (self.user_addr + self.user_len).expect("trailing offset");
        let trailing =
            current_task.read_memory_to_vec(trailing_addr, CANARY_PAD).expect("read trailing");
        leading.iter().all(|&b| b == CANARY_BYTE) && trailing.iter().all(|&b| b == CANARY_BYTE)
    }

    fn unwritten_tail_intact(&self, current_task: &CurrentTask, written_end: usize) -> bool {
        if written_end >= self.user_len {
            return true;
        }
        let tail = self.read_range(current_task, written_end, self.user_len);
        tail.iter().all(|&b| b == IN_INIT_BYTE)
    }
}

struct SubmittedUrbMem {
    urb_addr: UserAddress,
    canary_buf: CanaryUserBuffer,
    is_in: bool,
    is_control: bool,
    arch32: bool,
    in_flight: bool,
}

struct ReplayerContext {
    scenario_name: String,
    device_spec: DeviceSpec,
    divergences: DivergenceManifest,
    manager: UsbDeviceManager,
    device: Arc<UsbDevice>,
    registered_device: Option<Device>,
    server: Arc<ReplayerUsbServerState>,
    fds: HashMap<String, FileHandle>,
    urbs: HashMap<String, SubmittedUrbMem>,
    addr_to_urb: HashMap<u64, String>,
    scratch_page: UserAddress,
    unmapped_page: UserAddress,
    next_low32_hint: AtomicU64,
    pending_tasks: HashMap<String, Step>,
    deferred_disconnect: bool,
}

impl ReplayerContext {
    fn map_rw_buffer(&self, current_task: &CurrentTask, len: usize, arch32: bool) -> UserAddress {
        let page_size = *PAGE_SIZE as usize;
        let map_len = (len.max(page_size) + page_size - 1) & !(page_size - 1);
        let hint = if arch32 {
            let offset =
                self.next_low32_hint.fetch_add((map_len + page_size) as u64, Ordering::SeqCst);
            UserAddress::from(0x2000_0000u64 + offset)
        } else {
            UserAddress::NULL
        };
        let addr = do_mmap(
            current_task,
            hint,
            map_len,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_PRIVATE,
            FdNumber::from_raw(-1),
            0,
        )
        .expect("do_mmap rw buffer");
        if arch32 {
            assert!(
                u32::try_from(addr.ptr()).is_ok(),
                "arch32 buffer {addr:?} exceeded 32-bit address space"
            );
        }
        addr
    }

    fn resolve_ptr(&self, mode: Option<&str>, valid_addr: UserAddress) -> UserAddress {
        match mode.unwrap_or("valid") {
            "null" => UserAddress::NULL,
            "unmapped" => self.unmapped_page,
            _ => valid_addr,
        }
    }

    fn get_fd(&self, params: &Value) -> Option<FileHandle> {
        let name = params.get("fd").and_then(Value::as_str).unwrap_or("fd1");
        self.fds.get(name).cloned()
    }
}

fn errno_to_outcome(err: Errno) -> StepOutcome {
    StepOutcome::error(errno_to_name(err.code.error_code() as i32))
}

fn syscall_res_to_i64(res: Result<SyscallResult, Errno>) -> Result<i64, Errno> {
    res.map(|v| v.value() as i64)
}

fn parse_open_flags(mode: &str) -> OpenFlags {
    match mode {
        "rdonly" | "O_RDONLY" => OpenFlags::RDONLY,
        "wronly" | "O_WRONLY" => OpenFlags::WRONLY,
        _ => OpenFlags::RDWR,
    }
}

fn parse_poll_mask(events: Option<&Vec<Value>>) -> FdEvents {
    let Some(arr) = events else {
        return FdEvents::POLLIN
            | FdEvents::POLLOUT
            | FdEvents::POLLWRNORM
            | FdEvents::POLLERR
            | FdEvents::POLLHUP;
    };
    let mut mask = FdEvents::POLLERR | FdEvents::POLLHUP | FdEvents::POLLNVAL;
    for v in arr {
        match v.as_str().unwrap_or("") {
            "POLLIN" | "EPOLLIN" => mask |= FdEvents::POLLIN,
            "POLLOUT" | "EPOLLOUT" => mask |= FdEvents::POLLOUT,
            "POLLWRNORM" => mask |= FdEvents::POLLWRNORM,
            "POLLRDNORM" => mask |= FdEvents::POLLRDNORM,
            "POLLERR" | "EPOLLERR" => mask |= FdEvents::POLLERR,
            "POLLHUP" | "EPOLLHUP" => mask |= FdEvents::POLLHUP,
            "POLLPRI" => mask |= FdEvents::POLLPRI,
            _ => {}
        }
    }
    mask
}

fn format_fd_events(events: FdEvents) -> Vec<String> {
    let mut out = Vec::new();
    if events.contains(FdEvents::POLLIN) {
        out.push("POLLIN".to_string());
    }
    if events.contains(FdEvents::POLLOUT) {
        out.push("POLLOUT".to_string());
    }
    if events.contains(FdEvents::POLLWRNORM) {
        out.push("POLLWRNORM".to_string());
    }
    if events.contains(FdEvents::POLLERR) {
        out.push("POLLERR".to_string());
    }
    if events.contains(FdEvents::POLLHUP) {
        out.push("POLLHUP".to_string());
    }
    if events.contains(FdEvents::POLLNVAL) {
        out.push("POLLNVAL".to_string());
    }
    out
}

fn normalize_uevent(raw: &str, busnum: u16, devnum: u16) -> String {
    let minor = (u32::from(busnum).saturating_sub(1) * 128) + u32::from(devnum).saturating_sub(1);
    let minor_exact = format!("MINOR={minor}");
    let devname_exact = format!("DEVNAME=bus/usb/{:03}/{:03}", busnum, devnum);
    let devname_norm = format!("DEVNAME=bus/usb/{:03}/$DEVNUM", busnum);
    let devnum_exact = format!("DEVNUM={:03}", devnum);
    let devpath_exact = format!("DEVPATH=/devices/{}-{}", busnum, devnum);
    let devpath_norm = format!("DEVPATH=/devices/{}-$DEVNUM", busnum);

    let mut out_lines = Vec::new();
    for line in raw.lines() {
        if line.starts_with("DRIVER=") {
            continue;
        }
        if line == minor_exact {
            out_lines.push("MINOR=$MINOR".to_string());
        } else if line == devname_exact {
            out_lines.push(devname_norm.clone());
        } else if line == devnum_exact {
            out_lines.push("DEVNUM=$DEVNUM".to_string());
        } else if line == devpath_exact {
            out_lines.push(devpath_norm.clone());
        } else {
            out_lines.push(line.to_string());
        }
    }
    let mut joined = out_lines.join("\n");
    joined.push('\n');
    joined
}

fn parse_urb_flags(params: &Value) -> u32 {
    let mut flags = params.get("raw_extra_flags").and_then(Value::as_u64).unwrap_or(0) as u32;
    if let Some(arr) = params.get("flags").and_then(Value::as_array) {
        for item in arr {
            match item.as_str().unwrap_or("") {
                "SHORT_NOT_OK" | "USBDEVFS_URB_SHORT_NOT_OK" => flags |= USBDEVFS_URB_SHORT_NOT_OK,
                "ISO_ASAP" | "USBDEVFS_URB_ISO_ASAP" => flags |= USBDEVFS_URB_ISO_ASAP,
                "BULK_CONTINUATION" | "USBDEVFS_URB_BULK_CONTINUATION" => {
                    flags |= USBDEVFS_URB_BULK_CONTINUATION
                }
                "NO_FSBR" | "USBDEVFS_URB_NO_FSBR" => flags |= USBDEVFS_URB_NO_FSBR,
                "ZERO_PACKET" | "USBDEVFS_URB_ZERO_PACKET" => flags |= USBDEVFS_URB_ZERO_PACKET,
                "NO_INTERRUPT" | "USBDEVFS_URB_NO_INTERRUPT" => flags |= USBDEVFS_URB_NO_INTERRUPT,
                _ => {}
            }
        }
    }
    flags
}

fn execute_step_outcome(
    ctx: &mut ReplayerContext,
    current_task: &CurrentTask,
    step: &Step,
) -> StepOutcome {
    let p = &step.params;
    let arg_mode = p.get("arg_ptr").and_then(Value::as_str);
    let data_mode = p.get("data_ptr").and_then(Value::as_str);
    let arch32 = p.get("arch32").and_then(Value::as_bool).unwrap_or(false);

    match step.op.as_str() {
        "open" => {
            let fd_name = p.get("fd").and_then(Value::as_str).unwrap_or("fd1").to_string();
            let mode = p.get("mode").and_then(Value::as_str).unwrap_or("rdwr");
            let path = format!("bus/usb/{:03}/{:03}", ctx.device.bus_num, ctx.device.dev_num);
            let dev_root = NamespaceNode::new_anonymous(
                DevTmpFs::from_kernel(current_task.kernel()).root().clone(),
            );
            let mut lookup = LookupContext::default();
            let res = current_task
                .lookup_path(&mut lookup, dev_root, path.as_bytes().into())
                .and_then(|node| node.open(current_task, parse_open_flags(mode)));
            match res {
                Ok(file) => {
                    if let Some(old) = ctx.fds.insert(fd_name.clone(), file) {
                        drop(old);
                        current_task.kernel().delayed_releaser.apply(current_task);
                    }
                    StepOutcome::ok(json!({ "fd": fd_name }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "close" => {
            let fd_name = p.get("fd").and_then(Value::as_str).unwrap_or("fd1");
            if let Some(file) = ctx.fds.remove(fd_name) {
                drop(file);
                current_task.kernel().delayed_releaser.apply(current_task);
                StepOutcome::ok(json!({ "closed": true }))
            } else {
                StepOutcome::error("EBADF")
            }
        }
        "stat_dev" => {
            let info_res = if let Some(fd_name) = p.get("fd").and_then(Value::as_str) {
                ctx.fds
                    .get(fd_name)
                    .map(|f| f.node().info().clone())
                    .ok_or_else(|| starnix_uapi::errno!(EBADF))
            } else {
                let path = format!("bus/usb/{:03}/{:03}", ctx.device.bus_num, ctx.device.dev_num);
                let dev_root = NamespaceNode::new_anonymous(
                    DevTmpFs::from_kernel(current_task.kernel()).root().clone(),
                );
                let mut lookup = LookupContext::default();
                current_task
                    .lookup_path(&mut lookup, dev_root, path.as_bytes().into())
                    .map(|node| node.entry.node.info().clone())
            };
            match info_res {
                Ok(info) => {
                    let is_char_dev = info.mode.is_chr();
                    let major = info.rdev.major();
                    let minor = info.rdev.minor() as i64;
                    let expected_minor =
                        ((ctx.device.bus_num as i64 - 1) * 128) + (ctx.device.dev_num as i64 - 1);
                    StepOutcome::ok(json!({
                        "is_char_dev": is_char_dev,
                        "major": major,
                        "minor_offset_from_devnum": minor - expected_minor,
                    }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "read" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let size = p.get("size").and_then(Value::as_u64).unwrap_or(256) as usize;
            let mut buf = VecOutputBuffer::new(size);
            match file.read(current_task, &mut buf) {
                Ok(n) => {
                    let mut bytes = Vec::from(buf);
                    bytes.truncate(n);
                    StepOutcome::ok(json!({ "bytes_read": n, "data": bytes }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "pread" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let size = p.get("size").and_then(Value::as_u64).unwrap_or(64) as usize;
            let offset = p.get("offset").and_then(Value::as_i64).unwrap_or(0);
            if offset < 0 {
                return StepOutcome::error("EINVAL");
            }
            let mut buf = VecOutputBuffer::new(size);
            match file.read_at(current_task, offset as usize, &mut buf) {
                Ok(n) => {
                    let mut bytes = Vec::from(buf);
                    bytes.truncate(n);
                    StepOutcome::ok(json!({ "bytes_read": n, "data": bytes }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "lseek" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let offset = p.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let target = match p.get("whence") {
                Some(Value::String(s)) => match s.as_str() {
                    "SEEK_SET" => SeekTarget::Set(offset),
                    "SEEK_CUR" => SeekTarget::Cur(offset),
                    "SEEK_END" => SeekTarget::End(offset),
                    _ => return StepOutcome::error("EINVAL"),
                },
                Some(Value::Number(n)) => match n.as_i64().unwrap_or(0) {
                    0 => SeekTarget::Set(offset),
                    1 => SeekTarget::Cur(offset),
                    2 => SeekTarget::End(offset),
                    _ => return StepOutcome::error("EINVAL"),
                },
                _ => SeekTarget::Set(offset),
            };
            match file.seek(current_task, target) {
                Ok(pos) => StepOutcome::ok(json!({ "offset": pos })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "write" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let data: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_else(|| vec![1, 2, 3, 4]);
            let mut buf = VecInputBuffer::from(data);
            match file.write(current_task, &mut buf) {
                Ok(n) => StepOutcome::ok(json!({ "bytes_written": n })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "poll" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::ok(json!({ "ready": 1, "revents": ["POLLNVAL"] }));
            };
            let mask = parse_poll_mask(p.get("events").and_then(Value::as_array));
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut revents =
                file.ops().query_events(&file, current_task).expect("query_events") & mask;
            while revents.is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
                revents =
                    file.ops().query_events(&file, current_task).expect("query_events") & mask;
            }
            let ready = if revents.is_empty() { 0 } else { 1 };
            StepOutcome::ok(json!({
                "ready": ready,
                "revents": format_fd_events(revents),
            }))
        }
        "epoll_check" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let mask = parse_poll_mask(p.get("events").and_then(Value::as_array));
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_i64).unwrap_or(0).max(0) as u64;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut revents =
                file.ops().query_events(&file, current_task).expect("query_events") & mask;
            while revents.is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
                revents =
                    file.ops().query_events(&file, current_task).expect("query_events") & mask;
            }
            let ready = if revents.is_empty() { 0 } else { 1 };
            StepOutcome::ok(json!({
                "ready": ready,
                "revents": format_fd_events(revents),
            }))
        }
        "read_sysfs" => {
            let attr = p.get("attr").and_then(Value::as_str).unwrap_or("idVendor");
            let sysfs = get_sysfs(current_task.kernel());
            let root = NamespaceNode::new_anonymous(sysfs.root().clone());
            let path =
                format!("bus/usb/devices/{}-{}/{}", ctx.device.bus_num, ctx.device.dev_num, attr);
            let mut lookup = LookupContext::default();
            let res = current_task
                .lookup_path(&mut lookup, root, path.as_bytes().into())
                .and_then(|node| node.open(current_task, OpenFlags::RDONLY))
                .and_then(|file| {
                    let mut buf = VecOutputBuffer::new(4096);
                    let n = file.read(current_task, &mut buf)?;
                    let mut bytes = Vec::from(buf);
                    bytes.truncate(n);
                    Ok(bytes)
                });
            match res {
                Ok(bytes) => {
                    if attr == "descriptors" {
                        StepOutcome::ok(json!({ "attr": attr, "bytes": bytes }))
                    } else {
                        let val = String::from_utf8_lossy(&bytes).into_owned();
                        let normalized =
                            if attr == "devnum" && val.trim() == ctx.device.dev_num.to_string() {
                                "$DEVNUM\n".to_string()
                            } else if attr == "uevent" {
                                normalize_uevent(&val, ctx.device.bus_num, ctx.device.dev_num)
                            } else {
                                val
                            };
                        StepOutcome::ok(json!({ "attr": attr, "value": normalized }))
                    }
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "get_speed" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_GET_SPEED,
                UserAddress::NULL.into(),
            )) {
                Ok(speed) => StepOutcome::ok(json!({ "speed": speed })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "get_capabilities" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                current_task.write_object(UserRef::new(addr), &0xffff_ffffu32).expect("write u32");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_GET_CAPABILITIES,
                addr.into(),
            )) {
                Ok(_) => {
                    let caps: u32 =
                        current_task.read_object(UserRef::new(addr)).expect("read caps");
                    StepOutcome::ok(json!({ "capabilities": caps }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "connectinfo" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_CONNECTINFO,
                addr.into(),
            )) {
                Ok(_) => {
                    let ci: usbdevfs_connectinfo =
                        current_task.read_object(UserRef::new(addr)).expect("read connectinfo");
                    StepOutcome::ok(json!({
                        "devnum_matches": ci.devnum == u32::from(ctx.device.dev_num),
                        "slow": ci.slow,
                    }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "get_driver" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                let gd = usbdevfs_getdriver { interface, ..Default::default() };
                current_task.write_object(UserRef::new(addr), &gd).expect("write getdriver");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_GETDRIVER,
                addr.into(),
            )) {
                Ok(_) => {
                    let gd: usbdevfs_getdriver =
                        current_task.read_object(UserRef::new(addr)).expect("read getdriver");
                    let drv_bytes: Vec<u8> =
                        gd.driver.iter().map(|&c| c as u8).take_while(|&b| b != 0).collect();
                    let drv_str = String::from_utf8_lossy(&drv_bytes).into_owned();
                    StepOutcome::ok(json!({ "interface": gd.interface, "driver": drv_str }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "claim_interface" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                current_task.write_object(UserRef::new(addr), &interface).expect("write ifno");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_CLAIMINTERFACE,
                addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "claimed": interface })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "release_interface" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                current_task.write_object(UserRef::new(addr), &interface).expect("write ifno");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_RELEASEINTERFACE,
                addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "released": interface })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "disconnect_claim" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut flags = p.get("raw_flags").and_then(Value::as_u64).unwrap_or(0) as u32;
            if let Some(arr) = p.get("flags").and_then(Value::as_array) {
                for f in arr {
                    match f.as_str().unwrap_or("") {
                        "IF_DRIVER" => flags |= USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
                        "EXCEPT_DRIVER" => flags |= USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER,
                        _ => {}
                    }
                }
            }
            let driver = p.get("driver").and_then(Value::as_str).unwrap_or("usbfs");
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                let mut dc = usbdevfs_disconnect_claim { interface, flags, ..Default::default() };
                for (dst, &src) in dc.driver.iter_mut().zip(driver.as_bytes().iter()) {
                    *dst = src as _;
                }
                current_task.write_object(UserRef::new(addr), &dc).expect("write disconnect_claim");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_DISCONNECT_CLAIM,
                addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "claimed": interface })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "set_interface" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let altsetting = p.get("altsetting").and_then(Value::as_u64).unwrap_or(0) as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                let set = usbdevfs_setinterface { interface, altsetting };
                current_task.write_object(UserRef::new(addr), &set).expect("write setinterface");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_SETINTERFACE,
                addr.into(),
            )) {
                Ok(_) => {
                    StepOutcome::ok(json!({ "interface": interface, "altsetting": altsetting }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "set_configuration" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let config = p.get("config").and_then(Value::as_i64).unwrap_or(1) as i32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                current_task.write_object(UserRef::new(addr), &config).expect("write config");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_SETCONFIGURATION,
                addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "config": config })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "clear_halt" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let endpoint =
                p.get("endpoint").or_else(|| p.get("ep")).and_then(Value::as_u64).unwrap_or(0x81)
                    as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            if arg_mode.unwrap_or("valid") == "valid" {
                current_task.write_object(UserRef::new(addr), &endpoint).expect("write ep");
            }
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_CLEAR_HALT,
                addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "endpoint": endpoint })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "reset" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_RESET,
                UserAddress::NULL.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "reset": true })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "raw_ioctl" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let request = p.get("request").and_then(Value::as_u64).unwrap_or(0xdead_beef) as u32;
            let addr = ctx.resolve_ptr(arg_mode, ctx.scratch_page);
            match syscall_res_to_i64(file.ops().ioctl(&file, current_task, request, addr.into())) {
                Ok(rc) => StepOutcome::ok(json!({ "retval": rc })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "usbdevfs_ioctl" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let (ifno, ioctl_code) =
                parse_usbdevfs_ioctl_params(p, USBDEVFS_DISCONNECT as i32, USBDEVFS_CONNECT as i32);
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let ioctl_size = (((ioctl_code as u32) >> 16) & 0x3fff) as usize;
            let has_data = p.get("data").is_some() || data_mode.is_some();
            let data_addr = if has_data {
                let buf_len = ioctl_size.max(data_vec.len()).max(4);
                let canary_buf = CanaryUserBuffer::new(ctx, current_task, buf_len, false, arch32);
                if !data_vec.is_empty() {
                    canary_buf.write_at(current_task, 0, &data_vec);
                }
                ctx.resolve_ptr(data_mode, canary_buf.user_addr)
            } else {
                UserAddress::NULL
            };
            let arg_page = ctx.map_rw_buffer(current_task, 64, arch32);
            let addr = ctx.resolve_ptr(arg_mode, arg_page);
            let ioctl_req = if arch32 {
                #[cfg(target_arch = "aarch64")]
                {
                    if arg_mode.unwrap_or("valid") == "valid" {
                        let ctl32 = arch32::usbdevfs_ioctl {
                            ifno,
                            ioctl_code,
                            data: uaddr32 { addr: data_addr.ptr() as u32 },
                        };
                        current_task
                            .write_object(UserRef::new(addr), &ctl32)
                            .expect("write ioctl32");
                    }
                    arch32::USBDEVFS_IOCTL
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    let _ = data_addr;
                    unreachable!("arch32 steps are only replayed on aarch64")
                }
            } else {
                if arg_mode.unwrap_or("valid") == "valid" {
                    let ctl = usbdevfs_ioctl {
                        ifno,
                        ioctl_code,
                        data: uaddr { addr: data_addr.ptr() as u64 },
                    };
                    current_task.write_object(UserRef::new(addr), &ctl).expect("write ioctl");
                }
                USBDEVFS_IOCTL
            };
            match syscall_res_to_i64(file.ops().ioctl(&file, current_task, ioctl_req, addr.into()))
            {
                Ok(rc) => StepOutcome::ok(json!({ "ifno": ifno, "retval": rc })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "control" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let bm_request_type =
                p.get("bm_request_type").and_then(Value::as_u64).unwrap_or(0) as u8;
            let b_request = p.get("b_request").and_then(Value::as_u64).unwrap_or(0) as u8;
            let w_value = p.get("w_value").and_then(Value::as_u64).unwrap_or(0) as u16;
            let w_index = p.get("w_index").and_then(Value::as_u64).unwrap_or(0) as u16;
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let w_length = p
                .get("w_length")
                .and_then(Value::as_u64)
                .map(|v| v as u16)
                .unwrap_or(data_vec.len() as u16);
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000) as u32;

            let is_in = (bm_request_type & (USB_DIR_IN as u8)) != 0;
            let buf_len = (w_length as usize).max(data_vec.len());
            let canary_buf = CanaryUserBuffer::new(ctx, current_task, buf_len, is_in, arch32);
            if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(current_task, 0, &data_vec);
            }
            let data_addr = ctx.resolve_ptr(data_mode, canary_buf.user_addr);
            let arg_page = ctx.map_rw_buffer(current_task, 64, arch32);
            let arg_addr = ctx.resolve_ptr(arg_mode, arg_page);

            let ioctl_req = if arch32 {
                #[cfg(target_arch = "aarch64")]
                {
                    if arg_mode.unwrap_or("valid") == "valid" {
                        let ctrl32 = arch32::usbdevfs_ctrltransfer {
                            bRequestType: bm_request_type,
                            bRequest: b_request,
                            wValue: w_value,
                            wIndex: w_index,
                            wLength: w_length,
                            timeout: timeout_ms,
                            data: uaddr32 { addr: data_addr.ptr() as u32 },
                        };
                        current_task
                            .write_object(UserRef::new(arg_addr), &ctrl32)
                            .expect("write ctrl32");
                    }
                    arch32::USBDEVFS_CONTROL
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    unreachable!("arch32 steps are only replayed on aarch64")
                }
            } else {
                if arg_mode.unwrap_or("valid") == "valid" {
                    let ctrl = usbdevfs_ctrltransfer {
                        bRequestType: bm_request_type,
                        bRequest: b_request,
                        wValue: w_value,
                        wIndex: w_index,
                        wLength: w_length,
                        timeout: timeout_ms,
                        data: uaddr { addr: data_addr.ptr() as u64 },
                        ..Default::default()
                    };
                    current_task.write_object(UserRef::new(arg_addr), &ctrl).expect("write ctrl");
                }
                USBDEVFS_CONTROL
            };

            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                ioctl_req,
                arg_addr.into(),
            )) {
                Ok(n) => {
                    let actual = n.max(0) as usize;
                    if is_in {
                        StepOutcome::ok(json!({
                            "bytes_transferred": actual,
                            "data": canary_buf.read_range(current_task, 0, actual),
                            "canary_intact": canary_buf.canary_intact(current_task),
                            "unwritten_tail_intact": canary_buf.unwritten_tail_intact(current_task, actual),
                        }))
                    } else {
                        StepOutcome::ok(json!({
                            "bytes_transferred": actual,
                            "canary_intact": canary_buf.canary_intact(current_task),
                        }))
                    }
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "bulk" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(2) as u32;
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let len = p
                .get("len")
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or(data_vec.len() as u32);
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000) as u32;

            let is_in = (ep & USB_DIR_IN) != 0;
            let alloc_len = if (len as usize) <= 20 * 1024 * 1024 {
                (len as usize).max(data_vec.len())
            } else {
                4096
            };
            let canary_buf = CanaryUserBuffer::new(ctx, current_task, alloc_len, is_in, arch32);
            if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(current_task, 0, &data_vec);
            }
            let data_addr = ctx.resolve_ptr(data_mode, canary_buf.user_addr);
            let arg_page = ctx.map_rw_buffer(current_task, 64, arch32);
            let arg_addr = ctx.resolve_ptr(arg_mode, arg_page);

            let ioctl_req = if arch32 {
                #[cfg(target_arch = "aarch64")]
                {
                    if arg_mode.unwrap_or("valid") == "valid" {
                        let bulk32 = arch32::usbdevfs_bulktransfer {
                            ep,
                            len,
                            timeout: timeout_ms,
                            data: uaddr32 { addr: data_addr.ptr() as u32 },
                        };
                        current_task
                            .write_object(UserRef::new(arg_addr), &bulk32)
                            .expect("write bulk32");
                    }
                    arch32::USBDEVFS_BULK
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    unreachable!("arch32 steps are only replayed on aarch64")
                }
            } else {
                if arg_mode.unwrap_or("valid") == "valid" {
                    let bulk = usbdevfs_bulktransfer {
                        ep,
                        len,
                        timeout: timeout_ms,
                        data: uaddr { addr: data_addr.ptr() as u64 },
                        ..Default::default()
                    };
                    current_task.write_object(UserRef::new(arg_addr), &bulk).expect("write bulk");
                }
                USBDEVFS_BULK
            };

            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                ioctl_req,
                arg_addr.into(),
            )) {
                Ok(n) => {
                    let actual = n.max(0) as usize;
                    if is_in {
                        StepOutcome::ok(json!({
                            "bytes_transferred": actual,
                            "data": canary_buf.read_range(current_task, 0, actual),
                            "canary_intact": canary_buf.canary_intact(current_task),
                            "unwritten_tail_intact": canary_buf.unwritten_tail_intact(current_task, actual),
                        }))
                    } else {
                        StepOutcome::ok(json!({
                            "bytes_transferred": actual,
                            "canary_intact": canary_buf.canary_intact(current_task),
                        }))
                    }
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "submit_urb" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1").to_string();
            let urb_type_val = match p.get("urb_type") {
                Some(Value::String(s)) => match s.as_str() {
                    "iso" => USBDEVFS_URB_TYPE_ISO as u8,
                    "interrupt" => USBDEVFS_URB_TYPE_INTERRUPT as u8,
                    "control" => USBDEVFS_URB_TYPE_CONTROL as u8,
                    "bulk" => USBDEVFS_URB_TYPE_BULK as u8,
                    _ => 99,
                },
                Some(Value::Number(n)) => n.as_u64().unwrap_or(3) as u8,
                _ => USBDEVFS_URB_TYPE_BULK as u8,
            };
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x81) as u8;
            let flags = parse_urb_flags(p);
            let signr = p.get("signr").and_then(Value::as_u64).unwrap_or(0) as u32;
            let usercontext = p.get("usercontext").and_then(Value::as_u64).unwrap_or(0);
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();

            let is_control = urb_type_val == (USBDEVFS_URB_TYPE_CONTROL as u8);
            let setup_opt = p.get("setup");
            let is_in = if is_control {
                if let Some(setup_obj) = setup_opt {
                    let bm =
                        setup_obj.get("bm_request_type").and_then(Value::as_u64).unwrap_or(0) as u8;
                    let w_len = setup_obj.get("w_length").and_then(Value::as_u64).unwrap_or(0);
                    (bm & 0x80) != 0 && w_len > 0
                } else if data_vec.len() >= SETUP_PACKET_SIZE {
                    let bm = data_vec[0];
                    let w_len = u16::from_le_bytes([data_vec[6], data_vec[7]]);
                    (bm & 0x80) != 0 && w_len > 0
                } else {
                    (ep & 0x80) != 0
                }
            } else {
                (ep & 0x80) != 0
            };

            let default_buf_len = if is_control && setup_opt.is_some() {
                let w_len = setup_opt
                    .unwrap()
                    .get("w_length")
                    .and_then(Value::as_u64)
                    .unwrap_or(data_vec.len() as u64) as i32;
                8 + w_len
            } else {
                data_vec.len() as i32
            };
            let buffer_len = p
                .get("buffer_len")
                .and_then(Value::as_i64)
                .map(|v| v as i32)
                .unwrap_or(default_buf_len);

            let alloc_user_len = if buffer_len > 0 && (buffer_len as usize) <= 20 * 1024 * 1024 {
                (buffer_len as usize).max(data_vec.len() + if is_control { 8 } else { 0 })
            } else {
                64
            };
            let canary_buf =
                CanaryUserBuffer::new(ctx, current_task, alloc_user_len, is_in, arch32);
            if is_control {
                if let Some(setup_obj) = setup_opt {
                    let ctrl_req = usb_ctrlrequest {
                        bRequestType: setup_obj
                            .get("bm_request_type")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u8,
                        bRequest: setup_obj.get("b_request").and_then(Value::as_u64).unwrap_or(0)
                            as u8,
                        wValue: (setup_obj.get("w_value").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                        wIndex: (setup_obj.get("w_index").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                        wLength: (setup_obj.get("w_length").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                    };
                    canary_buf.write_at(current_task, 0, ctrl_req.as_bytes());
                    if !is_in && !data_vec.is_empty() {
                        canary_buf.write_at(current_task, SETUP_PACKET_SIZE, &data_vec);
                    }
                } else if !data_vec.is_empty() {
                    canary_buf.write_at(current_task, 0, &data_vec);
                }
            } else if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(current_task, 0, &data_vec);
            }

            let buf_addr = ctx.resolve_ptr(data_mode, canary_buf.user_addr);
            let valid_urb_addr =
                if let Some(same_as) = p.get("same_addr_as").and_then(Value::as_str) {
                    ctx.urbs
                        .get(same_as)
                        .map(|u| u.urb_addr)
                        .unwrap_or_else(|| ctx.map_rw_buffer(current_task, 128, arch32))
                } else {
                    ctx.map_rw_buffer(current_task, 128, arch32)
                };
            let urb_addr = ctx.resolve_ptr(arg_mode, valid_urb_addr);

            let ioctl_req = if arch32 {
                #[cfg(target_arch = "aarch64")]
                {
                    if arg_mode.unwrap_or("valid") == "valid" {
                        let urb32 = UsbdevfsUrb32Header {
                            type_: urb_type_val,
                            endpoint: ep,
                            status: 0,
                            flags,
                            buffer: buf_addr.ptr() as u32,
                            buffer_length: buffer_len,
                            actual_length: 0,
                            start_frame: 0,
                            number_of_packets_or_stream_id: 0,
                            error_count: 0,
                            signr,
                            usercontext: usercontext as u32,
                            ..Default::default()
                        };
                        current_task
                            .write_object(UserRef::new(urb_addr), &urb32)
                            .expect("write urb32");
                    }
                    arch32::USBDEVFS_SUBMITURB
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    unreachable!("arch32 steps are only replayed on aarch64")
                }
            } else {
                if arg_mode.unwrap_or("valid") == "valid" {
                    let urb = UsbdevfsUrbHeader {
                        type_: urb_type_val,
                        endpoint: ep,
                        status: 0,
                        flags,
                        buffer: buf_addr.ptr() as u64,
                        buffer_length: buffer_len,
                        actual_length: 0,
                        start_frame: 0,
                        number_of_packets_or_stream_id: 0,
                        error_count: 0,
                        signr,
                        usercontext,
                        ..Default::default()
                    };
                    current_task.write_object(UserRef::new(urb_addr), &urb).expect("write urb");
                }
                USBDEVFS_SUBMITURB
            };

            let submit_res = syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                ioctl_req,
                urb_addr.into(),
            ));
            if submit_res.is_ok() || !ctx.addr_to_urb.contains_key(&(valid_urb_addr.ptr() as u64)) {
                ctx.addr_to_urb.insert(valid_urb_addr.ptr() as u64, urb_name.clone());
            }
            ctx.urbs.insert(
                urb_name.clone(),
                SubmittedUrbMem {
                    urb_addr: valid_urb_addr,
                    canary_buf,
                    is_in,
                    is_control,
                    arch32,
                    in_flight: submit_res.is_ok(),
                },
            );

            match submit_res {
                Ok(_) => StepOutcome::ok(json!({ "urb": urb_name })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "discard_urb" => {
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1").to_string();
            let valid_urb_addr = ctx
                .urbs
                .get(&urb_name)
                .map(|u| u.urb_addr)
                .unwrap_or_else(|| (ctx.scratch_page + 512usize).expect("bogus urb offset"));
            let target_addr = ctx.resolve_ptr(arg_mode, valid_urb_addr);
            match syscall_res_to_i64(file.ops().ioctl(
                &file,
                current_task,
                USBDEVFS_DISCARDURB,
                target_addr.into(),
            )) {
                Ok(_) => StepOutcome::ok(json!({ "discarded": urb_name })),
                Err(e) => errno_to_outcome(e),
            }
        }
        "reap_urb" | "reap_urb_ndelay" => {
            let blocking = step.op == "reap_urb";
            let Some(file) = ctx.get_fd(p) else {
                return StepOutcome::error("EBADF");
            };
            let reap_buf = ctx.map_rw_buffer(current_task, 64, arch32);
            let addr = ctx.resolve_ptr(arg_mode, reap_buf);

            let ioctl_req = if arch32 {
                #[cfg(target_arch = "aarch64")]
                {
                    if blocking { arch32::USBDEVFS_REAPURB } else { arch32::USBDEVFS_REAPURBNDELAY }
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    unreachable!("arch32 steps are only replayed on aarch64")
                }
            } else if blocking {
                USBDEVFS_REAPURB
            } else {
                USBDEVFS_REAPURBNDELAY
            };

            let res =
                syscall_res_to_i64(file.ops().ioctl(&file, current_task, ioctl_req, addr.into()));
            match res {
                Ok(_) => {
                    let reaped_ptr: u64 = if arch32 {
                        let p32: u32 =
                            current_task.read_object(UserRef::new(addr)).expect("read reaped u32");
                        u64::from(p32)
                    } else {
                        current_task.read_object(UserRef::new(addr)).expect("read reaped u64")
                    };
                    let urb_name = ctx.addr_to_urb.get(&reaped_ptr).cloned().unwrap_or_default();
                    let (status_i32, actual_length, usercontext) =
                        if let Some(slot) = ctx.urbs.get_mut(&urb_name) {
                            slot.in_flight = false;
                            if slot.arch32 {
                                let h: UsbdevfsUrb32Header = current_task
                                    .read_object(UserRef::new(slot.urb_addr))
                                    .expect("read urb32");
                                (h.status, h.actual_length, u64::from(h.usercontext))
                            } else {
                                let h: UsbdevfsUrbHeader = current_task
                                    .read_object(UserRef::new(slot.urb_addr))
                                    .expect("read urb");
                                (h.status, h.actual_length, h.usercontext)
                            }
                        } else {
                            (0, 0, 0)
                        };
                    StepOutcome::ok(json!({
                        "urb": urb_name,
                        "status": urb_status_to_string(status_i32),
                        "actual_length": actual_length,
                        "usercontext": usercontext,
                    }))
                }
                Err(e) => errno_to_outcome(e),
            }
        }
        "inspect_urb" => {
            let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1");
            let Some(slot) = ctx.urbs.get(urb_name) else {
                return StepOutcome::error("ENOENT");
            };
            let (status_i32, actual_length_i32, error_count, usercontext) = if slot.arch32 {
                let h: UsbdevfsUrb32Header =
                    current_task.read_object(UserRef::new(slot.urb_addr)).expect("read urb32");
                (h.status, h.actual_length, h.error_count, u64::from(h.usercontext))
            } else {
                let h: UsbdevfsUrbHeader =
                    current_task.read_object(UserRef::new(slot.urb_addr)).expect("read urb");
                (h.status, h.actual_length, h.error_count, h.usercontext)
            };
            let actual_len = actual_length_i32.max(0) as usize;
            let canary_intact = slot.canary_buf.canary_intact(current_task);
            let (data, trailing_intact) = if slot.is_control {
                if slot.is_in {
                    (
                        slot.canary_buf.read_range(
                            current_task,
                            SETUP_PACKET_SIZE,
                            SETUP_PACKET_SIZE + actual_len,
                        ),
                        slot.canary_buf
                            .unwritten_tail_intact(current_task, SETUP_PACKET_SIZE + actual_len),
                    )
                } else {
                    (Vec::new(), true)
                }
            } else if slot.is_in {
                (
                    slot.canary_buf.read_range(current_task, 0, actual_len),
                    slot.canary_buf.unwritten_tail_intact(current_task, actual_len),
                )
            } else {
                (Vec::new(), true)
            };
            StepOutcome::ok(json!({
                "urb": urb_name,
                "status": urb_status_to_string(status_i32),
                "actual_length": actual_length_i32,
                "error_count": error_count,
                "usercontext": usercontext,
                "data": data,
                "canary_intact": canary_intact,
                "trailing_canary_intact": trailing_intact,
                "unwritten_tail_intact": trailing_intact,
            }))
        }
        "start_op" => {
            let task_name = p.get("task").and_then(Value::as_str).unwrap_or("t1").to_string();
            let inner_val = p.get("inner").cloned().unwrap_or(Value::Null);
            let inner_step: Step =
                serde_json::from_value(inner_val).expect("valid inner Step in start_op");
            ctx.pending_tasks.insert(task_name.clone(), inner_step);
            StepOutcome::ok(json!({ "task": task_name, "started": true }))
        }
        "await_task" => {
            let task_name = p.get("task").and_then(Value::as_str).unwrap_or("t1");
            let Some(inner_step) = ctx.pending_tasks.remove(task_name) else {
                return StepOutcome::error("ESRCH");
            };
            let disconnect_thread = if ctx.deferred_disconnect {
                ctx.deferred_disconnect = false;
                let dev_to_disc = Arc::clone(&ctx.device);
                let srv_to_wait = Arc::clone(&ctx.server);
                Some(std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !srv_to_wait.has_held_transfer() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    dev_to_disc.on_disconnect();
                }))
            } else {
                None
            };
            let outcome = execute_step_outcome(ctx, current_task, &inner_step);
            if let Some(handle) = disconnect_thread {
                handle.join().expect("join disconnect thread");
                if let Some(reg_dev) = ctx.registered_device.take() {
                    ctx.manager.remove_device(current_task, reg_dev).expect("remove_device");
                }
            }
            outcome
        }
        "device_script_control" => {
            let bm_request_type = p.get("bm_request_type").and_then(Value::as_u64).map(|v| v as u8);
            let b_request = p.get("b_request").and_then(Value::as_u64).unwrap_or(0) as u8;
            let w_value = p.get("w_value").and_then(Value::as_u64).map(|v| v as u16);
            let w_index = p.get("w_index").and_then(Value::as_u64).map(|v| v as u16);
            let w_length = p.get("w_length").and_then(Value::as_u64).map(|v| v as u16);
            let action = p.get("action").cloned().unwrap_or(Value::Null);
            ctx.server.script_control(
                bm_request_type,
                b_request,
                w_value,
                w_index,
                w_length,
                &action,
            );
            StepOutcome::ok(json!({ "ok": true }))
        }
        "device_release_control" => {
            let hold = p.get("hold").and_then(Value::as_str).unwrap_or("h1");
            let action = p.get("action").cloned().unwrap_or(Value::Null);
            ctx.server.release_control(hold, &action);
            StepOutcome::ok(json!({ "ok": true }))
        }
        "device_ep_push_in" => {
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x81) as u8;
            let data: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let len = data.len();
            ctx.server.push_in(ep, data);
            StepOutcome::ok(json!({ "ep": ep, "bytes_pushed": len }))
        }
        "device_ep_pop_out" => {
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x02) as u8;
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000);
            if let Some(data) = ctx.server.pop_out(ep, timeout_ms) {
                StepOutcome::ok(json!({
                    "ep": ep,
                    "bytes_popped": data.len(),
                    "data": data,
                }))
            } else {
                StepOutcome::error("ETIMEDOUT")
            }
        }
        "device_ep_assert_empty" => {
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x02) as u8;
            if ctx.server.is_ep_empty(ep) {
                StepOutcome::ok(json!({ "ep": ep, "empty": true }))
            } else {
                StepOutcome::error("EAGAIN")
            }
        }
        "device_disconnect" => {
            if !ctx.pending_tasks.is_empty() {
                ctx.deferred_disconnect = true;
            } else if let Some(reg_dev) = ctx.registered_device.take() {
                if ctx.urbs.values().any(|u| u.in_flight && (u.is_in || u.is_control)) {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !ctx.server.has_held_transfer() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                for u in ctx.urbs.values_mut() {
                    u.in_flight = false;
                }
                ctx.manager.remove_device(current_task, reg_dev).expect("remove_device");
            }
            StepOutcome::ok(json!({ "disconnected": true }))
        }
        "device_reconnect" => {
            if ctx.registered_device.is_none() {
                let (new_dev, new_reg, new_srv) =
                    create_and_register_device(current_task, &ctx.manager, &ctx.device_spec);
                ctx.device = new_dev;
                ctx.registered_device = Some(new_reg);
                ctx.server = new_srv;
            }
            StepOutcome::ok(json!({ "reconnected": true }))
        }
        other => panic!("unsupported Step op in starnix_replayer: {other}"),
    }
}

fn execute_recorded_step(
    ctx: &mut ReplayerContext,
    current_task: &CurrentTask,
    step_idx: usize,
    rec: &RecordedStep,
) {
    let actual = execute_step_outcome(ctx, current_task, &rec.step);
    if let Err(msg) = ctx.divergences.matches_or_allows(
        &ctx.scenario_name,
        &rec.step.op,
        &rec.outcome,
        &actual,
        true,
    ) {
        panic!("scenario '{}' step #{step_idx} ({}) failed: {msg}", ctx.scenario_name, rec.step.op);
    }
}

/// Replays a single golden trace JSON against Starnix `UsbDevice` and `ReplayerUsbServerState`.
pub(crate) fn replay_trace(name: &str, trace_json: &str) {
    let trace: RecordedTrace = serde_json::from_str(trace_json).expect("valid RecordedTrace JSON");
    #[cfg(not(target_arch = "aarch64"))]
    assert!(
        !trace.steps.iter().any(|s| s.step.uses_arch32()),
        "scenario '{name}' uses arch32 steps and must be marked #[ignore] on non-aarch64 targets"
    );
    let divergences: DivergenceManifest =
        serde_json::from_str(DIVERGENCES_JSON).expect("valid DivergenceManifest JSON");
    let scenario_name = name.to_string();

    let mut exec = fuchsia_async::LocalExecutor::default();
    exec.run_singlethreaded(async move {
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");
                let (device, registered, server) =
                    create_and_register_device(current_task, &manager, &trace.device);

                let scratch_page = do_mmap(
                    current_task,
                    UserAddress::NULL,
                    *PAGE_SIZE as usize,
                    PROT_READ | PROT_WRITE,
                    MAP_ANONYMOUS | MAP_PRIVATE,
                    FdNumber::from_raw(-1),
                    0,
                )
                .expect("map scratch page");
                let unmapped_page = do_mmap(
                    current_task,
                    UserAddress::from(0x1ff0_0000u64),
                    *PAGE_SIZE as usize,
                    0,
                    MAP_ANONYMOUS | MAP_PRIVATE,
                    FdNumber::from_raw(-1),
                    0,
                )
                .expect("map unmapped PROT_NONE page");

                let mut ctx = ReplayerContext {
                    scenario_name,
                    device_spec: trace.device.clone(),
                    divergences,
                    manager,
                    device,
                    registered_device: Some(registered),
                    server,
                    fds: HashMap::new(),
                    urbs: HashMap::new(),
                    addr_to_urb: HashMap::new(),
                    scratch_page,
                    unmapped_page,
                    next_low32_hint: AtomicU64::new(0),
                    pending_tasks: HashMap::new(),
                    deferred_disconnect: false,
                };

                for (idx, rec) in trace.steps.iter().enumerate() {
                    execute_recorded_step(&mut ctx, current_task, idx, rec);
                }

                ctx.fds.clear();
                current_task.kernel().delayed_releaser.apply(current_task);
                if let Some(reg_dev) = ctx.registered_device.take() {
                    let _ = ctx.manager.remove_device(current_task, reg_dev);
                }
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    });
}

usbfs_trace_schema::usbfs_trace_tests!(replay_trace);
