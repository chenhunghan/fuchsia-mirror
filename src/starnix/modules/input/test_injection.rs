// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
use crate::direct_input::DirectInputDevice;
use crate::direct_input::DirectInputManager;
use fidl::endpoints::{ClientEnd, RequestStream as _};
use fidl_fuchsia_input_injection::{
    InputDeviceRegistryRegisterAndGetDeviceInfoResponse, InputDeviceRegistryRequest,
    InputDeviceRegistryRequestStream,
};
use fidl_fuchsia_input_report as fir;
use futures::StreamExt;
use starnix_core::task::Kernel;
use starnix_logging::log_warn;
use starnix_uapi::errors::Errno;
use std::sync::Arc;
use zx;

/// Server implementing `fuchsia.input.injection.InputDeviceRegistry`.
///
/// Enables test frameworks (such as `input-helper`) to dynamically inject input
/// devices and report streams into Starnix without hardcoded device configs,
/// delegating device creation, minor allocation, reader loops, and ownership gating
/// to [`DirectInputManager`].
pub struct InputDeviceRegistry {
    direct_input: Arc<DirectInputManager>,
}

impl std::fmt::Debug for InputDeviceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputDeviceRegistry").field("direct_input", &self.direct_input).finish()
    }
}

impl InputDeviceRegistry {
    /// Creates a new `InputDeviceRegistry` backed by `direct_input`.
    pub fn new(direct_input: Arc<DirectInputManager>) -> Arc<Self> {
        Arc::new(Self { direct_input })
    }

    /// Serves incoming requests from an `InputDeviceRegistryRequestStream`.
    pub async fn serve_stream(
        self: Arc<Self>,
        mut stream: InputDeviceRegistryRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.next().await {
            match request? {
                InputDeviceRegistryRequest::Register { device, control_handle: _ } => {
                    if let Err(e) = self.register_device(device).await {
                        log_warn!("InputDeviceRegistry::Register failed: {:?}", e);
                    }
                }
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    match self.register_device(device).await {
                        Ok(device_id) => {
                            let _ = responder.send(
                                InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                                    device_id: Some(device_id),
                                    ..Default::default()
                                },
                            );
                        }
                        Err(e) => {
                            log_warn!(
                                "InputDeviceRegistry::RegisterAndGetDeviceInfo failed: {:?}",
                                e
                            );
                            let _ = responder.send(
                                InputDeviceRegistryRegisterAndGetDeviceInfoResponse::default(),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Registers an input device client by delegating to [`DirectInputManager::add_device`].
    pub async fn register_device(
        &self,
        device_client_end: ClientEnd<fir::InputDeviceMarker>,
    ) -> Result<u32, Errno> {
        self.direct_input.add_device(device_client_end.into_proxy(), None).await
    }

    /// Removes and unregisters an injected device.
    #[cfg(test)]
    pub fn remove_device(&self, device_id: u32) {
        self.direct_input.remove_device(device_id);
    }

    /// Returns an injected device if it is currently registered.
    #[cfg(test)]
    pub(crate) fn device(&self, device_id: u32) -> Option<DirectInputDevice> {
        self.direct_input.device(device_id)
    }
}

/// Serves `fuchsia.input.injection.InputDeviceRegistry` on the given request stream
/// using the kernel's active `DirectInputManager`.
pub fn serve_input_device_registry(kernel: &Kernel, stream: InputDeviceRegistryRequestStream) {
    let Some(direct_input) = kernel.expando.peek::<Arc<DirectInputManager>>() else {
        log_warn!("DirectInputManager not available; rejecting InputDeviceRegistry request");
        stream.control_handle().shutdown_with_epitaph(zx::Status::NOT_SUPPORTED);
        return;
    };
    let registry = InputDeviceRegistry::new((*direct_input).clone());
    kernel.kthreads.spawn_future(
        move || async move {
            if let Err(e) = registry.serve_stream(stream).await {
                log_warn!("InputDeviceRegistry serve error: {e:?}");
            }
        },
        "input_device_registry",
    );
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::{self, RequestStream, ServerEnd};
    use fidl_fuchsia_input_injection::{self as finputinjection, InputDeviceRegistryMarker};
    use fuchsia_async as fasync;
    use starnix_core::device::DeviceMode;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::{FileHandle, VecOutputBuffer};
    use starnix_sync::Mutex;
    use starnix_uapi::device_id::{DeviceId as StarnixDeviceId, INPUT_MAJOR};
    use starnix_uapi::errno;
    use starnix_uapi::errors::{EAGAIN, Errno};
    use zerocopy::FromBytes;

    const INPUT_EVENT_SIZE: usize = std::mem::size_of::<starnix_uapi::input_event>();

    fn read_uapi_events(
        file: &FileHandle,
        current_task: &starnix_core::task::CurrentTask,
    ) -> Vec<starnix_uapi::input_event> {
        std::iter::from_fn(|| {
            let mut event_bytes = VecOutputBuffer::new(INPUT_EVENT_SIZE);
            match file.read(current_task, &mut event_bytes) {
                Ok(INPUT_EVENT_SIZE) => Some(
                    starnix_uapi::input_event::read_from_bytes(Vec::from(event_bytes).as_slice())
                        .map_err(|_| anyhow::anyhow!("failed to read input_event from buffer")),
                ),
                Ok(other_size) => Some(Err(anyhow::anyhow!(
                    "got {} bytes (expected {})",
                    other_size,
                    INPUT_EVENT_SIZE
                ))),
                Err(Errno { code: EAGAIN, .. }) => None,
                Err(other_error) => Some(Err(anyhow::anyhow!("read failed: {:?}", other_error))),
            }
        })
        .enumerate()
        .map(|(i, read_res)| match read_res {
            Ok(event) => event,
            Err(e) => panic!("unexpected result {:?} on iteration {}", e, i),
        })
        .collect()
    }

    fn make_test_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x2345),
                version: Some(1),
                product_name: Some("Test Touch".to_string()),
                ..Default::default()
            }),
            touch: Some(fir::TouchDescriptor {
                input: Some(fir::TouchInputDescriptor {
                    contacts: Some(vec![fir::ContactInputDescriptor {
                        position_x: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 1080 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        position_y: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 2400 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        ..Default::default()
                    }]),
                    max_contacts: Some(10),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_keyboard_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x1234),
                version: Some(1),
                product_name: Some("Test Keyboard".to_string()),
                ..Default::default()
            }),
            keyboard: Some(fir::KeyboardDescriptor {
                input: Some(fir::KeyboardInputDescriptor::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[::fuchsia::test]
    async fn test_registration_and_event_dispatch_v2() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<fir::InputReportsReaderV2ControlHandle>();
            let mut reader_sender_opt = Some(reader_sender);
            let (ack_sender, ack_receiver) = futures::channel::oneshot::channel::<()>();
            let mut ack_sender_opt = Some(ack_sender);

            // Mock InputDevice server
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                let mut inner_tasks = Vec::new();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let reader_stream = reader.into_stream();
                            let control_handle = reader_stream.control_handle();
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(control_handle);
                            }
                            responder.send(1).unwrap();

                            let ack_sender_opt_inner = ack_sender_opt.take();
                            inner_tasks.push(fasync::Task::local(async move {
                                let mut ack_sender_opt_inner = ack_sender_opt_inner;
                                let mut reader_stream = reader_stream;
                                while let Some(req) = reader_stream.next().await {
                                    match req.unwrap() {
                                        fir::InputReportsReaderV2Request::AcknowledgeReports {
                                            ..
                                        } => {
                                            if let Some(sender) = ack_sender_opt_inner.take() {
                                                let _ = sender.send(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let injected = registry.device(device_id).expect("device should be registered");

            let file = injected.input_device.open_test(&current_task).unwrap();
            let control_handle = reader_receiver.await.unwrap();

            // Inject a touch report
            let touch_report = fir::InputReport {
                event_time: Some(100_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(500),
                        position_y: Some(600),
                        pressure: Some(50),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            control_handle.send_on_input_reports(vec![touch_report], 1).unwrap();

            // Deterministic sync: wait until reader acknowledges report
            ack_receiver.await.unwrap();

            let events = read_uapi_events(&file, &current_task);
            assert!(!events.is_empty());
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                && e.value == 500));
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                && e.value == 600));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pre_open_buffering() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<fir::InputReportsReaderV2ControlHandle>();
            let mut reader_sender_opt = Some(reader_sender);
            let (ack_sender, ack_receiver) = futures::channel::oneshot::channel::<()>();
            let mut ack_sender_opt = Some(ack_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                let mut inner_tasks = Vec::new();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let reader_stream = reader.into_stream();
                            let control_handle = reader_stream.control_handle();
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(control_handle);
                            }
                            responder.send(1).unwrap();

                            let ack_inner = ack_sender_opt.take();
                            inner_tasks.push(fasync::Task::local(async move {
                                let mut ack_inner = ack_inner;
                                let mut reader_stream = reader_stream;
                                while let Some(req) = reader_stream.next().await {
                                    match req.unwrap() {
                                        fir::InputReportsReaderV2Request::AcknowledgeReports {
                                            ..
                                        } => {
                                            if let Some(sender) = ack_inner.take() {
                                                let _ = sender.send(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let injected = registry.device(device_id).expect("device should be registered");

            let control_handle = reader_receiver.await.unwrap();

            // Inject a touch report BEFORE opening the file
            let touch_report = fir::InputReport {
                event_time: Some(150_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(700),
                        position_y: Some(800),
                        pressure: Some(40),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            control_handle.send_on_input_reports(vec![touch_report], 1).unwrap();

            // Deterministic sync: wait until reader acknowledges report
            ack_receiver.await.unwrap();

            // Now open the device *after* the event has already been dispatched
            let file = injected.input_device.open_test(&current_task).unwrap();

            let events = read_uapi_events(&file, &current_task);
            assert!(!events.is_empty());
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                && e.value == 700));
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                && e.value == 800));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_injected_device_minor_in_direct_input_minor_range() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let dev = registry.device(device_id).expect("device should be registered");

            assert!(crate::DIRECT_INPUT_MINOR_RANGE.contains(&dev.minor));
            assert_eq!(dev.minor, crate::DIRECT_INPUT_MINOR_RANGE.start);
            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, dev.minor), DeviceMode::Char)
                    .is_ok()
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_reports_not_delivered_when_ownership_inactive() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            // Explicitly inactive ownership
            direct_input.set_ownership_active(false);
            let registry = InputDeviceRegistry::new(direct_input.clone());
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<fir::InputReportsReaderV2ControlHandle>();
            let mut reader_sender_opt = Some(reader_sender);
            let (ack_sender, mut ack_receiver) = futures::channel::mpsc::channel::<()>(16);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                let mut inner_tasks = Vec::new();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let reader_stream = reader.into_stream();
                            let control_handle = reader_stream.control_handle();
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(control_handle);
                            }
                            responder.send(1).unwrap();

                            let mut ack_sender_clone = ack_sender.clone();
                            inner_tasks.push(fasync::Task::local(async move {
                                let mut reader_stream = reader_stream;
                                while let Some(req) = reader_stream.next().await {
                                    match req.unwrap() {
                                        fir::InputReportsReaderV2Request::AcknowledgeReports {
                                            ..
                                        } => {
                                            let _ = ack_sender_clone.try_send(());
                                        }
                                        _ => {}
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let injected = registry.device(device_id).expect("device should be registered");
            let file = injected.input_device.open_test(&current_task).unwrap();

            // While ownership is inactive, reader task is not spawned and no reports are delivered.
            let events = read_uapi_events(&file, &current_task);
            assert!(events.is_empty(), "reports must not be delivered when ownership is inactive");

            // Now activate ownership, which resumes readers and spawns the reader task
            direct_input.set_ownership_active(true);

            let control_handle = reader_receiver.await.unwrap();

            // Inject a touch report
            let touch_report = fir::InputReport {
                event_time: Some(200_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(400),
                        position_y: Some(500),
                        pressure: Some(30),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            control_handle.send_on_input_reports(vec![touch_report], 1).unwrap();

            // Wait for reader acknowledgment
            ack_receiver.next().await.unwrap();

            let events_after = read_uapi_events(&file, &current_task);
            assert!(
                !events_after.is_empty(),
                "reports must be delivered after ownership is activated"
            );
            assert!(events_after.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                && e.value == 400));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_rollback_on_closed_channel() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);

            let (closed_client, closed_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            drop(closed_server);

            let result = registry.register_device(closed_client).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), errno!(EIO));
            assert!(registry.device(1).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_and_get_device_info_fidl() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();

            let reg_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = reg_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (_reader_sender, _reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(_reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let proxy = registry_client.into_proxy();
            let res = proxy.register_and_get_device_info(device_client).await.unwrap();
            assert!(res.device_id.is_some());
            let id = res.device_id.unwrap();
            assert!(registry.device(id).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_one_way_fidl() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();

            let reg_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = reg_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (_reader_sender, _reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(_reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let proxy = registry_client.into_proxy();
            proxy.register(device_client).unwrap();

            // Wait deterministically for registration to complete
            while registry.device(1).is_none() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(registry.device(1).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_teardown() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            assert!(registry.device(device_id).is_some());

            // Receive reader server end and immediately drop it to simulate client disconnect.
            let reader_server = reader_receiver.await.unwrap();
            drop(reader_server);

            // Wait deterministically for disconnect handler to trigger
            while registry.device(device_id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(registry.device(device_id).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multi_device_registration_and_minor_allocation() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input.clone());
            let mut _tasks = Vec::new();
            let readers = Arc::new(Mutex::new(Vec::new()));

            // Device 1: Touch
            let (device_client_1, device_server_1) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_1 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_1.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_1.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            // Device 2: Keyboard
            let (device_client_2, device_server_2) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_2 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_2.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_keyboard_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_2.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let (res_1, res_2) = futures::join!(
                registry.register_device(device_client_1),
                registry.register_device(device_client_2),
            );
            let id_1 = res_1.unwrap();
            let id_2 = res_2.unwrap();

            assert_ne!(id_1, id_2);

            let dev_1 = registry.device(id_1).expect("device 1 registered");
            let dev_2 = registry.device(id_2).expect("device 2 registered");

            assert_ne!(dev_1.minor, dev_2.minor);
            assert!(crate::DIRECT_INPUT_MINOR_RANGE.contains(&dev_1.minor));
            assert!(crate::DIRECT_INPUT_MINOR_RANGE.contains(&dev_2.minor));
            assert_eq!(direct_input.num_devices(), 2);

            registry.remove_device(id_1);
            assert!(registry.device(id_1).is_none());
            assert!(registry.device(id_2).is_some());
            assert_eq!(direct_input.num_devices(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_registry_drop_unregisters_devices() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input.clone());
            let mut _tasks = Vec::new();
            let readers = Arc::new(Mutex::new(Vec::new()));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_clone = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_clone.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id = registry.register_device(device_client).await.unwrap();
            let dev = registry.device(id).expect("device should be registered");
            let minor = dev.minor;
            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                    .is_ok()
            );

            // Dropping the registry and direct_input must cleanly unregister devices
            drop(registry);
            drop(direct_input);

            while kernel
                .device_registry
                .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                .is_ok()
            {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_immediate_reader_termination_does_not_leak() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input.clone());

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let server_task = fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            // Immediately drop reader endpoint so stream terminates
                            drop(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            });

            let id = registry.register_device(device_client).await.unwrap();
            let minor = registry.device(id).expect("device should be registered").minor;

            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                    .is_ok(),
                "char device should be present in device registry"
            );

            // Wait deterministically for reader loop termination and remove_device to complete.
            while registry.device(id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(registry.device(id).is_none());
            assert_eq!(direct_input.num_devices(), 0);

            // Assert that the mock InputDevice server's request stream ends (channel closed).
            server_task.await;

            // Wait for the async kthread unregistration to complete and remove the char device.
            while kernel
                .device_registry
                .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                .is_ok()
            {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                    .is_err(),
                "char device must be unregistered"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_and_get_device_info_failure_does_not_close_stream() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input);
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();
            let registry_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = registry_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let registry_proxy = registry_client.into_proxy();

            // 1. Send invalid device client whose channel is immediately closed.
            let (closed_client, closed_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            drop(closed_server);

            let res = registry_proxy
                .register_and_get_device_info(closed_client)
                .await
                .expect("request should not fail FIDL call");
            assert_eq!(res.device_id, None);

            // 2. Stream should remain open; a valid registration succeeds.
            let (valid_client, valid_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = valid_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let res2 = registry_proxy
                .register_and_get_device_info(valid_client)
                .await
                .expect("stream should still be open and return Ok");
            assert!(res2.device_id.is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_remove_device_aborts_reader_task() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            direct_input.set_ownership_active(true);
            let registry = InputDeviceRegistry::new(direct_input.clone());
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id = registry.register_device(device_client).await.unwrap();
            assert!(registry.device(id).is_some());

            registry.remove_device(id);
            assert!(registry.device(id).is_none());
            assert_eq!(direct_input.num_devices(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_while_ownership_inactive_removes_device() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            // Ownership starts inactive (!is_ownership_active() and is_paused: true).
            let direct_input = DirectInputManager::new(Arc::downgrade(&kernel), None, None);
            assert!(!direct_input.is_ownership_active());
            let registry = InputDeviceRegistry::new(direct_input.clone());
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                if let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let dev = registry.device(device_id).expect("device must be registered");
            let minor = dev.minor;
            assert_eq!(direct_input.num_devices(), 1);

            // Assert that the char device is registered in device_registry.
            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                    .is_ok(),
                "char device should be present in device registry"
            );

            // Wait for disconnect handler to trigger and remove the device while inactive.
            while registry.device(device_id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(registry.device(device_id).is_none());
            assert_eq!(direct_input.num_devices(), 0);

            // Wait for the async kthread unregistration to complete and remove the char device.
            while kernel
                .device_registry
                .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                .is_ok()
            {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, minor), DeviceMode::Char)
                    .is_err(),
                "char device must be unregistered"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_serve_input_device_registry_without_manager_closes_stream() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            // Verify that no DirectInputManager exists in expando initially.
            assert!(kernel.expando.peek::<Arc<DirectInputManager>>().is_none());

            let (client, server) =
                endpoints::create_endpoints::<finputinjection::InputDeviceRegistryMarker>();

            serve_input_device_registry(&kernel, server.into_stream());

            // Since no manager exists, the stream must be closed with NOT_SUPPORTED epitaph.
            let proxy = client.into_proxy();
            let mut event_stream = proxy.take_event_stream();
            let event = event_stream.next().await;
            assert_matches!(
                event,
                Some(Err(fidl::Error::ClientChannelClosed { epitaph, .. })) if epitaph == zx::Status::NOT_SUPPORTED
            );

            // Serving the registry without a manager must NOT lazily initialize one.
            assert!(kernel.expando.peek::<Arc<DirectInputManager>>().is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_framebuffer_and_registry_share_same_direct_input_manager() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            // Simulate framebuffer module initializing the DirectInputManager via get_or_init_direct_input.
            let fb_manager = crate::get_or_init_direct_input(&kernel);

            // Repeated calls return the identical instance.
            let fb_manager_second = crate::get_or_init_direct_input(&kernel);
            assert!(Arc::ptr_eq(&fb_manager, &fb_manager_second));

            // Peek into expando to ensure the registry path accesses the exact same manager.
            let peeked = kernel
                .expando
                .peek::<Arc<DirectInputManager>>()
                .expect("DirectInputManager should be present in expando");
            assert!(Arc::ptr_eq(&fb_manager, &peeked));

            // Serve the registry and verify it operates on the shared manager.
            let (client, server) =
                endpoints::create_endpoints::<finputinjection::InputDeviceRegistryMarker>();
            serve_input_device_registry(&kernel, server.into_stream());

            let mut _tasks = Vec::new();
            let proxy = client.into_proxy();
            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let response = proxy.register_and_get_device_info(device_client).await.unwrap();
            let device_id = response.device_id.unwrap();

            // The registered device must exist in the shared fb_manager!
            assert!(fb_manager.device(device_id).is_some());
            assert_eq!(fb_manager.num_devices(), 1);

            // Clean up resources before executor terminates so no receivers outlive it.
            drop(proxy);
            drop(_tasks);
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(50),
            ))
            .await;
            drop(fb_manager);
            drop(fb_manager_second);
            drop(peeked);
            kernel.expando.remove::<Arc<DirectInputManager>>();
        })
        .await;
    }
}
