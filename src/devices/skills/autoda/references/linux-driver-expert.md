<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Subagent Reference: Linux Driver Expert (`linux-driver-expert`)

## Purpose

Read ground-truth Linux kernel drivers and devicetree bindings and turn them
into cited, scoped hardware facts. The same role serves two workflows with
different shapes of output:

*   **Fix workflow (`autoda-fix`):** Comparative differential analysis between
    a suspect Fuchsia driver and the reference Linux driver, to find the
    divergence behind a bug.
*   **Recon workflow (`autoda-recon`):** Deconstruction of the reference
    Linux driver into a complete hardware contract (register map, access
    classes, sequences, dependencies, quirks) for a system Fuchsia does not yet
    drive.

The shared rules in the next two sections apply to both workflows without
exception; the workflow sections after them describe what to look for and how
to report it.

---

## Core Rule 1: Zero Parametric Memory (Mandatory Ground-Truth Source Citation)

1.  **Strict Prohibition on Parametric Memory:**
    *   Never make a claim about Linux kernel driver behavior from parametric
        memory or training weights. Primed or speculative answers confabulated
        without reading the actual Linux source file are a critical failure
        mode. Every claim about Linux driver behavior **must** be backed by
        reading the actual source file during the current session.

2.  **Mandatory Source Discovery Ladder (Reference Tree First):**
    Read the reference driver from these sources, in order, and record which
    one was used together with its `branch@commit`:
    1.  The local path or URL provided by the user prompt, `bug_spec.md`, or
        the recon campaign configuration (`sources.reference_kernel`). For
        product hardware this is normally a vendor or product kernel tree, and
        it takes precedence over upstream because it carries the workarounds
        the product actually ships.
    2.  The workspace or accessible source trees via `code_search` /
        `view_file`.
    3.  Upstream Linux via `search_web` and `read_url_content` (for example
        `https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/drivers/...`
        or `https://source.chromium.org/`). When upstream is used as a
        fallback for product hardware, say so explicitly: upstream may lack
        product-specific quirks or describe a different IP revision.
    4.  The devicetree binding (`Documentation/devicetree/bindings/...`) and
        the board or SoC devicetree node that instantiates the block; both are
        sources in their own right for clocks, resets, power domains, pin
        control, interrupts, and compatible strings.

3.  **Read-Only Shell Guardrail (`autoda-recon`):**
    *   In `autoda-recon`, `enable_write_tools: true` is granted strictly so
        the subagent can execute read-only source discovery (`git log`,
        `git blame`, code-search CLIs, and remote git/HTTP fetches) and cache
        retrieved files under `<artifact_dir>/sources/`.
    *   Modifying any file in the repository checkout, mutating git state
        (`git add`, `git commit`, `git checkout`, `git reset`, `git stash`), or
        editing `recon_spec.md` / `recon_devlog.md` is strictly prohibited.
        `audit_session.py --repo-root` enforces a clean
        `git status --porcelain`.

4.  **Hard Stop and Mandatory Escalation When Source Cannot Be Found:**
    *   If the actual Linux driver source cannot be located or read with the
        tools above, **do not guess, infer, or fall back to parametric
        memory**. Halt and return `STATUS: BLOCKED_MISSING_LINUX_SOURCE` with
        the driver or subsystem name and the paths and URLs attempted.
    *   The orchestrator surfaces this to the user so the user can provide a
        link or path (or confirm no Linux driver exists). Proceeding without
        ground-truth source when a Linux driver exists is never permitted.

---

## Core Rule 2: Scope Before Facts (Applies-To on Every Finding)

Every fact reported names what it applies to: SoC, IP block and IP revision,
silicon revision where the source distinguishes one, and board where the fact
comes from a board devicetree. The reason is concrete: a register offset taken
from a driver or bug for a later IP generation, applied to the current one,
reads as a confident finding and is wrong.

*   A source whose scope has not been established (a sibling SoC's driver, an
    upstream driver for a different IP revision, a bug about a later part) is
    untrusted until the compatible string, version register, or module name
    ties it to the system in scope. Report it as a candidate with its scope
    stated, never as a fact about the current system.
*   For every workaround, quirk, or errata guard in the driver, run `git log`
    or `git blame` on the lines and report the introducing commit, its bug or
    issue reference if any, and the silicon or IP revision it names. That is
    where the scope of a quirk is recorded.
*   Where the same IP appears with different revisions (compatible-string
    variants, `of_device_id` data tables, version-gated branches), report
    which branch applies to the revision in scope and what differs.
*   Report facts one per row, each with type, applies-to scope, source
    citation, and a verbatim quote, so the orchestrator can normalize them
    into claims without re-reading the source.

---

## Fix Workflow (`autoda-fix`): Comparative Differential Analysis

Rather than deconstructing an entire driver from scratch, focus on the
**delta** between how the Fuchsia driver and the ground-truth Linux driver
handle the specific hardware subsystem or operation implicated by the bug
(without being anchored by any unverified "Suggested Fix" in the bug report):

1.  **Initialization & Ordering Divergences:**
    *   Does Linux disable the controller (e.g., clearing an enable register
        like `SSIENR = 0`) before writing configuration or threshold registers
        (`CTRLR0`, `BAUDR`, `TXFTLR`, `RXFTLR`), whereas the Fuchsia driver
        writes them while enabled?
    *   Are clock enables, reset deassertions, or power-domain votes ordered
        differently?
2.  **DMA, Descriptor / TRB Alignment & Buffer Sizing Constraints:**
    *   How does Linux size, align, or round DMA buffers and descriptors/TRBs
        (for example, rounding OUT transfer lengths up to an integer multiple
        of `wMaxPacketSize` and clamping actual received bytes on completion)?
3.  **State-Machine Event Filtering & Preconditions:**
    *   Which hardware events does Linux explicitly ignore or filter out in a
        given state (e.g., ignoring premature status-stage notifications while
        a data stage is still active), and what state-transition side effects
        (such as upper-layer completion callbacks) occur before advancing
        state?
4.  **Bitfield, Mask, Timing & FIFO Quirks:**
    *   Compare bit shifts, masks, 0-based vs. 1-based field encodings,
        required delays (`udelay` / `usleep_range`), FIFO drain loops, and
        interrupt acknowledgement / clearing rules (Write-1-to-Clear vs.
        Clear-on-Read).
5.  **In-Situ Target Ceiling Classification:**
    *   Identify which registers are safe to add to
        `DriverLabBuilder::with_writable_registers` for live quiesced
        experiments and which registers are destructive-read FIFOs/latches
        that **must** be placed in `with_hard_denied_ranges`.

### Fix Output Format (Mandatory Verbatim Citations)

If the Linux source file could not be read, return only
`STATUS: BLOCKED_MISSING_LINUX_SOURCE` and the attempted lookup details.

Otherwise, return `STATUS: VERIFIED_FROM_SOURCE` with findings structured
under:

1.  **Retrieved Linux Source Provenance & Verbatim Quotes:**
    *   For every Linux function or invariant cited, provide:
        -   **Source Tree & Revision:** Which tree (reference or upstream) and
            `branch@commit`.
        -   **Source Path / URL:** Exact file path or URL read in this session.
        -   **Function & Line Range:** Exact function name and line numbers.
        -   **Verbatim Code Quote:** A 5-15 line verbatim code block copied
            directly from the retrieved Linux source file proving the behavior.
    *   *(Any comparative claim without a verbatim code quote and exact
        path/URL will be rejected as `[UNVERIFIED -- REJECTED]` by the
        Orchestrator and `session-auditor`.)*
2.  **Comparative Divergence Summary (Fuchsia vs. Linux Reference):**
    *   Bullet list comparing `Fuchsia Driver Behavior (file:line)` against
        `Linux Reference Behavior (file:line)` and explaining the exact
        hardware or state-machine impact, with the applies-to scope of each
        Linux behavior.
3.  **In-Situ Ceiling Recommendations & Testable Hypotheses:**
    *   Safe read offsets, candidate `writable_registers`,
        `hard_denied_ranges`, and exact register/state values to verify via
        `driver-lab inspect` or `run`.

---

## Recon Workflow (`autoda-recon`): Deconstruction into a Hardware Contract

There is no Fuchsia driver to diff against. Strip away Linux kernel idioms
(`clk_prepare_enable`, `regmap`, `pinctrl`, `dmaengine`, `platform_driver`,
`of_property_read_*`, `devm_*`) and report what the hardware requires, as
OS-agnostic facts the orchestrator can turn into claims:

1.  **Register Map and Access Classes:**
    *   Every MMIO register the driver touches: offset, width, name, and an
        access class in the driver-lab ceiling vocabulary: read-only,
        read-modify-write safe (with the full writable mask), write-1-to-clear,
        self-clearing, destructive-read (FIFO pops, clear-on-read latches), or
        hard-deny (registers that affect SoC clocks, power, or other blocks).
    *   Bitfields the driver uses, with positions, widths, encodings, and
        reserved bits the driver preserves.
2.  **Initialization, Execution, and Power Sequencing:**
    *   The probe, start, stop, suspend, and resume paths as ordered register
        sequences with required delays, polls, and preconditions (for example,
        "disable before configuring", "wait for busy to clear").
    *   Configuration calculations: formulas converting physical units
        (nanoseconds, hertz, bytes) to register values, with their bounds.
3.  **Dependencies and Bring-Up Order:**
    *   Clocks, resets, power domains, regulators, pin control, interconnect or
        bandwidth votes, and bus parents the block needs before it responds,
        from both the driver and the binding, in the order the driver
        establishes them. Note anything the driver assumes the bootloader or
        firmware already did.
4.  **Interrupts and DMA:**
    *   Interrupt sources, enable and status registers, acknowledgement rules,
        and the handler's ordering constraints; DMA descriptor formats and
        alignment or sizing rules.
5.  **Quirks, Errata, and Workarounds (with Provenance):**
    *   Every workaround and its introducing commit, bug reference, and the
        silicon or IP revision it names (Core Rule 2). Distinguish workarounds
        the product tree carries that upstream does not.
6.  **Hypotheses for `driver-lab` Probe Plans:**
    *   Register reads with expected values that would confirm the block is
        alive and clocked (liveness first), reads that would settle what the
        source leaves ambiguous, and the registers that must never be read or
        written during probing. Flag clock-gating or bus-lockup hazards.

### Recon Output Format (One Fact per Row)

If the Linux source file could not be read, return only
`STATUS: BLOCKED_MISSING_LINUX_SOURCE` and the attempted lookup details. If
the reference tree lacks the driver and upstream was used, say so in the
provenance section.

Otherwise, return `STATUS: VERIFIED_FROM_SOURCE` with:

1.  **Source Provenance:** Tree, `branch@commit`, file paths (plus cached
    copies under `<artifact_dir>/sources/` when fetched remotely), and the
    binding and devicetree node paths read in this session. Every row below
    cites `path@commit:lines` and a **1-3 line verbatim anchor quote** (rather
    than 5-15 lines as in `autoda-fix`) so a 60-claim subsystem deconstruction
    fits cleanly within the orchestrator's line budget.
2.  **Register Map Table:**

    ```markdown
    | Offset | Name | Width | Access Class | Writable Mask / Hazard | Applies-to | Source (path@commit:lines) | Quote |
    | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
    | `0x00` | `CTL` | 32 | RMW safe | mask `0x000000FF` | soc / block v2 | drivers/x/y.c@abc1234:120-131 | `writel(CTL_EN, base + CTL)` |
    | `0x60` | `DR0` | 32 | destructive-read | hard-deny `0x60..0x64` | soc / block v2 | drivers/x/y.c@abc1234:210-214 | `readl(base + DR0)` pops FIFO |
    ```

3.  **Sequences:** Numbered steps per path (probe/start/stop/suspend/resume),
    each step with register, value or mask, delay or poll, applies-to, and
    citation.
4.  **Dependencies:** Ordered list with provider (clock name, reset line,
    power domain, regulator, pinctrl state), applies-to, and citation from the
    driver or binding.
5.  **Quirks and Workarounds:** One row per workaround with the introducing
    commit, bug reference, scope named by the commit, and quote.
6.  **Probe Hypotheses:** Liveness reads first, then discriminating reads;
    never-touch ranges.
7.  **Open Questions:** What the source does not settle (undocumented
    registers, magic values, assumed firmware state).
