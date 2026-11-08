<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Subagent Reference: Hardware Prober (`hardware-prober`)

## Purpose

Empirically interrogate live target hardware and in-driver software state
with Fuchsia's **`driver-lab`** (`lab_proxy`, `driver_lab_rust::embedded`,
and the `driver-lab.pyz` / `DriverLab` host tooling), and produce hashed
evidence bundles that back every `[VERIFIED]` claim. The same role serves two
workflows:

*   **Fix workflow (`autoda-fix`):** Investigate hardware register state
    (Mode A) or software state and knobs (Mode B) in an existing, bound
    driver, and verify a fix in-situ.
*   **Recon workflow (`autoda-recon`):** Verify hypotheses about a block that
    Fuchsia does not yet drive, usually through `lab_proxy` on an unclaimed
    node, within a campaign's consent policy and probe budget.

Mechanics (plan schema, grants, exit codes, evidence layout, Python API) are
specified by the [`driver-lab` skill](/src/devices/skills/driver-lab/SKILL.md)
and are not repeated here.

> **Note on Mode C Bugs:** Do not invoke `hardware-prober` to shoe-horn
> `driver-lab` into pure logic, parser, or deterministic lifecycle bugs
> (`Mode C`) that are faster and more directly verified via `fx test` or
> `ffx log`.

---

## Core Rules (Both Workflows)

1.  **Working Directory:** Always execute `driver-lab.pyz` with `Cwd` set to
    `$(fx get-build-dir)` so bundled `fuchsia-controller` shared libraries and
    `host_x64/ffx` resolve cleanly:
    ```bash
    cd $(fx get-build-dir)
    DRIVER_LAB="python3 host_x64/obj/tools/driver-lab/driver-lab.pyz"
    ```
2.  **Choose the Access Mode from the Node's State, Never from Habit:**
    *   `in-situ`: the node is owned by a bound driver that exposes
        `fuchsia.driver.lab.Service` (`list --debug-capable`). Preserves the
        driver's live state; required for Mode A and Mode B in the fix
        workflow and preferred in recon whenever an instrumented driver
        already owns the block.
    *   `proxy`: the node is unclaimed (`list --unclaimed`). Bind `lab_proxy`
        with `bind-proxy`, probe, and release with `end-proxy`. The normal
        case in recon. Never unbind a working driver to get proxy access.
    *   `direct`: the block's published FIDL protocol (GPIO, I2C, SPI) is
        enough and no private MMIO or target audit is needed.
    *   If no node is published, the block needs devicetree enablement; report
        `NO_NODE_PUBLISHED` to the orchestrator rather than improvising.
3.  **Identity Before Every Session:** Capture `boot_id`, `policy_digest`,
    `resource_digest`, and per-resource digests with `describe`, and pin
    `expected_boot_id` / `expected_resource_digest` in every plan. Re-run
    `describe` after any reboot, driver restart, or recovery.
4.  **Component Moniker vs. Node ID (in-situ):** `list --debug-capable`
    returns each node's `moniker` (e.g. `"spi-0"`). Pass `--node "spi-0"` and
    the full component moniker `--moniker "bootstrap/base-drivers:spi-0"` (or
    `"bootstrap/full-drivers:<node>"` if `:` is not already present) to
    `describe`, `inspect`, and `run`.
5.  **Grant Digest Resolution (per-resource digest):** When persisting read
    grants via `driver-lab permissions add`, pass the **per-resource digest**
    (`resources[i].digest` from `describe` or `target.description.json`) for
    the specific resource, which `driver-lab run` checks on each read. If also
    running `permissions explain --plan`, add a grant with the node bundle
    `resource_digest` as well.
6.  **No Persistent Write Grants:** Persistent write grants are rejected by
    design. For an operator-approved mutating plan (`mmio_write32`,
    `knob_write32`, `trigger_write32`), pass `--consent` and pipe `"a\n"` per
    write operation to `stdin`. Writes follow the session's consent policy
    (user approval in the fix workflow; `hardware.consent.writes` in a recon
    campaign).
7.  **Respect the Ceiling and the Classification:** Never read offsets that
    `linux-driver-expert` or `datasheet-researcher` classified destructive-read
    or hard-deny; a target-side denial (`hard_denied`, `rejected_allowlist`)
    is a finding to report, not an obstacle to route around.
8.  **Ground-Truth Evidence:** Never claim a register or state value is
    verified without citing `target-audit.jsonl` (`status: "ok"`), the
    finalized `manifest.json` SHA-256 from `evidence/<run_id>/`, and the
    `boot_id`. Report `run_id`, plan digest, manifest hash, and audit sequence
    range for every run.
9.  **Out-of-Band Recovery:** If a probe wedges the target, use the recovery
    method agreed in Phase 0 (fix) or `hardware.recovery` (recon): the board's
    power control, the fleet's recovery tooling, `ffx target reboot`, or `ffx
    emu stop` / `start` for virtual targets. Record the wedge, the offset that
    triggered it, the recovery action, and the `boot_id` change. A wedge is
    evidence about the block; do not retry the same access blindly.

---

## Fix Workflow (`autoda-fix`): In-Situ Investigation & Verification

### 1. Discover & Describe the Active Driver
```bash
# Discover active drivers exposing fuchsia.driver.lab.Service:
$DRIVER_LAB list --debug-capable --target <target>

# Capture live boot_id, policy_digest, resource_digest, and per-resource digests (mmio0, state0, irq0):
$DRIVER_LAB describe \
  --moniker "bootstrap/base-drivers:spi-0" \
  --node "spi-0" \
  --target <target>
```

### 2. Fast Single-Register / Single-Slot Inspection
Use `inspect` for quick read-only register or state slot checks and to view
the recent target audit ring without authoring a full plan:
```bash
$DRIVER_LAB inspect \
  --moniker "bootstrap/base-drivers:spi-0" \
  --node "spi-0" \
  --resource mmio0 \
  --offset 0x00 \
  --target <target>
```

### 3. Mode A: Execute Declarative Hardware Register / IRQ Probe Plans (`run`)
Author a canonical in-situ plan (`mode: "in-situ"`, `activation: "in-situ"`):
```json
{
  "schema_version": 1,
  "run_id": "run-20260925-diag-001",
  "case_id": "verify-register-state",
  "target": {
    "selector": "<target>",
    "expected_boot_id": "<boot_id_from_describe>"
  },
  "node": {
    "id": "spi-0",
    "driver_moniker": "bootstrap/base-drivers:spi-0",
    "expected_resource_digest": "<resource_digest_from_describe>"
  },
  "access": {
    "mode": "in-situ",
    "activation": "in-situ",
    "requires_target_policy": true,
    "requires_target_audit": true
  },
  "operations": [
    {"kind": "mmio_read32", "resource": "mmio0", "offset": "0x00"},
    {"kind": "mmio_read32", "resource": "mmio0", "offset": "0x08"},
    {"kind": "mmio_read32", "resource": "mmio0", "offset": "0x1c"},
    {"kind": "mmio_read32", "resource": "mmio0", "offset": "0x28"}
  ]
}
```

### 4. Mode B: Single-Build Software Bug Zero-Compile Loop (`StateBank`)
When investigating concurrency/race or state-machine bugs via a `"state0"`
`StateBank` (Rust) or `StateVmoBank` (C/C++), run a **Repro Plan**
(`KNOB_FIX_ENABLED = 0`) followed immediately in the **same boot** by a **Fix
Proof Plan** (`KNOB_FIX_ENABLED = 1`).

> **Mandatory Mode B Plan Schema Rule:** Always use the Phase 3 `StateBank`
> operation kinds (`knob_write32`, `trigger_write32`, `state_read32`,
> `state_poll32`) when targeting `"state0"`. **Never** use `mmio_read32`,
> `mmio_write32`, or `mmio_poll32` with `"resource": "state0"` (`driver-lab
> plan validate` rejects `mmio_*` operations on `state*` resources). Both
> `before_fix_probe.json` and `after_fix_probe.json` in Mode B should include
> `knob_write32` (setting fault/fix knobs), `trigger_write32` (or workload
> trigger), and `state_read32` / `state_poll32`.

```json
{
  "schema_version": 1,
  "run_id": "run-20260925-mode-b-repro-and-proof",
  "case_id": "single-build-race-repro-and-fix-proof",
  "target": {
    "selector": "<target>",
    "expected_boot_id": "<boot_id_from_describe>"
  },
  "node": {
    "id": "spi-0",
    "driver_moniker": "bootstrap/base-drivers:spi-0",
    "expected_resource_digest": "<resource_digest_from_describe>"
  },
  "access": {
    "mode": "in-situ",
    "activation": "in-situ",
    "requires_target_policy": true,
    "requires_target_audit": true
  },
  "operations": [
    {"kind": "knob_write32", "resource": "state0", "offset": "0x08", "value": "500"},
    {"kind": "knob_write32", "resource": "state0", "offset": "0x0c", "value": "0"},
    {"kind": "trigger_write32", "resource": "state0", "offset": "0x10", "arg": "1"},
    {"kind": "state_read32", "resource": "state0", "offset": "0x04"},
    {"kind": "knob_write32", "resource": "state0", "offset": "0x0c", "value": "1"},
    {"kind": "trigger_write32", "resource": "state0", "offset": "0x10", "arg": "1"},
    {"kind": "state_read32", "resource": "state0", "offset": "0x04"},
    {"kind": "state_poll32", "resource": "state0", "offset": "0x00", "expected": "0", "mask": "0xffffffff"}
  ]
}
```

Validate, digest, and run:
```bash
$DRIVER_LAB plan validate --plan plan.json
$DRIVER_LAB plan digest --plan plan.json

$DRIVER_LAB run \
  --plan plan.json \
  --evidence-dir evidence/ \
  --grants grants.toml \
  --target-scope "<target>" \
  --node-id "spi-0" \
  --moniker "bootstrap/base-drivers:spi-0" \
  --target "<target>" \
  --consent
```

### 5. Post-Fix Verification Checklist (Phase 3, Step 3c)
After `fuchsia-driver-fixer` builds and reloads/deploys a driver fix:
1.  **Refresh Identity:** Re-run `describe` first. A target reboot or `ffx
    driver restart` changes `boot_id` and/or `resource_digest` (if ceiling
    parameters or `StateBank` slots changed). Update `plan.json` and
    `grants.toml` if the resource digest changed.
2.  **Run Verification Plan:** Execute the verification `plan.json` against
    the patched driver.
3.  **Verify Audit Trail:** Inspect `evidence/<run_id>/target-audit.jsonl`
    and confirm:
    *   Every expected register or `state0` read/write has `decision:
        "allowed"` and `status: "ok"`.
    *   The observed values match the post-fix expectations in `bug_spec.md`.
    *   If mutating operations ran, `quiesce_engaged` and `quiesce_released`
        bracketed the writes.
4.  **Verify System Logs:** Check `evidence/<run_id>/serial.log` and `ffx log
    dump` to confirm no driver errors, panics, or bug symptoms remain.
5.  **Report Structured Evidence:** Return the `run_id`, `plan_digest`,
    `manifest.json` SHA-256 hash, audit sequence range, and
    per-register/per-slot results to the orchestrator.

---

## Recon Workflow (`autoda-recon`): Verifying Claims on an Unclaimed Block

### 1. Discover the Node and Pick the Mode
```bash
# Unclaimed nodes eligible for lab_proxy:
$DRIVER_LAB list --unclaimed --target <target>
# Nodes already owned by an instrumented driver (prefer in-situ for these):
$DRIVER_LAB list --debug-capable --target <target>

# Proxy mode on an unclaimed node:
$DRIVER_LAB bind-proxy --node "<node_id>" --target <target>
$DRIVER_LAB describe --node "<node_id>" --target <target>
# ... probe ...
$DRIVER_LAB end-proxy --node "<node_id>" --target <target>
```
If the node is absent from both lists, return `NO_NODE_PUBLISHED`; the
orchestrator owns devicetree enablement and will re-invoke this role once a
node exists.

### 2. Gate on Dependencies, Then Prove Liveness
*   Read `## 4. Dependencies & Bring-Up Order` in `recon_spec.md` before the
    first access. Do not issue any read or write to a block whose clocks,
    resets, or power domains are not established as on; reads of an unclocked
    block return garbage or hang the bus.
*   The first plan reads one identification, version, or status register with
    a known expected pattern (from the datasheet or driver). Proceed to
    hypotheses only after it returns a valid value. A liveness failure is a
    finding about dependencies, not a reason to try other offsets.

### 3. Probe What Documents Cannot Settle
Prioritize, in order: values that documents and source disagree on (the
contradiction ledger), registers classified reserved that the driver touches,
observed reset or firmware-configured values, and interrupt and status
behavior after a minimal sequence. Do not spend budget confirming what the
datasheet already states unless a contradiction depends on it. Stop at
`hardware.probe_budget` and report what remains.

### 4. Tag the Origin of Every Observed Value
Record `state_origin` for every value:
*   `firmware-configured`: the bootloader, secure firmware, or an earlier
    driver touched the block before the probe. On a booted device this is the
    default assumption.
*   `post-reset`: the plan itself deasserted reset or the dependency analysis
    shows nothing ran before the probe.

A value read on a booted device is not a power-on reset value. A datasheet
reset value and a `firmware-configured` observation that differ are not a
contradiction; report both with their origin.

### 5. Report for Claim Promotion
For each hypothesis: `run_id`, plan digest, manifest SHA-256, `boot_id`,
audit sequence range, observed value, `state_origin`, and whether it confirms,
refutes, or leaves open the hypothesis. The orchestrator promotes a claim to
`[HW_VERIFIED]` `[VERIFIED]` only from this record, and the same values must
appear in `recon_devlog.md` for the `verified` gate to pass.
