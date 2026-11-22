// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use crate::subcommands::node::common;

use anyhow::{Result, anyhow};
use args::ListNodeCommand;
use flex_fuchsia_driver_development as fdd;
#[cfg(feature = "fdomain")]
use fuchsia_driver_dev_fdomain as fuchsia_driver_dev;
use prettytable::format::consts::FORMAT_CLEAN;
use prettytable::{Table, row};
use serde::Serialize;
use std::io::Write;

#[derive(Serialize)]
pub struct NodeDetails {
    pub moniker: String,
    pub state: String,
    pub owner: String,
    pub disambiguator: String,
}

pub async fn get_nodes(
    cmd: &ListNodeCommand,
    driver_development_proxy: &fdd::ManagerProxy,
) -> Result<Vec<NodeDetails>> {
    let nodes = fuchsia_driver_dev::get_device_info(driver_development_proxy, &[], false).await?;
    let nodes = common::filter_nodes(nodes, cmd.filter.clone())?;

    let mut result = Vec::new();
    for node in nodes {
        let moniker = node.moniker.clone().unwrap_or_else(|| "Moniker not found.".to_string());
        let (state, owner) =
            common::get_state_and_owner(node.quarantined, &node.bound_driver_url, false);
        let disambiguator = node.disambiguator.unwrap_or_default();
        result.push(NodeDetails { moniker, state, owner, disambiguator });
    }
    Ok(result)
}

pub async fn list_node(
    cmd: ListNodeCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    let nodes = fuchsia_driver_dev::get_device_info(&driver_development_proxy, &[], false).await?;
    let nodes = common::filter_nodes(nodes, cmd.filter)?;
    let with_style = termion::is_tty(&std::io::stdout());

    if nodes.len() > 0 {
        if !cmd.verbose {
            for node in nodes {
                writeln!(
                    writer,
                    "{}",
                    node.moniker.unwrap_or_else(|| "Moniker not found.".to_string())
                )?;
            }
        } else {
            let table = create_table(nodes, with_style);
            table.print(writer)?;
        }
    } else {
        if cmd.fail_on_missing {
            return Err(anyhow!("No nodes found."));
        } else {
            writeln!(writer, "No nodes found.")?;
        }
    }

    Ok(())
}

fn create_table(nodes: Vec<fdd::NodeInfo>, with_style: bool) -> Table {
    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.set_titles(row!("State", "Moniker", "Owner", "Disambiguator"));

    for node in nodes {
        let moniker = node.moniker.unwrap_or_else(|| "Moniker not found.".to_string());
        let (state, owner) =
            common::get_state_and_owner(node.quarantined, &node.bound_driver_url, with_style);
        let disambiguator = node.disambiguator.unwrap_or_default();
        table.add_row(row!(state, moniker, owner, disambiguator));
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;
    use flex_client::fidl::ServerEnd;
    use fuchsia_async as fasync;
    use futures::stream::{StreamExt, TryStreamExt};

    async fn serve_node_info(
        iterator: ServerEnd<fdd::NodeInfoIteratorMarker>,
        nodes: Vec<fdd::NodeInfo>,
    ) {
        let mut stream = iterator.into_stream();
        let mut iter = nodes.into_iter().peekable();
        while let Ok(Some(fdd::NodeInfoIteratorRequest::GetNext { responder })) =
            stream.try_next().await
        {
            let batch: Vec<_> = iter.by_ref().take(100).collect();
            let _ = responder.send(&batch);
        }
    }

    fn sample_node_info() -> fdd::NodeInfo {
        fdd::NodeInfo {
            moniker: Some("dev.sys.platform.pci".to_string()),
            bound_driver_url: Some("fuchsia-boot:///pci#meta/pci.cm".to_string()),
            quarantined: Some(false),
            disambiguator: Some("0123456789abcdef".to_string()),
            ..Default::default()
        }
    }

    #[fuchsia::test]
    fn test_create_table_includes_disambiguator() {
        let table = create_table(vec![sample_node_info()], false);
        let rendered = table.to_string();
        assert!(!rendered.contains("Bus Topology"), "{}", rendered);
        assert!(rendered.contains("Disambiguator"), "{}", rendered);
        assert!(rendered.contains("0123456789abcdef"), "{}", rendered);
    }

    #[fuchsia::test]
    async fn test_get_nodes_populates_disambiguator() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client(|| Err(zx_status::Status::NOT_SUPPORTED));
        #[cfg(not(feature = "fdomain"))]
        let (proxy, mut stream) =
            flex_client::fidl::create_proxy_and_stream::<fdd::ManagerMarker>();
        #[cfg(feature = "fdomain")]
        let (proxy, mut stream) = client.create_proxy_and_stream::<fdd::ManagerMarker>();

        let scope = fasync::Scope::new();
        scope.spawn(async move {
            let iterator_scope = fasync::Scope::new();
            while let Some(Ok(request)) = stream.next().await {
                if let fdd::ManagerRequest::GetNodeInfo { iterator, .. } = request {
                    iterator_scope.spawn(serve_node_info(iterator, vec![sample_node_info()]));
                }
            }
        });

        let cmd = ListNodeCommand::from_args(&["list"], &["-v"]).unwrap();
        let details = get_nodes(&cmd, &proxy).await.unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].moniker, "dev.sys.platform.pci");
        assert_eq!(details[0].disambiguator, "0123456789abcdef");
    }
}
