---
name: autoda-recon
description: >
  Deconstruct a hardware system's reference Linux driver, devicetree binding,
  vendor documentation, and bug history into scoped, cited hardware claims,
  then verify the claims that documents cannot settle on target silicon with
  driver-lab, without writing or landing a Fuchsia driver. Use when gathering
  hardware context ahead of a future driver port, building a per-system
  hardware contract, or running unattended reconnaissance campaigns across
  many systems. Do not use to diagnose or fix a bug in an existing Fuchsia
  driver (use autoda-fix).
version: 1.0.0
---

# Skill: AutoDA Recon (`autoda-recon`)

## Purpose

Produce, for one hardware system, a hardware contract that a later driver
implementation can start from: every register, bitfield, access class, sequence,
dependency, and quirk expressed as a claim that names the silicon it applies to,
cites the source it came from, carries a confidence tier, and, where hardware
verification is enabled, cites the driver-lab evidence that confirmed it. The
workflow ends at the contract. It never writes, modifies, or lands a Fuchsia
driver.

`autoda-recon` is the reconnaissance entry point of the AutoDA skill family.
Role specifications, the subagent registry, the compliance auditor, and the
telemetry logger are shared with
[`autoda-fix`](/src/devices/skills/autoda-fix/SKILL.md) and live in the core
directory [`/src/devices/skills/autoda/`](/src/devices/skills/autoda/README.md),
referred to below as `<path_to_autoda_core>`. Every shared script takes
`--workflow recon`.

Differences from `autoda-fix` that shape everything below:

* The subject is a system, not a bug. There is no Fuchsia driver to diff against
  and no suggested fix to quarantine; instead, every source is untrusted until
  its scope (SoC, IP revision, silicon revision) is established.
* The session runs unattended. Standing decisions come from a campaign
  configuration
  ([`campaign-config.md`](/src/devices/skills/autoda/references/campaign-config.md)),
  not from per-session questions, so many systems can be processed in parallel
  with one orchestrator per system.
* The target node is usually unclaimed, so hardware access defaults to
  driver-lab proxy mode rather than in-situ instrumentation of a running driver.

---

## 1. Subagent-Centric Architecture (Hub & Spokes)

The primary agent acts as the **Recon Orchestrator (Hub)**. It owns the
artifacts, establishes scope, routes work to roles, reconciles contradictions,
and writes claims. It does not read Linux source, documents, or bug threads
itself when a role exists for that.

```text
                      ┌──────────────────────────────────────────┐
                      │          Recon Orchestrator (Hub)        │
                      │ • Owns recon_spec.md & recon_devlog.md   │
                      │ • Loads campaign config; sets scope      │
                      │ • Normalizes reports into claims         │
                      │ • Reconciles contradictions; runs gates  │
                      └───┬──────────────┬──────────────┬────────┘
                          │              │              │
        ┌─────────────────┘              │              └──────────────────┐
        ▼                                ▼                                 ▼
┌────────────────────────┐   ┌────────────────────────────┐   ┌────────────────────────┐
│ Ground Truth           │   │ hardware-prober            │   │ Audit & Falsification  │
│ • linux-driver-expert  │◄─►│ • proxy mode on unclaimed  │   │ • session-auditor      │
│   (deconstruction;     │   │   nodes; in-situ fallback  │   │   (static & verified   │
│    zero parametric     │   │ • liveness first, deps     │   │    gates)              │
│    memory)             │   │   gated, state_origin tag  │   │ • adversarial-reviewer │
│ • datasheet-researcher │   │ • hashed evidence bundles  │   │   (high-blast-radius   │
│   (whole-block maps)   │   └────────────────────────────┘   │    claims only)        │
└────────────────────────┘                                    └────────────────────────┘
```

### Subagent Setup

1.  **Multi-Agent Mode (Recommended):** If `define_subagent` and
    `invoke_subagent` are available, read
    [`register_subagents.py`](/src/devices/skills/autoda/scripts/register_subagents.py)
    and define any missing roles from `register_subagents.py --workflow recon`.
    The recon workflow registers `linux-driver-expert`, `datasheet-researcher`,
    `hardware-prober`, `session-auditor`, and `adversarial-reviewer`, each with
    a recon addendum appended to its shared prompt. In recon mode,
    `linux-driver-expert`, `datasheet-researcher`, and `session-auditor` set
    `enable_write_tools: true` strictly for read-only search/fetch CLIs, `git
    log` / `git blame`, caching retrieved files under `<artifact_dir>/sources/`,
    and running `audit_session.py`; repository checkout mutations are prohibited
    and checked via `--repo-root`. Role specifications are under
    `<path_to_autoda_core>/references/`.
2.  **Single-Agent Progressive Mode:** If subagent tools are unavailable,
    execute each phase directly while adopting the role constraints, run the
    auditor at both gates, and maintain both artifacts.

Steps that have no registered role yet (source location, dependency walking,
Fuchsia-precedent lookup, claim normalization) are performed by the
orchestrator, delegating bulk reading to a general research subagent when one is
available. Follow the rules for those steps in Phase 0 and Phase 1 exactly as if
a dedicated role existed.

Also read the companion [`driver-lab`
skill](/src/devices/skills/driver-lab/SKILL.md) for proxy and in-situ mode
mechanics, plan schema, grants, and evidence bundles.

---

## 2. Deliverables & Dual-Artifact Discipline

Maintain two living markdown files in the conversation artifact directory (or
`output.artifact_dir` from the campaign configuration). Both begin with a YAML
frontmatter block initialized in Phase 0 and finalized in Phase 4:

```yaml
---
skill: autoda-recon
skill_version: "1.0.0"
model: "<session_model_and_version>"
conversation_id: "<root_orchestrator_conversation_id>"
trajectory_id: "<trajectory_id_or_empty>"
campaign_id: "<campaign_id_or_empty>"
system_id: "<stable_system_identifier>"
system_name: "<human_readable_name>"
target_scope: "<board / SoC / IP block and revision / silicon revision>"
hardware_verification: "<enabled | disabled>"
outcome: "<IN_PROGRESS | STATIC_COMPLETE | HARDWARE_VERIFIED | BLOCKED>"
claims_total: 0
claims_verified: 0
claims_contradicted: 0
---
```

* **`system_id`**: A stable identifier for the system that stays constant across
  sessions and campaigns (for example `<board>-<ip-block>-<instance>`). It keys
  telemetry and must match between both artifacts.
* **`target_scope`**: Starts as the campaign `target_scope` and is narrowed to
  the IP block and revision once Phase 0 establishes them.
* **`outcome`**: `STATIC_COMPLETE` when the `static` gate passes and hardware
  verification is disabled; `HARDWARE_VERIFIED` when the `verified` gate passes;
  `BLOCKED` when a blocking status marker could not be resolved.

### 2.1 `recon_spec.md` (Living Hardware Contract)

The single source of truth for the system, with the following mandatory headings
(enforced by `audit_session.py --workflow recon`):

* **`## 1. System Identity & Scope`:** SoC, IP block, IP revision, silicon
  revision, board, instance (base address, interrupt, bus parent), and the
  Fuchsia-precedent classification: an in-tree driver exists for the same IP
  family, for related IP, or none.
* **`## 2. Reference Sources & Provenance`:** Every source consulted, with tree
  and `branch@commit` or document revision, sensitivity class, and the role's
  status marker (`VERIFIED_FROM_SOURCE` / `BLOCKED_MISSING_LINUX_SOURCE`,
  `VERIFIED_FROM_DATASHEET` / `UNAVAILABLE_NO_DATASHEET_FOUND`). Sources
  excluded by the campaign fence are listed as excluded.
* **`## 3. Claims`:** The claim ledger. Format below.
* **`## 4. Dependencies & Bring-Up Order`:** Clocks, resets, power domains, pin
  control, regulators, and bus parents the block needs before it responds, in
  the order the reference driver establishes them, plus what the bootloader or
  firmware is already known to configure.
* **`## 5. Cross-Subagent Contradiction & Reconciliation Ledger`:** Every
  disagreement between `linux-driver-expert`, `datasheet-researcher`, bug
  history, and `hardware-prober`, with the resolution and its citation, or an
  explicit statement that the reports agreed.
* **`## 6. Gaps & Blocked Sources`:** What was looked for and not found (missing
  TRM, missing reference driver, undocumented firmware interface), where it was
  searched, and who could supply it. This section is a deliverable in its own
  right.
* **`## 7. Hardware Verification Plan & Evidence`:** The hypotheses selected for
  probing, why those (what documents could not settle), the plan digests, run
  IDs, manifest hashes, and audit sequence ranges.

#### Claim Format (Bulleted List, Never a Table)

One fact per claim. Tier and status markers are square-bracketed tokens on the
header line; the auditor parses the fields below it.

```markdown
## 3. Claims
- **C1** `[LINUX_DERIVED]` `[UNVERIFIED]` -- CTL (0x00) bit 0 enables the counter.
  - **Type:** bitfield
  - **Applies-to:** example-soc / example-timer v2 / silicon B0
  - **Source:** vendor-kernel drivers/clocksource/example-timer.c@abc1234:120-131 -- "writel(CTL_EN, base + CTL)"
  - **Reasoning:** Set as the last step of probe(); the binding requires clk_timer first.
- **C2** `[HW_VERIFIED]` `[VERIFIED]` -- STA (0x04) reads 0x00000002 on a booted device.
  - **Type:** register
  - **Applies-to:** example-soc / example-timer v2 / silicon B0 / dev-board
  - **Source:** evidence/run-001/target-audit.jsonl seq 12 -- "read32 offset 4 value 2"
  - **Evidence:** manifest sha256:<hash> boot_id <id> seq 12-13 state_origin firmware-configured
  - **Reasoning:** Datasheet reset value is 0x0; bit 1 is set because firmware started the counter.
```

* **Type:** `register`, `bitfield`, `access-class`, `sequence`, `dependency`,
  `quirk`, `parity`, or `gap`.
* **Tier:** `[HW_VERIFIED]`, `[DATASHEET]`, `[LINUX_DERIVED]`, `[BUG_INFERRED]`,
  `[BINARY_INFERRED]`, `[AGENT_INFERRED]`. A claim's tier is the strongest
  source that supports it.
* **Status:** `[UNVERIFIED]`, `[VERIFIED]`, `[CONTRADICTED]`. Only
  `[HW_VERIFIED]` claims may be `[VERIFIED]`, and only with an `Evidence:` line
  naming a `sha256:` manifest hash and a `boot_id`.
* **Applies-to:** Never empty or a placeholder (`TBD`, `unknown`, `SCOPE`,
  `<scope>`, `same`). A fact whose scope cannot be established is recorded under
  `## 6. Gaps & Blocked Sources`, not as a claim.
* **Source:** A path with commit and line range, a document or URI locator, a
  bug comment, a URL, or an evidence record, followed by `-- "<verbatim quote>"`
  (1-3 anchor lines for code quotes in recon).
* **Evidence:** Required for `[VERIFIED]`. Include `state_origin`
  (`firmware-configured` or `post-reset`) for every observed value.

Access-class claims use the driver-lab ceiling vocabulary: read-only,
read-modify-write safe, write-1-to-clear, self-clearing, destructive-read,
hard-deny.

### 2.2 `recon_devlog.md` (Append-Only Chronological Journal)

Prefixed with the same frontmatter block. Records, in order: the campaign
configuration in effect (or the one-time Phase 0 answers), scope decisions and
the sources behind them, every subagent prompt and its status marker, claim
additions and status changes with their reasons, contradiction reconciliations,
plan digests, grants added, run IDs, manifest hashes, drained audit sequence
ranges, recovery events with `boot_id` changes, audit verdicts, and any parked
enablement patches.

---

## 3. Campaign Configuration (Unattended Operation)

Read
[`campaign-config.md`](/src/devices/skills/autoda/references/campaign-config.md).
The configuration supplies the decisions that `autoda-fix` asks for per session:
source trees and their priority, the source fence, sensitivity classes, whether
hardware verification is enabled, allowed access modes, consent policy, probe
budget, recovery method, and the enablement-patch policy.

* If a configuration is supplied, do not ask alignment questions. Stop only for
  a decision the configuration does not cover, and record the answer in
  `recon_devlog.md`.
* If none is supplied, ask every needed decision once, in a single consolidated
  `ask_question`, in Phase 0, then proceed as if configured.
* Fan out by system, never by role: one orchestrator per `system_id`, each with
  its own artifact directory, launched via `run_workflow` or a swarm. Share the
  campaign configuration and the `grants.toml` store; never share artifacts
  between systems.

---

## 4. Workflow Protocol

### Phase 0: Telemetry, Artifacts, Configuration & System Identity

> **Scope before facts.** Nothing is extracted until the system's identity
> is established, because an unscoped fact is the failure mode this workflow
> exists to prevent: a register offset taken from a bug about a later PHY
> generation, applied to the current one, reads as a confident finding and is
> wrong.

#### Step 0.1: Log the Invocation and Create the Artifacts

1.  Run the telemetry logger first:

    ```bash
    python3 <path_to_autoda_core>/scripts/log_invocation.py \
      --workflow recon \
      --system-id "<system_id>" \
      --campaign-id "<campaign_id_or_empty>" \
      --conversation-id "<root_orchestrator_conversation_id>" \
      --spec "<artifact_dir>/recon_spec.md" \
      --devlog "<artifact_dir>/recon_devlog.md"
    ```

2.  Create `recon_spec.md` and `recon_devlog.md` with the frontmatter block
    (`outcome: "IN_PROGRESS"`, `hardware_verification` from the campaign
    configuration) and the seven section skeletons.
3.  Load the campaign configuration; append it (or the consolidated answers) to
    `recon_devlog.md`.

#### Step 0.2: Establish System Identity and Locate Sources

Record the results in `## 1` and `## 2` before any deconstruction starts:

1.  **Identify the block.** SoC, IP block, IP revision (from compatible strings,
    version registers named in the binding, or vendor module names), silicon
    revision, board, and the instance (base address, interrupts, bus parent)
    from the board's reference devicetree.
2.  **Locate the reference driver.** Search the reference kernel tree first
    (`sources.reference_kernel`), then upstream. Record the tree, path, and
    `branch@commit` actually read. If no driver exists in either, record
    `BLOCKED_MISSING_LINUX_SOURCE` under `## 6` and continue with documentation
    only; set `outcome: "BLOCKED"` at the end if no claims can be made.
3.  **Locate the devicetree binding** for the compatible string and the board
    node that instantiates it; both are sources in `## 2`.
4.  **Classify Fuchsia precedent.** Search the Fuchsia tree for drivers of the
    same IP family (licensed blocks often appear under several boards) and
    related IP. Record the classification and paths; a precedent driver is a
    source of Fuchsia-side design decisions, not of hardware facts.
5.  **Apply the fence.** List every source the campaign fence excludes, and
    never open one. Record sensitivity classes for the sources that remain.

### Phase 1: Static Deconstruction & Claim Extraction

Delegate, in parallel where possible, and normalize every report into claims.

1.  **`linux-driver-expert`:** Deconstruct the reference driver and binding
    into: the register map with access class per register, bitfields the driver
    uses, initialization and power sequencing with ordering and delays,
    clock/reset/power-domain/pin-control dependencies, interrupt handling and
    acknowledgement rules, DMA and alignment constraints, and every quirk or
    errata workaround. For each workaround, require the commit (`git log` / `git
    blame`) that introduced it and the silicon it names. Require the status
    marker.
2.  **`datasheet-researcher`:** Extract the canonical bitfield map, power-on
    reset values, access rules, clock equations, and timing bounds for the whole
    block from `sources.documentation_roots`. Require `VERIFIED_FROM_DATASHEET`
    or `UNAVAILABLE_NO_DATASHEET_FOUND`, and a document revision for every
    citation.
3.  **Bug history:** If `sources.bug_history_skill` names a skill, invoke it for
    the subsystem. Each finding becomes a `[BUG_INFERRED]` claim scoped to the
    silicon the bug was filed against, never promoted by assumption to the
    current one.
4.  **Dependencies & bring-up order (`## 4`):** From the binding and the
    driver's probe path, record what must be on before the block responds, and
    what the bootloader or firmware is already known to configure (boot logs,
    firmware source if available). This section gates Phase 3.
5.  **Normalize into claims.** One fact per claim with type, tier, status,
    applies-to, source with `-- "..."` verbatim quote, and reasoning when
    non-obvious. Prefer fewer, load-bearing claims over transcription: extract
    what a driver needs, not the whole document. Keep frontmatter
    `claims_total`, `claims_verified`, and `claims_contradicted` synchronized
    with `## 3. Claims`.
6.  **Reconcile (`## 5`).** Compare the reports. Where one source names an
    invariant, value, or ordering that another omits or contradicts, log it, run
    a targeted follow-up query against the sources, and record the resolution
    with its citation. Mark unresolved disagreements `[CONTRADICTED]` on the
    affected claims rather than choosing silently.
7.  **Record gaps (`## 6`).** Every blocked or unavailable source, every
    register the driver touches that no document describes, every dependency
    whose provider is unknown.

### Phase 2: Static Gate

1.  Run the deterministic audit:

    ```bash
    python3 <path_to_autoda_core>/scripts/audit_session.py \
      --workflow recon --gate static \
      --spec "<artifact_dir>/recon_spec.md" \
      --devlog "<artifact_dir>/recon_devlog.md" \
      --repo-root "<checkout_root>"
    ```

2.  Invoke `session-auditor` for the `static` gate (see
    [`session-auditor.md`](/src/devices/skills/autoda/references/session-auditor.md)
    and its recon addendum): every claim scoped and cited, status markers
    present, contradictions reconciled, no simulated or synthetic values.
3.  Record the verdicts in `recon_devlog.md` and resolve every violation before
    proceeding. If `hardware_verification` is `disabled`, skip to Phase 4 with
    `outcome: "STATIC_COMPLETE"`.

### Phase 3: Hardware Verification (`hardware-prober`)

Spend probe budget on what documents cannot settle: actual reset or
firmware-configured values, which registers are destructive-read, behavior the
documentation calls reserved, and workarounds the driver applies without
comment. Do not probe to confirm what the datasheet already states unless a
contradiction in `## 5` depends on it.

1.  **Discover the node.** Run `driver-lab list --unclaimed` and `list
    --debug-capable`. If the node is unclaimed, use proxy mode (`bind-proxy`).
    If an existing debug-capable driver owns it, use in-situ mode. If no node is
    published, the block needs devicetree enablement: make the smallest local
    change that publishes the node (see the devicetree skills under
    [`/src/devices/skills/`](/src/devices/skills/)), keep it as a
    work-in-progress change per `hardware.enablement_patches`, and record the
    change and its reasoning in `recon_devlog.md`. Enablement changes never land
    as part of recon.
2.  **Gate probes on dependencies.** Do not issue any access to a block whose
    clocks, resets, or power domains (`## 4`) are not established as on. Reads
    of an unclocked block return garbage or hang the bus; recovery is the
    campaign's out-of-band method, and a wedge is a finding to record, not a
    retry.
3.  **Liveness first.** The first plan reads an identification or status
    register with a known expected pattern. Only after it returns a valid value
    proceed to the hypotheses in `## 7`.
4.  **Author plans, run, resolve grants.** Follow the driver-lab fail-closed
    loop: validate the plan, compute its digest, run unattended, add exact read
    grants per `hardware.consent.reads`, rerun with pinned `expected_boot_id`
    and `expected_resource_digest`. Writes follow `hardware.consent.writes`;
    destructive-read and hard-deny ranges from the access-class claims are never
    read or written.
5.  **Tag what was observed.** Every observed value records `state_origin`:
    `firmware-configured` when the block was touched by the bootloader or
    firmware before the probe, `post-reset` only when the plan itself deasserted
    reset or the dependency analysis shows nothing ran before. A booted device
    is not at reset, and a value read on one is not a power-on reset value.
6.  **Reconcile anomalies.** Hand unexpected values back to
    `linux-driver-expert` and `datasheet-researcher` with the evidence; update
    `## 5` and the affected claims. Promote a claim to `[HW_VERIFIED]`
    `[VERIFIED]` only with the manifest hash, `boot_id`, and audit sequence
    range on its `Evidence:` line, and log the same values in the devlog.
7.  **Stay within budget.** Stop at `hardware.probe_budget`; record what remains
    unverified under `## 7` as the next session's plan.
8.  Run the `verified` gate:

    ```bash
    python3 <path_to_autoda_core>/scripts/audit_session.py \
      --workflow recon --gate verified \
      --spec "<artifact_dir>/recon_spec.md" \
      --devlog "<artifact_dir>/recon_devlog.md" \
      --repo-root "<checkout_root>"
    ```

    and invoke `session-auditor` for the `verified` gate.

### Phase 4: Falsification, Finalization & Hand-Off

1.  **Optional claim falsification.** If `review.adversarial_claims` is
    `high_blast_radius`, spawn a cold-context `adversarial-reviewer` with only
    `recon_spec.md` `## 3`, `## 4`, and the evidence directory (not the devlog),
    restricted to writable register masks, hard-deny ranges, and power, clock,
    and reset sequences. Reconcile each objection against the sources; record
    the verdicts per claim in the devlog. One round.
2.  **Finalize frontmatter** in both artifacts: `outcome`, claim counts,
    narrowed `target_scope`.
3.  **Upsert telemetry:**

    ```bash
    python3 <path_to_autoda_core>/scripts/log_invocation.py \
      --workflow recon \
      --system-id "<system_id>" \
      --campaign-id "<campaign_id_or_empty>" \
      --conversation-id "<root_orchestrator_conversation_id>" \
      --spec "<artifact_dir>/recon_spec.md" \
      --devlog "<artifact_dir>/recon_devlog.md" \
      --hardware-verification "<enabled | disabled>" \
      --outcome "<STATIC_COMPLETE | HARDWARE_VERIFIED | BLOCKED>"
    ```

4.  **Hand off.** `recon_spec.md` is the deliverable. Summarize for the user:
    claim counts by tier and status, the gaps that need an owner (missing
    documents, missing sources, unknown dependency providers), parked enablement
    changes, and the unverified hypotheses left for the next hardware session.
    Do not generate driver code.

---

## Common Pitfalls

* **Extracting before scoping.** A deconstruction that starts before `## 1` is
  filled in produces facts with no `Applies-to`, which the auditor rejects and
  which are the most expensive kind of error to catch later.
* **Upstream first.** For product hardware the vendor or product kernel is the
  reference; upstream is a fallback and often lacks the workarounds. Record
  which tree was read.
* **Treating a booted device as reset.** Firmware configures many blocks before
  the kernel runs. Values read on a running device are `firmware-configured`
  unless proven otherwise.
* **Probing an unpowered block.** Hangs the bus, costs a recovery cycle, and
  teaches nothing. Establish `## 4` first and read a liveness register before
  anything else.
* **Promoting by reading.** No amount of source or document reading makes a
  claim `[VERIFIED]`; only evidence does. The auditor rejects `[VERIFIED]`
  claims with any tier other than `[HW_VERIFIED]`.
* **Transcribing the TRM.** The contract is for a driver, not a reference
  manual. Claims the driver will never consult are noise that hides the
  load-bearing ones.
* **Silent contradiction resolution.** When sources disagree, the disagreement
  is the finding. Log it in `## 5`; mark the claim `[CONTRADICTED]` until a
  citation or evidence settles it.
* **Opening fenced sources.** The campaign fence exists to hold out existing
  drivers during pipeline evaluation and to keep restricted material out of
  public outputs. Opening a denied source invalidates the session.
* **Landing enablement.** Devicetree or instrumentation changes made to publish
  a node for probing are work-in-progress only. Recon never lands changes.

## Further Reading

* [AutoDA core README](/src/devices/skills/autoda/README.md): family layout,
  shared scripts, and conventions.
* [`campaign-config.md`](/src/devices/skills/autoda/references/campaign-config.md):
  campaign configuration schema.
* [`autoda-fix`](/src/devices/skills/autoda-fix/SKILL.md): the bug-fix entry
  point; its Phase 1 ground-truth rules and Phase 2 contradiction ledger are the
  model for Phase 1 here.
* [`driver-lab`](/src/devices/skills/driver-lab/SKILL.md): proxy and in-situ
  modes, plan schema, grants, evidence bundles, and exit codes.
* Devicetree skills for publishing an unclaimed node:
  [`driver_bind_devicetree`](/src/devices/skills/driver_bind_devicetree/SKILL.md),
  [`devicetree_debugging`](/src/devices/skills/devicetree_debugging/SKILL.md),
  [`debug_driver_binding`](/src/devices/skills/debug_driver_binding/SKILL.md).
