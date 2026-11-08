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

The state machine lives in `Machine::transition` (`src/state.rs`), matching on the current spec
state (`Unattached`, `AttachWait`, `Attached`) and incoming `Event`. Each transition updates
`Phase` and returns a `Transition` (`Absorbed` or `Moved`), while `Phase::debounce_timer` reports
the debounce timer active in the resulting phase.

## Driver Integration

Embed a `TypecPort` inside your `Arc`-managed driver struct and implement `PortController` (the
hardware operations) and `Typec` (the port's callbacks); import `TypecExt` when calling state
queries or Inspect helpers.

```rust
use anyhow::Result;
use typec::{CcStatus, PortController, PowerRole, Typec, TypecPort};

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
        // Qualifies AttachWait.SRC -> Attached.SRC.
        self.hw.vbus_below_0v8().await
    }

    async fn is_vbus_present(&self) -> Result<bool> {
        // Qualifies AttachWait.SNK -> Attached.SNK and detects vSinkDisconnect.
        self.hw.vbus_present().await
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
        // Enable Sink charging / USB device switch, or source VBUS and enter Source host mode.
        Ok(())
    }

    async fn on_partner_detached(&self, _wake_lease: Option<&zx::EventPair>) {
        // Stop sourcing VBUS and tear down the USB data path.
    }
}
```

| Lifecycle Event | `TypecPort` / `TypecExt` Call |
| :--- | :--- |
| **Construction** | `TypecPort::new(scope)` with optional `.with_sink_debug_accessory_mode(true)` |
| **State Queries & Diagnostics** | `typec_state()`, `is_connected()`, `attached_power_role()`, `orientation()`, `accessory()`, `sink_rp_level()`, `cc_status()`, `is_disabled()`, `sample_attachable_role()`, `is_busy()`, `active_debounce_direction()`, and `record_typec_inspect(&node)`. `TypecState` is `#[non_exhaustive]`; use its query methods rather than matching variants |

### Inspect Properties

| Property | Type | Meaning |
| :--- | :--- | :--- |
| `is_connected` | bool | A partner is attached (`Attached.SNK`, `DebugAccessory.SNK`, or `Attached.SRC`) |
| `last_updated_time_ns` | int | Boot time of the last `typec_state` change |
| `cc1_state` / `cc2_state` | string | CC line state as TCPCI names it (e.g. `SNK.Power1.5`, `SRC.Rd`); `Open` while disabled |
| `typec_state` | string | Connection state as R2.5 names it (e.g. `AttachWait.SNK`) |
| `active_debounce_direction` | string | `attach`, `disconnect`, or `none` |
| `attach_count` | uint | Committed attaches |
| `disconnect_count` | uint | Confirmed detaches from an attached state |
| `debounce_cancelled_count` | uint | Attach or disconnect debounces cancelled or superseded before committing |
| `sink_rp_level` | string | `Default`, `1.5A`, or `3.0A` advertised by the Source when it attached; absent unless attached as a Sink |

## Spec Notes

- **Sink disconnect:** `Attached.SNK` and `DebugAccessory.SNK` detach only on VBUS loss
  (`vSinkDisconnect`, R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2). Without USB PD communication, CC reading
  `SNK.Open` while VBUS stays present (e.g. ground offset pulling Default `Rp` below `vRd-USB` under
  load) does not detach a Sink.
- **Not yet supported:** Source-only `Unattached.SRC`, `tRpValueChange` dynamic `Rp` updates, VCONN
  (R2.5 §4.5.2.2.9.1), and `UnorientedDebugAccessory.SRC`; all tracked in
  TODO(https://fxbug.dev/575661321). `SRC.Ra/SRC.Ra` enters `AttachWait.SRC` and never attaches, as
  in R2.5.

## Testing

```bash
fx test typec-tests
```
