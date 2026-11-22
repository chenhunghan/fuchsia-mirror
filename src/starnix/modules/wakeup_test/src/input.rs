// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_input::ConsumerControlButton;
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_ui_test_input as futinput;
use fuchsia_async::{MonotonicInstant, Timer};
use starnix_logging::{log_error, log_info};

const DEFAULT_TOUCH_CONTACT_ID: u32 = 1;

pub(crate) async fn create_media_buttons_proxy(
    registry: &futinput::RegistryProxy,
) -> Result<futinput::MediaButtonsDeviceProxy> {
    let (key_client, key_server) = create_proxy::<futinput::MediaButtonsDeviceMarker>();

    registry
        .register_media_buttons_device(futinput::RegistryRegisterMediaButtonsDeviceRequest {
            device: Some(key_server),
            ..Default::default()
        })
        .await
        .map_err(|err| anyhow!("Could not register MediaButtonsDevice to Registry: {err}"))?;

    Ok(key_client)
}

pub(crate) async fn schedule_wakeup_power_button(
    media_button_proxy: &futinput::MediaButtonsDeviceProxy,
    delay: zx::BootDuration,
) -> Result<()> {
    log_info!("WakeupTestDevice::wakeup_schedule_power_button called with time: {delay:?}");

    media_button_proxy
        .schedule_simulate_button_press(
            &futinput::MediaButtonsDeviceScheduleSimulateButtonPressRequest {
                button: Some(ConsumerControlButton::Power),
                delay: Some(delay.into_nanos()),
                ..Default::default()
            },
        )
        .await
        .map_err(|err| {
            log_error!("failed to schedule power press event: {err:?}");
            anyhow!("failed to schedule power press event: {err}")
        })?;

    log_info!("successfully scheduled power press event");
    Ok(())
}

pub(crate) async fn create_touchscreen_proxy(
    registry: &futinput::RegistryProxy,
    max_x: i64,
    max_y: i64,
) -> Result<futinput::TouchScreenProxy> {
    if max_x <= 0 || max_y <= 0 {
        return Err(anyhow!("Invalid touchscreen dimensions: max_x={max_x}, max_y={max_y}"));
    }

    let (touch_client, touch_server) = create_proxy::<futinput::TouchScreenMarker>();

    registry
        .register_touch_screen(futinput::RegistryRegisterTouchScreenRequest {
            device: Some(touch_server),
            coordinate_unit: Some(futinput::CoordinateUnit::RegisteredDimensions),
            display_dimensions: Some(futinput::DisplayDimensions {
                min_x: 0,
                min_y: 0,
                max_x,
                max_y,
            }),
            ..Default::default()
        })
        .await
        .map_err(|err| anyhow!("Could not register TouchScreen device to Registry: {err}"))?;

    Ok(touch_client)
}

pub(crate) async fn simulate_tap(
    proxy: &futinput::TouchScreenProxy,
    x: i64,
    y: i64,
    hold_duration: zx::MonotonicDuration,
) -> Result<()> {
    let down_report = futinput::TouchInputReport {
        contacts: Some(vec![futinput::ContactInputReport {
            contact_id: Some(DEFAULT_TOUCH_CONTACT_ID),
            position_x: Some(x),
            position_y: Some(y),
            ..Default::default()
        }]),
        ..Default::default()
    };
    proxy
        .simulate_touch_event(&down_report)
        .await
        .map_err(|err| anyhow!("Failed to simulate touch down event: {err}"))?;

    Timer::new(MonotonicInstant::after(hold_duration)).await;

    let up_report = futinput::TouchInputReport { contacts: Some(vec![]), ..Default::default() };
    proxy
        .simulate_touch_event(&up_report)
        .await
        .map_err(|err| anyhow!("Failed to simulate touch up event: {err}"))?;

    Ok(())
}

pub(crate) async fn simulate_swipe(
    proxy: &futinput::TouchScreenProxy,
    start: fmath::Vec_,
    end: fmath::Vec_,
    duration_ns: i64,
    move_event_count: u32,
) -> Result<()> {
    proxy
        .simulate_swipe(&futinput::TouchScreenSimulateSwipeRequest {
            start_location: Some(start),
            end_location: Some(end),
            move_event_count: Some(move_event_count),
            duration: Some(duration_ns),
            ..Default::default()
        })
        .await
        .map_err(|err| anyhow!("Failed to simulate swipe event: {err}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TOUCH_CONTACT_ID, create_media_buttons_proxy, create_touchscreen_proxy,
        schedule_wakeup_power_button, simulate_swipe, simulate_tap,
    };
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_input::ConsumerControlButton;
    use fidl_fuchsia_math as fmath;
    use fidl_fuchsia_ui_test_input as futinput;
    use futures::StreamExt;

    #[fuchsia::test]
    async fn create_proxies_with_registry_sends_expected_registration() {
        let (registry_proxy, mut registry_stream) =
            create_proxy_and_stream::<futinput::RegistryMarker>();

        let server_fut = async move {
            let touch_register =
                registry_stream.next().await.expect("touch register").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                touch_register
            else {
                panic!("expected RegisterTouchScreen, got {touch_register:?}");
            };
            assert!(payload.device.is_some());
            assert_eq!(
                payload.coordinate_unit,
                Some(futinput::CoordinateUnit::RegisteredDimensions)
            );
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1080, max_y: 2400 })
            );
            responder.send().expect("send touch register response");

            let buttons_register =
                registry_stream.next().await.expect("buttons register").expect("fidl ok");
            let futinput::RegistryRequest::RegisterMediaButtonsDevice { payload, responder } =
                buttons_register
            else {
                panic!("expected RegisterMediaButtonsDevice, got {buttons_register:?}");
            };
            assert!(payload.device.is_some());
            responder.send().expect("send media buttons register response");
        };

        let client_fut = async move {
            assert!(create_touchscreen_proxy(&registry_proxy, 0, 1000).await.is_err());
            let _touch_proxy = create_touchscreen_proxy(&registry_proxy, 1080, 2400)
                .await
                .expect("touchscreen registration succeeds");
            let _buttons_proxy = create_media_buttons_proxy(&registry_proxy)
                .await
                .expect("media buttons registration succeeds");
        };

        futures::join!(server_fut, client_fut);
    }

    #[fuchsia::test]
    async fn simulate_tap_sends_down_and_up_reports() {
        let (proxy, mut stream) = create_proxy_and_stream::<futinput::TouchScreenMarker>();

        let server_fut = async move {
            let first = stream.next().await.expect("down request").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateTouchEvent { report: down_report, responder } =
                first
            else {
                panic!("expected SimulateTouchEvent for down, got {first:?}");
            };
            responder.send().expect("send down response");
            let contacts = down_report.contacts.expect("contacts present");
            assert_eq!(contacts.len(), 1);
            assert_eq!(contacts[0].contact_id, Some(DEFAULT_TOUCH_CONTACT_ID));
            assert_eq!(contacts[0].position_x, Some(320));
            assert_eq!(contacts[0].position_y, Some(640));

            let second = stream.next().await.expect("up request").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateTouchEvent { report: up_report, responder } =
                second
            else {
                panic!("expected SimulateTouchEvent for up, got {second:?}");
            };
            responder.send().expect("send up response");
            let up_contacts = up_report.contacts.expect("up contacts present");
            assert!(up_contacts.is_empty());
        };

        let client_fut = async move {
            simulate_tap(&proxy, 320, 640, zx::MonotonicDuration::from_millis(1))
                .await
                .expect("simulate_tap succeeds");
        };

        futures::join!(server_fut, client_fut);
    }

    #[fuchsia::test]
    async fn simulate_swipe_sends_expected_request() {
        let (proxy, mut stream) = create_proxy_and_stream::<futinput::TouchScreenMarker>();
        let start = fmath::Vec_ { x: 540, y: 1920 };
        let end = fmath::Vec_ { x: 540, y: 600 };

        let server_fut = async move {
            let request = stream.next().await.expect("swipe request").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateSwipe { payload, responder } = request else {
                panic!("expected SimulateSwipe, got {request:?}");
            };
            responder.send().expect("send swipe response");
            assert_eq!(payload.start_location, Some(start));
            assert_eq!(payload.end_location, Some(end));
            assert_eq!(payload.duration, Some(80_000_000));
            assert_eq!(payload.move_event_count, Some(10));
        };

        let client_fut = async move {
            simulate_swipe(&proxy, start, end, 80_000_000, 10)
                .await
                .expect("simulate_swipe succeeds");
        };

        futures::join!(server_fut, client_fut);
    }

    #[fuchsia::test]
    async fn schedule_power_button_sends_delay_and_button() {
        let (proxy, mut stream) = create_proxy_and_stream::<futinput::MediaButtonsDeviceMarker>();

        let server_fut = async move {
            let request = stream.next().await.expect("button request").expect("fidl ok");
            let futinput::MediaButtonsDeviceRequest::ScheduleSimulateButtonPress {
                payload,
                responder,
            } = request
            else {
                panic!("expected ScheduleSimulateButtonPress, got {request:?}");
            };
            responder.send().expect("send button response");
            assert_eq!(payload.button, Some(ConsumerControlButton::Power));
            assert_eq!(payload.delay, Some(2_500_000_000));
        };

        let client_fut = async move {
            schedule_wakeup_power_button(&proxy, zx::BootDuration::from_millis(2500))
                .await
                .expect("schedule_wakeup_power_button succeeds");
        };

        futures::join!(server_fut, client_fut);
    }
}
