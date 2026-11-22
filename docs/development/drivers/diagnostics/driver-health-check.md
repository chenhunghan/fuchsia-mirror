# Driver health checks

Health checks let the driver framework ask a running driver whether it is
working, and let a driver report a fault as soon as it notices one. This guide
explains what a driver should report and how to implement health checks in C++
and Rust.

## Why health checks {#why}

Active health checks improve visibility of driver status across start, bind, and
stop of a driver. Without these, there is little understanding if a driver is
running, but non-functional, for example if the driver has stopped serving some
or all of its clients.

## How it works {#how-it-works}

* Driver manager, ffx, or a test sends a health check to a driver at one of
  three levels: PING, FAST, or DEEP.
* The driver host enforces a deadline for each level. If the driver does not
  answer in time, the check fails with a timeout, which usually means the
  driver's dispatcher is stuck.
* A driver can also push an alert when it detects a fault, without waiting to
  be asked.
* Driver manager records every result in its own Inspect tree and logs a line
  when a driver's health changes. Serious faults trigger a snapshot.

The FIDL definitions are in
[driver.fidl](/sdk/fidl/fuchsia.driver.framework/driver.fidl) and
[health.fidl](/sdk/fidl/fuchsia.driver.framework/health.fidl).

## Check levels {#check-levels}

### PING {#ping}

* **Purpose:** Confirm that the driver's dispatcher is responsive and its
  in-memory software state is consistent.
* **Time limit:** Aim for under 1 ms (250 ms deadline enforced by the driver
  host).
* **Example checks to do:** Verify internal state machine invariants and merge
  any active faults from `HealthAlertLatch`.
* **Example checks to avoid:** MMIO reads, bus transfers (I2C, SPI, USB, PCI),
  blocking locks, or waking suspended hardware.
* **Default behavior:** Returns `OK`.

### FAST {#fast}

* **Purpose:** Verify that the underlying hardware and data path are responsive
  without disrupting active operations.
* **Time limit:** Aim for under 25 ms (2-second deadline enforced by the driver
  host).
* **Example checks to do:** Read a safe status or identity register over MMIO or
  I2C, or verify that an interrupt or output pipeline is making progress.
* **Example checks to avoid:** Clearing read-on-clear registers, disturbing
  active DMA, mutating hardware state, or waking hardware from runtime idle.
* **Default behavior:** Returns `NOT_SUPPORTED`.

### DEEP {#deep}

* **Purpose:** Run an extended or built-in self-test when explicitly requested
  by an operator (`ffx` or a lab test); never sent automatically by the
  framework.
* **Time limit:** Aim for under 10 seconds (2-minute deadline enforced by the
  driver host).
* **Example checks to do:** Run a controller loopback or built-in self-test that
  temporarily pauses active I/O and restores the previous state afterward.
* **Example checks to avoid:** Destructive actions that permanently lose device
  configuration or fail to restore normal operation when the check finishes.
* **Default behavior:** Returns `NOT_SUPPORTED`.

## What to report {#what-to-report}

A health check response contains a list of conditions. Each condition
describes one active problem:

* A status: DEGRADED for a fault the driver can recover from, or UNHEALTHY
  for a fault it cannot.
* A reason code, such as TRANSACTION_TIMEOUT, that is unique within the
  response.
* A short message for humans.
* Optional diagnostics, as key-value pairs.

The overall status is the most severe condition. An empty list means OK.
You can also attach diagnostics that describe the whole check, such as the
current power mode or activity counters.

If the driver cannot run the check at all, return an error instead of a
response. Drivers usually return one of these:

| Error | When to return it |
| ----- | ----------------- |
| UNAVAILABLE | The hardware is powered down for runtime idle, not initialized yet, or briefly busy. Never return this for PING. |
| NOT_SUPPORTED | The driver does not implement this level. |
| COMMUNICATION_ERROR | A bus or register access failed while running the check. |

The framework produces the rest itself. TIMEOUT means the driver missed the
deadline. INVALID_STATE means the driver is not running, for example during
system suspend. INTERNAL_ERROR means the framework failed to deliver the
check, or that the channel closed during the check.

Only TIMEOUT counts against the driver. The other errors are treated as "no
information", not as evidence of a fault.

## Report faults as they happen {#alerts}

You don't have to wait for a health check to report a fault. Use a
`HealthAlertLatch` to track active faults:

* Call Raise when you detect a fault, for example when a transfer times out.
* Call Clear when a later operation succeeds.
* Call MergeInto when you build a health check response, so the response
  includes every active fault.

The latch sends an alert only when the set of active faults changes. Raising
the same fault repeatedly in an error loop sends one alert, and clearing it
sends one more. The latch also tracks when each fault was first and last seen
and how many times it occurred.

If the fault is in a parent (ancestor) or child (descendant) driver, such as a
bus that stops responding or a child that stops consuming data, send a
neighbor alert instead: `HealthAlert::upstream_neighbor_fault` reports a
suspected fault in a bound parent (ancestor) node, and
`HealthAlert::downstream_neighbor_fault` reports a suspected fault in a bound
child (descendant) node. Driver manager responds by running FAST checks on
those immediate bound parents or children. Neighbor alerts do not change your
driver's own health record.

## Guidelines {#guidelines}

* **Report a fault only while it is active.** Use the latch rather than
  calling the alert API yourself or keeping your own time windows. If lifetime
  counters are useful, add them as diagnostics.

* **Don't change hardware state in FAST.** Many interrupt status and error
  registers clear when read, or pop an entry from a FIFO. Reading them in a
  health check steals events from your interrupt handler. Only read registers
  that are safe to read at any time.

* **Check the power state before touching hardware.** If the hardware is
  powered down for runtime idle, return UNAVAILABLE for FAST and DEEP. Make
  sure this check can't race with a power transition: read the power state
  using the same lock or dispatcher that your driver uses to power the
  hardware on and off.

* **Always answer PING.** A driver in runtime idle is still running, so answer
  PING from software state and include any active faults. You don't need to
  handle system suspend. The driver host answers INVALID_STATE for you without
  calling your driver.

## C++ implementation {#cpp}

By default, drivers based on `fdf::DriverBase2` answer OK for PING and
NOT_SUPPORTED for FAST and DEEP. DFv1 drivers running in the compat shim behave
the same way.

To add your own checks, override HealthCheck and keep an `fdf::HealthAlertLatch`
member. The latch is thread-safe, so you can raise and clear faults from any
thread. Call the completer once, on the driver dispatcher. If you drop it
without calling it, the check fails with INTERNAL_ERROR.

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/health_alert_latch.h>

class MyBusDriver : public fdf::DriverBase2 {
  // ...
 private:
  void HealthCheck(fuchsia_driver_framework::HealthCheckRequest request,
                   HealthCheckCompleter completer) override;
  void OnTransactionResult(zx_status_t status);

  bool is_powered_on_ = false;
  // Thread-safe; posts edge-triggered self_fault alerts onto the driver dispatcher.
  fdf::HealthAlertLatch health_alert_latch_{*this};
};

void MyBusDriver::HealthCheck(fuchsia_driver_framework::HealthCheckRequest request,
                              HealthCheckCompleter completer) {
  if (request == fuchsia_driver_framework::HealthCheckRequest::kDeep) {
    completer(fit::error(fuchsia_driver_framework::HealthCheckError::kNotSupported));
    return;
  }

  fuchsia_driver_framework::HealthCheckResponse response;
  if (request == fuchsia_driver_framework::HealthCheckRequest::kFast) {
    if (!is_powered_on_) {
      completer(fit::error(fuchsia_driver_framework::HealthCheckError::kUnavailable));
      return;
    }
    std::vector<fuchsia_driver_framework::HealthDiagnosticItem> items;
    items.emplace_back(
        "fifo_status",
        fuchsia_driver_framework::HealthDiagnosticValue::WithUintValue(ReadFifoStatus()));
    response.diagnostics(std::move(items));
  }

  // Merge any active conditions tracked by the driver's HealthAlertLatch.
  health_alert_latch_.MergeInto(response);
  completer(fit::ok(std::move(response)));
}

// May run on an IRQ or I/O dispatcher; the latch handles the hop to the driver dispatcher.
void MyBusDriver::OnTransactionResult(zx_status_t status) {
  if (status == ZX_ERR_TIMED_OUT) {
    health_alert_latch_.Raise(fuchsia_driver_framework::HealthCondition{{
        .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
        .reason_code = "TRANSACTION_TIMEOUT",
        .message = "Timed out waiting for transfer completion interrupt",
    }});
  } else if (status == ZX_OK) {
    health_alert_latch_.Clear("TRANSACTION_TIMEOUT");
  }
}
```

To report a neighbor fault, call ReportHealthAlert on the driver dispatcher. If
your driver calls `CreateInspector()`, `fdf::DriverBase2` also writes each
health check result and latched alert to `root/driver_health_check` in your
driver's Inspect tree automatically.

## Rust implementation {#rust}

Rust drivers have the same defaults: OK for PING and NOT_SUPPORTED for FAST and
DEEP. To add your own checks, override the health_check method of your Driver
implementation and get a latch from `DriverContext::health_alert_latch()`. To
report a neighbor fault, use `DriverContext::report_health_alert`.

```rust
async fn health_check(
    &self,
    request: fdf_component::HealthCheckRequest,
) -> Result<fdf_component::HealthCheckResponse, fdf_component::HealthCheckError> {
    if request == fdf_component::HealthCheckRequest::Deep {
        return Err(fdf_component::HealthCheckError::NotSupported);
    }
    let mut response = fdf_component::HealthCheckResponse {
        diagnostics: Some(vec![fdf_component::HealthDiagnosticItem {
            key: "check_level".to_string(),
            value: fdf_component::HealthDiagnosticValue::StringValue(format!("{request:?}")),
        }]),
        ..Default::default()
    };
    self.health_latch.merge_into(&mut response);
    Ok(response)
}

fn on_transaction_result(&self, status: zx::Status) {
    if status == zx::Status::TIMED_OUT {
        self.health_latch.raise(fdf_component::HealthCondition {
            status: Some(fdf_component::HealthStatus::Unhealthy),
            reason_code: Some("TRANSACTION_TIMEOUT".to_string()),
            message: Some("Timed out waiting for transfer completion interrupt".to_string()),
            ..Default::default()
        });
    } else if status == zx::Status::OK {
        self.health_latch.clear("TRANSACTION_TIMEOUT");
    }
}
```

Unlike C++, the Rust SDK does not write to your driver's Inspect tree
automatically. If you want a copy at `root/driver_health_check` as well, create
an `fdf_component::HealthCheckInspect` on your inspector root, pass it to
`set_inspect` on the latch, and call `record` on each health check result.

## Run health checks {#run}

To check one driver, pass its component moniker or topological path:

```posix-terminal
ffx driver health-check bootstrap/boot-drivers:dev.sys.platform.dw-i2c --level fast
```

To check every running driver:

```posix-terminal
ffx driver health-check --all --level ping
```

To see the latest result and recent history that driver manager keeps for each
driver:

```posix-terminal
ffx inspect show "bootstrap/driver_manager:root/health_checks"
```

## Where results go {#where-results-go}

You don't need to publish health results yourself. Driver manager records them
for every driver, in C++ or Rust.

Inspect
: Driver manager keeps a record for each driver under
  `bootstrap/driver_manager:root/health_checks`. The record holds the latest
  status, the active conditions and diagnostics, counts of faults and
  recoveries, and the last 16 health changes. Because driver manager owns the
  record, it stays accurate even if the driver is stuck. A passing PING does
  not hide an earlier FAST or DEEP failure. The record is kept when the driver
  restarts, and a restart marker is added to the history.

Logs
: When a driver's health changes, driver manager logs one line tagged
  `[DHC]`. Logs survive a reboot when Inspect data does not. New faults log at
  WARN and recoveries log at INFO. Passing checks that don't change anything
  log nothing. To prevent a flapping error from spamming the logs and making
  other issues hard to diagnose, logging is rate-limited per driver (with a
  summary count of suppressed transitions) and across the system.

  ```none
  [DHC][UNHEALTHY] node=bootstrap/boot-drivers:dev.sys.platform.dw-i2c url=fuchsia-boot:///dw-i2c#meta/dw-i2c.cm level=FAST reason=I2C_TRANSACTION_TIMEOUT suppressed=0 msg="Timed out waiting for TX_EMPTY interrupt"
  [DHC][OK] node=bootstrap/boot-drivers:dev.sys.platform.dw-i2c url=fuchsia-boot:///dw-i2c#meta/dw-i2c.cm level=ALERT reason=- suppressed=0 msg=""
  ```

Snapshots
: The [driver_health_detect](/src/diagnostics/config/triage/detect/driver_health_detect.triage)
  triage rule files a snapshot when a driver is UNHEALTHY or times out, at most
  once a day for each type of failure. DEGRADED conditions and UNAVAILABLE
  errors never trigger a snapshot.

A driver's own Inspect tree can also hold a copy at `root/driver_health_check`
(see the [C++](#cpp) and [Rust](#rust) sections), but triage and tools read the
driver manager record.

## FAQ {#faq}

### Will health checks spam the logs? {#faq-log-spam}

No. Passing checks and UNAVAILABLE errors log nothing. The latch sends one
alert when a fault starts and one when it clears, no matter how often you call
Raise. Driver manager also rate-limits its log lines and probes each neighbor
at most once every 5 seconds.

### Will I get a lot of bugs? {#faq-bug-volume}

No. Recoverable problems and powered-down hardware should be reported as
DEGRADED or UNAVAILABLE, and those never trigger snapshots. Only UNHEALTHY
faults and timeouts trigger snapshots, and each snapshot signature is
rate-limited to at most once every 24 hours per device.

### How do I find out about problems in my driver? {#faq-notifications}

On a device, use ffx inspect to view the driver manager record described above.
Across devices, search crash reports for the driver-health-check-unhealthy and
driver-dispatcher-timeout signatures.
