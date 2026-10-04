// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Battery CLI Diagnostic and Control Tool.

mod battery;
mod charger;
mod common;
mod spmi;

use anyhow::Result;
use argh::FromArgs;
use charger::{ChargerModeArg, ModeArg};

#[derive(FromArgs, Debug, PartialEq)]
/// Inspect battery telemetry and control power/charging.
pub struct Args {
    #[argh(option, short = 'p')]
    /// optional specific service instance or device path (e.g.
    /// '/svc/fuchsia.hardware.power.battery.Service/default')
    pub path: Option<String>,

    #[argh(subcommand)]
    pub command: Option<Subcommand>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum Subcommand {
    Get(GetCommand),
    Watch(WatchCommand),
    Mode(ModeCommand),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// inspect battery and charger telemetry
pub struct GetCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "watch")]
/// stream real-time battery and charger status updates via hanging-get
pub struct WatchCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "mode")]
/// set charger operating mode (charging/usb, passthrough, discharging/battery, otg, auto)
pub struct ModeCommand {
    #[argh(positional)]
    /// mode: charging/usb, passthrough, discharging/battery, otg, or auto (clear overrides)
    pub mode: ModeArg,
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let path = args.path.as_deref();

    match args.command {
        None | Some(Subcommand::Get(_)) => battery::get_battery_info(path).await,
        Some(Subcommand::Watch(_)) => battery::watch_battery(path).await,
        Some(Subcommand::Mode(ModeCommand { mode })) => match mode {
            ModeArg::Set(ChargerModeArg(mode)) => charger::set_charger_mode(path, mode).await,
            ModeArg::Auto => charger::clear_charger_overrides(path).await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_hardware_power_charger as fcharger;

    #[test]
    fn test_argh_args_parsing() {
        let args = Args::from_args(&["batteryutil"], &["get"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Get(GetCommand {})));

        let args =
            Args::from_args(&["batteryutil"], &["-p", "/svc/test", "mode", "charging"]).unwrap();
        assert_eq!(args.path, Some("/svc/test".to_string()));
        assert_eq!(
            args.command,
            Some(Subcommand::Mode(ModeCommand {
                mode: ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Charging))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["watch"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Watch(WatchCommand {})));

        let args = Args::from_args(&["batteryutil"], &["mode", "battery"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Mode(ModeCommand {
                mode: ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Discharging))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["mode", "auto"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Mode(ModeCommand { mode: ModeArg::Auto })));
    }
}
