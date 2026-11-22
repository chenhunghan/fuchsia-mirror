// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::{CommandSource, Commands, gen_commands};
use fuchsia_sync::Mutex;
use std::sync::Arc;

use crate::State;

// `Cmd` is the declarative specification of all commands that bt-cli accepts.
gen_commands! {
    pub enum Cmd {
        Connect = (
            "connect",
            [],
            ["id|addr"],
            "connect to a peer",
        ),
        Bond = (
            "bond",
            [],
            ["id|addr"],
            "connect to a peer, accepting a pairing with that peer",
        ),
        Disconnect = (
            "disconnect",
            [],
            ["id|addr"],
            "disconnect from a peer",
        ),
        Pair = (
            "pair",
            [],
            [
                "id|addr",
                "security_level",
                "bondable_mode",
                "transport (optional)",
            ],
            "pair to a peer",
        ),
        AllowPairing = (
            "allow-pairing",
            [],
            [
                "input cap. (none|confirmation|keyboard)",
                "output cap. (none|display)",
            ],
            "Enable pairing mode to allow incoming pairing requests. \
             Capabilities default to none if no arguments are passed in.",
        ),
        Forget = (
            "forget",
            [],
            ["id|addr"],
            "delete and disconnect a remote peer",
        ),
        Controller = (
            "controller",
            [],
            [],
            "Show the active controller",
        ),
        SetController = (
            "set-controller",
            [],
            ["id"],
            "Set the active controller",
        ),
        SetLocalName = (
            "set-local-name",
            [],
            ["name"],
            "Set the name of the active controller",
        ),
        SetDeviceClass = (
            "set-device-class",
            [],
            [
                "Major Class",
                "Minor Class",
                "Service Classes...",
            ],
            "Set the device class of the active controller",
        ),
        ListControllers = (
            "list-controllers",
            [],
            [],
            "List all known bluetooth controllers",
        ),
        ListPeers = (
            "list-peers",
            ["--named-only|-n"],
            ["filter"],
            "Show all known peers in a summarized view \
             (pass --named-only|-n to hide unnamed peers; optionally filtered)",
        ),
        ShowPeers = (
            "show-peers",
            ["--named-only|-n"],
            ["filter"],
            "Show details for all known peers \
             (pass --named-only|-n to hide unnamed peers; optionally filtered)",
        ),
        Peer = (
            "peer",
            [],
            ["id|addr"],
            "Show details for a known peer",
        ),
        StartDiscovery = (
            "start-discovery",
            [],
            [],
            "Start Discovery",
        ),
        StopDiscovery = (
            "stop-discovery",
            [],
            [],
            "Stop Discovery",
        ),
        Discoverable = (
            "discoverable",
            [],
            [],
            "Set this device to be discoverable",
        ),
        NotDiscoverable = (
            "not-discoverable",
            [],
            [],
            "Revoke device discoverability",
        ),
    }
}

impl Cmd {
    /// Returns whether this command takes the identifier or address of a known
    /// peer as its first argument.
    const fn completes_peer(self) -> bool {
        match self {
            Self::Connect
            | Self::Bond
            | Self::Disconnect
            | Self::Pair
            | Self::Forget
            | Self::Peer => true,
            Self::AllowPairing
            | Self::Controller
            | Self::SetController
            | Self::SetLocalName
            | Self::SetDeviceClass
            | Self::ListControllers
            | Self::ListPeers
            | Self::ShowPeers
            | Self::StartDiscovery
            | Self::StopDiscovery
            | Self::Discoverable
            | Self::NotDiscoverable => false,
        }
    }
}

/// Returns a [`CommandSource`] for the bt-cli commands that completes the
/// identifiers and addresses of known peers for commands that take a peer.
pub fn command_source(state: Arc<Mutex<State>>) -> impl CommandSource {
    Commands::<Cmd>::new().with_arg_completer(move |command, partial| {
        if !command.completes_peer() {
            return Vec::new();
        }
        state
            .lock()
            .peers
            .values()
            .flat_map(|peer| [peer.id.to_string(), peer.address.as_hex_string()])
            .filter(|candidate| candidate.starts_with(partial))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_repl::CommandSet as _;
    use fidl_fuchsia_bluetooth_sys as fsys;
    use fuchsia_bluetooth::types::{Address, Peer, PeerId};

    fn state_with_peer() -> (Arc<Mutex<State>>, Peer) {
        let peer = Peer {
            id: PeerId(0xdeadbeef),
            address: Address::Public([1, 0, 0, 0, 0, 0]),
            technology: fsys::TechnologyType::LowEnergy,
            connected: false,
            bonded: false,
            name: None,
            appearance: None,
            device_class: None,
            rssi: None,
            tx_power: None,
            le_services: vec![],
            bredr_services: vec![],
        };
        let mut state = State::new();
        let _ = state.peers.insert(peer.id, peer.clone());
        (Arc::new(Mutex::new(state)), peer)
    }

    #[fuchsia::test]
    fn completes_known_peers_for_peer_commands() {
        let (state, peer) = state_with_peer();
        let source = command_source(state);
        let expected = vec![peer.id.to_string(), peer.address.as_hex_string()];
        for command in Cmd::all().into_iter().filter(|cmd| cmd.completes_peer()) {
            let name = command.to_string();
            assert_eq!(source.complete_argument(&name, ""), expected);
            assert_eq!(source.complete_argument(&name, "00000000dead"), vec![peer.id.to_string()]);
            assert_eq!(
                source.complete_argument(&name, "00:00:"),
                vec![peer.address.as_hex_string()]
            );
            assert!(source.complete_argument(&name, "ff").is_empty());
        }
    }

    #[fuchsia::test]
    fn does_not_complete_arguments_for_other_commands() {
        let (state, _peer) = state_with_peer();
        let source = command_source(state);
        for command in Cmd::all().into_iter().filter(|cmd| !cmd.completes_peer()) {
            assert!(source.complete_argument(&command.to_string(), "").is_empty());
        }
        assert!(source.complete_argument("bogus", "").is_empty());
    }

    #[fuchsia::test]
    fn provides_command_names() {
        let source = command_source(Arc::new(Mutex::new(State::new())));
        assert!(source.command_names().contains(&"discoverable".to_string()));
        assert_eq!(source.hint("peer"), Some("<id|addr> ".to_string()));
        assert_eq!(source.hint("list-peers"), Some("[--named-only|-n] <filter> ".to_string()));
        assert_eq!(source.hint("show-peers"), Some("[--named-only|-n] <filter> ".to_string()));
    }
}
