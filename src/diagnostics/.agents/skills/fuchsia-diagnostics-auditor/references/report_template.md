# [Driver / Starnix Module Name] Diagnostics Gap Analysis

## 1. Subsystem Architecture

* **Target Type**: [Fuchsia Driver (e.g., Battery / Thermal / Power / WLAN /
  Bus) | Starnix Module (`//src/starnix/...`)]
* **Subsystem Architecture & Dependency Tree**:
  - Direct Dependencies & Linked Libraries (`BUILD.gn`): [e.g.
    `//src/connectivity/wlan/lib/mlme/fullmac`,
    `//src/connectivity/wlan/lib/sme`, etc.]
  - Parent / Child Driver Protocols: [e.g.
    `fuchsia.wlan.fullmac.WlanFullmacImpl`, `fuchsia.hardware.spmi`, etc.]
  - Peer Services / VMO Consumers: [e.g. `wlandevicemonitor`, `wlancfg`,
    `ActivityGovernor`]

## 2. Existing Diagnostics Baseline Across All 5 Pillars

Audit what is ALREADY implemented in the target and its supporting subsystem
before assuming any gaps:

* **Pillar 1 - Inspect**: [Existing Inspect nodes, `Inspector` VMO exports, or
  `BoundedListNode` instances in target and backing libraries]
* **Pillar 2 - Logging (`ffx log`)**: [Current `LogSink` usage, structured
  key-value adoption, severity discipline, rate-limiting in loops]
* **Pillar 3 - Tracing (`ffx trace`)**: [Current `fuchsia_trace` usage, trace
  categories in `.cml`/`.dml`, duration/flow markers]
* **Pillar 4 - Snapshots & Automated Triage**: [Existing `detect/*.triage` rules
  monitoring this component. Note: Plain triage rules in
  `src/diagnostics/config/triage/` are deprecated in favor of internal system
  health rules]
* **Pillar 5 - Cobalt / Fleet Telemetry**: [Existing
  `src/diagnostics/sampler/config/` mappings for Inspect-to-Cobalt forwarding]

## 3. High-Priority Diagnostics Gap Analysis

Only report gaps that are **critical and valuable for debugging** field
failures or post-mortem crashes. Avoid low-value boilerplate.

| Pillar | Diagnostic Gap | Severity (Critical / High / Medium) | Mobile & Cross-Device Impact | Proposed Solution | Target Existing File |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Inspect** | [e.g. Unrecorded hardware bus timeout / NACK count] | Critical | Prevents diagnosis of bus hangs on mobile devices | Expose atomic error counter under root node | `src/device.rs` |
| **Logs** | [e.g. Unthrottled WARN in polling loop causes log overflow] | High | Rollover drops critical crash logs in snapshots | Add `warn_every_n_seconds` throttling | `src/core.cc` |
| **Trace** | [e.g. Missing duration slice on firmware IPC round-trip] | High | Blindspot during suspend/resume latency analysis | Add `TRACE_DURATION("wlan", "FirmwareIpc")` | `src/ipc.rs` |
| ... | ... | ... | ... | ... | ... |

## 4. Omission Rationale & Data Analysis (Why Certain Pillars Were NOT Added)

For any diagnostics pillar where no additions or changes are proposed, document
the technical rationale and supporting data:

| Pillar | Status | Technical & Architectural Rationale | Supporting Data / Constraints |
| :--- | :--- | :--- | :--- |
| **Tracing (`ffx trace`)** | [Omitted / Added] | [e.g. Component executes exclusively on infrequent asynchronous control events (<1 Hz). Tracing would add binary bloat without timing insights.] | [Call frequency: ~0.1 calls/min, latency SLA: non-realtime, no lock contention observed] |
| **Snapshots / Triage (Detect)** | [Omitted / Added] | [e.g. Failure modes are non-fatal or existing Detect rules already trigger crash snapshots; plain triage rules under `src/diagnostics/config/triage/` are deprecated in favor of internal system health rules.] | [Rule coverage: 100% of unrecoverable driver hang conditions covered in `detect/*.triage`] |
| **Cobalt / Fleet Telemetry** | [Omitted / Added] | [e.g. Internal hardware register retries are debug-level; high-level connection reliability is already forwarded by platform policy.] | [Privacy/bandwidth guidelines: debug metrics not eligible for fleet upload] |
| **Inspect** | [Omitted / Added] | [...] | [...] |
| **Logs** | [Omitted / Added] | [...] | [...] |

## 5. Proposed Code Changes (Integrated into Existing Files)

[Show C++ / Rust code snippets and `BUILD.gn` updates.

**STRICT REQUIREMENT**: Integrate changes directly into pre-existing source
files (e.g. `device.rs`, `lib.rs`, `core.cc`, `inspect.rs`). Do NOT introduce a
standalone `diagnostics.rs` or `diagnostics.cc` file unless the component's
architecture explicitly requires it. Use inline comments for constants or a
`<component>/README.md` section if complex diagnostic bitmasks/thresholds are
introduced.]
