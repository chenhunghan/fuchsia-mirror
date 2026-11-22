// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use bt_common::Uuid;
use bt_repl::{Commands, ReplControl, ReplHandler};
use futures::{Future, FutureExt, select_biased};

use super::{
    GattClient,
    GattClientPtr,
    commands::Cmd,
    do_connect,
    //  do_read_by_type,
    do_disable_notify,
    do_enable_notify,
    do_find_services,
    do_list,
    do_pacs,
    do_read_chr,
    do_read_desc,
    do_read_long_chr,
    do_read_long_desc,
    do_vcs,
    do_write_chr,
    do_write_desc,
    do_write_long_chr,
    do_write_long_desc,
};

const PROMPT: &str = "GATT> ";
// Escape codes:
/// Clear the pty line on which the cursor is located.
/// Used when evented output is intermingled with the REPL prompt.
pub static CLEAR_LINE: &str = "\x1b[2K";
/// Move cursor to column 1
pub static CHA: &str = "\x1b[1G";

/// Trait for GATT clients that can notify when the underlying peer connection closes.
pub trait WatchDisconnect {
    /// Returns a future that resolves when the remote peer disconnects or the
    /// underlying FIDL client channel is closed.
    fn wait_for_disconnect(&self) -> impl Future<Output = ()> + 'static;
}

impl WatchDisconnect for bt_gatt_fuchsia::Client {
    fn wait_for_disconnect(&self) -> impl Future<Output = ()> + 'static {
        bt_gatt_fuchsia::Client::wait_for_disconnect(self)
    }
}

struct GattHandler<T: bt_gatt::GattTypes> {
    client: GattClientPtr<T>,
}

impl<T: bt_gatt::GattTypes> ReplHandler for GattHandler<T> {
    type Command = Cmd;

    async fn handle(&mut self, cmd: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        let to_string_vec = || args.iter().map(|s| String::from(*s)).collect();

        match cmd {
            Cmd::List => {
                do_list(args, &self.client);
            }
            Cmd::Find => {
                do_find_services(args, &self.client).await;
            }
            Cmd::Connect => do_connect(args, &self.client).await?,
            Cmd::ReadChr => do_read_chr(args, &self.client).await?,
            Cmd::ReadLongChr => do_read_long_chr(args, &self.client).await?,
            Cmd::WriteChr => do_write_chr(args.to_vec(), &self.client).await?,
            Cmd::WriteLongChr => do_write_long_chr(args.to_vec(), &self.client).await?,
            Cmd::ReadDesc => do_read_desc(args, &self.client).await?,
            Cmd::ReadLongDesc => do_read_long_desc(args, &self.client).await?,
            Cmd::WriteDesc => do_write_desc(args.to_vec(), &self.client).await?,
            Cmd::WriteLongDesc => do_write_long_desc(args, &self.client).await?,
            Cmd::EnableNotify => do_enable_notify(args, &self.client).await?,
            Cmd::DisableNotify => do_disable_notify(args, &self.client).await?,
            Cmd::Pacs => do_pacs(to_string_vec(), self.client.clone()).await?,
            Cmd::Vol => do_vcs(to_string_vec(), self.client.clone()).await?,
            Cmd::ReadByType => {
                println!("Not implemented: {cmd:?}");
            }
        }
        Ok(ReplControl::Continue)
    }
}

// Starts the GATT REPL. This first requests a list of remote services and resolves the
// returned future with an error if no services are found.
pub async fn start_gatt_loop<T: bt_gatt::GattTypes>(
    client: T::Client,
    service: Option<Uuid>,
) -> Result<(), Error>
where
    <T as bt_gatt::GattTypes>::Client: Clone + WatchDisconnect,
{
    let disconnect_fut = client.wait_for_disconnect().fuse();
    let client = GattClient::<T>::new(client);

    if let Some(uuid) = service {
        println!("  discovering service {}...", uuid.recognize());
        client.find_service(uuid).await;
    }

    let handler = GattHandler { client };
    let repl_fut = bt_repl::run(PROMPT, Commands::<Cmd>::new(), handler).fuse();
    futures::pin_mut!(disconnect_fut, repl_fut);

    select_biased! {
        () = disconnect_fut => {
            println!("{CLEAR_LINE}{CHA}Peer disconnected");
            Ok(())
        }
        res = repl_fut => res,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_gatt::Central as _;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_bluetooth_le as fidl_le;
    use fuchsia_async as fasync;
    use fuchsia_bluetooth::types::PeerId;
    use futures::StreamExt as _;
    use futures::task::Poll;

    #[fuchsia::test]
    async fn gatt_handler_dispatches_commands() {
        let (proxy, _central_stream) = create_proxy_and_stream::<fidl_le::CentralMarker>();
        let central = bt_gatt_fuchsia::Central::new(proxy);
        let client = central
            .connect(bt_gatt_fuchsia::to_gatt_peer_id(&PeerId(1).into()))
            .await
            .expect("connect should succeed");
        let mut handler =
            GattHandler { client: GattClient::<bt_gatt_fuchsia::FuchsiaTypes>::new(client) };

        let res = handler.handle(Cmd::List, &[]).await.expect("list should succeed");
        assert_eq!(res, ReplControl::Continue);

        let res = handler
            .handle(Cmd::ReadByType, &[])
            .await
            .expect("read-by-type should return Continue");
        assert_eq!(res, ReplControl::Continue);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn watch_disconnect_resolves_when_connection_closes() {
        let (proxy, mut central_stream) = create_proxy_and_stream::<fidl_le::CentralMarker>();
        let central = bt_gatt_fuchsia::Central::new(proxy);
        let Poll::Ready(Ok(client)) = fasync::TestExecutor::poll_until_stalled(
            central.connect(bt_gatt_fuchsia::to_gatt_peer_id(&PeerId(1).into())),
        )
        .await
        else {
            panic!("connect should complete");
        };

        let connection_server_end = match central_stream.next().await {
            Some(Ok(fidl_le::CentralRequest::Connect { handle, .. })) => handle,
            other => panic!("unexpected central request: {other:?}"),
        };

        let mut disconnect_fut = Box::pin(WatchDisconnect::wait_for_disconnect(&client));
        assert!(fasync::TestExecutor::poll_until_stalled(&mut disconnect_fut).await.is_pending());

        drop(connection_server_end);
        assert!(fasync::TestExecutor::poll_until_stalled(&mut disconnect_fut).await.is_ready());
    }
}
