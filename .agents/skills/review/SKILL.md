---
name: review
description: >
  Performs an AI code review of Fuchsia changes (fuchsia.git and
  vendor/google) using the same Fuchsia review skills as the Gerrit CL Review
  Agent. Use when asked to review, self-review, or `/review` changes in a
  Fuchsia checkout (including `/review quick`, `/review deep`, and `/review
  simplify`). Supports `quick`, `standard`, `deep`, and `simplify` modes, up-
  front skip globs, a `code-simplification` pass (in `deep` and `simplify`),
  an adversarial challenger pass, and `[Pre-existing]` tagging.
---

# Fuchsia code review

This skill reviews a Fuchsia change with the review skills listed in
`.agents/code-review/agent_configs.textproto` (fuchsia.git, plus the
vendor/google copy for vendor/google changes), the same files the Gerrit CL
Review Agent (`groupagent-fuchsia-review` / `groupagent-turquoise-review`)
reads. Local and hosted reviews therefore load the same skills.

It is self-contained: do not load or follow any other top-level `review` skill.
It is named `review` so that `/review` in a Fuchsia checkout runs it.

## Overview & Flow

1.  **Scope, Effort & Skip Globs**: Detects `fuchsia.git` and `vendor/google`
    changes (`jj` or `git`), picks the effort/mode (`quick`, `standard`, `deep`,
    or `simplify`), and filters out non-reviewable files (`Cargo.lock`,
    `*.golden`, `*.pb.*`, binaries, `out/**`) before dispatching reviewers.
1.  **Mode Dispatch & Round-1 Review**: Resolves domain skills from
    `.agents/code-review/agent_configs.textproto` via `resolve_skills.py`, then
    loads **only** the selected mode's execution profile under `modes/`:
    - `quick` ([review-quick](modes/review-quick/SKILL.md)): single diff-focused
      pass per repository for `blocking` bugs (`confidence >= 90`), skipping
      Round 2.
    - `standard` ([review-standard](modes/review-standard/SKILL.md)): parallel
      domain-skill + `general` reviewers.
    - `deep` ([review-deep](modes/review-deep/SKILL.md)): `standard` plus
      cross-file caller/callee/invariant tracing and a parallel
      `code-simplification` subagent per repository.
    - `simplify` ([review-simplify](modes/review-simplify/SKILL.md)): standalone
      `code-simplification` subagent per repository.
1.  **Adversarial Challenger Pass (Round 2, `standard`, `deep` & `simplify`)**:
    Deduplicates Round-1 candidate findings and runs `fuchsia_code_challenger`
    ([challenger.md](references/challenger.md)) to score confidence (`0-100`,
    keeping `>= 80`), prune false positives and subjective nitpicks (including
    findings that depend on peer firmware or hardware behavior the peer cannot
    produce), and tag exposed legacy bugs as `[Pre-existing]` non-blocking
    suggestions.
1.  **Merge & Render**: Writes verified findings, `effort`, and `skipped_files`
    to `review.json` and renders the Markdown report via `render_report.py`
    (read-only by default unless the user explicitly asks to apply fixes).

## Step 1: Determine the scope and effort level

1.  Select the effort level from the user's request (`standard` when
    unspecified):
    - `quick` (if the user asks for `quick`, `fast`, or a quick sanity check):
      single-pass diff check per repository for high-certainty `blocking`
      defects (`confidence >= 90`), skipping Round 2.
    - `standard` (default): parallel domain-skill + `general` reviewers followed
      by the Round-2 adversarial challenger (`confidence >= 80`).
    - `deep` (if the user asks for `deep` or `thorough`): `standard` plus
      cross-file caller/callee, invariant, concurrency, and wire-contract
      tracing in both Round 1 and Round 2, **plus** a parallel
      `code-simplification` pass per repository (`confidence >= 80`).
    - `simplify` (if the user asks for `/review simplify`, `simplify`, or a
      focused simplification pass): runs **only** the read-only
      `code-simplification` pass per repository followed by the Round-2
      adversarial challenger (`confidence >= 80`), emitting non-blocking
      `suggestion` findings.
1.  Check each repository in the checkout separately: the fuchsia.git root, and
    `vendor/google` inside it if that directory exists. Detect the VCS at each
    repository's root (`jj` if a `.jj` directory exists there, otherwise `git`).
1.  Pick the target, treating each repository with changes as its own scope:
    - If the user named revisions, commits, branches, or Gerrit changes, review
      those in their owning repository (or both repositories if both are named).
    - Otherwise, if either repository has working-copy changes (`jj diff -r @`
      or `git diff HEAD`), review every repository that does.
    - Otherwise, review the most recent commit (`@-` in `jj`, `HEAD` in `git`)
      in each repository ahead of `JIRI_HEAD` (or `origin/main`), falling back
      to the current directory's repository if neither is ahead.
1.  For each repository in scope, record its root, VCS, immutable commit
    hash(es) (or `working copy` for uncommitted changes), and changed files as
    paths relative to that repository, exactly as Gerrit shows them.

## Step 2: Summarize the intent

Read the commit description(s) and any task context from the conversation, and
write a short, factual intent summary: the objective and the key logic changes.
When reviewing code written earlier in the same session, do not include claims
about correctness or justifications for design choices; they bias the reviewers.

## Step 3: Filter skip globs and resolve domain skills

Run from the fuchsia.git root once per repository in scope:

```bash
fuchsia-vendored-python .agents/skills/review/scripts/resolve_skills.py \
  --files="<path 1>,<path 2>" \
  --project="<fuchsia|vendor/google>" \
  --output_format=envelope
```

If `fuchsia-vendored-python` is not on `PATH`, use
`prebuilt/third_party/python3/<host>/bin/python3` or any `python3` 3.9 or later;
both scripts use only the standard library.

- The output is a JSON object with `tasks` (`[{"skill_path": ..., "files":
  [...]}]`), `retained_files`, and `skipped_files`. Files matching default skip
  globs (`Cargo.lock`, lockfiles, `*.golden`, `*.snap`, `*.pb.*`, images,
  archives, `out/**`, `.venvs/**`) are excluded up-front and listed in
  `skipped_files`. Pass `--skip_globs="<glob 1>,<glob 2>"` if the user asked to
  skip extra paths, or `--no_default_skips` if the user explicitly asked to
  review skipped files.
- Keep outputs from different repositories separate so each keeps its own
  repository root and VCS.
- Pass `--project=fuchsia` with fuchsia.git-relative paths for fuchsia.git, and
  `--project=vendor/google` with `vendor/google`-relative paths for
  `vendor/google`. For `vendor/google`, the script also reads
  `vendor/google/.agents/code-review/agent_configs.textproto` and resolves its
  skills relative to `vendor/google`. Pass `--repo_root=<fuchsia.git root>` if
  you run the script outside the checkout.
- If `retained_files` is empty across **all** repositories in scope, skip Step 4
  (do not load a mode skill) and render `decision: "not_applicable"` with
  `skipped_files` in Step 5.

## Step 4: Load and execute the selected mode profile

Use `${TMPDIR:-/tmp}/fuchsia_review_<run_id>` as `<out_dir>`, where `<run_id>`
comes from `uuidgen`. Never write review files inside the checkout.

Read **only** the `SKILL.md` for the effort level chosen in Step 1 (do **not**
read the other mode files) and follow its task-building, Round-1 reviewer, and
Round-2 challenger instructions:

- **`quick`**: Read [modes/review-quick/SKILL.md](modes/review-quick/SKILL.md)
  (`.agents/skills/review/modes/review-quick/SKILL.md`).
- **`standard`** (default): Read
  [modes/review-standard/SKILL.md](modes/review-standard/SKILL.md)
  (`.agents/skills/review/modes/review-standard/SKILL.md`).
- **`deep`**: Read [modes/review-deep/SKILL.md](modes/review-deep/SKILL.md)
  (`.agents/skills/review/modes/review-deep/SKILL.md`).
- **`simplify`**: Read
  [modes/review-simplify/SKILL.md](modes/review-simplify/SKILL.md)
  (`.agents/skills/review/modes/review-simplify/SKILL.md`).

Once the selected mode profile finishes, proceed to Step 5.

## Step 5: Merge, render, and present

1.  Combine verified results across all repositories in scope. For
    `vendor/google`, prefix every `file_path` in `reviewed_files`,
    `skipped_files`, and `findings` with `vendor/google/` (if not already
    prefixed) so all paths are relative to the fuchsia.git root. Write one
    merged JSON object to `<out_dir>/review.json` with:
    - `decision`: `changes_requested` if any surviving finding is `blocking`
      (and not `pre_existing` and `effort != "simplify"`),
      `approved_with_suggestions` if there are only suggestions (including
      `[Pre-existing]` and `code-simplification` findings), `approved` if there
      are none, or `not_applicable` if there were no applicable tasks
      (`retained_files` was empty or every Round-1 task returned `decision:
      "not_applicable"`; `render_report.py` also re-derives `decision` from
      surviving findings).
    - `summary`: 1 to 2 short plain-English sentences covering what changed and
      high-level coverage (such as clean production files, `[Pre-existing]`
      notes, same-session self-review, or any reviewer that failed twice),
      without repeating individual findings or challenger jargon in prose.
    - `scope`: the reviewed target per repository in scope (name both when the
      review spans both), for example `fuchsia.git` commit `1a2b3c4d` and
      `vendor/google` working copy (`git diff HEAD`).
    - `effort`: `"quick"`, `"standard"`, `"deep"`, or `"simplify"`.
    - `challenger_kept` and `challenger_total`: optional non-negative integers
      (`0 <= challenger_kept <= challenger_total`, with `challenger_total > 0`)
      passed only when Round 2 ran (omitted in `quick` mode or when Round 1
      produced zero candidate findings), summed across repositories where Round
      2 ran.
    - `agent_skills`: the union of all reviewers' `agent_skills`.
    - `reviewed_files`: the union of all reviewers' `reviewed_files` (with
      `vendor/google/` prefixes applied).
    - `skipped_files`: all files matched by up-front skip globs (with
      `vendor/google/` prefixes applied).
    - `findings`: all surviving findings (with `vendor/google/` prefixes,
      `confidence`, and `pre_existing` applied).
1.  Render the report with `--workspace` set to the fuchsia.git root,
    `--effort=<quick|standard|deep|simplify>`, and `--min_confidence=90` (for
    `quick`) or `--min_confidence=80` (for `standard`, `deep`, and `simplify`):

    ```bash
    fuchsia-vendored-python .agents/skills/review/scripts/render_report.py \
      --review_result_file=<out_dir>/review.json \
      --workspace=<fuchsia.git root> \
      --effort=<quick|standard|deep|simplify> \
      --min_confidence=<90|80>
    ```

    If it reports a validation error, fix `review.json` and run it again.

1.  Present the rendered Markdown to the user unchanged. Findings are numbered
    in the report (`1.`, `2.`, ...); use those numbers when discussing fixes.
1.  All modes (including `simplify`) are read-only by default: never edit files
    unless the user explicitly asked to apply/fix findings. If the user asked to
    fix the findings, apply the fixes in each repository with its VCS after
    presenting the report and verify with `fx build` (and affected `fx test`
    targets when applicable).

## Notes

- `agent_configs.textproto` matches like the hosted agent: `path_regex` must
  match the whole repo-relative path, a file matching any `exclude_filters`
  block is skipped, and an entry with no `project` applies to every project.
  Entries without `skills` add nothing here; the `general` task covers them.
- A skill without `/` resolves to `.agents/skills/<name>/SKILL.md`; a skill with
  `/` resolves to `<fuchsia.git root>/<skill>/SKILL.md`. A skill that already
  ends in `/SKILL.md` names the file itself. Skills from the vendor/google
  config resolve the same way relative to `vendor/google`. The script warns on
  stderr if a resolved `SKILL.md` is missing.
