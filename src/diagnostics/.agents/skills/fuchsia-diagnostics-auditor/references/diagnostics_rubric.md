# Multi-Pillar Diagnostic Audit Rubric for Fuchsia Drivers & Starnix Modules

## Overview: The 5 Diagnostics Pillars

Observability on Fuchsia is built across five complementary diagnostics pillars:

1. **Inspect (`ffx inspect`)**: Component state, health indicators, critical
   failure counters, and bounded history.
2. **Logs (`ffx log` / `fuchsia.logger.LogSink`)**: Structured event logs,
   failure context, and rate-limited error signals.
3. **Trace (`ffx trace` / `fuchsia_trace` / Perfetto)**: Submicrosecond
   timelines, execution flow, and latency bottlenecks.
4. **Snapshots & Automated Triage (`ffx target snapshot`, `Detect`)**:
   Post-mortem crash analysis, automated anomaly detection, and VMO memory
   budgeting.
5. **Cobalt / Fleet Telemetry (`src/diagnostics/sampler/config/`)**: Aggregated
   fleet reliability metrics.

When auditing any Fuchsia driver (battery, thermal, power, WLAN, USB, audio,
display, buses) or Starnix module (`//src/starnix/...`), evaluate **all five
pillars**. Every proposed addition must be **critical and high-value for
debugging**, avoiding boilerplate wrappers and low-value metrics.

---

## 1. Mandatory Pre-Step: Deep Subsystem & Existing Code Analysis

**Never assume a diagnostics gap exists merely from the lack of a local
`diagnostics.rs` or `diagnostics.cc` file.** Before declaring any gap:

1. **Trace Subsystem Dependencies in `BUILD.gn` / `BUILD.bazel`**:
   - Inspect all direct dependencies, backing libraries (e.g.
     `//src/connectivity/wlan/lib/mlme/fullmac`,
     `//src/connectivity/wlan/lib/sme`), and shared diagnostic crates.
   - Determine whether diagnostics are already implemented in a backing library,
     parent driver, or companion service.
2. **Audit Existing Diagnostics Across the Subsystem**:
   - **Inspect**: Search for `Inspector`, `inspect_insert!`, `BoundedListNode`,
     `copy_vmo()`, or `duplicate_vmo()` across the target and its dependencies.
     Check if an Inspect VMO is already forwarded over a bootstrap protocol
     (e.g. `UsmeBootstrap::Start`) or published by a peer/parent component.
   - **Logs**: Search for `fuchsia.logger.LogSink`, `tracing::`, `log::`, or
     `syslog`. Audit log severity discipline, structured keys, and
     rate-limiting.
   - **Trace**: Search for `fuchsia_trace`, `TRACE_DURATION`, `TRACE_FLOW`, and
     trace categories in `.cml` / `.dml` / `BUILD.gn`.
   - **Detect**: Search `detect/*.triage` for existing rules monitoring this
     component or subsystem.
   - **Cobalt / Sampler**: Search `src/diagnostics/sampler/config/` for
     Inspect-to-Cobalt mappings.
3. **Identify Integration Targets in Existing Source Files**:
   - Note which existing source files (e.g. `device.rs`, `lib.rs`, `core.cc`,
     `inspect.rs`, `service.rs`) already handle the relevant operations so new
     diagnostics are integrated directly into them.

---

## 2. The 5-Pillar Diagnostics Checklist

### Pillar 1: Inspect (`ffx inspect` / Component State & Health)

* **Purpose**: Structured representation of current state, health indicators,
  critical failure counters, and bounded recent history for snapshot triage.
* **Critical & High-Value Diagnostics to Look For**:
  - **Hardware Bus & Transport Deadlocks**: Real hardware timeouts
    (`ZX_ERR_TIMED_OUT`), NACKs, CRC/checksum corruptions, and bus
    reset/recovery events across SPMI, I2C, SPI, PCIe, USB, and MIPI-DSI.
  - **Power & Thermal State Desyncs**: Fuel gauge metrics, charging state
    anomalies, battery capacity degradation ratios, thermal trip-point
    crossings, throttling durations, and wake-lease/suspend-blocker desyncs.
  - **Starnix Kernel & Bridge Anomalies**: Android-to-Fuchsia FIDL/HAL bridge
    failures, `ioctl`/sysfs error rates, Linux `errno` <-> `zx_status_t`
    translation faults, and wake-lock state.
  - **Lifecycle & State of Health (SoH)**: Hardware initialization status,
    firmware version, degraded mode flags, and device recovery counts.
  - **Static vs. Lazy Inspect Nodes (Use Lazy Only When Appropriate)**:
    - **Prefer Static VMO Properties by Default**: Use static properties
      (`inspect::UintProperty`, `StringProperty`, `Node`, etc.) for fixed
      configuration and metadata (e.g. VID/PID, revisions, initialization
      parameters), error and event counters, and lifecycle state. Static
      properties are preserved directly in the Inspect VMO, making them
      readable in post-mortem crash dumps, minidumps, and hung-thread triage
      without needing to execute driver code.
    - **Use Lazy Nodes (`CreateLazyNode` / `CreateLazyValues`) Only When
      Appropriate**:
      * Appropriate for expensive-to-compute or high-cardinality state (e.g.
        hardware register dumps, routing tables, cache contents) that should
        only be materialized on-demand when queried.
      * Appropriate for extremely hot paths where continuous VMO property
        mutations would cause unacceptable locking or memory churn, provided an
        active async dispatcher serves `fuchsia.inspect.Tree` (DFv2).
      * Anti-pattern: Do NOT use lazy nodes for simple counters, static device
        metadata, or in components/drivers where failure in the async loop would
        prevent post-mortem crash inspection. Never use lazy nodes in legacy
        DFv1 drivers that only export a static VMO via
        `DeviceAddArgs::set_inspect_vmo()`.
  - **Bounded History (`BoundedListNode`)**: A bounded circular buffer (e.g.
    16-32 entries) of recent state transitions or fatal errors -- **only** if
    this history cannot be derived from logs and is strictly budgeted within the
    driver's Inspect VMO memory limit.
* **Low-Value Fluff to Avoid**:
  - Do NOT wrap every FIDL method to count total calls and errors when the FIDL
    framework or higher layers already track channel status.
  - Do NOT generate arbitrary 16-bucket latency histograms unless there is a
    specific latency SLA or known performance cliff.
  - Do NOT create a parallel, disconnected `Inspector` if an Inspect hierarchy
    is already rooted in a parent component or peer service.

### Pillar 2: Logging (`ffx log` / `fuchsia.logger.LogSink`)

* **Purpose**: Human-readable, low-frequency event recording, structured search,
  and failure context in snapshots.
* **Critical & High-Value Diagnostics to Look For**:
  - **Structured Key-Value Fields**: Verify that log statements use structured
    key-value pairs (e.g. `info!(tag = "fullmac", status = %status, bssid =
    %bssid, "...")` in Rust or `FX_LOGS(INFO)` with key-values in C++) rather
    than unsearchable string interpolation.
  - **Rate Limiting & Log Flooding Prevention**: Verify that hot paths (e.g.
    interrupt handlers, frame rx/tx loops, polling loops) use throttling (e.g.
    `warn_every_n_seconds`, token bucket, or deduping). Unthrottled logs roll
    over the syslog buffer and erase critical crash logs in snapshots.
  - **Severity Discipline**:
    - `ERROR`: Reserved strictly for actual bugs and defects in driver/system
      behavior that require investigation (e.g., assertion failures, internal
      state corruption, unexpected protocol invariants broken, hardware
      resets, unexpected channel drops, and data loss). Do not log at `ERROR`
      for operational or expected failure conditions.
    - `WARN`: Expected failure conditions or degraded states (even if
      unrecoverable for that request) that do not indicate a bug in the
      component itself (e.g., bad client inputs, remote peer disconnection,
      external network drop, exhausted transient retries, or missing optional
      services).
    - `INFO`: Sparse lifecycle milestones (startup, shutdown, link up/down).
    - `DEBUG`/`TRACE`: High-frequency per-packet or polling details.
  - **Silent Failure Prevention**: Ensure functions returning errors log
    actionable context and `zx_status_t` rather than failing silently.

### Pillar 3: Tracing (`ffx trace` / `fuchsia_trace` / Perfetto)

* **Purpose**: Submicrosecond timeline correlation, execution flow
  visualization, and performance bottleneck analysis in Perfetto.
* **Critical & High-Value Diagnostics to Look For**:
  - **High-Frequency & Latency-Critical Paths**: Packet tx/rx processing,
    hardware DMA transfers, interrupt dispatch loops, firmware command
    round-trips, IPC latency between Starnix and Fuchsia services, and
    suspend/resume preparation.
  - **Trace Primitives**:
    - `TRACE_DURATION("category", "name")`: Scope-based duration tracking.
    - `TRACE_FLOW_BEGIN/STEP/END`: Correlating asynchronous requests across
      threads, processes, and drivers.
    - `TRACE_COUNTER`: Tracking instantaneous queue depths, active buffers, or
      pending work.
  - **Manifest Declaration**: Ensure the trace category is declared in the
    component manifest (`.cml` / `.dml`) and build configs.
* **When Tracing is NOT Appropriate (Justify with Data)**:
  - If the component only processes infrequent asynchronous configuration
    requests (e.g. < 1 Hz) and has no sub-millisecond latency budgets or
    real-time pipelines, tracing adds binary bloat and overhead with no
    diagnostic value.

### Pillar 4: Snapshots & Automated Triage (`ffx target snapshot`, `Detect`)

* **Purpose**: Holistic system state capture, post-mortem crash analysis, and
  automated anomaly detection.
* **Critical & High-Value Diagnostics to Look For**:
  - **Detect Integration (`detect/*.triage`)**: Conditions that should
    automatically trigger a system snapshot or crash report before a device
    reboots or hangs (e.g., fatal hardware hangs, persistent firmware
    unresponsive states).
  - **Inspect VMO Budgeting**: Ensure the component's total Inspect memory
    footprint stays within allocated limits (`INSPECT_VMO_SIZE_BYTES`),
    preventing truncated snapshots.
  - **Preservation Across Teardown**: Ensuring critical state from crashed or
    destroyed instances is preserved in a surviving parent/monitor component.
* **Deprecation Notice & Plain Triage Rules**:
  - **Discourage Plain Triage Rules**: Authoring plain triage rules under
    `src/diagnostics/config/triage/` for manual `ffx triage` is deprecated in
    favor of internal system health monitoring rules. Only integrate with
    Detect (`detect/*.triage`) when automated crash/snapshot captures are
    warranted.
* **When Detect Rules are NOT Appropriate (Justify with Data)**:
  - If existing Detect rules or internal health monitors already inspect the
    subsystem, or if failure modes are non-fatal, recoverable, and handled
    locally without requiring an emergency crash snapshot.

### Pillar 5: Cobalt / Fleet Telemetry (`src/diagnostics/sampler/config/`)

* **Purpose**: Privacy-preserving, fleet-wide reliability tracking across
  production and dogfood devices.
* **Critical & High-Value Diagnostics to Look For**:
  - **Sampler Mappings**: Verify whether key Inspect counters (e.g. hardware
    crash count, unexpected reboot count, daily connection failure rate) should
    be sampled by `Sampler` into Cobalt metrics.
* **When Cobalt is NOT Appropriate (Justify with Data)**:
  - Detailed driver-internal debug counters (like individual register retry
    counts or microsecond histograms) are unsuitable for fleet-wide telemetry
    due to privacy, bandwidth, and aggregation limits. Cobalt is reserved for
    high-level user-impacting health and reliability metrics.
