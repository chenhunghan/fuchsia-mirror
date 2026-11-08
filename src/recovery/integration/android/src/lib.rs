// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use bootloader_message::{BootloaderMessage, BootloaderMessageRaw};
use fidl::endpoints::{DiscoverableProtocolMarker as _, RequestStream as _, ServiceMarker as _};
use fidl_fuchsia_data as fdata;
use fidl_fuchsia_fshost::{
    RecoveryMarker, RecoveryMountSystemBlobVolumeResponder, RecoveryRequest, RecoveryRequestStream,
};
use fidl_fuchsia_hardware_adb as fadb;
use fidl_fuchsia_hardware_display as fdisplay;
use fidl_fuchsia_hardware_power_statecontrol::{
    AdminMarker, ShutdownAction, ShutdownOptions, ShutdownReason,
};
use fidl_fuchsia_input_report as finput;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_recovery::{FactoryResetMarker, FactoryResetRequest, FactoryResetRequestStream};
use fidl_fuchsia_recovery_android as frecovery_android;
use fidl_fuchsia_storage_block as fstorage_block;
use fidl_fuchsia_sysmem2 as fsysmem2;
use fidl_fuchsia_testing as ftesting;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use futures::channel::mpsc;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use mock_reboot::MockRebootService;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use vfs::execution_scope::ExecutionScope;
use vmo_backed_block_server::VmoBackedServer;
use zerocopy::IntoBytes as _;

const EXECUTOR_FLUSH_ROUNDS: usize = 4;
const CONSECUTIVE_POWER_KEYS_FOR_REBOOT: usize = 7;

struct TestEnvBuilder {
    recovery_args: String,
    use_fake_clock: bool,
    hold_blob_mount: bool,
}

impl TestEnvBuilder {
    fn new() -> Self {
        Self { recovery_args: "".into(), use_fake_clock: false, hold_blob_mount: false }
    }

    fn recovery_args(mut self, args: impl Into<String>) -> Self {
        self.recovery_args = args.into();
        self
    }

    fn use_fake_clock(mut self) -> Self {
        self.use_fake_clock = true;
        self
    }

    fn hold_blob_mount(mut self) -> Self {
        self.hold_blob_mount = true;
        self
    }

    async fn build(self) -> TestEnv {
        let builder = RealmBuilder::new().await.unwrap();
        let system_recovery = builder
            .add_child(
                "system_recovery",
                "#meta/system_recovery_android.cm",
                ChildOptions::new().eager(),
            )
            .await
            .unwrap();

        let (reboot_sender, reboot_receiver) = mpsc::unbounded();
        let reboot_service = Arc::new(MockRebootService::new(Box::new(move |options| {
            reboot_sender.unbounded_send(options).unwrap();
            Ok(())
        })));

        let (factory_reset_sender, factory_reset_receiver) = mpsc::channel(1);
        let (adb_system_type_sender, adb_system_type_receiver) = mpsc::unbounded();
        let (input_reader_sender, input_reader_receiver) = mpsc::unbounded();
        let (blob_mount_sender, blob_mount_receiver) = mpsc::unbounded();
        let hold_blob_mount = self.hold_blob_mount;

        let msg: BootloaderMessageRaw =
            BootloaderMessage::with_args(&self.recovery_args).try_into().unwrap();
        let vmo_server = Arc::new(VmoBackedServer::new(8, 512, msg.as_bytes()).unwrap());

        let mount_called = Arc::new(AtomicU32::new(0));
        let mock_fshost_mount_called = Arc::clone(&mount_called);

        let service_reflector_root = vfs::pseudo_directory! {
            "block" => vfs::pseudo_directory! {
                "misc" => vfs::pseudo_directory! {
                    fstorage_block::BlockMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                        let vmo_server = Arc::clone(&vmo_server);
                        async move {
                            let () = vmo_server.serve(stream).await.unwrap();
                        }
                    }),
                }
            },
            "svc" => vfs::pseudo_directory! {
                FactoryResetMarker::PROTOCOL_NAME => vfs::service::host(move |mut stream: FactoryResetRequestStream| {
                    let mut factory_reset_sender = factory_reset_sender.clone();
                    async move {
                        while let Some(Ok(request)) = stream.next().await {
                            factory_reset_sender.send(request).await.unwrap();
                        }
                    }
                }),
                AdminMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                    let reboot_service = Arc::clone(&reboot_service);
                    async move {
                        let () = reboot_service.run_reboot_service(stream).await.unwrap();
                    }
                }),
                fadb::StateControllerMarker::PROTOCOL_NAME => vfs::service::host(move |mut stream: fadb::StateControllerRequestStream| {
                    let adb_system_type_sender = adb_system_type_sender.clone();
                    async move {
                        while let Some(Ok(request)) = stream.next().await {
                            match request {
                                fadb::StateControllerRequest::SetSystemType { system_type, responder } => {
                                    responder.send(Ok(())).unwrap();
                                    let _ = adb_system_type_sender.unbounded_send(system_type);
                                }
                                _ => {}
                            }
                        }
                    }
                }),
                RecoveryMarker::PROTOCOL_NAME => vfs::service::host(move |mut stream: RecoveryRequestStream| {
                    let mount_called = Arc::clone(&mock_fshost_mount_called);
                    let blob_mount_sender = blob_mount_sender.clone();
                    async move {
                        while let Some(Ok(RecoveryRequest::MountSystemBlobVolume {
                            blob_exposed_dir: _,
                            responder,
                        })) = stream.next().await
                        {
                            mount_called.fetch_add(1, Ordering::SeqCst);
                            if hold_blob_mount {
                                let _ = blob_mount_sender.unbounded_send(responder);
                            } else {
                                responder.send(Ok(())).unwrap();
                            }
                        }
                    }
                }),
                finput::ServiceMarker::SERVICE_NAME => vfs::pseudo_directory! {
                    "default" => vfs::pseudo_directory! {
                        "input_device" => vfs::service::host(move |mut stream: finput::InputDeviceRequestStream| {
                            let input_reader_sender = input_reader_sender.clone();
                            async move {
                                while let Some(Ok(request)) = stream.next().await {
                                    match request {
                                        finput::InputDeviceRequest::GetDescriptor { responder } => {
                                            responder.send(&finput::DeviceDescriptor::default()).unwrap();
                                        }
                                        finput::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            max_unacknowledged_reports_limit,
                                            responder,
                                        } => {
                                            responder.send(max_unacknowledged_reports_limit).unwrap();
                                            let _ = input_reader_sender.unbounded_send(reader.into_stream());
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }),
                    },
                },
            },
        };

        let service_reflector = builder
            .add_local_child(
                "service_reflector",
                move |handles| {
                    let scope = ExecutionScope::new();
                    vfs::directory::serve_on(
                        service_reflector_root.clone(),
                        fio::PERM_READABLE | fio::PERM_WRITABLE,
                        scope.clone(),
                        handles.outgoing_dir,
                    );
                    async move { Ok(scope.wait().await) }.boxed()
                },
                ChildOptions::new(),
            )
            .await
            .unwrap();

        builder
            .add_route(
                Route::new()
                    .capability(
                        Capability::directory("block").path("/block").rights(fio::R_STAR_DIR),
                    )
                    .capability(Capability::protocol::<FactoryResetMarker>())
                    .capability(Capability::protocol::<AdminMarker>())
                    .capability(Capability::protocol::<RecoveryMarker>())
                    .capability(Capability::protocol::<fadb::StateControllerMarker>())
                    .capability(Capability::service::<finput::ServiceMarker>())
                    .from(&service_reflector)
                    .to(&system_recovery),
            )
            .await
            .unwrap();

        let fake_display = builder
            .add_child("fake-display", "#meta/fake-display-stack-host.cm", ChildOptions::new())
            .await
            .unwrap();
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<fdisplay::ServiceMarker>())
                    .from(&fake_display)
                    .to(&system_recovery),
            )
            .await
            .unwrap();
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fsysmem2::AllocatorMarker>())
                    .from(Ref::parent())
                    .to(&fake_display),
            )
            .await
            .unwrap();

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fsysmem2::AllocatorMarker>())
                    .from(Ref::parent())
                    .to(&system_recovery),
            )
            .await
            .unwrap();

        builder
            .add_route(
                Route::new()
                    .capability(Capability::configuration("fuchsia.recovery.DisplayRotation"))
                    .capability(Capability::configuration("fuchsia.recovery.android.TopMargin"))
                    .capability(Capability::configuration(
                        "fuchsia.recovery.android.TouchScreenEnabled",
                    ))
                    .from(Ref::void())
                    .to(&system_recovery),
            )
            .await
            .unwrap();

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<frecovery_android::UpdaterMarker>())
                    .from(&system_recovery)
                    .to(Ref::parent()),
            )
            .await
            .unwrap();

        if self.use_fake_clock {
            let mut decl = builder.get_component_decl(&system_recovery).await.unwrap();
            let entries = decl
                .program
                .as_mut()
                .expect("program")
                .info
                .entries
                .as_mut()
                .expect("program entries");
            let binary =
                entries.iter_mut().find(|entry| entry.key == "binary").expect("binary entry");
            binary.value = Some(Box::new(fdata::DictionaryValue::Str(
                "bin/system_recovery_android_fake_time".to_string(),
            )));
            let mut uses = Vec::from(std::mem::take(&mut decl.uses));
            uses.push(cm_rust::UseDecl::Protocol(cm_rust::UseProtocolDecl {
                source: cm_rust::UseSource::Parent,
                source_name: ftesting::FakeClockMarker::PROTOCOL_NAME.parse().unwrap(),
                source_dictionary: Default::default(),
                target_path: Some(
                    format!("/svc/{}", ftesting::FakeClockMarker::PROTOCOL_NAME).parse().unwrap(),
                ),
                dependency_type: cm_rust::DependencyType::Strong,
                availability: cm_rust::Availability::Required,
                numbered_handle: None,
            }));
            decl.uses = uses.into_boxed_slice();
            builder.replace_component_decl(&system_recovery, decl).await.unwrap();

            let fake_clock = builder
                .add_child("fake_clock", "#meta/fake_clock.cm", ChildOptions::new())
                .await
                .unwrap();
            builder
                .add_route(
                    Route::new()
                        .capability(Capability::protocol::<ftesting::FakeClockMarker>())
                        .from(&fake_clock)
                        .to(&system_recovery),
                )
                .await
                .unwrap();
            builder
                .add_route(
                    Route::new()
                        .capability(Capability::protocol::<ftesting::FakeClockControlMarker>())
                        .from(&fake_clock)
                        .to(Ref::parent()),
                )
                .await
                .unwrap();
        }

        let realm = builder.build().await.unwrap();
        TestEnv {
            realm,
            mount_called,
            reboot_receiver,
            factory_reset_receiver,
            adb_system_type_receiver,
            blob_mount_receiver,
            input_reader_receiver,
            input_reader_stream: None,
            next_report_stamp: 0,
        }
    }
}

struct TestEnv {
    realm: RealmInstance,
    mount_called: Arc<AtomicU32>,
    reboot_receiver: mpsc::UnboundedReceiver<ShutdownOptions>,
    factory_reset_receiver: mpsc::Receiver<FactoryResetRequest>,
    adb_system_type_receiver: mpsc::UnboundedReceiver<fadb::SystemType>,
    blob_mount_receiver: mpsc::UnboundedReceiver<RecoveryMountSystemBlobVolumeResponder>,
    input_reader_receiver: mpsc::UnboundedReceiver<finput::InputReportsReaderV2RequestStream>,
    input_reader_stream: Option<finput::InputReportsReaderV2RequestStream>,
    next_report_stamp: u64,
}

impl TestEnv {
    fn updater_proxy(&self) -> frecovery_android::UpdaterProxy {
        self.realm.root.connect_to_protocol_at_exposed_dir().expect("connect to updater")
    }

    fn fake_clock_control(&self) -> ftesting::FakeClockControlProxy {
        self.realm.root.connect_to_protocol_at_exposed_dir().expect("connect to fake clock control")
    }

    fn mount_called(&self) -> u32 {
        self.mount_called.load(Ordering::SeqCst)
    }

    async fn wait_for_reboot(&mut self) -> ShutdownOptions {
        self.reboot_receiver.next().await.expect("wait for reboot")
    }

    async fn wait_for_factory_reset_request(&mut self) -> FactoryResetRequest {
        self.factory_reset_receiver.next().await.expect("wait for factory reset request")
    }

    async fn wait_for_adb_system_type(&mut self) -> fadb::SystemType {
        self.adb_system_type_receiver.next().await.expect("wait for adb system type")
    }

    async fn wait_for_blob_mount_request(&mut self) -> RecoveryMountSystemBlobVolumeResponder {
        self.blob_mount_receiver.next().await.expect("wait for blob mount request")
    }

    async fn send_input_reports(&mut self, reports: Vec<finput::InputReport>) {
        let reader_stream = match &mut self.input_reader_stream {
            Some(stream) => stream,
            None => {
                let stream =
                    self.input_reader_receiver.next().await.expect("wait for input reader");
                self.input_reader_stream.insert(stream)
            }
        };
        self.next_report_stamp += 1;
        reader_stream
            .control_handle()
            .send_on_input_reports(reports, self.next_report_stamp)
            .expect("send input reports");
        match reader_stream.next().await {
            Some(Ok(finput::InputReportsReaderV2Request::AcknowledgeReports {
                last_acknowledged_report_stamp,
                ..
            })) => assert_eq!(last_acknowledged_report_stamp, self.next_report_stamp),
            other => panic!("unexpected input reader request: {other:?}"),
        }
    }

    async fn flush_recovery_executor(&mut self) {
        // Send a non-empty batch so Carnelian forwards a no-op report to its view loop.
        for _ in 0..EXECUTOR_FLUSH_ROUNDS {
            self.send_input_reports(vec![finput::InputReport::default()]).await;
        }
    }

    async fn press_button(&mut self, button: finput::ConsumerControlButton) {
        let make_report = |pressed_buttons| finput::InputReport {
            event_time: Some(0),
            consumer_control: Some(finput::ConsumerControlInputReport {
                pressed_buttons: Some(pressed_buttons),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.send_input_reports(vec![make_report(vec![button]), make_report(vec![])]).await;
    }
}

#[fuchsia::test]
async fn test_wipe_data() {
    let mut env = TestEnvBuilder::new().recovery_args("--wipe_data\n").build().await;
    match env.wait_for_factory_reset_request().await {
        FactoryResetRequest::Reset { responder } => {
            responder.send(zx::sys::ZX_OK).unwrap();
        }
    }
}

#[fuchsia::test]
async fn test_sideload_auto_reboot() {
    let mut env = TestEnvBuilder::new().recovery_args("--sideload_auto_reboot\n").build().await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    // This returns after update has finished, but the update is expected to fail, because the test
    // doesn't provide all the dependencies needed for a successful update.
    let () = env.updater_proxy().update("https://fuchsia.com/ota_manifest").await.unwrap();
    assert_eq!(env.mount_called(), 1);

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
}

#[fuchsia::test]
async fn test_update_rejected_outside_sideload() {
    let env = TestEnvBuilder::new().build().await;

    let err = env
        .updater_proxy()
        .update("https://fuchsia.com/ota_manifest")
        .await
        .expect_err("update outside sideload should be rejected");
    assert!(err.is_closed(), "unexpected error: {err:?}");
    assert_eq!(env.mount_called(), 0);
}

#[fuchsia::test]
async fn test_sideload_from_menu() {
    let mut env = TestEnvBuilder::new().build().await;

    // Move down past "Apply update from ADB" and back up to select it from the main menu.
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeUp).await;
    env.press_button(finput::ConsumerControlButton::Power).await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    let () = env.updater_proxy().update("https://fuchsia.com/ota_manifest").await.unwrap();
    assert_eq!(env.mount_called(), 1);
    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Recovery);
    env.flush_recovery_executor().await;
    assert!(env.reboot_receiver.try_recv().is_err());

    // Dismiss the update log screen back to the main menu, then select "Reboot system now".
    env.press_button(finput::ConsumerControlButton::Power).await;
    env.press_button(finput::ConsumerControlButton::Power).await;

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
}

async fn fast_forward_fake_clock(fake_clock: &ftesting::FakeClockControlProxy) {
    fake_clock.pause().await.expect("pause fake clock");
    fake_clock
        .resume_with_increments(
            zx::MonotonicDuration::from_millis(1).into_nanos(),
            &ftesting::Increment::Determined(zx::MonotonicDuration::from_seconds(10).into_nanos()),
        )
        .await
        .expect("resume_with_increments fidl")
        .expect("resume_with_increments");
}

#[fuchsia::test]
async fn test_sideload_timeout() {
    let mut env =
        TestEnvBuilder::new().recovery_args("--sideload\n").use_fake_clock().build().await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);
    let fake_clock = env.fake_clock_control();
    fake_clock.pause().await.expect("pause fake clock");
    env.flush_recovery_executor().await;

    fake_clock
        .advance(&ftesting::Increment::Determined(
            zx::MonotonicDuration::from_minutes(4).into_nanos(),
        ))
        .await
        .expect("advance fidl")
        .expect("advance 4 minutes");
    env.flush_recovery_executor().await;
    assert!(env.adb_system_type_receiver.try_recv().is_err());

    fast_forward_fake_clock(&fake_clock).await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Recovery);
    fake_clock.pause().await.expect("pause fake clock");
    env.flush_recovery_executor().await;
    let err = env
        .updater_proxy()
        .update("https://fuchsia.com/ota_manifest")
        .await
        .expect_err("update after sideload timeout should be rejected");
    assert!(err.is_closed(), "unexpected error: {err:?}");
    assert_eq!(env.mount_called(), 0);
    env.realm.destroy().await.unwrap();
    assert!(env.reboot_receiver.try_recv().is_err());
}

#[fuchsia::test]
async fn test_sideload_auto_reboot_timeout() {
    let mut env = TestEnvBuilder::new()
        .recovery_args("--sideload_auto_reboot\n")
        .use_fake_clock()
        .build()
        .await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    let fake_clock = env.fake_clock_control();
    fast_forward_fake_clock(&fake_clock).await;

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
    assert_eq!(env.mount_called(), 0);
}

#[fuchsia::test]
async fn test_sideload_timeout_from_menu() {
    let mut env = TestEnvBuilder::new().use_fake_clock().build().await;

    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::Power).await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    let fake_clock = env.fake_clock_control();
    fast_forward_fake_clock(&fake_clock).await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Recovery);
    fake_clock.pause().await.expect("pause fake clock");
    env.flush_recovery_executor().await;
    assert_eq!(env.mount_called(), 0);
    assert!(env.reboot_receiver.try_recv().is_err());

    // Dismiss the timeout screen back to the main menu, then select "Reboot system now".
    env.press_button(finput::ConsumerControlButton::Power).await;
    env.press_button(finput::ConsumerControlButton::Power).await;

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
}

#[fuchsia::test]
async fn test_power_button_reboot_during_sideload() {
    let mut env = TestEnvBuilder::new().recovery_args("--sideload\n").build().await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    for _ in 0..CONSECUTIVE_POWER_KEYS_FOR_REBOOT {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
    let err = env
        .updater_proxy()
        .update("https://fuchsia.com/ota_manifest")
        .await
        .expect_err("update after power-button reboot should be rejected");
    assert!(err.is_closed(), "unexpected error: {err:?}");
    assert_eq!(env.mount_called(), 0);
}

#[fuchsia::test]
async fn test_power_button_count_resets_on_other_button() {
    let mut env =
        TestEnvBuilder::new().recovery_args("--sideload\n").use_fake_clock().build().await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    for _ in 0..(CONSECUTIVE_POWER_KEYS_FOR_REBOOT - 1) {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    for _ in 0..(CONSECUTIVE_POWER_KEYS_FOR_REBOOT - 1) {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }

    // Time out the sideload wait to prove the sideload task was not cancelled by a premature
    // reboot, and that `TaskDone` resets the consecutive power count before confirmation.
    let fake_clock = env.fake_clock_control();
    fast_forward_fake_clock(&fake_clock).await;
    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Recovery);
    fake_clock.pause().await.expect("pause fake clock");
    env.flush_recovery_executor().await;

    // Pressing Power once on the timeout confirmation screen should dismiss back to the main menu
    // rather than hitting 7 consecutive presses and rebooting; then move down to Sideload, enter
    // Sideload again, reset with VolumeDown, and verify 7 consecutive Power presses do reboot.
    env.press_button(finput::ConsumerControlButton::Power).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::Power).await;
    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.flush_recovery_executor().await;
    assert!(env.reboot_receiver.try_recv().is_err());

    for _ in 0..CONSECUTIVE_POWER_KEYS_FOR_REBOOT {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }
    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    env.flush_recovery_executor().await;
    env.realm.destroy().await.unwrap();
    assert!(env.reboot_receiver.try_recv().is_err());
}

#[fuchsia::test]
async fn test_power_button_reboot_during_wipe_data() {
    let mut env = TestEnvBuilder::new().recovery_args("--wipe_data\n").build().await;

    let _reset_request = env.wait_for_factory_reset_request().await;

    for _ in 0..CONSECUTIVE_POWER_KEYS_FOR_REBOOT {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }

    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    assert_eq!(reboot_options.reasons, Some(vec![ShutdownReason::DeveloperRequest]));
}

#[fuchsia::test]
async fn test_power_button_ignored_during_sideload_update() {
    let mut env =
        TestEnvBuilder::new().recovery_args("--sideload\n").hold_blob_mount().build().await;

    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    let update_future = env.updater_proxy().update("https://fuchsia.com/ota_manifest");
    let mount_responder = env.wait_for_blob_mount_request().await;

    for _ in 0..(CONSECUTIVE_POWER_KEYS_FOR_REBOOT + 1) {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }

    mount_responder.send(Err(zx::Status::INTERNAL.into_raw())).unwrap();
    let () = update_future.await.unwrap();
    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Recovery);
    env.flush_recovery_executor().await;
    assert!(env.reboot_receiver.try_recv().is_err());

    // Dismiss the failed-update screen back to the main menu, re-enter Sideload, reset with
    // VolumeDown, and verify power-key reboot works again and only one reboot was ever sent.
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.press_button(finput::ConsumerControlButton::Power).await;
    assert_eq!(env.wait_for_adb_system_type().await, fadb::SystemType::Sideload);

    env.press_button(finput::ConsumerControlButton::VolumeDown).await;
    env.flush_recovery_executor().await;
    assert!(env.reboot_receiver.try_recv().is_err());

    for _ in 0..CONSECUTIVE_POWER_KEYS_FOR_REBOOT {
        env.press_button(finput::ConsumerControlButton::Power).await;
    }
    let reboot_options = env.wait_for_reboot().await;
    assert_eq!(reboot_options.action, Some(ShutdownAction::Reboot));
    env.flush_recovery_executor().await;
    env.realm.destroy().await.unwrap();
    assert!(env.reboot_receiver.try_recv().is_err());
}
