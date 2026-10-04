// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::Result;
use args::ListCommand;
use flex_fuchsia_driver_development as fdd;
#[cfg(feature = "fdomain")]
use fuchsia_driver_dev_fdomain as fuchsia_driver_dev;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

#[derive(Serialize)]
pub struct DriverHost {
    pub koid: u64,
    pub name: Option<String>,
    pub drivers: Vec<String>,
}

pub async fn get_driver_hosts(
    driver_development_proxy: &fdd::ManagerProxy,
) -> Result<Vec<DriverHost>> {
    let device_info = fuchsia_driver_dev::get_device_info(
        driver_development_proxy,
        &[],
        /* exact_match= */ false,
    )
    .await?;

    let driver_host_info =
        fuchsia_driver_dev::get_driver_host_info(driver_development_proxy).await?;

    let mut driver_host_drivers = BTreeMap::new();

    for device in device_info {
        if let Some(koid) = device.driver_host_koid
            && let Some(url) = device.bound_driver_url
        {
            driver_host_drivers.entry(koid).or_insert(BTreeSet::new()).insert(url);
        }
    }

    let mut driver_hosts_names = BTreeMap::new();

    for host in driver_host_info {
        if let Some(koid) = host.process_koid
            && let Some(name) = host.name
            && !name.is_empty()
        {
            driver_hosts_names.insert(koid, name);
        }
    }

    let mut result = Vec::new();
    for (koid, drivers) in driver_host_drivers {
        result.push(DriverHost {
            koid,
            name: driver_hosts_names.get(&koid).cloned(),
            drivers: drivers.into_iter().collect(),
        });
    }

    Ok(result)
}

fn write_driver_hosts(w: &mut dyn Write, hosts: &[DriverHost], is_tty: bool) -> Result<()> {
    let max_koid_len = hosts.iter().map(|h| h.koid.to_string().len()).max().unwrap_or(0).max(5);
    for host in hosts {
        if is_tty {
            writeln!(w, "Driver Host: {}", host.koid)?;
            if let Some(name) = &host.name {
                writeln!(w, "Name: {}", name)?;
            }
            for driver in &host.drivers {
                writeln!(w, "{:>4}{}", "", driver)?;
            }
            writeln!(w, "")?;
        } else {
            for driver in &host.drivers {
                writeln!(w, "Driver Host: {:<width$} {}", host.koid, driver, width = max_koid_len)?;
            }
        }
    }
    Ok(())
}

pub async fn list(
    _cmd: ListCommand,
    w: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    let hosts = get_driver_hosts(&driver_development_proxy).await?;
    write_driver_hosts(w, &hosts, termion::is_tty(&std::io::stdout()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use flex_client::fidl::ServerEnd;
    use fuchsia_async as fasync;
    use futures::future::FutureExt;
    use futures::stream::StreamExt;

    async fn run_device_info_iterator_server(
        mut device_infos: Vec<fdd::NodeInfo>,
        iterator: ServerEnd<fdd::NodeInfoIteratorMarker>,
    ) -> Result<()> {
        let mut iterator = iterator.into_stream();
        while let Some(res) = iterator.next().await {
            let request = res.context("Failed to get request")?;
            match request {
                fdd::NodeInfoIteratorRequest::GetNext { responder } => {
                    responder
                        .send(&device_infos)
                        .context("Failed to send device infos to responder")?;
                    device_infos.clear();
                }
            }
        }
        Ok(())
    }

    async fn run_driver_host_info_iterator_server(
        mut host_infos: Vec<fdd::DriverHostInfo>,
        iterator: ServerEnd<fdd::DriverHostInfoIteratorMarker>,
    ) -> Result<()> {
        let mut iterator = iterator.into_stream();
        while let Some(res) = iterator.next().await {
            let request = res.context("Failed to get request")?;
            match request {
                fdd::DriverHostInfoIteratorRequest::GetNext { responder } => {
                    responder
                        .send(&host_infos)
                        .context("Failed to send driver host infos to responder")?;
                    host_infos.clear();
                }
            }
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_non_tty_koid_formatting() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        #[cfg(not(feature = "fdomain"))]
        let client = flex_client::fidl::ZirconClient;
        let (driver_development_proxy, mut driver_development_requests) =
            client.create_proxy_and_stream::<fdd::ManagerMarker>();

        let request_handler_task: fasync::Task<Result<()>> = fasync::Task::spawn(async move {
            while let Some(res) = driver_development_requests.next().await {
                let request = res.context("Failed to get next request")?;
                match request {
                    fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                        run_device_info_iterator_server(
                            vec![
                                fdd::NodeInfo {
                                    driver_host_koid: Some(99897),
                                    bound_driver_url: Some(
                                        "fuchsia-boot:///spi#meta/spi.cm".to_string(),
                                    ),
                                    ..Default::default()
                                },
                                fdd::NodeInfo {
                                    driver_host_koid: Some(109217),
                                    bound_driver_url: Some(
                                        "fuchsia-pkg://fuchsia.com/dw-spi#meta/dw-spi.cm"
                                            .to_string(),
                                    ),
                                    ..Default::default()
                                },
                                fdd::NodeInfo {
                                    driver_host_koid: Some(50012267),
                                    bound_driver_url: Some(
                                        "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm".to_string(),
                                    ),
                                    ..Default::default()
                                },
                            ],
                            iterator,
                        )
                        .await?;
                    }
                    fdd::ManagerRequest::GetDriverHostInfo { iterator, .. } => {
                        run_driver_host_info_iterator_server(vec![], iterator).await?;
                    }
                    _ => {}
                }
            }
            anyhow::bail!("Driver development request stream unexpectedly closed");
        });

        let hosts = futures::select! {
            res = request_handler_task.fuse() => {
                res.unwrap();
                panic!("Request handler task unexpectedly finished");
            }
            res = get_driver_hosts(&driver_development_proxy).fuse() => res.expect("get_driver_hosts failed"),
        };

        let mut writer = Vec::new();
        write_driver_hosts(&mut writer, &hosts, false).expect("write_driver_hosts failed");
        let output = String::from_utf8(writer).expect("valid utf8");

        assert_eq!(
            output,
            concat!(
                "Driver Host: 99897    fuchsia-boot:///spi#meta/spi.cm\n",
                "Driver Host: 109217   fuchsia-pkg://fuchsia.com/dw-spi#meta/dw-spi.cm\n",
                "Driver Host: 50012267 fuchsia-pkg://fuchsia.com/foo#meta/foo.cm\n",
            )
        );

        for line in output.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts.len(), 4);
            assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
        }
    }
}
