// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_fuchsia_input_report as fidl_input_report;
use fuchsia_async::{MonotonicDuration, MonotonicInstant, Timer};
use fuchsia_sync::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

const WAIT_TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(5);
const WAIT_POLL_INTERVAL: MonotonicDuration = MonotonicDuration::from_millis(5);

/// A fake implementation of [`fidl_input_report::InputDevice`] that supports
/// serving multiple concurrent InputReportsReaderV2 clients.
#[derive(Clone)]
pub struct FakeInputDevice {
    state: Arc<Mutex<FakeInputDeviceState>>,
}

#[derive(Clone)]
struct ReaderClient {
    server: fidl_next::Server<fidl_input_report::InputReportsReaderV2>,
    max_unacknowledged_reports: u64,
    last_sent_report_stamp: u64,
    /// Must be at most `last_sent_report_stamp`.
    last_acknowledged_report_stamp: u64,
}

struct FakeInputDeviceState {
    descriptor: fidl_input_report::DeviceDescriptor,
    readers: HashMap<u64, ReaderClient>,
    next_reader_id: u64,
    scope: fuchsia_async::Scope,
}

fn clone_report(r: &fidl_input_report::InputReport) -> fidl_input_report::InputReport {
    assert!(r.wake_lease.is_none(), "FakeInputDevice does not support wake_lease in InputReport",);

    // InputReport does not implement Clone due to the potential zx::EventPair handle,
    // so we clone each report field by field.
    fidl_input_report::InputReport {
        event_time: r.event_time,
        mouse: r.mouse.clone(),
        trace_id: r.trace_id,
        sensor: r.sensor.clone(),
        touch: r.touch.clone(),
        keyboard: r.keyboard.clone(),
        consumer_control: r.consumer_control.clone(),
        report_id: r.report_id,

        // wake_lease is not implemented in the FakeInputDevice, so it must be
        // None.
        wake_lease: None,
    }
}

impl FakeInputDevice {
    /// Creates a new [`FakeInputDevice`] with the given [`fidl_input_report::DeviceDescriptor`].
    pub fn new(descriptor: fidl_input_report::DeviceDescriptor) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeInputDeviceState {
                descriptor,
                readers: HashMap::new(),
                next_reader_id: 1,
                scope: fuchsia_async::Scope::new(),
            })),
        }
    }

    /// Serves this [`FakeInputDevice`] on the given [`fuchsia_async::Scope`].
    pub fn serve_on_scope(
        &self,
        scope: &fuchsia_async::Scope,
        server_end: fidl_next::ServerEnd<fidl_input_report::InputDevice>,
    ) {
        let handler = FakeInputDeviceHandler { state: Arc::clone(&self.state) };
        scope.spawn(async move {
            let dispatcher =
                fidl_next::ServerDispatcher::<fidl_input_report::InputDevice>::new(server_end);
            if let Err(err) = dispatcher.run(handler).await {
                match err {
                    fidl_next::ProtocolError::PeerClosed | fidl_next::ProtocolError::Stopped => {}
                    _ => log::error!("FakeInputDevice server dispatcher failed: {err:?}"),
                }
            }
        });
    }

    /// Serves this [`FakeInputDevice`] as a [`vfs::service::Service`] node.
    pub fn serve(&self) -> Arc<vfs::service::Service> {
        let device = self.clone();
        vfs::service::endpoint(move |vfs_scope, channel| {
            let server_end = fidl_next::ServerEnd::<fidl_input_report::InputDevice>::from_untyped(
                channel.into_zx_channel(),
            );
            let scope = fuchsia_async::Scope::new();
            device.serve_on_scope(&scope, server_end);
            vfs_scope.spawn(async move {
                scope.await;
            });
        })
    }

    /// Sends a single input report to all connected [`fidl_input_report::InputReportsReaderV2`]
    /// clients.
    ///
    /// Panics if any active client has unacknowledged reports that have reached its
    /// `max_unacknowledged_reports` limit.
    pub async fn send_input_report(&self, report: fidl_input_report::InputReport) {
        self.send_input_reports(std::slice::from_ref(&report)).await;
    }

    /// Sends input reports to all connected [`fidl_input_report::InputReportsReaderV2`] clients.
    ///
    /// `reports` must not be empty.
    ///
    /// Panics if `reports` is empty, or if any active client has
    /// unacknowledged reports that have reached its `max_unacknowledged_reports` limit.
    pub async fn send_input_reports(&self, reports: &[fidl_input_report::InputReport]) {
        assert!(!reports.is_empty(), "reports must not be empty");

        let readers_to_send: Vec<(
            fidl_next::Server<fidl_input_report::InputReportsReaderV2>,
            u64,
        )> = {
            let mut state = self.state.lock();
            let count = reports.len() as u64;
            let mut to_send = Vec::with_capacity(state.readers.len());
            for reader in state.readers.values_mut() {
                let last_sent = reader.last_sent_report_stamp;
                let last_acked = reader.last_acknowledged_report_stamp;
                assert!(
                    last_sent >= last_acked,
                    "Last acknowledged stamp {last_acked} exceeds last sent stamp {last_sent}",
                );
                let unacknowledged = last_sent - last_acked;
                assert!(
                    unacknowledged < reader.max_unacknowledged_reports,
                    "Client has {unacknowledged} unacknowledged reports (last sent: {last_sent}, \
                     last acked: {last_acked}) exceeding maximum limit of {}",
                    reader.max_unacknowledged_reports,
                );
                reader.last_sent_report_stamp += count;
                to_send.push((reader.server.clone(), reader.last_sent_report_stamp));
            }
            to_send
        };

        for (server, stamp) in readers_to_send {
            let reports_to_send: Vec<fidl_input_report::InputReport> =
                reports.iter().map(clone_report).collect();
            server.on_input_reports(reports_to_send, stamp).await.expect("on_input_reports failed");
        }
    }

    /// Returns the number of currently connected reader clients.
    pub fn num_readers(&self) -> usize {
        self.state.lock().readers.len()
    }

    /// Waits until at least `count` reader clients have connected.
    ///
    /// # Panics
    ///
    /// Panics if fewer than `count` reader clients connect within [`WAIT_TIMEOUT`].
    pub async fn wait_for_readers(&self, count: usize) {
        let deadline = MonotonicInstant::after(WAIT_TIMEOUT);
        while self.state.lock().readers.len() < count {
            if MonotonicInstant::now() >= deadline {
                panic!(
                    "Timed out waiting for {count} readers; currently have {}",
                    self.state.lock().readers.len()
                );
            }
            Timer::new(MonotonicInstant::after(WAIT_POLL_INTERVAL)).await;
        }
    }
}

struct FakeInputDeviceHandler {
    state: Arc<Mutex<FakeInputDeviceState>>,
}

impl fidl_input_report::InputDeviceServerHandler for FakeInputDeviceHandler {
    async fn get_descriptor(
        &mut self,
        responder: fidl_next::Responder<fidl_input_report::input_device::GetDescriptor>,
    ) {
        let descriptor = self.state.lock().descriptor.clone();
        if let Err(err) = responder.respond(&descriptor).await {
            log::error!("GetDescriptor responder failed: {err:?}");
        }
    }

    async fn get_input_reports_reader(
        &mut self,
        #[expect(unused)] request: fidl_next::Request<
            fidl_input_report::input_device::GetInputReportsReader,
        >,
    ) {
        unimplemented!("GetInputReportsReader (v1) is not supported; use GetInputReportsReaderV2");
    }

    async fn get_input_reports_reader_v2(
        &mut self,
        request: fidl_next::Request<fidl_input_report::input_device::GetInputReportsReaderV2>,
        responder: fidl_next::Responder<fidl_input_report::input_device::GetInputReportsReaderV2>,
    ) {
        let payload = request.payload();
        // The fake device doesn't have any internal unacknowledged reports limit, so it returns the
        // limit from the client.
        let max_unacknowledged = payload.max_unacknowledged_reports_limit;
        let reader_dispatcher = fidl_next::ServerDispatcher::<
            fidl_input_report::InputReportsReaderV2,
        >::new(payload.reader);

        let reader_client = ReaderClient {
            server: reader_dispatcher.server(),
            max_unacknowledged_reports: max_unacknowledged as u64,
            last_sent_report_stamp: 0,
            last_acknowledged_report_stamp: 0,
        };
        let (reader_id, scope) = {
            let mut state = self.state.lock();
            let id = state.next_reader_id;
            state.next_reader_id += 1;
            state.readers.insert(id, reader_client);
            (id, state.scope.clone())
        };
        let state = Arc::clone(&self.state);
        scope.spawn(async move {
            let handler = FakeReaderHandler { state: Arc::clone(&state), reader_id };
            if let Err(err) = reader_dispatcher.run(handler).await {
                match err {
                    fidl_next::ProtocolError::PeerClosed | fidl_next::ProtocolError::Stopped => {}
                    _ => log::error!("InputReportsReaderV2 server dispatcher failed: {err:?}"),
                }
            }
            state.lock().readers.remove(&reader_id);
        });

        if let Err(err) = responder.respond(max_unacknowledged).await {
            log::error!("GetInputReportsReaderV2 responder failed: {err:?}");
        }
    }

    async fn send_output_report(
        &mut self,
        #[expect(unused)] request: fidl_next::Request<
            fidl_input_report::input_device::SendOutputReport,
        >,
        responder: fidl_next::Responder<fidl_input_report::input_device::SendOutputReport>,
    ) {
        if let Err(err) = responder.respond_err(zx::Status::NOT_SUPPORTED).await {
            log::error!("SendOutputReport responder failed: {err:?}");
        }
    }

    async fn get_feature_report(
        &mut self,
        responder: fidl_next::Responder<fidl_input_report::input_device::GetFeatureReport>,
    ) {
        if let Err(err) = responder.respond_err(zx::Status::NOT_SUPPORTED).await {
            log::error!("GetFeatureReport responder failed: {err:?}");
        }
    }

    async fn set_feature_report(
        &mut self,
        #[expect(unused)] request: fidl_next::Request<
            fidl_input_report::input_device::SetFeatureReport,
        >,
        responder: fidl_next::Responder<fidl_input_report::input_device::SetFeatureReport>,
    ) {
        if let Err(err) = responder.respond_err(zx::Status::NOT_SUPPORTED).await {
            log::error!("SetFeatureReport responder failed: {err:?}");
        }
    }

    async fn get_input_report(
        &mut self,
        #[expect(unused)] request: fidl_next::Request<
            fidl_input_report::input_device::GetInputReport,
        >,
        responder: fidl_next::Responder<fidl_input_report::input_device::GetInputReport>,
    ) {
        if let Err(err) = responder.respond_err(zx::Status::NOT_SUPPORTED).await {
            log::error!("GetInputReport responder failed: {err:?}");
        }
    }
}

struct FakeReaderHandler {
    state: Arc<Mutex<FakeInputDeviceState>>,
    reader_id: u64,
}

impl fidl_input_report::InputReportsReaderV2ServerHandler for FakeReaderHandler {
    async fn acknowledge_reports(
        &mut self,
        request: fidl_next::Request<fidl_input_report::input_reports_reader_v2::AcknowledgeReports>,
    ) {
        let acked_stamp = request.payload().last_acknowledged_report_stamp;
        let mut state = self.state.lock();
        let reader = state.readers.get_mut(&self.reader_id).expect("reader not found");
        let last_sent = reader.last_sent_report_stamp;
        assert!(
            acked_stamp <= last_sent,
            "Client acknowledged non-existent report stamp {acked_stamp}; \
             highest sent stamp: {last_sent}",
        );
        let prev_acked = reader.last_acknowledged_report_stamp;
        assert!(
            acked_stamp >= prev_acked,
            "Client re-acknowledged previously acknowledged stamp {acked_stamp}; \
             stamps up to {prev_acked} were already acknowledged",
        );
        reader.last_acknowledged_report_stamp = acked_stamp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    #[derive(Clone, Copy, Debug, Default)]
    struct TestReaderOptions {
        pub auto_ack: bool,
    }

    struct TestReader {
        client: fidl_next::Client<fidl_input_report::InputReportsReaderV2>,
        reports: Arc<Mutex<Vec<fidl_input_report::InputReport>>>,
        join_handle: fidl_next::HandlerJoinHandle<zx::Channel, TestReaderHandler>,
    }

    impl TestReader {
        fn new(
            client_end: fidl_next::ClientEnd<fidl_input_report::InputReportsReaderV2>,
            options: TestReaderOptions,
        ) -> Self {
            let reports = Arc::new(Mutex::new(Vec::new()));
            let reports_clone = reports.clone();
            let (client, join_handle) = client_end.spawn_handler_full_with(|client| {
                TestReaderHandler { reports: reports_clone, client, auto_ack: options.auto_ack }
            });
            Self { client, reports, join_handle }
        }

        async fn wait_for_reports(&self, count: usize) -> Vec<fidl_input_report::InputReport> {
            let deadline = MonotonicInstant::after(MonotonicDuration::from_seconds(5));
            while self.reports.lock().len() < count {
                if MonotonicInstant::now() >= deadline {
                    panic!(
                        "Timed out waiting for {count} reports; received {}",
                        self.reports.lock().len()
                    );
                }
                fuchsia_async::Timer::new(MonotonicInstant::after(MonotonicDuration::from_millis(
                    5,
                )))
                .await;
            }
            self.reports.lock().iter().map(clone_report).collect()
        }

        async fn close(self) {
            self.client.close();
            let _ = self.join_handle.await;
        }
    }

    struct TestReaderHandler {
        reports: Arc<Mutex<Vec<fidl_input_report::InputReport>>>,
        client: fidl_next::Client<fidl_input_report::InputReportsReaderV2>,
        auto_ack: bool,
    }

    impl fidl_input_report::InputReportsReaderV2ClientHandler for TestReaderHandler {
        async fn on_input_reports(
            &mut self,
            request: fidl_next::Request<fidl_input_report::input_reports_reader_v2::OnInputReports>,
        ) {
            let payload = request.payload();
            self.reports.lock().extend(payload.reports);
            if self.auto_ack {
                self.client
                    .acknowledge_reports(payload.last_report_stamp)
                    .await
                    .expect("acknowledge_reports failed");
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_fake_input_device_descriptor() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor {
            device_information: Some(fidl_input_report::DeviceInformation {
                vendor_id: Some(0x1234),
                product_id: Some(0x5678),
                manufacturer_name: Some("TestManuf".to_string()),
                product_name: Some("TestProduct".to_string()),
                serial_number: Some("TestSerial".to_string()),
                ..Default::default()
            }),
            keyboard: Some(fidl_input_report::KeyboardDescriptor { ..Default::default() }),
            ..Default::default()
        });

        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let response = device_client.get_descriptor().await.expect("get_descriptor failed");
        let info = response.descriptor.device_information.expect("device_information missing");
        expect_eq!(info.vendor_id, Some(0x1234));
        expect_eq!(info.product_id, Some(0x5678));
        expect_that!(response.descriptor.keyboard, some(anything()));
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_fake_input_device_send_reports_and_ack() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 16)
            .await
            .expect("get_input_reports_reader_v2 failed");
        expect_eq!(response.max_unacknowledged_reports, 16);

        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: true });

        let report =
            fidl_input_report::InputReport { event_time: Some(12345), ..Default::default() };
        device.send_input_report(report).await;

        let reports = reader.wait_for_reports(1).await;
        assert_eq!(reports.len(), 1);
        expect_eq!(reports[0].event_time, Some(12345));

        reader.close().await;
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_fake_input_device_flow_control_assertions() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 2)
            .await
            .expect("get_input_reports_reader_v2 failed");
        expect_eq!(response.max_unacknowledged_reports, 2);

        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: false });

        // Send 2 reports up to the limit of 2.
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(1),
                ..Default::default()
            })
            .await;
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(2),
                ..Default::default()
            })
            .await;

        // Acknowledge the first report and yield so the server processes the ack.
        reader.client.acknowledge_reports(1).await.expect("acknowledge_reports failed");
        fuchsia_async::yield_now().await;
        loop {
            let acked = {
                let state = device.state.lock();
                let reader = state.readers.values().next().expect("reader not found");
                reader.last_acknowledged_report_stamp == 1
            };
            if acked {
                break;
            }
            fuchsia_async::Timer::new(MonotonicInstant::after(MonotonicDuration::from_millis(10)))
                .await;
        }

        // Sending the 3rd report should now succeed.
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(3),
                ..Default::default()
            })
            .await;

        let reports = reader.wait_for_reports(3).await;
        assert_eq!(reports.len(), 3);
        expect_eq!(reports[0].event_time, Some(1));
        expect_eq!(reports[1].event_time, Some(2));
        expect_eq!(reports[2].event_time, Some(3));

        reader.close().await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(expected = "reports must not be empty")]
    async fn test_fake_input_device_send_empty_reports_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        device.send_input_reports(&[]).await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(expected = "exceeding maximum limit")]
    async fn test_fake_input_device_unacknowledged_limit_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 1)
            .await
            .expect("get_input_reports_reader_v2 failed");
        expect_eq!(response.max_unacknowledged_reports, 1);

        #[expect(unused)]
        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: false });

        // First report is allowed (unacknowledged = 0 < 1).
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(1),
                ..Default::default()
            })
            .await;

        // Second report should panic because 1 report is unacknowledged (limit is 1).
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(2),
                ..Default::default()
            })
            .await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(
        expected = "Client acknowledged non-existent report stamp 1; highest sent stamp: 0"
    )]
    async fn test_fake_input_device_future_ack_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 16)
            .await
            .expect("get_input_reports_reader_v2 failed");
        expect_eq!(response.max_unacknowledged_reports, 16);

        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: false });

        // Acknowledge stamp 1 when no reports have been sent (last sent stamp is 0).
        reader.client.acknowledge_reports(1).await.expect("acknowledge_reports failed");
        fuchsia_async::yield_now().await;
        fuchsia_async::Timer::new(MonotonicInstant::after(MonotonicDuration::from_millis(50)))
            .await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(
        expected = "Client re-acknowledged previously acknowledged stamp 1; stamps up to 2 were already acknowledged"
    )]
    async fn test_fake_input_device_reacknowledged_stamp_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 16)
            .await
            .expect("get_input_reports_reader_v2 failed");
        expect_eq!(response.max_unacknowledged_reports, 16);

        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: false });

        device
            .send_input_reports(&[
                fidl_input_report::InputReport { event_time: Some(1), ..Default::default() },
                fidl_input_report::InputReport { event_time: Some(2), ..Default::default() },
            ])
            .await;

        reader.client.acknowledge_reports(2).await.expect("acknowledge_reports failed");
        reader.client.acknowledge_reports(1).await.expect("acknowledge_reports failed");
        fuchsia_async::yield_now().await;
        fuchsia_async::Timer::new(MonotonicInstant::after(MonotonicDuration::from_millis(50)))
            .await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(expected = "FakeInputDevice does not support wake_lease in InputReport")]
    async fn test_fake_input_device_wake_lease_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();

        #[expect(unused)]
        let response = device_client
            .get_input_reports_reader_v2(reader_server_end, 16)
            .await
            .expect("get_input_reports_reader_v2 failed");
        #[expect(unused)]
        let reader = TestReader::new(reader_client_end, TestReaderOptions { auto_ack: true });

        #[expect(unused)]
        let (wake_lease, peer) = zx::EventPair::create();
        device
            .send_input_report(fidl_input_report::InputReport {
                event_time: Some(1),
                wake_lease: Some(wake_lease),
                ..Default::default()
            })
            .await;
    }

    #[gtest]
    #[fuchsia::test]
    #[should_panic(
        expected = "GetInputReportsReader (v1) is not supported; use GetInputReportsReaderV2"
    )]
    async fn test_fake_input_device_get_input_reports_reader_v1_panics() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        #[expect(unused)]
        let (reader_client_end, reader_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReader>();
        device_client.get_input_reports_reader(reader_server_end).await.expect("send failed");
        fuchsia_async::yield_now().await;
        fuchsia_async::Timer::new(MonotonicInstant::after(MonotonicDuration::from_millis(50)))
            .await;
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_fake_input_device_unsupported_methods() {
        let device = FakeInputDevice::new(fidl_input_report::DeviceDescriptor::default());
        let scope = fuchsia_async::Scope::new();
        let (device_client_end, device_server_end) =
            fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
        device.serve_on_scope(&scope, device_server_end);
        let device_client = device_client_end.spawn();

        let res = device_client
            .send_output_report(fidl_input_report::OutputReport::default())
            .await
            .expect("fidl call failed");
        expect_eq!(res, Err(zx::Status::NOT_SUPPORTED));

        let res = device_client.get_feature_report().await.expect("fidl call failed");
        expect_eq!(res, Err(zx::Status::NOT_SUPPORTED));

        let res = device_client
            .set_feature_report(fidl_input_report::FeatureReport::default())
            .await
            .expect("fidl call failed");
        expect_eq!(res, Err(zx::Status::NOT_SUPPORTED));

        let res = device_client
            .get_input_report(fidl_input_report::DeviceType::Mouse)
            .await
            .expect("fidl call failed");
        expect_eq!(res, Err(zx::Status::NOT_SUPPORTED));
    }
}
