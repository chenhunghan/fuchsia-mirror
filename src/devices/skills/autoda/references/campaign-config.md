<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# Reference: Recon Campaign Configuration (`campaign.json`)

A recon campaign is a set of `autoda-recon` sessions, one per hardware
system, that share standing policy decisions. The campaign configuration
replaces the per-session alignment questions that `autoda-fix` asks in its
Phase 0, so sessions can run unattended and be fanned out in parallel.

The file is JSON (`schema_version: 1`). Pass its path when invoking the skill
(`/autoda-recon <system_id> --campaign <path>`), or place it at
`campaign.json` in the artifact directory. When no campaign configuration is
available, the orchestrator asks the same questions once in Phase 0, records
the answers in `recon_devlog.md`, and never re-prompts within the session.

## Schema

```json
{
  "schema_version": 1,
  "campaign_id": "recon-example-board-2026q4",
  "target_scope": "example-board / example-soc / silicon B0",
  "sources": {
    "reference_kernel": {
      "tree": "/path/or/url/to/vendor-kernel",
      "branch": "release-branch",
      "commit": "",
      "priority": 1
    },
    "upstream_kernel": {
      "tree": "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
      "branch": "main",
      "priority": 2
    },
    "documentation_roots": ["/path/to/datasheets"],
    "bug_history_skill": "",
    "fence": {
      "allow": [],
      "deny": ["^src/devices/.*/example-timer/"]
    },
    "sensitivity": {
      "reference_kernel": "internal",
      "documentation_roots": "nda"
    }
  },
  "hardware": {
    "verification": "enabled",
    "target": "",
    "access_modes": ["proxy", "in-situ"],
    "consent": {"reads": "grant_persisted", "writes": "ask"},
    "probe_budget": {"max_runs": 20, "max_minutes": 60},
    "recovery": "ffx target reboot",
    "enablement_patches": "wip_only"
  },
  "review": {"adversarial_claims": "high_blast_radius"},
  "output": {"artifact_dir": ""}
}
```

## Fields

| Field | Meaning |
| :--- | :--- |
| `campaign_id` | Groups per-system sessions in telemetry (`log_invocation.py --campaign-id`). |
| `target_scope` | Default `Applies-to` prefix for claims and the `target_scope` frontmatter value. Sessions narrow it per system (IP block and revision). |
| `sources.reference_kernel` | The product or vendor kernel tree to read first. `linux-driver-expert` records the tree and `branch@commit` actually read. |
| `sources.upstream_kernel` | Fallback tree, used only when the reference tree lacks the driver; the devlog records the fallback. |
| `sources.documentation_roots` | Local or shared roots that `datasheet-researcher` may search. |
| `sources.bug_history_skill` | Name of a bug-history digest skill to invoke for the subsystem, if one exists for the product. |
| `sources.fence.allow` / `deny` | Regular expressions over repository paths, Code Search scopes, and bug IDs. A denied source is never opened by any role; use `deny` to hold out an existing driver when evaluating the pipeline against a system that Fuchsia already drives. |
| `sources.sensitivity` | Sensitivity class per source (`public`, `internal`, `nda`). Claims inherit the class of their source; consumers that write into public repositories may only carry `public` citations. |
| `hardware.verification` | `enabled` or `disabled`. Disabled sessions stop at the `static` gate with `outcome: STATIC_COMPLETE`. |
| `hardware.target` | `ffx` target selector; empty means the default target. |
| `hardware.access_modes` | Allowed driver-lab modes in preference order. `proxy` binds `lab_proxy` to an unclaimed node; `in-situ` uses an existing debug-capable driver. |
| `hardware.consent.reads` | `grant_persisted`: `hardware-prober` may add exact read grants to `grants.toml` for registers classified non-destructive. `ask`: every new read offset requires operator consent. |
| `hardware.consent.writes` | `forbidden` or `ask`. Persistent write grants are never supported; `ask` means one-shot `--consent` writes after operator approval. |
| `hardware.probe_budget` | Caps on driver-lab runs and wall-clock minutes per system, to bound fleet contention. |
| `hardware.recovery` | Out-of-band recovery command when a probe wedges the target (board power control, fleet tooling, or `ffx target reboot`). |
| `hardware.enablement_patches` | `wip_only`: devicetree or instrumentation changes needed to publish a node are kept small, parked as work-in-progress changes, and referenced from the spec; they never land as part of recon. |
| `review.adversarial_claims` | `high_blast_radius` runs `adversarial-reviewer` against write masks, hard-deny ranges, and power, clock, and reset sequences; `none` skips it. |
| `output.artifact_dir` | Override for the artifact directory; empty means the conversation artifact directory. |

## Decisions the Configuration Replaces

In `autoda-fix`, Phase 0 asks the user, once per bug, about verification
mode, adversarial review, deployment mechanism, instrumentation policy,
consent policy, and recovery. In a recon campaign those answers are the
same for every system, so they are made once here. A session only stops to
ask when the configuration is silent on a decision it needs.
