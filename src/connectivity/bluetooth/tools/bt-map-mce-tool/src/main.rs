// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use fidl_fuchsia_bluetooth_map::MessagingClientMarker;
use fuchsia_component::client::connect_to_protocol;
use log::{info, warn};
use simplelog::{Config, LevelFilter, WriteLogger};
use std::io;

mod accessor;
mod commands;
mod repl;

use crate::accessor::AccessorClient;
use crate::repl::start_accessor_loop;

#[fuchsia::main(logging = false)]
async fn main() -> Result<(), Error> {
    let () = WriteLogger::init(LevelFilter::Info, Config::default(), io::stderr()).unwrap();

    // Connect to message client service.
    let svc = connect_to_protocol::<MessagingClientMarker>()
        .context("Failed to connect to MessagingClient FIDL interface")?;

    info!("Waiting for a peer...");
    match svc.watch_accessor().await? {
        Ok(response) => {
            let client = AccessorClient::new(
                response.peer_id.unwrap(),
                response.accessor.unwrap().into_proxy(),
            );
            if let Err(e) = start_accessor_loop(client.clone()).await {
                warn!(e:?; "MESSAGE_ACCESSOR REPL for peer {:?} closed", client.peer_id());
            }
        }
        Err(e) => {
            warn!(e:?; "Error while getting an Accessor FIDL");
            return Err(format_err!("{e:?}"));
        }
    }
    Ok(())
}
