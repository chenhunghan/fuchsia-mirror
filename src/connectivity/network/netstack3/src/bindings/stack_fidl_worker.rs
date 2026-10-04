// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_net_stack::{StackRequest, StackRequestStream};
use futures::TryStreamExt as _;
use log::{debug, error};

use super::util::ResultExt as _;

pub(crate) async fn serve(stream: StackRequestStream) -> Result<(), fidl::Error> {
    stream
        .try_for_each(|req| async {
            match req {
                StackRequest::SetDhcpClientEnabled { responder, id: _, enable } => {
                    // TODO(https://fxbug.dev/42162065): Remove this once
                    // DHCPv4 client is implemented out-of-stack.
                    if enable {
                        error!("TODO(https://fxbug.dev/42062356): Support starting DHCP client");
                    }
                    responder.send(Ok(())).unwrap_or_log("failed to respond");
                }
                StackRequest::BridgeInterfaces { interfaces: _, bridge, control_handle: _ } => {
                    error!("bridging is not supported in netstack3");
                    bridge
                        .close_with_epitaph(zx::Status::NOT_SUPPORTED)
                        .unwrap_or_else(|e| debug!("failed to close bridge control {:?}", e));
                }
            }
            Ok(())
        })
        .await
}
