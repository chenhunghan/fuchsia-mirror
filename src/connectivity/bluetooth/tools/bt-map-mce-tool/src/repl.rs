// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use bt_repl::{Commands, ReplControl, ReplHandler};
use futures::{FutureExt, select_biased};

use crate::accessor::{
    AccessorClient, CHA, CLEAR_LINE, list_all_mas_instances, register_for_notifications,
    unregister_notifications,
};
use crate::commands::Cmd;

const PROMPT: &str = "MESSAGE_ACCESSOR> ";

struct AccessorHandler {
    client: AccessorClient,
}

impl ReplHandler for AccessorHandler {
    type Command = Cmd;

    async fn handle(&mut self, cmd: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        match cmd {
            Cmd::ListAllMasInstances => list_all_mas_instances(self.client.clone()).await?,
            Cmd::TurnOnNotifications => {
                register_for_notifications(args, self.client.clone()).await?
            }
            Cmd::TurnOffNotifications => unregister_notifications(self.client.clone()).await?,
        }
        Ok(ReplControl::Continue)
    }
}

// Starts the accessor REPL.
pub async fn start_accessor_loop(client: AccessorClient) -> Result<(), Error> {
    let peer_id = client.peer_id();
    let disconnect_fut = client.wait_for_disconnect().fuse();
    let handler = AccessorHandler { client };
    let repl_fut = bt_repl::run(PROMPT, Commands::<Cmd>::new(), handler).fuse();
    futures::pin_mut!(disconnect_fut, repl_fut);

    select_biased! {
        () = disconnect_fut => {
            println!("{CLEAR_LINE}{CHA}Peer {peer_id:?} disconnected");
            Ok(())
        }
        res = repl_fut => res,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_bluetooth::PeerId;
    use fidl_fuchsia_bluetooth_map::{AccessorMarker, AccessorRequest};
    use fuchsia_async as fasync;
    use futures::StreamExt as _;

    #[fuchsia::test]
    async fn accessor_handler_dispatches_commands() {
        let (proxy, mut stream) = create_proxy_and_stream::<AccessorMarker>();
        let client = AccessorClient::new(PeerId { value: 1 }, proxy);
        let mut handler = AccessorHandler { client };

        let server_task = fasync::Task::local(async move {
            match stream.next().await {
                Some(Ok(AccessorRequest::ListAllMasInstances { responder })) => {
                    responder.send(Ok(&[])).expect("send response");
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });

        let res = handler.handle(Cmd::ListAllMasInstances, &[]).await.expect("list ok");
        assert_eq!(res, ReplControl::Continue);
        server_task.await;

        let res = handler
            .handle(Cmd::TurnOffNotifications, &[])
            .await
            .expect("turn-off-notifications ok");
        assert_eq!(res, ReplControl::Continue);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn wait_for_disconnect_resolves_when_accessor_closes() {
        let (proxy, stream) = create_proxy_and_stream::<AccessorMarker>();
        let client = AccessorClient::new(PeerId { value: 1 }, proxy);

        let mut disconnect_fut = Box::pin(client.wait_for_disconnect());
        assert!(fasync::TestExecutor::poll_until_stalled(&mut disconnect_fut).await.is_pending());

        drop(stream);
        assert!(fasync::TestExecutor::poll_until_stalled(&mut disconnect_fut).await.is_ready());
    }
}
