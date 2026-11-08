# Battery Simulator

Battery Simulator is a simple command line interface for developers to use in testing behavior of their code with respect to different battery states independent (or in the absence) of a physical battery.


# Usage

Build and update your project on the target device (note that the product configuration must include //src/power:battery). Add the following to the
developer overrides:

  shell_commands = [{
      package = "battery-cli"
      components = [ "battery_cli" ]
  }]

  base_packages = [
      "//src/power/battery-manager/battery-cli",
  ]

 Flash to device or fx ota. Then run **fx shell** to enter the shell. To run the simulator, type **battery_cli**.

## Single commands

Pass any command as arguments to run it once without starting the REPL (e.g. `battery_cli governor list`).

## Charge governors

`governor` subcommands use `fuchsia.power.battery.test.GovernorDebug` to inspect and control charge governors in `battery-manager`:

| Subcommand | Description |
| :--- | :--- |
| `governor [list]` | List each charge governor with its lifecycle, restarts, and sample interval. |
| `governor control` | Show effective charge control values, aggregate overrides, and per-governor votes. |
| `governor pause <governor>` | Stop `<governor>` and withdraw its votes until resumed. |
| `governor resume <governor>` | Restart a paused `<governor>`. |
| `governor override [<governor>] --<field> <value> ...` | Override fields (`--mode`, `--icl`, `--fcc`, `--fv`, `--iterm`, `--vin`, `--topoff`) across all governors, or for `<governor>` alone. |
| `governor clear [<governor>] [--<field> ...]` | Clear the specified field overrides (or all overrides if no flags are given). |

## Disconnect

Disconnects the BatteryManager from the physical battery, providing clients with simulated BatteryInfo data.
> battman> disconnect

## Reconnect

Reconnects the BatteryManager to the physical battery, providing clients with read BatteryInfo data.
> battman> reconnect

## Get

To get the current BatteryInfo, run
> battman> get

## Set

Modifies the state of the simulated battery. Must call 'disconnect' prior to 'set'. Arguments of the commands are separated by spaces.
> battman> set battery_attribute1 battery_attribute_value1 ... battery_attributeN battery_attribute_valueN

Where attribute is any of the following:

- BatteryPercentage
		- any int value ranging [1..100]
- ChargeSource
		- UNKNOWN
		- NONE
		- AC_ADAPTER
		- USB
		- WIRELESS
- LevelStatus
		- UNKNOWN
		- OK
		- WARNING
		- LOW
		- CRITICAL
- ChargeStatus
		- UNKNOWN
		- NOT_CHARGING
		- CHARGING
		- DISCHARGING
- BatteryStatus
		- UNKNOWN
		- OK
		- NOT_AVAILABLE
		- NOT_PRESENT
- TimeRemaining
        - Time represented in seconds

Some example commands
> battman> set BatteryPercentage 95
> battman> set LevelStatus LOW ChargeSource WIRELESS BatteryPercentage 12

