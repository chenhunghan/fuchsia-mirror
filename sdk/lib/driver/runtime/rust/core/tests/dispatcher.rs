// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration tests for driver runtime dispatchers.

use std::sync::mpsc;

use fdf_core::dispatcher::{DispatcherBuilder, DriverDispatcherRef, OnDriverDispatcher};
use fdf_env::test::with_driver_dispatcher;
use fdf_env::{Environment, SuspendCompleter};
use futures::channel::mpsc as async_mpsc;
use futures::{SinkExt, StreamExt};
use libasync_dispatcher::{AsAsyncDispatcherRef, AsyncDispatcher, OnDispatcher};
use zx::Status;

const NO_SYNC_CALLS_ROLE: &str = "no sync calls role";

/// Runs `p` on the current thread with a new root dispatcher that allows sync calls.
fn with_dispatcher<T>(name: &str, p: impl FnOnce(AsyncDispatcher) -> T) -> T {
    with_driver_dispatcher(
        name,
        |_env, dispatcher| dispatcher.allow_thread_blocking(),
        |_driver, dispatcher| p(dispatcher),
    )
}

/// Runs `p` on the current thread with a new root dispatcher that does not allow thread migration.
fn with_no_thread_migration_dispatcher<T>(name: &str, p: impl FnOnce(AsyncDispatcher) -> T) -> T {
    with_driver_dispatcher(
        name,
        |env, dispatcher| {
            env.set_scheduler_role_opts(
                NO_SYNC_CALLS_ROLE,
                Environment::SCHEDULER_ROLE_OPTION_NO_SYNC_CALLS,
            )
            .unwrap();
            dispatcher.scheduler_role(NO_SYNC_CALLS_ROLE).no_thread_migration()
        },
        |_driver, dispatcher| p(dispatcher),
    )
}

#[test]
fn start_test_dispatcher() {
    with_dispatcher("testing", |dispatcher| {
        println!("hello {dispatcher:?}");
    })
}

#[test]
fn scheduler_role() {
    // Non-empty roles are covered by the runtime's own `DispatcherTest.GetSchedulerRole`;
    // creating a dispatcher with a role here hits b/488397193.
    with_dispatcher("no role", |dispatcher| {
        let dispatcher =
            DriverDispatcherRef::from_async_dispatcher(dispatcher.as_async_dispatcher_ref());
        assert_eq!(dispatcher.scheduler_role(), "");
        assert_eq!(dispatcher.always_on_dispatcher().scheduler_role(), "");
    });
}

#[test]
fn post_task_on_dispatcher() {
    with_dispatcher("testing task", |dispatcher| {
        let (tx, rx) = mpsc::channel();
        dispatcher
            .post_task_sync(move |status| {
                assert_eq!(status, Ok(()));
                tx.send(status).unwrap();
            })
            .unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(()));
    });
}

#[test]
fn post_task_on_subdispatcher() {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    with_dispatcher("testing task top level", move |dispatcher| {
        let (tx, rx) = mpsc::channel();
        let (inner_tx, inner_rx) = mpsc::channel();
        dispatcher
            .post_task_sync(move |status| {
                assert_eq!(status, Ok(()));
                let inner = DispatcherBuilder::new()
                    .name("testing task second level")
                    .scheduler_role("")
                    .allow_thread_blocking()
                    .shutdown_observer(move |_dispatcher| {
                        println!("shutdown observer called");
                        shutdown_tx.send(1).unwrap();
                    })
                    .create()
                    .unwrap();
                inner
                    .post_task_sync(move |status| {
                        assert_eq!(status, Ok(()));
                        tx.send(status).unwrap();
                    })
                    .unwrap();
                // we want to make sure the inner dispatcher lives long
                // enough to run the task, so we sent it out to the outer
                // closure.
                inner_tx.send(inner).unwrap();
            })
            .unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(()));
        inner_rx.recv().unwrap();
    });
    assert_eq!(shutdown_rx.recv().unwrap(), 1);
}

#[test]
fn spawn_local_fails_on_normal_dispatcher() {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    with_dispatcher("spawn local failures", move |dispatcher| {
        let inside_dispatcher = dispatcher.clone();
        dispatcher.spawn(async move {
            assert_eq!(
                inside_dispatcher.spawn_local(futures::future::ready(())).await.unwrap_err(),
                Status::BAD_STATE
            );
            assert_eq!(
                inside_dispatcher.compute_local(futures::future::ready(())).await.unwrap_err(),
                Status::BAD_STATE
            );
            shutdown_tx.send(()).unwrap();
        });
        shutdown_rx.recv().unwrap();
    });
}

#[test]
#[ignore = "Pending resolution of b/488397193"]
fn spawn_local_succeeds_on_no_thread_migration_dispatcher() {
    let (tx, rx) = mpsc::channel();
    with_no_thread_migration_dispatcher("spawn local success", move |dispatcher| {
        let inside_dispatcher = dispatcher.clone();
        dispatcher.spawn(async move {
            let tx_clone = tx.clone();
            inside_dispatcher.spawn_local(async move {
                tx_clone.send(()).unwrap();
            });
            inside_dispatcher
                .compute_local(async move {
                    tx.send(()).unwrap();
                })
                .await
                .unwrap();
        });
        // one empty object received each for spawn and compute _local.
        rx.recv().unwrap();
        rx.recv().unwrap();
    });
}

#[test]
#[ignore = "Pending resolution of b/488397193"]
fn spawn_local_fails_on_no_thread_migration_dispatcher_from_different_thread() {
    with_no_thread_migration_dispatcher("spawn local success", move |dispatcher| {
        let mut executor = fuchsia_async::LocalExecutor::default();
        executor.run_singlethreaded(async {
            // we are not currently running in any driver dispatcher here, so this is a
            // context where the 'current dispatcher' is definitely not the one in question.
            assert_eq!(
                dispatcher.spawn_local(futures::future::ready(())).await.unwrap_err(),
                Status::BAD_STATE
            );
            assert_eq!(
                dispatcher.compute_local(futures::future::ready(())).await.unwrap_err(),
                Status::BAD_STATE
            );
        });
    });
}

async fn ping(mut tx: async_mpsc::Sender<u8>, mut rx: async_mpsc::Receiver<u8>) {
    println!("starting ping!");
    tx.send(0).await.unwrap();
    while let Some(next) = rx.next().await {
        println!("ping! {next}");
        tx.send(next + 1).await.unwrap();
    }
}

async fn pong(
    fin_tx: std::sync::mpsc::Sender<()>,
    mut tx: async_mpsc::Sender<u8>,
    mut rx: async_mpsc::Receiver<u8>,
) {
    println!("starting pong!");
    while let Some(next) = rx.next().await {
        println!("pong! {next}");
        if next > 10 {
            println!("bye!");
            break;
        }
        tx.send(next + 1).await.unwrap();
    }
    fin_tx.send(()).unwrap();
}

#[test]
fn async_ping_pong() {
    with_dispatcher("async ping pong", |dispatcher| {
        let (fin_tx, fin_rx) = mpsc::channel();
        let (ping_tx, pong_rx) = async_mpsc::channel(10);
        let (pong_tx, ping_rx) = async_mpsc::channel(10);
        dispatcher.spawn(ping(ping_tx, ping_rx));
        dispatcher.spawn(pong(fin_tx, pong_tx, pong_rx));

        fin_rx.recv().expect("to receive final value");
    });
}

async fn slow_pong(
    fin_tx: std::sync::mpsc::Sender<()>,
    mut tx: async_mpsc::Sender<u8>,
    mut rx: async_mpsc::Receiver<u8>,
) {
    use zx::MonotonicDuration;
    println!("starting pong!");
    while let Some(next) = rx.next().await {
        println!("pong! {next}");
        fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
            MonotonicDuration::from_seconds(1),
        ))
        .await;
        if next > 10 {
            println!("bye!");
            break;
        }
        tx.send(next + 1).await.unwrap();
    }
    fin_tx.send(()).unwrap();
}

#[test]
fn mixed_executor_async_ping_pong() {
    with_dispatcher("async ping pong", |dispatcher| {
        let (fin_tx, fin_rx) = mpsc::channel();
        let (ping_tx, pong_rx) = async_mpsc::channel(10);
        let (pong_tx, ping_rx) = async_mpsc::channel(10);

        // spawn ping on the driver dispatcher
        dispatcher.spawn(ping(ping_tx, ping_rx));

        // and run pong on the fuchsia_async executor
        let mut executor = fuchsia_async::LocalExecutor::default();
        executor.run_singlethreaded(slow_pong(fin_tx, pong_tx, pong_rx));

        fin_rx.recv().expect("to receive final value");
    });
}

#[test]
fn wake_vector_registration() {
    with_dispatcher("wake vector test", |dispatcher| {
        let dispatcher_ref =
            DriverDispatcherRef::from_async_dispatcher(dispatcher.as_async_dispatcher_ref());
        let event = zx::Event::create();
        let signals = zx::Signals::USER_0;

        // Register and explicitly unregister.
        let reg = dispatcher_ref.register_wake_vector(&event, signals).unwrap();
        assert_eq!(reg.unregister(), Ok(()));

        // Unregistering with empty signals removes the handle's wake vector entirely, so a
        // second registration for the same handle reports NOT_FOUND.
        let reg_all = dispatcher_ref.register_wake_vector(&event, zx::Signals::empty()).unwrap();
        let reg_user0 = dispatcher_ref.register_wake_vector(&event, signals).unwrap();
        assert_eq!(reg_all.unregister(), Ok(()));
        assert_eq!(reg_user0.unregister(), Err(Status::NOT_FOUND));

        // Test automatic unregister on drop via OnDriverDispatcher.
        {
            let _reg_drop = dispatcher.register_wake_vector(&event, signals).unwrap();
        }
    });
}

#[test]
fn always_on_dispatcher_runs_task_and_irq_while_suspended() {
    with_driver_dispatcher(
        "always_on_test",
        |_env, dispatcher| dispatcher.allow_thread_blocking(),
        |driver, dispatcher| {
            let always_on = dispatcher.always_on_dispatcher();

            let (suspend_tx, suspend_rx) = mpsc::channel();
            driver.driver_suspend(SuspendCompleter::new(move || suspend_tx.send(()).unwrap()));
            suspend_rx.recv().unwrap();

            let (normal_tx, normal_rx) = mpsc::channel();
            dispatcher.spawn(async move {
                normal_tx.send(()).unwrap();
            });

            let (always_on_tx, always_on_rx) = mpsc::channel();
            let irq = zx::Interrupt::create_virtual().unwrap();
            let mut on_irq = libasync::OnInterrupt::new_on(
                always_on.clone(),
                irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            );
            always_on.spawn(async move {
                let _ = on_irq.next().await.unwrap().unwrap();
                always_on_tx.send(()).unwrap();
            });
            irq.trigger(zx::BootInstant::from_nanos(1234)).unwrap();

            // Always-on task and interrupt run while the driver dispatcher is suspended.
            always_on_rx.recv().unwrap();
            // Normal task remains queued on sleep_queue_ while suspended.
            assert!(normal_rx.try_recv().is_err());

            driver.driver_resume();
            normal_rx.recv().unwrap();
        },
    );
}
