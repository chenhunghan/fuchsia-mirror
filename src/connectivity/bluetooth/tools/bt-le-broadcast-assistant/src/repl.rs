// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use bt_broadcast_assistant::debug::*;
use bt_common::core::AddressType;
use bt_gatt::pii::GetPeerAddr;
use bt_repl::{CommandSet as _, ReplControl, ReplHandler};
use fuchsia_async::{self as fasync, DurationExt, TimeoutExt};
use futures::StreamExt;
use futures::stream::FusedStream;

use crate::address_lookup::LocalPeerAddrCache;
use crate::assistant::*;
use crate::commands::*;

const DEFAULT_SCAN_DURATION_SEC: i64 = 10;
const PROMPT: &str = "ASSISTANT> ";

async fn look_for_scan_delegators<
    T: bt_gatt::GattTypes + 'static,
    R: GetPeerAddr + Send + Sync + 'static,
>(
    state: &mut AssistantState<T, R>,
    scan_duration_sec: i64,
) where
    T::ScanResultStream: FusedStream + Send + Unpin,
{
    let mut scan_result_stream = match state.debug.look_for_scan_delegators() {
        Ok(stream) => stream,
        Err(e) => {
            println!("\n[Error] Error starting scan: {e:#?}\n");
            return;
        }
    };

    let timeout = fasync::MonotonicDuration::from_seconds(scan_duration_sec).after_now();

    println!("\nLooking for scan delegators for {} seconds...", scan_duration_sec);
    loop {
        let result = scan_result_stream.next().on_timeout(timeout, || None).await;
        match result {
            Some(Ok(scanned)) => {
                let name = match &scanned.name {
                    bt_gatt::central::PeerName::CompleteName(n) => n.as_str(),
                    bt_gatt::central::PeerName::PartialName(n) => n.as_str(),
                    bt_gatt::central::PeerName::Unknown => "<Unknown>",
                };
                println!(
                    "\tFound scan delegator peer ({}): {} (connectable? {})",
                    scanned.id, name, scanned.connectable
                );
            }
            Some(Err(e)) => {
                println!("\n\t[Error] Received scan error: {e:#?}\n");
                break;
            }
            None => {
                println!("\n\tNo more scan results, scan finished.\n");
                break;
            }
        }
    }
    println!("Finished looking for scan delegators after {scan_duration_sec} seconds...\n");
}

struct AssistantHandler<T: bt_gatt::GattTypes, R: GetPeerAddr> {
    state: AssistantState<T, R>,
}

impl<T: bt_gatt::GattTypes + 'static, R: GetPeerAddr + Send + Sync + 'static> ReplHandler
    for AssistantHandler<T, R>
where
    T::ScanResultStream: FusedStream + Send + Unpin,
    T::NotificationStream: Send,
    T::Central: Send,
    T::Client: Send,
    T::PeerService: Send,
{
    type Command = ReplCmd;

    async fn handle(&mut self, cmd: ReplCmd, args: &[&str]) -> Result<ReplControl, Error> {
        match cmd {
            ReplCmd::Local(Cmd::Scan) => {
                let mut duration = DEFAULT_SCAN_DURATION_SEC;
                if args.len() == 1 {
                    match args[0].parse::<i64>() {
                        Ok(d) if d > 0 => duration = d,
                        Ok(_) => {
                            return Err(format_err!("Scan duration must be a positive number."));
                        }
                        Err(e) => {
                            return Err(format_err!("Invalid duration: {e}"));
                        }
                    }
                } else if args.len() > 1 {
                    return Err(format_err!("Usage: {}", Cmd::Scan.usage()));
                }
                self.state.stop_broadcast_assistant().await;
                look_for_scan_delegators(&mut self.state, duration).await;
                self.state.start_broadcast_assistant();
            }
            ReplCmd::Local(Cmd::SetPeerAddr) => {
                if let Some(cache) = &self.state.local_cache {
                    handle_set_peer_addr_cmd(cache, args)?;
                } else {
                    return Err(format_err!(
                        "`set-peer-addr` is only available with the --use-static-address flag."
                    ));
                }
            }
            ReplCmd::Assistant(cmd) => {
                let args_str: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                assistant_cmd(&mut self.state, cmd, args_str).await;
            }
        }
        Ok(ReplControl::Continue)
    }

    async fn on_exit(&mut self) {
        self.state.stop_broadcast_assistant().await;
    }
}

// Starts the GATT REPL.
pub async fn start_command_loop<
    T: bt_gatt::GattTypes + 'static,
    R: GetPeerAddr + Send + Sync + 'static,
>(
    debug: AssistantDebug<T, R>,
    local_cache: Option<LocalPeerAddrCache>,
) -> Result<(), Error>
where
    T::PeerService: Send,
    T::ScanResultStream: FusedStream + Send + Unpin,
    T::Central: Send,
    T::Client: Send,
    T::NotificationStream: Send,
{
    let mut state = AssistantState::new(debug, local_cache);
    state.start_broadcast_assistant();

    let handler = AssistantHandler { state };
    bt_repl::run(PROMPT, command_source(), handler).await
}

fn handle_set_peer_addr_cmd(cache: &LocalPeerAddrCache, args: &[&str]) -> Result<(), Error> {
    if args.len() != 3 {
        return Err(format_err!("Usage: {}", Cmd::SetPeerAddr.usage()));
    }

    let peer_id = bt_broadcast_assistant::debug::parse_peer_id(args[0])
        .map_err(|e| format_err!("Invalid Peer ID: {}", e))?;

    let addr = bt_broadcast_assistant::debug::parse_bd_addr(args[1])
        .map_err(|e| format_err!("Invalid Address: {}", e))?;

    let addr_type = match args[2] {
        "Public" => AddressType::Public,
        "Random" => AddressType::Random,
        _ => {
            return Err(format_err!(
                "Invalid address type: {}. Must be Public or Random.",
                args[2]
            ));
        }
    };

    cache.set_peer_address(peer_id, addr, addr_type);
    println!("\nCaching {peer_id} address as {addr:?} {addr_type:?}\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::AssistantState;
    use bt_common::core::AddressType;
    use bt_gatt::pii::StaticPeerAddr;
    use bt_gatt_fuchsia::{Central, FuchsiaTypes};

    fn setup_test_handler(
        local_cache: Option<LocalPeerAddrCache>,
    ) -> AssistantHandler<FuchsiaTypes, StaticPeerAddr> {
        let (central_proxy, _central_mock) =
            fidl::endpoints::create_proxy_and_stream::<fidl_fuchsia_bluetooth_le::CentralMarker>();
        let central = Central::new(central_proxy);
        let peer_addr_getter = StaticPeerAddr::new([0; 6], AddressType::Public);
        let debug = AssistantDebug::<FuchsiaTypes, _>::new(central, peer_addr_getter);
        AssistantHandler { state: AssistantState::new(debug, local_cache) }
    }

    #[fuchsia::test]
    async fn test_handle_scan_invalid_args() {
        let mut handler = setup_test_handler(None);
        assert!(handler.handle(ReplCmd::Local(Cmd::Scan), &["0"]).await.is_err());
        assert!(handler.handle(ReplCmd::Local(Cmd::Scan), &["-5"]).await.is_err());
        assert!(handler.handle(ReplCmd::Local(Cmd::Scan), &["abc"]).await.is_err());
        assert!(handler.handle(ReplCmd::Local(Cmd::Scan), &["1", "2"]).await.is_err());
    }

    #[fuchsia::test]
    async fn test_handle_set_peer_addr() {
        let mut handler = setup_test_handler(None);
        // Without local_cache, set-peer-addr returns an error.
        assert!(
            handler
                .handle(ReplCmd::Local(Cmd::SetPeerAddr), &["1", "01:02:03:04:05:06", "Public"],)
                .await
                .is_err()
        );

        let mut handler = setup_test_handler(Some(LocalPeerAddrCache::new()));
        assert!(handler.handle(ReplCmd::Local(Cmd::SetPeerAddr), &["1"]).await.is_err());
        assert_eq!(
            handler
                .handle(ReplCmd::Local(Cmd::SetPeerAddr), &["1", "01:02:03:04:05:06", "Public"],)
                .await
                .unwrap(),
            ReplControl::Continue
        );
    }

    #[fuchsia::test]
    async fn test_handle_assistant_cmd() {
        let mut handler = setup_test_handler(None);
        let res = handler.handle(ReplCmd::Assistant(AssistantCmd::Info), &[]).await;
        assert_eq!(res.unwrap(), ReplControl::Continue);
    }

    #[fuchsia::test]
    async fn test_on_exit() {
        let mut handler = setup_test_handler(None);
        handler.on_exit().await;
    }
}
