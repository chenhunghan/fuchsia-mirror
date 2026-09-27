// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::Result;
use args::{PidOrName, ShowCommand};
use flex_fuchsia_driver_development as fdd;
#[cfg(feature = "fdomain")]
use fuchsia_driver_dev_fdomain as fuchsia_driver_dev;
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::Write;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ThreadDetails {
    pub koid: u64,
    pub name: String,
    pub scheduler_role: String,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct NonInlinedStatsDetails {
    pub allow_sync_calls: u64,
    pub parallel_dispatch: u64,
    pub task: u64,
    pub unknown_thread: u64,
    pub reentrant: u64,
    pub channel_wait_not_yet_registered: u64,
    pub no_thread_migration: u64,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatcherDebugStatsDetails {
    pub num_total_requests: u64,
    pub num_inlined_requests: u64,
    pub non_inlined: NonInlinedStatsDetails,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct QueuedTaskDetails {
    pub ptr: u64,
    pub handler: u64,
    pub initiating_dispatcher: u64,
    pub initiating_dispatcher_name: Option<String>,
    pub initiating_driver: u64,
    pub initiating_driver_url: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DispatcherDetails {
    pub driver: String,
    pub name: String,
    pub options: u32,
    pub scheduler_role: String,
    pub dispatcher_ptr: u64,
    pub driver_ptr: u64,
    pub synchronized: bool,
    pub allow_sync_calls: bool,
    pub state: String,
    pub destroy_context: Option<String>,
    pub destroy_user_initiated: Option<bool>,
    pub debug_stats: DispatcherDebugStatsDetails,
    pub num_queued_tasks: u64,
    pub queued_tasks: Vec<QueuedTaskDetails>,
}

const FDF_DISPATCHER_OPTION_UNSYNCHRONIZED: u32 = 1;
const FDF_DISPATCHER_OPTION_ALLOW_SYNC_CALLS: u32 = 2;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DriverHostDetails {
    pub name: Option<String>,
    pub koid: Option<u64>,
    pub drivers: Vec<String>,
    pub devices: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<ThreadDetails>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatchers: Option<Vec<DispatcherDetails>>,
}

fn format_dispatcher_state(state: Option<fdd::DispatcherState>) -> String {
    match state {
        Some(fdd::DispatcherState::Running) => "running".to_string(),
        Some(fdd::DispatcherState::ShuttingDown) => "shutting down".to_string(),
        Some(fdd::DispatcherState::Shutdown) => "shutdown".to_string(),
        Some(fdd::DispatcherState::Destroyed) => "destroyed".to_string(),
        _ => "unknown".to_string(),
    }
}

pub async fn get_driver_host_details(
    cmd: &ShowCommand,
    driver_development_proxy: &fdd::ManagerProxy,
) -> Result<DriverHostDetails> {
    let device_info = fuchsia_driver_dev::get_device_info(
        driver_development_proxy,
        &[],
        /* exact_match= */ false,
    )
    .await?;

    let driver_host_info =
        fuchsia_driver_dev::get_driver_host_info(driver_development_proxy).await?;

    let mut drivers = BTreeSet::new();
    let mut devices = Vec::new();

    let Some(driver_host) = driver_host_info.iter().find(|info| match &cmd.pid_or_name {
        PidOrName::Pid(pid) => info.process_koid == Some(*pid),
        PidOrName::Name(name) => info.name.as_deref() == Some(name.as_str()),
    }) else {
        anyhow::bail!("driver host not found");
    };

    for device in device_info {
        if let Some(koid) = device.driver_host_koid
            && Some(koid) == driver_host.process_koid
        {
            if let Some(url) = device.bound_driver_url {
                drivers.insert(url);
            }
            if let Some(moniker) = device.moniker {
                devices.push(moniker);
            }
        }
    }

    let (threads, dispatchers) = if cmd.runtime {
        let threads = driver_host.threads.as_ref().map(|threads| {
            threads
                .iter()
                .map(|t| ThreadDetails {
                    koid: t.koid.unwrap_or(0),
                    name: t.name.clone().unwrap_or_default(),
                    scheduler_role: t.scheduler_role.clone().unwrap_or_default(),
                })
                .collect()
        });

        let dispatchers = driver_host.dispatchers.as_ref().map(|dispatchers| {
            dispatchers
                .iter()
                .map(|d| {
                    let options = d.options.unwrap_or(0);
                    let synchronized = d
                        .synchronized
                        .unwrap_or((options & FDF_DISPATCHER_OPTION_UNSYNCHRONIZED) == 0);
                    let allow_sync_calls = d
                        .allow_sync_calls
                        .unwrap_or((options & FDF_DISPATCHER_OPTION_ALLOW_SYNC_CALLS) != 0);
                    let debug_stats = d
                        .debug_stats
                        .as_ref()
                        .map(|stats| {
                            let non_inlined = stats
                                .non_inlined
                                .as_ref()
                                .map(|ni| NonInlinedStatsDetails {
                                    allow_sync_calls: ni.allow_sync_calls.unwrap_or(0),
                                    parallel_dispatch: ni.parallel_dispatch.unwrap_or(0),
                                    task: ni.task.unwrap_or(0),
                                    unknown_thread: ni.unknown_thread.unwrap_or(0),
                                    reentrant: ni.reentrant.unwrap_or(0),
                                    channel_wait_not_yet_registered: ni
                                        .channel_wait_not_yet_registered
                                        .unwrap_or(0),
                                    no_thread_migration: ni.no_thread_migration.unwrap_or(0),
                                })
                                .unwrap_or_default();
                            DispatcherDebugStatsDetails {
                                num_total_requests: stats.num_total_requests.unwrap_or(0),
                                num_inlined_requests: stats.num_inlined_requests.unwrap_or(0),
                                non_inlined,
                            }
                        })
                        .unwrap_or_default();
                    let queued_tasks: Vec<QueuedTaskDetails> = d
                        .queued_tasks
                        .as_ref()
                        .map(|tasks| {
                            tasks
                                .iter()
                                .map(|t| QueuedTaskDetails {
                                    ptr: t.ptr.unwrap_or(0),
                                    handler: t.handler.unwrap_or(0),
                                    initiating_dispatcher: t.initiating_dispatcher.unwrap_or(0),
                                    initiating_dispatcher_name: t
                                        .initiating_dispatcher_name
                                        .clone(),
                                    initiating_driver: t.initiating_driver.unwrap_or(0),
                                    initiating_driver_url: t.initiating_driver_url.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let num_queued_tasks = d.num_queued_tasks.unwrap_or(queued_tasks.len() as u64);
                    DispatcherDetails {
                        driver: d.driver.clone().unwrap_or_default(),
                        name: d.name.clone().unwrap_or_default(),
                        options,
                        scheduler_role: d.scheduler_role.clone().unwrap_or_default(),
                        dispatcher_ptr: d.dispatcher_ptr.unwrap_or(0),
                        driver_ptr: d.driver_ptr.unwrap_or(0),
                        synchronized,
                        allow_sync_calls,
                        state: format_dispatcher_state(d.state),
                        destroy_context: d.destroy_context.clone(),
                        destroy_user_initiated: d.destroy_user_initiated,
                        debug_stats,
                        num_queued_tasks,
                        queued_tasks,
                    }
                })
                .collect()
        });

        (threads, dispatchers)
    } else {
        (None, None)
    };

    Ok(DriverHostDetails {
        name: driver_host.name.clone(),
        koid: driver_host.process_koid,
        drivers: drivers.into_iter().collect(),
        devices,
        threads,
        dispatchers,
    })
}

pub async fn show(
    cmd: ShowCommand,
    w: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    let details = get_driver_host_details(&cmd, &driver_development_proxy).await?;

    if let Some(name) = &details.name
        && !name.is_empty()
    {
        writeln!(w, "Name: {name}")?;
    }
    if let Some(koid) = details.koid {
        writeln!(w, "PID:  {koid}")?;
        writeln!(w, "")?;
    }

    writeln!(w, "Drivers:")?;
    for driver in details.drivers {
        writeln!(w, "{:>4}{}", "", driver)?;
    }
    writeln!(w, "")?;
    writeln!(w, "Devices:")?;
    for device in details.devices {
        writeln!(w, "{:>4}{}", "", device)?;
    }

    if let Some(threads) = &details.threads
        && !threads.is_empty()
    {
        writeln!(w, "")?;
        writeln!(w, "Threads:")?;
        for thread in threads {
            if thread.scheduler_role.is_empty() {
                writeln!(w, "{:>4}Koid: {}, Name: {}", "", thread.koid, thread.name)?;
            } else {
                writeln!(
                    w,
                    "{:>4}Koid: {}, Name: {}, Role: {}",
                    "", thread.koid, thread.name, thread.scheduler_role
                )?;
            }
        }
    }

    if let Some(dispatchers) = &details.dispatchers
        && !dispatchers.is_empty()
    {
        writeln!(w, "")?;
        writeln!(w, "Dispatchers:")?;
        for dispatcher in dispatchers {
            writeln!(w, "{:>4}Name: {} ({:#x})", "", dispatcher.name, dispatcher.dispatcher_ptr)?;
            if dispatcher.driver.is_empty() {
                writeln!(w, "{:>8}Driver: ({:#x})", "", dispatcher.driver_ptr)?;
            } else {
                writeln!(
                    w,
                    "{:>8}Driver: {} ({:#x})",
                    "", dispatcher.driver, dispatcher.driver_ptr
                )?;
            }
            writeln!(w, "{:>8}State: {}", "", dispatcher.state)?;
            writeln!(w, "{:>8}Synchronized: {}", "", dispatcher.synchronized)?;
            writeln!(w, "{:>8}Allow sync calls: {}", "", dispatcher.allow_sync_calls)?;
            if !dispatcher.scheduler_role.is_empty() {
                writeln!(w, "{:>8}Scheduler role: {}", "", dispatcher.scheduler_role)?;
            }
            if let Some(context) = &dispatcher.destroy_context
                && !context.is_empty()
            {
                writeln!(w, "{:>8}A call to Destroy() was made by dispatcher: {}", "", context)?;
            }
            if let Some(user_initiated) = dispatcher.destroy_user_initiated {
                writeln!(
                    w,
                    "{:>8}Destroy() was initiated by: {}",
                    "",
                    if user_initiated { "user" } else { "env" }
                )?;
            }
            writeln!(
                w,
                "{:>8}Processed {} requests, {} were inlined",
                "",
                dispatcher.debug_stats.num_total_requests,
                dispatcher.debug_stats.num_inlined_requests
            )?;
            if dispatcher.debug_stats.num_total_requests
                != dispatcher.debug_stats.num_inlined_requests
            {
                writeln!(w, "{:>8}Reasons why requests were not inlined:", "")?;
                let non_inlined = &dispatcher.debug_stats.non_inlined;
                if non_inlined.allow_sync_calls > 0 {
                    writeln!(
                        w,
                        "{:>10}* calling from a non-blocking to a blocking (ALLOW_SYNC_CALLS) dispatcher: {} times",
                        "", non_inlined.allow_sync_calls
                    )?;
                }
                if non_inlined.parallel_dispatch > 0 {
                    writeln!(
                        w,
                        "{:>10}* another thread already dispatching a request: {} times",
                        "", non_inlined.parallel_dispatch
                    )?;
                }
                if non_inlined.task > 0 {
                    writeln!(w, "{:>10}* request was a task: {} times", "", non_inlined.task)?;
                }
                if non_inlined.unknown_thread > 0 {
                    writeln!(
                        w,
                        "{:>10}* request was queued from an unknown thread: {} times",
                        "", non_inlined.unknown_thread
                    )?;
                }
                if non_inlined.reentrant > 0 {
                    writeln!(
                        w,
                        "{:>10}* request would have been reentrant: {} times",
                        "", non_inlined.reentrant
                    )?;
                }
                if non_inlined.channel_wait_not_yet_registered > 0 {
                    writeln!(
                        w,
                        "{:>10}* channel wait was not yet registered when message received: {} times",
                        "", non_inlined.channel_wait_not_yet_registered
                    )?;
                }
                if non_inlined.no_thread_migration > 0 {
                    writeln!(
                        w,
                        "{:>10}* called into a dispatcher that wasn't allowed to migrate threads: {} times",
                        "", non_inlined.no_thread_migration
                    )?;
                }
            }
            if dispatcher.num_queued_tasks == 0 && dispatcher.queued_tasks.is_empty() {
                writeln!(w, "{:>8}No queued tasks", "")?;
            } else {
                if dispatcher.num_queued_tasks > dispatcher.queued_tasks.len() as u64 {
                    writeln!(
                        w,
                        "{:>8}{} queued tasks (showing first {}):",
                        "",
                        dispatcher.num_queued_tasks,
                        dispatcher.queued_tasks.len()
                    )?;
                } else {
                    writeln!(w, "{:>8}{} queued tasks:", "", dispatcher.num_queued_tasks)?;
                }
                for task in &dispatcher.queued_tasks {
                    writeln!(w, "{:>10}- Task {:#x}: handler {:#x}", "", task.ptr, task.handler)?;
                    if task.initiating_dispatcher != 0 || task.initiating_driver != 0 {
                        let disp_name = task
                            .initiating_dispatcher_name
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("<unknown>");
                        let drv_url = task
                            .initiating_driver_url
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("<unknown>");
                        writeln!(
                            w,
                            "{:>14}Initiated by dispatcher: {} ({:#x}), driver: {} ({:#x})",
                            "",
                            disp_name,
                            task.initiating_dispatcher,
                            drv_url,
                            task.initiating_driver
                        )?;
                    } else {
                        writeln!(w, "{:>14}Task was not queued from a managed thread", "")?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;
    use fuchsia_async as fasync;
    use futures::future::{Future, FutureExt};
    use futures::stream::StreamExt;

    async fn test_show<F, Fut>(cmd: ShowCommand, on_manager_request: F) -> Result<String>
    where
        F: Fn(fdd::ManagerRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        #[cfg(not(feature = "fdomain"))]
        let client = flex_client::fidl::ZirconClient;
        let (driver_development_proxy, mut stream) =
            client.create_proxy_and_stream::<fdd::ManagerMarker>();
        let mut writer = Vec::new();
        let request_handler_task = fasync::Task::spawn(async move {
            while let Some(Ok(request)) = stream.next().await {
                on_manager_request(request).await.unwrap();
            }
        });
        futures::select! {
            res = show(cmd, &mut writer, driver_development_proxy).fuse() => res?,
            _ = request_handler_task.fuse() => anyhow::bail!("Request handler task should not complete"),
        }
        Ok(String::from_utf8(writer)?)
    }

    #[fuchsia::test]
    async fn test_show_with_runtime_diagnostics() {
        let cmd = ShowCommand::from_args(&["show"], &["1000", "--runtime"]).unwrap();
        let output = test_show(cmd, |request| async move {
            match request {
                fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                    let mut stream = iterator.into_stream();
                    let mut sent = false;
                    while let Some(Ok(fdd::NodeInfoIteratorRequest::GetNext { responder })) =
                        stream.next().await
                    {
                        if !sent {
                            sent = true;
                            responder.send(&[fdd::NodeInfo {
                                driver_host_koid: Some(1000),
                                bound_driver_url: Some(
                                    "fuchsia-boot:///my-driver#meta/my-driver.cm".to_string(),
                                ),
                                moniker: Some("dev.sys.my-node".to_string()),
                                ..Default::default()
                            }])?;
                        } else {
                            responder.send(&[])?;
                        }
                    }
                }
                fdd::ManagerRequest::GetDriverHostInfo { iterator, .. } => {
                    let mut stream = iterator.into_stream();
                    let mut sent = false;
                    while let Some(Ok(fdd::DriverHostInfoIteratorRequest::GetNext { responder })) =
                        stream.next().await
                    {
                        if !sent {
                            sent = true;
                            responder.send(&[fdd::DriverHostInfo {
                                process_koid: Some(1000),
                                name: Some("driver-host-test".to_string()),
                                threads: Some(vec![
                                    fdd::ThreadInfo {
                                        koid: Some(1111),
                                        name: Some("initial-thread".to_string()),
                                        scheduler_role: None,
                                        ..Default::default()
                                    },
                                    fdd::ThreadInfo {
                                        koid: Some(2222),
                                        name: Some(
                                            "fdf-dispatcher-thread-0:fuchsia.test.role".to_string(),
                                        ),
                                        scheduler_role: Some("fuchsia.test.role".to_string()),
                                        ..Default::default()
                                    },
                                ]),
                                dispatchers: Some(vec![fdd::DispatcherInfo {
                                    driver: Some(
                                        "fuchsia-boot:///my-driver#meta/my-driver.cm".to_string(),
                                    ),
                                    name: Some("main-dispatcher".to_string()),
                                    options: Some(0),
                                    scheduler_role: Some("fuchsia.test.role".to_string()),
                                    dispatcher_ptr: Some(0x1234),
                                    driver_ptr: Some(0x5678),
                                    synchronized: Some(true),
                                    allow_sync_calls: Some(false),
                                    state: Some(fdd::DispatcherState::Running),
                                    destroy_context: Some("main-dispatcher".to_string()),
                                    destroy_user_initiated: Some(true),
                                    debug_stats: Some(fdd::DispatcherDebugStats {
                                        num_total_requests: Some(11),
                                        num_inlined_requests: Some(8),
                                        non_inlined: Some(fdd::NonInlinedRequestStats {
                                            allow_sync_calls: Some(1),
                                            task: Some(2),
                                            ..Default::default()
                                        }),
                                        ..Default::default()
                                    }),
                                    num_queued_tasks: Some(3),
                                    queued_tasks: Some(vec![fdd::QueuedTaskInfo {
                                        ptr: Some(0xabcd),
                                        handler: Some(0xef01),
                                        initiating_dispatcher: Some(0x1234),
                                        initiating_dispatcher_name: Some(
                                            "main-dispatcher".to_string(),
                                        ),
                                        initiating_driver: Some(0x5678),
                                        initiating_driver_url: Some(
                                            "fuchsia-boot:///my-driver#meta/my-driver.cm"
                                                .to_string(),
                                        ),
                                        ..Default::default()
                                    }]),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            }])?;
                        } else {
                            responder.send(&[])?;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })
        .await
        .unwrap();

        assert!(output.contains("Threads:"), "output was: {output}");
        assert!(output.contains("Koid: 1111, Name: initial-thread"), "output was: {output}");
        assert!(
            output.contains(
                "Koid: 2222, Name: fdf-dispatcher-thread-0:fuchsia.test.role, Role: fuchsia.test.role"
            ),
            "output was: {output}"
        );
        assert!(output.contains("Dispatchers:"), "output was: {output}");
        assert!(output.contains("Name: main-dispatcher (0x1234)"), "output was: {output}");
        assert!(
            output.contains("Driver: fuchsia-boot:///my-driver#meta/my-driver.cm (0x5678)"),
            "output was: {output}"
        );
        assert!(output.contains("State: running"), "output was: {output}");
        assert!(output.contains("Synchronized: true"), "output was: {output}");
        assert!(output.contains("Allow sync calls: false"), "output was: {output}");
        assert!(output.contains("Scheduler role: fuchsia.test.role"), "output was: {output}");
        assert!(
            output.contains("A call to Destroy() was made by dispatcher: main-dispatcher"),
            "output was: {output}"
        );
        assert!(output.contains("Destroy() was initiated by: user"), "output was: {output}");
        assert!(output.contains("Processed 11 requests, 8 were inlined"), "output was: {output}");
        assert!(
            output.contains(
                "* calling from a non-blocking to a blocking (ALLOW_SYNC_CALLS) dispatcher: 1 times"
            ),
            "output was: {output}"
        );
        assert!(output.contains("* request was a task: 2 times"), "output was: {output}");
        assert!(output.contains("3 queued tasks (showing first 1):"), "output was: {output}");
        assert!(output.contains("- Task 0xabcd: handler 0xef01"), "output was: {output}");
        assert!(
            output.contains("Initiated by dispatcher: main-dispatcher (0x1234), driver: fuchsia-boot:///my-driver#meta/my-driver.cm (0x5678)"),
            "output was: {output}"
        );
    }
}
