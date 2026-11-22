// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake `fuchsia.time.alarms.WakeAlarms` that records alarm setup and cancellation events.

use anyhow::Result;
use fidl_fuchsia_time_alarms as falarms;
use fuchsia_component::server as fserver;
use fuchsia_component_test::LocalComponentHandles;
use futures::channel::mpsc;
use futures::{StreamExt as _, TryStreamExt as _};

/// Wake alarm event recorded by `run_fake_wake_alarms`.
#[derive(Debug)]
pub enum WakeAlarmEvent {
    SetAndWait {
        alarm_id: String,
        deadline: zx::BootInstant,
        responder: falarms::WakeAlarmsSetAndWaitResponder,
    },
    Cancel {
        alarm_id: String,
    },
}

/// Serves `fuchsia.time.alarms.WakeAlarms` as a local component, sending a `WakeAlarmEvent` to
/// `event_sender` when an alarm is armed or cancelled.
pub(crate) async fn run_fake_wake_alarms(
    handles: LocalComponentHandles,
    event_sender: mpsc::UnboundedSender<WakeAlarmEvent>,
) -> Result<()> {
    let mut fs = fserver::ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: falarms::WakeAlarmsRequestStream| stream);
    fs.serve_connection(handles.outgoing_dir)?;
    fs.for_each_concurrent(None, move |mut stream| {
        let event_sender = event_sender.clone();
        async move {
            while let Some(request) = stream.try_next().await.expect("failed to serve WakeAlarms") {
                match request {
                    falarms::WakeAlarmsRequest::SetAndWait {
                        deadline,
                        mode,
                        alarm_id,
                        responder,
                    } => {
                        if let falarms::SetMode::NotifySetupDone(done) = mode {
                            let _ = done.signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED);
                        }
                        let _ = event_sender.unbounded_send(WakeAlarmEvent::SetAndWait {
                            alarm_id,
                            deadline,
                            responder,
                        });
                    }
                    falarms::WakeAlarmsRequest::Cancel { alarm_id, .. } => {
                        let _ = event_sender.unbounded_send(WakeAlarmEvent::Cancel { alarm_id });
                    }
                    other => panic!("Fake WakeAlarms: Unimplemented method: {other:?}"),
                }
            }
        }
    })
    .await;
    Ok(())
}
