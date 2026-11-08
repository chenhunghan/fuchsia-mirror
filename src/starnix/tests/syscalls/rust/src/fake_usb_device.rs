// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake USB device provider serving `fuchsia.hardware.usb.UsbService` for `usb_test`.

use anyhow::Error;
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::{StreamExt as _, TryStreamExt as _};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

struct PendingInRequest {
    conn_id: u64,
    request: fusb_request::Request,
    vmo: Option<zx::Vmo>,
    offset: u64,
    size: usize,
    control_handle: fusb_endpoint::EndpointControlHandle,
}

#[derive(Default)]
struct EndpointState {
    last_ctrl_out: Vec<u8>,
    in_queue: VecDeque<Vec<u8>>,
    pending_in: VecDeque<PendingInRequest>,
}

#[derive(Default)]
struct SharedUsbState {
    active_config: AtomicU8,
    next_conn_id: AtomicU64,
    // Note: fake_usb_device runs on a single-threaded LocalExecutor (ServiceFs::new_local),
    // but state is unified under a single fuchsia_sync::Mutex to enforce atomic queue
    // state transitions and comply with the Fuchsia Rust Rubric.
    state: Mutex<EndpointState>,
}

fn device_descriptor() -> fusb_descriptor::UsbDeviceDescriptor {
    fusb_descriptor::UsbDeviceDescriptor {
        b_length: 18,
        b_descriptor_type: 1,
        bcd_usb: 0x0200,
        b_device_class: 0,
        b_device_sub_class: 0,
        b_device_protocol: 0,
        b_max_packet_size0: 64,
        id_vendor: 0x1234,
        id_product: 0x5678,
        bcd_device: 0x0543,
        i_manufacturer: 1,
        i_product: 2,
        i_serial_number: 3,
        b_num_configurations: 1,
    }
}

fn config_descriptor_bytes() -> Vec<u8> {
    vec![
        // Configuration descriptor (9 bytes):
        // bLength=9, bDescriptorType=CONFIG(2), wTotalLength=32,
        // bNumInterfaces=1, bConfigurationValue=1, iConfiguration=0,
        // bmAttributes=0x80 (bus-powered), bMaxPower=50 (100mA)
        9, 2, 32, 0, 1, 1, 0, 0x80, 50,
        //
        // Interface descriptor (9 bytes):
        // bLength=9, bDescriptorType=INTERFACE(4), bInterfaceNumber=0, bAlternateSetting=0,
        // bNumEndpoints=2, bInterfaceClass=0xff (vendor), SubClass=0, Protocol=0, iInterface=1
        9, 4, 0, 0, 2, 0xff, 0, 0, 1,
        //
        // Endpoint 1 IN descriptor (7 bytes):
        // bLength=7, bDescriptorType=ENDPOINT(5), bEndpointAddress=0x81, bmAttributes=Bulk(2),
        // wMaxPacketSize=512 (0x0200), bInterval=0
        7, 5, 0x81, 2, 0x00, 0x02, 0,
        //
        // Endpoint 2 OUT descriptor (7 bytes):
        // bLength=7, bDescriptorType=ENDPOINT(5), bEndpointAddress=0x02, bmAttributes=Bulk(2),
        // wMaxPacketSize=512 (0x0200), bInterval=0
        7, 5, 0x02, 2, 0x00, 0x02, 0,
    ]
}

async fn handle_endpoint_stream(
    ep_addr: u8,
    mut stream: fusb_endpoint::EndpointRequestStream,
    state: Arc<SharedUsbState>,
) {
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
    let mut vmos: HashMap<u64, zx::Vmo> = HashMap::new();
    while let Ok(Some(req)) = stream.try_next().await {
        match req {
            fusb_endpoint::EndpointRequest::GetInfo { responder } => {
                let _ = responder.send(Err(zx::sys::ZX_ERR_NOT_SUPPORTED));
            }
            fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                let mut registered = Vec::new();
                for info in vmo_ids {
                    if let (Some(id), Some(size)) = (info.id, info.size) {
                        if let Ok(vmo) = zx::Vmo::create(size) {
                            if let Ok(dup) = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                                vmos.insert(id, vmo);
                                registered.push(fusb_endpoint::VmoHandle {
                                    id: Some(id),
                                    vmo: Some(dup),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
                let _ = responder.send(registered);
            }
            fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                for id in &vmo_ids {
                    vmos.remove(id);
                }
                let _ = responder.send(&[], &[]);
            }
            fusb_endpoint::EndpointRequest::QueueRequests { req: reqs, control_handle } => {
                let mut completions = Vec::new();
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
                        .and_then(|id| vmos.get(&id))
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
                        // Bulk IN (0x81)
                        if size == 0 {
                            completions.push(fusb_endpoint::Completion {
                                request: Some(r),
                                status: Some(zx::sys::ZX_OK),
                                transfer_size: Some(0),
                                ..Default::default()
                            });
                            continue;
                        }
                        let mut state_lock = state.state.lock();
                        if let Some(payload) = state_lock.in_queue.pop_front() {
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
                                Ok(()) => (zx::sys::ZX_OK, write_len as u64),
                                Err(status) => {
                                    // If VMO write failed, retain payload in queue.
                                    state_lock.in_queue.push_front(payload);
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
                            // Hold IN request until Bulk OUT provides data or CancelAll cancels it.
                            state_lock.pending_in.push_back(PendingInRequest {
                                conn_id,
                                request: r,
                                vmo,
                                offset,
                                size,
                                control_handle: control_handle.clone(),
                            });
                        }
                    } else {
                        // Bulk OUT (0x02)
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
                            let mut state_lock = state.state.lock();
                            // Satisfy a live waiting IN request, or enqueue for later IN read.
                            let mut delivered = false;
                            while let Some(pending_in) = state_lock.pending_in.pop_front() {
                                let write_len = std::cmp::min(pending_in.size, buf.len());
                                let write_res = if write_len > 0 {
                                    if let Some(ref in_vmo) = pending_in.vmo {
                                        in_vmo.write(&buf[..write_len], pending_in.offset)
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
                                if pending_in
                                    .control_handle
                                    .send_on_completion(vec![fusb_endpoint::Completion {
                                        request: Some(pending_in.request),
                                        status: Some(status),
                                        transfer_size: Some(transfer_size),
                                        ..Default::default()
                                    }])
                                    .is_ok()
                                {
                                    if write_res.is_ok() {
                                        delivered = true;
                                        break;
                                    }
                                }
                            }
                            if !delivered {
                                state_lock.in_queue.push_back(buf);
                            }
                        }
                        completions.push(fusb_endpoint::Completion {
                            request: Some(r),
                            status: Some(zx::sys::ZX_OK),
                            transfer_size: Some(size as u64),
                            ..Default::default()
                        });
                    }
                }
                if !completions.is_empty() {
                    let _ = control_handle.send_on_completion(completions);
                }
            }
            fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                if (ep_addr & 0x80) != 0 {
                    let drained: Vec<_> = state.state.lock().pending_in.drain(..).collect();
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
                }
                let _ = responder.send(Ok(()));
            }
        }
    }
    if (ep_addr & 0x80) != 0 {
        state.state.lock().pending_in.retain(|req| req.conn_id != conn_id);
    }
}

async fn handle_usb_stream(
    mut stream: fhardware_usb::UsbRequestStream,
    state: Arc<SharedUsbState>,
    scope: fuchsia_async::ScopeHandle,
) {
    while let Ok(Some(req)) = stream.try_next().await {
        match req {
            fhardware_usb::UsbRequest::GetDeviceId { responder } => {
                let _ = responder.send(1);
            }
            fhardware_usb::UsbRequest::GetDeviceSpeed { responder } => {
                let _ = responder.send(fusb_descriptor::UsbSpeed::High);
            }
            fhardware_usb::UsbRequest::GetDeviceDescriptor { responder } => {
                let _ = responder.send(&device_descriptor());
            }
            fhardware_usb::UsbRequest::GetConfigurationDescriptor { config, responder } => {
                if config == 0 || config == 1 {
                    let _ = responder.send(Ok(&config_descriptor_bytes()));
                } else {
                    let _ = responder.send(Err(zx::sys::ZX_ERR_INVALID_ARGS));
                }
            }
            fhardware_usb::UsbRequest::GetConfiguration { responder } => {
                let _ = responder.send(state.active_config.load(Ordering::Relaxed));
            }
            fhardware_usb::UsbRequest::SetConfiguration { configuration, responder } => {
                if configuration == 0 || configuration == 1 {
                    state.active_config.store(configuration, Ordering::Relaxed);
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
                if interface_number == 0 && alt_setting == 0 {
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
                if is_vendor && setup.b_request == 0xff {
                    let _ = responder.send(Err(zx::sys::ZX_ERR_IO_REFUSED));
                    continue;
                }
                if (setup.bm_request_type & 0x80) == 0 {
                    // Control OUT
                    if is_vendor && setup.b_request == 0x01 {
                        state.state.lock().last_ctrl_out = write_data;
                    }
                    let _ = responder.send(Ok(&[]));
                } else {
                    // Control IN
                    let w_len = setup.w_length as usize;
                    let mut resp = if is_vendor && setup.b_request == 0x01 {
                        let stored = state.state.lock().last_ctrl_out.clone();
                        if stored.is_empty() { vec![0x11, 0x22, 0x33, 0x44] } else { stored }
                    } else {
                        vec![0x42; w_len]
                    };
                    resp.truncate(w_len);
                    let _ = responder.send(Ok(&resp));
                }
            }
            fhardware_usb::UsbRequest::ConnectToEndpoint { ep_addr, ep, responder } => {
                let _ = responder.send(Ok(()));
                let state_clone = Arc::clone(&state);
                let _ = scope.spawn_local(async move {
                    handle_endpoint_stream(ep_addr, ep.into_stream(), state_clone).await;
                });
            }
            _ => {}
        }
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let state = Arc::new(SharedUsbState { active_config: AtomicU8::new(1), ..Default::default() });
    let scope = fuchsia_async::Scope::new();
    let scope_handle = scope.to_handle();

    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service_instance(
        "default",
        |request: fhardware_usb::UsbServiceRequest| {
            let fhardware_usb::UsbServiceRequest::Device(stream) = request;
            stream
        },
    );
    service_fs.take_and_serve_directory_handle()?;

    service_fs
        .for_each_concurrent(None, move |stream: fhardware_usb::UsbRequestStream| {
            let state = Arc::clone(&state);
            let scope_handle = scope_handle.clone();
            async move {
                handle_usb_stream(stream, state, scope_handle).await;
            }
        })
        .await;

    scope.join().await;

    Ok(())
}
