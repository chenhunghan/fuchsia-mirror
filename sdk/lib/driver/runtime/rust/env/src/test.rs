// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers for writing tests using drivers and dispatchers

use std::sync::{Arc, mpsc};

use fdf_core::dispatcher::{AutoReleaseDispatcher, CurrentDispatcher};
use libasync_dispatcher::{AsAsyncDispatcherRef, AsyncDispatcher, OnDispatcher};

use super::*;

/// The value pointed to by the drivers created by these helpers.
const TEST_DRIVER_VALUE: u32 = 0x1337;

/// Runs the given closure under the a new root driver dispatcher. Use [`fdf::CurrentDispatcher`]
/// to reference the dispatcher if needed.
pub fn run_in_driver<T: Send + 'static>(name: &str, p: impl FnOnce() -> T + Send + 'static) -> T {
    run_in_driver_raw(name, true, false, |tx| tx.send(p()).unwrap())
}

/// Runs the given closure under a new root driver dispatcher, with some additional options.
/// Use [`fdf::CurrentDispatcher`] to reference the dispatcher if needed.
pub fn run_in_driver_etc<T: Send + 'static>(
    name: &str,
    allow_thread_blocking: bool,
    unsynchronized: bool,
    p: impl FnOnce() -> T + Send + 'static,
) -> T {
    run_in_driver_raw(name, allow_thread_blocking, unsynchronized, |tx| tx.send(p()).unwrap())
}

/// Runs the given future to completion under the a new root driver dispatcher. Use
/// [`fdf::CurrentDispatcher`] to reference the dispatcher if needed.
pub fn spawn_in_driver<T: Send + 'static>(
    name: &str,
    p: impl Future<Output = T> + Send + 'static,
) -> T {
    run_in_driver_raw(name, true, false, |tx| {
        CurrentDispatcher.spawn(async move { tx.send(p.await).unwrap() });
    })
}

/// Runs the given future to completion under a new root driver dispatcher, with some additional
/// options. Use [`fdf::CurrentDispatcher`] to reference the dispatcher if needed.
pub fn spawn_in_driver_etc<T: Send + 'static>(
    name: &str,
    allow_thread_blocking: bool,
    unsynchronized: bool,
    p: impl Future<Output = T> + Send + 'static,
) -> T {
    run_in_driver_raw(name, allow_thread_blocking, unsynchronized, |tx| {
        CurrentDispatcher.spawn(async move { tx.send(p.await).unwrap() });
    })
}

/// Creates a new driver with a new root dispatcher, and runs the given closure on the current
/// thread with them.
///
/// Unlike [`run_in_driver`], the closure is not run on the dispatcher, so it may block waiting on
/// work done by the dispatcher.
///
/// `configure` may be used to set additional options on the dispatcher before it is created. It is
/// also given the [`Environment`], so that it can e.g. set scheduler role options first. It must not
/// set a shutdown observer.
///
/// `p` is given the driver, which may be used to e.g. suspend and resume its dispatchers, and the
/// root dispatcher. Once `p` returns, all dispatchers owned by the driver are shut down, and this
/// blocks until the driver runtime has finished destroying them.
///
/// # Panics
///
/// This must not be called from a thread managed by the driver runtime.
pub fn with_driver_dispatcher<T>(
    name: &str,
    configure: impl FnOnce(&Environment, DispatcherBuilder) -> DispatcherBuilder,
    p: impl FnOnce(&Driver<u32>, AsyncDispatcher) -> T,
) -> T {
    with_test_driver(name, configure, |driver, dispatcher| {
        p(driver, dispatcher.as_async_dispatcher())
    })
}

fn run_in_driver_raw<T: Send + 'static>(
    name: &str,
    allow_thread_blocking: bool,
    unsynchronized: bool,
    p: impl FnOnce(mpsc::Sender<T>) + Send + 'static,
) -> T {
    with_test_driver(
        name,
        |_env, dispatcher| {
            let dispatcher =
                if allow_thread_blocking { dispatcher.allow_thread_blocking() } else { dispatcher };
            if unsynchronized { dispatcher.unsynchronized() } else { dispatcher }
        },
        |_driver, dispatcher| {
            let (finished_tx, finished_rx) = mpsc::channel();
            dispatcher
                .post_task_sync(move |_| {
                    p(finished_tx);
                })
                .unwrap();
            finished_rx.recv().unwrap()
        },
    )
}

/// Creates a new driver with a root dispatcher configured by `configure`, and calls `p` with them.
/// Once `p` returns, shuts down the driver and waits for the runtime to finish destroying all of
/// its dispatchers.
fn with_test_driver<T>(
    name: &str,
    configure: impl FnOnce(&Environment, DispatcherBuilder) -> DispatcherBuilder,
    p: impl FnOnce(&Driver<u32>, &AutoReleaseDispatcher) -> T,
) -> T {
    let env = Arc::new(Environment::start(0).unwrap());
    let env_clone = env.clone();

    let driver_value: u32 = TEST_DRIVER_VALUE;
    let driver_value_ptr = &driver_value as *const u32;
    let driver = env.new_driver(driver_value_ptr);
    let dispatcher =
        configure(&env, DispatcherBuilder::new().name(name)).shutdown_observer(move |dispatcher| {
            // We verify that the dispatcher has no tasks left queued in it,
            // just because this is testing code.
            assert!(!env_clone.dispatcher_has_queued_tasks(dispatcher.as_dispatcher_ref()));
        });
    let dispatcher = driver.new_dispatcher(dispatcher).unwrap().release();

    let res = p(&driver, &dispatcher);

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    driver.shutdown(move |driver| {
        // SAFETY: driver lives on the stack, it's safe to dereference it.
        assert!(unsafe { *driver.0 } == TEST_DRIVER_VALUE);
        shutdown_tx.send(()).unwrap();
    });
    shutdown_rx.recv().unwrap();

    // The driver shutdown observer is called once every dispatcher shutdown observer for the
    // driver has returned, but the runtime may still be in the middle of destroying the
    // dispatchers. Wait for that to finish, so that the test (and with it, the test process)
    // doesn't finish while the runtime is still tearing them down.
    env.destroy_all_dispatchers();

    res
}
