// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use bt_repl::{CommandSet, Commands, ReplControl, ReplHandler};
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_bluetooth_avdtp_test::*;
use fuchsia_sync::RwLock;
use futures::{FutureExt, TryStreamExt, select};
use log::info;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::pin::pin;
use std::sync::Arc;

use crate::commands::Cmd;
use crate::types::{CLEAR_LINE, PROMPT, PeerFactoryMap};

mod commands;
mod types;

// TODO(https://fxbug.dev/42112689): Spawn listener for PeerEventStream to delete peer from map
// when a peer disconnects from service.
async fn peer_manager_listener(
    avdtp_svc: &PeerManagerProxy,
    mut stream: PeerManagerEventStream,
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<(), Error> {
    while let Some(evt) = stream.try_next().await? {
        print!("{}", CLEAR_LINE);
        match evt {
            PeerManagerEvent::OnPeerConnected { peer_id } => {
                let (client, server) = create_endpoints::<PeerControllerMarker>();
                let peer = client.into_proxy();

                match peer_map.write().entry(peer_id.value.to_string()) {
                    Entry::Occupied(mut entry) => {
                        let _ = entry.insert(peer);
                        println!("Known peer connected with id: {}", peer_id.value)
                    }
                    Entry::Vacant(entry) => {
                        let _ = entry.insert(peer);
                        println!("Inserted device into PeerFactoryMap with id: {}", peer_id.value);
                    }
                };
                // Establish channel with the given peer_id and server endpoint.
                let _ = avdtp_svc.get_peer(&peer_id, server);
                info!("Getting peer with peer_id: {}", peer_id.value);
            }
        }
    }
    Ok(())
}

async fn set_configuration<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::SetConfig, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.set_configuration().await;
            Ok(String::from("Set configuration command"))
        }
        Err(e) => Err(e),
    }
}

async fn get_configuration<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::GetConfig, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.get_configuration().await;
            Ok(String::from("Get configuration command"))
        }
        Err(e) => Err(e),
    }
}

async fn get_capabilities<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::GetCapabilities, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.get_capabilities().await;
            Ok(String::from("Get capabilities command"))
        }
        Err(e) => Err(e),
    }
}

async fn get_all_capabilities<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::GetAllCapabilities, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.get_all_capabilities().await;
            Ok(String::from("Get all capabilities command"))
        }
        Err(e) => Err(e),
    }
}

async fn reconfigure<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::Reconfigure, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.reconfigure_stream().await;
            Ok(String::from("Reconfigure command"))
        }
        Err(e) => Err(e),
    }
}

async fn suspend<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::Suspend, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.suspend_stream().await;
            Ok(String::from("Suspend command"))
        }
        Err(e) => Err(e),
    }
}

async fn suspend_reconfigure<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::SuspendReconfigure, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.suspend_and_reconfigure().await;
            Ok(String::from("Suspend and reconfigure command"))
        }
        Err(e) => Err(e),
    }
}

async fn release_stream<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::ReleaseStream, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.release_stream().await;
            Ok(String::from("Release stream command"))
        }
        Err(e) => Err(e),
    }
}

async fn establish_stream<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::EstablishStream, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.establish_stream().await;
            Ok(String::from("Establish stream command"))
        }
        Err(e) => Err(e),
    }
}

async fn abort_stream<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::AbortStream, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.abort_stream().await;
            Ok(String::from("Abort stream command"))
        }
        Err(e) => Err(e),
    }
}

async fn start_stream<'a>(
    args: &'a [&'a str],
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<String, Error> {
    match get_controller_from_args(args, Cmd::StartStream, peer_map) {
        Ok(peer_controller) => {
            let _ = peer_controller.start_stream().await;
            Ok(String::from("Start stream command"))
        }
        Err(e) => Err(e),
    }
}

fn get_controller_from_args<'a>(
    args: &'a [&'a str],
    cmd: Cmd,
    peer_map: Arc<RwLock<PeerFactoryMap>>,
) -> Result<PeerControllerProxy, Error> {
    if args.len() < 1 {
        return Err(format_err!("usage: {}", cmd.usage()));
    }

    // Handle the case where user wants to understand command usage.
    if args[0] == "help" {
        return Err(format_err!("{}", cmd.usage()));
    }

    let id = args[0];
    match peer_map.write().entry(id.to_string()) {
        Entry::Occupied(entry) => Ok(entry.get().clone()),
        Entry::Vacant(_) => Err(format_err!("No peer with id: {:?}.\n Usage: {}", id, cmd.usage())),
    }
}

/// Handles the commands entered into the bt-avdtp-tool REPL.
struct AvdtpHandler {
    peer_map: Arc<RwLock<PeerFactoryMap>>,
}

impl ReplHandler for AvdtpHandler {
    type Command = Cmd;

    async fn handle(&mut self, command: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        let peer_map = self.peer_map.clone();
        let res = match command {
            Cmd::AbortStream => abort_stream(args, peer_map).await,
            Cmd::EstablishStream => establish_stream(args, peer_map).await,
            Cmd::ReleaseStream => release_stream(args, peer_map).await,
            Cmd::StartStream => start_stream(args, peer_map).await,
            Cmd::SetConfig => set_configuration(args, peer_map).await,
            Cmd::GetConfig => get_configuration(args, peer_map).await,
            Cmd::GetCapabilities => get_capabilities(args, peer_map).await,
            Cmd::GetAllCapabilities => get_all_capabilities(args, peer_map).await,
            Cmd::Reconfigure => reconfigure(args, peer_map).await,
            Cmd::Suspend => suspend(args, peer_map).await,
            Cmd::SuspendReconfigure => suspend_reconfigure(args, peer_map).await,
        }?;
        if !res.is_empty() {
            println!("{}", res);
        }
        Ok(ReplControl::Continue)
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let avdtp_svc = fuchsia_component::client::connect_to_protocol::<PeerManagerMarker>()
        .context("Failed to connect to AVDTP Peer Manager")?;
    // Get the list of currently connected peers.
    // Since we are spinning up bt-a2dp locally, there should be no connected peers.
    let _peers = avdtp_svc.connected_peers().await?;

    let evt_stream = avdtp_svc.take_event_stream();
    let peer_map: Arc<RwLock<PeerFactoryMap>> = Arc::new(RwLock::new(HashMap::new()));

    let event_fut = peer_manager_listener(&avdtp_svc, evt_stream, peer_map.clone()).fuse();
    let repl_fut =
        bt_repl::run(PROMPT, Commands::<Cmd>::new(), AvdtpHandler { peer_map: peer_map.clone() })
            .fuse();

    let mut event_fut = pin!(event_fut);
    let mut repl_fut = pin!(repl_fut);

    // Exit when either the REPL ends or the AVDTP Peer Manager closes.
    select! {
        result = event_fut => result.context("AVDTP Peer Manager event stream failed"),
        result = repl_fut => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fidl::endpoints::create_proxy;

    #[fuchsia::test]
    /// Tests parsing input arguments works successfully.
    /// Success case: The id provided exists, and the PeerControllerProxy is correctly returned.
    /// Error case: Invalid id, Error string + help message returned.
    async fn test_get_controller_from_args() {
        let mut peer_map = HashMap::new();
        let (client_proxy, _server) = create_proxy::<PeerControllerMarker>();

        let supported_id = "123";
        let _ = peer_map.insert(supported_id.to_string(), client_proxy.clone());
        let peer_map_obj = Arc::new(RwLock::new(peer_map));

        let invalid_id_args = ["2"];
        let res =
            get_controller_from_args(&invalid_id_args, Cmd::StartStream, peer_map_obj.clone());
        assert!(res.is_err());

        let valid_id_args = [supported_id];
        let res = get_controller_from_args(&valid_id_args, Cmd::StartStream, peer_map_obj);
        assert!(res.is_ok());
    }

    #[fuchsia::test]
    /// Tests that a command for an unknown peer returns an error that includes the command usage.
    async fn test_handler_reports_unknown_peer() {
        let mut handler = AvdtpHandler { peer_map: Arc::new(RwLock::new(HashMap::new())) };
        let error =
            handler.handle(Cmd::StartStream, &["2"]).await.expect_err("peer 2 is not connected");
        assert!(error.to_string().contains(&Cmd::StartStream.usage()));
    }
}
