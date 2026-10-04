// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Write;

use anyhow::{Context as _, Result, anyhow};
use argh::FromArgs;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fidl_fuchsia_io as fio;
use fuchsia_component::client as fclient;

#[derive(FromArgs, Debug, PartialEq)]
/// Read power state residencies from a fuchsia.hardware.power.stats provider.
///
/// Workflow:
/// # Start a shell in the scope of the provider.
/// $ ffx component explore <provider moniker>
/// # Read all power entities.
/// $ power_stats_util
/// # Read only the "gpu" entity.
/// $ power_stats_util gpu
struct Args {
    /// optional name of a specific power entity to read
    #[argh(positional)]
    entity: Option<String>,
}

/// Directories to look for the service in: the provider's outgoing directory when run from within
/// its namespace, then the incoming one.
const SVC_DIRS: &[&str] = &["/out/svc", "/svc"];

async fn connect() -> Result<fpowerstats::DeviceProxy> {
    for &path in SVC_DIRS {
        let Ok(dir) = fuchsia_fs::directory::open_in_namespace(path, fio::PERM_READABLE) else {
            continue;
        };
        let Ok(service) = fclient::Service::open_from_dir(dir, fpowerstats::ServiceMarker) else {
            continue;
        };
        let Ok(instances) = service.enumerate().await else {
            continue;
        };
        for instance in instances {
            if let Ok(proxy) = instance.connect_to_device() {
                return Ok(proxy);
            }
        }
    }
    Err(anyhow!("Failed to find a power stats provider in {}", SVC_DIRS.join(" or ")))
}

/// Writes the residency of each entity in `reports`, or only of `entity` if given.
fn write_stats(
    out: &mut impl Write,
    reports: &[fpowerstats::Stats],
    entity: Option<&str>,
) -> Result<()> {
    let mut matched = false;
    for stats in reports {
        if entity.is_some_and(|wanted| stats.entity != wanted) {
            continue;
        }
        matched = true;

        writeln!(out, "{} ({} states)", stats.entity, stats.state_residency_data.len())?;
        for state in &stats.state_residency_data {
            writeln!(
                out,
                "  state {:<8} entries {:<10} time {:>12} ms",
                state.name, state.entry_count, state.total_time_ms
            )?;
        }
    }

    if !matched {
        return Err(match entity {
            Some(wanted) => anyhow!("No power entity named '{wanted}'"),
            None => anyhow!("The power stats provider reported no entities"),
        });
    }
    Ok(())
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let proxy = connect().await?;
    let reports = proxy.get_stats().await.context("GetStats failed")?;
    write_stats(&mut std::io::stdout(), &reports, args.entity.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(entity: &str, states: &[(i32, u32, u64)]) -> fpowerstats::Stats {
        fpowerstats::Stats {
            entity: entity.to_string(),
            state_residency_data: states
                .iter()
                .map(|&(id, entry_count, total_time_ms)| fpowerstats::StateResidency {
                    id,
                    name: id.to_string(),
                    total_time_ms,
                    entry_count,
                    last_entry_timestamp_ms: 0,
                })
                .collect(),
        }
    }

    fn written(reports: &[fpowerstats::Stats], entity: Option<&str>) -> Result<String> {
        let mut out = Vec::new();
        write_stats(&mut out, reports, entity)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[fuchsia::test]
    fn writes_every_entity() {
        let reports = [stats("a", &[(0, 3, 1500), (1, 0, 0)]), stats("b", &[])];

        assert_eq!(
            written(&reports, None).unwrap(),
            "a (2 states)\n\
             \x20 state 0        entries 3          time         1500 ms\n\
             \x20 state 1        entries 0          time            0 ms\n\
             b (0 states)\n"
        );
    }

    #[fuchsia::test]
    fn writes_only_the_requested_entity() {
        let reports = [stats("a", &[(0, 1, 2)]), stats("b", &[(0, 3, 4)])];

        let out = written(&reports, Some("b")).unwrap();
        assert!(out.starts_with("b (1 states)\n"));
        assert!(!out.contains("a ("));
    }

    #[fuchsia::test]
    fn fails_for_an_unknown_entity() {
        let reports = [stats("a", &[])];

        assert!(written(&reports, Some("x")).is_err());
    }

    #[fuchsia::test]
    fn fails_without_entities() {
        assert!(written(&[], None).is_err());
    }
}
