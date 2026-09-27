// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Write;

use anyhow::{Result, anyhow};
use argh::FromArgs;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_hardware_google_odpm as fgoogle_odpm;
use fidl_fuchsia_io as fio;
use fuchsia_component::client as fclient;

#[derive(FromArgs, Debug, PartialEq)]
/// Read ODPM power rails and configure polling settings.
///
/// Workflow:
/// # Start a shell in the scope of the ODPM driver.
/// $ ffx component explore <odpm_moniker>
/// # Read all rails.
/// $ odpm_util read
/// # Read only the "cpu_big" rail.
/// $ odpm_util read cpu_big
/// # Read instantaneous power (Watts).
/// $ odpm_util read -i
/// # Read current (Amperes) instead of power (Watts).
/// $ odpm_util read -c
/// # Read energy (Joules) instead of power (Watts).
/// $ odpm_util read -e
/// # Configure ODPM driver polling settings.
/// $ odpm_util config get
/// # Set the polling interval to 100ms, and record a series entry every 1s.
/// $ odpm_util config set --poll-interval-ms 100
/// $ odpm_util config set --polls-per-series-entry 10
/// # Restore the default configuration.
/// $ odpm_util config reset
struct Args {
    #[argh(subcommand)]
    command: Option<SubCommands>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum SubCommands {
    Read(ReadArgs),
    Config(ConfigArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "read")]
/// Read ODPM power rails.
struct ReadArgs {
    /// optional name of a specific rail to read (e.g. cpu_big)
    #[argh(positional)]
    rail: Option<String>,

    /// read instantaneous power (Watts) instead of average power
    #[argh(switch, short = 'i')]
    instantaneous: bool,

    /// read current (Amperes) instead of power (Watts)
    #[argh(switch, short = 'c')]
    current: bool,

    /// read energy (Joules) instead of power (Watts)
    #[argh(switch, short = 'e')]
    energy: bool,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "config")]
/// Query or configure ODPM driver polling settings.
struct ConfigArgs {
    #[argh(subcommand)]
    subcommand: ConfigSubCommands,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum ConfigSubCommands {
    Set(ConfigSetArgs),
    Get(ConfigGetArgs),
    Reset(ConfigResetArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "set")]
/// Set ODPM driver polling configuration parameters.
struct ConfigSetArgs {
    /// hardware polling interval in milliseconds (e.g. 100, 500, 1000)
    #[argh(option, short = 'i')]
    poll_interval_ms: Option<u32>,

    /// number of hardware polls per Inspect time-series entry (e.g. 10, 60)
    #[argh(option, short = 'p')]
    polls_per_series_entry: Option<u32>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// Display current ODPM driver polling configuration.
struct ConfigGetArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "reset")]
/// Reset ODPM driver polling configuration to driver defaults.
struct ConfigResetArgs {}

struct PowerRails {
    base_path: String,
    entries: Vec<fuchsia_fs::directory::DirEntry>,
}

async fn find_power_rails() -> Result<PowerRails> {
    const CANDIDATES: &[&str] = &[
        "/out/svc/fuchsia.hardware.google.odpm.Service",
        "/svc/fuchsia.hardware.google.odpm.Service",
    ];

    for &path in CANDIDATES {
        if let Ok(dir) = fuchsia_fs::directory::open_in_namespace(path, fio::PERM_READABLE) {
            if let Ok(mut entries) = fuchsia_fs::directory::readdir(&dir).await {
                if !entries.is_empty() {
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    return Ok(PowerRails { base_path: path.to_string(), entries });
                }
            }
        }
    }

    Err(anyhow!(
        "Failed to find power rails in /out/svc/fuchsia.hardware.google.odpm.Service or /svc/fuchsia.hardware.google.odpm.Service"
    ))
}

async fn run_read<W: Write>(args: ReadArgs, writer: &mut W) -> Result<()> {
    let rails = find_power_rails().await?;
    let mut matched = false;

    for entry in rails.entries {
        let node_path = format!("{}/{}/device", rails.base_path, entry.name);

        let proxy =
            match fclient::connect_to_protocol_at_path::<fgoogle_odpm::DeviceMarker>(&node_path) {
                Ok(proxy) => proxy,
                Err(e) => {
                    eprintln!("Failed to connect to {}: {:?}", node_path, e);
                    continue;
                }
            };

        let metadata = match proxy.get_rail_metadata().await {
            Ok(Ok(meta)) => meta,
            Ok(Err(status)) => {
                eprintln!(
                    "Failed to get rail metadata for {}: {:?}",
                    entry.name,
                    zx::Status::err_from_raw(status)
                );
                continue;
            }
            Err(e) => {
                eprintln!("FIDL error getting rail metadata for {}: {:?}", entry.name, e);
                continue;
            }
        };

        let rail_name = metadata.name.unwrap_or_else(|| entry.name.clone());
        let channel_id_str =
            metadata.channel_id.map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());
        let schematic_name = metadata.schematic_name.unwrap_or_default();

        if let Some(ref target) = args.rail {
            if !entry.name.eq_ignore_ascii_case(target)
                && !rail_name.eq_ignore_ascii_case(target)
                && !schematic_name.eq_ignore_ascii_case(target)
            {
                continue;
            }
        }

        matched = true;
        let rail_label =
            format!("{:<17} [{:<23},Ch{:>2}]", rail_name, schematic_name, channel_id_str);

        read_rail(&proxy, &args, &rail_label, writer).await?;
    }

    if let Some(ref target) = args.rail {
        if !matched {
            return Err(anyhow!("Rail '{}' not found", target));
        }
    }

    Ok(())
}

async fn read_rail<W: Write>(
    proxy: &fgoogle_odpm::DeviceProxy,
    args: &ReadArgs,
    rail_label: &str,
    writer: &mut W,
) -> Result<()> {
    if args.energy {
        let options = fgoogle_odpm::Options {
            measurement_type: Some(fgoogle_odpm::MeasurementType::Cumulative),
            ..Default::default()
        };
        match proxy.get_energy_joules(&options).await {
            Ok(Ok(energy_reading)) => {
                if let Some(energy_uj) = energy_reading.energy_uj {
                    let energy_j = energy_uj as f64 / 1e6;
                    if let Some(timestamp) = energy_reading.timestamp {
                        writeln!(
                            writer,
                            "Rail {}: {:>8.6} J (timestamp: {})",
                            rail_label, energy_j, timestamp
                        )?;
                    } else {
                        writeln!(writer, "Rail {}: {:>8.6} J", rail_label, energy_j)?;
                    }
                } else {
                    eprintln!("Rail {}: No energy data returned in reading", rail_label);
                }
            }
            Ok(Err(status)) => {
                eprintln!(
                    "Rail {}: Error from driver: {:?}",
                    rail_label,
                    zx::Status::err_from_raw(status)
                );
            }
            Err(e) => {
                eprintln!("Rail {}: FIDL error: {:?}", rail_label, e);
            }
        }
    } else if args.current {
        let options = fgoogle_odpm::Options::default();
        match proxy.get_current_amperes(&options).await {
            Ok(Ok(current_reading)) => {
                if let Some(current_ua) = current_reading.current_ua {
                    let current_a = current_ua as f64 / 1e6;
                    if let Some(timestamp) = current_reading.timestamp {
                        writeln!(
                            writer,
                            "Rail {}: {:>8.6} A (timestamp: {})",
                            rail_label, current_a, timestamp
                        )?;
                    } else {
                        writeln!(writer, "Rail {}: {:>8.6} A", rail_label, current_a)?;
                    }
                } else {
                    eprintln!("Rail {}: No current data returned in reading", rail_label);
                }
            }
            Ok(Err(status)) => {
                eprintln!(
                    "Rail {}: Error from driver: {:?}",
                    rail_label,
                    zx::Status::err_from_raw(status)
                );
            }
            Err(e) => {
                eprintln!("Rail {}: FIDL error: {:?}", rail_label, e);
            }
        }
    } else {
        let options = if args.instantaneous {
            fgoogle_odpm::Options {
                measurement_type: Some(fgoogle_odpm::MeasurementType::Instantaneous),
                ..Default::default()
            }
        } else {
            fgoogle_odpm::Options::default()
        };
        match proxy.get_power_watts(&options).await {
            Ok(Ok(power_reading)) => {
                if let Some(power_uw) = power_reading.power_uw {
                    let power_w = power_uw as f64 / 1e6;
                    let mode_suffix = if args.instantaneous { " (instant)" } else { "" };
                    if let Some(timestamp) = power_reading.timestamp {
                        writeln!(
                            writer,
                            "Rail {}: {:>8.6} W{} (timestamp: {})",
                            rail_label, power_w, mode_suffix, timestamp
                        )?;
                    } else {
                        writeln!(writer, "Rail {}: {:>8.6} W{}", rail_label, power_w, mode_suffix)?;
                    }
                } else {
                    eprintln!("Rail {}: No power data returned in reading", rail_label);
                }
            }
            Ok(Err(status)) => {
                eprintln!(
                    "Rail {}: Error from driver: {:?}",
                    rail_label,
                    zx::Status::err_from_raw(status)
                );
            }
            Err(e) => {
                eprintln!("Rail {}: FIDL error: {:?}", rail_label, e);
            }
        }
    }
    Ok(())
}

async fn connect_to_driver_config() -> Result<fgoogle_odpm::DriverConfigProxy> {
    // 1. Standard incoming namespace connection for discoverable protocol (/svc).
    if let Ok(proxy) = fclient::connect_to_protocol::<fgoogle_odpm::DriverConfigMarker>() {
        if proxy.get_polling_config().await.is_ok() {
            return Ok(proxy);
        }
    }

    // 2. Fallback for `ffx component explore` where outgoing capabilities live at /out/svc.
    let out_path = format!("/out/svc/{}", fgoogle_odpm::DriverConfigMarker::PROTOCOL_NAME);
    if let Ok(proxy) =
        fclient::connect_to_protocol_at_path::<fgoogle_odpm::DriverConfigMarker>(&out_path)
    {
        if proxy.get_polling_config().await.is_ok() {
            return Ok(proxy);
        }
    }

    Err(anyhow!(
        "Failed to connect to {} via /svc or /out/svc",
        fgoogle_odpm::DriverConfigMarker::PROTOCOL_NAME
    ))
}

fn format_config(cfg: &fgoogle_odpm::PollingConfig) -> String {
    let interval_nanos = cfg.poll_interval.unwrap_or(0);
    let interval_ms = interval_nanos as f64 / 1e6;
    let interval_s = interval_nanos as f64 / 1e9;
    let polls = cfg.polls_per_series_entry.unwrap_or(0);
    let series_interval_s = interval_s * (polls as f64);

    format!(
        "ODPM Polling Configuration:\n  \
         Hardware poll interval:    {:.1} ms ({:.3} s)\n  \
         Polls per series entry:    {}\n  \
         Series recording interval: {:.2} s",
        interval_ms, interval_s, polls, series_interval_s
    )
}

const MIN_POLL_INTERVAL_MS: u32 = (fgoogle_odpm::MIN_POLL_INTERVAL / 1_000_000) as u32;

async fn run_config_set_with_proxy<W: Write>(
    args: ConfigSetArgs,
    proxy: &fgoogle_odpm::DriverConfigProxy,
    writer: &mut W,
) -> Result<()> {
    if args.poll_interval_ms.is_none() && args.polls_per_series_entry.is_none() {
        return Err(anyhow!(
            "At least one configuration option must be specified (--poll-interval-ms or --polls-per-series-entry)"
        ));
    }

    let mut new_config = fgoogle_odpm::PollingConfig::default();
    if let Some(ms) = args.poll_interval_ms {
        if ms < MIN_POLL_INTERVAL_MS {
            return Err(anyhow!("poll-interval-ms must be at least {MIN_POLL_INTERVAL_MS} ms"));
        }
        let duration = zx::MonotonicDuration::from_millis(ms.into());
        new_config.poll_interval = Some(duration.into_nanos());
    }
    if let Some(polls) = args.polls_per_series_entry {
        if polls == 0 {
            return Err(anyhow!("polls-per-series-entry must be greater than 0"));
        }
        new_config.polls_per_series_entry = Some(polls);
    }

    match proxy.set_polling_config(&new_config).await {
        Ok(Ok(())) => {
            writeln!(writer, "Successfully updated ODPM polling configuration.")?;
        }
        Ok(Err(status)) => {
            return Err(anyhow!("Failed to set config: {:?}", zx::Status::err_from_raw(status)));
        }
        Err(e) => {
            return Err(anyhow!("FIDL error setting config: {:?}", e));
        }
    }

    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config_get_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::DriverConfigProxy,
    writer: &mut W,
) -> Result<()> {
    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config_reset_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::DriverConfigProxy,
    writer: &mut W,
) -> Result<()> {
    match proxy.reset_polling_config().await {
        Ok(Ok(())) => {
            writeln!(writer, "Reset ODPM polling configuration to driver defaults.")?;
        }
        Ok(Err(status)) => {
            return Err(anyhow!("Failed to reset config: {:?}", zx::Status::err_from_raw(status)));
        }
        Err(e) => {
            return Err(anyhow!("FIDL error resetting config: {:?}", e));
        }
    }
    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config<W: Write>(args: ConfigArgs, writer: &mut W) -> Result<()> {
    let proxy = connect_to_driver_config().await?;
    match args.subcommand {
        ConfigSubCommands::Set(set_args) => {
            run_config_set_with_proxy(set_args, &proxy, writer).await
        }
        ConfigSubCommands::Get(_) => run_config_get_with_proxy(&proxy, writer).await,
        ConfigSubCommands::Reset(_) => run_config_reset_with_proxy(&proxy, writer).await,
    }
}

async fn run(args: Args) -> Result<()> {
    run_with_writer(args, &mut std::io::stdout()).await
}

async fn run_with_writer<W: Write>(args: Args, writer: &mut W) -> Result<()> {
    if let Some(cmd) = args.command {
        match cmd {
            SubCommands::Read(read_args) => run_read(read_args, writer).await,
            SubCommands::Config(config_args) => run_config(config_args, writer).await,
        }
    } else {
        writeln!(
            writer,
            "{}",
            Args::from_args(&["odpm_util"], &["--help"]).unwrap_err().output.trim_end()
        )?;
        Ok(())
    }
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    if let Err(e) = run(args).await {
        eprintln!("Error: {e}");
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn test_args_parsing_no_subcommand() {
        let args = Args::from_args(&["odpm_util"], &[]).unwrap();
        assert_eq!(args, Args { command: None });

        assert!(Args::from_args(&["odpm_util"], &["cpu_big"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-i"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--instantaneous"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-i"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-c"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--current"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-c"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-e"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--energy"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-e"]).is_err());
    }

    #[test]
    fn test_args_parsing_read() {
        let args = Args::from_args(&["odpm_util"], &["read"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: false,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-i"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: true,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--instantaneous"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: true,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-i"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: true,
                    current: false,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-c"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: true,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--current"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: true,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-c"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: true,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-e"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: true,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--energy"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: true,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-e"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: false,
                    energy: true,
                })),
            }
        );
    }

    #[test]
    fn test_args_parsing_config() {
        let args = Args::from_args(&["odpm_util"], &["config", "set"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Set(ConfigSetArgs {
                        poll_interval_ms: None,
                        polls_per_series_entry: None,
                    }),
                })),
            }
        );

        let args =
            Args::from_args(&["odpm_util"], &["config", "set", "-i", "100", "-p", "10"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Set(ConfigSetArgs {
                        poll_interval_ms: Some(100),
                        polls_per_series_entry: Some(10),
                    }),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["config", "get"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Get(ConfigGetArgs {}),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["config", "reset"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Reset(ConfigResetArgs {}),
                })),
            }
        );
    }

    #[fuchsia::test]
    async fn test_run_no_subcommand_displays_help() {
        let mut buf = Vec::new();
        let res = run_with_writer(Args { command: None }, &mut buf).await;
        assert!(res.is_ok());

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Usage: odpm_util"));
        assert!(output.contains("read"));
        assert!(output.contains("config"));
    }

    #[test]
    fn test_format_config() {
        let cfg = fgoogle_odpm::PollingConfig {
            poll_interval: Some(1_000_000_000),
            polls_per_series_entry: Some(60),
            ..Default::default()
        };
        let formatted = format_config(&cfg);
        assert!(formatted.contains("1000.0 ms"));
        assert!(formatted.contains("60"));
        assert!(formatted.contains("60.00 s"));
    }

    #[fuchsia::test]
    async fn test_run_config_set_update() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::DriverConfigRequest::SetPollingConfig { payload, responder } => {
                        assert_eq!(payload.poll_interval, Some(250_000_000));
                        assert_eq!(payload.polls_per_series_entry, Some(10));
                        let _ = responder.send(Ok(()));
                    }
                    fgoogle_odpm::DriverConfigRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(250_000_000),
                            polls_per_series_entry: Some(10),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ConfigSetArgs { poll_interval_ms: Some(250), polls_per_series_entry: Some(10) };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Successfully updated ODPM polling configuration."));
        assert!(output.contains("250.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_set_no_options_error() {
        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let args = ConfigSetArgs { poll_interval_ms: None, polls_per_series_entry: None };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_run_config_get() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::DriverConfigRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(1_000_000_000),
                            polls_per_series_entry: Some(60),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_config_get_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("1000.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_reset() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::DriverConfigRequest::ResetPollingConfig { responder } => {
                        let _ = responder.send(Ok(()));
                    }
                    fgoogle_odpm::DriverConfigRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(1_000_000_000),
                            polls_per_series_entry: Some(60),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_config_reset_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Reset ODPM polling configuration to driver defaults."));
        assert!(output.contains("1000.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_set_below_min_interval_error() {
        let expected_msg = format!("poll-interval-ms must be at least {MIN_POLL_INTERVAL_MS} ms");

        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let args = ConfigSetArgs { poll_interval_ms: Some(0), polls_per_series_entry: None };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains(&expected_msg));

        let args = ConfigSetArgs {
            poll_interval_ms: Some(MIN_POLL_INTERVAL_MS - 1),
            polls_per_series_entry: None,
        };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains(&expected_msg));
    }

    #[fuchsia::test]
    async fn test_run_config_set_zero_polls_error() {
        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DriverConfigMarker>();
        let args = ConfigSetArgs { poll_interval_ms: None, polls_per_series_entry: Some(0) };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_read_rail_energy() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::DeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::DeviceRequest::GetEnergyJoules { responder, .. } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::EnergyReading {
                            energy_uj: Some(12_345_678),
                            timestamp: Some(10_000_000_000),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ReadArgs {
            rail: Some("gpu".to_string()),
            instantaneous: false,
            current: false,
            energy: true,
        };
        let mut buf = Vec::new();
        let res =
            read_rail(&proxy, &args, "gpu               [S2S_VDD_GPU            ,Ch42]", &mut buf)
                .await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("12.345678 J"));
        assert!(output.contains("timestamp: 10000000000"));
        drop(proxy);
        task.await;
    }
}
