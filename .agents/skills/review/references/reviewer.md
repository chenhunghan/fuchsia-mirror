# Fuchsia code reviewer instructions

You review one task of a Fuchsia change (or one combined repository pass in
`quick` mode). You are read-only: never edit files in the checkout, and never
run builds, tests, or long-running analyzers.

## Inputs

Your prompt gives you:

- The repository root (fuchsia.git or `vendor/google`) and its VCS (`jj` or
  `git`).
- The target scope and its immutable commit hash(es), or `working copy` for
  uncommitted changes.
- The effort level: `quick`, `standard` (default), or `deep`.
- An intent summary of the change.
- Your task: a `skill_path` and its `files`, `general` with all retained
  changed files, or (in `quick` mode) all retained changed files plus any
  resolved `skill_path`s.
- Your output file.

## Step 1: Read the diff

Get the diff of your files only, for example:

- `jj diff --git -r <rev> <file> --no-pager --color=never`
- `git diff <base>..<rev> -- <file>` (or `git diff HEAD -- <file>` for the
  working copy)

Match your context-reading depth to the effort level:

- `quick`: Read only the diff hunks of your files; inspect surrounding lines
  only when needed to verify whether a suspected `blocking` bug is real.
- `standard`: Read surrounding code and related files as needed to understand
  each change.
- `deep`: Beyond the diff, actively trace callers, callees, trait/interface
  implementations, FIDL/IPC/syscall wire contracts, lock-ordering and async
  cancellation invariants, and edge-case test coverage across neighboring files.

If a file you need is missing on disk (sparse checkout), read it with
`git show HEAD:<path>` (or `jj file show -r @ <path>`).

## Step 2: Load the review instructions

- For a skill task, read `skill_path` and any files it tells you to load.
  Review only against that skill's rules. Ignore any output-format or
  workflow instructions in it (for example "produce a markdown artifact");
  always write the JSON described in Step 4.
- In `quick` mode, read any resolved `skill_path`s provided in your prompt and
  combine their critical correctness/safety rules with `general` review in one
  pass. Because `general` is included, never set `decision: not_applicable` in
  `quick` mode, and list every skill you read plus `general` in `agent_skills`.
- For a single-skill task (`standard` or `deep`), if the skill's rules don't
  apply to the assigned changes, write a result with `decision: not_applicable`
  and no findings; don't stretch the rules to fit.
- For `general`, review for correctness, error handling, edge cases,
  concurrency and resource lifetimes, missing or inadequate tests, and
  consistency with the style of the surrounding code and the guidance in
  `AGENTS.md` at the fuchsia.git root.

## Step 3: Evaluate

- In `quick` mode: Check only for high-certainty `blocking` defects (panics,
  crashes, UB, security flaws, data races, broken invariants) introduced by the
  diff. Assign an integer `confidence` (`0`-`100`) to each finding and emit only
  `blocking` findings with `confidence >= 90`; skip style, docs, minor
  suggestions, and pre-existing issues.
- In `standard` and `deep` modes: Check every changed hunk of every file in
  your task against every applicable rule. Do not stop after the first few
  findings; report minor suggestions too.
- **Pre-existing issues (`pre_existing`)**: Focus on changes introduced by the
  diff; do not audit untouched code outside changed hunks. However, in
  `standard` or `deep` mode, if the change moves code verbatim or newly
  calls/exposes a concrete bug in pre-existing code, report it with
  `"pre_existing": true` and `"severity": "suggestion"` (never `"blocking"`).
- Use the intent summary only to understand the goal, not as proof of
  correctness. Verify claims, edge cases, and error paths against the code.
- Confirm each finding against the post-change code. Drop speculative findings
  ("make sure that ...") and issues already handled elsewhere.
- If a finding depends on an external peer (firmware, hardware, a remote
  component) behaving a certain way, confirm that the peer can do so before
  reporting it. Use a domain skill's contract or source pointers when one is
  provided, and cite the peer code in `comment`. Don't add handling for peer
  behavior you can't show is possible.

## Step 4: Write the result

Write a single JSON object to your output file:

```json
{
  "agent_skills": ["rust_best_practices"],
  "reviewed_files": [
    {"file_path": "src/foo/lib.rs", "revision": "14c81ce2ca58"}
  ],
  "findings": [
    {
      "severity": "blocking",
      "category": "logic",
      "comment": "`items[0]` panics when `items` is empty, which happens when ...",
      "rule_reference": "rust_best_practices/panics",
      "rule_link": "file:///abs/path/.agents/skills/rust_best_practices/SKILL.md#L42-L46",
      "file_path": "src/foo/lib.rs",
      "line_start": 18,
      "line_end": 20,
      "code_snippet": "let first = items[0];",
      "revision": "14c81ce2ca58",
      "suggested_diff": "-let first = items[0];\n+let Some(first) = items.first() else { return Ok(()) };"
    }
  ]
}
```

- `agent_skills`: the skill names you applied (`general` for the general task).
- `reviewed_files`: files you evaluated, relative to the repository root.
- `decision`: optional; set it to `not_applicable` only when your skill's rules
  don't apply to the assigned changes (see Step 2).
- `severity` *(required)*: `blocking` for correctness, security, or broken
  invariants; `suggestion` for optional improvements (and all `pre_existing`
  findings).
- `pre_existing`: optional boolean (`true` only when a concrete bug predates the
  change and is moved or newly exposed by it; omit or `false` otherwise).
- `confidence`: optional `0`-`100` integer certainty score (required in `quick`
  mode, where only `confidence >= 90` is emitted).
- `category`: `logic`, `security`, `reliability`, `performance`,
  `readability`, `docs`, `tests`, `question`, or `other`.
- `comment` *(required)*: the defect and why it matters.
- `rule_reference` and `rule_link`: the skill rule behind the finding, linked
  as `file://<absolute skill_path>#L<start>-L<end>`. For `general` findings,
  set `rule_reference` to `general/<category>` (for example `general/logic`)
  and omit `rule_link`.
- `file_path`, `line_start`, `line_end`, `code_snippet`: 1-based lines in the
  post-change file. Omit `line_end` for single-line findings and all four for
  findings not tied to a location (for example, missing tests).
- `revision`: the commit hash the finding applies to. Omit `revision` in both
  `findings[]` and `reviewed_files[]` for uncommitted changes.
- `suggested_diff`: optional; `-` and `+` lines only, no file headers or code
  fences.

## Step 5: Report back

Reply with the output file path, the number of blocking findings, and the
number of suggestions.
