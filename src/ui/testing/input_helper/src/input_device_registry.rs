// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::InputDevice;
use crate::new_fake_device_info;
use anyhow::{Context as _, Error};
use async_utils::event::Event as AsyncEvent;
use fidl::endpoints;
use fidl_fuchsia_input::Key;
use fidl_fuchsia_input_injection::InputDeviceRegistryProxy;
use fidl_fuchsia_input_report::{
    Axis, ConsumerControlButton, ConsumerControlDescriptor, ConsumerControlInputDescriptor,
    ContactInputDescriptor, DeviceDescriptor, InputDeviceMarker, KeyboardDescriptor,
    KeyboardInputDescriptor, MouseDescriptor, MouseInputDescriptor, Range, TOUCH_MAX_CONTACTS,
    TouchDescriptor, TouchInputDescriptor, TouchType, Unit, UnitType,
};
use fidl_fuchsia_ui_test_input::MouseButton;

/// Implements the client side of the `fuchsia.input.injection.InputDeviceRegistry` protocol.
pub(crate) struct InputDeviceRegistry {
    proxy: InputDeviceRegistryProxy,
    starnix_proxy: Option<InputDeviceRegistryProxy>,
    starnix_connector: Option<crate::input_device::StarnixConnector>,
    got_input_reports_reader: AsyncEvent,
    starnix_registration_timeout: zx::MonotonicDuration,
}

impl InputDeviceRegistry {
    #[cfg(test)]
    pub fn new(
        proxy: InputDeviceRegistryProxy,
        starnix_proxy: Option<InputDeviceRegistryProxy>,
        got_input_reports_reader: AsyncEvent,
    ) -> Self {
        Self::new_with_starnix_connector_and_timeout(
            proxy,
            starnix_proxy,
            None,
            got_input_reports_reader,
            crate::input_device::STARNIX_REGISTRATION_TIMEOUT,
        )
    }

    #[cfg(test)]
    pub fn new_with_starnix_timeout(
        proxy: InputDeviceRegistryProxy,
        starnix_proxy: Option<InputDeviceRegistryProxy>,
        got_input_reports_reader: AsyncEvent,
        starnix_registration_timeout: zx::MonotonicDuration,
    ) -> Self {
        Self::new_with_starnix_connector_and_timeout(
            proxy,
            starnix_proxy,
            None,
            got_input_reports_reader,
            starnix_registration_timeout,
        )
    }

    pub fn new_with_starnix_connector(
        proxy: InputDeviceRegistryProxy,
        starnix_proxy: Option<InputDeviceRegistryProxy>,
        starnix_connector: Option<crate::input_device::StarnixConnector>,
        got_input_reports_reader: AsyncEvent,
    ) -> Self {
        Self::new_with_starnix_connector_and_timeout(
            proxy,
            starnix_proxy,
            starnix_connector,
            got_input_reports_reader,
            crate::input_device::STARNIX_REGISTRATION_TIMEOUT,
        )
    }

    pub fn new_with_starnix_connector_and_timeout(
        proxy: InputDeviceRegistryProxy,
        starnix_proxy: Option<InputDeviceRegistryProxy>,
        starnix_connector: Option<crate::input_device::StarnixConnector>,
        got_input_reports_reader: AsyncEvent,
        starnix_registration_timeout: zx::MonotonicDuration,
    ) -> Self {
        Self {
            proxy,
            starnix_proxy,
            starnix_connector,
            got_input_reports_reader,
            starnix_registration_timeout,
        }
    }

    /// Registers a touchscreen device, with in injection coordinate space that spans [-1000, 1000]
    /// on both axes.
    /// # Returns
    /// A `input_device::InputDevice`, which can be used to send events to the
    /// `fuchsia.input.report.InputDevice` that has been registered with the
    /// `fuchsia.input.injection.InputDeviceRegistry` service.
    pub async fn add_touchscreen_device(
        &mut self,
        min_x: i64,
        max_x: i64,
        min_y: i64,
        max_y: i64,
    ) -> Result<InputDevice, Error> {
        self.add_device(DeviceDescriptor {
            touch: Some(TouchDescriptor {
                input: Some(TouchInputDescriptor {
                    contacts: Some(
                        std::iter::repeat(ContactInputDescriptor {
                            position_x: Some(Axis {
                                range: Range { min: min_x, max: max_x },
                                unit: Unit { type_: UnitType::Other, exponent: 0 },
                            }),
                            position_y: Some(Axis {
                                range: Range { min: min_y, max: max_y },
                                unit: Unit { type_: UnitType::Other, exponent: 0 },
                            }),
                            contact_width: Some(Axis {
                                range: Range { min: min_x, max: max_x },
                                unit: Unit { type_: UnitType::Other, exponent: 0 },
                            }),
                            contact_height: Some(Axis {
                                range: Range { min: min_y, max: max_y },
                                unit: Unit { type_: UnitType::Other, exponent: 0 },
                            }),
                            ..Default::default()
                        })
                        .take(
                            usize::try_from(TOUCH_MAX_CONTACTS)
                                .context("usize is impossibly small")?,
                        )
                        .collect(),
                    ),
                    max_contacts: Some(TOUCH_MAX_CONTACTS),
                    touch_type: Some(TouchType::Touchscreen),
                    buttons: Some(vec![]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
    }

    /// Registers a media buttons device.
    /// # Returns
    /// A `input_device::InputDevice`, which can be used to send events to the
    /// `fuchsia.input.report.InputDevice` that has been registered with the
    /// `fuchsia.input.injection.InputDeviceRegistry` service.
    pub async fn add_media_buttons_device(&mut self) -> Result<InputDevice, Error> {
        self.add_device(DeviceDescriptor {
            consumer_control: Some(ConsumerControlDescriptor {
                input: Some(ConsumerControlInputDescriptor {
                    buttons: Some(vec![
                        ConsumerControlButton::VolumeUp,
                        ConsumerControlButton::VolumeDown,
                        ConsumerControlButton::Pause,
                        ConsumerControlButton::FactoryReset,
                        ConsumerControlButton::MicMute,
                        ConsumerControlButton::Reboot,
                        ConsumerControlButton::CameraDisable,
                        ConsumerControlButton::Power,
                        ConsumerControlButton::Function,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
    }

    /// Registers a keyboard device.
    /// # Returns
    /// An `input_device::InputDevice`, which can be used to send events to the
    /// `fuchsia.input.report.InputDevice` that has been registered with the
    /// `fuchsia.input.injection.InputDeviceRegistry` service.
    pub async fn add_keyboard_device(&mut self) -> Result<InputDevice, Error> {
        // Generate a `Vec` of all known keys.
        // * Because there is no direct way to iterate over enum values, we iterate
        //   over the values corresponding to `Key::A` and `Key::MediaVolumeDecrement`.
        // * Some values in the range have no corresponding enum value. For example,
        //   the value 0x00070065 sits between `NonUsBackslash` (0x00070064), and
        //   `KeypadEquals` (0x00070067). Such primitives are removed by `filter_map()`.
        //
        // TODO(https://fxbug.dev/42059900): Extend to include all values of the Key enum.
        let all_keys: Vec<Key> = (Key::A.into_primitive()
            ..=Key::MediaVolumeDecrement.into_primitive())
            .filter_map(Key::from_primitive)
            .collect();
        self.add_device(DeviceDescriptor {
            // Required for DeviceDescriptor.
            device_information: Some(new_fake_device_info()),
            keyboard: Some(KeyboardDescriptor {
                input: Some(KeyboardInputDescriptor {
                    keys3: Some(all_keys),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
    }

    pub async fn add_mouse_device(&mut self) -> Result<InputDevice, Error> {
        self.add_device(DeviceDescriptor {
            // Required for DeviceDescriptor.
            device_information: Some(new_fake_device_info()),
            mouse: Some(MouseDescriptor {
                input: Some(MouseInputDescriptor {
                    movement_x: Some(Axis {
                        range: Range { min: -1000, max: 1000 },
                        unit: Unit { type_: UnitType::Other, exponent: 0 },
                    }),
                    movement_y: Some(Axis {
                        range: Range { min: -1000, max: 1000 },
                        unit: Unit { type_: UnitType::Other, exponent: 0 },
                    }),
                    // `scroll_v` and `scroll_h` are range of tick number on
                    // driver's report. [-100, 100] should be enough for
                    // testing.
                    scroll_v: Some(Axis {
                        range: Range { min: -100, max: 100 },
                        unit: Unit { type_: UnitType::Other, exponent: 0 },
                    }),
                    scroll_h: Some(Axis {
                        range: Range { min: -100, max: 100 },
                        unit: Unit { type_: UnitType::Other, exponent: 0 },
                    }),
                    // Match to the values of fuchsia.ui.test.input.MouseButton.
                    buttons: Some(
                        (MouseButton::First.into_primitive()..=MouseButton::Third.into_primitive())
                            .map(|b| {
                                b.try_into().expect("failed to convert mouse button to primitive")
                            })
                            .collect(),
                    ),
                    position_x: None,
                    position_y: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
    }

    /// Adds a device to the `InputDeviceRegistry` FIDL server connected to this
    /// `InputDeviceRegistry` struct.
    ///
    /// # Returns
    /// A `input_device::InputDevice`, which can be used to send events to the
    /// `fuchsia.input.report.InputDevice` that has been registered with the
    /// `fuchsia.input.injection.InputDeviceRegistry` service.
    async fn add_device(&self, descriptor: DeviceDescriptor) -> Result<InputDevice, Error> {
        let (client_end, request_stream) = endpoints::create_request_stream::<InputDeviceMarker>();
        let mut device: InputDevice = InputDevice::new(
            request_stream,
            descriptor.clone(),
            self.got_input_reports_reader.clone(),
        );

        let res = self.proxy.register_and_get_device_info(client_end).await?;
        let device_id = res.device_id.expect("missing device_id");
        device.device_id = device_id;

        let mut starnix_attached = false;
        if let Some(ref starnix_proxy) = self.starnix_proxy {
            starnix_attached = device
                .attach_starnix(
                    starnix_proxy,
                    descriptor.clone(),
                    self.starnix_registration_timeout,
                )
                .await;
        }

        if !starnix_attached {
            if let Some(ref connector) = self.starnix_connector {
                device
                    .start_starnix_retry_task(connector.clone(), self.starnix_registration_timeout);
            }
        }

        Ok(device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_input_injection::{
        InputDeviceRegistryMarker, InputDeviceRegistryRegisterAndGetDeviceInfoResponse,
        InputDeviceRegistryRequest,
    };
    use fidl_fuchsia_input_report::InputReportsReaderV2Marker;
    use futures::{FutureExt as _, StreamExt};
    use std::sync::{Arc, Mutex};
    use test_case::test_case;

    const MAX_UNACKNOWLEDGED_REPORTS_LIMIT: u16 = 120;

    enum TestDeviceType {
        TouchScreen,
        MediaButtons,
        Keyboard,
        Mouse,
    }

    async fn add_device_for_test(
        registry: &mut InputDeviceRegistry,
        ty: TestDeviceType,
    ) -> Result<InputDevice, Error> {
        match ty {
            TestDeviceType::TouchScreen => registry.add_touchscreen_device(1, 1000, 1, 1000).await,
            TestDeviceType::MediaButtons => registry.add_media_buttons_device().await,
            TestDeviceType::Keyboard => registry.add_keyboard_device().await,
            TestDeviceType::Mouse => registry.add_mouse_device().await,
        }
    }

    #[test_case(TestDeviceType::TouchScreen =>
                matches Ok(DeviceDescriptor {
                    touch: Some(TouchDescriptor {
                        input: Some(TouchInputDescriptor { .. }),
                        ..
                    }),
                    .. });
                "touchscreen_device")]
    #[test_case(TestDeviceType::MediaButtons =>
                matches Ok(DeviceDescriptor {
                    consumer_control: Some(ConsumerControlDescriptor {
                        input: Some(ConsumerControlInputDescriptor { .. }),
                        ..
                    }),
                    .. });
                "media_buttons_device")]
    #[test_case(TestDeviceType::Keyboard =>
                matches Ok(DeviceDescriptor {
                    keyboard: Some(KeyboardDescriptor { .. }),
                    ..
                });
                "keyboard_device")]
    #[test_case(TestDeviceType::Mouse =>
                matches Ok(DeviceDescriptor {
                    mouse: Some(MouseDescriptor { .. }),
                    ..
                });
                "mouse_device")]
    #[fuchsia::test]
    async fn add_device_registers_correct_device_type(
        device_type: TestDeviceType,
    ) -> Result<DeviceDescriptor, Error> {
        let (registry_proxy, mut registry_request_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let mut input_device_registry =
            InputDeviceRegistry::new(registry_proxy, None, AsyncEvent::new());

        let add_device_fut = add_device_for_test(&mut input_device_registry, device_type);

        let input_device_proxy_fut = async {
            // `input_device_registry` should send a `Register` messgage to `registry_request_stream`.
            // Use `registry_request_stream` to grab the `ClientEnd` of the device added above,
            // and convert the `ClientEnd` into an `InputDeviceProxy`.
            //
            // Here only handle InputDeviceRegistryRequest once.
            let input_device_proxy = match registry_request_stream
                .next()
                .await
                .expect("stream read should yield Some")
                .expect("fidl read")
            {
                InputDeviceRegistryRequest::Register { .. } => {
                    unreachable!("InputDeviceRegistryRequest::Register should not be called");
                }
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo {
                    device, responder, ..
                } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(1),
                            ..Default::default()
                        })
                        .expect("RegisterAndGetDeviceInfo send response failed");

                    device
                }
            }
            .into_proxy();

            input_device_proxy
        };

        let (add_device_res, input_device_proxy) =
            futures::join!(add_device_fut, input_device_proxy_fut);

        let input_device = add_device_res.expect("add_device failed");
        assert_ne!(input_device.device_id, 0);

        let input_device_get_descriptor = input_device_proxy.get_descriptor().await;

        let input_device_server_fut = input_device.flush();

        // Avoid unrelated `panic()`: `InputDevice` requires clients to get an input
        // reports reader, to help debug integration test failures where no component
        // read events from the fake device.
        let (_input_reports_reader_proxy, input_reports_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        let _ = input_device_proxy
            .get_input_reports_reader_v2(
                input_reports_reader_server_end,
                MAX_UNACKNOWLEDGED_REPORTS_LIMIT,
            )
            .await;

        std::mem::drop(input_device_proxy); // Terminate stream served by `input_device_server_fut`.
        input_device_server_fut.await;

        input_device_get_descriptor.map_err(anyhow::Error::from)
    }

    #[fuchsia::test]
    async fn dual_registration_registers_with_both_fakes_and_fans_out_reports() -> Result<(), Error>
    {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let got_input_reports_reader = AsyncEvent::new();

        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(10),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    let device_proxy = device.into_proxy();
                    let descriptor =
                        device_proxy.get_descriptor().await.expect("starnix get_descriptor");
                    assert!(descriptor.keyboard.is_some());
                    let feature_report = device_proxy
                        .get_feature_report()
                        .await
                        .expect("starnix get_feature_report");
                    assert!(feature_report.is_ok());
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(20),
                            ..Default::default()
                        })
                        .unwrap();
                    device_proxy
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, starnix_device_proxy) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let input_device = add_res.expect("add_device should succeed");
        // Bookkeeping device_id comes from scene_manager (10)
        assert_eq!(input_device.device_id, 10);

        // Both clients call get_input_reports_reader_v2
        let (sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");

        let (starnix_reader_proxy, starnix_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_server_end, 10)
            .await
            .expect("starnix get reader");

        let keyboard_report = fidl_fuchsia_input_report::KeyboardInputReport {
            pressed_keys3: Some(vec![Key::A]),
            ..Default::default()
        };

        // Send an input report
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(12345),
                keyboard: Some(keyboard_report.clone()),
                ..Default::default()
            })
            .expect("send report");

        // Verify report delivered to scene_manager reader
        let mut sm_event_stream = sm_reader_proxy.take_event_stream();
        let sm_event = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(12345));
                assert_eq!(reports[0].keyboard, Some(keyboard_report.clone()));
            }
            _ => panic!("unexpected event"),
        }

        // Verify report delivered to Starnix reader
        let mut starnix_event_stream = starnix_reader_proxy.take_event_stream();
        let starnix_event = starnix_event_stream.next().await.unwrap().unwrap();
        match starnix_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(12345));
                assert_eq!(reports[0].keyboard, Some(keyboard_report));
            }
            _ => panic!("unexpected event"),
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn starnix_registry_failure_does_not_break_scene_manager() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, starnix_server_end) =
            endpoints::create_endpoints::<InputDeviceRegistryMarker>();
        // Drop the starnix server end immediately so the Starnix channel is closed
        // (e.g. UNAVAILABLE epitaph / PEER_CLOSED).
        std::mem::drop(starnix_server_end);

        let got_input_reports_reader = AsyncEvent::new();
        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy.into_proxy()),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(42),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy) = futures::join!(add_device_fut, scene_manager_fake);
        let input_device = add_res.expect("add_device should succeed despite Starnix failure");
        assert_eq!(input_device.device_id, 42);

        // Verify scene_manager reader still works
        let (sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");

        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(999),
                ..Default::default()
            })
            .expect("send report");

        let mut sm_event_stream = sm_reader_proxy.take_event_stream();
        let sm_event = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(999));
            }
            _ => panic!("unexpected event"),
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn starnix_registry_timeout_does_not_break_scene_manager() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();

        let got_input_reports_reader = AsyncEvent::new();
        let mut input_device_registry = InputDeviceRegistry::new_with_starnix_timeout(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
            zx::MonotonicDuration::from_millis(50),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(42),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { responder, .. } => {
                    // Intentionally do not reply to simulate an unresponsive or unready Starnix kernel.
                    // Keep responder alive so the channel does not close with PEER_CLOSED.
                    responder
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, _starnix_responder) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let input_device =
            add_res.expect("add_device should succeed despite Starnix registration timeout");
        assert_eq!(input_device.device_id, 42);

        // Verify scene_manager reader still works
        let (sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");

        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(999),
                ..Default::default()
            })
            .expect("send report");

        let mut sm_event_stream = sm_reader_proxy.take_event_stream();
        let sm_event = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(999));
            }
            _ => panic!("unexpected event"),
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn got_input_reports_reader_signaled_by_scene_manager_only() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let got_input_reports_reader = AsyncEvent::new();

        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(1),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    let device_proxy = device.into_proxy();
                    let descriptor =
                        device_proxy.get_descriptor().await.expect("starnix get_descriptor");
                    assert!(descriptor.keyboard.is_some());
                    let feature_report = device_proxy
                        .get_feature_report()
                        .await
                        .expect("starnix get_feature_report");
                    assert!(feature_report.is_ok());
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(2),
                            ..Default::default()
                        })
                        .unwrap();
                    device_proxy
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, starnix_device_proxy) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let _input_device = add_res.expect("add_device succeeded");

        // Starnix connects reader first
        let (_starnix_reader_proxy, starnix_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_server_end, 10)
            .await
            .expect("starnix get reader");

        // got_input_reports_reader must NOT be signaled by Starnix!
        assert!(got_input_reports_reader.wait().now_or_never().is_none());

        // Now scene_manager connects reader
        let (_sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");

        // got_input_reports_reader MUST now be signaled by scene_manager!
        assert!(got_input_reports_reader.wait().now_or_never().is_some());

        Ok(())
    }

    #[fuchsia::test]
    async fn starnix_reader_lifecycle_drops_when_inactive_and_supports_replacement()
    -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let got_input_reports_reader = AsyncEvent::new();

        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(10),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    let device_proxy = device.into_proxy();
                    let descriptor =
                        device_proxy.get_descriptor().await.expect("starnix get_descriptor");
                    assert!(descriptor.keyboard.is_some());
                    let feature_report = device_proxy
                        .get_feature_report()
                        .await
                        .expect("starnix get_feature_report");
                    assert!(feature_report.is_ok());
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(20),
                            ..Default::default()
                        })
                        .unwrap();
                    device_proxy
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, starnix_device_proxy) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let input_device = add_res.expect("add_device should succeed");

        // Open scene_manager reader
        let (sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");
        let mut sm_event_stream = sm_reader_proxy.take_event_stream();

        // 1. Send report X before opening a Starnix reader; check scene_manager gets it and
        // Starnix buffers it.
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(100),
                ..Default::default()
            })
            .expect("send report X");

        let sm_event_x = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event_x {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(100));
            }
            _ => panic!("unexpected event"),
        }

        // 2. Open Starnix reader #1. The buffered report X should be delivered first.
        let (starnix_reader_1_proxy, starnix_reader_1_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_1_server_end, 10)
            .await
            .expect("starnix get reader 1");

        let mut starnix_1_event_stream = starnix_reader_1_proxy.take_event_stream();
        let starnix_1_event_x = starnix_1_event_stream.next().await.unwrap().unwrap();
        match starnix_1_event_x {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                last_report_stamp,
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(100));
                starnix_reader_1_proxy
                    .acknowledge_reports(last_report_stamp)
                    .expect("ack report X");
            }
            _ => panic!("unexpected event"),
        }

        // Send report A while reader #1 is active; check reader #1 gets A.
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(200),
                ..Default::default()
            })
            .expect("send report A");

        let starnix_1_event_a = starnix_1_event_stream.next().await.unwrap().unwrap();
        match starnix_1_event_a {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                last_report_stamp,
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(200));
                starnix_reader_1_proxy
                    .acknowledge_reports(last_report_stamp)
                    .expect("ack report A");
            }
            _ => panic!("unexpected event"),
        }

        // 3. Drop reader #1 and check the Starnix InputDevice proxy still answers get_descriptor().
        std::mem::drop(starnix_1_event_stream);
        std::mem::drop(starnix_reader_1_proxy);

        let descriptor = starnix_device_proxy
            .get_descriptor()
            .await
            .expect("starnix get_descriptor after reader drop");
        assert!(descriptor.keyboard.is_some());

        // Send report Y while reader #1 is dropped (inactive); this report should NOT be buffered.
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(250),
                ..Default::default()
            })
            .expect("send report Y");

        // 4. Open reader #2, send report B, check reader #2's first event contains exactly B.
        let (starnix_reader_2_proxy, starnix_reader_2_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_2_server_end, 10)
            .await
            .expect("starnix get reader 2");

        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(300),
                ..Default::default()
            })
            .expect("send report B");

        let mut starnix_2_event_stream = starnix_reader_2_proxy.take_event_stream();
        let starnix_2_event = starnix_2_event_stream.next().await.unwrap().unwrap();
        match starnix_2_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(300));
            }
            _ => panic!("unexpected event"),
        }

        // 5. Check scene_manager reader got X, A, Y, and B.
        let sm_event_a = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event_a {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(200));
            }
            _ => panic!("unexpected event"),
        }

        let sm_event_y = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event_y {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(250));
            }
            _ => panic!("unexpected event"),
        }

        let sm_event_b = sm_event_stream.next().await.unwrap().unwrap();
        match sm_event_b {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].event_time, Some(300));
            }
            _ => panic!("unexpected event"),
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn primary_reader_closure_and_replacement_with_active_starnix() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let got_input_reports_reader = AsyncEvent::new();

        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(10),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    let device_proxy = device.into_proxy();
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(20),
                            ..Default::default()
                        })
                        .unwrap();
                    device_proxy
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, starnix_device_proxy) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let input_device = add_res.expect("add_device should succeed");

        // 1. Open primary (scene_manager) reader #1 and Starnix reader
        let (sm_reader_1_proxy, sm_reader_1_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_1_server_end, 10)
            .await
            .expect("sm get reader 1");
        let mut sm_1_event_stream = sm_reader_1_proxy.take_event_stream();

        let (starnix_reader_proxy, starnix_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_server_end, 10)
            .await
            .expect("starnix get reader");
        let mut starnix_event_stream = starnix_reader_proxy.take_event_stream();

        // 2. Send report 1 -> both readers receive it
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(1),
                ..Default::default()
            })
            .expect("send report 1");

        let sm_1_event = sm_1_event_stream.next().await.unwrap().unwrap();
        match sm_1_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                last_report_stamp,
            } => {
                assert_eq!(reports[0].event_time, Some(1));
                sm_reader_1_proxy.acknowledge_reports(last_report_stamp).unwrap();
            }
            _ => panic!("unexpected event"),
        }

        let starnix_event_1 = starnix_event_stream.next().await.unwrap().unwrap();
        match starnix_event_1 {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                last_report_stamp,
            } => {
                assert_eq!(reports[0].event_time, Some(1));
                starnix_reader_proxy.acknowledge_reports(last_report_stamp).unwrap();
            }
            _ => panic!("unexpected event"),
        }

        // 3. Drop primary reader #1 (simulate focus switch to Starnix ClientInputOwned)
        std::mem::drop(sm_1_event_stream);
        std::mem::drop(sm_reader_1_proxy);

        // 4. Send report 2 -> primary reader is closed, but Starnix is active!
        // send_input_report MUST succeed without panicking!
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(2),
                ..Default::default()
            })
            .expect("send report 2 should succeed because Starnix is active");

        let starnix_event_2 = starnix_event_stream.next().await.unwrap().unwrap();
        match starnix_event_2 {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports[0].event_time, Some(2));
            }
            _ => panic!("unexpected event"),
        }

        // 5. Primary connects reader #2 (replacement)
        let (sm_reader_2_proxy, sm_reader_2_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_2_server_end, 10)
            .await
            .expect("sm get reader 2");
        let mut sm_2_event_stream = sm_reader_2_proxy.take_event_stream();

        // 6. Send report 3 -> both primary reader #2 and Starnix receive it
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(3),
                ..Default::default()
            })
            .expect("send report 3");

        let sm_2_event = sm_2_event_stream.next().await.unwrap().unwrap();
        match sm_2_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports[0].event_time, Some(3));
            }
            _ => panic!("unexpected event"),
        }

        let starnix_event_3 = starnix_event_stream.next().await.unwrap().unwrap();
        match starnix_event_3 {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports[0].event_time, Some(3));
            }
            _ => panic!("unexpected event"),
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn starnix_lazy_retry_attaches_when_initially_unavailable() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, starnix_server_end) =
            endpoints::create_endpoints::<InputDeviceRegistryMarker>();
        // Drop the starnix server end immediately so initial registration fails
        // (simulating Starnix runner unavailable / not running).
        std::mem::drop(starnix_server_end);

        let retry_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry_calls_clone = retry_calls.clone();

        let (ready_starnix_proxy, mut ready_starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let ready_proxy_holder = Arc::new(Mutex::new(Some(ready_starnix_proxy)));

        let connector = crate::input_device::StarnixConnector::Test(Arc::new(move || {
            retry_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(proxy) = ready_proxy_holder.lock().unwrap().take() {
                Ok(proxy)
            } else {
                Err(anyhow::format_err!("already connected"))
            }
        }));

        let got_input_reports_reader = AsyncEvent::new();
        let mut input_device_registry = InputDeviceRegistry::new_with_starnix_connector_and_timeout(
            scene_manager_proxy,
            Some(starnix_proxy.into_proxy()),
            Some(connector),
            got_input_reports_reader.clone(),
            zx::MonotonicDuration::from_millis(50),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(10),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, _sm_device_proxy) = futures::join!(add_device_fut, scene_manager_fake);
        let input_device = add_res.expect("add_device should succeed");

        // Starnix server fake that handles retry registration
        let starnix_server_fake = async {
            let req = ready_starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(99),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        // Calling send_input_report wakes up the retry loop via wake_retry
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(555),
                ..Default::default()
            })
            .expect("send report");

        let starnix_device_proxy = starnix_server_fake.await;

        // Open Starnix reader on the newly attached device
        let (starnix_reader_proxy, starnix_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_server_end, 10)
            .await
            .expect("starnix get reader");

        // Send another report now that Starnix is active
        input_device
            .send_input_report(fidl_fuchsia_input_report::InputReport {
                event_time: Some(777),
                ..Default::default()
            })
            .expect("send report 777");

        let mut starnix_event_stream = starnix_reader_proxy.take_event_stream();
        let starnix_event = starnix_event_stream.next().await.unwrap().unwrap();
        match starnix_event {
            fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                reports,
                ..
            } => {
                assert_eq!(reports[0].event_time, Some(777));
            }
            _ => panic!("unexpected event"),
        }

        assert!(retry_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        Ok(())
    }

    #[fuchsia::test]
    async fn starnix_initial_reports_are_buffered_until_reader_connects() -> Result<(), Error> {
        let (scene_manager_proxy, mut scene_manager_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let (starnix_proxy, mut starnix_stream) =
            endpoints::create_proxy_and_stream::<InputDeviceRegistryMarker>();
        let got_input_reports_reader = AsyncEvent::new();

        let mut input_device_registry = InputDeviceRegistry::new(
            scene_manager_proxy,
            Some(starnix_proxy),
            got_input_reports_reader.clone(),
        );

        let add_device_fut = input_device_registry.add_keyboard_device();

        let scene_manager_fake = async {
            let req = scene_manager_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(10),
                            ..Default::default()
                        })
                        .unwrap();
                    device.into_proxy()
                }
                _ => panic!("unexpected request"),
            }
        };

        let starnix_fake = async {
            let req = starnix_stream.next().await.unwrap().unwrap();
            match req {
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    let device_proxy = device.into_proxy();
                    responder
                        .send(InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                            device_id: Some(20),
                            ..Default::default()
                        })
                        .unwrap();
                    device_proxy
                }
                _ => panic!("unexpected request"),
            }
        };

        let (add_res, sm_device_proxy, starnix_device_proxy) =
            futures::join!(add_device_fut, scene_manager_fake, starnix_fake);
        let input_device = add_res.expect("add_device should succeed");

        // Primary (scene_manager) connects reader
        let (sm_reader_proxy, sm_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        sm_device_proxy
            .get_input_reports_reader_v2(sm_reader_server_end, 10)
            .await
            .expect("sm get reader");
        let mut sm_event_stream = sm_reader_proxy.take_event_stream();

        // Starnix has registered (device_id=20) but has NOT yet called GetInputReportsReaderV2.
        // Send 3 input reports immediately.
        for stamp in [101, 102, 103] {
            input_device
                .send_input_report(fidl_fuchsia_input_report::InputReport {
                    event_time: Some(stamp),
                    ..Default::default()
                })
                .expect("send report");
        }

        // Verify scene_manager received all 3 reports
        let mut sm_received_stamps = Vec::new();
        while sm_received_stamps.len() < 3 {
            let sm_event = sm_event_stream.next().await.unwrap().unwrap();
            match sm_event {
                fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                    reports,
                    last_report_stamp,
                } => {
                    for r in reports {
                        sm_received_stamps.push(r.event_time.unwrap());
                    }
                    sm_reader_proxy.acknowledge_reports(last_report_stamp).unwrap();
                }
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(sm_received_stamps, vec![101, 102, 103]);

        // Now Starnix calls GetInputReportsReaderV2.
        let (starnix_reader_proxy, starnix_reader_server_end) =
            endpoints::create_proxy::<InputReportsReaderV2Marker>();
        starnix_device_proxy
            .get_input_reports_reader_v2(starnix_reader_server_end, 10)
            .await
            .expect("starnix get reader");

        // Starnix reader MUST receive all 3 buffered reports in exact order!
        let mut starnix_event_stream = starnix_reader_proxy.take_event_stream();
        let mut received_stamps = Vec::new();
        while received_stamps.len() < 3 {
            let event = starnix_event_stream.next().await.unwrap().unwrap();
            match event {
                fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                    reports,
                    last_report_stamp,
                } => {
                    for r in reports {
                        received_stamps.push(r.event_time.unwrap());
                    }
                    starnix_reader_proxy.acknowledge_reports(last_report_stamp).unwrap();
                }
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(received_stamps, vec![101, 102, 103]);

        Ok(())
    }
}
