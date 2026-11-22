// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::Commands;
use fidl_fuchsia_bluetooth_hfp as hfp;
use futures::{FutureExt, pin_mut, select};

mod commands;
mod fidl;

use commands::{Command, CommandHandler};
use fidl::hands_free::HandsFree;

/// Prompt shown by the REPL.
const PROMPT: &str = "hf> ";

#[fuchsia::main]
async fn main() {
    let hands_free_proxy = fuchsia_component::client::connect_to_protocol::<hfp::HandsFreeMarker>()
        .expect("Failed to connect to HandsFree service");
    let hands_free = HandsFree::new(hands_free_proxy);

    let peers = hands_free.peers.clone();
    let calls = hands_free.calls.clone();
    let command_handler = CommandHandler::new(peers, calls);

    let hands_free_fut = hands_free.fuse();
    pin_mut!(hands_free_fut);

    let repl_fut = bt_repl::run(PROMPT, Commands::<Command>::new(), command_handler).fuse();
    pin_mut!(repl_fut);

    let fut = async {
        select! {
            repl_result = repl_fut =>
                if let Err(err) = repl_result {
                    println!("REPL error: {:?}", err)
                },
            hands_free_result = hands_free_fut =>
                if let Err(err) = hands_free_result {
                    println!("FIDL error: {:?}", err)
                }
        }
    };

    fut.await
}
