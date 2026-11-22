// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! rf-medium-dispatcher: the simulated IEEE 802.15.4 medium as a component. Serves
//! `test.lowpan.rfsim.Medium` to nodes and `test.lowpan.rfsim.Monitor` to tests.

use anyhow::Context as _;
use fidl_test_lowpan_rfsim as rfsim;
use fuchsia_component::server::{ServiceFs, ServiceFsDir};
use futures::StreamExt as _;
use log::info;
use rf_medium_dispatcher::MediumDispatcher;

enum IncomingService {
    Medium(rfsim::MediumRequestStream),
    Monitor(rfsim::MonitorRequestStream),
}

#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    let medium = MediumDispatcher::new();
    let mut fs = ServiceFs::new_local();
    let _: &mut ServiceFsDir<'_, _> = fs
        .dir("svc")
        .add_fidl_service(IncomingService::Medium)
        .add_fidl_service(IncomingService::Monitor);
    let _: &mut ServiceFs<_> =
        fs.take_and_serve_directory_handle().context("serving the outgoing directory")?;
    info!("rf-medium-dispatcher serving");
    // `serve_*` spawn a task per connection and return immediately.
    while let Some(service) = fs.next().await {
        match service {
            IncomingService::Medium(stream) => medium.serve_medium(stream),
            IncomingService::Monitor(stream) => medium.serve_monitor(stream),
        }
    }
    Ok(())
}
