// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![forbid(unsafe_code)]

mod pressure_notifier;
mod pressure_observer;

use crate::pressure_notifier::PressureNotifier;
use crate::pressure_observer::PressureObserver;
use anyhow::Context as _;
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_kernel as fkernel;
use fidl_fuchsia_memory_debug as fdebug;
use fidl_fuchsia_memorypressure as fmp;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::server::ServiceFs;
use futures::{FutureExt as _, StreamExt as _};
use log::{debug, error};
use std::path::Path;

const SEND_CRITICAL_PRESSURE_CRASH_REPORTS_PATH: &str =
    "/config/data/send_critical_pressure_crash_reports";

enum IncomingRequest {
    Provider(fmp::ProviderRequestStream),
    Debug(fdebug::MemoryPressureRequestStream),
}

async fn create_pressure_observer() -> Result<PressureObserver, anyhow::Error> {
    let proxy = connect_to_protocol::<fkernel::RootJobForInspectMarker>()
        .context("Failed to connect to fuchsia.kernel.RootJobForInspect")?;
    let job = proxy.get().await.context("Failed to get root job from RootJobForInspect")?;
    PressureObserver::new(&job).context("Failed to initialize PressureObserver")
}

#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    debug!("memory_pressure_signaler: starting");

    fuchsia_trace_provider::trace_provider_create_with_fdio();

    let send_critical_pressure_crash_reports =
        Path::new(SEND_CRITICAL_PRESSURE_CRASH_REPORTS_PATH).exists();

    let crash_reporter = connect_to_protocol::<ffeedback::CrashReporterMarker>()
        .context("Failed to connect to fuchsia.feedback.CrashReporter")?;

    let notifier = PressureNotifier::new(send_critical_pressure_crash_reports, crash_reporter);

    let mut service_fs = ServiceFs::new_local();
    service_fs
        .dir("svc")
        .add_fidl_service(IncomingRequest::Provider)
        .add_fidl_service(IncomingRequest::Debug);
    service_fs
        .take_and_serve_directory_handle()
        .context("Failed to serve outgoing directory from startup info")?;

    let service_fs_fut = async {
        while let Some(request) = service_fs.next().await {
            match request {
                IncomingRequest::Provider(stream) => notifier.handle_provider_stream(stream),
                IncomingRequest::Debug(stream) => notifier.handle_debug_stream(stream),
            }
        }
    };

    match create_pressure_observer().await {
        Ok(mut observer) => {
            let observer_fut = async {
                loop {
                    let level = observer.wait_on_level_change().await.map_err(|status| {
                        anyhow::anyhow!("wait_on_level_change failed: {status}")
                    })?;
                    notifier.post_level_change(level);
                }
            };
            futures::pin_mut!(observer_fut, service_fs_fut);
            futures::select! {
                res = observer_fut.fuse() => res,
                () = service_fs_fut.fuse() => {
                    debug!("memory_pressure_signaler: service_fs finished, exiting");
                    Ok(())
                }
            }
        }
        Err(error) => {
            error!(
                "Failed to initialize PressureObserver; kernel memory pressure events will not be observed: {error:#}"
            );
            service_fs_fut.await;
            debug!("memory_pressure_signaler: service_fs finished, exiting");
            Ok(())
        }
    }
}
