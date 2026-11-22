// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use argh::FromArgs;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_usb_policy as usb_policy;

mod config;
mod health;
mod inspect;

#[derive(FromArgs, PartialEq, Debug)]
/// USB diagnostics and configuration CLI tool.
struct UsbCliArgs {
    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Health(HealthArgs),
    Inspect(InspectArgs),
    Diagnostics(DiagnosticsArgs),
    Diag(DiagArgs),
    GetConfig(GetConfigArgs),
    SetConfig(SetConfigArgs),
    CableBreaker(CableBreakerArgs),
}

#[derive(FromArgs, PartialEq, Debug)]
/// Simulates a physical USB-C cable unplug/replug by opening CC1/CC2 terminations in the TCPC
/// (e.g. `usb-cli cable-breaker --duration 10s` or `usb-cli cable-breaker --reconnect`).
#[argh(subcommand, name = "cable-breaker")]
struct CableBreakerArgs {
    /// duration before automatically waking and reconnecting CC lines, with a unit suffix
    /// ("500000000ns", "500us", "250ms", "10s") or in whole seconds ("10"), measured from the
    /// request rather than from the CC drop. Values under the Type-C CC debounce (~200ms) may
    /// not register as an unplug. Without it, the cable stays broken until
    /// `usb-cli cable-breaker --reconnect`.
    #[argh(option, short = 'd', from_str_fn(parse_duration_nanos))]
    duration: Option<i64>,

    /// restores the USB-C cable connection after a prior `cable-breaker`, cancelling any
    /// pending automatic reconnect.
    #[argh(switch, short = 'r')]
    reconnect: bool,

    /// waits for the automatic reconnect to finish before exiting (requires `--duration`).
    #[argh(switch, short = 'w')]
    wait: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the USB policy health report.
#[argh(subcommand, name = "health")]
struct HealthArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the device-side USB Inspect diagnostics.
#[argh(subcommand, name = "inspect")]
struct InspectArgs {}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints both USB health report and Inspect diagnostics.
#[argh(subcommand, name = "diagnostics")]
struct DiagnosticsArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints both USB health report and Inspect diagnostics (alias for 'diagnostics').
#[argh(subcommand, name = "diag")]
struct DiagArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the current USB peripheral configuration in JSON format.
#[argh(subcommand, name = "get-config")]
struct GetConfigArgs {}

#[derive(FromArgs, PartialEq, Debug)]
/// Sets the USB peripheral configuration.
///
/// Supported input formats:
///   - Single configuration (comma-separated functions):
///       usb-cli set-config "cdc,adb"
///       usb-cli set-config "sourcesink"
///       usb-cli set-config "loopback"
///   - Multi-configuration (semicolon-separated configurations):
///       usb-cli set-config "sourcesink;loopback"
///       usb-cli set-config "cdc;vsock"
///       usb-cli set-config "cdc,adb;vsock"
///   - JSON configuration string:
///       usb-cli set-config '{"configurations": [["sourcesink"]]}'
///       usb-cli set-config '{"configurations": [["cdc", "sourcesink"], ["loopback"]]}'
///   - JSON configuration file path:
///       usb-cli set-config /path/to/usb_config.json
#[argh(subcommand, name = "set-config")]
struct SetConfigArgs {
    /// configuration string (e.g. "cdc,adb", "cdc;vsock", "sourcesink;loopback"), inline JSON, or JSON file path
    #[argh(positional)]
    config: String,
}

#[fuchsia::main(logging_tags = ["usb-cli"])]
async fn main() {
    if let Err(e) = run_cli().await {
        eprintln!("usb-cli error: {:?}", e);
        std::process::exit(1);
    }
    println!("[usb-cli:DONE]");
}

fn connect_protocol_any<P: DiscoverableProtocolMarker>() -> Result<P::Proxy, Error> {
    let ns_path = format!("/ns/svc/{}", P::PROTOCOL_NAME);
    if std::path::Path::new(&ns_path).exists() {
        return fuchsia_component::client::connect_to_protocol_at_path::<P>(&ns_path)
            .with_context(|| format!("Failed to connect to {ns_path}"));
    }
    let exposed_path = format!("/exposed/{}", P::PROTOCOL_NAME);
    if std::path::Path::new(&exposed_path).exists() {
        return fuchsia_component::client::connect_to_protocol_at_path::<P>(&exposed_path)
            .with_context(|| format!("Failed to connect to {exposed_path}"));
    }
    fuchsia_component::client::connect_to_protocol::<P>()
        .with_context(|| format!("Failed to connect to {} protocol", P::PROTOCOL_NAME))
}

async fn get_configuration_client() -> Result<usb_policy::ConfigurationProxy, Error> {
    connect_protocol_any::<usb_policy::ConfigurationMarker>()
}

async fn run_get_config(_args: GetConfigArgs) -> Result<(), Error> {
    let config_client = get_configuration_client().await?;
    let (device_desc, config_descriptors) = config_client
        .get_configuration()
        .await
        .context("Failed FIDL call get_configuration")?
        .map_err(zx::Status::err_from_raw)
        .context("GetConfiguration returned an error status")?;

    let configurations = config::config_descriptors_to_names(&config_descriptors);

    let json_output = config::UsbConfigJson {
        configurations,
        id_vendor: (device_desc.id_vendor != 0).then_some(device_desc.id_vendor),
        id_product: Some(device_desc.id_product),
        product: (!device_desc.product.is_empty()).then_some(device_desc.product),
    };

    let serialized = serde_json::to_string_pretty(&json_output).context("Failed to format JSON")?;
    println!("{}", serialized);
    Ok(())
}

async fn run_set_config(args: SetConfigArgs) -> Result<(), Error> {
    let parsed_config = config::load_config_input(&args.config)?;
    let config_descriptors = config::resolve_config_descriptors(&parsed_config)?;
    let config_client = get_configuration_client().await?;

    let (device_desc, _) = config_client
        .get_configuration()
        .await
        .context("Failed FIDL call get_configuration")?
        .map_err(zx::Status::err_from_raw)
        .context("GetConfiguration returned an error status")?;

    let num_configurations =
        u8::try_from(config_descriptors.len()).context("Too many configurations")?;
    let (device_desc, standard_derived) =
        config::update_device_descriptor(&parsed_config, device_desc, num_configurations);

    if standard_derived {
        println!(
            "Using standard USB identifiers: VID 0x{:04x}, PID 0x{:04x} ('{}')",
            device_desc.id_vendor, device_desc.id_product, device_desc.product
        );
    } else {
        match (&parsed_config.id_product, &parsed_config.product) {
            (None, None) => {
                println!(
                    "Note: No standard USB PID found for this configuration; retaining current PID (0x{:04x}) and product string.",
                    device_desc.id_product
                );
            }
            (None, Some(_)) => {
                println!(
                    "Note: No standard USB PID found for this configuration; retaining current PID (0x{:04x}).",
                    device_desc.id_product
                );
            }
            _ => {}
        }
    }

    println!(
        "Applying new configuration via Policy ({} configuration(s): {:?})...",
        config_descriptors.len(),
        config::config_descriptors_to_names(&config_descriptors)
    );
    config_client
        .set_configuration(&device_desc, &config_descriptors)
        .await
        .context("Failed set_configuration FIDL call")?
        .map_err(zx::Status::err_from_raw)
        .context("SetConfiguration returned an error status")?;

    println!("Successfully applied USB peripheral configuration.");

    let (active_device_desc, active_config_descriptors) = config_client
        .get_configuration()
        .await
        .context("Failed FIDL call get_configuration to query active configuration")?
        .map_err(zx::Status::err_from_raw)
        .context("GetConfiguration returned an error status")?;

    let active_configurations = config::config_descriptors_to_names(&active_config_descriptors);

    println!(
        "Active configuration (VID: 0x{:04x}, PID: 0x{:04x}, {} configuration(s)): {:?}",
        active_device_desc.id_vendor,
        active_device_desc.id_product,
        active_configurations.len(),
        active_configurations
    );
    Ok(())
}

async fn get_health_report() -> Result<usb_policy::HealthReport, Error> {
    let health = connect_protocol_any::<usb_policy::HealthMarker>()?;

    health
        .get_report()
        .await
        .map_err(|e| anyhow::format_err!("Failed to communicate (get_report): {e:?}"))?
        .map_err(|e| anyhow::format_err!("Failed to get report (zx status): {e:?}"))
}

async fn run_health(args: HealthArgs) -> Result<(), Error> {
    match get_health_report().await {
        Ok(report) => {
            if args.verbose {
                println!("{}", health::format_verbose(&report));
            } else {
                println!("{}", health::format_dashboard(&report));
            }
            Ok(())
        }
        Err(e) => {
            println!(
                "USB Policy Health service not available: fuchsia.usb.policy.Health not found."
            );
            if args.verbose {
                println!("Error details: {e:?}");
            }
            Err(e)
        }
    }
}

async fn run_diagnostics(verbose: bool) -> Result<(), Error> {
    match get_health_report().await {
        Ok(report) => {
            if verbose {
                println!("{}", health::format_verbose(&report));
            } else {
                println!("{}", health::format_dashboard(&report));
            }
        }
        Err(_) => {
            println!("USB Policy Health service not available (skipping).");
        }
    }

    if let Err(e) = inspect::print_usb_inspect_diagnostics().await {
        println!("Failed to print USB inspect diagnostics: {:?}", e);
    }

    Ok(())
}

/// Parses a cable-breaker duration into nanoseconds. Accepts "ns", "us", "ms", or "s" suffixes,
/// or bare whole seconds (e.g. "500000000ns", "500us", "250ms", "10s", or "10").
fn parse_duration_nanos(duration: &str) -> Result<i64, String> {
    let trimmed = duration.trim();
    let (numeric, nanos_per_unit) = if let Some(ns) = trimmed.strip_suffix("ns") {
        (ns, 1i64)
    } else if let Some(us) = trimmed.strip_suffix("us") {
        (us, 1_000i64)
    } else if let Some(ms) = trimmed.strip_suffix("ms") {
        (ms, 1_000_000i64)
    } else {
        (trimmed.strip_suffix('s').unwrap_or(trimmed), 1_000_000_000i64)
    };
    let count: i64 = numeric.parse().map_err(|_| {
        format!(
            "Invalid duration '{duration}': expected e.g. '500000000ns', '500us', '250ms', \
             '10s', or '10'"
        )
    })?;
    if count <= 0 {
        return Err("Duration must be greater than 0".to_string());
    }
    count.checked_mul(nanos_per_unit).ok_or_else(|| "Duration is too large".to_string())
}

fn format_duration_nanos(nanos: i64) -> String {
    if nanos % 1_000_000_000 == 0 {
        format!("{}s", nanos / 1_000_000_000)
    } else if nanos % 1_000_000 == 0 {
        format!("{}ms", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!("{}us", nanos / 1_000)
    } else {
        format!("{nanos}ns")
    }
}

async fn execute_cable_breaker(
    config_client: &usb_policy::ConfigurationProxy,
    args: &CableBreakerArgs,
) -> Result<(), Error> {
    if args.reconnect {
        if args.duration.is_some() {
            anyhow::bail!("Cannot specify both --reconnect and --duration");
        }
        if args.wait {
            anyhow::bail!("Cannot specify both --reconnect and --wait");
        }
        println!("Restoring Type-C cable connection (closing CC1/CC2)...");
        config_client
            .connect_cable()
            .await
            .context("Failed FIDL call connect_cable")?
            .map_err(zx::Status::err_from_raw)
            .context("ConnectCable returned an error status")?;
        println!("Type-C cable connection restored.");
        return Ok(());
    }

    if args.wait && args.duration.is_none() {
        anyhow::bail!("Cannot specify --wait without --duration");
    }

    let duration_nanos = if let Some(nanos) = args.duration {
        if nanos <= 0 {
            anyhow::bail!("Duration must be greater than 0");
        }
        let action = if args.wait { "waiting for" } else { "scheduling" };
        println!(
            "Breaking Type-C cable connection (opening CC1/CC2) and {action} wake alarm \
             reconnect ({})...",
            format_duration_nanos(nanos)
        );
        nanos
    } else {
        println!("Breaking Type-C cable connection (opening CC1/CC2)...");
        0
    };

    config_client
        .disconnect_cable(duration_nanos, args.wait)
        .await
        .context("Failed FIDL call disconnect_cable")?
        .map_err(zx::Status::err_from_raw)
        .context("DisconnectCable returned an error status")?;
    if args.wait {
        println!("Type-C cable connection restored.");
    } else {
        println!("Type-C cable connection broken.");
    }
    Ok(())
}

async fn run_cable_breaker(args: CableBreakerArgs) -> Result<(), Error> {
    let config_client = get_configuration_client().await?;
    execute_cable_breaker(&config_client, &args).await
}

async fn run_cli() -> Result<(), Error> {
    let args: UsbCliArgs = argh::from_env();
    match args.subcommand {
        SubCommand::Health(sc_args) => run_health(sc_args).await,
        SubCommand::Inspect(_) => inspect::print_usb_inspect_diagnostics().await,
        SubCommand::Diagnostics(sc_args) => run_diagnostics(sc_args.verbose).await,
        SubCommand::Diag(sc_args) => run_diagnostics(sc_args.verbose).await,
        SubCommand::GetConfig(sc_args) => run_get_config(sc_args).await,
        SubCommand::SetConfig(sc_args) => run_set_config(sc_args).await,
        SubCommand::CableBreaker(sc_args) => run_cable_breaker(sc_args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn test_parse_missing_subcommand() {
        assert!(UsbCliArgs::from_args(&["usb-cli"], &[]).is_err());
    }

    #[test]
    fn test_parse_cable_breaker() {
        let cb_default = UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker"]).unwrap();
        assert_eq!(
            cb_default,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: None,
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_duration =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "--duration", "10s"]).unwrap();
        assert_eq!(
            cb_duration,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(10_000_000_000),
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_short = UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "15"]).unwrap();
        assert_eq!(
            cb_short,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(15_000_000_000),
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_ms = UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "250ms"]).unwrap();
        assert_eq!(
            cb_ms,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(250_000_000),
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_us = UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "500us"]).unwrap();
        assert_eq!(
            cb_us,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(500_000),
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_ns =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "500000000ns"]).unwrap();
        assert_eq!(
            cb_ns,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(500_000_000),
                    reconnect: false,
                    wait: false,
                })
            }
        );

        let cb_wait =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "5s", "--wait"]).unwrap();
        assert_eq!(
            cb_wait,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(5_000_000_000),
                    reconnect: false,
                    wait: true,
                })
            }
        );

        let cb_wait_short =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "5s", "-w"]).unwrap();
        assert_eq!(
            cb_wait_short,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: Some(5_000_000_000),
                    reconnect: false,
                    wait: true,
                })
            }
        );

        let cb_reconnect =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "--reconnect"]).unwrap();
        assert_eq!(
            cb_reconnect,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: None,
                    reconnect: true,
                    wait: false,
                })
            }
        );

        let cb_reconnect_short =
            UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-r"]).unwrap();
        assert_eq!(
            cb_reconnect_short,
            UsbCliArgs {
                subcommand: SubCommand::CableBreaker(CableBreakerArgs {
                    duration: None,
                    reconnect: true,
                    wait: false,
                })
            }
        );

        assert_eq!(parse_duration_nanos("10s").unwrap(), 10_000_000_000);
        assert_eq!(parse_duration_nanos("10").unwrap(), 10_000_000_000);
        assert_eq!(parse_duration_nanos("250ms").unwrap(), 250_000_000);
        assert_eq!(parse_duration_nanos(" 1ms ").unwrap(), 1_000_000);
        assert_eq!(parse_duration_nanos("500us").unwrap(), 500_000);
        assert_eq!(parse_duration_nanos("500000000ns").unwrap(), 500_000_000);
        assert!(parse_duration_nanos("0s").is_err());
        assert!(parse_duration_nanos("0ms").is_err());
        assert!(parse_duration_nanos("0us").is_err());
        assert!(parse_duration_nanos("0ns").is_err());
        assert!(parse_duration_nanos("-5s").is_err());
        assert!(parse_duration_nanos("10m").is_err());
        assert!(parse_duration_nanos("1.5s").is_err());
        assert!(parse_duration_nanos("99999999999999999999s").is_err());
        assert!(UsbCliArgs::from_args(&["usb-cli"], &["cable-breaker", "-d", "0s"]).is_err());
    }

    /// Serves `fuchsia.usb.policy.Configuration` until the client closes it, replying to
    /// `DisconnectCable` and `ConnectCable` with `result`. Returns the requests it received,
    /// in order.
    fn serve_configuration(
        mut stream: usb_policy::ConfigurationRequestStream,
        result: Result<(), zx::Status>,
    ) -> fuchsia_async::Task<Vec<String>> {
        fuchsia_async::Task::local(async move {
            let mut events = Vec::new();
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    usb_policy::ConfigurationRequest::DisconnectCable {
                        duration,
                        wait,
                        responder,
                    } => {
                        events.push(format!("disconnect_cable:{duration}:{wait}"));
                        responder.send(result.map_err(zx::Status::into_raw)).unwrap();
                    }
                    usb_policy::ConfigurationRequest::ConnectCable { responder } => {
                        events.push("connect_cable".to_string());
                        responder.send(result.map_err(zx::Status::into_raw)).unwrap();
                    }
                    request => panic!("Unexpected request: {request:?}"),
                }
            }
            events
        })
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_with_duration() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        execute_cable_breaker(
            &config_proxy,
            &CableBreakerArgs { duration: Some(10_000_000_000), reconnect: false, wait: false },
        )
        .await
        .expect("execute_cable_breaker should succeed");
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["disconnect_cable:10000000000:false"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_with_subsecond_duration() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        execute_cable_breaker(
            &config_proxy,
            &CableBreakerArgs { duration: Some(250_000_000), reconnect: false, wait: false },
        )
        .await
        .expect("execute_cable_breaker should succeed");
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["disconnect_cable:250000000:false"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_with_wait() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        execute_cable_breaker(
            &config_proxy,
            &CableBreakerArgs { duration: Some(250_000_000), reconnect: false, wait: true },
        )
        .await
        .expect("execute_cable_breaker --wait should succeed");
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["disconnect_cable:250000000:true"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_wait_requires_duration() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        assert!(
            execute_cable_breaker(
                &config_proxy,
                &CableBreakerArgs { duration: None, reconnect: false, wait: true },
            )
            .await
            .is_err()
        );
        drop(config_proxy);

        assert_eq!(server_task.await, Vec::<String>::new());
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_wait_conflicts_with_reconnect() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        assert!(
            execute_cable_breaker(
                &config_proxy,
                &CableBreakerArgs { duration: None, reconnect: true, wait: true },
            )
            .await
            .is_err()
        );
        drop(config_proxy);

        assert_eq!(server_task.await, Vec::<String>::new());
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_reconnect() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        execute_cable_breaker(
            &config_proxy,
            &CableBreakerArgs { duration: None, reconnect: true, wait: false },
        )
        .await
        .expect("execute_cable_breaker --reconnect should succeed");
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["connect_cable"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_reconnect_failure() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Err(zx::Status::UNAVAILABLE));

        assert!(
            execute_cable_breaker(
                &config_proxy,
                &CableBreakerArgs { duration: None, reconnect: true, wait: false },
            )
            .await
            .is_err()
        );
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["connect_cable"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_reconnect_conflicts_with_duration() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        assert!(
            execute_cable_breaker(
                &config_proxy,
                &CableBreakerArgs { duration: Some(10_000_000_000), reconnect: true, wait: false },
            )
            .await
            .is_err()
        );
        drop(config_proxy);

        assert_eq!(server_task.await, Vec::<String>::new());
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_without_duration() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Ok(()));

        execute_cable_breaker(
            &config_proxy,
            &CableBreakerArgs { duration: None, reconnect: false, wait: false },
        )
        .await
        .expect("execute_cable_breaker should succeed");
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["disconnect_cable:0:false"]);
    }

    #[fuchsia::test]
    async fn test_execute_cable_breaker_failure() {
        let (config_proxy, config_stream) =
            fidl::endpoints::create_proxy_and_stream::<usb_policy::ConfigurationMarker>();
        let server_task = serve_configuration(config_stream, Err(zx::Status::UNAVAILABLE));

        assert!(
            execute_cable_breaker(
                &config_proxy,
                &CableBreakerArgs { duration: Some(10_000_000_000), reconnect: false, wait: false },
            )
            .await
            .is_err()
        );
        drop(config_proxy);

        assert_eq!(server_task.await, vec!["disconnect_cable:10000000000:false"]);
    }

    #[test]
    fn test_parse_health() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["health"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs { subcommand: SubCommand::Health(HealthArgs { verbose: false }) }
        );

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["health", "-v"]).unwrap();
        assert_eq!(
            args_v,
            UsbCliArgs { subcommand: SubCommand::Health(HealthArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_inspect() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["inspect"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::Inspect(InspectArgs {}) });
    }

    #[test]
    fn test_parse_diagnostics() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["diagnostics"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs { subcommand: SubCommand::Diagnostics(DiagnosticsArgs { verbose: false }) }
        );

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["diagnostics", "--verbose"]).unwrap();
        assert_eq!(
            args_v,
            UsbCliArgs { subcommand: SubCommand::Diagnostics(DiagnosticsArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_diag_alias() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["diag"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: false }) });

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["diag", "-v"]).unwrap();
        assert_eq!(args_v, UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: true }) });

        let args_verbose = UsbCliArgs::from_args(&["usb-cli"], &["diag", "--verbose"]).unwrap();
        assert_eq!(
            args_verbose,
            UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_get_config() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["get-config"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::GetConfig(GetConfigArgs {}) });
    }

    #[test]
    fn test_parse_set_config() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["set-config", "cdc,sourcesink"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs {
                subcommand: SubCommand::SetConfig(SetConfigArgs {
                    config: "cdc,sourcesink".to_string(),
                }),
            }
        );

        let multi_args =
            UsbCliArgs::from_args(&["usb-cli"], &["set-config", "cdc,sourcesink;loopback"])
                .unwrap();
        assert_eq!(
            multi_args,
            UsbCliArgs {
                subcommand: SubCommand::SetConfig(SetConfigArgs {
                    config: "cdc,sourcesink;loopback".to_string(),
                }),
            }
        );

        let test_args =
            UsbCliArgs::from_args(&["usb-cli"], &["set-config", "sourcesink;loopback"]).unwrap();
        assert_eq!(
            test_args,
            UsbCliArgs {
                subcommand: SubCommand::SetConfig(SetConfigArgs {
                    config: "sourcesink;loopback".to_string(),
                }),
            }
        );

        let json_input = r#"{"configurations": [["sourcesink"]]}"#;
        let json_args = UsbCliArgs::from_args(&["usb-cli"], &["set-config", json_input]).unwrap();
        assert_eq!(
            json_args,
            UsbCliArgs {
                subcommand: SubCommand::SetConfig(SetConfigArgs { config: json_input.to_string() }),
            }
        );
    }
}
