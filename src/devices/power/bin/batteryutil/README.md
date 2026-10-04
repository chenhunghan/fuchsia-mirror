# batteryutil

`batteryutil` is a command-line diagnostic and control utility for battery fuel gauges and power
path chargers on Fuchsia.

It connects to `fuchsia.hardware.power.battery.Service` (Fuel Gauge data plane),
`fuchsia.hardware.power.charger.Service` (charger telemetry),
`fuchsia.hardware.power.charger.DebugService` (charger operating mode control),
and `fuchsia.power.battery` services.

## Specifying Paths & Multiple Devices

By default, `batteryutil` automatically discovers available battery and charger service instances
under `/svc/`. If multiple instances are present:
- `get` queries and reports telemetry for all discovered instances.
- `watch` multiplexes real-time streaming updates across all discovered instances.
- `mode` selects an available charger instance, prompting or warning if multiple are present.

To target a specific battery or charger instance directly, pass the `-p` / `--path` option before
the command:

```bash
# Query a specific battery service instance
$ batteryutil -p /svc/fuchsia.hardware.power.battery.Service/default get

# Watch a specific fuel gauge instance
$ batteryutil -p /svc/fuchsia.hardware.power.battery.Service/default watch

# Set operating mode on a specific charger instance
$ batteryutil -p /svc/fuchsia.hardware.power.charger.DebugService/default mode charging
```

## Commands

### 1. Telemetry Inspection (`get`)
Query real-time battery hardware status (SOC, voltage, current, temp, cycles).

```console
$ batteryutil get
Model: MAX77779
Chemistry: Li-Ion
Design Capacity: 4.947 Ah
Design Voltage: 3.850 V
Supported Triggers: level_percent, charge_status, cycle_count
Supported Wake Triggers: None
Present: true
Charge Status: Charging
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)
```

### 2. Real-Time Event Streaming (`watch`)
Stream state transitions and telemetry changes via hanging-get without polling.

```console
$ batteryutil watch
Watching battery and charger events on all instances (press Ctrl+C to exit)...

=== Battery Telemetry Update (/svc/fuchsia.hardware.power.battery.Service/default) ===
Present: true
Charge Status: Charging
Level: 26.1%
Remaining Capacity: 1.295 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.5 C
Voltage: 3.897 V
Current: 1.251 A
Cycle Count: 2
Time Remaining: 20m 44s (1244.6s)

=== Battery Telemetry Update (/svc/fuchsia.hardware.power.battery.Service/default) ===
Present: true
Charge Status: Charging
Level: 27.0%
Remaining Capacity: 1.335 Ah
Full Charge Capacity: 4.947 Ah
Temperature: 30.6 C
Voltage: 3.912 V
Current: 1.248 A
Cycle Count: 2
Time Remaining: 20m 05s (1205.2s)
```

### 3. Charger Operating Mode & Power Source Control (`mode`)
Set the charger operating mode via `fuchsia.hardware.power.charger.DebugService` (falling back to
Sorrel `SPMI 0x2954` + legacy `fuchsia.power.battery.ChargerService`). Supported modes:
- `charging` / `usb`: `OperatingMode::Charging` (USB powers system + charges battery)
- `passthrough`: `OperatingMode::Passthrough` (USB powers system, charging inhibited)
- `discharging` / `battery`: `OperatingMode::Discharging` (active discharging from battery)
- `otg`: `OperatingMode::Otg` (reverse boost)
- `auto`: clears all `DebugService` overrides (`Debug.ClearOverrides`)

Modes set through `DebugService` are sticky overrides: they stay in effect after `batteryutil`
exits and take precedence over the production policy client connected to `Controller` until they
are cleared with `batteryutil mode auto`.

```console
$ batteryutil mode charging
Successfully set charger operating mode to Charging via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil mode passthrough
Successfully set charger operating mode to Passthrough via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil mode discharging
Successfully set charger operating mode to Discharging via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)

$ batteryutil mode auto
Successfully cleared charger overrides via fuchsia.hardware.power.charger.Debug (/svc/fuchsia.hardware.power.charger.DebugService/default/debug)
```


