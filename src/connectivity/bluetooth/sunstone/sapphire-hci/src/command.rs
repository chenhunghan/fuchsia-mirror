// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::num::NonZeroU8;

use sapphire_async::pool::ObjectGuard;
use sapphire_async::rpc::Server;
use sapphire_collections::vec::StackVec;
use sapphire_emboss::OkState;
use sapphire_emboss::hci_common::CommandPacket;
use sapphire_rpc_macro::rpc;

use crate::hci::{CommandRpcChannel, HciCfg, HciError, HciEvent, HciEventRouter};
use crate::transport::TransportError;

/// Maximum size of an HCI command packet.
pub use sapphire_emboss::hci_common::command_packet::MAX_SIZE_IN_BYTES as MAX_COMMAND_SIZE;

/// Pool-allocated storage for one HCI command packet.
pub type CommandBuffer<'buf, PCfg> = ObjectGuard<'buf, StackVec<u8, MAX_COMMAND_SIZE>, PCfg>;

/// An HCI command packet with a validated header, sent through a
/// [`CommandClient`](crate::hci::CommandClient).
pub type OutgoingCommandPacket<'buf, PCfg> = CommandPacket<CommandBuffer<'buf, PCfg>, OkState>;

/// Reasons a single command failed. None of them are fatal to
/// [`HciRunner::run`](crate::hci::HciRunner::run).
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The command could not be written to the transport.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    /// The controller rejected the command with this Command_Status error code.
    #[error("controller rejected the command with status {0:#04x}")]
    Status(NonZeroU8),
    /// The controller answered with Command_Complete where Command_Status was expected, or with a
    /// successful Command_Status where Command_Complete was expected.
    #[error("unexpected command response")]
    UnexpectedResponse,
}

/// Sends HCI commands, one at a time, and waits for their Command_Complete / Command_Status.
///
/// This is the server side of [`CommandChannelClient`].
pub struct CommandChannel<'r, 'buf, H: HciCfg<'buf>> {
    #[expect(dead_code, reason = "read once the command endpoints are implemented")]
    tx: H::HciTx,
    #[expect(dead_code, reason = "read once the command endpoints are implemented")]
    router: &'r HciEventRouter<'buf, H>,
}

#[rpc]
impl<'r, 'buf, H: HciCfg<'buf>> CommandChannel<'r, 'buf, H> {
    /// Sends a command that the controller answers with Command_Complete, and returns that event.
    ///
    /// The status in the event's return parameters is not checked.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError::Status`] if the controller rejects the command with
    /// Command_Status, or [`CommandError::UnexpectedResponse`] if it answers with a successful
    /// Command_Status.
    pub async fn send_command(
        &mut self,
        _command: OutgoingCommandPacket<'buf, H::CommandPoolCfg>,
    ) -> Result<HciEvent<'buf, H>, CommandError> {
        // TODO(https://fxbug.dev/535983448): Send `command` and wait for its Command_Complete.
        todo!()
    }

    /// Sends a command that the controller answers with Command_Status, and returns once the
    /// controller has accepted it.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError::Status`] if the controller rejects the command, or
    /// [`CommandError::UnexpectedResponse`] if it answers with Command_Complete.
    pub async fn start_command(
        &mut self,
        _command: OutgoingCommandPacket<'buf, H::CommandPoolCfg>,
    ) -> Result<(), CommandError> {
        // TODO(https://fxbug.dev/535983448): Send `command` and wait for its Command_Status.
        todo!()
    }
}

impl<'r, 'buf, H: HciCfg<'buf>> CommandChannel<'r, 'buf, H> {
    /// Creates a command channel for a freshly reset controller (which accepts one command),
    /// sending on `tx` and subscribing to command events on `router`.
    pub(crate) fn new(tx: H::HciTx, router: &'r HciEventRouter<'buf, H>) -> Self {
        Self { tx, router }
    }

    /// Serves command requests from `server`, one at a time, until every client is dropped.
    pub(crate) async fn run(
        &mut self,
        server: &Server<&'r CommandRpcChannel<'buf, H>>,
    ) -> Result<(), HciError> {
        // TODO(https://fxbug.dev/535983448): Subscribe to command events on `self.router` and track command credits.
        while let Ok((req, responder)) = server.recv().await {
            self.route_request(req, responder).await;
        }
        // `recv` only fails once every client is dropped. Events must still be received and
        // broadcast, so this is not an error.
        Ok(())
    }
}
