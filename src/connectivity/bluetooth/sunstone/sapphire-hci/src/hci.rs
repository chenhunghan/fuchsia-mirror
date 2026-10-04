// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::convert::Infallible;

use sapphire_async::broadcast::{BroadcastCfg, Interest};
use sapphire_async::pool::PoolCfg;
use sapphire_async::rpc::{CallError, RpcCfg, RpcChannel, Server};

use crate::command::{
    CommandChannel, CommandChannelClient, CommandChannelRpc, CommandError, OutgoingCommandPacket,
};
use crate::events::{EventRouter, EventSubscriber, PublishedEventPacket};
use crate::transport::{HciRx, HciTx, IncomingHciPacket, TransportError};

/// Selects the transport, synchronization primitives and buffer configuration of the HCI layer.
pub trait HciCfg<'buf> {
    /// Configuration of the broadcast channel that fans events out to subscribers.
    type EventBroadcastCfg: BroadcastCfg;
    /// Transport half that sends packets to the controller.
    type HciTx: HciTx + Clone;
    /// Transport half that receives packets from the controller.
    type HciRx: HciRx<'buf>;
    /// Configuration of the pool that command buffers are allocated from.
    type CommandPoolCfg: PoolCfg + 'buf;
    /// Configuration of the channel that carries command requests to the [`HciRunner`].
    type CommandRpcCfg: RpcCfg;
}

/// The received-packet buffer configuration of an [`HciCfg`].
type HciRxCfgOf<'buf, Cfg> = <<Cfg as HciCfg<'buf>>::HciRx as HciRx<'buf>>::Cfg;

/// An event received by the HCI layer with configuration `Cfg`.
pub type HciEvent<'buf, Cfg> = PublishedEventPacket<'buf, HciRxCfgOf<'buf, Cfg>>;

/// A subscriber to the events of the HCI layer with configuration `Cfg`.
pub type HciSubscriber<'r, 'buf, Cfg> =
    EventSubscriber<'r, 'buf, HciRxCfgOf<'buf, Cfg>, <Cfg as HciCfg<'buf>>::EventBroadcastCfg>;

pub(crate) type HciEventRouter<'buf, Cfg> =
    EventRouter<'buf, HciRxCfgOf<'buf, Cfg>, <Cfg as HciCfg<'buf>>::EventBroadcastCfg>;

pub(crate) type CommandRpcChannel<'buf, Cfg> =
    RpcChannel<CommandChannelRpc<'buf, Cfg>, <Cfg as HciCfg<'buf>>::CommandRpcCfg>;

/// Fatal errors that end [`HciRunner::run`]. The controller should be reset.
#[derive(Debug, thiserror::Error)]
pub enum HciError {
    /// Reading from or writing to the transport failed.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    /// A Command_Complete / Command_Status event did not answer the in-flight command.
    #[error("command response does not match the in-flight command")]
    UnexpectedCommandResponse,
    /// A Command_Complete / Command_Status event is too short to hold its fixed fields.
    #[error("malformed command response")]
    MalformedCommandResponse,
    /// The controller sent an event whose length doesn't match its header.
    #[error("controller sent a malformed event")]
    ControllerError,
}

/// Storage for the HCI layer of a single controller: the event router and the command channel.
///
/// [`HciResources::split`] divides it into an [`HciRunner`], which drives the controller, and an
/// [`HciHandle`], which holds the clients that send commands and subscribe to events.
pub struct HciResources<'buf, Cfg: HciCfg<'buf>> {
    router: HciEventRouter<'buf, Cfg>,
    commands: CommandRpcChannel<'buf, Cfg>,
}

impl<'buf, Cfg: HciCfg<'buf>> HciResources<'buf, Cfg> {
    /// Creates resources with no event subscribers and no queued commands.
    pub fn new() -> Self
    where
        HciEventRouter<'buf, Cfg>: Default,
        CommandRpcChannel<'buf, Cfg>: Default,
    {
        Self { router: Default::default(), commands: Default::default() }
    }

    /// Returns the runner that drives the controller and the handle holding its clients.
    pub fn split(&mut self) -> (HciRunner<'_, 'buf, Cfg>, HciHandle<'_, 'buf, Cfg>) {
        let (client, server) = self.commands.split();
        let runner = HciRunner { router: &self.router, server };
        let handle = HciHandle {
            commands: CommandClient { commands: CommandChannelClient::new(client) },
            events: EventClient { router: &self.router },
        };
        (runner, handle)
    }
}

/// Drives the controller: sends the commands of every [`CommandClient`] and broadcasts the
/// events the controller sends back.
pub struct HciRunner<'r, 'buf, Cfg: HciCfg<'buf>> {
    router: &'r HciEventRouter<'buf, Cfg>,
    server: Server<&'r CommandRpcChannel<'buf, Cfg>>,
}

impl<'r, 'buf, Cfg: HciCfg<'buf>> HciRunner<'r, 'buf, Cfg> {
    /// Drives the controller until a fatal error: serves command requests by writing them to
    /// `tx`, and reads packets from `rx`, broadcasting every event.
    ///
    /// The controller must be freshly reset: it is assumed to accept one command.
    ///
    /// # Errors
    ///
    /// Returns an [`HciError`] when the controller's state can no longer be trusted. The
    /// controller should be reset before it is driven by a new HCI layer.
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future ends it like an error does: the command in flight fails with
    /// [`CallError::ServerCancel`]. A packet may be partially written, so the controller must be
    /// reset before it is used again.
    pub async fn run(self, tx: Cfg::HciTx, rx: Cfg::HciRx) -> Result<Infallible, HciError> {
        let mut commands = CommandChannel::new(tx, self.router);
        futures::try_join!(commands.run(&self.server), Self::receive_packets(self.router, rx),)
            .map(|((), never)| never)
    }

    /// Receives packets from `rx` and routes every event through `router`.
    async fn receive_packets(
        router: &HciEventRouter<'buf, Cfg>,
        mut rx: Cfg::HciRx,
    ) -> Result<Infallible, HciError> {
        loop {
            match rx.next_packet().await? {
                IncomingHciPacket::Event(buf) => {
                    router.route_event(buf).await?;
                }
                IncomingHciPacket::Acl(_)
                | IncomingHciPacket::Sco(_)
                | IncomingHciPacket::Iso(_) => {
                    // TODO(https://fxbug.dev/535983448): Dispatch data packets.
                }
            }
        }
    }
}

/// The clients of the HCI layer. Clone a client to share it.
pub struct HciHandle<'r, 'buf, Cfg: HciCfg<'buf>> {
    /// Sends commands to the controller.
    pub commands: CommandClient<'r, 'buf, Cfg>,
    /// Subscribes to the controller's events.
    pub events: EventClient<'r, 'buf, Cfg>,
    // TODO(https://fxbug.dev/535983448): Add a client for each data channel.
}

/// Sends commands to the controller.
pub struct CommandClient<'r, 'buf, Cfg: HciCfg<'buf>> {
    commands: CommandChannelClient<'buf, &'r CommandRpcChannel<'buf, Cfg>, Cfg>,
}

impl<'r, 'buf, Cfg: HciCfg<'buf>> CommandClient<'r, 'buf, Cfg> {
    /// Sends a command that the controller answers with Command_Complete, and returns that event.
    ///
    /// The status in the event's return parameters is not checked. A command sent before
    /// [`HciRunner::run`] starts waits for it.
    ///
    /// # Errors
    ///
    /// Returns [`CallError::ServerCancel`] if [`HciRunner::run`] ended while the command was
    /// being served, and [`CallError::Closed`] if the [`HciRunner`] was dropped or `run` ended
    /// before serving it. Otherwise returns the [`CommandError`] if the command failed.
    pub async fn send_command(
        &self,
        command: OutgoingCommandPacket<'buf, Cfg::CommandPoolCfg>,
    ) -> Result<Result<HciEvent<'buf, Cfg>, CommandError>, CallError> {
        self.commands.send_command(command).await
    }

    /// Sends a command that the controller answers with Command_Status, and returns once the
    /// controller has accepted it.
    ///
    /// A command sent before [`HciRunner::run`] starts waits for it.
    ///
    /// # Errors
    ///
    /// Returns [`CallError::ServerCancel`] if [`HciRunner::run`] ended while the command was
    /// being served, and [`CallError::Closed`] if the [`HciRunner`] was dropped or `run` ended
    /// before serving it. Otherwise returns the [`CommandError`] if the command failed.
    pub async fn start_command(
        &self,
        command: OutgoingCommandPacket<'buf, Cfg::CommandPoolCfg>,
    ) -> Result<Result<(), CommandError>, CallError> {
        self.commands.start_command(command).await
    }
}

// Implemented manually: deriving would require `Cfg` to be `Clone`.
impl<'r, 'buf, Cfg: HciCfg<'buf>> Clone for CommandClient<'r, 'buf, Cfg> {
    fn clone(&self) -> Self {
        Self { commands: self.commands.clone() }
    }
}

/// Subscribes to the controller's events.
pub struct EventClient<'r, 'buf, Cfg: HciCfg<'buf>> {
    router: &'r HciEventRouter<'buf, Cfg>,
}

impl<'r, 'buf, Cfg: HciCfg<'buf>> EventClient<'r, 'buf, Cfg> {
    /// Subscribes to the events accepted by `filter`.
    ///
    /// A subscriber must keep reading its events: once the event channel is full, reading from
    /// the transport is delayed until space is available.
    ///
    /// Returns `None` if the router has no room for another subscriber.
    pub fn subscribe(
        &self,
        filter: fn(&HciEvent<'buf, Cfg>) -> Interest,
    ) -> Option<HciSubscriber<'r, 'buf, Cfg>> {
        self.router.subscribe(filter)
    }
}

// Implemented manually: deriving would require `Cfg` to be `Clone` and `Copy`.
impl<'r, 'buf, Cfg: HciCfg<'buf>> Clone for EventClient<'r, 'buf, Cfg> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'r, 'buf, Cfg: HciCfg<'buf>> Copy for EventClient<'r, 'buf, Cfg> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;
    use sapphire_emboss::hci_common::EventCode;

    /// A controller, a command pool, and an HCI layer split into its runner and clients.
    struct Fixture<'r, 'buf> {
        controller: &'buf MockController,
        pool: &'buf CommandPool,
        runner: HciRunner<'r, 'buf, TestHciCfg>,
        commands: CommandClient<'r, 'buf, TestHciCfg>,
        #[expect(dead_code, reason = "read once event subscription tests are added")]
        events: EventClient<'r, 'buf, TestHciCfg>,
    }

    /// Runs `test` with a fresh [`Fixture`].
    fn with_hci(test: impl FnOnce(Fixture<'_, '_>)) {
        let controller = MockController::new();
        let pool = CommandPool::new(2).unwrap();
        let mut resources = HciResources::<TestHciCfg>::new();
        let (runner, HciHandle { commands, events }) = resources.split();
        test(Fixture { controller: &controller, pool: &pool, runner, commands, events });
    }

    #[test]
    fn commands_fail_once_the_runner_is_dropped() {
        with_hci(|Fixture { pool, runner, commands, .. }| {
            BoundedExecutor::new(TestExecutor::new(), |s| {
                let mut queued =
                    s.spawn(commands.send_command(make_command(pool, RESET_OPCODE, &[])));
                s.run_until_stalled();
                assert!(!queued.is_finished());

                drop(runner);
                s.run_until_stalled();
                assert!(matches!(queued.get(), Some(Err(CallError::Closed))));

                let mut late =
                    s.spawn(commands.send_command(make_command(pool, RESET_OPCODE, &[])));
                s.run_until_stalled();
                assert!(matches!(late.get(), Some(Err(CallError::Closed))));
            });
        });
    }

    #[test]
    fn run_fails_on_malformed_event() {
        with_hci(|Fixture { controller, runner, .. }| {
            let (tx, rx) = controller.transport();

            controller.push_truncated_event(EventCode::HARDWARE_ERROR, 5, &[1]);

            BoundedExecutor::new(TestExecutor::new(), |s| {
                let mut run = s.spawn(runner.run(tx, rx));
                s.run_until_stalled();

                assert!(matches!(run.get(), Some(Err(HciError::ControllerError))));
            });
        });
    }
}
