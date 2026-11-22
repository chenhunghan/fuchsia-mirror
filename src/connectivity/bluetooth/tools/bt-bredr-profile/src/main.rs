// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use anyhow::{Context, Error, anyhow};
use bt_repl::{CommandSet, Commands, ReplControl, ReplHandler};
use bt_rfcomm::ServerChannel;
use fidl::endpoints::create_request_stream;
use fidl_fuchsia_bluetooth::{ChannelMode, ChannelParameters, SecurityRequirements};
use fidl_fuchsia_bluetooth_bredr::*;
use fidl_fuchsia_bluetooth_rfcomm_test::RfcommTestMarker;
use fuchsia_async as fasync;
use fuchsia_bluetooth::types::{PeerId, Uuid};
use fuchsia_component::client::connect_to_protocol;
use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::{FutureExt, TryStreamExt, select};
use std::sync::Arc;
use test_rfcomm_client::RfcommManager;

use crate::commands::Cmd;
use crate::types::*;

mod commands;
mod types;

/// Prompt to be shown for tool's REPL
pub static PROMPT: &str = "\x1b[34mprofile>\x1b[0m ";

/// Escape code to clear the pty line on which the cursor is located.
/// Used when evented output is intermingled with the REPL prompt.
pub static RESET_LINE: &str = "\x1b[2K\r";

async fn channels(state: Arc<Mutex<ProfileState>>) {
    for (chan_id, chan) in state.lock().await.l2cap_channels.map() {
        print!(
            "Channel:\n  Id: {}\n  Mode: {:?}\n  Max Tx Sdu Size: {}\n",
            chan_id, chan.mode, chan.max_tx_sdu_size
        );
    }
}

fn channel_mode_from_str(s: &str) -> Result<ChannelMode, Error> {
    match s {
        "basic" | "b" => Ok(ChannelMode::Basic),
        "ertm" | "e" => Ok(ChannelMode::EnhancedRetransmission),
        s => Err(anyhow!("Invalid channel mode {}", s)),
    }
}

fn security_requirements_from_str(s: &str) -> Result<Option<SecurityRequirements>, Error> {
    match s {
        "none" => Ok(None),
        "auth" => Ok(Some(SecurityRequirements {
            authentication_required: Some(true),
            secure_connections_required: None,
            ..Default::default()
        })),
        "sc" => Ok(Some(SecurityRequirements {
            authentication_required: None,
            secure_connections_required: Some(true),
            ..Default::default()
        })),
        "auth-sc" => Ok(Some(SecurityRequirements {
            authentication_required: Some(true),
            secure_connections_required: Some(true),
            ..Default::default()
        })),
        s => Err(anyhow!("Invalid security requirements {}", s)),
    }
}

/// Listen on the control event channel for new events.
async fn connection_receiver(
    mut connection_requests: ConnectionReceiver2RequestStream,
    end_ad_receiver: oneshot::Receiver<()>,
    state: Arc<Mutex<ProfileState>>,
    service_id: u32,
) -> Result<(), Error> {
    let tag = "ConnectionReceiver";
    let mut end_ad_receiver = end_ad_receiver.fuse();
    loop {
        select! {
          request = connection_requests.try_next() => {
            let event = match request {
                Err(e) => return Err(anyhow!("{} error: {:?}", tag, e)),
                Ok(None) => return Err(anyhow!("{} channel closed", tag)),
                Ok(Some(event)) => event,
            };
            let ConnectionReceiver2Request::Connected { payload, responder } = event else {
                return Err(anyhow!("ConnectionReceiver2Request: unknown method"));
            };
            let _ = responder.send();
            let channel = payload.channel.ok_or_else(|| anyhow!("{}: missing channel", tag))?;
            let socket = channel.socket.ok_or_else(|| anyhow!("{}: missing socket", tag))?;
            let mode = channel.channel_mode.ok_or_else(|| anyhow!("{}: missing channel mode", tag))?;
            let max_tx_sdu_size = channel.max_tx_sdu_size.ok_or_else(|| anyhow!("{}: missing max tx sdu", tag))?;
            let chan_id = state.lock().await.l2cap_channels.insert(L2capChannel { socket, mode, max_tx_sdu_size });
            print!("{} Channel Connected to service {}:\n  Channel:\n    Id: {}\n    Mode: {:?}\n    Max Tx Sdu Size: {}\n", RESET_LINE, service_id, chan_id, mode, max_tx_sdu_size);
        },
            _ = end_ad_receiver => break,
            complete => break,
        }
    }
    print!("{} ConnectionReceiver closed for service {}", RESET_LINE, service_id);
    let args = vec![format!("{}", service_id)];
    remove_service(state, &args).await
}

async fn setup_rfcomm(state: Arc<Mutex<ProfileState>>) -> Result<(), Error> {
    state.lock().await.rfcomm.advertise()
}

async fn advertise(
    profile_svc: &ProfileProxy,
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 3 {
        println!("Error: invalid number of arguments");
        println!("usage: {}", Cmd::Advertise.usage());
        return Ok(());
    }

    let psm = args[0].parse::<u16>().map_err(|_| anyhow!("psm must be a 16-bit integer"))?;
    let channel_mode =
        channel_mode_from_str(args[1].as_ref()).map_err(|_| anyhow!("invalid channel mode"))?;
    let max_rx_sdu_size = args[2]
        .parse::<u16>()
        .map_err(|_| anyhow!("max-rx-sdu-size must be an integer i the range 0 - 65535"))?;
    let params = ChannelParameters {
        channel_mode: Some(channel_mode),
        max_rx_packet_size: Some(max_rx_sdu_size),
        security_requirements: None,
        ..Default::default()
    };

    let audio_sink_uuid = Uuid::new16(0x110B); // Audio Sink

    let svc_defs = vec![ServiceDefinition {
        service_class_uuids: Some(vec![audio_sink_uuid.into()]),
        protocol_descriptor_list: Some(vec![ProtocolDescriptor {
            protocol: Some(ProtocolIdentifier::L2Cap),
            params: Some(vec![DataElement::Uint16(psm)]),
            ..Default::default()
        }]),
        ..Default::default()
    }];

    let (connect_client, connect_requests) = create_request_stream();

    let _ = profile_svc
        .advertise(ProfileAdvertiseRequest {
            services: Some(svc_defs),
            parameters: Some(params),
            connection_receiver: Some(connect_client),
            ..Default::default()
        })
        .check()?;

    let (end_ad_sender, end_ad_receiver) = oneshot::channel::<()>();
    let service_id = state.lock().await.services.insert(SdpService {
        advertisement_stopper: end_ad_sender,
        params: ChannelParameters {
            channel_mode: Some(channel_mode),
            max_rx_packet_size: Some(max_rx_sdu_size),
            security_requirements: None,
            ..Default::default()
        },
    });

    print!("Advertised Service Id: {}\n", service_id);

    let request_handler_fut =
        connection_receiver(connect_requests, end_ad_receiver, state.clone(), service_id);
    fasync::Task::spawn(async move {
        if let Err(e) = request_handler_fut.await {
            print!("{} ConnectionReceiver ended with error: {:?}", RESET_LINE, e);
        }
    })
    .detach();

    Ok(())
}

async fn remove_service(state: Arc<Mutex<ProfileState>>, args: &Vec<String>) -> Result<(), Error> {
    if args.len() != 1 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let service_id =
        args[0].parse::<u32>().map_err(|_| anyhow!("service-id must be a positive number"))?;

    let _ = state
        .lock()
        .await
        .services
        .remove(&service_id)
        .ok_or_else(|| anyhow!("Unknown service"))?;
    Ok(())
}

async fn services(state: Arc<Mutex<ProfileState>>) {
    for (id, service) in &state.lock().await.services {
        print!(
            "Service:\n  Id: {}\n  Mode: {:?}, Max Rx Sdu Size: {}",
            id,
            service.params.channel_mode.unwrap(),
            service.params.max_rx_packet_size.unwrap()
        );
    }
}

async fn connect_l2cap(
    profile_svc: &ProfileProxy,
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 5 {
        return Err(anyhow!("Invalid number of arguments"));
    }
    let peer_id: PeerId = args[0].parse()?;
    let psm = args[1].parse::<u16>().map_err(|_| anyhow!("Psm must be [0, 65535]"))?;
    let channel_mode = channel_mode_from_str(args[2].as_ref())?;
    let max_rx_sdu_size =
        args[3].parse::<u16>().map_err(|_| anyhow!("max-sdu-size must be [0, 65535]"))?;
    let security_requirements = security_requirements_from_str(args[4].as_ref())?;
    let params = ChannelParameters {
        channel_mode: Some(channel_mode),
        max_rx_packet_size: Some(max_rx_sdu_size),
        security_requirements,
        ..Default::default()
    };

    let channel = match profile_svc
        .connect(
            &peer_id.into(),
            &ConnectParameters::L2cap(L2capParameters {
                psm: Some(psm),
                parameters: Some(params),
                ..Default::default()
            }),
        )
        .await?
    {
        Err(e) => return Err(anyhow!("Could not connect to {}: {:?}", peer_id, e)),
        Ok(channel) => channel,
    };

    let mode = match channel.channel_mode {
        Some(m) => m,
        None => return Err(anyhow!("Missing channel mode in response")),
    };

    let max_tx_sdu_size = match channel.max_tx_sdu_size {
        Some(s) => s,
        None => return Err(anyhow!("Missing max tx sdu size in response")),
    };

    let chan_id = match channel.socket {
        Some(socket) => {
            state.lock().await.l2cap_channels.insert(L2capChannel { socket, mode, max_tx_sdu_size })
        }
        None => {
            println!("Error: failed to receive a socket");
            return Ok(());
        }
    };

    print!(
        "Channel:\n  Id: {}\n  Mode: {:?}\n  Max Tx Sdu Size: {}\n",
        chan_id, mode, max_tx_sdu_size
    );

    Ok(())
}

/// Attempts to make an outbound RFCOMM connection to the peer specified in `args`.
async fn connect_rfcomm(state: Arc<Mutex<ProfileState>>, args: &Vec<String>) -> Result<(), Error> {
    if args.len() != 2 {
        return Err(anyhow!("Invalid number of arguments"));
    }
    let peer_id: PeerId = args[0].parse()?;
    let server_channel =
        args[1].parse::<u8>().map_err(|_| anyhow!("Server channel must be a u8"))?;
    let server_channel = ServerChannel::try_from(server_channel)?;

    state.lock().await.rfcomm.outgoing_rfcomm_channel(peer_id, server_channel).await
}

async fn disconnect_l2cap(
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 1 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let chan_id = args[0].parse::<u32>().map_err(|_| anyhow!("channel-id must be an integer"))?;

    match state.lock().await.l2cap_channels.remove(&chan_id) {
        Some(_) => println!("Channel {} disconnected", chan_id),
        None => println!("No channel with id {} exists", chan_id),
    }
    Ok(())
}

async fn disconnect_rfcomm(
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 2 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let peer_id: PeerId = args[0].parse()?;
    let server_channel =
        args[1].parse::<u8>().map_err(|_| anyhow!("Server channel must be a u8"))?;
    let server_channel = ServerChannel::try_from(server_channel)?;

    match state.lock().await.rfcomm.close_rfcomm_channel(peer_id, server_channel) {
        Ok(()) => println!("RFCOMM Channel {:?} disconnected", server_channel),
        Err(e) => println!("Couldn't close RFCOMM channel: {:?}", e),
    }
    Ok(())
}

async fn disconnect_rfcomm_session(
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 1 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let peer_id: PeerId = args[0].parse()?;
    match state.lock().await.rfcomm.close_session(peer_id) {
        Ok(_) => println!("Closed RFCOMM Session with peer {:?}", peer_id),
        Err(e) => println!("Error closing RFCOMM Session with peer {:?}: {:?}", peer_id, e),
    }
    Ok(())
}

async fn send_rls(state: Arc<Mutex<ProfileState>>, args: &Vec<String>) -> Result<(), Error> {
    if args.len() != 2 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let peer_id: PeerId = args[0].parse()?;
    let server_channel =
        args[1].parse::<u8>().map_err(|_| anyhow!("Server channel must be a u8"))?;
    let server_channel = ServerChannel::try_from(server_channel)?;

    match state.lock().await.rfcomm.send_rls(peer_id, server_channel) {
        Ok(_) => println!("Sent RLS to peer {:?}", peer_id),
        Err(e) => println!("Error sending RLS to peer {:?}: {:?}", peer_id, e),
    }
    Ok(())
}

async fn write_l2cap(state: Arc<Mutex<ProfileState>>, args: &Vec<String>) -> Result<(), Error> {
    if args.len() != 2 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let chan_id = args[0].parse::<u32>().map_err(|_| anyhow!("channel-id must be an integer"))?;
    let bytes = args[1].as_bytes();

    let num_bytes = match state.lock().await.l2cap_channels.map().get(&chan_id) {
        Some(chan) => chan.socket.write(bytes).map_err(|_| anyhow!("error writing data"))?,
        None => return Err(anyhow!("No channel with id {} exists", chan_id)),
    };
    println!("{} bytes written", num_bytes);

    Ok(())
}

/// Sends a user data payload to the ServerChannel specified in `args`.
async fn write_rfcomm(state: Arc<Mutex<ProfileState>>, args: &Vec<String>) -> Result<(), Error> {
    if args.len() != 3 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let peer_id: PeerId = args[0].parse()?;
    let server_channel =
        args[1].parse::<u8>().map_err(|_| anyhow!("Server channel must be a u8"))?;
    let server_channel = ServerChannel::try_from(server_channel)?;
    let bytes = args[2].as_bytes().to_vec();
    state.lock().await.rfcomm.send_user_data(peer_id, server_channel, bytes)
}

async fn write_large_rfcomm(
    state: Arc<Mutex<ProfileState>>,
    args: &Vec<String>,
) -> Result<(), Error> {
    if args.len() != 2 {
        return Err(anyhow!("Invalid number of arguments"));
    }

    let peer_id: PeerId = args[0].parse()?;
    let server_channel =
        args[1].parse::<u8>().map_err(|_| anyhow!("Server channel must be a u8"))?;
    let server_channel = ServerChannel::try_from(server_channel)?;
    // Arbitrary payload of size 900. Just a bunch of "a" chars.
    let bytes: Vec<u8> = vec![b'a'; 900];
    for _i in 0..10 {
        state.lock().await.rfcomm.send_user_data(peer_id, server_channel, bytes.clone())?;
    }
    Ok(())
}

async fn cleanup(state: Arc<Mutex<ProfileState>>) {
    state.lock().await.reset();
}

/// Handles the commands entered into the bt-bredr-profile REPL.
struct ProfileHandler {
    profile_svc: ProfileProxy,
    state: Arc<Mutex<ProfileState>>,
}

impl ReplHandler for ProfileHandler {
    type Command = Cmd;

    async fn handle(&mut self, command: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let profile_svc = &self.profile_svc;
        let state = &self.state;
        match command {
            Cmd::Advertise => advertise(profile_svc, state.clone(), &args).await?,
            Cmd::RemoveService => remove_service(state.clone(), &args).await?,
            Cmd::Services => services(state.clone()).await,
            Cmd::Channels => channels(state.clone()).await,
            Cmd::ConnectL2cap => connect_l2cap(profile_svc, state.clone(), &args).await?,
            Cmd::ConnectRfcomm => connect_rfcomm(state.clone(), &args).await?,
            Cmd::DisconnectL2cap => disconnect_l2cap(state.clone(), &args).await?,
            Cmd::DisconnectRfcomm => disconnect_rfcomm(state.clone(), &args).await?,
            Cmd::DisconnectRfcommSession => disconnect_rfcomm_session(state.clone(), &args).await?,
            Cmd::SetupRfcomm => setup_rfcomm(state.clone()).await?,
            Cmd::SendRls => send_rls(state.clone(), &args).await?,
            Cmd::WriteL2cap => write_l2cap(state.clone(), &args).await?,
            Cmd::WriteRfcomm => write_rfcomm(state.clone(), &args).await?,
            Cmd::WriteLargeRfcomm => write_large_rfcomm(state.clone(), &args).await?,
        };
        Ok(ReplControl::Continue)
    }

    /// Removes all services and closes all channels.
    async fn on_exit(&mut self) {
        cleanup(self.state.clone()).await;
    }

    fn help_footer(&self) -> Option<String> {
        Some("See README for documentation.".to_string())
    }
}

#[fuchsia::main(logging_tags = ["bt-bredr-profile"])]
async fn main() -> Result<(), Error> {
    let profile_svc = connect_to_protocol::<ProfileMarker>()
        .context("failed to connect to bluetooth profile service")?;
    let rfcomm_test_svc = connect_to_protocol::<RfcommTestMarker>()
        .context("failed to connect to RFCOMM Test protocol")?;

    let rfcomm_mgr = RfcommManager::from_proxy(profile_svc.clone(), rfcomm_test_svc);
    let state = Arc::new(Mutex::new(ProfileState::new(rfcomm_mgr)));

    bt_repl::run(PROMPT, Commands::<Cmd>::new(), ProfileHandler { profile_svc, state }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn disconnect_l2cap_channel_succeeds() {
        let (profile, _profile_server) =
            fidl::endpoints::create_proxy_and_stream::<ProfileMarker>();
        let (rfcomm_test, _rfcomm_test_server) =
            fidl::endpoints::create_proxy_and_stream::<RfcommTestMarker>();
        let state = Arc::new(Mutex::new(ProfileState::new(RfcommManager::from_proxy(
            profile,
            rfcomm_test,
        ))));

        let (s, _) = fidl::Socket::create_stream();
        assert_eq!(
            0,
            state.lock().await.l2cap_channels.insert(L2capChannel {
                socket: s,
                mode: ChannelMode::Basic,
                max_tx_sdu_size: 672
            })
        );
        assert_eq!(1, state.lock().await.l2cap_channels.map().len());
        let args = vec!["0".to_string()];
        assert!(disconnect_l2cap(state.clone(), &args).await.is_ok());
        assert!(state.lock().await.l2cap_channels.map().is_empty());

        // Disconnecting an already disconnected channel should not fail.
        // (It should only print a message)
        assert!(disconnect_l2cap(state.clone(), &args).await.is_ok());
    }

    #[fuchsia::test]
    async fn on_exit_cleans_up_state() {
        let (profile, _profile_server) =
            fidl::endpoints::create_proxy_and_stream::<ProfileMarker>();
        let (rfcomm_test, _rfcomm_test_server) =
            fidl::endpoints::create_proxy_and_stream::<RfcommTestMarker>();
        let state = Arc::new(Mutex::new(ProfileState::new(RfcommManager::from_proxy(
            profile.clone(),
            rfcomm_test,
        ))));
        let (socket, _peer_socket) = fidl::Socket::create_stream();
        let _ = state.lock().await.l2cap_channels.insert(L2capChannel {
            socket,
            mode: ChannelMode::Basic,
            max_tx_sdu_size: 672,
        });

        let mut handler = ProfileHandler { profile_svc: profile, state: state.clone() };
        handler.on_exit().await;
        assert!(state.lock().await.l2cap_channels.map().is_empty());
    }
}
