// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, Result, anyhow};
use fidl::endpoints::{ProtocolMarker, create_endpoints};
use fidl_fuchsia_power_system as fps;
use fuchsia_async as fasync;
use fuchsia_component::client;
use futures::channel::mpsc;
use futures::{Future, SinkExt, StreamExt, TryStreamExt, future};
use log::{debug, error};
use std::rc::Rc;

const SUSPEND_BLOCKER_NAME: &str = "timekeeper";

const POWER_ON: u8 = 0xff;
const POWER_OFF: u8 = 0x00;

enum SuspendBlockerResponder {
    BeforeSuspend(fps::SuspendBlockerBeforeSuspendResponder),
    AfterResume(fps::SuspendBlockerAfterResumeResponder),
}

pub async fn manage(
    activity_signal: mpsc::Sender<super::Command>,
    timer_loop: Option<Rc<alarms::Loop>>,
) -> Result<fasync::Task<()>> {
    let governor_proxy = client::connect_to_protocol::<fps::ActivityGovernorMarker>()
        .with_context(|| {
            format!("while connecting to: {:?}", fps::ActivityGovernorMarker::DEBUG_NAME)
        })?;

    manage_internal(governor_proxy, activity_signal, timer_loop, management_loop).await
}

async fn manage_internal<F, G>(
    governor_proxy: fps::ActivityGovernorProxy,
    activity: mpsc::Sender<super::Command>,
    timer_loop: Option<Rc<alarms::Loop>>,
    // Injected in tests.
    loop_fn: F,
) -> Result<fasync::Task<()>>
where
    G: Future<Output = fasync::Task<()>>,
    F: Fn(
        fps::SuspendBlockerRequestStream,
        mpsc::Sender<super::Command>,
        Option<Rc<alarms::Loop>>,
    ) -> G,
{
    let (suspend_blocker_client, suspend_blocker_server) =
        create_endpoints::<fps::SuspendBlockerMarker>();
    let result = governor_proxy
        .register_suspend_blocker(fps::ActivityGovernorRegisterSuspendBlockerRequest {
            suspend_blocker: Some(suspend_blocker_client),
            name: Some(SUSPEND_BLOCKER_NAME.into()),
            ..Default::default()
        })
        .await
        .context("while calling fuchsia.power.system.ActivityGovernor/RegisterSuspendBlocker")?;
    let suspend_blocker = suspend_blocker_server.into_stream();
    match result {
        Ok(_) => Ok(loop_fn(suspend_blocker, activity, timer_loop).await),
        Err(e) => Err(anyhow!("error while registering suspend blocker: {:?}", e)),
    }
}

// Loop around and react to SuspendBlocker messages. Use separate tasks to ensure
// we can insert a power transition process in between.
//
// Returns the task spawned for transition control.
async fn management_loop(
    mut suspend_blocker_stream: fps::SuspendBlockerRequestStream,
    mut activity: mpsc::Sender<super::Command>,
    timer_loop: Option<Rc<alarms::Loop>>,
) -> fasync::Task<()> {
    // The Sender is used to ensure that rcv_task does not send before send_task
    // is done.
    let (mut send, mut rcv) = mpsc::channel::<(u8, mpsc::Sender<()>)>(1);

    let rcv_task = fasync::Task::local(async move {
        loop {
            let Ok(Some(request)) = suspend_blocker_stream.try_next().await else {
                error!("error while waiting for suspend blocker request, bailing");
                break;
            };
            let result = handle_suspend_blocker_request(request).await;
            match result {
                Ok((level, responder)) => {
                    let (s, mut r) = mpsc::channel::<()>(1);
                    if let Err(e) = send.send((level, s)).await {
                        error!("error while processing power level, bailing: {:?}", e);
                        break;
                    }
                    // Wait until send_task propagates the new required level.
                    r.next().await.unwrap();
                    // Respond to the SuspendBlockerRequest, indicating the level transition is
                    // complete.
                    match responder {
                        SuspendBlockerResponder::BeforeSuspend(responder) => {
                            responder.send().expect("BeforeSuspend resp failed");
                        }
                        SuspendBlockerResponder::AfterResume(responder) => {
                            responder.send().expect("AfterResume resp failed");
                        }
                    }
                }
                Err(e) => {
                    error!("error while watching level, bailing: {:?}", e);
                    break;
                }
            }
        }
        debug!("no longer watching required level");
    });
    let send_task = fasync::Task::local(async move {
        while let Some((new_level, mut s)) = rcv.next().await {
            let suspended = match new_level {
                POWER_OFF => {
                    debug!("new required level: power off");
                    Some(true)
                }
                POWER_ON => {
                    debug!("new required level: power on");
                    Some(false)
                }
                _ => {
                    error!("invalid required level: {}", new_level);
                    None
                }
            };
            if let Some(suspended) = suspended {
                let (done_tx, mut done_rx) = mpsc::channel(1);
                if let Err(e) = activity
                    .send(super::Command::PowerManagement { suspended, done: done_tx })
                    .await
                {
                    error!("error sending PowerManagement command: {:?}", e);
                } else if done_rx.next().await.is_none() {
                    error!("PowerManagement command responder dropped without signaling");
                }
                if suspended && let Some(ref tl) = timer_loop {
                    tl.before_suspend().await;
                }
            }
            // Signal rcv_task that level transition handling is complete.
            s.send(()).await.unwrap();
        }
        debug!("no longer reporting required level");
    });
    fasync::Task::local(async move {
        future::join(rcv_task, send_task).await;
    })
}

async fn handle_suspend_blocker_request(
    request: fps::SuspendBlockerRequest,
) -> Result<(u8, SuspendBlockerResponder), Error> {
    match request {
        fps::SuspendBlockerRequest::BeforeSuspend { responder } => {
            return Ok((POWER_OFF, SuspendBlockerResponder::BeforeSuspend(responder)));
        }
        fps::SuspendBlockerRequest::AfterResume { responder } => {
            return Ok((POWER_ON, SuspendBlockerResponder::AfterResume(responder)));
        }
        fps::SuspendBlockerRequest::_UnknownMethod { .. } => {
            return Err(Error::msg("SuspendBlockerRequest::_UnknownMethod received"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command;
    use fidl::endpoints;
    use log::debug;

    #[fuchsia::test]
    async fn propagate_level() {
        let (suspend_blocker_client, suspend_blocker_server) =
            create_endpoints::<fps::SuspendBlockerMarker>();
        let suspend_blocker = suspend_blocker_server.into_stream();
        let suspend_blocker_proxy = suspend_blocker_client.into_proxy();
        let (activity_s, mut activity_r) = mpsc::channel::<Command>(1);

        // Management loop is also asynchronous.
        fasync::Task::local(async move {
            management_loop(suspend_blocker, activity_s, None).await.await;
        })
        .detach();

        let (mut observed_tx, mut observed_rx) = mpsc::channel::<bool>(3);
        fasync::Task::local(async move {
            while let Some(cmd) = activity_r.next().await {
                match cmd {
                    Command::PowerManagement { suspended, mut done } => {
                        observed_tx.send(suspended).await.unwrap();
                        done.send(()).await.unwrap();
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            }
        })
        .detach();

        assert!(suspend_blocker_proxy.after_resume().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(false));

        assert!(suspend_blocker_proxy.before_suspend().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(true));

        assert!(suspend_blocker_proxy.after_resume().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(false));
    }

    async fn empty_loop(
        _: fps::SuspendBlockerRequestStream,
        mut activity: mpsc::Sender<Command>,
        _: Option<Rc<alarms::Loop>>,
    ) -> fasync::Task<()> {
        let (done_tx, _done_rx) = mpsc::channel(1);
        let _ = activity.send(Command::PowerManagement { suspended: false, done: done_tx }).await;
        fasync::Task::local(async move {})
    }

    #[fuchsia::test]
    async fn test_manage_internal() {
        let (g_proxy, mut g_stream) =
            endpoints::create_proxy_and_stream::<fps::ActivityGovernorMarker>();
        let (_activity_s, mut activity_r) = mpsc::channel::<Command>(1);

        // Run the server side activity governor.
        fasync::Task::local(async move {
            while let Some(request) = g_stream.next().await {
                match request {
                    Ok(fps::ActivityGovernorRequest::RegisterSuspendBlocker {
                        payload: _,
                        responder,
                    }) => {
                        let (_unused_server_token, client_token) = fps::LeaseToken::create();
                        responder.send(Ok(client_token)).expect("never fails");
                    }
                    Ok(_) | Err(_) => unimplemented!(),
                }
            }
            debug!("governor server side test exiting");
        })
        .detach();

        fasync::Task::local(async move {
            manage_internal(g_proxy, _activity_s, None, empty_loop).await.unwrap().await;
        })
        .detach();

        activity_r.next().await.unwrap();
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn before_suspend_flushes_alarms_after_clock_suspend() {
        fasync::TestExecutor::advance_to(fasync::MonotonicInstant::from_nanos(0)).await;
        let scope = fasync::Scope::new();
        let (suspend_blocker_client, suspend_blocker_server) =
            create_endpoints::<fps::SuspendBlockerMarker>();
        let suspend_blocker = suspend_blocker_server.into_stream();
        let suspend_blocker_proxy = suspend_blocker_client.into_proxy();
        let (activity_s, mut activity_r) = mpsc::channel::<Command>(1);

        let (hrtimer_proxy, mut hrtimer_stream) =
            endpoints::create_proxy_and_stream::<fidl_fuchsia_hardware_hrtimer::DeviceMarker>();
        let events = Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
        let events_hrtimer = events.clone();
        scope.spawn_local(async move {
            let mut pending_responders = Vec::new();
            while let Some(Ok(req)) = hrtimer_stream.next().await {
                match req {
                    fidl_fuchsia_hardware_hrtimer::DeviceRequest::GetProperties { responder } => {
                        responder
                            .send(fidl_fuchsia_hardware_hrtimer::Properties {
                                timers_properties: Some(vec![
                                    fidl_fuchsia_hardware_hrtimer::TimerProperties {
                                        id: Some(0),
                                        supported_resolutions: Some(vec![
                                            fidl_fuchsia_hardware_hrtimer::Resolution::Duration(
                                                1_000_000_000,
                                            ),
                                        ]),
                                        max_ticks: Some(1000),
                                        supports_wait: Some(true),
                                        ..Default::default()
                                    },
                                ]),
                                ..Default::default()
                            })
                            .unwrap();
                    }
                    fidl_fuchsia_hardware_hrtimer::DeviceRequest::StartAndWait {
                        ticks,
                        setup_event,
                        responder,
                        ..
                    } => {
                        events_hrtimer.borrow_mut().push(if ticks == 2 {
                            "hrtimer_arm_initial"
                        } else {
                            "hrtimer_reprogram_before_suspend"
                        });
                        setup_event.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).unwrap();
                        pending_responders.push(responder);
                    }
                    fidl_fuchsia_hardware_hrtimer::DeviceRequest::Stop { responder, .. } => {
                        events_hrtimer.borrow_mut().push("hrtimer_stop");
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("unexpected hrtimer request: {other:?}"),
                }
            }
        });

        let (governor_proxy, mut governor_stream) =
            endpoints::create_proxy_and_stream::<fps::ActivityGovernorMarker>();
        let server_tokens = Rc::new(std::cell::RefCell::new(Vec::<zx::EventPair>::new()));
        let tokens_for_sag = server_tokens.clone();
        scope.spawn_local(async move {
            while let Some(Ok(req)) = governor_stream.next().await {
                match req {
                    fps::ActivityGovernorRequest::AcquireWakeLease { responder, .. } => {
                        let (server_token, client_token) = zx::EventPair::create();
                        tokens_for_sag.borrow_mut().push(server_token);
                        responder.send(Ok(client_token)).unwrap();
                    }
                    other => panic!("unexpected ActivityGovernor request: {other:?}"),
                }
            }
        });

        let utc_clock = fuchsia_runtime::UtcClock::create(
            zx::ClockOpts::empty(),
            Some(fuchsia_runtime::UtcInstant::from_nanos(1_000)),
        )
        .unwrap();
        let inspector = fuchsia_inspect::Inspector::default();
        let timer_loop = Rc::new(alarms::Loop::new(
            scope.to_handle(),
            hrtimer_proxy,
            inspector.root().create_child("wake_alarms"),
            utc_clock,
            Some(governor_proxy),
        ));

        let (wake_proxy, wake_stream) =
            endpoints::create_proxy_and_stream::<fidl_fuchsia_time_alarms::WakeAlarmsMarker>();
        let tl_serve = timer_loop.clone();
        scope.spawn_local(async move {
            alarms::serve(tl_serve, wake_stream).await;
        });

        // 1 Hz hrtimer rounds 1.2s up to 2 ticks, so `overdue_head` becomes overdue
        // at t = 1.5s while hardware has not fired yet.
        let setup_1 = zx::Event::create();
        let alarm_1 = wake_proxy.set_and_wait(
            fidl::BootInstant::from_nanos(1_200_000_000),
            fidl_fuchsia_time_alarms::SetMode::NotifySetupDone(
                setup_1.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            ),
            "overdue_head",
        );
        fasync::OnSignals::new(&setup_1, zx::Signals::EVENT_SIGNALED).await.unwrap();

        let setup_2 = zx::Event::create();
        let _alarm_2 = wake_proxy.set_and_wait(
            fidl::BootInstant::from_nanos(30_000_000_000),
            fidl_fuchsia_time_alarms::SetMode::NotifySetupDone(
                setup_2.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            ),
            "next_wake",
        );
        fasync::OnSignals::new(&setup_2, zx::Signals::EVENT_SIGNALED).await.unwrap();

        fasync::TestExecutor::advance_to(fasync::MonotonicInstant::from_nanos(1_500_000_000)).await;

        let events_pm = events.clone();
        scope.spawn_local(async move {
            while let Some(cmd) = activity_r.next().await {
                match cmd {
                    Command::PowerManagement { suspended: true, mut done } => {
                        events_pm.borrow_mut().push("clock_manager_suspended");
                        done.send(()).await.unwrap();
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            }
        });

        let mgmt_task = management_loop(suspend_blocker, activity_s, Some(timer_loop)).await;

        suspend_blocker_proxy.before_suspend().await.unwrap();
        assert_eq!(
            *events.borrow(),
            vec![
                "hrtimer_arm_initial",
                "clock_manager_suspended",
                "hrtimer_stop",
                "hrtimer_reprogram_before_suspend",
            ]
        );
        let client_lease = alarm_1.await.unwrap().unwrap();
        assert_eq!(server_tokens.borrow().len(), 1);
        assert!(matches!(
            server_tokens.borrow()[0]
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(_)
        ));
        drop(client_lease);
        assert!(matches!(
            server_tokens.borrow()[0]
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(_)
        ));

        drop(_alarm_2);
        drop(wake_proxy);
        drop(suspend_blocker_proxy);
        mgmt_task.await;
        scope.cancel().await;
    }
}
