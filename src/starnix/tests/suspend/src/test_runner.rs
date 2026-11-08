// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_power_system as fpower;
use fidl_fuchsia_process as fprocess;
use fidl_fuchsia_starnix_runner as fstarnix;
use fidl_fuchsia_time_alarms as fta;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmBuilderParams,
    RealmInstance, Ref, Route,
};
use fuchsia_runtime::{HandleInfo, HandleType};
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures::task::Poll;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;
use zx::Peered;

const AWAKE_SIGNAL: zx::Signals = zx::Signals::USER_0;
const ASLEEP_SIGNAL: zx::Signals = zx::Signals::USER_1;

async fn build_realm() -> RealmInstance {
    let builder =
        RealmBuilder::with_params(RealmBuilderParams::new().from_relative_url("#meta/realm.cm"))
            .await
            .expect("created");
    builder.build().await.unwrap()
}

#[fuchsia::test]
async fn test_register_wake_watcher() {
    let realm_instance = build_realm().await;
    let manager: fstarnix::ManagerProxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    let job = fuchsia_runtime::job_default();
    let child_job = job.create_child_job().unwrap();

    let (wake_watcher, wake_watcher_remote) = zx::EventPair::create();

    manager
        .register_wake_watcher(fstarnix::ManagerRegisterWakeWatcherRequest {
            watcher: Some(wake_watcher_remote),
            ..Default::default()
        })
        .await
        .unwrap();

    // Get initial AWAKE SIGNAL.
    fasync::OnSignals::new(&wake_watcher, AWAKE_SIGNAL).await.expect("awake");

    let mut signal_fut = pin!(fasync::OnSignals::new(&wake_watcher, ASLEEP_SIGNAL));
    assert_eq!(Poll::Pending, futures::poll!(&mut signal_fut));

    let suspend_fut = manager.suspend_container(fstarnix::ManagerSuspendContainerRequest {
        container_job: Some(child_job.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
        wake_locks: None,
        ..Default::default()
    });

    signal_fut.await.expect("asleep future");

    fasync::OnSignals::new(&wake_watcher, AWAKE_SIGNAL).await.expect("final awake signal");
    let _ = suspend_fut.await;
}

#[fuchsia::test]
async fn test_wake_lock() {
    let realm_instance = build_realm().await;
    let manager: fstarnix::ManagerProxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    let (wake_lock, wake_lock_remote) = zx::EventPair::create();
    let job = fuchsia_runtime::job_default();
    let child_job = job.create_child_job().unwrap();

    // If we signal on the wake lock then suspend will fail.
    wake_lock.signal_peer(zx::Signals::empty(), zx::Signals::EVENT_SIGNALED).unwrap();
    assert!(matches!(
        manager
            .suspend_container(fstarnix::ManagerSuspendContainerRequest {
                container_job: Some(child_job.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                wake_locks: Some(wake_lock_remote),
                ..Default::default()
            })
            .await
            .unwrap(),
        Err(fstarnix::SuspendError::WakeLocksExist)
    ));
}

type PendingAlarms = Arc<Mutex<HashMap<String, fta::WakeAlarmsSetAndWaitResponder>>>;

enum MockService {
    ActivityGovernor(fpower::ActivityGovernorRequestStream),
    WakeAlarms(fta::WakeAlarmsRequestStream),
}

async fn serve_mocks(
    handles: LocalComponentHandles,
    pending_alarms: PendingAlarms,
    alarms_tx: mpsc::UnboundedSender<String>,
    leases_tx: mpsc::UnboundedSender<zx::EventPair>,
) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    fs.dir("svc")
        .add_fidl_service(MockService::ActivityGovernor)
        .add_fidl_service(MockService::WakeAlarms);
    fs.serve_connection(handles.outgoing_dir)?;
    fs.for_each_concurrent(None, |service| {
        let pending_alarms = pending_alarms.clone();
        let alarms_tx = alarms_tx.clone();
        let leases_tx = leases_tx.clone();
        async move {
            match service {
                MockService::ActivityGovernor(mut stream) => {
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
                                responder,
                                ..
                            } => {
                                let (client, server) = zx::EventPair::create();
                                let _ = leases_tx.unbounded_send(server);
                                let _ = responder.send(client);
                            }
                            other => panic!("unexpected ActivityGovernor request: {other:?}"),
                        }
                    }
                }
                MockService::WakeAlarms(mut stream) => {
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            fta::WakeAlarmsRequest::SetAndWait {
                                mode,
                                alarm_id,
                                responder,
                                ..
                            } => {
                                if let fta::SetMode::NotifySetupDone(done) = mode {
                                    done.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED)
                                        .unwrap();
                                }
                                if let Some(prev) =
                                    pending_alarms.lock().insert(alarm_id.clone(), responder)
                                {
                                    let _ = prev.send(Err(fta::WakeAlarmsError::Dropped));
                                }
                                let _ = alarms_tx.unbounded_send(alarm_id);
                            }
                            fta::WakeAlarmsRequest::Cancel { alarm_id, .. } => {
                                if let Some(prev) = pending_alarms.lock().remove(&alarm_id) {
                                    let _ = prev.send(Err(fta::WakeAlarmsError::Dropped));
                                }
                            }
                            other => panic!("unexpected WakeAlarms request: {other:?}"),
                        }
                    }
                }
            }
        }
    })
    .await;
    Ok(())
}

fn fire_alarm(pending_alarms: &PendingAlarms, alarm_id: &str) -> zx::EventPair {
    let responder =
        pending_alarms.lock().remove(alarm_id).expect("expected pending SetAndWait responder");
    let (lease_client, lease_server) = zx::EventPair::create();
    responder.send(Ok(lease_client)).expect("send SetAndWait response");
    lease_server
}

#[fuchsia::test]
async fn test_waking_timer_blocks_suspend_until_epoll_wait_completes() {
    let pending_alarms: PendingAlarms = Arc::new(Mutex::new(HashMap::new()));
    let (alarms_tx, mut alarms_rx) = mpsc::unbounded();
    let (sag_leases_tx, mut sag_leases_rx) = mpsc::unbounded();

    let builder = RealmBuilder::with_params(
        RealmBuilderParams::new().from_relative_url("#meta/container_realm.cm"),
    )
    .await
    .unwrap();

    let mocks_alarms = pending_alarms.clone();
    let mocks = builder
        .add_local_child(
            "mocks",
            move |h| {
                Box::pin(serve_mocks(
                    h,
                    mocks_alarms.clone(),
                    alarms_tx.clone(),
                    sag_leases_tx.clone(),
                ))
            },
            ChildOptions::new(),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(
                    fpower::ActivityGovernorMarker::PROTOCOL_NAME,
                ))
                .capability(Capability::protocol_by_name(fta::WakeAlarmsMarker::PROTOCOL_NAME))
                .from(&mocks)
                .to(Ref::child("runner")),
        )
        .await
        .unwrap();

    let realm = builder.build().await.unwrap();

    let manager: fstarnix::ManagerProxy = realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    let (wake_watcher, wake_watcher_remote) = zx::EventPair::create();
    manager
        .register_wake_watcher(fstarnix::ManagerRegisterWakeWatcherRequest {
            watcher: Some(wake_watcher_remote),
            ..Default::default()
        })
        .await
        .unwrap();
    fasync::OnSignals::new(&wake_watcher, AWAKE_SIGNAL).await.unwrap();

    let (stdin_recv, puppet_stdin) = zx::Socket::create_stream();
    let (stdout_recv, stdout_send) = zx::Socket::create_stream();
    let puppet_alive = stdout_recv.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let test_realm: fcomponent::RealmProxy =
        realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    test_realm
        .create_child(
            &fdecl::CollectionRef { name: "puppets".to_string() },
            &fdecl::Child {
                name: Some("puppet".to_string()),
                url: Some("#meta/wake_alarm_epoll_puppet.cm".to_string()),
                startup: Some(fdecl::StartupMode::Lazy),
                ..Default::default()
            },
            fcomponent::CreateChildArgs {
                numbered_handles: Some(vec![
                    fprocess::HandleInfo {
                        id: HandleInfo::new(HandleType::FileDescriptor, 0).as_raw(),
                        handle: stdin_recv.into(),
                    },
                    fprocess::HandleInfo {
                        id: HandleInfo::new(HandleType::FileDescriptor, 1).as_raw(),
                        handle: stdout_send.into(),
                    },
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();

    let mut puppet_stdin = fasync::Socket::from_socket(puppet_stdin);
    let mut puppet_stdout = BufReader::new(fasync::Socket::from_socket(stdout_recv));
    let mut line = String::new();

    let second_alarm = futures::select_biased! {
        _ = fasync::OnSignals::new(&puppet_alive, zx::Signals::SOCKET_PEER_CLOSED).fuse() => {
            panic!("puppet exited unexpectedly before final wakeup");
        }
        second_alarm = async {
            // 1. Wait for initial alarm setup and initial container suspend.
            let initial_sag_lease = sag_leases_rx.next().await.unwrap();
            let first_alarm = alarms_rx.next().await.unwrap();
            fasync::OnSignals::new(&wake_watcher, ASLEEP_SIGNAL).await.unwrap();
            fasync::OnSignals::new(&initial_sag_lease, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .await
                .unwrap();

            // 2. Fire the alarm, wait for resume + fresh SAG lease + WakeAlarms lease drop, then
            //    release the puppet to step through frozen waiter -> thaw -> read(timerfd) ->
            //    next epoll_wait.
            let first_alarm_lease = fire_alarm(&pending_alarms, &first_alarm);
            fasync::OnSignals::new(&wake_watcher, AWAKE_SIGNAL).await.unwrap();
            let resume_sag_lease = sag_leases_rx.next().await.unwrap();
            fasync::OnSignals::new(&first_alarm_lease, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .await
                .unwrap();
            puppet_stdin.write_all(b"lease_dropped\n").await.unwrap();

            // 3. Enforce continuously that neither wake_watcher nor resume_sag_lease suspends
            //    until the second epoll_wait clears EPOLLWAKEUP and unblocks
            //    `/sys/power/wakeup_count`.
            futures::select_biased! {
                _ = fasync::OnSignals::new(&wake_watcher, ASLEEP_SIGNAL).fuse() => {
                    panic!("container entered ASLEEP_SIGNAL before next epoll_wait");
                }
                _ = fasync::OnSignals::new(
                    &resume_sag_lease,
                    zx::Signals::EVENTPAIR_PEER_CLOSED,
                ).fuse() => {
                    panic!("runner dropped SAG lease before next epoll_wait");
                }
                extra = sag_leases_rx.next() => {
                    panic!("runner acquired unexpected extra SAG lease: {extra:?}");
                }
                res = puppet_stdout.read_line(&mut line).fuse() => {
                    res.expect("read puppet stdout");
                    assert_eq!(line, "wakeup_cleared\n");
                }
            }

            // 4. Release the puppet to re-suspend and verify container suspend succeeds once
            //    EPOLLWAKEUP is cleared.
            let second_alarm = alarms_rx.next().await.unwrap();
            puppet_stdin.write_all(b"resuspend\n").await.unwrap();
            fasync::OnSignals::new(&wake_watcher, ASLEEP_SIGNAL).await.unwrap();
            fasync::OnSignals::new(&resume_sag_lease, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .await
                .unwrap();
            second_alarm
        }.fuse() => second_alarm,
    };

    let _second_alarm_lease = fire_alarm(&pending_alarms, &second_alarm);
    line.clear();
    puppet_stdout.read_line(&mut line).await.unwrap();
    assert_eq!(line, "resuspended\n");
    realm.destroy().await.unwrap();
}
