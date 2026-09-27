// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::driver::Driver;
use crate::utils::update_process_name;
use anyhow::{Context, Result};
use fidl::encoding::{DefaultFuchsiaResourceDialect, clear_tls_buf};
use fidl::endpoints::{ClientEnd, ServerEnd};
use fidl_fuchsia_driver_framework as fidl_fdf;
use fidl_fuchsia_driver_host as fdh;
use fidl_fuchsia_ldsvc as fldsvc;
use fidl_fuchsia_system_state as fss;
use fuchsia_async as fasync;
use fuchsia_async::Timer;
use fuchsia_component::client;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::{StreamExt, TryStreamExt};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use zx::Status;

/// Any stored data is removed after this amount of time
const EXCEPTIONS_CLEANUP_DEADLINE_SECONDS: i64 = 600;

/// Maximum number of threads, dispatchers, and queued tasks to include in diagnostic dumps so that
/// a batch of 4 DriverHostInfo entries in DriverHostInfoIterator::GetNext stays well below the
/// 64 KiB channel message size limit.
const MAX_THREADS: usize = 32;
const MAX_DISPATCHERS: usize = 32;
const MAX_QUEUED_TASKS_PER_DISPATCHER: usize = 16;
const MAX_TOTAL_QUEUED_TASKS: usize = 32;

/// We use Weak<Driver> to avoid accidentally extending the lifetime of the Driver. Driver must be
/// droped and have it's destroy hook called in the driver runtime's shutdown observer callback in
/// order to comply with the guarantees the driver framework provides for drivers. The driver host
/// itself doesn't actually ever access the drivers, it strictly uses it for debugging and keeping
/// track of when to shut doesn the driver host.
struct WeakDriver(Weak<Driver>);

impl Ord for WeakDriver {
    fn cmp(&self, other: &WeakDriver) -> std::cmp::Ordering {
        (self.0.as_ptr() as usize).cmp(&(other.0.as_ptr() as usize))
    }
}

impl PartialOrd for WeakDriver {
    fn partial_cmp(&self, other: &WeakDriver) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for WeakDriver {
    fn eq(&self, other: &Self) -> bool {
        (self.0.as_ptr() as usize).eq(&(other.0.as_ptr() as usize))
    }
}

impl Eq for WeakDriver {}

#[derive(Debug)]
pub(crate) struct ExceptionRecord {
    // The point at which this record should be deleted
    deadline: zx::MonotonicInstant,
    // The koid of the thread an exception was observed on
    koid: zx::Koid,
    // The driver info that was associated with the exception
    info: fdh::DriverCrashInfo,
}

pub(crate) struct DriverHost {
    env: fdf_env::Environment,
    drivers: RefCell<BTreeSet<WeakDriver>>,
    no_more_drivers_signaler: RefCell<Option<oneshot::Sender<()>>>,
    exceptions: RefCell<Vec<ExceptionRecord>>,
    scope: fuchsia_async::Scope,
}

impl DriverHost {
    pub fn new(
        env: fdf_env::Environment,
        no_more_drivers_signaler: oneshot::Sender<()>,
    ) -> DriverHost {
        DriverHost {
            env,
            drivers: RefCell::new(BTreeSet::new()),
            no_more_drivers_signaler: RefCell::new(Some(no_more_drivers_signaler)),
            exceptions: RefCell::new(Vec::new()),
            scope: fuchsia_async::Scope::new(),
        }
    }

    pub async fn run_driver_host_server(self: Rc<Self>, stream: fdh::DriverHostRequestStream) {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each_concurrent(None, |request| {
                let this = self.clone();
                async move {
                    match request {
                        fdh::DriverHostRequest::Start {
                            start_args,
                            driver,
                            host_name,
                            responder,
                        } => {
                            responder
                                .send(this.start_driver(start_args, driver, &host_name).await)
                                .or_else(ignore_peer_closed)?;
                        }
                        fdh::DriverHostRequest::StartLoadedDriver {
                            start_args,
                            dynamic_linking_abi,
                            driver,
                            responder,
                        } => {
                            responder
                                .send(
                                    this.start_loaded_driver(
                                        start_args,
                                        dynamic_linking_abi,
                                        driver,
                                    )
                                    .await,
                                )
                                .or_else(ignore_peer_closed)?;
                        }
                        fdh::DriverHostRequest::GetProcessInfo { responder } => {
                            let res = this.get_process_info();
                            let res_ref = res
                                .as_ref()
                                .map(|info| {
                                    (
                                        info.job_koid,
                                        info.process_koid,
                                        info.main_thread_koid,
                                        info.threads.as_slice(),
                                        info.dispatchers.as_slice(),
                                    )
                                })
                                .map_err(|e| *e);
                            if let Err(e) = responder.send(res_ref).or_else(ignore_peer_closed) {
                                log::warn!("Failed to send GetProcessInfo response: {e}");
                            }
                        }
                        fdh::DriverHostRequest::InstallLoader { loader, .. } => {
                            install_loader(loader);
                        }
                        fdh::DriverHostRequest::TriggerStackTrace { .. } => {
                            fasync::unblock(|| {
                                debug::backtrace_request_all_threads();
                            })
                            .await;
                        }
                        fdh::DriverHostRequest::FindDriverCrashInfoByThreadKoid {
                            thread_koid,
                            responder,
                        } => {
                            let info = this
                                .take_exception_by_thread_koid(&zx::Koid::from_raw(thread_koid));
                            responder
                                .send(info.ok_or_else(|| Status::NOT_FOUND.into_raw()))
                                .or_else(ignore_peer_closed)?;
                        }
                    }
                    clear_tls_buf::<DefaultFuchsiaResourceDialect>();
                    let _ = scudo::mallopt(scudo::M_PURGE_ALL, 0);
                    Ok(())
                }
            })
            .await
            .expect("Failed to handle request")
    }

    pub fn run_exception_listener(self: Rc<Self>) {
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            let mut exceptions = task_exceptions::ExceptionsStream::register_with_task(
                &*fuchsia_runtime::process_self(),
            )
            .expect("To create exception stream on process.");
            loop {
                match exceptions.try_next().await {
                    Ok(Some(exception_info)) => {
                        if let Some(this) = this.upgrade() {
                            this.add_exception(exception_info);
                        } else {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::error!("failed to read exception stream: {}", error);
                        break;
                    }
                }
            }
        });
    }

    pub fn run_exception_cleanup_task(self: Rc<Self>) {
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            loop {
                let sleep_until = this.upgrade().map(|s| {
                    if s.exceptions.borrow().is_empty() {
                        // If we have no records, then we can sleep for as long as the timeout and
                        // check again
                        zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
                            EXCEPTIONS_CLEANUP_DEADLINE_SECONDS,
                        ))
                    } else {
                        s.exceptions.borrow()[0].deadline
                    }
                });

                let Some(sleep_until) = sleep_until else {
                    break;
                };

                let timer = Timer::new(sleep_until);
                timer.await;

                let Some(this) = this.upgrade() else {
                    break;
                };

                let mut exceptions = this.exceptions.borrow_mut();
                while !exceptions.is_empty() && zx::MonotonicInstant::get() > exceptions[0].deadline
                {
                    exceptions.remove(0);
                }
            }
        });
    }

    pub fn run_thread_monitor_task(&self) {
        let (tx, mut rx) = mpsc::channel(0);
        let tx = Mutex::new(tx);

        let scanner = fdf_env::StallScanner::new(move |duration| {
            // We don't log errors here as it can be excessive if the
            // rx handling is slow. We don't expect the other side close either.
            let _ = tx.lock().try_send(duration);
        });
        self.env.register_stall_scanner(scanner);

        self.scope.spawn_local(async move {
            loop {
                // Drain queue.
                while rx.try_recv().is_ok() {}

                // SAFETY: this call does not use any memory allocated by rust and only does
                // anything if the fdf_env is currently set up, otherwise it does nothing.
                let mut next_wait = unsafe { fdf_sys::fdf_env_scan_threads_for_stalls2() };
                if next_wait == 0 {
                    // Wait for start.
                    let Some(duration) = rx.next().await else {
                        break;
                    };
                    next_wait = duration;
                }
                Timer::new(zx::MonotonicDuration::from_nanos(next_wait)).await;
            }
        });
    }

    async fn start_driver(
        self: Rc<Self>,
        start_args: fidl_fdf::DriverStartArgs,
        request: ServerEnd<fdh::DriverMarker>,
        host_name: &str,
    ) -> Result<(), i32> {
        let (driver, start_args) =
            Driver::load(&self.env, start_args).await.map_err(Status::into_raw)?;
        let (shutdown_signaler, shutdown_event) = oneshot::channel();

        // We carry a weak reference to avoid accidentally extending the lifetime of the
        // driver_host.
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            let driver = shutdown_event.await.unwrap();
            if let Some(this) = this.upgrade() {
                let is_empty = {
                    let mut drivers = this.drivers.borrow_mut();
                    drivers.remove(&WeakDriver(driver));
                    drivers.is_empty()
                };

                // If this is the last driver instance running, we should exit.
                if is_empty {
                    // We only exit if we're not shutting down in order to match DFv1 behavior.
                    // TODO(https://fxbug.dev/42075187): We should always exit driver hosts when we
                    // get down to 0 drivers.
                    let client = client::connect_to_protocol::<fss::SystemStateTransitionMarker>()
                        .expect("Could not connect to SystemStateTransition protocol.");
                    match client.get_termination_system_state().await {
                        Err(_) | Ok(fss::SystemPowerState::FullyOn) => (),
                        _ => return,
                    };

                    if let Some(signaler) = this.no_more_drivers_signaler.borrow_mut().take() {
                        signaler.send(()).unwrap();
                    }
                }
            }
        });

        driver
            .start(start_args, request, shutdown_signaler, &self.scope)
            .await
            .map_err(Status::into_raw)?;
        update_process_name(host_name, driver.get_url(), self.drivers.borrow().len());
        self.drivers.borrow_mut().insert(WeakDriver(Arc::downgrade(&driver)));

        Ok(())
    }

    async fn start_loaded_driver(
        self: Rc<Self>,
        start_args: fidl_fdf::DriverStartArgs,
        dynamic_linking_abi: u64,
        request: ServerEnd<fdh::DriverMarker>,
    ) -> Result<(), i32> {
        let (driver, start_args) = Driver::initialize(&self.env, start_args, dynamic_linking_abi)
            .await
            .map_err(Status::into_raw)?;
        let (shutdown_signaler, shutdown_event) = oneshot::channel();

        // We carry a weak reference to avoid accidentally extending the lifetime of the
        // driver_host.
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            let driver = shutdown_event.await.unwrap();
            if let Some(this) = this.upgrade() {
                let is_empty = {
                    let mut drivers = this.drivers.borrow_mut();
                    drivers.remove(&WeakDriver(driver));
                    drivers.is_empty()
                };

                // If this is the last driver instance running, we should exit.
                if is_empty
                    && let Some(signaler) = this.no_more_drivers_signaler.borrow_mut().take()
                {
                    signaler.send(()).unwrap();
                }
            }
        });

        driver
            .start(start_args, request, shutdown_signaler, &self.scope)
            .await
            .map_err(Status::into_raw)?;
        update_process_name("", driver.get_url(), self.drivers.borrow().len());
        self.drivers.borrow_mut().insert(WeakDriver(Arc::downgrade(&driver)));
        Ok(())
    }

    fn add_exception(&self, info: task_exceptions::ExceptionInfo) {
        let Ok(thread_koid) = info.thread.koid() else {
            log::error!("failed to get the exception thread's koid.");
            return;
        };
        let driver_on_thread_koid = self.env.get_driver_on_thread_koid(thread_koid);

        // No driver on exception thread indicates the exception occurred in the driver runtime.
        // Forensics can just fallback to component attribution and report the driver host.
        if let Some(driver_ref) = driver_on_thread_koid {
            let found = self.drivers.borrow().iter().find_map(|d| {
                let locked_driver = d.0.upgrade();
                if let Some(driver) = locked_driver
                    && *driver == driver_ref
                {
                    return Some(driver);
                }

                None
            });

            if let Some(found) = found {
                let mut exceptions = self.exceptions.borrow_mut();
                let crash_info = fdh::DriverCrashInfo {
                    url: Some(found.get_url().to_string()),
                    node_token: found.duplicate_node_token(),
                    ..Default::default()
                };
                exceptions.push(ExceptionRecord {
                    deadline: zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
                        EXCEPTIONS_CLEANUP_DEADLINE_SECONDS,
                    )),
                    koid: thread_koid,
                    info: crash_info,
                });
                log::error!("Driver exception in driver host: Driver url: {}", found.get_url());
            } else {
                log::warn!(
                    "{} {}",
                    "Failed to validate driver with the driver host.",
                    "This might indicate exceptions in the driver's Stop() or destructor."
                );
            }
        }
    }

    fn take_exception_by_thread_koid(
        &self,
        thread_koid: &zx::Koid,
    ) -> Option<fdh::DriverCrashInfo> {
        let mut exceptions = self.exceptions.borrow_mut();

        let index_to_remove = exceptions
            .iter()
            .enumerate()
            .find_map(|(i, exception)| if &exception.koid == thread_koid { Some(i) } else { None });

        index_to_remove.map(|i| exceptions.remove(i).info)
    }

    fn get_process_info(&self) -> Result<fdh::ProcessInfo, i32> {
        let job_koid =
            fuchsia_runtime::job_default().koid().map_err(zx::Status::into_raw)?.raw_koid();
        let process_koid =
            fuchsia_runtime::process_self().koid().map_err(Status::into_raw)?.raw_koid();
        let main_thread_koid = fuchsia_runtime::with_thread_self(|thread| {
            thread.koid().map_err(zx::Status::into_raw)
        })?
        .raw_koid();

        let thread_koids = fuchsia_runtime::process_self().threads().unwrap_or_default();
        let mut fdf_threads: HashMap<u64, (String, String)> = self
            .env
            .dump_all_threads()
            .into_iter()
            .map(|t| (t.koid, (t.name, t.scheduler_role)))
            .collect();
        let mut threads = Vec::new();
        let live_fdf_count =
            thread_koids.iter().filter(|k| fdf_threads.contains_key(&k.raw_koid())).count();
        let mut non_fdf_budget = MAX_THREADS.saturating_sub(live_fdf_count);
        for koid in &thread_koids {
            if threads.len() >= MAX_THREADS {
                break;
            }
            if let Some((name, scheduler_role)) = fdf_threads.remove(&koid.raw_koid()) {
                threads.push(fdh::ThreadInfo { koid: koid.raw_koid(), name, scheduler_role });
            } else if non_fdf_budget > 0 {
                non_fdf_budget -= 1;
                let name = fuchsia_runtime::process_self()
                    .get_child(koid, zx::Rights::SAME_RIGHTS)
                    .and_then(|handle| handle.get_name())
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                threads.push(fdh::ThreadInfo {
                    koid: koid.raw_koid(),
                    name,
                    scheduler_role: String::new(),
                });
            }
        }
        if threads.len() < MAX_THREADS && !fdf_threads.is_empty() {
            let mut remaining_fdf: Vec<_> = fdf_threads.into_iter().collect();
            remaining_fdf.sort_by_key(|(koid, _)| *koid);
            for (koid, (name, scheduler_role)) in remaining_fdf {
                if threads.len() >= MAX_THREADS {
                    break;
                }
                if thread_koids.is_empty()
                    || fuchsia_runtime::process_self()
                        .get_child(&zx::Koid::from_raw(koid), zx::Rights::SAME_RIGHTS)
                        .is_ok()
                {
                    threads.push(fdh::ThreadInfo { koid, name, scheduler_role });
                }
            }
        }

        let entries = self.env.dump_all_dispatchers();
        let driver_urls: HashMap<u64, String> = self
            .drivers
            .borrow()
            .iter()
            .filter_map(|d| {
                d.0.upgrade().map(|drv| (d.0.as_ptr() as u64, drv.get_url().to_string()))
            })
            .collect();
        let dispatcher_names: HashMap<u64, String> =
            entries.iter().map(|e| (e.dispatcher_ptr, e.name.clone())).collect();

        let mut total_queued_tasks_remaining = MAX_TOTAL_QUEUED_TASKS;
        let mut dispatchers = Vec::new();
        for entry in entries.into_iter().take(MAX_DISPATCHERS) {
            let driver = driver_urls.get(&entry.driver).cloned().unwrap_or_default();
            let state = match entry.state {
                fdf_env::DispatcherState::Running => fdh::DispatcherState::Running,
                fdf_env::DispatcherState::ShuttingDown => fdh::DispatcherState::ShuttingDown,
                fdf_env::DispatcherState::Shutdown => fdh::DispatcherState::Shutdown,
                fdf_env::DispatcherState::Destroyed => fdh::DispatcherState::Destroyed,
            };
            let (has_destroy_user_initiated, destroy_user_initiated) =
                match entry.destroy_user_initiated {
                    Some(val) => (true, val),
                    None => (false, false),
                };
            let num_queued_tasks = entry.queued_tasks.len() as u64;
            let limit = MAX_QUEUED_TASKS_PER_DISPATCHER.min(total_queued_tasks_remaining);
            let queued_tasks: Vec<fdh::QueuedTaskInfo> = entry
                .queued_tasks
                .into_iter()
                .take(limit)
                .map(|task| {
                    let initiating_dispatcher_name = dispatcher_names
                        .get(&task.initiating_dispatcher)
                        .cloned()
                        .unwrap_or_default();
                    let initiating_driver_url =
                        driver_urls.get(&task.initiating_driver).cloned().unwrap_or_default();
                    fdh::QueuedTaskInfo {
                        ptr: task.ptr,
                        handler: task.handler,
                        initiating_dispatcher: task.initiating_dispatcher,
                        initiating_dispatcher_name,
                        initiating_driver: task.initiating_driver,
                        initiating_driver_url,
                    }
                })
                .collect();
            total_queued_tasks_remaining -= queued_tasks.len();
            dispatchers.push(fdh::DispatcherInfo {
                driver,
                name: entry.name,
                options: entry.options,
                scheduler_role: entry.scheduler_role,
                dispatcher_ptr: entry.dispatcher_ptr,
                driver_ptr: entry.driver,
                synchronized: entry.synchronized,
                allow_sync_calls: entry.allow_sync_calls,
                state,
                destroy_context: entry.destroy_context,
                has_destroy_user_initiated,
                destroy_user_initiated,
                debug_stats: fdh::DispatcherDebugStats {
                    num_total_requests: entry.debug_stats.num_total_requests,
                    num_inlined_requests: entry.debug_stats.num_inlined_requests,
                    non_inlined: fdh::NonInlinedRequestStats {
                        allow_sync_calls: entry.debug_stats.non_inlined.allow_sync_calls,
                        parallel_dispatch: entry.debug_stats.non_inlined.parallel_dispatch,
                        task: entry.debug_stats.non_inlined.task,
                        unknown_thread: entry.debug_stats.non_inlined.unknown_thread,
                        reentrant: entry.debug_stats.non_inlined.reentrant,
                        channel_wait_not_yet_registered: entry
                            .debug_stats
                            .non_inlined
                            .channel_wait_not_yet_registered,
                        no_thread_migration: entry.debug_stats.non_inlined.no_thread_migration,
                    },
                },
                num_queued_tasks,
                queued_tasks,
            });
        }

        Ok(fdh::ProcessInfo { job_koid, process_koid, main_thread_koid, threads, dispatchers })
    }
}

impl Drop for DriverHost {
    fn drop(&mut self) {
        // All drivers should now be shutdown and stopped.
        // Destroy all dispatchers in case any weren't freed correctly.
        // This will block until all dispatcher callbacks complete.
        self.env.destroy_all_dispatchers();
    }
}

unsafe extern "C" {
    fn dl_set_loader_service(handle: zx::sys::zx_handle_t) -> zx::sys::zx_handle_t;
}

fn install_loader(loader: ClientEnd<fldsvc::LoaderMarker>) {
    let loader_handle = loader.into_channel().into_raw();
    // SAFETY: The old loader implementation should be a valid channel which should be closed after
    // it is swapped out.
    let _old_loader = unsafe { zx::NullableHandle::from_raw(dl_set_loader_service(loader_handle)) };
}

fn ignore_peer_closed(err: fidl::Error) -> Result<(), fidl::Error> {
    if err.is_closed() { Ok(()) } else { Err(err) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf::DispatcherBuilder;

    #[fuchsia::test]
    async fn get_process_info_test() {
        let env = fdf_env::Environment::start(0).unwrap();
        let driver = env.new_driver(&42u32);
        let _dispatcher = driver
            .new_dispatcher(DispatcherBuilder::new().name("test-dispatcher"))
            .unwrap()
            .release();
        let _role_dispatcher = driver
            .new_dispatcher(
                DispatcherBuilder::new()
                    .name("role-dispatcher")
                    .scheduler_role("fuchsia.test.role"),
            )
            .unwrap()
            .release();
        let (tx, _rx) = oneshot::channel();
        let driver_host = DriverHost::new(env, tx);
        let fdh::ProcessInfo { main_thread_koid, threads, dispatchers, .. } =
            driver_host.get_process_info().unwrap();
        assert!(!threads.is_empty());
        assert!(threads.iter().any(|t| t.koid == main_thread_koid));
        assert!(threads.iter().any(|t| t.name.starts_with("fdf-dispatcher-thread-")));
        assert!(threads.iter().any(|t| t.scheduler_role == "fuchsia.test.role"));
        assert_eq!(dispatchers.len(), 2);
        assert!(dispatchers.iter().any(|d| d.name == "test-dispatcher"));
        assert!(
            dispatchers
                .iter()
                .any(|d| d.name == "role-dispatcher" && d.scheduler_role == "fuchsia.test.role")
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        driver.shutdown(move |_| {
            let _ = shutdown_tx.send(());
        });
        shutdown_rx.await.unwrap();
    }
}
