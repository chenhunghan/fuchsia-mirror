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
4. **Respect Verification Mode Boundaries:**
   * **Mode A (MMIO/IRQ):** Wire `with_mmio`, `with_writable_registers`, `with_hard_denied_ranges`, `with_interrupt`, and `with_quiesce_hook`.
   * **Mode B (`StateBank` Single-Build Loop):** In Phase 1, wire a `"state0"` `StateBank` with observable state slots (`define_state_slot`), a fault/race-window injection knob (`define_knob`), a candidate fix-toggle knob (`define_knob`), and a diagnostic trigger (`define_trigger`) so `hardware-prober` can prove both reproduction (`fix = 0`) and resolution (`fix = 1`) in a single build. In Phase 3, strip the temporary knobs and apply the clean, unconditional production fix plus a permanent unit/realm regression test.
   * **Mode C (Hermetic Test & Log Loop):** Do **not** shoe-horn `driver-lab` instrumentation into pure logic, parser, or deterministic lifecycle bugs. Fix the bug directly and verify with a unit/realm test (`fx test`) or `ffx log`.
5. **Reversible Instrumentation:** Keep any temporary `driver-lab` ceiling adjustments or Mode B diagnostic knobs cleanly isolated so they can be narrowed or stripped in Phase 3/4.

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

## Responsibility 2: Iterative Bug Fixing & Fast Reload

When invoked in **Phase 3 (Step 3a)** of the Fix $\leftrightarrow$ Verify loop:

1. **Review the Evidence-Backed Root Cause:**
   * Read `bug_spec.md` and the latest `driver-lab` evidence bundle (`operations.jsonl`, `target-audit.jsonl`) or failing test output.
   * Confirm the exact register offset, bitmask, sequence order, synchronization lock/guard, state transition, or interrupt handling bug to fix.
2. **Apply the Minimal Fix:**
   * Edit only the lines necessary in the driver source.
   * In **Mode B (Phase 3)**, replace the temporary runtime knobs (`KNOB_RACE_DELAY_US`, `KNOB_FIX_ENABLED`) with the unconditional production fix and add a permanent unit or driver realm regression test.
3. **Compile, Run Local Checks & Reload:**
   * For Rust drivers, run `fx clippy <target>` for fast feedback, then `fx build`.
   * Run unit or driver realm tests (`fx test <driver_test_target>`) to ensure local coverage passes.
   * For reloadable non-bootfs drivers on target, prefer fast reload via `ffx driver restart <driver_url>` (or `ffx driver disable` + `enable`) before falling back to full `fx ota` + device reboot.
4. **Commit Trailer Attribution (When Creating or Amending a CL):**
   * When creating or amending a git commit for the surgical fix, include the `/autoda-fix` observability trailers in the commit message footer:
     ```text
     Bug: <bug_id>
     Test: <verification command or driver-lab run summary>
     TAG=agy
     TAG: autoda-fix
     CONV: <root_orchestrator_conversation_id>
     Change-Id: I...
     ```
     (Use `Fixed: <bug_id>` instead of `Bug: <bug_id>` when closing the bug.)
   * Always use the **parent Bug Orchestrator's** `<root_orchestrator_conversation_id>` in `CONV:` (or include both the root orchestrator and subagent `CONV:` lines) alongside `TAG=agy` and `TAG: autoda-fix`.
5. **Return Structured Fix Report:**
   * Report the exact files/lines changed, the expected post-fix register (`mmio0`) or state (`state0`) values at each offset, and any new/updated invariants or regression tests verified.

---

## Critical Driver Bug & Hardware Invariants Checklist

Before finalizing any Phase 3 surgical fix, audit your candidate change against these four recurring hardware/driver failure patterns:

1. **FIFO Level Register vs. Shift-Register Pipeline Depth (`TXFLR`):**
   * On full-duplex serial controllers (such as DesignWare SSI SPI, UART, or I2C), hardware TX FIFO level registers (`TXFLR` / `tx_words`) only count entries sitting in TX FIFO SRAM -- they **do not** count the active word currently shifting out in the hardware TX shift register.
   * When computing `tx_free` from `FIFO_SIZE - tx_words` to bound TX refills and prevent RX FIFO overflow, always subtract 1 word for the shift register:
     ```rust
     let tx_free = (FIFO_SIZE - tx_words).saturating_sub(1);
     ```
   * Do not rely solely on software `rx_remaining - tx_remaining` gap tracking, which equals `0` on the initial burst when 1 word has already moved from the TX FIFO into the shift register while `tx_words` reads `0`.

2. **Premature Interrupt Arming vs. Symptom Clearing (Root-Cause Ordering):**
   * When hardware interrupts or overflow registers (e.g., `INTR_OVERFLOW`) latch during bring-up because interrupts were unmasked before the asynchronous interrupt consumer task (`fuchsia_async::OnInterrupt`) was spawned:
     - **Never** mask the symptom by merely clearing the status/overflow register right before a bring-up diagnostic check while leaving interrupts armed early.
     - **Remove** the premature interrupt-enable call from early bring-up (and update any strict-order bring-up MMIO mock expectations in unit tests to remove those early enable writes), and **move** the interrupt-enable call to the start of the async interrupt consumer task immediately before entering the `irq.next().await` loop.
     - Also read and W1C-clear the interrupt overflow register on every interrupt pass inside the runtime IRQ loop (logging a warning if frame or transfer completion events overflowed).

3. **Ring-Buffer & Descriptor-Pool Exhaustion Backpressure Ordering:**
   * When fixing RX ring or buffer-pool exhaustion by awaiting async buffer return (e.g., replacing synchronous `free_rx_buffers.pop()` with `free_rx_buffers.pop_wait_available().await`):
     - **Always** `.take()` the completed buffer out of the active descriptor slot (`let old_buf = self.active_rx_buffers[idx].take().unwrap();`) and dispatch `old_buf` to the upper-layer consumer **before** awaiting a replacement buffer (`let new_buf = self.free_rx_buffers.pop_wait_available().await; self.active_rx_buffers[idx] = Some(new_buf);`).
     - **Never** await a replacement buffer *before* extracting and delivering `old_buf` -- holding a completed RX packet across an `.await` point starves upper-layer consumers that must process and return in-flight buffers to unblock the pool.

4. **Debounced State Machine Symmetry:**
   * In debounced attach/disconnect or plug/unplug state machines (e.g., Type-C TCPC CC debounce):
     - Track both the active debounce direction (`Attach` vs. `Disconnect`) and a monotonic sequence counter (`debounce_seq`) across async `.await` points, and clear `debounce_task` upon completion if the sequence is still current.
     - **Never** mutate the committed connection state flag (such as `telemetry.is_connected.store(true, ...)`) *before* spawning the attach debounce task. Commit `is_connected = true` **inside** the attach debounce task only after the debounce timer expires and post-debounce re-sampling confirms the connection is stable -- symmetric with the disconnect debounce path.

