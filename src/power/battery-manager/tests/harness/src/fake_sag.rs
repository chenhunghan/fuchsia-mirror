// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake System Activity Governor that hands out wake leases and reports their lifecycle.

use anyhow::Result;
use fidl_fuchsia_power_system as fsystem;
use fuchsia_async as fasync;
use fuchsia_component::server as fserver;
use fuchsia_component_test::LocalComponentHandles;
use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _, TryStreamExt as _};

/// Wake lease lifecycle event recorded by the fake System Activity Governor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseEvent {
    Acquired(String),
    Dropped(String),
}

/// Serves `fuchsia.power.system.ActivityGovernor` as a local component, sending a `LeaseEvent`
/// to `events` when each wake lease is acquired and when it is dropped.
pub(crate) async fn run_fake_sag(
    handles: LocalComponentHandles,
    events: mpsc::Sender<LeaseEvent>,
) -> Result<()> {
    let mut fs = fserver::ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: fsystem::ActivityGovernorRequestStream| stream);
    fs.serve_connection(handles.outgoing_dir)?;
    fs.for_each_concurrent(None, |stream| serve_activity_governor(stream, events.clone())).await;
    Ok(())
}

async fn serve_activity_governor(
    mut stream: fsystem::ActivityGovernorRequestStream,
    mut events: mpsc::Sender<LeaseEvent>,
) {
    while let Some(request) = stream.try_next().await.expect("failed to serve ActivityGovernor") {
        match request {
            fsystem::ActivityGovernorRequest::AcquireWakeLease { name, responder } => {
                log::info!("Fake SAG: AcquireWakeLease called: {name}");
                let lease = acquire_lease(name, &mut events).await;
                responder.send(Ok(lease)).expect("failed to send response");
            }
            fsystem::ActivityGovernorRequest::AcquireUnmonitoredWakeLease { name, responder } => {
                log::info!("Fake SAG: AcquireUnmonitoredWakeLease called: {name}");
                let lease = acquire_lease(name, &mut events).await;
                responder.send(Ok(lease)).expect("failed to send response");
            }
            _ => panic!("Fake SAG: unimplemented method"),
        }
    }
}

/// Records that the lease `name` was acquired, and returns its token, reporting when the holder
/// drops it.
async fn acquire_lease(name: String, events: &mut mpsc::Sender<LeaseEvent>) -> zx::EventPair {
    let (local_token, remote_token) = zx::EventPair::create();
    let _ = events.send(LeaseEvent::Acquired(name.clone())).await;
    let mut events = events.clone();
    fasync::Task::local(async move {
        let _ = fasync::OnSignals::new(&local_token, zx::Signals::OBJECT_PEER_CLOSED).await;
        log::info!("Fake SAG: Lease token dropped for {name}");
        let _ = events.send(LeaseEvent::Dropped(name)).await;
    })
    .detach();
    remote_token
}
