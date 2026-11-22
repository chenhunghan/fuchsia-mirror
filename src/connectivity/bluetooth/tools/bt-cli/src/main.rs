// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use bt_repl::{CommandSet, ReplControl, ReplHandler};
use fidl_fuchsia_bluetooth::{HostId as FidlHostId, PeerId as FidlPeerId};
use fidl_fuchsia_bluetooth_sys::{
    AccessMarker, AccessProxy, BondableMode, HostWatcherMarker, HostWatcherProxy,
    PairingDelegateMarker, PairingMarker, PairingOptions, PairingProxy, PairingSecurityLevel,
    ProcedureTokenProxy, TechnologyType,
};
use fuchsia_async as fasync;
use fuchsia_bluetooth::types::io_capabilities::{InputCapability, OutputCapability};
use fuchsia_bluetooth::types::{HostId, HostInfo, Peer, PeerId, addresses_to_custom_string};
use fuchsia_component::client::connect_to_protocol;
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt, TryFutureExt, select};
use prettytable::{Row, Table, format, row};
use regex::Regex;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::pin::pin;
use std::str::FromStr;
use std::sync::Arc;

use crate::commands::{Cmd, command_source};
use crate::types::{DeviceClass, MajorClass, MinorClass, TryInto};

mod commands;
mod types;

static PROMPT: &str = "\x1b[34mbt>\x1b[0m ";
/// Escape code to clear the pty line on which the cursor is located.
/// Used when event output is intermingled with the REPL prompt.
static CLEAR_LINE: &str = "\x1b[2K";

async fn get_active_host(state: &Mutex<State>) -> Result<String, Error> {
    let state = state.lock();
    let active_iter = state.hosts.iter().find(|&h| h.active);
    match active_iter {
        Some(host) => Ok(host.to_string()),
        None => Ok(String::from("No host instances")),
    }
}

async fn get_hosts(state: &Mutex<State>) -> Result<String, Error> {
    let hosts = &state.lock().hosts;
    if hosts.is_empty() {
        return Ok(String::from("No host instances detected"));
    }
    // Create table of results
    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);
    let _ = table.set_titles(row![
        "HostId",
        "Addresses",
        "Active",
        "Technology",
        "Name",
        "Discoverable",
        "Discovering",
    ]);
    for host in hosts {
        let _ = table.add_row(row![
            host.id.to_string(),
            addresses_to_custom_string(&host.addresses, "\n"),
            host.active.to_string(),
            format!("{:?}", host.technology),
            host.local_name.clone().unwrap_or_else(|| "(unknown)".to_string()),
            host.discoverable.to_string(),
            host.discovering.to_string(),
        ]);
    }
    Ok(format!("{}", table))
}

async fn set_active_host<'a>(
    args: &'a [&'a str],
    host_svc: &'a HostWatcherProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Err(format_err!("usage: {}", Cmd::SetController.usage()));
    }
    println!("Setting active controller");
    let host_id = HostId::from_str(args[0])?;
    let fidl_host_id: FidlHostId = host_id.into();
    match host_svc.set_active(&fidl_host_id).await {
        Ok(_) => Ok(String::new()),
        Err(err) => Ok(format!("Error setting active controller: {}", err)),
    }
}

async fn set_local_name<'a>(
    args: &'a [&'a str],
    access_svc: &'a AccessProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", Cmd::SetLocalName.usage()));
    }
    println!("Setting local name of the active controller");
    match access_svc.set_local_name(args[0]) {
        Ok(_) => Ok(String::new()),
        Err(err) => Ok(format!("Error setting local name: {:?}", err)),
    }
}

/// Set the class of device for the currently active controller. Arguments are optional, and
/// defaults will be used if arguments aren't provided.
///
/// Returns an error if the input is not recognized as a valid device class .
async fn set_device_class<'a>(
    args: &'a [&'a str],
    access_svc: &'a AccessProxy,
) -> Result<String, Error> {
    let mut args = args.iter();
    println!("Setting device class of the active controller");
    let device_class = DeviceClass {
        major: args
            .next()
            .map(|arg| TryInto::try_into(&**arg))
            .unwrap_or(Ok(MajorClass::Uncategorized))?,
        minor: args
            .next()
            .map(|arg| TryInto::try_into(&**arg))
            .unwrap_or(Ok(MinorClass::not_set()))?,
        service: TryInto::try_into(args)?,
    }
    .into();

    match access_svc.set_device_class(&device_class) {
        Ok(_) => Ok(format!("Set device class to 0x{:x}", device_class.value)),
        Err(err) => Ok(format!("Error setting device class: {}", err)),
    }
}

fn match_peer<'a>(pattern: &'a str, peer: &Peer) -> bool {
    let pattern_lower = &pattern.to_lowercase();
    peer.id.to_string().to_lowercase().contains(pattern_lower)
        || peer.address.to_string().to_lowercase().contains(pattern_lower)
        || peer.name.as_ref().is_some_and(|p| p.to_lowercase().contains(pattern_lower))
}

/// Order named peers as greater than unnamed peers, connected peers as greater than unconnected
/// peers, and bonded peers as greater than unbonded peers, so more relevant peers appear last in
/// the output.
fn cmp_peers(a: &Peer, b: &Peer) -> Ordering {
    (a.name.is_some(), a.connected, a.bonded).cmp(&(b.name.is_some(), b.connected, b.bonded))
}

fn get_peers<'a>(args: &'a [&'a str], state: &Mutex<State>, full_details: bool) -> String {
    let cmd = if full_details { Cmd::ShowPeers } else { Cmd::ListPeers };
    let mut named_only = false;
    let mut find: Option<&str> = None;
    for &arg in args {
        match arg {
            "--named-only" | "-n" => named_only = true,
            _ if arg.starts_with('-') => return format!("usage: {}", cmd.usage()),
            _ if find.is_none() => find = Some(arg),
            _ => return format!("usage: {}", cmd.usage()),
        }
    }
    let state = state.lock();
    if state.peers.is_empty() {
        return String::from("No known peers");
    }
    let find = find.unwrap_or("");
    let mut peers: Vec<&Peer> = state
        .peers
        .values()
        .filter(|p| (!named_only || p.name.is_some()) && match_peer(find, p))
        .collect();
    peers.sort_by(|a, b| cmp_peers(a, b));
    let matched = format!("Showing {}/{} peers\n", peers.len(), state.peers.len());
    if peers.is_empty() {
        return matched;
    }

    if full_details {
        return String::from_iter(
            std::iter::once(matched).chain(peers.iter().map(|p| p.to_string())),
        );
    }

    // Create table of results
    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);
    let _ = table.set_titles(row![
        "PeerId",
        "Address",
        "Technology",
        "Name",
        "Appearance",
        "Connected",
        "Bonded",
    ]);
    for val in peers.into_iter() {
        let _ = table.add_row(peer_to_table_row(val));
    }
    format!("{matched}{table}")
}

/// Get the string representation of a peer from either an identifier or address
fn get_peer<'a>(args: &'a [&'a str], state: &Mutex<State>) -> String {
    if args.len() != 1 {
        return format!("usage: {}", Cmd::Peer.usage());
    }

    to_identifier(state, args[0])
        .and_then(|id| state.lock().peers.get(&id).map(|peer| peer.to_string()))
        .unwrap_or_else(|| String::from("No known peer"))
}

/// Returns basic peer information formatted as a prettytable Row
fn peer_to_table_row(peer: &Peer) -> Row {
    row![
        peer.id.to_string(),
        peer.address.to_string(),
        format! {"{:?}", peer.technology},
        peer.name.as_ref().map_or_else(|| "".to_string(), |x| format!("{:?}", x)),
        peer.appearance.as_ref().map_or_else(|| "".to_string(), |x| format!("{:?}", x)),
        peer.connected.to_string(),
        peer.bonded.to_string(),
    ]
}

async fn set_discovery(
    discovery: bool,
    state: &Mutex<State>,
    access_svc: &AccessProxy,
) -> Result<String, Error> {
    println!("{} Discovery!", if discovery { "Starting" } else { "Stopping" });
    if !discovery {
        state.lock().discovery_token = None;
        return Ok(String::new());
    }

    let (token, token_server) = fidl::endpoints::create_proxy();
    match access_svc.start_discovery(token_server).await? {
        Ok(_) => {
            state.lock().discovery_token = Some(token);
            Ok(String::new())
        }
        Err(err) => Ok(format!("Discovery error: {:?}", err)),
    }
}

// Find the identifier for a `Peer` based on a `key` that is either an identifier or an
// address.
// Returns `None` if the given address does not belong to a known peer.
fn to_identifier(state: &Mutex<State>, key: &str) -> Option<PeerId> {
    // Compile regex inline because it is not ever expected to be a bottleneck
    let address_pattern = Regex::new(r"^([0-9A-Fa-f]{2}[:-]){5}([0-9A-Fa-f]{2})$")
        .expect("Could not compile mac address regex pattern.");
    if address_pattern.is_match(key) {
        state.lock().peers.values().find(|peer| &peer.address == key).map(|peer| peer.id)
    } else {
        key.parse().ok()
    }
}

async fn connect<'a>(
    args: &'a [&'a str],
    state: &'a Mutex<State>,
    access_proxy: &'a AccessProxy,
    with_pairing: Option<&'a PairingProxy>,
) -> Result<String, Error> {
    if args.len() != 1 {
        let cmd = if with_pairing.is_some() { Cmd::Bond } else { Cmd::Connect };
        return Ok(format!("usage: {}", cmd.usage()));
    }
    // `args[0]` is the identifier of the peer to connect to
    let peer_id = match to_identifier(state, args[0]) {
        Some(id) => id,
        None => return Ok(format!("Unable to connect: Unknown address {}", args[0])),
    };
    if let Err(e) = access_proxy.connect(&peer_id.into()).await? {
        return Ok(format!("connect error: {:?}", e));
    }

    let pairing = with_pairing
        .map(|p| create_pairing_task(InputCapability::None, OutputCapability::None, &p))
        .transpose()?;
    if let Some((pairing_task, mut recv)) = pairing {
        if let Some((paired_id, paired)) = recv.next().await {
            // If pairing was completed, exit.
            let _ = pairing_task.abort();
            println!(
                "Completed {} pairing for {}.",
                if paired { "successful" } else { "unsuccessful" },
                paired_id
            );
        } else {
            println!("Note: No pairing occurred with {}.", peer_id);
        }
    }
    Ok(String::new())
}

fn parse_disconnect<'a>(args: &'a [&'a str], state: &'a Mutex<State>) -> Result<PeerId, String> {
    if args.len() != 1 {
        return Err(format!("usage: {}", Cmd::Disconnect.usage()));
    }
    // `args[0]` is the identifier of the peer to connect to
    match to_identifier(state, args[0]) {
        Some(id) => Ok(id),
        None => return Err(format!("Unable to disconnect: Unknown address {}", args[0])),
    }
}

async fn handle_disconnect(id: PeerId, access_svc: &AccessProxy) -> Result<String, Error> {
    let response = access_svc.disconnect(&id.into()).await?;
    match response {
        Ok(_) => Ok(String::new()),
        Err(err) => Ok(format!("Disconnect error: {:?}", err)),
    }
}

async fn disconnect<'a>(
    args: &'a [&'a str],
    state: &'a Mutex<State>,
    access_svc: &'a AccessProxy,
) -> Result<String, Error> {
    match parse_disconnect(args, state) {
        Ok(id) => handle_disconnect(id, access_svc).await,
        Err(msg) => Ok(msg),
    }
}

fn parse_pairing_security_level(level: &str) -> Result<PairingSecurityLevel, String> {
    match level.to_ascii_uppercase().as_str() {
        "AUTH" => Ok(PairingSecurityLevel::Authenticated),
        "ENC" => Ok(PairingSecurityLevel::Encrypted),
        _ => {
            return Err(
                "Unable to pair: security level must be either \"AUTH\" or \"ENC\"".to_string()
            );
        }
    }
}

fn parse_bondable_mode(mode: &str) -> Result<BondableMode, String> {
    match mode.to_ascii_uppercase().as_str() {
        "T" => Ok(BondableMode::Bondable),
        "F" => Ok(BondableMode::NonBondable),
        _ => return Err("Bondable mode must be either \"T\" or \"F\"".to_string()),
    }
}

fn parse_pairing_transport(transport: &str) -> Result<TechnologyType, String> {
    match transport.to_ascii_uppercase().as_str() {
        "BREDR" | "CLASSIC" => Ok(TechnologyType::Classic),
        "LE" => Ok(TechnologyType::LowEnergy),
        _ => {
            return Err("If present, transport must be \"BREDR\"/\"CLASSIC\" or \"LE\"".to_string());
        }
    }
}

fn parse_pair(args: &[&str], state: &Mutex<State>) -> Result<(FidlPeerId, PairingOptions), String> {
    if args.len() < 3 || args.len() > 4 {
        return Err(format!("usage: {}", Cmd::Pair.usage()));
    }
    // `args[0]` is the identifier of the peer to connect to
    let peer_id = match to_identifier(state, args[0]) {
        Some(id) => id,
        None => return Err(format!("Unable to pair: invalid peer address: {}", args[0])),
    };
    let le_security_level = Some(parse_pairing_security_level(args[1])?);
    // `args[2]` is the requested bonding preference of the pairing
    let bondable_mode = Some(parse_bondable_mode(args[2])?);
    // if `args[3]` is present, it corresponds to the connected transport over which to pair
    let transport = if args.len() == 4 { Some(parse_pairing_transport(args[3])?) } else { None };
    Ok((
        peer_id.into(),
        PairingOptions { le_security_level, bondable_mode, transport, ..Default::default() },
    ))
}

async fn handle_pair(
    peer_id: FidlPeerId,
    pairing_opts: PairingOptions,
    access_svc: &AccessProxy,
) -> Result<String, Error> {
    match access_svc.pair(&peer_id, &pairing_opts).await? {
        Ok(_) => Ok(String::new()),
        Err(err) => Ok(format!("Pair error: {:?}", err)),
    }
}

async fn pair(
    args: &[&str],
    state: &Mutex<State>,
    access_svc: &AccessProxy,
) -> Result<String, Error> {
    match parse_pair(args, state) {
        Ok((peer_id, pairing_opts)) => handle_pair(peer_id, pairing_opts, access_svc).await,
        Err(e) => Ok(e),
    }
}

// Creates a pairing task with the given input / output capabilities which will prompt for consent
// to the pairing.  The returned task will continue prompting for requests until it is dropped.
fn create_pairing_task(
    input_cap: InputCapability,
    output_cap: OutputCapability,
    pairing_svc: &PairingProxy,
) -> Result<(fasync::Task<()>, mpsc::Receiver<(PeerId, bool)>), Error> {
    let (pairing_delegate_client, delegate_stream) =
        fidl::endpoints::create_request_stream::<PairingDelegateMarker>();
    let (sender, recv) = mpsc::channel(0);
    let pairing_delegate_server = pairing_delegate::handle_requests(delegate_stream, sender);

    let _ = pairing_svc.set_pairing_delegate(
        input_cap.into(),
        output_cap.into(),
        pairing_delegate_client,
    );

    let task = fasync::Task::spawn(pairing_delegate_server.map(|res| println!("{res:?}")));
    println!(
        "Pairing delegate setup with input capability {:?} and output capability {:?}.",
        input_cap, output_cap
    );
    Ok((task, recv))
}

async fn allow_pairing(args: &[&str], access_svc: &PairingProxy) -> Result<String, Error> {
    let (input_cap, output_cap) = match args.len() {
        0 => (InputCapability::None, OutputCapability::None),
        2 => (
            InputCapability::from_str(args[0]).map_err(|_| {
                format_err!("unknown input capability: {}", Cmd::AllowPairing.usage())
            })?,
            OutputCapability::from_str(args[1]).map_err(|_| {
                format_err!("unknown output capability: {}", Cmd::AllowPairing.usage())
            })?,
        ),
        _ => return Err(format_err!("usage: {}", Cmd::AllowPairing.usage())),
    };
    let (delegate_task, mut receiver) = create_pairing_task(input_cap, output_cap, access_svc)?;

    // TODO(fxbug.dev/443280885): Make repeated pairing attempts optional or
    // provide a mechanism for early exit without killing the entire process.
    while let Some((paired_id, paired)) = receiver.next().await {
        if !paired {
            println!("Pairing with {} was unsuccessful.", paired_id);
            continue;
        }

        let _ = delegate_task.abort();
        return Ok(format!("Completed successful pairing with {}.", paired_id));
    }
    Err(format_err!("Pairing delegate closed without a pairing"))
}

async fn forget<'a>(
    args: &'a [&'a str],
    state: &'a Mutex<State>,
    access_svc: &'a AccessProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", Cmd::Forget.usage()));
    }
    // `args[0]` is the identifier of the remote device to connect to
    let peer_id = match to_identifier(state, args[0]) {
        Some(id) => id,
        None => return Ok(format!("Unable to forget: Unknown address {}", args[0])),
    };
    match access_svc.forget(&peer_id.into()).await? {
        Ok(_) => {
            println!("Peer has been removed");
            Ok(String::new())
        }
        Err(err) => Ok(format!("Forget error: {:?}", err)),
    }
}

async fn set_discoverable(
    discoverable: bool,
    access_svc: &AccessProxy,
    state: &Mutex<State>,
) -> Result<String, Error> {
    if discoverable {
        println!("Becoming discoverable..");
        if state.lock().discoverable_token.is_some() {
            return Ok(String::new());
        }
        let (token, token_server) = fidl::endpoints::create_proxy();
        match access_svc.make_discoverable(token_server).await? {
            Ok(_) => {
                state.lock().discoverable_token = Some(token);
                Ok(String::new())
            }
            Err(err) => Ok(format!("MakeDiscoverable error: {:?}", err)),
        }
    } else {
        println!("Revoking discoverability..");
        state.lock().discoverable_token = None;
        Ok(String::new())
    }
}

fn print_peer_state_updates(state: &State, peer: &Peer) {
    if let Some(msg) = peer_state_updates(state, peer) {
        println!("{} {} {}", peer.id, peer.address, msg)
    }
}

fn peer_state_updates(state: &State, peer: &Peer) -> Option<String> {
    let previous = state.peers.get(&peer.id);
    let was_connected = previous.is_some_and(|p| p.connected);
    let was_bonded = previous.is_some_and(|p| p.bonded);

    let conn_str = match (was_connected, peer.connected) {
        (false, true) => Some("[connected]"),
        (true, false) => Some("[disconnected]"),
        _ => None,
    };
    let bond_str = match (was_bonded, peer.bonded) {
        (false, true) => Some("[bonded]"),
        (true, false) => Some("[unbonded]"),
        _ => None,
    };
    match (conn_str, bond_str) {
        (Some(a), Some(b)) => Some(format!("{} {}", a, b)),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    }
}

/// Tracks all state local to the command line tool.
pub struct State {
    pub peers: HashMap<PeerId, Peer>,
    pub discoverable_token: Option<ProcedureTokenProxy>,
    pub discovery_token: Option<ProcedureTokenProxy>,
    pub hosts: Vec<HostInfo>,
}

impl State {
    pub fn new() -> State {
        State {
            peers: HashMap::new(),
            discoverable_token: None,
            discovery_token: None,
            hosts: vec![],
        }
    }
}

/// Handles the commands entered into the bt-cli REPL.
struct BtCliHandler {
    access_svc: AccessProxy,
    host_svc: HostWatcherProxy,
    pairing_svc: PairingProxy,
    state: Arc<Mutex<State>>,
}

impl ReplHandler for BtCliHandler {
    type Command = Cmd;

    async fn handle(&mut self, cmd: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        let access_svc = &self.access_svc;
        let host_svc = &self.host_svc;
        let pairing_svc = &self.pairing_svc;
        let state = &self.state;
        let res = match cmd {
            Cmd::Connect => connect(args, state, access_svc, None).await,
            Cmd::Bond => connect(args, state, access_svc, Some(pairing_svc)).await,
            Cmd::Disconnect => disconnect(args, state, access_svc).await,
            Cmd::Pair => pair(args, state, access_svc).await,
            Cmd::AllowPairing => allow_pairing(args, pairing_svc).await,
            Cmd::Forget => forget(args, state, access_svc).await,
            Cmd::StartDiscovery => set_discovery(true, state, access_svc).await,
            Cmd::StopDiscovery => set_discovery(false, state, access_svc).await,
            Cmd::Discoverable => set_discoverable(true, access_svc, state).await,
            Cmd::NotDiscoverable => set_discoverable(false, access_svc, state).await,
            Cmd::ListPeers => Ok(get_peers(args, state, false)),
            Cmd::ShowPeers => Ok(get_peers(args, state, true)),
            Cmd::Peer => Ok(get_peer(args, state)),
            Cmd::ListControllers => get_hosts(state).await,
            Cmd::SetController => set_active_host(args, host_svc).await,
            Cmd::SetLocalName => set_local_name(args, access_svc).await,
            Cmd::SetDeviceClass => set_device_class(args, access_svc).await,
            Cmd::Controller => get_active_host(state).await,
        }?;
        if !res.is_empty() {
            println!("{}", res);
        }
        Ok(ReplControl::Continue)
    }
}

async fn watch_peers(access_svc: AccessProxy, state: Arc<Mutex<State>>) -> Result<(), Error> {
    // Used to avoid printing all peers on first watch_peers() call.
    let mut first_loop = true;
    loop {
        let (updated, removed) = access_svc.watch_peers().await?;
        for peer in updated.into_iter() {
            let peer = Peer::try_from(peer).context("Malformed FIDL peer")?;
            if !first_loop {
                print!("{}", CLEAR_LINE);
                print_peer_state_updates(&state.lock(), &peer);
            }
            let _ = state.lock().peers.insert(peer.id, peer);
        }
        for id in removed.into_iter() {
            let peer_id = PeerId::try_from(id).context("Malformed FIDL peer id")?;
            let _ = state.lock().peers.remove(&peer_id);
        }
        first_loop = false;
    }
}

async fn watch_hosts(host_svc: HostWatcherProxy, state: Arc<Mutex<State>>) -> Result<(), Error> {
    let mut first_result = true;
    loop {
        let fidl_hosts = host_svc.watch().await?;
        let mut hosts = Vec::<HostInfo>::new();
        if !first_result && !hosts.is_empty() {
            print!("{}", CLEAR_LINE);
        }
        for fidl_host in &fidl_hosts {
            let host = HostInfo::try_from(fidl_host)?;
            if !first_result {
                println!(
                    "Controller updated: [addresses: {}, active: {}, discoverable: {}, discovering: {}]",
                    addresses_to_custom_string(&host.addresses, " "),
                    host.active,
                    host.discoverable,
                    host.discovering
                );
            }
            hosts.push(host);
        }
        state.lock().hosts = hosts;
        first_result = false;
    }
}

#[fuchsia::main(logging = false)]
async fn main() -> Result<(), Error> {
    let access_svc = connect_to_protocol::<AccessMarker>()
        .context("failed to connect to bluetooth access interface")?;
    let host_watcher_svc =
        connect_to_protocol::<HostWatcherMarker>().context("failed to watch hosts")?;
    let pairing_svc = connect_to_protocol::<PairingMarker>()
        .context("failed to connect to bluetooth pairing interface")?;
    let state = Arc::new(Mutex::new(State::new()));
    let peer_watcher = watch_peers(access_svc.clone(), state.clone());
    let handler = BtCliHandler {
        access_svc,
        host_svc: host_watcher_svc.clone(),
        pairing_svc,
        state: state.clone(),
    };
    let repl = bt_repl::run(PROMPT, command_source(state.clone()), handler)
        .map_err(|e| e.context("REPL failed unexpectedly"));
    let host_watcher = watch_hosts(host_watcher_svc, state);
    let repl = pin!(repl);
    let peer_watcher = pin!(peer_watcher);
    select! {
        r = repl.fuse() => r,
        p = peer_watcher.fuse() => p,
        h = host_watcher.fuse() => h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use bt_fidl_mocks::sys::{AccessMock, PairingMock};
    use fidl::endpoints::Proxy;
    use fidl_fuchsia_bluetooth as fbt;
    use fidl_fuchsia_bluetooth_sys as fsys;
    use fidl_fuchsia_bluetooth_sys::{InputCapability, OutputCapability};
    use fuchsia_bluetooth::types::Address;
    use futures::join;
    use std::task::Poll;
    use zx::MonotonicDuration;

    fn peer(connected: bool, bonded: bool) -> Peer {
        Peer {
            id: PeerId(0xdeadbeef),
            address: Address::Public([1, 0, 0, 0, 0, 0]),
            technology: fsys::TechnologyType::LowEnergy,
            connected,
            bonded,
            name: None,
            appearance: Some(fbt::Appearance::Phone),
            device_class: None,
            rssi: None,
            tx_power: None,
            le_services: vec![],
            bredr_services: vec![],
        }
    }

    fn named_peer(id: PeerId, address: Address, name: Option<String>) -> Peer {
        Peer {
            id,
            address,
            technology: fsys::TechnologyType::LowEnergy,
            connected: false,
            bonded: false,
            name,
            appearance: Some(fbt::Appearance::Phone),
            device_class: None,
            rssi: None,
            tx_power: None,
            le_services: vec![],
            bredr_services: vec![],
        }
    }

    fn custom_peer(
        id: PeerId,
        address: Address,
        connected: bool,
        bonded: bool,
        rssi: Option<i8>,
    ) -> Peer {
        Peer {
            id,
            address,
            technology: fsys::TechnologyType::LowEnergy,
            connected,
            bonded,
            name: None,
            appearance: Some(fbt::Appearance::Phone),
            device_class: None,
            rssi,
            tx_power: None,
            le_services: vec![],
            bredr_services: vec![],
        }
    }

    fn custom_host(
        id: HostId,
        address: Address,
        active: bool,
        discoverable: bool,
        discovering: bool,
        name: Option<String>,
    ) -> HostInfo {
        HostInfo {
            id,
            technology: fsys::TechnologyType::LowEnergy,
            addresses: vec![address],
            active,
            local_name: name,
            discoverable,
            discovering,
        }
    }

    fn state_with(p: Peer) -> State {
        let mut state = State::new();
        let _ = state.peers.insert(p.id, p);
        state
    }

    #[fuchsia::test]
    async fn test_get_hosts() {
        // Fields for table view of hosts
        let fields = Regex::new(r"HostId[ \t]*\|[ \t]*Addresses[ \t]*\|[ \t]*Active[ \t]*\|[ \t]*Technology[ \t]*\|[ \t]*Name[ \t]*\|[ \t]*Discoverable[ \t]*\|[ \t]*Discovering").unwrap();

        // No hosts
        let mut output = get_hosts(&Mutex::new(State::new())).await.unwrap();
        assert!(!fields.is_match(&output));
        assert!(output.contains("No host instances"));

        let mut state = State::new();
        state.hosts.push(custom_host(
            HostId(0xbeef),
            Address::Public([0x11, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
            false,
            false,
            false,
            Some("Sapphire".to_string()),
        ));
        state.hosts.push(custom_host(
            HostId(0xabcd),
            Address::Random([0x22, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
            false,
            false,
            true,
            None,
        ));
        let state = Mutex::new(state);

        // Hosts exist
        output = get_hosts(&state).await.unwrap();
        assert!(fields.is_match(&output));
        assert!(output.contains("ef"));
        assert!(output.contains("cd"));
    }

    #[test]
    fn test_match_peer() {
        let nameless_peer =
            named_peer(PeerId(0xabcd), Address::Public([0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]), None);
        let named_peer = named_peer(
            PeerId(0xbeef),
            Address::Public([0x11, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
            Some("Sapphire".to_string()),
        );

        assert!(match_peer("23", &nameless_peer));
        assert!(!match_peer("23", &named_peer));

        assert!(match_peer("cd", &nameless_peer));
        assert!(match_peer("bee", &named_peer));
        assert!(match_peer("BEE", &named_peer));

        assert!(!match_peer("Sapphire", &nameless_peer));
        assert!(match_peer("Sapphire", &named_peer));
        assert!(match_peer("sapphire", &named_peer));
        assert!(match_peer("SAPPHIRE", &named_peer));
        assert!(match_peer("sApPh", &named_peer));

        assert!(match_peer("", &nameless_peer));
        assert!(match_peer("", &named_peer));

        assert!(match_peer("DE", &named_peer));
        assert!(match_peer("de", &named_peer));
    }

    #[test]
    fn test_get_peers_full_details() {
        let mut state = State::new();
        let _ = state.peers.insert(
            PeerId(0xabcd),
            named_peer(PeerId(0xabcd), Address::Public([0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]), None),
        );
        let _ = state.peers.insert(
            PeerId(0xbeef),
            named_peer(
                PeerId(0xbeef),
                Address::Public([0x11, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
                Some("Sapphire".to_string()),
            ),
        );
        let state = Mutex::new(state);

        let get_peers =
            |args: &[&str], state: &Mutex<State>| -> String { get_peers(args, state, true) };

        // Fields for detailed view of peers
        let fields = Regex::new(r"Id(?s).*Address(?s).*Technology(?s).*Name(?s).*Appearance(?s).*Connected(?s).*Bonded(?s).*LE Services(?s).*BR/EDR Serv\.").unwrap();

        // Default (empty arguments) shows all peers, with named peers listed last.
        let default_output = get_peers(&[], &state);
        assert!(fields.is_match(&default_output));
        assert!(default_output.contains("Showing 2/2 peers\n"));
        let unnamed_pos = default_output.find("01:23:45").unwrap();
        let named_pos = default_output.find("AD:DE:7E").unwrap();
        assert!(unnamed_pos < named_pos);

        // --named-only / -n hides unnamed peers.
        for flag in ["--named-only", "-n"] {
            let named_only_output = get_peers(&[flag], &state);
            assert!(fields.is_match(&named_only_output));
            assert!(named_only_output.contains("Showing 1/2 peers\n"));
            assert!(!named_only_output.contains("01:23:45"));
            assert!(named_only_output.contains("AD:DE:7E"));
        }

        // No matches prints nothing.
        assert!(!fields.is_match(&get_peers(&["nomatch"], &state)));
        assert!(get_peers(&["nomatch"], &state).contains("Showing 0/2 peers\n"));
        assert!(!get_peers(&["nomatch"], &state).contains("01:23:45"));
        assert!(!get_peers(&["nomatch"], &state).contains("AD:DE:7E"));

        // We can match either peer without --named-only, and combine --named-only with a filter.
        assert!(get_peers(&["01:23"], &state).contains("Showing 1/2 peers\n"));
        assert!(get_peers(&["01:23"], &state).contains("01:23:45"));
        assert!(get_peers(&["abcd"], &state).contains("Showing 1/2 peers\n"));
        assert!(get_peers(&["beef"], &state).contains("AD:DE:7E"));
        assert!(get_peers(&["--named-only", "Sapphire"], &state).contains("AD:DE:7E"));
        assert!(get_peers(&["Sapphire", "-n"], &state).contains("AD:DE:7E"));
        assert_eq!(get_peers(&["--named-only", "01:23"], &state), "Showing 0/2 peers\n");
        assert_eq!(get_peers(&["01:23", "-n"], &state), "Showing 0/2 peers\n");

        // Unrecognized flags or extra positional arguments return usage.
        let expected_usage = format!("usage: {}", Cmd::ShowPeers.usage());
        assert_eq!(get_peers(&["--unknown"], &state), expected_usage);
        assert_eq!(get_peers(&["--all"], &state), expected_usage);
        assert_eq!(get_peers(&["01:23", "beef"], &state), expected_usage);
    }

    #[test]
    fn test_get_peers_less_details() {
        let mut state = State::new();
        let _ = state.peers.insert(
            PeerId(0xabcd),
            named_peer(PeerId(0xabcd), Address::Public([0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]), None),
        );
        let _ = state.peers.insert(
            PeerId(0xbeef),
            named_peer(
                PeerId(0xbeef),
                Address::Public([0x11, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
                Some("Sapphire".to_string()),
            ),
        );
        let state = Mutex::new(state);

        let get_peers =
            |args: &[&str], state: &Mutex<State>| -> String { get_peers(args, state, false) };

        // Fields for table view of peers
        let fields = Regex::new(r"PeerId[ \t]*\|[ \t]*Address[ \t]*\|[ \t]*Technology[ \t]*\|[ \t]*Name[ \t]*\|[ \t]*Appearance[ \t]*\|[ \t]*Connected[ \t]*\|[ \t]*Bonded").unwrap();

        // Default (empty arguments) shows all peers without extra lines between rows, with named
        // peers listed last.
        let default_output = get_peers(&[], &state);
        assert!(fields.is_match(&default_output));
        assert!(default_output.starts_with("Showing 2/2 peers\n"));
        let lines: Vec<&str> = default_output.lines().collect();
        // Summary line, title row, single header separator, and 2 peer rows (no blank line or
        // inter-row separators).
        assert_eq!(lines.len(), 5);
        assert!(lines[3].contains("01:23:45"));
        assert!(lines[4].contains("AD:DE:7E"));

        // --named-only / -n hides unnamed peers.
        for flag in ["--named-only", "-n"] {
            let named_only_output = get_peers(&[flag], &state);
            assert!(fields.is_match(&named_only_output));
            assert!(named_only_output.starts_with("Showing 1/2 peers\n"));
            assert!(!named_only_output.contains("01:23:45"));
            assert!(named_only_output.contains("AD:DE:7E"));
            assert_eq!(named_only_output.lines().count(), 4);
        }

        // No matches prints nothing.
        assert!(!fields.is_match(&get_peers(&["nomatch"], &state)));
        assert!(get_peers(&["nomatch"], &state).contains("Showing 0/2 peers\n"));
        assert!(!get_peers(&["nomatch"], &state).contains("01:23:45"));
        assert!(!get_peers(&["nomatch"], &state).contains("AD:DE:7E"));

        // We can match either peer without --named-only, and combine --named-only with a filter.
        assert!(get_peers(&["01:23"], &state).contains("Showing 1/2 peers\n"));
        assert!(get_peers(&["01:23"], &state).contains("01:23:45"));
        assert!(get_peers(&["abcd"], &state).contains("Showing 1/2 peers\n"));
        assert!(get_peers(&["beef"], &state).contains("AD:DE:7E"));
        assert!(get_peers(&["--named-only", "Sapphire"], &state).contains("AD:DE:7E"));
        assert!(get_peers(&["Sapphire", "-n"], &state).contains("AD:DE:7E"));
        assert_eq!(get_peers(&["--named-only", "01:23"], &state), "Showing 0/2 peers\n");
        assert_eq!(get_peers(&["01:23", "-n"], &state), "Showing 0/2 peers\n");

        // Unrecognized flags or extra positional arguments return usage.
        let expected_usage = format!("usage: {}", Cmd::ListPeers.usage());
        assert_eq!(get_peers(&["-z"], &state), expected_usage);
        assert_eq!(get_peers(&["--all"], &state), expected_usage);
        assert_eq!(get_peers(&["01:23", "beef"], &state), expected_usage);
    }

    #[fuchsia::test]
    fn test_get_peers_named_only_and_sorting() {
        // Empty peer map still reports no known peers.
        let empty_state = Mutex::new(State::new());
        assert_eq!(get_peers(&[], &empty_state, false), "No known peers");
        assert_eq!(get_peers(&["--named-only"], &empty_state, false), "No known peers");

        // All peers unnamed: default shows all of them; --named-only hides all of them.
        let mut only_unnamed = State::new();
        let _ = only_unnamed.peers.insert(
            PeerId(0x1001),
            named_peer(PeerId(0x1001), Address::Public([1, 0, 0, 0, 0, 0]), None),
        );
        let _ = only_unnamed.peers.insert(
            PeerId(0x1002),
            named_peer(PeerId(0x1002), Address::Public([2, 0, 0, 0, 0, 0]), None),
        );
        let _ = only_unnamed.peers.insert(
            PeerId(0x1003),
            named_peer(PeerId(0x1003), Address::Public([3, 0, 0, 0, 0, 0]), None),
        );
        let only_unnamed = Mutex::new(only_unnamed);
        assert!(get_peers(&[], &only_unnamed, false).starts_with("Showing 3/3 peers\n"));
        assert_eq!(get_peers(&["--named-only"], &only_unnamed, false), "Showing 0/3 peers\n");
        assert_eq!(get_peers(&["-n"], &only_unnamed, false), "Showing 0/3 peers\n");

        // Mixed unnamed and named peers: named peers always sort last (after unnamed peers, even
        // connected + bonded ones), and connected + bonded peers sort last within each group.
        let mut mixed = State::new();
        let _ = mixed.peers.insert(
            PeerId(0x2000),
            custom_peer(PeerId(0x2000), Address::Public([0xDD, 0, 0, 0, 0, 0]), false, false, None),
        );
        let _ = mixed.peers.insert(
            PeerId(0x2003),
            custom_peer(PeerId(0x2003), Address::Public([0xCC, 0, 0, 0, 0, 0]), true, true, None),
        );
        let _ = mixed.peers.insert(
            PeerId(0x2001),
            named_peer(
                PeerId(0x2001),
                Address::Public([0xAA, 0, 0, 0, 0, 0]),
                Some("Headphones".to_string()),
            ),
        );
        let mut connected_named = named_peer(
            PeerId(0x2002),
            Address::Public([0xBB, 0, 0, 0, 0, 0]),
            Some("Speaker".to_string()),
        );
        connected_named.connected = true;
        connected_named.bonded = true;
        let _ = mixed.peers.insert(PeerId(0x2002), connected_named);
        let mixed = Mutex::new(mixed);

        for full_details in [false, true] {
            let all_output = get_peers(&[], &mixed, full_details);
            assert!(all_output.starts_with("Showing 4/4 peers\n"));
            let unnamed_disconnected_pos = all_output.find("00:00:00:00:00:DD").unwrap();
            let unnamed_connected_pos = all_output.find("00:00:00:00:00:CC").unwrap();
            let headphones_pos = all_output.find("Headphones").unwrap();
            let speaker_pos = all_output.find("Speaker").unwrap();
            assert!(unnamed_disconnected_pos < unnamed_connected_pos);
            assert!(unnamed_connected_pos < headphones_pos);
            assert!(headphones_pos < speaker_pos);

            let named_output = get_peers(&["--named-only"], &mixed, full_details);
            assert!(named_output.starts_with("Showing 2/4 peers\n"));
            assert!(!named_output.contains("00:00:00:00:00:DD"));
            assert!(!named_output.contains("00:00:00:00:00:CC"));
            assert!(
                named_output.find("Headphones").unwrap() < named_output.find("Speaker").unwrap()
            );
        }
    }

    #[fuchsia::test]
    fn test_get_peers_case_insensitive_name() {
        let mut state = State::new();
        let _ = state.peers.insert(
            PeerId(0xabcd),
            named_peer(
                PeerId(0xabcd),
                Address::Public([0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]),
                Some("MOMENTUM 5b".to_string()),
            ),
        );
        let _ = state.peers.insert(
            PeerId(0xbeef),
            named_peer(
                PeerId(0xbeef),
                Address::Public([0x11, 0x00, 0x55, 0x7E, 0xDE, 0xAD]),
                Some("Sapphire".to_string()),
            ),
        );
        let state = Mutex::new(state);

        for full_details in [false, true] {
            for filter in ["Moment", "moment", "MOMENT", "5B", "5b"] {
                let output = get_peers(&[filter], &state, full_details);
                assert!(output.contains("1/2 peers"));
                assert!(output.contains("MOMENTUM 5b"));
                assert!(!output.contains("Sapphire"));
            }

            for filter in ["sapphire", "SAPPHIRE", "sApPhIrE"] {
                let output = get_peers(&[filter], &state, full_details);
                assert!(output.contains("1/2 peers"));
                assert!(output.contains("Sapphire"));
                assert!(!output.contains("MOMENTUM 5b"));
            }
        }
    }

    #[test]
    fn cmp_peers_correctly_orders_peers() {
        // Sorts named peers after unnamed peers (at the bottom of the list), both with matching
        // connection state and when the unnamed peer is connected and bonded.
        let named = named_peer(
            PeerId(0x1111),
            Address::Public([3, 0, 0, 0, 0, 0]),
            Some("Sapphire".to_string()),
        );
        let unnamed_disconnected =
            custom_peer(PeerId(0x2221), Address::Public([5, 0, 0, 0, 0, 0]), false, false, None);
        let unnamed_connected =
            custom_peer(PeerId(0x2222), Address::Public([4, 0, 0, 0, 0, 0]), true, true, None);
        assert_eq!(cmp_peers(&unnamed_disconnected, &named), Ordering::Less);
        assert_eq!(cmp_peers(&named, &unnamed_disconnected), Ordering::Greater);
        assert_eq!(cmp_peers(&unnamed_connected, &named), Ordering::Less);
        assert_eq!(cmp_peers(&named, &unnamed_connected), Ordering::Greater);

        // Sorts connected correctly
        let peer_a =
            custom_peer(PeerId(0xbeef), Address::Public([1, 0, 0, 0, 0, 0]), false, false, None);
        let peer_b =
            custom_peer(PeerId(0xbaaf), Address::Public([2, 0, 0, 0, 0, 0]), true, false, None);
        assert_eq!(cmp_peers(&peer_a, &peer_b), Ordering::Less);

        // Sorts bonded correctly
        let peer_a =
            custom_peer(PeerId(0xbeef), Address::Public([1, 0, 0, 0, 0, 0]), false, false, None);
        let peer_b =
            custom_peer(PeerId(0xbaaf), Address::Public([2, 0, 0, 0, 0, 0]), false, true, None);
        assert_eq!(cmp_peers(&peer_a, &peer_b), Ordering::Less);
    }

    #[test]
    fn test_peer_updates() {
        // Expected Value Table
        // each row lists:
        //   (current peer conn/bond state, new peer conn/bond state, expected string)
        let test_cases = vec![
            // missing
            (None, (true, false), Some("[connected]")),
            (None, (true, true), Some("[connected] [bonded]")),
            (None, (false, true), Some("[bonded]")),
            (None, (false, false), None),
            // disconnected, unbonded
            (Some((false, false)), (true, false), Some("[connected]")),
            (Some((false, false)), (true, true), Some("[connected] [bonded]")),
            (Some((false, false)), (false, true), Some("[bonded]")),
            (Some((false, false)), (false, false), None),
            // connected, unbonded
            (Some((true, false)), (true, false), None),
            (Some((true, false)), (true, true), Some("[bonded]")),
            (Some((true, false)), (false, true), Some("[disconnected] [bonded]")),
            (Some((true, false)), (false, false), Some("[disconnected]")),
            // disconnected, bonded
            (Some((false, true)), (true, false), Some("[connected] [unbonded]")),
            (Some((false, true)), (true, true), Some("[connected]")),
            (Some((false, true)), (false, true), None),
            (Some((false, true)), (false, false), Some("[unbonded]")),
            // connected, bonded
            (Some((true, true)), (true, false), Some("[unbonded]")),
            (Some((true, true)), (true, true), None),
            (Some((true, true)), (false, true), Some("[disconnected]")),
            (Some((true, true)), (false, false), Some("[disconnected] [unbonded]")),
        ];

        for case in test_cases {
            let (prev, (connected, bonded), expected) = case;
            let state = match prev {
                Some((c, b)) => state_with(peer(c, b)),
                None => State::new(),
            };
            assert_eq!(
                peer_state_updates(&state, &peer(connected, bonded)),
                expected.map(|s| s.to_string())
            );
        }
    }

    // Test that command lines entered parse correctly to the expected disconnect calls
    #[test]
    fn test_parse_disconnect() {
        let state = Mutex::new(state_with(peer(true, false)));
        let cases = vec![
            // valid peer id
            ("disconnect deadbeef", Ok(PeerId(0xdeadbeef))),
            // unknown address
            (
                "disconnect 00:00:00:00:00:00",
                Err("Unable to disconnect: Unknown address 00:00:00:00:00:00".to_string()),
            ),
            // known address
            ("disconnect 00:00:00:00:00:01", Ok(PeerId(0xdeadbeef))),
            // no id param
            ("disconnect", Err(format!("usage: {}", Cmd::Disconnect.usage()))),
        ];
        for (line, expected) in cases {
            assert_eq!(parse_disconnect_id(line, &state), expected);
        }
    }

    /// Splits `line` into a command and its arguments, as the REPL does.
    fn parse_cmd(line: &str) -> Option<(Cmd, Vec<&str>)> {
        let mut words = line.split_whitespace();
        let cmd = words.next()?.parse::<Cmd>().ok()?;
        Some((cmd, words.collect()))
    }

    fn parse_disconnect_id(line: &str, state: &Mutex<State>) -> Result<PeerId, String> {
        let args = match parse_cmd(line) {
            Some((Cmd::Disconnect, args)) => Ok(args),
            Some((_, _)) => Err("Command is not disconnect"),
            None => Err("failed"),
        }?;
        parse_disconnect(&args, state)
    }

    // Tests that command lines entered parse correctly to the expected pairing calls
    #[test]
    fn test_parse_pairing_security_level() {
        let cases = vec![
            ("Enc", Ok(PairingSecurityLevel::Encrypted)),
            ("AUTH", Ok(PairingSecurityLevel::Authenticated)),
            (
                "SC",
                Err("Unable to pair: security level must be either \"AUTH\" or \"ENC\"".to_string()),
            ),
        ];
        for (input_str, expected) in cases {
            assert_eq!(parse_pairing_security_level(input_str), expected);
        }
    }

    #[test]
    fn test_parse_bondable_mode() {
        let cases = vec![
            ("T", Ok(BondableMode::Bondable)),
            ("f", Ok(BondableMode::NonBondable)),
            ("TEST", Err("Bondable mode must be either \"T\" or \"F\"".to_string())),
        ];
        for (input_str, expected) in cases {
            assert_eq!(parse_bondable_mode(input_str), expected);
        }
    }

    #[test]
    fn test_parse_pairing_transport() {
        let cases = vec![
            ("CLAssIC", Ok(TechnologyType::Classic)),
            ("BrEdr", Ok(TechnologyType::Classic)),
            ("LE", Ok(TechnologyType::LowEnergy)),
            (
                "TEST",
                Err("If present, transport must be \"BREDR\"/\"CLASSIC\" or \"LE\"".to_string()),
            ),
        ];
        for (input_str, expected) in cases {
            assert_eq!(parse_pairing_transport(input_str), expected);
        }
    }
    #[test]
    fn test_parse_pair() {
        let state = Mutex::new(state_with(custom_peer(
            PeerId(0xbeef),
            Address::Public([1, 0, 0, 0, 0, 0]),
            true,
            false,
            None,
        )));
        let cases = vec![
            // valid peer id
            (
                "pair beef ENC T LE",
                Ok((
                    FidlPeerId { value: u64::from_str_radix("beef", 16).unwrap() },
                    PairingOptions {
                        le_security_level: Some(PairingSecurityLevel::Encrypted),
                        bondable_mode: Some(BondableMode::Bondable),
                        transport: Some(TechnologyType::LowEnergy),
                        ..Default::default()
                    },
                )),
            ),
            // known address, no transport
            (
                "pair 00:00:00:00:00:01 AUTH F",
                Ok((
                    FidlPeerId { value: u64::from_str_radix("beef", 16).unwrap() },
                    PairingOptions {
                        le_security_level: Some(PairingSecurityLevel::Authenticated),
                        bondable_mode: Some(BondableMode::NonBondable),
                        transport: None,
                        ..Default::default()
                    },
                )),
            ),
            // no id param
            ("pair", Err(format!("usage: {}", Cmd::Pair.usage()))),
        ];
        for (line, expected) in cases {
            assert_eq!(parse_pair_args(line, &state), expected);
        }
    }

    fn parse_pair_args(
        line: &str,
        state: &Mutex<State>,
    ) -> Result<(FidlPeerId, PairingOptions), String> {
        let args = match parse_cmd(line) {
            Some((Cmd::Pair, args)) => Ok(args),
            Some((_, _)) => Err("Command is not pair"),
            None => Err("failed"),
        }?;
        parse_pair(&args, state)
    }

    fn timeout() -> MonotonicDuration {
        MonotonicDuration::from_seconds(20)
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_disconnect() {
        let peer = peer(true, false);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let args = vec![peer_id_string.as_str()];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) = AccessMock::new(timeout()).expect("failed to create mock");

        let cmd = disconnect(args.as_slice(), &state, &proxy);
        let mock_expect = mock.expect_disconnect(peer_id.into(), Ok(()));
        let (result, mock_result) = join!(cmd, mock_expect);

        let _ = mock_result.expect("mock FIDL expectation not satisfied");
        assert_eq!("".to_string(), result.expect("expected success"));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_disconnect_error() {
        let peer = peer(true, false);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let args = vec![peer_id_string.as_str()];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) = AccessMock::new(timeout()).expect("failed to create mock");

        let cmd = disconnect(args.as_slice(), &state, &proxy);
        let mock_expect = mock.expect_disconnect(peer_id.into(), Err(fsys::Error::Failed));
        let (result, mock_result) = join!(cmd, mock_expect);

        let _ = mock_result.expect("mock FIDL expectation not satisfied");
        assert!(result.expect("expected a result").contains("Disconnect error"));
    }

    #[test]
    fn test_allow_pairing_no_args() {
        let mut exec = fasync::TestExecutor::new();

        let args = vec![];
        let (proxy, mut mock) =
            PairingMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");
        let pair = allow_pairing(args.as_slice(), &proxy);
        let mut pair = pin!(pair);

        assert!(exec.run_until_stalled(&mut pair).is_pending());

        let mock_expect =
            mock.expect_set_pairing_delegate(InputCapability::None, OutputCapability::None);

        assert!(exec.run_singlethreaded(mock_expect).is_ok());
    }

    #[fuchsia::test]
    fn test_allow_pairing_args() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut mock) =
            PairingMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        // Enable pairing with confirmation input cap and display output cap.
        let args = vec!["confirmation", "display"];
        let pair = allow_pairing(args.as_slice(), &proxy);
        let mut pair = pin!(pair);

        // Should be waiting until pairing is completed.
        assert!(exec.run_until_stalled(&mut pair).is_pending());

        // Expect pairing delegate to be set with the correct capabilities.
        let proxy = exec
            .run_singlethreaded(mock.expect_set_pairing_delegate(
                InputCapability::Confirmation,
                OutputCapability::Display,
            ))
            .expect("cannot get proxy");
        assert!(!proxy.is_closed());

        // Force closing of the proxy.
        std::mem::drop(proxy);

        // Verify that allow_pairing existed with error.
        assert_matches!(exec.run_until_stalled(&mut pair), Poll::Ready(Err(_)));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_allow_pairing_error() {
        // Arguments that don't correspond to any capabilities.
        let args = vec!["nonsense", "fake"];
        let (proxy, _mock) =
            PairingMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        assert!(allow_pairing(args.as_slice(), &proxy).await.is_err());

        // Incorrect number of arguments.
        let args = vec!["none"];
        let (proxy, _mock) =
            PairingMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        assert!(allow_pairing(args.as_slice(), &proxy).await.is_err());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_forget() {
        let peer = peer(true, false);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let args = vec![peer_id_string.as_str()];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) =
            AccessMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        let cmd = forget(args.as_slice(), &state, &proxy);
        let mock_expect = mock.expect_forget(peer_id.into(), Ok(()));
        let (result, mock_result) = join!(cmd, mock_expect);

        assert!(mock_result.is_ok());
        assert_eq!("".to_string(), result.expect("expected success"));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_forget_error() {
        let peer = peer(true, false);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let args = vec![peer_id_string.as_str()];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) =
            AccessMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        let cmd = forget(args.as_slice(), &state, &proxy);
        let mock_expect = mock.expect_forget(peer_id.into(), Err(fsys::Error::Failed));
        let (result, mock_result) = join!(cmd, mock_expect);

        assert!(mock_result.is_ok());
        assert!(result.expect("expected a result").contains("Forget error"));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_pair() {
        let peer =
            custom_peer(PeerId(0xbeef), Address::Public([1, 0, 0, 0, 0, 0]), true, false, None);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let pairing_options = PairingOptions {
            le_security_level: Some(PairingSecurityLevel::Encrypted),
            bondable_mode: Some(BondableMode::Bondable),
            transport: None,
            ..Default::default()
        };

        let args = vec![peer_id_string.as_str(), "ENC", "T"];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) =
            AccessMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        let cmd = pair(args.as_slice(), &state, &proxy);
        let mock_expect = mock.expect_pair(peer_id.into(), pairing_options, Ok(()));
        let (result, mock_result) = join!(cmd, mock_expect);

        assert!(mock_result.is_ok());
        assert_eq!("".to_string(), result.expect("expected success"));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_pair_error() {
        let peer =
            custom_peer(PeerId(0xbeef), Address::Public([1, 0, 0, 0, 0, 0]), true, false, None);
        let peer_id = peer.id;
        let peer_id_string = peer.id.to_string();
        let pairing_options = PairingOptions {
            le_security_level: Some(PairingSecurityLevel::Encrypted),
            bondable_mode: Some(BondableMode::Bondable),
            transport: None,
            ..Default::default()
        };

        let args = vec![peer_id_string.as_str(), "ENC", "T"];
        let state = Mutex::new(state_with(peer));
        let (proxy, mut mock) =
            AccessMock::new(MonotonicDuration::from_seconds(1)).expect("failed to create mock");

        let cmd = pair(args.as_slice(), &state, &proxy);
        let mock_expect =
            mock.expect_pair(peer_id.into(), pairing_options, Err(fsys::Error::Failed));
        let (result, mock_result) = join!(cmd, mock_expect);

        assert!(mock_result.is_ok());
        assert!(result.expect("expected a result").contains("Pair error"));
    }

    #[fuchsia::test]
    async fn handle_dispatches_commands() {
        let (access_svc, _access_server) = fidl::endpoints::create_proxy::<AccessMarker>();
        let (host_svc, _host_server) = fidl::endpoints::create_proxy::<HostWatcherMarker>();
        let (pairing_svc, _pairing_server) = fidl::endpoints::create_proxy::<PairingMarker>();
        let state = Arc::new(Mutex::new(State::new()));
        let mut handler = BtCliHandler { access_svc, host_svc, pairing_svc, state };

        assert_eq!(handler.handle(Cmd::ListPeers, &[]).await.unwrap(), ReplControl::Continue);
        assert_eq!(handler.handle(Cmd::Controller, &[]).await.unwrap(), ReplControl::Continue);
        assert!(handler.handle(Cmd::SetController, &[]).await.is_err());
    }
}
