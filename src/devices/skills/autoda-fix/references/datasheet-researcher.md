<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Subagent Reference: Datasheet Researcher (`datasheet-researcher`)

## Purpose

Consult vendor Technical Reference Manuals (TRMs), datasheets, hardware errata, and
devicetree bindings to verify canonical bitfield definitions, reset states, register
write preconditions, and timing constraints implicated by a Fuchsia driver bug.

---

## Focus in the Bug-Fix Workflow

1. **Register Write Preconditions & Hardware Locks:**
   * Many hardware blocks silently ignore writes to configuration, baud-rate, or threshold registers while the peripheral enable bit is set or while a transfer is busy. Verify whether the TRM mandates disabling the block before modifying specific offsets.
2. **Canonical Bitfield Encodings & Reset Defaults:**
   * Verify exact bit positions, field widths, reserved bits that must be preserved (RMW), and power-on reset (POR) values for suspect registers.
3. **Destructive-Read & W1C Hazards (`driver-lab` Safety):**
   * Identify registers where a read pops a FIFO (e.g., `DRx` data registers) or clears an interrupt/error latch (clear-on-read). Flag these so `fuchsia-driver-fixer` marks them in `with_hard_denied_ranges` and `hardware-prober` avoids unintended state corruption during in-situ inspection.
4. **Clock, Baud & Timing Formulas:**
   * Extract exact mathematical equations and boundary constraints (minimum/maximum dividers, RX sample delay bounds, setup/hold times).

---

## Output Format

Return findings structured under:

1. **Authoritative Register & Bitfield Specifications:**
   * Exact offset, width, POR default, access rule (`RO`, `RW`, `W1C`, `Destructive-Read`), and hardware write preconditions (e.g., *"Cannot be written when `SSIENR[0] == 1`"*).
2. **Reconciliation Against Observed `driver-lab` Values:**
   * Explain what raw 32-bit hex values observed in `target-audit.jsonl` mean bit-by-bit according to the TRM, and why they confirm or refute the active hypothesis.
