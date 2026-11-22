---
name: fuchsia-diagnostics-auditor
description: >
  Audits Fuchsia drivers and Starnix modules across all five diagnostics
  pillars (Inspect, Logs, Trace, Cobalt, and Snapshots/Detect), produces a gap
  analysis report, and implements high-value instrumentation. Use when asked
  to run a diagnostics or observability audit, perform a multi-pillar
  diagnostics gap analysis, or systematically harden instrumentation in a
  driver or Starnix module. Don't use for code review (/review), live crash or
  test failure debugging (fuchsia-debugger or ffx log/inspect), or adding a
  single requested log or trace statement.
---

# Fuchsia Diagnostics Auditor Runbook (Multi-Pillar Diagnostics)

Follow these steps to perform an **agentic diagnostics** audit of any Fuchsia
driver (e.g., battery, thermal, power, WLAN, peripheral buses) or Starnix module
across **all five Fuchsia diagnostics pillars** (Inspect, Logs, Trace, Cobalt,
and Snapshots/Detect).

**Core Caliber Standards**:

1.  **Analyze Existing Code Before Assuming Gaps**: Never assume a diagnostics
    gap exists from a missing local file. Inspect the target component AND its
    subsystem dependencies (backing libraries, parent/child drivers, and peer
    services).
2.  **Reuse Existing Files (No Unnecessary New Files)**: Always integrate
    diagnostics directly into existing source files (e.g., `device.rs`,
    `lib.rs`, `core.cc`, `inspect.rs`). Do NOT default to creating
    `diagnostics.rs` or `diagnostics.cc`.
3.  **High-Value Metrics Only**: Add only critical, actionable diagnostics for
    debugging field failures or post-mortem crashes. Reject boilerplate wrappers
    and arbitrary histograms.
4.  **Multi-Pillar Evaluation & Omission Justifications**: Audit all 5 pillars.
    For any pillar not added (e.g. Trace or Snapshot/Detect), document the exact
    technical rationale and supporting data in the gap analysis report.

---

## Workflow Steps

### Step 1: Resolve Target & Map Subsystem Architecture

The user input may be an exact path or a free-form name:

* **Case A: Explicit Path Provided** (e.g.,
  `//src/devices/power/drivers/battery`,
  `//src/connectivity/wlan/drivers/wlanif`, `//src/starnix/modules/...`):
  1.  Strip leading `//` to get the relative repository path.
  2.  Verify that `BUILD.gn` (or `BUILD.bazel`) exists in the target directory
      and inspect the build targets and component manifests (`.cml`).

* **Case B: Raw Name Provided** (e.g., `"battery fuel gauge"`, `"wlan driver"`,
  `"thermal driver"`, `"starnix power module"`):
  1.  Locate the component hermetically using `git ls-files` scoped to driver
      and module trees (excluding `out/`):
     ```bash
     git ls-files 'src/devices/*BUILD.gn' 'src/starnix/*BUILD.gn'
     ```
     Or search for component manifests (`.cml`). If connected to a development
     target, discover active components via `ffx driver list` or `ffx component
     list`.
  2.  Select the matching target driver or module directory.

* **Target Validity & Anti-Stale Guardrails (Prevent Hallucinations)**: Before
  performing an audit on a resolved directory, verify:
  1.  **Production Component Verification**: Ensure the target directory defines
      an active production driver component (`fuchsia_driver_component` in
      `BUILD.gn` with `runner: "driver"` in its `.cml`) or Starnix module, NOT a
      test fake or sample.
  2.  **Path Exclusion**: Discard any candidates inside `testing/`, `tests/`,
      `fake/`, `mock/`, `examples/`, or `testing_utils`.
  3.  **Freshness & Deprecation Check**: Run `git log -n 3 --oneline
      <target_directory>`. Discard targets marked as deprecated or superseded.
  4.  **DFv2 vs DFv1 Architecture & Gerrit In-Flight Check**:
     - Check whether the driver is modern DFv2 (`driver_base.h`) or legacy DFv1
       (`ddk/device.h`). Prioritize active DFv2 implementations.
     - **If only a legacy DFv1 implementation is present**: Query Gerrit to
       check if any open CLs are targeting migration to DFv2 before authoring
       code changes:
       ```bash
       curl -s "https://fuchsia-review.googlesource.com/changes/?q=status:open+project:fuchsia+<driver-name>+dfv2"
       ```
       If an active DFv2 migration is in flight, coordinate with that migration
       CL rather than adding diagnostics to deprecated DFv1 code.

* **Map the Subsystem Dependency Tree (Crucial)**: Examine `BUILD.gn` (or
  `BUILD.bazel`) in the target directory to identify:
  - Backing libraries (e.g., `//src/connectivity/wlan/lib/mlme/fullmac`,
    `//src/connectivity/wlan/lib/sme`, helper libraries).
  - Parent/child driver protocols (e.g. `fuchsia.wlan.fullmac.WlanFullmacImpl`,
    `fuchsia.hardware.spmi`).
  - Peer services or consumers that receive Inspect VMOs or handle telemetry
    (e.g., `wlandevicemonitor`, `ActivityGovernor`).

---

### Step 2: Deep Existing Codebase Audit Across All 5 Pillars

**Do NOT assume a diagnostics gap exists without thoroughly reviewing existing
code in both the target and its backing libraries.**

Review the [5-pillar diagnostics rubric](references/diagnostics_rubric.md) for
the comprehensive checklist of what to look for and what anti-patterns to avoid
across all five pillars:

1.  **Pillar 1 - Inspect (`ffx inspect`)**: Audit existing nodes, properties,
    and exported VMOs in the target and linked libraries.
2.  **Pillar 2 - Logging (`ffx log` / `fuchsia.logger.LogSink`)**: Audit
    structured key-values, severity discipline (`ERROR` for bugs vs `WARN` for
    expected failures), and rate limiting in loops.
3.  **Pillar 3 - Tracing (`ffx trace` / `fuchsia_trace` / Perfetto)**: Audit
    latency-critical paths, duration/flow markers, and manifest declarations.
4.  **Pillar 4 - Snapshots & Automated Triage (`ffx target snapshot`,
    `Detect`)**: Audit `detect/*.triage` rules. Plain triage under
    `src/diagnostics/config/triage/` is deprecated in favor of internal health
    monitoring rules.
5.  **Pillar 5 - Cobalt / Fleet Telemetry (`src/diagnostics/sampler/config/`)**:
    Audit Inspect-to-Cobalt mappings in `src/diagnostics/sampler/config/`.

---

### Step 3: Multi-Pillar Gap Analysis & Omission Rationale

1.  Using the [gap analysis report template](references/report_template.md),
    produce a comprehensive gap analysis artifact.
   - Save the report to a scratch or artifact directory (e.g.
     `/tmp/{leaf_name}_diagnostics_gap_analysis.md` or the conversation artifact
     directory) to avoid writing untracked Markdown files directly into source
     directories.
2.  **High-Value Bar (No Low-Value Fluff)**:
   - Propose additions **only** for critical failure modes, unrecorded hardware
     timeouts, power state desyncs, or missing rate limits.
   - Avoid mechanical wrappers around every FIDL call or generic histograms.
3.  **Mandatory Omission Documentation**:
   - For every pillar where no additions are proposed (e.g., why Tracing,
     Snapshot/Detect, or Cobalt was omitted), provide the exact technical
     rationale and supporting data (call frequency, non-realtime latency
     constraints, privacy guidelines, or existing coverage).

---

### Step 4: Implement Proposed Changes (Strictly Reuse Existing Files)

1.  **Git Hygiene**:
   - Check `git status --short` before editing to ensure a clean working copy.
   - If working on multiple isolated changes in parallel, use an isolated
     worktree via the `fx-worktree` skill.
2.  **STRICT RULE: Integrate Into Existing Files First**:
   - **Always add diagnostics directly into pre-existing source files** (e.g.,
     `device.rs`, `lib.rs`, `core.cc`, `inspect.rs`, `service.rs`).
   - **DO NOT create a new `diagnostics.rs`, `diagnostics.cc`, or
     `diagnostics.h` file** unless the component explicitly uses a multi-file
     modular architecture and no existing file can house the logic.
   - **DO NOT create boilerplate wrapper classes** that wrap every method just
     to increment counters.
3.  **Inspect Pattern: Use Lazy Nodes Only When Appropriate**:
   - **Default to static Inspect VMO properties** for fixed metadata (e.g.
     VID/PID, firmware revisions), error counters, and lifecycle state.
     Static properties survive driver crashes and dispatcher hangs, making
     them readable in post-mortem crash dumps without executing driver code.
   - **Use lazy nodes (`CreateLazyNode` / `CreateLazyValues`) only when
     appropriate**: for expensive-to-compute or high-cardinality state
     materialized on read, or hot paths where mutating VMO properties causes
     lock contention. Lazy nodes require an active async dispatcher serving
     `fuchsia.inspect.Tree` (DFv2); do not use them in legacy DFv1 drivers
     exporting static VMOs. See the
     [diagnostics rubric](references/diagnostics_rubric.md) for criteria.
4.  Update `BUILD.gn` (or `BUILD.bazel`) for dependencies (`fuchsia_inspect`,
    `fuchsia_trace`, `fuchsia.logger`) only if newly required.
5.  Document specific bitmasks, status codes, or thresholds using inline code
    comments (or `<component>/README.md` only for complex multi-parameter
    subsystems).
6.  **Verification Loop (Build -> Lint -> Test)**:
   - **Build**: Run `fx build` to ensure the target compiles cleanly.
   - **Lint**: Run `fx lint` or `fx format-code` on modified files.
   - **Test**: Run `fx test <target-test-package>` to verify existing tests
     pass, and add or update unit tests (e.g. using `assert_data_tree!` in Rust
     or inspect hierarchy assertions in C++) to verify new Inspect properties
     and diagnostics behavior.

---

### Step 5: Format, Commit & Upload Gerrit CL

Follow the `fuchsia-commit-message-auditor` skill guidelines:

1.  **Format Code**: Run `fx format-code` on modified files.
2.  **Stage Intended Files Explicitly**: Stage only target files by path (`git
    add path/to/file1.cc path/to/file2.rs`). Avoid catch-all staging (`git add
    .`).
3.  **Craft Commit Message**:
   - **Subject Line**: Use the component's concise leaf tag (e.g., `[usb-bus]`,
     `[battery]`, `[wlanif]`, not deep directory paths), imperative mood, under
     50 characters (e.g., `[battery] Add Inspect health diagnostics and
     tracepoints`).
   - **Body**: Provide concise multi-pillar bullets describing only the exact
     changes made across Inspect, Logs, or Trace.
   - **Footer**: Include standard `Test:` and `Change-Id:` footers. Do not add
     free-text prose in the trailer block to maintain Git/Gerrit trailer
     key-value parsing.
4.  **User Confirmation Before Upload**:
   - **Always confirm with the user before uploading** (e.g. presenting the diff
     and commit message for review), unless the user's initial request
     explicitly instructed you to upload the CL.
   - Once confirmed, upload using standard Jiri:
     ```bash
     jiri upload -topic=diagnostics-audit-<leaf-component-name>
     ```
