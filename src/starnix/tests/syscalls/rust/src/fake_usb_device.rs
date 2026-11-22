// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake USB device provider serving `fuchsia.hardware.usb.UsbService` and a TCP
//! `DeviceCommand` control plane for `usb_test`.

use anyhow::Error;
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use fidl_fuchsia_io as fio;
use fuchsia_sync::Mutex;
use futures::TryStreamExt as _;
use futures::future::{AbortHandle, Abortable};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use usbfs_trace_schema::{DeviceCommand, DeviceResponse, DeviceSpec};
use vfs::directory::helper::DirectlyMutable as _;
use vfs::directory::immutable::simple::Simple;
use vfs::execution_scope::ExecutionScope;

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

struct PendingInRequest {
    conn_id: u64,
    request: fusb_request::Request,
    vmo: Option<zx::Vmo>,
    offset: u64,
    size: usize,
    control_handle: fusb_endpoint::EndpointControlHandle,
}

#[derive(Default)]
struct EndpointQueueState {
    in_packets: VecDeque<Vec<u8>>,
    out_packets: VecDeque<Vec<u8>>,
    held_in: VecDeque<PendingInRequest>,
    vmos: HashMap<u64, zx::Vmo>,
    cancelled_on_shutdown: bool,
}

struct SharedUsbState {
    spec: DeviceSpec,
    device_id: u32,
    first_in_ep: u8,
    active_config: AtomicU8,
    next_conn_id: AtomicU64,
    dirty_topology: AtomicBool,
    enumerated: AtomicBool,
    shutting_down: AtomicBool,
    auto_echo_bulk: AtomicBool,
    scripted_controls: Mutex<Vec<ScriptedControlRule>>,
    held_controls: Mutex<VecDeque<HeldControlTransfer>>,
    released_controls: Mutex<HashMap<String, ControlScriptAction>>,
    last_ctrl_out: Mutex<Vec<u8>>,
    endpoints: Mutex<HashMap<u8, EndpointQueueState>>,
    abort_handles: Mutex<Vec<AbortHandle>>,
    ep_abort_handles: Mutex<Vec<AbortHandle>>,
}

impl SharedUsbState {
    fn new(spec: DeviceSpec, device_id: u32, auto_echo_bulk: bool) -> Arc<Self> {
        let first_in_ep = spec
            .configs
            .iter()
            .flat_map(|c| c.interfaces.iter())
            .flat_map(|i| i.endpoints.iter())
            .find(|ep| (ep.address & 0x80) != 0)
            .map(|ep| ep.address)
            .unwrap_or(0x81);
        let mut endpoints = HashMap::new();
        for cfg in &spec.configs {
            for iface in &cfg.interfaces {
                for ep in &iface.endpoints {
                    endpoints.entry(ep.address).or_insert_with(EndpointQueueState::default);
                }
            }
        }
        Arc::new(Self {
            spec,
            device_id,
            first_in_ep,
            active_config: AtomicU8::new(1),
            next_conn_id: AtomicU64::new(0),
            dirty_topology: AtomicBool::new(false),
            enumerated: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            auto_echo_bulk: AtomicBool::new(auto_echo_bulk),
            scripted_controls: Mutex::new(Vec::new()),
            held_controls: Mutex::new(VecDeque::new()),
            released_controls: Mutex::new(HashMap::new()),
            last_ctrl_out: Mutex::new(Vec::new()),
            endpoints: Mutex::new(endpoints),
            abort_handles: Mutex::new(Vec::new()),
            ep_abort_handles: Mutex::new(Vec::new()),
        })
    }

    fn reset_in_place(&self, auto_echo_bulk: bool) {
        self.shutting_down.store(false, Ordering::SeqCst);
        self.auto_echo_bulk.store(auto_echo_bulk, Ordering::SeqCst);
        self.scripted_controls.lock().clear();
        self.released_controls.lock().clear();
        for item in self.held_controls.lock().drain(..) {
            let _ = item.responder.send(Err(zx::sys::ZX_ERR_CANCELED));
        }
        self.last_ctrl_out.lock().clear();
        let mut eps = self.endpoints.lock();
        for ep_state in eps.values_mut() {
            ep_state.in_packets.clear();
            ep_state.out_packets.clear();
            ep_state.cancelled_on_shutdown = false;
            for pending in ep_state.held_in.drain(..) {
                let _ =
                    pending.control_handle.send_on_completion(vec![fusb_endpoint::Completion {
                        request: Some(pending.request),
                        status: Some(zx::sys::ZX_ERR_CANCELED),
                        transfer_size: Some(0),
                        ..Default::default()
                    }]);
            }
        }
    }

    fn complete_control_with_action(
        &self,
        setup: &fusb_descriptor::UsbSetup,
        write_data: Vec<u8>,
        responder: fhardware_usb::UsbControlTransferResponder,
        action: ControlScriptAction,
    ) {
        let is_in = (setup.bm_request_type & 0x80) != 0;
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

    fn script_control(
        &self,
        bm_request_type: Option<u8>,
        b_request: u8,
        w_value: Option<u16>,
        w_index: Option<u16>,
        w_length: Option<u16>,
        action: &Value,
    ) -> Result<(), String> {
        if let Some(auto_echo) = action.get("auto_echo").and_then(Value::as_bool) {
            self.auto_echo_bulk.store(auto_echo, Ordering::SeqCst);
            if auto_echo {
                self.scripted_controls.lock().clear();
                self.released_controls.lock().clear();
            }
            return Ok(());
        }
        if let Some(act) = ControlScriptAction::from_value(action) {
            self.scripted_controls.lock().push(ScriptedControlRule {
                bm_request_type,
                b_request,
                w_value,
                w_index,
                w_length,
                action: act,
            });
            return Ok(());
        }
        Err(format!("unsupported ScriptControl action: {action}"))
    }

    fn release_control(&self, hold_id: &str, action: &Value) -> Result<(), String> {
        let Some(resolved) = ControlScriptAction::from_value(action) else {
            return Err(format!("unsupported ReleaseControl action: {action}"));
        };
        let mut held_list = self.held_controls.lock();
        if let Some(pos) = held_list.iter().position(|h| h.hold_id == hold_id) {
            let held = held_list.remove(pos).expect("valid index");
            drop(held_list);
            self.complete_control_with_action(
                &held.setup,
                held.write_data,
                held.responder,
                resolved,
            );
        } else {
            drop(held_list);
            self.released_controls.lock().insert(hold_id.to_string(), resolved);
        }
        Ok(())
    }

    fn push_in(&self, ep: u8, mut data: Vec<u8>) -> Result<(), String> {
        if (ep & 0x80) == 0 {
            return Err(format!("PushIn requires an IN endpoint (0x80..0xff), got 0x{ep:02x}"));
        }
        let mut eps = self.endpoints.lock();
        let Some(ep_state) = eps.get_mut(&ep) else {
            return Err(format!("unknown IN endpoint 0x{ep:02x}"));
        };
        let mut delivered = false;
        while let Some(pending) = ep_state.held_in.pop_front() {
            let write_len = std::cmp::min(pending.size, data.len());
            let write_res = if write_len > 0 {
                if let Some(ref vmo) = pending.vmo {
                    vmo.write(&data[..write_len], pending.offset)
                } else {
                    Err(zx::Status::INVALID_ARGS)
                }
            } else {
                Ok(())
            };
            let (status, transfer_size) = match write_res {
                Ok(()) => (zx::sys::ZX_OK, write_len as u64),
                Err(e) => (e.into_raw(), 0),
            };
            if pending
                .control_handle
                .send_on_completion(vec![fusb_endpoint::Completion {
                    request: Some(pending.request),
                    status: Some(status),
                    transfer_size: Some(transfer_size),
                    ..Default::default()
                }])
                .is_ok()
                && write_res.is_ok()
            {
                data.drain(..write_len);
                if data.is_empty() {
                    delivered = true;
                    break;
                }
            }
        }
        if !delivered {
            ep_state.in_packets.push_back(data);
        }
        Ok(())
    }

    fn pop_out(&self, ep: u8, timeout_ms: u64) -> Result<Vec<u8>, String> {
        if (ep & 0x80) != 0 {
            return Err(format!("PopOut requires an OUT endpoint (0x00..0x7f), got 0x{ep:02x}"));
        }
        if !self.endpoints.lock().contains_key(&ep) {
            return Err(format!("unknown OUT endpoint 0x{ep:02x}"));
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(pkt) =
                self.endpoints.lock().get_mut(&ep).and_then(|s| s.out_packets.pop_front())
            {
                return Ok(pkt);
            }
            if Instant::now() >= deadline {
                return Err("ETIMEDOUT".to_string());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn is_ep_empty(&self, ep: u8) -> Result<bool, String> {
        let eps = self.endpoints.lock();
        let Some(ep_state) = eps.get(&ep) else {
            return Err(format!("unknown endpoint 0x{ep:02x}"));
        };
        Ok(if (ep & 0x80) != 0 {
            ep_state.in_packets.is_empty()
        } else {
            ep_state.out_packets.is_empty()
        })
    }

    fn has_held_transfer(&self) -> bool {
        if !self.held_controls.lock().is_empty() {
            return true;
        }
        self.endpoints.lock().values().any(|ep| !ep.held_in.is_empty())
    }

    fn all_endpoints_cancelled_on_shutdown(&self) -> bool {
        self.endpoints
            .lock()
            .values()
            .filter(|ep| !ep.vmos.is_empty())
            .all(|ep| ep.cancelled_on_shutdown)
    }

    fn shutdown_usb_streams(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        {
            let mut held = self.held_controls.lock();
            for item in held.drain(..) {
                let _ = item.responder.send(Err(zx::sys::ZX_ERR_IO_NOT_PRESENT));
            }
        }
        let handles: Vec<AbortHandle> = self.abort_handles.lock().drain(..).collect();
        for h in handles {
            h.abort();
        }
    }

    fn shutdown_endpoint_streams(&self) {
        {
            let mut eps = self.endpoints.lock();
            for ep_state in eps.values_mut() {
                for pending in ep_state.held_in.drain(..) {
                    let _ = pending.control_handle.send_on_completion(vec![
                        fusb_endpoint::Completion {
                            request: Some(pending.request),
                            status: Some(zx::sys::ZX_ERR_CANCELED),
                            transfer_size: Some(0),
                            ..Default::default()
                        },
                    ]);
                }
            }
        }
        let handles: Vec<AbortHandle> = self.ep_abort_handles.lock().drain(..).collect();
        for h in handles {
            h.abort();
        }
    }
}

fn build_fidl_device_descriptor(spec: &DeviceSpec) -> fusb_descriptor::UsbDeviceDescriptor {
    fusb_descriptor::UsbDeviceDescriptor {
        b_length: 18,
        b_descriptor_type: 1,
        bcd_usb: spec.bcd_usb,
        b_device_class: spec.device_class,
        b_device_sub_class: spec.device_subclass,
        b_device_protocol: spec.device_protocol,
        b_max_packet_size0: spec.max_packet_size0,
        id_vendor: spec.vendor_id,
        id_product: spec.product_id,
        bcd_device: spec.bcd_device,
        i_manufacturer: 1,
        i_product: 2,
        i_serial_number: 3,
        b_num_configurations: spec.configs.len() as u8,
    }
}

fn build_fidl_speed(spec: &DeviceSpec) -> fusb_descriptor::UsbSpeed {
    match spec.speed.to_ascii_lowercase().as_str() {
        "low" => fusb_descriptor::UsbSpeed::Low,
        "full" => fusb_descriptor::UsbSpeed::Full,
        "super" => fusb_descriptor::UsbSpeed::Super,
        "super_plus" | "super-plus" => fusb_descriptor::UsbSpeed::EnhancedSuper,
        "high" => fusb_descriptor::UsbSpeed::High,
        _ => fusb_descriptor::UsbSpeed::Undefined,
    }
}

async fn handle_endpoint_stream(
    ep_addr: u8,
    mut stream: fusb_endpoint::EndpointRequestStream,
    state: Arc<SharedUsbState>,
) {
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
    while let Ok(Some(req)) = stream.try_next().await {
        match req {
            fusb_endpoint::EndpointRequest::GetInfo { responder } => {
                let _ = responder.send(Err(zx::sys::ZX_ERR_NOT_SUPPORTED));
            }
            fusb_endpoint::EndpointRequest::GetAlignmentInfo { responder } => {
                let _ = responder.send(Err(zx::sys::ZX_ERR_NOT_SUPPORTED));
            }
            fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                let mut registered = Vec::new();
                let mut eps = state.endpoints.lock();
                let ep_state = eps.entry(ep_addr).or_default();
                for info in vmo_ids {
                    if let (Some(id), Some(size)) = (info.id, info.size) {
                        if let Ok(vmo) = zx::Vmo::create(size) {
                            if let Ok(dup) = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                                ep_state.vmos.insert(id, vmo);
                                registered.push(fusb_endpoint::VmoHandle {
                                    id: Some(id),
                                    vmo: Some(dup),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
                drop(eps);
                let _ = responder.send(registered);
            }
            fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                let mut eps = state.endpoints.lock();
                let ep_state = eps.entry(ep_addr).or_default();
                for id in &vmo_ids {
                    ep_state.vmos.remove(id);
                }
                drop(eps);
                let _ = responder.send(&[], &[]);
            }
            fusb_endpoint::EndpointRequest::QueueRequests { req: reqs, control_handle } => {
                let mut completions = Vec::new();
                let mut echoed_packets = Vec::new();
                let auto_echo = state.auto_echo_bulk.load(Ordering::SeqCst);
                {
                    let mut eps = state.endpoints.lock();
                    let ep_state = eps.entry(ep_addr).or_default();
                    for r in reqs {
                        let (offset, size, vmo_id) = match r.data.as_ref().and_then(|d| d.first()) {
                            Some(reg) => {
                                let offset = reg.offset.unwrap_or(0);
                                let size = reg.size.unwrap_or(0) as usize;
                                let vmo_id = match reg.buffer {
                                    Some(fusb_request::Buffer::VmoId(id)) => Some(id),
                                    _ => None,
                                };
                                (offset, size, vmo_id)
                            }
                            None => (0, 0, None),
                        };
                        let vmo = vmo_id
                            .and_then(|id| ep_state.vmos.get(&id))
                            .and_then(|v| v.duplicate_handle(zx::Rights::SAME_RIGHTS).ok());
                        if size > 0 && vmo.is_none() {
                            completions.push(fusb_endpoint::Completion {
                                request: Some(r),
                                status: Some(zx::sys::ZX_ERR_INVALID_ARGS),
                                transfer_size: Some(0),
                                ..Default::default()
                            });
                            continue;
                        }

                        if (ep_addr & 0x80) != 0 {
                            if let Some(mut payload) = ep_state.in_packets.pop_front() {
                                let write_len = std::cmp::min(size, payload.len());
                                let write_res = if write_len > 0 {
                                    if let Some(ref v) = vmo {
                                        v.write(&payload[..write_len], offset)
                                    } else {
                                        Err(zx::Status::INVALID_ARGS)
                                    }
                                } else {
                                    Ok(())
                                };
                                let (status, transfer_size) = match write_res {
                                    Ok(()) => {
                                        if write_len < payload.len() {
                                            ep_state
                                                .in_packets
                                                .push_front(payload.split_off(write_len));
                                        }
                                        (zx::sys::ZX_OK, write_len as u64)
                                    }
                                    Err(status) => {
                                        ep_state.in_packets.push_front(payload);
                                        (status.into_raw(), 0)
                                    }
                                };
                                completions.push(fusb_endpoint::Completion {
                                    request: Some(r),
                                    status: Some(status),
                                    transfer_size: Some(transfer_size),
                                    ..Default::default()
                                });
                            } else {
                                ep_state.held_in.push_back(PendingInRequest {
                                    conn_id,
                                    request: r,
                                    vmo,
                                    offset,
                                    size,
                                    control_handle: control_handle.clone(),
                                });
                            }
                        } else {
                            let mut buf = vec![0u8; size];
                            if size > 0 {
                                let read_res = if let Some(ref v) = vmo {
                                    v.read(&mut buf, offset)
                                } else {
                                    Err(zx::Status::INVALID_ARGS)
                                };
                                if let Err(status) = read_res {
                                    completions.push(fusb_endpoint::Completion {
                                        request: Some(r),
                                        status: Some(status.into_raw()),
                                        transfer_size: Some(0),
                                        ..Default::default()
                                    });
                                    continue;
                                }
                            }
                            if auto_echo {
                                if size > 0 {
                                    echoed_packets.push(buf);
                                }
                            } else {
                                ep_state.out_packets.push_back(buf);
                            }
                            completions.push(fusb_endpoint::Completion {
                                request: Some(r),
                                status: Some(zx::sys::ZX_OK),
                                transfer_size: Some(size as u64),
                                ..Default::default()
                            });
                        }
                    }
                }
                for pkt in echoed_packets {
                    let _ = state.push_in(state.first_in_ep, pkt);
                }
                if !completions.is_empty() {
                    let _ = control_handle.send_on_completion(completions);
                }
            }
            fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                let drained: Vec<_> = {
                    let mut eps = state.endpoints.lock();
                    let ep_state = eps.entry(ep_addr).or_default();
                    if state.shutting_down.load(Ordering::SeqCst) {
                        ep_state.cancelled_on_shutdown = true;
                    }
                    if (ep_addr & 0x80) != 0 {
                        ep_state.held_in.drain(..).collect()
                    } else {
                        Vec::new()
                    }
                };
                for pending_in in drained {
                    let _ = pending_in.control_handle.send_on_completion(vec![
                        fusb_endpoint::Completion {
                            request: Some(pending_in.request),
                            status: Some(zx::sys::ZX_ERR_CANCELED),
                            transfer_size: Some(0),
                            ..Default::default()
                        },
                    ]);
                }
                let _ = responder.send(Ok(()));
            }
        }
    }
    {
        let mut eps = state.endpoints.lock();
        let ep_state = eps.entry(ep_addr).or_default();
        ep_state.cancelled_on_shutdown = true;
        if (ep_addr & 0x80) != 0 {
            ep_state.held_in.retain(|req| req.conn_id != conn_id);
        }
    }
}

async fn handle_usb_stream(
    mut stream: fhardware_usb::UsbRequestStream,
    state: Arc<SharedUsbState>,
    scope: fuchsia_async::ScopeHandle,
) {
    let dev_desc_bytes = state.spec.build_device_descriptor_bytes();

    while let Ok(Some(req)) = stream.try_next().await {
        match req {
            fhardware_usb::UsbRequest::GetDeviceId { responder } => {
                let _ = responder.send(state.device_id);
            }
            fhardware_usb::UsbRequest::GetDeviceSpeed { responder } => {
                let _ = responder.send(build_fidl_speed(&state.spec));
            }
            fhardware_usb::UsbRequest::GetDeviceDescriptor { responder } => {
                let _ = responder.send(&build_fidl_device_descriptor(&state.spec));
            }
            fhardware_usb::UsbRequest::GetConfigurationDescriptor { config, responder } => {
                if let Some(bytes) = state.spec.build_single_config_descriptor_bytes(config) {
                    let _ = responder.send(Ok(&bytes));
                } else {
                    let _ = responder.send(Err(zx::sys::ZX_ERR_INVALID_ARGS));
                }
            }
            fhardware_usb::UsbRequest::GetConfiguration { responder } => {
                let cfg = state.active_config.load(Ordering::SeqCst);
                let _ = responder.send(cfg);
                state.enumerated.store(true, Ordering::SeqCst);
            }
            fhardware_usb::UsbRequest::SetConfiguration { configuration, responder } => {
                let valid = configuration == 0
                    || state.spec.configs.iter().any(|c| c.config_value == configuration);
                if valid {
                    state.active_config.store(configuration, Ordering::SeqCst);
                    state.dirty_topology.store(true, Ordering::SeqCst);
                    let _ = responder.send(Ok(()));
                } else {
                    let _ = responder.send(Err(zx::sys::ZX_ERR_INVALID_ARGS));
                }
            }
            fhardware_usb::UsbRequest::SetInterface {
                interface_number,
                alt_setting,
                responder,
            } => {
                let active_cfg = state.active_config.load(Ordering::SeqCst);
                let valid =
                    state.spec.configs.iter().find(|c| c.config_value == active_cfg).is_some_and(
                        |c| {
                            c.interfaces.iter().any(|i| {
                                i.interface_number == interface_number
                                    && i.alt_setting == alt_setting
                            })
                        },
                    );
                if valid {
                    state.dirty_topology.store(true, Ordering::SeqCst);
                    let _ = responder.send(Ok(()));
                } else {
                    let _ = responder.send(Err(zx::sys::ZX_ERR_INVALID_ARGS));
                }
            }
            fhardware_usb::UsbRequest::EnableEndpoint { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fhardware_usb::UsbRequest::ResetEndpoint { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fhardware_usb::UsbRequest::ControlTransfer { setup, write_data, responder, .. } => {
                let is_vendor = (setup.bm_request_type & 0x60) == 0x40;
                let w_len = setup.w_length as usize;
                let b_req = setup.b_request;

                let scripted = state
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
                    Some(ControlScriptAction::Hold(hold_id)) => {
                        let pre_released = state.released_controls.lock().remove(&hold_id);
                        if let Some(resolved) = pre_released {
                            state.complete_control_with_action(
                                &setup, write_data, responder, resolved,
                            );
                        } else {
                            state.held_controls.lock().push_back(HeldControlTransfer {
                                hold_id,
                                setup,
                                write_data,
                                responder,
                            });
                        }
                    }
                    Some(action) => {
                        state.complete_control_with_action(&setup, write_data, responder, action);
                    }
                    None => {
                        if setup.bm_request_type == 0x80
                            && b_req == 0x06
                            && (setup.w_value >> 8) == 0x01
                        {
                            let len = std::cmp::min(dev_desc_bytes.len(), w_len);
                            let _ = responder.send(Ok(&dev_desc_bytes[..len]));
                        } else if setup.bm_request_type == 0x80
                            && b_req == 0x06
                            && (setup.w_value >> 8) == 0x02
                        {
                            let cfg_idx = (setup.w_value & 0xff) as usize;
                            if let Some(cfg_bytes) = state.spec.configs.get(cfg_idx).and_then(|c| {
                                state.spec.build_single_config_descriptor_bytes(c.config_value)
                            }) {
                                let len = std::cmp::min(cfg_bytes.len(), w_len);
                                let _ = responder.send(Ok(&cfg_bytes[..len]));
                            } else {
                                let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                            }
                        } else if is_vendor && b_req == 0xff {
                            let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                        } else if (setup.bm_request_type & 0x80) == 0 {
                            if is_vendor && b_req == 0x01 {
                                *state.last_ctrl_out.lock() = write_data;
                            }
                            let _ = responder.send(Ok(&[]));
                        } else {
                            let mut resp = if is_vendor && b_req == 0x01 {
                                let stored = state.last_ctrl_out.lock().clone();
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
                        }
                    }
                }
            }
            fhardware_usb::UsbRequest::ConnectToEndpoint { ep_addr, ep, responder } => {
                let _ = responder.send(Ok(()));
                let state_clone = Arc::clone(&state);
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                state.ep_abort_handles.lock().push(abort_handle);
                let _ = scope.spawn_local(async move {
                    let _ = Abortable::new(
                        handle_endpoint_stream(ep_addr, ep.into_stream(), state_clone),
                        abort_reg,
                    )
                    .await;
                });
            }
            _ => {}
        }
    }
}

struct UsbServiceController {
    usb_service_dir: Arc<Simple>,
    scope: fuchsia_async::ScopeHandle,
    next_instance_id: AtomicU64,
    active_instance: Mutex<Option<(String, Arc<SharedUsbState>)>>,
    last_spec: Mutex<DeviceSpec>,
}

impl UsbServiceController {
    fn new(usb_service_dir: Arc<Simple>, scope: fuchsia_async::ScopeHandle) -> Arc<Self> {
        Arc::new(Self {
            usb_service_dir,
            scope,
            next_instance_id: AtomicU64::new(1),
            active_instance: Mutex::new(None),
            last_spec: Mutex::new(DeviceSpec::default()),
        })
    }

    fn disconnect_current(&self) {
        let prev = self.active_instance.lock().take();
        if let Some((inst_name, state)) = prev {
            let _ = self.usb_service_dir.remove_entry(&inst_name, false);
            state.shutdown_usb_streams();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !state.all_endpoints_cancelled_on_shutdown() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            state.shutdown_endpoint_streams();
        }
    }

    fn attach_instance(
        &self,
        spec: DeviceSpec,
        auto_echo_bulk: bool,
        wait_for_enum: bool,
    ) -> Arc<SharedUsbState> {
        *self.last_spec.lock() = spec.clone();
        let id = self.next_instance_id.fetch_add(1, Ordering::SeqCst);
        let state = SharedUsbState::new(spec, id as u32, auto_echo_bulk);
        let inst_name = format!("inst_{id}");

        let inst_dir = vfs::directory::immutable::simple();
        let state_for_svc = Arc::clone(&state);
        let scope_for_svc = self.scope.clone();
        let device_svc = vfs::service::host(move |stream: fhardware_usb::UsbRequestStream| {
            let st = Arc::clone(&state_for_svc);
            let scope = scope_for_svc.clone();
            let (abort_handle, abort_reg) = AbortHandle::new_pair();
            st.abort_handles.lock().push(abort_handle);
            async move {
                let _ = Abortable::new(handle_usb_stream(stream, st, scope), abort_reg).await;
            }
        });
        inst_dir.add_entry("device", device_svc).expect("add device entry");
        self.usb_service_dir
            .add_entry(inst_name.as_str(), inst_dir)
            .expect("add UsbService instance entry");

        *self.active_instance.lock() = Some((inst_name, Arc::clone(&state)));

        if wait_for_enum {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !state.enumerated.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        state
    }

    fn handle_command(&self, cmd: DeviceCommand) -> DeviceResponse {
        match cmd {
            DeviceCommand::ConfigureDevice { device } => {
                if let Err(e) = device.validate_for_ffs() {
                    return DeviceResponse::err(e);
                }
                let can_reuse = {
                    let guard = self.active_instance.lock();
                    guard.as_ref().is_some_and(|(_, st)| {
                        st.spec == device
                            && st.enumerated.load(Ordering::SeqCst)
                            && st.active_config.load(Ordering::SeqCst) == 1
                            && !st.dirty_topology.load(Ordering::SeqCst)
                    })
                };
                if can_reuse {
                    if let Some((_, st)) = self.active_instance.lock().as_ref() {
                        st.reset_in_place(false);
                    }
                    DeviceResponse::success()
                } else {
                    self.disconnect_current();
                    let st = self.attach_instance(device, false, true);
                    if st.enumerated.load(Ordering::SeqCst) {
                        DeviceResponse::success()
                    } else {
                        DeviceResponse::err("enumeration timed out")
                    }
                }
            }
            DeviceCommand::ScriptControl {
                bm_request_type,
                b_request,
                w_value,
                w_index,
                w_length,
                action,
            } => {
                if let Some((_, st)) = self.active_instance.lock().as_ref() {
                    match st.script_control(
                        bm_request_type,
                        b_request,
                        w_value,
                        w_index,
                        w_length,
                        &action,
                    ) {
                        Ok(()) => DeviceResponse::success(),
                        Err(e) => DeviceResponse::err(e),
                    }
                } else {
                    DeviceResponse::err("no active USB device instance")
                }
            }
            DeviceCommand::ReleaseControl { hold, action } => {
                if let Some((_, st)) = self.active_instance.lock().as_ref() {
                    match st.release_control(&hold, &action) {
                        Ok(()) => DeviceResponse::success(),
                        Err(e) => DeviceResponse::err(e),
                    }
                } else {
                    DeviceResponse::err("no active USB device instance")
                }
            }
            DeviceCommand::PushIn { ep, data } => {
                if let Some((_, st)) = self.active_instance.lock().as_ref() {
                    match st.push_in(ep, data) {
                        Ok(()) => DeviceResponse::success(),
                        Err(e) => DeviceResponse::err(e),
                    }
                } else {
                    DeviceResponse::err("no active USB device instance")
                }
            }
            DeviceCommand::PopOut { ep, timeout_ms } => {
                let st_opt = self.active_instance.lock().as_ref().map(|(_, s)| Arc::clone(s));
                if let Some(st) = st_opt {
                    match st.pop_out(ep, timeout_ms) {
                        Ok(bytes) => DeviceResponse::success_with_data(bytes),
                        Err(e) => DeviceResponse::err(e),
                    }
                } else {
                    DeviceResponse::err("no active USB device instance")
                }
            }
            DeviceCommand::AssertEmpty { ep } => {
                if let Some((_, st)) = self.active_instance.lock().as_ref() {
                    match st.is_ep_empty(ep) {
                        Ok(true) => DeviceResponse::success(),
                        Ok(false) => DeviceResponse::err("EAGAIN"),
                        Err(e) => DeviceResponse::err(e),
                    }
                } else {
                    DeviceResponse::err("no active USB device instance")
                }
            }
            DeviceCommand::Disconnect { wait_for_held_transfer } => {
                if wait_for_held_transfer {
                    let st_opt = self.active_instance.lock().as_ref().map(|(_, s)| Arc::clone(s));
                    if let Some(st) = st_opt {
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while !st.has_held_transfer() && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
                self.disconnect_current();
                DeviceResponse::success()
            }
            DeviceCommand::Reconnect => {
                if self.active_instance.lock().is_some() {
                    return DeviceResponse::success();
                }
                let spec = self.last_spec.lock().clone();
                let st = self.attach_instance(spec, false, true);
                if st.enumerated.load(Ordering::SeqCst) {
                    DeviceResponse::success()
                } else {
                    DeviceResponse::err("enumeration timed out")
                }
            }
        }
    }
}

static GLOBAL_CONTROLLER: OnceLock<Arc<UsbServiceController>> = OnceLock::new();

fn handle_device_command_json(line: &str) -> String {
    let resp = match serde_json::from_str::<DeviceCommand>(line) {
        Ok(cmd) => {
            if let Some(ctrl) = GLOBAL_CONTROLLER.get() {
                ctrl.handle_command(cmd)
            } else {
                DeviceResponse::err("USB controller not initialized")
            }
        }
        Err(e) => DeviceResponse::err(format!("invalid DeviceCommand JSON: {e}")),
    };
    serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

fn spawn_control_server(addr: &str) {
    let addr_owned = addr.to_string();
    std::thread::spawn(move || {
        let listener =
            TcpListener::bind(&addr_owned).expect("bind fake_usb_device TCP control server");
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut writer = match conn.try_clone() {
                    Ok(w) => w,
                    Err(_) => return,
                };
                let mut reader = BufReader::new(conn);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let mut reply = handle_device_command_json(trimmed);
                        reply.push('\n');
                        if writer.write_all(reply.as_bytes()).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    line.clear();
                }
            });
        }
    });
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let root = vfs::directory::immutable::simple();
    let svc_dir = vfs::directory::immutable::simple();
    let usb_service_dir = vfs::directory::immutable::simple();

    svc_dir
        .add_entry("fuchsia.hardware.usb.UsbService", usb_service_dir.clone())
        .expect("add fuchsia.hardware.usb.UsbService directory");
    root.add_entry("svc", svc_dir).expect("add svc directory");

    let task_scope = fuchsia_async::Scope::new();
    let controller = UsbServiceController::new(usb_service_dir, task_scope.to_handle());
    let _ = GLOBAL_CONTROLLER.set(Arc::clone(&controller));

    // Attach the default 0x1234:0x5678 device immediately with auto_echo_bulk enabled so existing
    // hand-written tests in `usb_test.rs` work out of the box.
    let default_spec = DeviceSpec::default();
    debug_assert_eq!((default_spec.vendor_id, default_spec.product_id), (0x1234, 0x5678));
    controller.attach_instance(default_spec, true, false);

    spawn_control_server(usbfs_trace_schema::DEFAULT_CONTROL_ADDR);

    let scope = ExecutionScope::new();
    if let Some(dir_handle) =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
    {
        vfs::directory::serve_on(
            root,
            fio::PERM_READABLE,
            scope.clone(),
            ServerEnd::new(dir_handle.into()),
        );
    }

    futures::future::pending::<()>().await;
    Ok(())
}
