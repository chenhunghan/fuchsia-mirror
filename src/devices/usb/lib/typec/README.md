# `typec`: USB Type-C CC and Port State Machine

`//src/devices/usb/lib/typec` implements a hardware-agnostic USB Type-C Configuration Channel (CC)
decoder and non-PD port connection state machine per the **USB Type-C Cable and Connector
Specification (R2.5, §4.5.2, Table 4-34, Appendix B)**.

## State Machine

States are named as in USB Type-C R2.5 §4.5.2.2 (`TypecState`, and Inspect `typec_state`).

```text
                     +------------------------+
                     |     Unattached.SNK     |
                     +------------------------+
                       |  ^              |  ^
      partner Rp       |  | SNK.Open     |  | SRC.Open
      detected         v  | (tPDDebounce)v  |       partner Rd (or Ra/Ra) detected
            +-----------------+      +-----------------+
            | AttachWait.SNK  |      | AttachWait.SRC  |
            +-----------------+      +-----------------+
                     |                        |
         tCCDebounce |            tCCDebounce | (+ vSafe0V)
    (+ VBUS present) v                        v
  +---------------------------+  +-----------------+
  | Attached.SNK /            |  |  Attached.SRC   |
  | DebugAccessory.SNK (Rp/Rp)|  |  (Role: Host)   |
  +---------------------------+  +-----------------+
                     |                        |
    VBUS loss        |                        | SRC.Open (tSRCDisconnect)
    (vSinkDisconnect)|                        |
                     +-----------+------------+
                                 v
                           Unattached.SNK
```

The state machine lives in `Machine::transition` (`src/state.rs`), matching on its private phase
(`Unattached`, `AttachWait`, `AttachedSnk`, `AttachedSrc`) and incoming `Event`. Each transition
returns a runner-level `Step` (`Absorbed`, or `Advanced(Option<Effect>)` with `EnterUnattached`,
`EnterAttachWait`, `EnterAttached`, or `NotifySinkRp`), while `Machine::debounce_timer` reports the
active debounce window. The worker in `src/runner.rs` (`execute_step`) dispatches driver callbacks,
arms debounce timers, and holds partners whose attach failed.

## Driver Integration

Embed a `TypecPort` inside your `Arc`-managed driver struct and implement `PortController` (the
hardware operations) and `Typec` (the port's callbacks); import `TypecExt` when calling state
queries or Inspect helpers.

```rust
use anyhow::Result;
use typec::{CcStatus, PortController, PowerRole, RpValue, Typec, TypecPort};

pub struct MyTcpc {
    typec: TypecPort,
    hw: MyController,
}

impl PortController for MyTcpc {
    async fn read_cc_status(&self) -> Result<CcStatus> {
        // Decode the controller's CC comparators against the terminations it presents.
        self.hw.read_cc().await
    }

    async fn is_vbus_safe0v(&self) -> Result<bool> {
        // Qualifies AttachWait.SRC -> Attached.SRC and ends VBUS discharge.
        self.hw.vbus_below_0v8().await
    }

    async fn is_vbus_present(&self) -> Result<bool> {
        // Qualifies AttachWait.SNK -> Attached.SNK and detects vSinkDisconnect.
        self.hw.vbus_present().await
    }

    async fn write_vbus_discharge(&self, armed: bool) -> Result<bool> {
        // Optional: returns whether hardware discharge is now armed (Ok(false) if unsupported;
        // if already armed on edge-triggered hardware, clear then set for a 0 -> 1 edge).
        self.hw.set_discharge(armed).await
    }

    async fn write_vbus_sink(&self, enable: bool) -> Result<bool> {
        // Optional: returns whether the sink path is now open (Ok(false) if managed externally);
        // TypecPort opens it on Attached.SNK and closes it on detach or AttachWait.SRC.
        self.hw.set_vbus_sink(enable).await
    }
}

impl Typec for MyTcpc {
    fn typec(&self) -> &TypecPort {
        &self.typec
    }

    async fn on_unattached(&self) {
        // Re-arm connection detection if the controller requires it.
    }

    async fn on_partner_attached(
        &self,
        _role: PowerRole,
        _wake_lease: Option<&zx::EventPair>,
    ) -> Result<()> {
        // Enable Sink charging (reading initial `self.sink_rp_level()`) / USB device switch, or
        // source VBUS and enter Source host mode.
        Ok(())
    }

    async fn on_sink_rp_changed(&self, _rp: RpValue, _wake_lease: Option<&zx::EventPair>) {
        // Optional: a Source changed its Rp advertisement after attach (Default / 1.5A / 3.0A).
    }

    async fn on_partner_detached(&self, _wake_lease: Option<&zx::EventPair>) {
        // Stop sourcing VBUS and tear down the USB data path.
    }
}
```

| Lifecycle Event | `TypecPort` / `TypecExt` Call |
| :--- | :--- |
| **Construction** | `TypecPort::new(scope)` with optional `.with_sink_debug_accessory_mode(true)` |
| **Initial Sync / Cold Boot** | `self.sync_initial_state(lease).await?`. A bootloader-attached Source partner commits `Attached.SNK` immediately if VBUS is present (otherwise debouncing `tCCDebounce`); a Sink partner still runs `tCCDebounce` (R2.5 §4.5.2.2.8.2); with no partner `on_unattached()` runs |
| **CC change IRQ** (`ALERT.CCStatus`) | Clear the interrupt, then call `self.handle_cc_status(lease).await?`. Sink `Rp` level changes report `Typec::on_sink_rp_changed` after `tRpValueChange` (15ms default, R2.5 §4.5.2.3); `RpValue::typec_current_ua` returns the advertised current (R2.5 Table 4-20, Table B-2) |
| **VBUS change IRQ** (`ALERT.ExtendedStatus` / `PowerStatus`) | Clear the interrupt, then call `self.handle_vbus_change(lease).await?` (qualifies Source `vSafe0V` and Sink VBUS-present attaches, detaches `Attached.SNK` / `DebugAccessory.SNK` on VBUS loss, and debounces `vSafe0V` to disarm discharge and call `Typec::on_vsafe0v` in `Unattached`) |
| **Partner Attach** | `Typec::on_partner_attached(role, lease)`: returning `Err` holds the partner in `AttachWait` until its CC terminations change (including across VBUS power cycles while `Rp` stays asserted) without calling `on_partner_detached` |
| **State Queries & Diagnostics** | `typec_state()`, `is_connected()`, `attached_power_role()`, `orientation()`, `accessory()`, `sink_rp_level()`, `cc_status()`, `is_vbus_discharge_armed()`, `is_disabled()`, `sample_attachable_role()`, `is_busy()`, `active_debounce_direction()`, and `record_typec_inspect(&node)`. `TypecState` is `#[non_exhaustive]`; use its query methods rather than matching variants |
| **Unit Tests** | Drive timing boundaries with `fuchsia_async::TestExecutor::new_with_fake_time()`, or configure zero debounce durations and `zx::BootDuration::INFINITE` on `with_discharge_timeout` and synchronize via `wait_for_debounce()` |

### Power Path and Discharge Details

- **Sink VBUS:** `TypecPort` opens `write_vbus_sink(true)` before `on_partner_attached(Sink)` (TCPCI
  Figure 4-24) and closes it on detach, `AttachWait.SRC`, and `Unattached`, requiring it closed
  before `Attached.SRC`. Keep a hardware input-current ceiling `<= 500 mA` (Default USB Power, R2.5
  Table 4-20) until `on_partner_attached` applies the committed `Rp` limit. Because
  `DisableSinkVbus` also discharges VBUS (TCPCI Table 4-27), `TypecPort` skips closing across
  `AttachWait.SNK`, held Sink attaches, and non-detach `Unattached` entries on live external VBUS,
  closing a held Sink or `Unattached` sink path once VBUS drops.
- **VBUS discharge:**
  - **Arming:** `TypecPort` arms `write_vbus_discharge(true)` when residual VBUS is above `vSafe0V`
    before `on_unattached` and on entering `AttachWait.SRC` (R2.5 §4.5.2.2.8.1; `on_unattached` can
    inspect `is_vbus_discharge_armed()`). Non-detach `Unattached` entries on live external VBUS skip
    arming unless discharge is already confirmed armed.
  - **Disarming:** Discharge stops on `AttachWait.SNK`, attach (blocking `Attached.SRC` if disarm
    fails, while warning and continuing on `Attached.SNK` and retrying on later CC/VBUS alerts),
    live external VBUS arriving in `Unattached` after VBUS was below `vVBUSPresent` (or after a
    failed disarm), or debounced `vSafe0V`, calling informational `Typec::on_vsafe0v` when still in
    `Unattached`.
  - **Timeout:** If `vSafe0V` does not arrive within 650ms (`with_discharge_timeout`, matching
    `tVBUSOFF` and `tVsafe0V` in R2.5 Table 4-32; `zx::BootDuration::INFINITE` disables it),
    discharge stops without calling `on_vsafe0v`. The timer holds a wake lease without keeping
    `is_busy()` / `wait_for_debounce()` active.

### Wake Leases

Every `TypecExt` command taking `wake_lease: Option<zx::EventPair>` (`handle_cc_status`,
`handle_vbus_change`, `sync_initial_state`) holds that lease for the entire actor turn, passes it
as `Option<&zx::EventPair>` to `Typec` callbacks run on that turn, and duplicates the handle into
any debounce, `vSafe0V`, or initial `discharge_timeout` timer armed during the turn. When a timer
expires, its actor turn runs under the timer's stored lease and releases it when the turn finishes
(a single `discharge_timeout` disarm-retry window and `vSafe0V` bounce re-arms run without holding
a new turn lease). Disarming VBUS discharge forwards any cancelled `vSafe0V` or
`discharge_timeout` timer lease into `Typec::on_vsafe0v`.

### Inspect Properties

| Property | Type | Meaning |
| :--- | :--- | :--- |
| `is_connected` | bool | A partner is attached (`Attached.SNK`, `DebugAccessory.SNK`, or `Attached.SRC`) |
| `last_updated_time_ns` | int | Boot time of the last `typec_state` change |
| `cc1_state` / `cc2_state` | string | CC line state as TCPCI names it (e.g. `SNK.Power1.5`, `SRC.Rd`); `Open` while disabled |
| `typec_state` | string | Connection state as R2.5 names it (e.g. `AttachWait.SNK`) |
| `active_debounce_direction` | string | `attach`, `disconnect`, or `none` (including during `rp_value_change`) |
| `attach_count` | uint | Committed attaches |
| `disconnect_count` | uint | Confirmed detaches from an attached state |
| `debounce_cancelled_count` | uint | Attach or disconnect debounces cancelled or superseded before committing |
| `sink_rp_level` | string | `Default`, `1.5A`, or `3.0A`, as the attached Source advertises; absent unless attached as a Sink |
| `vbus_discharge_armed` | bool | VBUS discharge is armed, or a disarm/re-arm write through `PortController::write_vbus_discharge` failed while armed |
| `vbus_sink_enabled` | bool | Controller-managed VBUS sink path is open, or closing through `PortController::write_vbus_sink` failed (`false` while `Unknown`, confirmed closed, or externally managed) |
| `pending_timer` | string | Name of the sleeping timer (e.g. `cc_attach`, `snk_exit`, `disconnect`, `rp_value_change`, `vsafe0v`, `discharge_timeout`), or `none` |
| `rp_change_count` | uint | Sink `Rp` level changes reported through `Typec::on_sink_rp_changed` |
| `sink_attach_failure_count` | uint | Sink attaches whose sink path or `on_partner_attached` failed, holding the partner; not counted in `attach_count` |
| `source_attach_failure_count` | uint | Source attaches whose discharge disarm, sink path close, or `on_partner_attached` failed, holding the partner; not counted in `attach_count` |
| `discharge_timeout_count` | uint | VBUS discharges stopped after the discharge timeout without reaching `vSafe0V` |

## Spec Notes

- **Sink disconnect:** `Attached.SNK` and `DebugAccessory.SNK` detach only on VBUS loss
  (`vSinkDisconnect`, R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2). Without USB PD communication, CC reading
  `SNK.Open` while VBUS stays present (e.g. ground offset pulling Default `Rp` below `vRd-USB` under
  load) does not detach a Sink.
- **Sink `Rp` changes:** an `Rp` rise is reported only after every sample matches for
  `tRpValueChange` (15ms default within 10ms-20ms, R2.5 §4.5.2.3, Table 4-34), so one noisy
  `CC_STATUS` sample never raises the reported level. An attach-time drop commits the lower level
  immediately; if `SNK.Open` noise at expiry or two consecutive expiry read errors cancel a rise
  without a later `CC_STATUS` edge, the Sink keeps the lower committed level. An `Rp` fall cancels
  if CC returns `>=` the committed level (filtering short non-idle USB PD BMC dips, Table 4-34,
  §4.5.2.3.2.2, §4.5.2.3.3.2), while keeping its `tRpValueChange` window running across `SNK.Open`
  ground-offset chatter and lower-level steps so debouncing adds at most `tRpValueChange` (15ms),
  leaving margin within the 60ms `tSinkAdj` budget (Table 4-32) provided `Typec` callbacks complete
  promptly.
- **Not yet supported:** Source-only `Unattached.SRC`, VCONN (R2.5 §4.5.2.2.9.1), and
  `UnorientedDebugAccessory.SRC`; all tracked in TODO(https://fxbug.dev/575661321).
  `SRC.Ra/SRC.Ra` enters `AttachWait.SRC` and never attaches, as in R2.5.

## Testing

```bash
fx test typec-tests
```
