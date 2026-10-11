// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::FromArgs;
use fdf_component::{Driver, DriverContext, DriverError, Node};
use fidl_fuchsia_driver_debug::DebugRequestStream;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "ping")]
/// Ping the driver to check responsiveness.
pub struct PingArgs {
    #[argh(option, short = 'c', default = "1", description = "ping count")]
    pub count: u32,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "reset")]
/// Reset the driver state.
pub struct ResetArgs {
    #[argh(switch, description = "hard reset")]
    pub hard: bool,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "fault")]
/// Induce driver fault.
pub struct FaultArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum TestCommands {
    Ping(PingArgs),
    Reset(ResetArgs),
    Fault(FaultArgs),
}

pub struct TestDriver {
    _node: Node,
    _scope: fuchsia_async::Scope,
}

async fn handle_debug(mut stream: DebugRequestStream) -> Result<(), fidl::Error> {
    while let Some((cmd, responder)) = driver_debug::next_command(&mut stream).await? {
        let result: Result<String, anyhow::Error> = match cmd {
            TestCommands::Ping(args) => {
                let mut out = String::new();
                for i in 1..=args.count {
                    out.push_str(&format!("ping {i}\n"));
                }
                Ok(out)
            }
            TestCommands::Reset(args) => {
                if args.hard {
                    Ok("hard reset\n".to_string())
                } else {
                    Ok("soft reset\n".to_string())
                }
            }
            TestCommands::Fault(_) => Err(anyhow::anyhow!("driver internal fault")),
        };
        responder.send(result)?;
    }
    Ok(())
}

impl Driver for TestDriver {
    const NAME: &'static str = "test_driver";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;
        let scope = fuchsia_async::Scope::new_with_name("test driver scope");
        let mut service_fs = ServiceFs::new();

        service_fs.dir("svc").add_fidl_service(move |stream: DebugRequestStream| {
            fuchsia_async::Scope::current().spawn(async move {
                let _ = handle_debug(stream).await;
            });
        });

        context.serve_outgoing(&mut service_fs)?;
        scope.spawn(service_fs.collect::<()>());

        Ok(Self { _node: node, _scope: scope })
    }

    async fn stop(&self) {}
}
