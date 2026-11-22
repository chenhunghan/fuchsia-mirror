---
name: review-quick
description: >
  Executes the quick single-pass execution profile for Fuchsia `/review`,
  checking changed diff hunks per repository for high-certainty blocking
  defects (confidence 90+) without running Round-2 challenger validation. Use
  when `.agents/skills/review/SKILL.md` dispatches a `quick` mode review. Do
  NOT use directly as a standalone review orchestrator (invoke `/review quick`
  via `.agents/skills/review/SKILL.md` instead).
---

# Fuchsia quick review mode

Execute this profile when `.agents/skills/review/SKILL.md` selects `effort:
"quick"` and at least one repository in scope has non-empty `retained_files`.

## Task setup

For each repository in scope whose `retained_files` from `resolve_skills.py` is
non-empty, prepare one combined `quick` pass containing:

- The repository root, detected VCS (`jj` or `git`), target scope, and immutable
  commit hash(es) (or `working copy`).
- The factual intent summary from Step 2.
- `retained_files` (never `skipped_files`) and all resolved domain `skill_path`s
  from `resolve_skills.py` for that repository.
- Output path `<out_dir>/quick_<repo>.json` (never inside the checkout).

## Execution

If `define_subagent` and `invoke_subagent` are available:

1.  **Define `fuchsia_code_reviewer`**: Enable write tools (so it can run VCS
    diff commands and write its result file). Its system prompt must state that
    it is a strictly read-only reviewer: it never edits files in the checkout
    and never runs builds or tests, and it must read and follow [reviewer
    instructions](../../references/reviewer.md)
    (`.agents/skills/review/references/reviewer.md`).
1.  **Run combined per-repository pass**: Invoke one `fuchsia_code_reviewer` per
    repository in scope in parallel with `effort: quick`, its repository root,
    VCS, target scope and commit hash(es), intent summary, `retained_files`, all
    resolved `skill_path`s for that repository, and output file
    `<out_dir>/quick_<repo>.json`.
1.  **Wait and retry once on failure**: Wait for all reviewers to finish. If a
    reviewer fails or its output file is missing or invalid, invoke it once
    more; if it fails again, leave it out and note the failure in the Step 5
    report summary.
1.  **Filter findings**: Keep only `blocking` findings with `confidence >= 90`.
    Skip deduplication and Round-2 challenger validation, and return to Step 5
    of [review](../../SKILL.md).

Otherwise, follow [reviewer instructions](../../references/reviewer.md)
yourself: run one combined pass per repository, keep only `blocking` findings
with `confidence >= 90`, note in the Step 5 report summary when reviewing your
own change in the same session (since same-session self-review is not
independent), and return to Step 5 of [review](../../SKILL.md).
