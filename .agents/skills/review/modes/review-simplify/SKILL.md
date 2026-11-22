---
name: review-simplify
description: >
  Executes the standalone simplification execution profile for Fuchsia
  `/review`, running only the read-only `code-simplification` pass per
  repository followed by the Round-2 adversarial challenger (confidence 80+)
  to emit non-blocking suggestions. Use when `.agents/skills/review/SKILL.md`
  dispatches a `simplify` mode review. Do NOT use directly as a standalone
  review orchestrator (invoke `/review simplify` via
  `.agents/skills/review/SKILL.md` instead).
---

# Fuchsia simplify review mode

Execute this profile when `.agents/skills/review/SKILL.md` selects `effort:
"simplify"` and at least one repository in scope has non-empty `retained_files`.

## Build Round-1 task list

For each repository in scope whose `retained_files` from `resolve_skills.py` is
non-empty:

- Replace the resolved domain-skill tasks with a single task using
  [code-simplification](../../../code-simplification/SKILL.md) (`<fuchsia.git
  root>/.agents/skills/code-simplification/SKILL.md`) and `retained_files` (do
  **not** add `general`).
- Keep the resolved domain `skill_path`s from `resolve_skills.py` so Round-2
  Challenger can verify subsystem, peer, and ABI constraints.

Keep tasks from different repositories separate so each preserves its own
repository root and VCS.

## Execution

If `define_subagent` and `invoke_subagent` are available:

1.  **Define `fuchsia_code_reviewer`**: Enable write tools (so it can run VCS
    diff commands and write its result file). Its system prompt must state that
    it is a strictly read-only reviewer: it never edits files in the checkout
    and never runs builds or tests, and it must read and follow [reviewer
    instructions](../../references/reviewer.md)
    (`.agents/skills/review/references/reviewer.md`).
1.  **Round 1 (`code-simplification` Reviewer)**: In a single turn, invoke one
    `fuchsia_code_reviewer` per repository in scope in parallel. Pass its
    repository root and VCS, target scope and commit hash(es), `effort:
    simplify`, intent summary, the `code-simplification` `skill_path` with
    `retained_files`, and its output file `<out_dir>/task_<n>.json`.
1.  **Wait and retry once on failure**: Wait for all reviewers. If a reviewer
    fails or its output file is missing or invalid, invoke it once more; if it
    fails again, leave it out and say so in the Step 5 report summary.
1.  **Collect and deduplicate candidate findings**: Read the Round-1 result file
    per repository, deduplicating any findings with the same `file_path`,
    overlapping lines, and the same defect. If no candidate findings remain,
    skip Round 2 (omitting `challenger_kept` and `challenger_total`) and return
    to Step 5 of [review](../../SKILL.md) (using `decision: "not_applicable"` if
    every Round-1 task returned `decision: "not_applicable"`).
1.  **Round 2 (Adversarial Challenger Pass)**: For each repository with
    candidate findings, write `<out_dir>/draft_findings_<repo>.json` with a
    0-based `finding_index` on each candidate finding. Define a **separate**
    subagent named `fuchsia_code_challenger` with write tools enabled (strictly
    read-only on the checkout; never runs builds or tests) that must read and
    follow [challenger instructions](../../references/challenger.md)
    (`.agents/skills/review/references/challenger.md`). Invoke one
    `fuchsia_code_challenger` per repository in parallel with its repository
    root, VCS, target scope and commit hash(es), `effort: simplify`, intent
    summary, `<out_dir>/draft_findings_<repo>.json`, every resolved domain
    `skill_path` for that repository plus `<fuchsia.git
    root>/.agents/skills/code-simplification/SKILL.md` (so it can verify peer
    contracts, ABI rules, and simplification guardrails), and output file
    `<out_dir>/evaluations_<repo>.json`.
1.  **Join challenger verdicts**: Join `<out_dir>/evaluations_<repo>.json` back
    onto the candidate findings by `finding_index`: keep only findings where
    `verdict == "accept"` and `confidence >= 80`, set `confidence` and
    `pre_existing` (`true` or `false`, overwriting any Round-1 value) on each
    surviving finding, ensure every surviving finding has `severity` set to
    `"suggestion"` (never `"blocking"`), record total deduplicated Round-1
    candidates (`challenger_total`) and accepted findings (`challenger_kept`)
    for Step 5, and return to Step 5 of [review](../../SKILL.md).

Otherwise, follow [reviewer instructions](../../references/reviewer.md) yourself
for each repository's `code-simplification` task in sequence and deduplicate
candidate findings. When candidate findings remain, follow [challenger
instructions](../../references/challenger.md) to challenge and score each
candidate finding (`0-100`), keep only findings where `verdict == "accept"` and
`confidence >= 80` with `severity: "suggestion"`, and record `challenger_total`
and `challenger_kept` for Step 5 (otherwise skip Round 2 and omit both counts).
Note in the Step 5 report summary when reviewing your own change in the same
session, and return to Step 5 of [review](../../SKILL.md).
