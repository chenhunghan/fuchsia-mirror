<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# AutoDA Skill Family: Shared Core (`autoda`)

This directory is not a skill. It holds the role specifications, subagent
registry, compliance auditor, and telemetry logger shared by the AutoDA
entry-point skills. Each entry point is a sibling directory with its own
`SKILL.md` that owns one workflow protocol and delegates everything else here.

| Entry point | Workflow | Starts from | Produces |
| :--- | :--- | :--- | :--- |
| [`autoda-fix`](/src/devices/skills/autoda-fix/SKILL.md) | `fix` | A bug in an existing Fuchsia driver | A verified surgical fix (`bug_spec.md`, `bug_devlog.md`, CL) |
| [`autoda-recon`](/src/devices/skills/autoda-recon/SKILL.md) | `recon` | A hardware system Fuchsia does not yet drive | Scoped, cited, optionally hardware-verified claims (`recon_spec.md`, `recon_devlog.md`); no driver |

A future new-driver-development entry point (`dev`) slots in the same way:
add the workflow to `WORKFLOWS` in the registry, add gates to the auditor,
and write a thin `SKILL.md`.

## Layout

```text
autoda/
  README.md                          This file: family layout and shared conventions.
  references/<role>.md               One specification per subagent role.
  scripts/register_subagents.py      Role prompts + per-workflow overrides/addenda.
  scripts/register_subagents_test.py Unit tests for the subagent registry.
  scripts/audit_session.py           Deterministic gate checks (--workflow fix|recon).
  scripts/audit_session_test.py      Unit tests for the auditor.
  scripts/log_invocation.py          Invocation telemetry (--workflow fix|recon).
  scripts/log_invocation_test.py     Unit tests for the telemetry logger.
autoda-fix/SKILL.md                  Fix entry point.
autoda-recon/SKILL.md                Recon entry point.
```

In entry-point documents, `<path_to_autoda_core>` means this directory
(`//src/devices/skills/autoda`).

## Shared Scripts

```bash
# Subagent configurations for an entry point (JSON on stdout).
python3 <path_to_autoda_core>/scripts/register_subagents.py --workflow fix
python3 <path_to_autoda_core>/scripts/register_subagents.py --workflow recon

# Deterministic gate audit. Fix gates: pre-fix, pre-commit.
# Recon gates: static, verified.
python3 <path_to_autoda_core>/scripts/audit_session.py \
  --workflow recon --gate static \
  --spec <artifact_dir>/recon_spec.md --devlog <artifact_dir>/recon_devlog.md \
  --repo-root <checkout_root>

# Invocation telemetry (prints the resolved record; writes only when
# Fuchsia metrics collection is enabled). Pass --spec/--devlog when
# campaign.json uses a custom output.artifact_dir.
python3 <path_to_autoda_core>/scripts/log_invocation.py --workflow recon \
  --system-id <system_id> --campaign-id <campaign_id> \
  --conversation-id <root_conversation_id> \
  --spec <artifact_dir>/recon_spec.md --devlog <artifact_dir>/recon_devlog.md

# Unit tests.
python3 <path_to_autoda_core>/scripts/register_subagents_test.py
python3 <path_to_autoda_core>/scripts/audit_session_test.py
python3 <path_to_autoda_core>/scripts/log_invocation_test.py
```

`--workflow` defaults to `fix` everywhere, so existing `autoda-fix` command
lines keep working. Both `audit_session.py` and `log_invocation.py` accept
`--spec`/`--devlog` and the older `--bug-spec`/`--bug-devlog` spellings.

## Roles and Workflow Membership

Roles are defined once in `ROLES` in
[`register_subagents.py`](scripts/register_subagents.py). `WORKFLOWS` lists
which roles each entry point registers. Every registered prompt ends with an
`ACTIVE WORKFLOW` banner naming the entry point; roles shared by several
workflows (`linux-driver-expert`, `datasheet-researcher`, `hardware-prober`)
carry shared invariants followed by one section per workflow and follow the
section the banner names. A workflow may also attach an addendum, appended
after the banner, for roles that need extra workflow-specific instructions.

| Role | `fix` | `recon` | Specification |
| :--- | :---: | :---: | :--- |
| `bug-investigator` | yes | | [bug-investigator.md](references/bug-investigator.md) |
| `fuchsia-driver-fixer` | yes | | [fuchsia-driver-fixer.md](references/fuchsia-driver-fixer.md) |
| `linux-driver-expert` | yes | yes | [linux-driver-expert.md](references/linux-driver-expert.md) |
| `datasheet-researcher` | yes | yes | [datasheet-researcher.md](references/datasheet-researcher.md) |
| `hardware-prober` | yes | yes | [hardware-prober.md](references/hardware-prober.md) |
| `session-auditor` | yes | yes | [session-auditor.md](references/session-auditor.md) |
| `adversarial-reviewer` | yes | yes | [adversarial-reviewer.md](references/adversarial-reviewer.md) |

Recon-specific roles (source locator, dependency walker, Fuchsia-precedent
researcher, claim curator) are not yet registered; until they are, the recon
orchestrator performs those steps itself as described in the recon `SKILL.md`.

## Shared Conventions

### Subagent Setup and Execution Modes

1.  **Multi-Agent Mode (Recommended):** When `define_subagent` and
    `invoke_subagent` are available, define any missing roles from
    `register_subagents.py --workflow <workflow>` and delegate each phase to
    the role that owns it. Pin `adversarial-reviewer` to the strongest
    available model (`swarm(action="add", model=..., ...)` or
    `invoke_subagent` with `Model="pro"`).
2.  **Single-Agent Progressive Mode:** When subagent tools are unavailable,
    execute each phase directly while adopting the role constraints in
    `references/*.md`, run the auditor at every gate, and maintain both
    artifacts.

The hub (orchestrator) preserves its context window: it routes, reconciles,
and writes artifacts, and never reads raw bug threads, Linux source, or log
dumps itself when a role exists for that.

### Dual-Artifact Discipline

Every session maintains two markdown files in the conversation artifact
directory, each beginning with a YAML frontmatter block that the auditor
parses:

*   A **spec** (`bug_spec.md` or `recon_spec.md`): the living contract, with
    mandatory headings enforced by `audit_session.py`.
*   A **devlog** (`bug_devlog.md` or `recon_devlog.md`): an append-only
    chronological journal of subagent prompts, citations, probe plan digests,
    evidence manifest hashes, audit verdicts, and reconciliation rationale.
    Never alter or delete earlier entries.

Frontmatter records the root orchestrator's `conversation_id` (never a child
subagent's), the `skill_version` of the entry point, and the resolved
`model`, all populated from `log_invocation.py` output.

### Zero Parametric Memory

`linux-driver-expert` and `datasheet-researcher` never state how a driver or
a document behaves from training data. Every claim cites a path or URL read in
the current session, the function or section, and a verbatim quote. Status
markers make the outcome auditable:

| Role | Success marker | Failure marker | Effect |
| :--- | :--- | :--- | :--- |
| `linux-driver-expert` | `STATUS: VERIFIED_FROM_SOURCE` | `STATUS: BLOCKED_MISSING_LINUX_SOURCE` | Blocking: halt and ask for the source location |
| `datasheet-researcher` | `STATUS: VERIFIED_FROM_DATASHEET` | `STATUS: UNAVAILABLE_NO_DATASHEET_FOUND` | Non-blocking: proceed on source and hardware evidence |

A claim about hardware is `[VERIFIED]` only when it cites a finalized
driver-lab evidence bundle (`manifest.json` hash, `boot_id`, audit sequence
range). Reading documentation or source never verifies a claim.

### Scope Before Facts

Every extracted fact names what it applies to: SoC, IP block and revision,
silicon revision, and board where relevant. A source whose scope is not yet
established (a bug about a sibling part, an upstream driver for a different
SoC, a document for a later IP generation) is untrusted until it is. Record
contradictions between sources as findings rather than resolving them
silently.

### Telemetry and Attribution

`log_invocation.py` runs at the start of Phase 0 and again whenever a
recorded policy changes (verification mode, hardware verification, outcome).
Records are written only when Fuchsia metrics collection (`fx metrics`) is
enabled. Commits produced by the `fix` workflow carry the trailers listed in
the `autoda-fix` skill so CLs can be joined back to their session.

## Adding a Workflow

1.  Add the workflow to `WORKFLOWS` in `register_subagents.py` with its role
    list and any addenda; add new roles to `ROLES` and a specification under
    `references/`.
2.  Add the workflow's gates and checks to `audit_session.py` and cover them
    in `audit_session_test.py`.
3.  Add the workflow's skill name and default version to `log_invocation.py`.
4.  Write a thin `../autoda-<workflow>/SKILL.md` that owns only the workflow
    protocol and links here for everything shared.
