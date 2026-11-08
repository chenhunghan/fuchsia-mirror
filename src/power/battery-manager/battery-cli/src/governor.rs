// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Subcommands of `governor`, which use `fuchsia.power.battery.test.GovernorDebug` to inspect and
//! control the charge governors that battery-manager runs, and to override their votes.

use anyhow::{Error, bail, format_err};
use argh::{FromArgValue, FromArgs};
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery_test as spower;
use fuchsia_component::client::connect_to_protocol;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Inspect and control charge governors and override their votes.
#[derive(FromArgs, Debug, PartialEq)]
struct GovernorArgs {
    #[argh(subcommand)]
    command: Option<GovernorSubcommand>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum GovernorSubcommand {
    List(ListCommand),
    Control(ControlCommand),
    Pause(PauseCommand),
    Resume(ResumeCommand),
    Override(OverrideCommand),
    Clear(ClearCommand),
}

/// List charge governors and their state.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "list")]
struct ListCommand {}

/// Show each voter's charge control votes and the effective values.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "control")]
struct ControlCommand {}

/// Stop a charge governor and withdraw its votes until resumed.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "pause")]
struct PauseCommand {
    /// name of the charge governor to pause (e.g. msc)
    #[argh(positional)]
    governor: String,
}

/// Restart a paused charge governor.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "resume")]
struct ResumeCommand {
    /// name of the charge governor to resume (e.g. msc)
    #[argh(positional)]
    governor: String,
}

#[derive(FromArgValue, Clone, Copy, Debug, PartialEq)]
enum OperatingMode {
    Charging,
    Passthrough,
    Discharging,
}

impl From<OperatingMode> for fcharger::OperatingMode {
    fn from(mode: OperatingMode) -> Self {
        match mode {
            OperatingMode::Charging => Self::Charging,
            OperatingMode::Passthrough => Self::Passthrough,
            OperatingMode::Discharging => Self::Discharging,
        }
    }
}

const MICROS_PER_MILLI: f64 = 1_000.0;
const MICROS_PER_UNIT: f64 = 1_000_000.0;

/// Parses a current or voltage string into micro-units (`uA` or `uV`), supporting integer
/// micro-units (`"1500000"`, `"1500000uA"`), milli-units (`"1500mA"`, `"4350mV"`), or base units
/// (`"1.5A"`, `"4.35V"`).
fn parse_micro_value(input: &str, unit: &str) -> Result<u32, String> {
    let s = input.trim();
    let s_lower = s.to_ascii_lowercase();
    let unit_lower = unit.to_ascii_lowercase();
    let Some(prefix) = s_lower.strip_suffix(&unit_lower) else {
        return s.parse().map_err(|_| format!("invalid value '{input}'"));
    };
    if let Some(stripped) = prefix.strip_suffix('u') {
        return s[..stripped.len()].trim().parse().map_err(|_| format!("invalid value '{input}'"));
    }
    let (num_str, scale) = if let Some(stripped) = prefix.strip_suffix('m') {
        (&s[..stripped.len()], MICROS_PER_MILLI)
    } else {
        (&s[..prefix.len()], MICROS_PER_UNIT)
    };
    let num: f64 = num_str.trim().parse().map_err(|_| format!("invalid value '{input}'"))?;
    let micros = (num * scale).round();
    if !(0.0..=f64::from(u32::MAX)).contains(&micros) {
        return Err(format!("value '{input}' is out of range"));
    }
    Ok(micros as u32)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CurrentArg(u32);

impl FromStr for CurrentArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_micro_value(s, "A").map(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VoltageArg(u32);

impl FromStr for VoltageArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_micro_value(s, "V").map(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DurationArg(i64);

impl FromStr for DurationArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (num, secs_per_unit) = match s.strip_suffix("min") {
            Some(m) => (m, 60u64),
            None => (s.strip_suffix('s').unwrap_or(s), 1),
        };
        let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
        n.checked_mul(secs_per_unit)
            .map(Duration::from_secs)
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .map(Self)
            .ok_or_else(|| format!("duration '{s}' is out of range"))
    }
}

/// Override all or one charge governor's votes.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "override")]
struct OverrideCommand {
    /// governor to override (overrides all governors if omitted)
    #[argh(positional)]
    governor: Option<String>,
    /// operating mode (charging, passthrough, discharging)
    #[argh(option)]
    mode: Option<OperatingMode>,
    /// input current limit (e.g. 2A, 2000mA, 2000000)
    #[argh(option)]
    icl: Option<CurrentArg>,
    /// charge current limit (e.g. 1.5A, 1500mA, 1500000)
    #[argh(option)]
    fcc: Option<CurrentArg>,
    /// float voltage (e.g. 4.35V, 4350mV, 4350000)
    #[argh(option)]
    fv: Option<VoltageArg>,
    /// charge termination current (e.g. 200mA)
    #[argh(option)]
    iterm: Option<CurrentArg>,
    /// input voltage limit (e.g. 4.6V)
    #[argh(option)]
    vin: Option<VoltageArg>,
    /// top-off duration (e.g. 30s, 10min, 600)
    #[argh(option)]
    topoff: Option<DurationArg>,
}

impl OverrideCommand {
    fn options(&self) -> fcharger::ControlOptions {
        fcharger::ControlOptions {
            operating_mode: self.mode.map(Into::into),
            input_current_limit_ua: self.icl.map(|v| v.0),
            charge_current_limit_ua: self.fcc.map(|v| v.0),
            float_voltage_uv: self.fv.map(|v| v.0),
            charge_term_current_ua: self.iterm.map(|v| v.0),
            input_voltage_limit_uv: self.vin.map(|v| v.0),
            top_off_duration: self.topoff.map(|v| v.0),
            ..Default::default()
        }
    }
}

/// Clear overrides on all or one charge governor's votes (all fields if no flags given).
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "clear")]
struct ClearCommand {
    /// governor whose overrides to clear (clears aggregate overrides if omitted)
    #[argh(positional)]
    governor: Option<String>,
    /// clear operating mode override
    #[argh(switch)]
    mode: bool,
    /// clear input current limit override
    #[argh(switch)]
    icl: bool,
    /// clear charge current limit override
    #[argh(switch)]
    fcc: bool,
    /// clear float voltage override
    #[argh(switch)]
    fv: bool,
    /// clear charge termination current override
    #[argh(switch)]
    iterm: bool,
    /// clear input voltage limit override
    #[argh(switch)]
    vin: bool,
    /// clear top-off duration override
    #[argh(switch)]
    topoff: bool,
}

impl ClearCommand {
    /// Builds a `ClearControl` mask with `Some` set on each selected field (field values are
    /// ignored by `ClearControl`; only presence selects which overrides to clear).
    fn mask(&self) -> fcharger::ControlOptions {
        fcharger::ControlOptions {
            operating_mode: self.mode.then_some(fcharger::OperatingMode::Charging),
            input_current_limit_ua: self.icl.then_some(0),
            charge_current_limit_ua: self.fcc.then_some(0),
            float_voltage_uv: self.fv.then_some(0),
            charge_term_current_ua: self.iterm.then_some(0),
            input_voltage_limit_uv: self.vin.then_some(0),
            top_off_duration: self.topoff.then_some(0),
            ..Default::default()
        }
    }
}

/// Formats micro-units (`uA` or `uV`) in base units (`A` or `V`).
struct MicroUnit(u32, &'static str);

impl fmt::Display for MicroUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3} {}", f64::from(self.0) / MICROS_PER_UNIT, self.1)
    }
}

struct DisplayControlOptions<'a>(Option<&'a fcharger::ControlOptions>);

impl fmt::Display for DisplayControlOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(o) = self.0 else {
            return f.write_str("-");
        };
        let mut sep = "";
        let mut write_field = |f: &mut fmt::Formatter<'_>, k: &str, v: fmt::Arguments<'_>| {
            write!(f, "{sep}{k}={v}")?;
            sep = " ";
            Ok(())
        };
        if let Some(v) = o.operating_mode {
            write_field(f, "mode", format_args!("{v:?}"))?;
        }
        if let Some(v) = o.input_current_limit_ua {
            write_field(f, "icl", format_args!("{}", MicroUnit(v, "A")))?;
        }
        if let Some(v) = o.charge_current_limit_ua {
            write_field(f, "fcc", format_args!("{}", MicroUnit(v, "A")))?;
        }
        if let Some(v) = o.float_voltage_uv {
            write_field(f, "fv", format_args!("{}", MicroUnit(v, "V")))?;
        }
        if let Some(v) = o.charge_term_current_ua {
            write_field(f, "iterm", format_args!("{}", MicroUnit(v, "A")))?;
        }
        if let Some(v) = o.input_voltage_limit_uv {
            write_field(f, "vin", format_args!("{}", MicroUnit(v, "V")))?;
        }
        if let Some(v) = o.top_off_duration {
            let d = Duration::from_nanos(u64::try_from(v).unwrap_or_default());
            write_field(f, "topoff", format_args!("{d:?}"))?;
        }
        if sep.is_empty() { f.write_str("-") } else { Ok(()) }
    }
}

struct DisplayGovernors<'a>(&'a [spower::GovernorState]);

impl fmt::Display for DisplayGovernors<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return writeln!(f, "No charge governors configured.");
        }
        for g in self.0 {
            let lifecycle = g
                .lifecycle
                .map(|l| format!("{l:?}").to_ascii_lowercase())
                .unwrap_or_else(|| "?".to_owned());
            write!(
                f,
                "{} ({lifecycle}, {} restarts)\n  url: {}\n  sample interval: ",
                g.name.as_deref().unwrap_or("?"),
                g.restarts.unwrap_or(0),
                g.url.as_deref().unwrap_or("?"),
            )?;
            match g.sample_interval {
                Some(ns) => writeln!(
                    f,
                    "{:?}",
                    Duration::from_nanos(u64::try_from(ns).unwrap_or_default())
                )?,
                None => writeln!(f, "-")?,
            }
        }
        Ok(())
    }
}

struct DisplayChargeControl<'a>(&'a spower::ChargeControlState);

impl fmt::Display for DisplayChargeControl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "effective: {}", DisplayControlOptions(self.0.effective.as_ref()))?;
        if self.0.overrides.as_ref().is_some_and(|o| *o != fcharger::ControlOptions::default()) {
            writeln!(
                f,
                "override:  {}  (OVERRIDE ACTIVE)",
                DisplayControlOptions(self.0.overrides.as_ref())
            )?;
        } else {
            writeln!(f, "override:  -")?;
        }
        let governors = self.0.governors.as_deref().unwrap_or_default();
        if governors.is_empty() {
            return writeln!(f, "votes:     -");
        }
        writeln!(f, "votes:")?;
        for g in governors {
            write!(
                f,
                "  {}: {}",
                g.name.as_deref().unwrap_or("?"),
                DisplayControlOptions(g.votes.as_ref()),
            )?;
            if g.overrides.as_ref().is_some_and(|o| *o != fcharger::ControlOptions::default()) {
                writeln!(f, "  (OVERRIDDEN: {})", DisplayControlOptions(g.overrides.as_ref()))?;
            } else {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

/// Opens governor `name` through `debug`.
async fn open_governor(
    debug: &spower::GovernorDebugProxy,
    name: &str,
) -> Result<spower::GovernorProxy, Error> {
    let (governor, server) = fidl::endpoints::create_proxy::<spower::GovernorMarker>();
    debug
        .open_governor(name, server)
        .await?
        .map_err(|status| format_err!("{name}: {}", zx_status::Status::err_from_raw(status)))?;
    Ok(governor)
}

/// Parses and validates `args` into a `GovernorSubcommand`, or `None` if `--help` was printed.
fn parse_args(args: &[String]) -> Result<Option<GovernorSubcommand>, Error> {
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let parsed = match GovernorArgs::from_args(&["governor"], &strs) {
        Ok(parsed) => parsed,
        Err(early_exit) => {
            print!("{}", early_exit.output);
            early_exit.status.map_err(|()| format_err!("invalid governor arguments"))?;
            return Ok(None);
        }
    };
    let subcommand = parsed.command.unwrap_or(GovernorSubcommand::List(ListCommand {}));
    if let GovernorSubcommand::Override(cmd) = &subcommand
        && cmd.options() == fcharger::ControlOptions::default()
    {
        bail!("expected at least one field option (see 'governor override --help')");
    }
    Ok(Some(subcommand))
}

/// Executes `subcommand` against `debug`.
async fn execute(
    debug: &spower::GovernorDebugProxy,
    subcommand: GovernorSubcommand,
) -> Result<(), Error> {
    match subcommand {
        GovernorSubcommand::List(_) => {
            print!("{}", DisplayGovernors(&debug.get_state().await?.governors.unwrap_or_default()));
        }
        GovernorSubcommand::Control(_) => {
            print!("{}", DisplayChargeControl(&debug.get_state().await?));
        }
        GovernorSubcommand::Pause(PauseCommand { governor }) => {
            open_governor(debug, &governor).await?.pause().await?
        }
        GovernorSubcommand::Resume(ResumeCommand { governor }) => {
            open_governor(debug, &governor).await?.resume().await?
        }
        GovernorSubcommand::Override(cmd) => {
            let options = cmd.options();
            let result = match cmd.governor.as_deref() {
                None => debug.set_control(&options).await?,
                Some(name) => open_governor(debug, name).await?.set_control(&options).await?,
            };
            result.map_err(|e| format_err!("override failed: {e:?}"))?;
        }
        GovernorSubcommand::Clear(cmd) => {
            let mask = cmd.mask();
            let result = match cmd.governor.as_deref() {
                None => debug.clear_control(&mask).await?,
                Some(name) => open_governor(debug, name).await?.clear_control(&mask).await?,
            };
            result.map_err(|e| format_err!("clear failed: {e:?}"))?;
        }
    }
    Ok(())
}

/// Parses and runs a `governor` command against `fuchsia.power.battery.test.GovernorDebug`.
pub(crate) async fn run(args: &[String]) -> Result<(), Error> {
    let Some(subcommand) = parse_args(args)? else {
        return Ok(());
    };
    let debug = connect_to_protocol::<spower::GovernorDebugMarker>()?;
    execute(&debug, subcommand).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;
    use futures::StreamExt as _;

    fn parse_subcommand(line: &str) -> Result<GovernorArgs, argh::EarlyExit> {
        let words: Vec<&str> = line.split_whitespace().collect();
        GovernorArgs::from_args(&["governor"], &words)
    }

    fn run_args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    /// Verifies that `CurrentArg`, `VoltageArg`, and `DurationArg` parse with and without units,
    /// and reject fractional bare or micro-unit numbers.
    #[test]
    fn test_parse_units() {
        assert_eq!("1.5A".parse::<CurrentArg>().unwrap(), CurrentArg(1_500_000));
        assert_eq!("1500mA".parse::<CurrentArg>().unwrap(), CurrentArg(1_500_000));
        assert_eq!("1500000uA".parse::<CurrentArg>().unwrap(), CurrentArg(1_500_000));
        assert_eq!("1500000".parse::<CurrentArg>().unwrap(), CurrentArg(1_500_000));
        assert_eq!("4.35V".parse::<VoltageArg>().unwrap(), VoltageArg(4_350_000));
        assert_eq!("4350mV".parse::<VoltageArg>().unwrap(), VoltageArg(4_350_000));
        assert!("4.35".parse::<VoltageArg>().is_err());
        assert!("0.2".parse::<CurrentArg>().is_err());
        assert!("1.5uA".parse::<CurrentArg>().is_err());
        assert!("-1A".parse::<CurrentArg>().is_err());
        assert!("fast".parse::<CurrentArg>().is_err());
        assert_eq!(
            "30s".parse::<DurationArg>().unwrap(),
            DurationArg(Duration::from_secs(30).as_nanos() as i64)
        );
        assert_eq!(
            "10min".parse::<DurationArg>().unwrap(),
            DurationArg(Duration::from_secs(600).as_nanos() as i64)
        );
        assert_eq!(
            "600".parse::<DurationArg>().unwrap(),
            DurationArg(Duration::from_secs(600).as_nanos() as i64)
        );
        assert!("-30s".parse::<DurationArg>().is_err());
        assert!(format!("{}min", u64::MAX).parse::<DurationArg>().is_err());
        assert!("soon".parse::<DurationArg>().is_err());
    }

    /// Verifies that `GovernorArgs` parses `list`, `control`, `pause`, `resume`, `override`,
    /// `clear`, and `--help`.
    #[test]
    fn test_parse_governor_command() {
        assert_eq!(parse_subcommand("").unwrap(), GovernorArgs { command: None });
        assert_eq!(
            parse_subcommand("list").unwrap(),
            GovernorArgs { command: Some(GovernorSubcommand::List(ListCommand {})) }
        );
        assert_eq!(
            parse_subcommand("control").unwrap(),
            GovernorArgs { command: Some(GovernorSubcommand::Control(ControlCommand {})) }
        );
        assert_eq!(
            parse_subcommand("pause msc").unwrap(),
            GovernorArgs {
                command: Some(GovernorSubcommand::Pause(PauseCommand {
                    governor: "msc".to_string()
                }))
            }
        );
        assert_eq!(
            parse_subcommand("resume msc").unwrap(),
            GovernorArgs {
                command: Some(GovernorSubcommand::Resume(ResumeCommand {
                    governor: "msc".to_string()
                }))
            }
        );
        assert!(parse_subcommand("--help").unwrap_err().status.is_ok());
        assert!(parse_subcommand("override --help").unwrap_err().status.is_ok());
        assert!(parse_subcommand("clear --help").unwrap_err().status.is_ok());
        assert!(parse_subcommand("list extra").unwrap_err().status.is_err());
        assert!(parse_subcommand("control extra").unwrap_err().status.is_err());
        assert!(parse_subcommand("pause").unwrap_err().status.is_err());
        assert!(parse_subcommand("resume a b").unwrap_err().status.is_err());
        assert!(parse_subcommand("unknown").unwrap_err().status.is_err());

        let Some(GovernorSubcommand::Override(cmd)) =
            parse_subcommand("override --fcc 1A --fv 4.2V --mode passthrough").unwrap().command
        else {
            panic!("expected Override");
        };
        assert_eq!(cmd.governor, None);
        assert_eq!(
            cmd.options(),
            fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Passthrough),
                charge_current_limit_ua: Some(1_000_000),
                float_voltage_uv: Some(4_200_000),
                ..Default::default()
            }
        );

        let Some(GovernorSubcommand::Override(cmd)) =
            parse_subcommand("override msc --fcc 500mA").unwrap().command
        else {
            panic!("expected Override");
        };
        assert_eq!(cmd.governor.as_deref(), Some("msc"));
        assert_eq!(
            cmd.options(),
            fcharger::ControlOptions {
                charge_current_limit_ua: Some(500_000),
                ..Default::default()
            }
        );

        let Some(GovernorSubcommand::Clear(cmd)) = parse_subcommand("clear --fcc").unwrap().command
        else {
            panic!("expected Clear");
        };
        assert_eq!(cmd.governor, None);
        assert_eq!(
            cmd.mask(),
            fcharger::ControlOptions { charge_current_limit_ua: Some(0), ..Default::default() }
        );

        let Some(GovernorSubcommand::Clear(cmd)) = parse_subcommand("clear msc").unwrap().command
        else {
            panic!("expected Clear");
        };
        assert_eq!(cmd.governor.as_deref(), Some("msc"));
        assert_eq!(cmd.mask(), fcharger::ControlOptions::default());

        assert!(parse_subcommand("override --speed 1A").unwrap_err().status.is_err());
        assert!(parse_subcommand("override --mode otg").unwrap_err().status.is_err());
        assert!(parse_subcommand("clear --speed").unwrap_err().status.is_err());
    }

    /// Verifies that `DisplayChargeControl` lists the effective values, flags the overrides
    /// of every governor's votes, and lists each governor's votes, flagging its overrides.
    #[test]
    fn test_display_charge_control() {
        let fcc = |ua| fcharger::ControlOptions {
            charge_current_limit_ua: Some(ua),
            ..Default::default()
        };
        let governor = |name: &str, votes, overrides| spower::GovernorState {
            name: Some(name.to_string()),
            votes: Some(votes),
            overrides: Some(overrides),
            ..Default::default()
        };
        let state = spower::ChargeControlState {
            governors: Some(vec![
                governor("msc", fcc(2_000_000), fcc(500_000)),
                governor("thermal", fcc(1_500_000), fcharger::ControlOptions::default()),
            ]),
            overrides: Some(fcc(1_000_000)),
            effective: Some(fcc(1_000_000)),
            ..Default::default()
        };
        assert_eq!(
            DisplayChargeControl(&state).to_string(),
            "effective: fcc=1.000 A\noverride:  fcc=1.000 A  (OVERRIDE ACTIVE)\nvotes:\n  \
             msc: fcc=2.000 A  (OVERRIDDEN: fcc=0.500 A)\n  thermal: fcc=1.500 A\n"
        );

        assert_eq!(
            DisplayChargeControl(&spower::ChargeControlState::default()).to_string(),
            "effective: -\noverride:  -\nvotes:     -\n"
        );
    }

    /// Verifies that `DisplayGovernors` formats configured governors, missing fields, and empty
    /// lists.
    #[test]
    fn test_display_governors() {
        assert_eq!(DisplayGovernors(&[]).to_string(), "No charge governors configured.\n");
        let report = DisplayGovernors(&[spower::GovernorState {
            name: Some("msc".to_string()),
            url: Some("fuchsia-pkg://fuchsia.com/msc#meta/msc.cm".to_string()),
            lifecycle: Some(spower::GovernorLifecycle::Running),
            restarts: Some(2),
            sample_interval: Some(Duration::from_secs(30).as_nanos() as i64),
            ..Default::default()
        }])
        .to_string();
        assert_eq!(
            report,
            "msc (running, 2 restarts)\n  url: fuchsia-pkg://fuchsia.com/msc#meta/msc.cm\n  \
             sample interval: 30s\n"
        );

        let report = DisplayGovernors(&[spower::GovernorState {
            name: Some("msc".to_string()),
            lifecycle: Some(spower::GovernorLifecycle::Paused),
            ..Default::default()
        }])
        .to_string();
        assert!(report.starts_with("msc (paused, 0 restarts)\n"), "{report}");
        assert!(report.ends_with("  sample interval: -\n"), "{report}");

        let report = DisplayGovernors(&[spower::GovernorState::default()]).to_string();
        assert!(report.starts_with("? (?, 0 restarts)\n"), "{report}");
    }

    /// Verifies that `parse_args` rejects `override` without any field flags and returns `Ok(None)`
    /// for `--help`, and that `execute` routes aggregate `override`/`clear` to `GovernorDebug` and
    /// per-governor `override`/`clear`/`pause`/`resume` through `OpenGovernor`, propagating
    /// `NOT_FOUND` and charger errors.
    #[fuchsia::test]
    async fn test_execute_dispatches_aggregate_and_per_governor_commands() {
        assert_eq!(parse_args(&run_args("--help")).unwrap(), None);
        assert!(parse_args(&run_args("override")).is_err());
        assert!(parse_args(&run_args("override msc")).is_err());

        let (debug, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<spower::GovernorDebugMarker>();
        let fcc_high = fcharger::ControlOptions {
            charge_current_limit_ua: Some(1_500_000),
            ..Default::default()
        };
        let fcc_low = fcharger::ControlOptions {
            charge_current_limit_ua: Some(500_000),
            ..Default::default()
        };
        let fcc_mask =
            fcharger::ControlOptions { charge_current_limit_ua: Some(0), ..Default::default() };

        let server = fasync::Task::local({
            let fcc_high = fcc_high.clone();
            let fcc_low = fcc_low.clone();
            let fcc_mask = fcc_mask.clone();
            async move {
                // 1. Aggregate override.
                match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::SetControl { options, responder } => {
                        assert_eq!(options, fcc_high);
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                // 2. Aggregate clear.
                match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::ClearControl { options, responder } => {
                        assert_eq!(options, fcc_mask);
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                // 3. Per-governor override on `msc`.
                let mut gov_stream = match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::OpenGovernor { name, governor, responder } => {
                        assert_eq!(name, "msc");
                        responder.send(Ok(())).unwrap();
                        governor.into_stream()
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                match gov_stream.next().await.unwrap().unwrap() {
                    spower::GovernorRequest::SetControl { options, responder } => {
                        assert_eq!(options, fcc_low);
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("unexpected governor request: {other:?}"),
                }
                // 4. Per-governor pause on `msc`.
                let mut gov_stream = match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::OpenGovernor { name, governor, responder } => {
                        assert_eq!(name, "msc");
                        responder.send(Ok(())).unwrap();
                        governor.into_stream()
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                match gov_stream.next().await.unwrap().unwrap() {
                    spower::GovernorRequest::Pause { responder } => {
                        responder.send().unwrap();
                    }
                    other => panic!("unexpected governor request: {other:?}"),
                }
                // 5. Unknown governor returns `NOT_FOUND`.
                match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::OpenGovernor { name, responder, .. } => {
                        assert_eq!(name, "missing");
                        responder.send(Err(zx_status::Status::NOT_FOUND.into_raw())).unwrap();
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                // 6. Rejected override surfaces error.
                match debug_stream.next().await.unwrap().unwrap() {
                    spower::GovernorDebugRequest::SetControl { responder, .. } => {
                        responder.send(Err(fcharger::Error::InvalidArgs)).unwrap();
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        });

        let exec_cmd = |line: &str| {
            let sub = parse_args(&run_args(line)).unwrap().unwrap();
            execute(&debug, sub)
        };
        exec_cmd("override --fcc 1.5A").await.unwrap();
        exec_cmd("clear --fcc").await.unwrap();
        exec_cmd("override msc --fcc 500mA").await.unwrap();
        exec_cmd("pause msc").await.unwrap();
        assert!(exec_cmd("resume missing").await.unwrap_err().to_string().contains("NOT_FOUND"));
        assert!(
            exec_cmd("override --fcc 1.5A").await.unwrap_err().to_string().contains("InvalidArgs")
        );
        server.await;
    }
}
