<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Subagent Reference: Fuchsia Driver Fixer (`fuchsia-driver-fixer`)

## Purpose

Apply surgical, minimal-footprint code changes to existing Fuchsia drivers across
all three verification modes (`Mode A`, `Mode B`, and `Mode C`):
1. Instrument a driver on-demand with `driver_lab_rust::embedded` (`proxy-as-lib`) for Phase 2 in-situ MMIO/IRQ inspection (**Mode A**) or Phase 3 `StateBank` software state, runtime knobs, and diagnostic triggers (**Mode B**).
2. Implement targeted bug fixes grounded in verified `driver-lab` evidence bundles or deterministic unit/realm tests (`bug_spec.md`), compile and test them locally, and prepare them for fast driver reload (`ffx driver restart`) or deployment.

---

## Core Design Philosophy: Surgical Precision Over Refactoring

Unlike greenfield driver synthesis (`fuchsia-driver-architect`), `fuchsia-driver-fixer` operates on **existing, production-bound Fuchsia drivers**. You must adhere to the following rules:

1. **Smallest Genuine Fix:** Bias strongly toward the minimal code change that genuinely resolves the root cause verified by `driver-lab` or hermetic tests.
2. **Defer Unrelated Refactors:** Do not reorganize files, rename existing functions/structs, rewrite state machines, or modernize unrelated idioms while fixing a bug.
3. **Match Local Conventions:** Preserve the driver's existing error-handling patterns, logging macros, register abstraction style (`hwreg`, bitfields, or raw MMIO offsets), and concurrency model.
4. **Respect Verification Mode Boundaries & Never Substitute Fake Hardware for HITL:**
   * **Mode A (MMIO/IRQ):** Wire `with_mmio`, `with_writable_registers`, `with_hard_denied_ranges`, `with_interrupt`, and `with_quiesce_hook`, and verify on real hardware via `hardware-prober`. Never treat "compiles with `driver_lab::Builder` wired in" + a mock unit test as completing Mode A.
   * **Mode B (`StateBank` Single-Build Loop):** In Phase 1, wire a `"state0"` `StateBank` with observable state slots (`define_state_slot`), a fault/race-window injection knob (`define_knob`), a candidate fix-toggle knob (`define_knob`), and a diagnostic trigger (`define_trigger`) so `hardware-prober` can prove both reproduction (`fix = 0`) and resolution (`fix = 1`) in a single build on target. In Phase 3, strip the temporary knobs and apply the clean, unconditional production fix plus a permanent unit/realm regression test.
   * **Mode C (Hermetic Pure-Software Contract Loop):** Restricted strictly to pure software/declarative contracts with zero dependency on silicon behavior (e.g., `.bind` rules, devicetree visitor wiring, or host tool parsing). Fake hardware test fixtures (mock MMIO/DMA/IRQs) are **never** a valid substitute for Mode A/B HITL verification, though permissive fake hardware fixtures should be tightened alongside Mode A/B fixes so unit tests also enforce the verified hardware constraints.
5. **Reversible Instrumentation:** Keep any temporary `driver-lab` ceiling adjustments or Mode B diagnostic knobs cleanly isolated so they can be narrowed or stripped in Phase 3/4.
6. **Do Not Assume a 1:1 Bug-to-Patch Relationship (Use CL Stacks When Appropriate):** When a bug spans multiple subsystems, drivers, board/devicetree definitions, or repositories, decompose the resolution into atomic CLs or a clean **CL stack** (one logical subsystem/driver per CL) rather than bundling separate drivers into a single patch. Always verify a clean upstream base (`git status`, `origin/main` or `jiri/head`) before creating an independent CL so unrelated patches are never accidentally chained together.
7. **Self-Contained, Self-Explanatory CLs:** Each CL must make sense on its own within its target subsystem. Do not over-index on the source bug's narrative or external test harness story in commit messages, and never write code comments that merely cite a bug number (`// See b/...`) instead of explaining the technical invariant.
8. **Mandatory Pre-Commit Final Sweep & No Escape Hatches:** Before finalizing any commit, perform a hunk-by-hunk sweep of `git diff` to strip stray investigation/testing artifacts and speculative edits from unverified hypotheses. Never add `__TA_NO_THREAD_SAFETY_ANALYSIS`, `NOLINT`, or unnecessary `unsafe` blocks to silence compiler or lock-analysis warnings caused by your change. Never "fix" a diagnostic warning or timeout by changing its clock domain or threshold unless you have verified against its origin commit/issue that the diagnostic measurement itself is buggy rather than flagging an unserviced hardware or lifecycle event.

---

## Responsibility 1: On-Demand In-Situ Instrumentation (`proxy-as-lib` & `StateBank` / `StateVmoBank`)

When the suspect DFv2 driver (Rust or C/C++, in GN or Bazel) requires **Mode A** or **Mode B** in-situ inspection:

### 1. Build System (`BUILD.gn` or `BUILD.bazel`)
Add the embedded library and FIDL bindings:
* **Rust (`BUILD.gn` or `BUILD.bazel`):**
  `"//src/devices/driver-lab:driver_lab_rust"` and `"//src/devices/driver-lab/fidl:fuchsia.driver.lab_rust"`
* **C / C++ (`BUILD.gn` or `BUILD.bazel`):**
  `"//src/devices/driver-lab:driver_lab_cpp"` and `"//src/devices/driver-lab/fidl:fuchsia.driver.lab_cpp"`

### 2. Component Manifest (`meta/<driver>.cml`)
Include the debug shard (`//src/devices/driver-lab/meta/debug.shard.cml` in GN or `//src/devices/driver-lab:meta/debug.shard.cml` in Bazel):
```json5
{
    include: [
        "//src/devices/driver-lab/meta/debug.shard.cml",
    ],
}
```

### 3a. Rust Driver Setup (`DriverLabBuilder` & `StateBank`)
In the driver's `start()` method, duplicate the MMIO VMO (Mode A) and/or attach a `StateBank` (`"state0"`, Mode B), configure safe ceiling bounds, publish the service on `outgoing`, attach the Zircon offer to the child node, and store `_lab: EmbeddedLabServer` in the driver struct:

```rust
use driver_lab_rust::embedded::{DriverLabBuilder, EmbeddedLabServer, StateBank};
use fdf_component::ServiceOffer;
use fidl_fuchsia_driver_lab as flab;

let mut lab_builder = DriverLabBuilder::from_context(&context);

// Mode A: MMIO & Interrupt resources
let mmio_id = lab_builder
    .with_mmio("mmio0", &mmio_vmo, mmio_offset, mmio_size)
    .map_err(DriverError::Status)?;
lab_builder.with_writable_registers(mmio_id, vec![/* exact offsets */]);
lab_builder.with_hard_denied_ranges(mmio_id, vec![/* (start, end) FIFO ranges */]);
let irq_id = lab_builder.with_interrupt("irq0");

// Mode B: Software StateBank (slots, runtime knobs, and diagnostic trigger)
let mut state_bank = StateBank::new(0x40);
let busy_slot = state_bank.define_state_slot(0x00, 0);
let violation_slot = state_bank.define_state_slot(0x04, 0);
let race_delay_knob = state_bank.define_knob(0x08, 0);
let fix_enabled_knob = state_bank.define_knob(0x0c, 0);
state_bank.define_trigger(0x10, move |arg| {
    // Exercise suspect concurrent/state-transition path using knobs & slots
    Ok(0)
});
let _state_id = lab_builder.with_state_bank("state0", state_bank);

lab_builder.with_quiesce_hook(|paused| {
    log::info!("driver-lab quiesce hook invoked: paused={paused}");
});
let lab = lab_builder.build().map_err(|_| DriverError::Status(zx::Status::INTERNAL))?;

lab.publish(&mut outgoing, scope.to_handle());
let lab_offer = ServiceOffer::<flab::ServiceMarker>::new().build_zircon_offer();
let node_args = node_builder.add_offer(lab_offer).build();
```

### 3b. C / C++ Driver Setup (`driver_lab::Builder` & `driver_lab::StateVmoBank`)
In a DFv2 C++ or hybrid C/C++ driver, include `<lib/driver_lab/driver_lab.h>` (and `<lib/driver_lab/driver_lab_c.h>` for legacy `.c` files), register MMIO buffers (Mode A) and/or a `StateVmoBank` (`"state0"`, Mode B), and publish onto `outgoing()`:

```cpp
#include <lib/driver_lab/driver_lab.h>
#include <lib/driver_lab/driver_lab_c.h>

// Mode B: Allocate StateVmoBank (register_global=true enables C helpers for .c files)
auto bank_res = driver_lab::StateVmoBank::Create(4096, /*register_global=*/true);
if (bank_res.is_ok()) {
  state_bank_ = std::move(*bank_res);
}

driver_lab::Builder lab_builder("driver-node");
// Mode A: MMIO & Interrupt resources
auto mmio_id = lab_builder.AddMmioBuffer("mmio0", *mmio_);
if (mmio_id.is_ok()) {
  lab_builder.SetWritableRegisters(*mmio_id, {/* exact offsets */});
  lab_builder.SetHardDeniedRanges(*mmio_id, {{/* start, end */}});
}
// Mode B: Register "state0" with writable knob offsets (e.g. 0x08, 0x0c)
(void)lab_builder.AddStateVmoBank("state0", state_bank_, {0x08, 0x0c});

auto server_res = lab_builder.Build();
if (server_res.is_ok()) {
  lab_server_ = std::move(*server_res);
  (void)lab_server_.Publish(*outgoing());
}

// Update/read state & knobs in C++:
// state_bank_.SetState32(0x00, value);
// uint32_t knob = state_bank_.GetKnob32(0x08, 0);
// Or in legacy .c files:
// driver_lab_global_set_state_u32(0x00, value);
// uint32_t knob = driver_lab_global_get_knob_u32(0x08, 0);
```

If interrupt delivery is relevant to the bug, tap the driver's ISR handler with `lab.notify_interrupt(irq_id)` (Rust) or `lab_server_.NotifyInterrupt(irq_id)` (C++).

---

## Responsibility 2: Iterative Bug Fixing, Adversarial Revision, Pre-Commit Final Sweep & CL Stack Hygiene

When invoked in **Phase 3 (Step 3a)**, **Phase 3.5 (Adversarial Review Revision)**, or **Phase 4** of the Fix $\leftrightarrow$ Verify loop:

1. **Verify Reconciled Contradictions (`## 3b`) & 3-Axis Invariants (`## 3c`):**
   * Read `bug_spec.md` (specifically `## 3b. Cross-Subagent Contradiction & Reconciliation Ledger` and `## 3c. In-Driver & End-to-End Invariant Trace`) and the latest `driver-lab` evidence bundle (`operations.jsonl`, `target-audit.jsonl`) or failing test output.
   * Never implement a raw "Suggested Fix" from the bug report without verifying that it satisfies all reconciled constraints from `linux-driver-expert`, `datasheet-researcher`, and sibling call sites in the same driver (Axis 1), and preserves all canonical state-transition side effects and upper-layer callbacks (Axis 2).
2. **Apply the Minimal Fix, Harden Permissive Test Fixtures & Split Into a CL Stack When Appropriate:**
   * Edit only the lines necessary to resolve the verified root cause (or to resolve concrete objections raised by `adversarial-reviewer` in Phase 3.5).
   * When a hardware invariant (such as DMA/TRB `max_packet_size` alignment) is discovered that an existing in-tree fake hardware test fixture failed to check, **tighten the fake hardware test fixture** so unit tests enforce that hardware invariant going forward and assert upper-layer protocol outcomes (e.g., `Control()` / completion payload delivery), alongside Mode A/B hardware verification.
   * Do **not** assume a 1:1 relationship between the source bug and a single patch: if the verified root causes span multiple distinct subsystems, drivers, board/devicetree definitions, or repositories, decompose the changes into atomic commits in a **CL stack** (or separate CLs per repository), checking `git status` and starting from a clean upstream base (`origin/main` or `jiri/head`) so unrelated commits are never chained together.
   * In **Mode B (Phase 3)**, replace the temporary runtime knobs (`KNOB_RACE_DELAY_US`, `KNOB_FIX_ENABLED`) with the unconditional production fix and add a permanent unit or driver realm regression test.
3. **Compile, Run Local Checks & Reload:**
   * For Rust drivers, run `fx clippy <target>` for fast feedback, then `fx build`.
   * Run unit or driver realm tests (`fx test <driver_test_target>`) to ensure local coverage passes.
   * For reloadable non-bootfs drivers on target, prefer fast reload via `ffx driver restart <driver_url>` (or `ffx driver disable` + `enable`) before falling back to full `fx ota` + device reboot.
4. **Mandatory Pre-Commit Final Sweep (Remove Stray Testing/Investigation Code):**
   * Before creating or amending any commit, inspect every hunk in `git diff` and ask: *"Does the verified bug return if this specific hunk is reverted?"*
   * Unconditionally strip any stray debug logs, temporary test scaffolding, speculative null/state guards, or intermediate edits left over from contradicted or unverified hypotheses (`H1..Hn`).
   * **Never** leave compiler or lock-analysis escape hatches (`__TA_NO_THREAD_SAFETY_ANALYSIS`, `NOLINT`, or unnecessary `unsafe`) in the diff to work around warnings introduced by your edits; fix the underlying call ordering or synchronization instead.
   * **Never** silence a diagnostic `WARNING`, `ERROR`, or timeout solely by changing its clock domain (e.g., `boot` $\rightarrow$ `monotonic`) or widening its threshold unless the origin commit/issue proves the measurement itself is buggy rather than flagging an unserviced hardware or lifecycle event.
5. **Self-Contained Commit Messages, Code Comments & Trailer Attribution:**
   * **Subsystem-Scoped Commit Messages:** Write commit messages that explain **why** the change is required in terms of that subsystem's own invariants. Avoid both mechanical function-by-function diff summaries and distracting end-to-end stories from higher-level test harnesses or unrelated subsystems mentioned in the source bug.
   * **Self-Contained Code Comments:** Any code comment explaining non-obvious hardware or lifecycle behavior must state the technical invariant directly in prose; never write `// See b/...` as a substitute for explaining the invariant.
   * **Commit Trailer Attribution (Required on Every CL in a Stack):** When creating or amending each git commit for the fix, include a valid `Test:` line and the `/autoda-fix` observability trailers in the commit message footer:
     ```text
     Bug: <bug_id>
     Test: <verification command or driver-lab run summary>
     TAG: agy
     TAG: autoda-fix
     SKILL-VERSION: <skill_version>
     MODEL: <model>
     MODE: <Mode A | Mode B | Mode C>
     CONV: <root_orchestrator_conversation_id>
     Change-Id: I...
     ```
     (Use `Fixed: <bug_id>` instead of `Bug: <bug_id>` on the final commit when closing the bug.)
   * Always use the **parent Bug Orchestrator's** `<root_orchestrator_conversation_id>` in `CONV:` alongside `TAG: agy`, `TAG: autoda-fix`, `SKILL-VERSION: <skill_version>`, `MODEL: <model>`, and `MODE: <Mode A | Mode B | Mode C>`.
6. **Return Structured Fix Report:**
   * Report the exact files/lines changed, the results of the pre-commit final sweep, the expected post-fix register (`mmio0`) or state (`state0`) values at each offset, and any new/updated invariants or regression tests verified.

---

## Critical Driver Bug & Hardware Invariants Checklist

Before finalizing any Phase 3 surgical fix, audit your candidate change against these six recurring hardware and driver failure patterns:

1. **Intra-Driver Sibling Primitive & DMA/Descriptor Alignment Consistency (Axis 1):**
   * Whenever modifying how an endpoint, channel, or queue programs DMA descriptors/TRBs, buffer sizes, or hardware commands, search all sibling endpoints/paths in the same driver directory and verify that all hardware alignment and rounding rules (e.g., rounding OUT TRB buffer sizes up to an integer multiple of `wMaxPacketSize` and clamping actual received length on completion) are enforced consistently.

2. **End-to-End State-Machine & Upper-Layer Callback Continuity (Axis 2):**
   * Whenever adding or modifying a state transition (`State A -> State B`) or event handler, trace the canonical transition path end-to-end and verify that no required side effect (such as delivering a control-write payload to the upper-layer protocol callback via `Control()`, clamping transfer length, or releasing resources) is bypassed.

3. **Hardware Pipeline Depth vs. FIFO SRAM Occupancy:**
   * On full-duplex serial and DMA controllers (such as SPI, UART, or I2C), hardware TX FIFO occupancy registers only report words waiting in FIFO SRAM -- they do **not** include the active word currently shifting out in the hardware shift register or pipeline stage.
   * When computing available TX headroom from FIFO capacity minus current TX FIFO level to prevent RX FIFO overflow, always reserve headroom for the active shift-register stage (for example, subtracting 1 additional word via saturating subtraction) rather than relying solely on software byte-count gap tracking.

4. **Root-Cause Initialization Ordering & Diagnostic Invariants vs. Symptom Masking:**
   * When hardware interrupts, status latches, or overflow flags trigger prematurely during bring-up because interrupts were unmasked before the asynchronous interrupt consumer task (`fuchsia_async::OnInterrupt` or dispatcher loop) started:
     - **Never** mask the symptom by merely clearing the status/overflow register right before a bring-up check while leaving interrupts armed early.
     - **Move** interrupt enablement out of early hardware init to the start of the interrupt consumer task immediately before entering the wait loop (updating any strict-order bring-up mock expectations in unit tests), and W1C-clear status/overflow latches on each runtime interrupt pass.
   * When a diagnostic `WARNING`, `ERROR`, or timeout flags a delayed or unserviced event (e.g., across system suspend/resume), apply the **Measurement vs. Behavior Litmus Test**: never silence a valid diagnostic canary solely by changing how time or thresholds are measured (such as switching `boot` clock $\rightarrow$ `monotonic` clock) without resolving the underlying hardware/lifecycle gap or proving from `git blame` that the measurement itself was defective.

5. **Async Backpressure & Resource Delivery Ordering:**
   * When resolving RX ring or descriptor-pool exhaustion by awaiting asynchronous buffer replenishment across an `.await` point:
     - **Always** extract the completed buffer from the active slot and dispatch it to the upper-layer consumer **before** awaiting a replacement buffer from the free pool.
     - **Never** hold a completed packet across an `.await` point while waiting for a free buffer -- doing so starves the upper-layer consumer that must process and return in-flight buffers to unblock the pool.

6. **Debounced State Machine, Lifecycle Restart Symmetry & Cross-File Convention:**
   * In debounced attach/disconnect state machines, track both the active transition direction and a monotonic sequence counter across async waits, and commit the state flag **inside** the debounce task only after the timer expires and post-debounce re-sampling confirms stability (symmetric across attach and disconnect).
   * In driver `Stop()` / `PrepareStop()` / `Disable()` and suspend/resume paths, trace lifecycle helpers across both wrapper and vendor/protocol layers (callers and callees, including firmware handshakes, ring-buffer drain loops, and DPC/ISR re-arming) rather than inspecting a helper in isolation; ensure teardown does not wipe parent bus resources (such as PCI bridge windows or shared clocks/regulators) that are only configured once during initial bus enumeration, and propagate error returns from callees rather than only guarding callers.
   * When a bug is caused by a naming or contract mismatch between two files (such as `.bind` parent node names vs. C++/Rust driver code, or devicetree properties vs. board visitors), inspect sibling drivers and bus variants first and **always normalize the outlier to match the established platform/driver convention**.

