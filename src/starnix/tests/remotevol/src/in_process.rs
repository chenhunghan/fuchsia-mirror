// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use component_events::events::{Event, EventStream, ExitStatus, Stopped};
use component_events::matcher::EventMatcher;
use fuchsia_component_test::{RealmBuilder, RealmBuilderParams};
use log::info;
use std::collections::BTreeMap;

#[fuchsia::test]
async fn in_process_remotevol_not_supported() {
    let mut events = EventStream::open().await.unwrap();
    let builder = RealmBuilder::with_params(
        RealmBuilderParams::new()
            .realm_name("in_process_remotevol")
            .from_relative_url("#meta/kernel_with_container.cm"),
    )
    .await
    .unwrap();

    info!("starting realm");
    let kernel_with_container = builder.build().await.unwrap();
    let realm_moniker = format!("realm_builder:{}", kernel_with_container.root.child_name());
    info!(realm_moniker:%; "started");
    let container_moniker = format!("{realm_moniker}/debian_container");
    let kernel_moniker = format!("{realm_moniker}/kernel");

    info!("triggering container startup");
    let _ = fuchsia_fs::directory::open_file(
        kernel_with_container.root.get_exposed_dir(),
        "/fs_root/proc/sysrq-trigger",
        fuchsia_fs::PERM_WRITABLE,
    )
    .await;

    info!("waiting for exit");
    assert_matches!(
        wait_for_exit_status(&mut events, [&container_moniker, &kernel_moniker]).await,
        [ExitStatus::Crash(..), ExitStatus::Crash(..)]
    );
}

async fn wait_for_exit_status<const N: usize>(
    events: &mut EventStream,
    monikers: [&str; N],
) -> [ExitStatus; N] {
    info!(monikers:% = monikers.join(","); "waiting for exit status");
    let mut statuses = BTreeMap::new();

    // Wait for all the provided monikers to stop.
    let mut num_stopped = 0;
    while num_stopped < N {
        let stopped = EventMatcher::ok().monikers(monikers).wait::<Stopped>(events).await.unwrap();
        let moniker = stopped.target_moniker().to_string();
        let status = stopped.result().unwrap().status;
        info!(moniker:%, status:?; "component stopped");
        statuses.insert(moniker, status);
        num_stopped += 1;
    }

    // Put the exit statuses in the order of monikers provided.
    let mut ret = [ExitStatus::Clean; N];
    for (i, moniker) in monikers.iter().enumerate() {
        ret[i] = statuses[*moniker];
    }
    ret
}
