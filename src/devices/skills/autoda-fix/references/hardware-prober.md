<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Subagent Reference: Hardware Prober (`hardware-prober`)

## Purpose

Empirically interrogate live target hardware (`Mode A`) and in-driver software
state/concurrency knobs (`Mode B`) and verify driver bug fixes in-situ using
Fuchsia's **`driver-lab` embedded library** (`driver_lab_rust::embedded` / `in-situ` mode)
and companion host tooling (`driver-lab.pyz` / `DriverLab`).

> **Note on Mode C Bugs:** Do not invoke `hardware-prober` to shoe-horn `driver-lab` into pure logic, parser, or deterministic lifecycle bugs (`Mode C`) that are faster and more directly verified via `fx test` or `ffx log`.

---

## Critical Runtime Rules

1. **Working Directory:** Always execute `driver-lab.pyz` with `Cwd` set to `$(fx get-build-dir)` so bundled `fuchsia-controller` shared libraries and `host_x64/ffx` resolve cleanly:
   ```bash
   cd $(fx get-build-dir)
   DRIVER_LAB="python3 host_x64/obj/tools/driver-lab/driver-lab.pyz"
   ```
2. **Component Moniker vs. Node ID:**
   * `driver-lab list --debug-capable` returns each node's `moniker` (e.g. `"spi-0"`).
   * When invoking `describe`, `inspect`, or `run` in-situ, pass `--node "spi-0"` and the full component moniker `--moniker "bootstrap/base-drivers:spi-0"` (or `"bootstrap/full-drivers:<node>"` if `:` is not already present).
3. **Grant Digest Resolution (`per-resource digest`):**
   * When persisting read grants via `driver-lab permissions add`, always pass the **per-resource digest** (`resources[i].digest` from `describe` or `target.description.json`) for the specific resource (`"mmio0"` or `"state0"`), which `driver-lab run` checks on each read (`mmio_read32`, `state_read32`, `state_poll32`).
   * If also running `permissions explain --plan`, add a grant entry with the node bundle `resource_digest` as well.
4. **No Persistent Write Grants:**
   * Persistent write grants in `grants.toml` are rejected by design.
   * When executing an operator-approved mutating plan (`mmio_write32`, `knob_write32`, or `trigger_write32`), pass `--consent` and pipe `"a\n"` per write operation to `stdin`.

---

## Phases 2 & 3 In-Situ Investigation & Verification Protocol

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
Use `inspect` for quick read-only register or state slot checks and to view the recent target audit ring without authoring a full plan:
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
When investigating concurrency/race or state-machine bugs via a `"state0"` `StateBank` (Rust) or `StateVmoBank` (C/C++), run a **Repro Plan** (`KNOB_FIX_ENABLED = 0`) followed immediately in the **same boot** by a **Fix Proof Plan** (`KNOB_FIX_ENABLED = 1`).

> **Mandatory Mode B Plan Schema Rule:** Always use the Phase 3 `StateBank` operation kinds (`knob_write32`, `trigger_write32`, `state_read32`, `state_poll32`) when targeting `"state0"`. **Never** use `mmio_read32`, `mmio_write32`, or `mmio_poll32` with `"resource": "state0"` (`driver-lab plan validate` rejects `mmio_*` operations on `state*` resources). Both `before_fix_probe.json` and `after_fix_probe.json` in Mode B should include `knob_write32` (setting fault/fix knobs), `trigger_write32` (or workload trigger), and `state_read32` / `state_poll32`.

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
1. **Refresh Identity:** Re-run `describe` first! A target reboot or `ffx driver restart` changes `boot_id` and/or `resource_digest` (if ceiling parameters or `StateBank` slots changed). Update `plan.json` and `grants.toml` if the resource digest changed.
2. **Run Verification Plan:** Execute the verification `plan.json` against the patched driver.
3. **Verify Audit Trail:** Inspect `evidence/<run_id>/target-audit.jsonl` and confirm:
   * Every expected register or `state0` read/write has `decision: "allowed"` and `status: "ok"`.
   * The observed values match the post-fix expectations in `bug_spec.md`.
   * If mutating operations ran, `quiesce_engaged` and `quiesce_released` bracketed the writes.
4. **Verify System Logs:** Check `evidence/<run_id>/serial.log` and `ffx log dump` to confirm no driver errors, panics, or bug symptoms remain.
5. **Report Structured Evidence:** Return the `run_id`, `plan_digest`, `manifest.json` SHA-256 hash, audit sequence range, and per-register/per-slot results to the orchestrator.
