---
name: graduating-planter-rules-to-shac
description: >
  Graduates (retires, dedupes) Planter machinery rules under
  //build/beads/planter/ (check finding categories and the prompt prose that
  restates them) that a shac Bazel lint check in //scripts/shac/ already
  enforces as an error, as one local commit per rule. Use when asked to
  graduate, retire, or dedupe Planter rules that shac enforces, shrink
  coder.md or reviewer prompts by removing rules shac covers, or follow up on
  a shac check reaching full enforcement.
---

# Graduating Planter Rules to shac

A **rule** is one Planter finding `category` emitted by a check script in
`machinery/checks/`, plus any prompt prose that restates it. A **shac check** is
a Bazel lint check in `scripts/shac/bazel_lint/checks/`, registered in
`scripts/shac/bazel.star`. Paths are relative to `build/beads/planter/` unless
they start with `scripts/` or `build/`.

shac runs on every CL in CQ. Inside Planter,
`machinery/checks/bazel_shac_lint.sh` runs every registered shac check (through
the checkout's `scripts/shac/bazel_lint/main.py`) on the Bazel files the change
adds, modifies, or renames, and reports each result as a Planter finding whose
`category` is the shac check name. It runs even with `PLANTER_SKIP_BUILD=1`,
which skips the full `shac check` in `build_verification.sh`.

So when a shac check reports every case of a Planter rule as an error, the
Planter rule is a duplicate: the coder sees the shac finding before upload, and
CQ blocks on it after. Planter's loop also blocks on shac warnings, but CQ
doesn't, so only an error-level shac finding replaces a Planter error.

## Workflow

Prepare local commits in a separate git worktree (`git worktree add`), not in
the user's checkout, and don't upload unless the user asks. Each rule gets its
own commit based on origin/main (one CL per rule, not a stack), so each CL can
land or be reverted alone. Never run `build_verification.sh`, `fx build`, or `fx
bazel`.

### Step 1: List shac checks

```sh
skill=<checkout>/build/beads/.agent/skills/graduating_planter_rules_to_shac
python3 "$skill/scripts/list_shac_checks.py" --fuchsia-dir <worktree> [--json]
```

For each shac check, this prints its enforcement, the files it runs on
(`applies_to_globs`, `file_kinds`), its exemptions, and its allowlist; the
module docstring documents every field. If it exits non-zero, it prints only
errors; fix or report them. Use `python3` 3.13+ (the `fuchsia-vendored-python`
shebang needs the checkout's prebuilts), and pass `--fuchsia-dir` with the
worktree you edit; without it, the script inspects the checkout that contains
it.

Only checks at `enforcement=all` are candidates: under
`unchanged_lines_warning_only`, CQ downgrades errors on unchanged lines to
warnings. (Enforcement only matters for CQ; Planter's loop treats both modes the
same.) For shac internals, see "Related skills".

### Step 2: Check the preconditions

Stop and report if either fails:

- `bazel_shac_lint` is `"enabled": true` in `machinery/checks/manifest.json`.
- The script lists the shac check (exit 0 means it is registered). Native
  `shac.check()` functions don't qualify: only `build_verification` runs them.

### Step 3: Find overlaps

Grep `machinery/checks/` and `machinery/prompts/` for each shac check's topic
(for example `exports_files`, `default_visibility`, `visibility:public`,
`HOST_CONSTRAINTS`, `//vendor/`) and list the matching `"category"` values.

Examples (last classification; may be stale, so still do Step 4). Pruning CLs
don't need to update this list.

- `disallowed_host_constraints` (`bazel_minimality.sh`) and
  `bazel_host_constraints`: likely FULL.
- `visibility_scoping` (`visibility_audit.sh`) and
  `bazel_package_default_visibility`: PARTIAL; shac errors only on broad
  `default_visibility` and warns on narrow, Planter errors on any.

### Step 4: Classify coverage

Read both implementations and compare the trigger (the AST condition in
`checks/<name>.py` vs. the condition guarding the Planter finding), the level
(shac `level` vs. Planter `severity`), the files (see "shac only sees some
files" vs. the Planter check's candidate files), the exemptions (allowlist and
`policy_exempt_globs` vs. pre-existing-call skips and path prefixes), and the
remediation. Then classify the rule:

- **FULL:** every case Planter reports is a shac `error` in a file shac sees.
  Allowlisted and policy-exempt files count as covered (see "Allowlists and
  exemptions are decisions"); shac being stricter is fine. Delete the rule.
- **PARTIAL:** shac misses a case, only warns on it, or doesn't see some files
  the Planter rule reports on: files in sub-repositories (`vendor/*`), under
  `_EXCLUDED_GLOBS`, of a type outside `applies_to_globs` or `file_kinds`, files
  the change didn't touch, or non-Bazel files such as `BUILD.gn` (check the
  Planter check's candidate files and pre-existing skips). Skip and report the
  gap; don't narrow the rule. A human decides what to do with it. If the user
  asks to graduate it anyway, the commit message must name the cases CQ won't
  block.
- **NONE:** shac doesn't check this. Skip.
- **CONFLICTING:** shac and Planter require different fixes for the same code.
  Stop and report; never edit either side.

### Step 5: Edit, then validate

Edit one rule per commit:

- Delete the finding and any detection only it uses. Update the check's
  `manifest.json` description only if it names the category or a topic that only
  this rule covers.
- Don't disable a check in a pruning commit. When a check has no categories left
  after its pruning CLs land, disable it (`"enabled": false` in `manifest.json`)
  in a separate follow-up CL; a later cleanup CL deletes the script and its
  references.
- Remove prose in `machinery/prompts/` (`coder.md`, `playbooks/*.md`,
  `reviewers/*.md`, `panel.json`) only where it just restates the rule. Keep
  prose that adds information shac doesn't enforce.
- If losing Planter's remediation leaves the coder without guidance, check that
  another finding or skill gives it (for example
  `determining_bazel_visibility`), or add it to `REMEDIATION_BY_RULE`.
- `git grep -n '<category>' -- build/beads` must print nothing, or only intended
  citations (it finds lessons, tests, `run_checks.sh`, `panel.json`, and other
  skills).

Then validate:

1.  Run `bash -n` on each edited script (including `bazel_shac_lint.sh` if you
    edited `REMEDIATION_BY_RULE`) and `python3 -m py_compile` on its embedded
    Python (between `<<'PYEOF'` and `PYEOF`) and on edited libraries
    (`rdeps_lib.py`, `planter_check_lib.py`). Run `python3 -m json.tool
    machinery/checks/manifest.json`. If `planter_check_lib_test.py` mentions the
    check, run it with `python3`.
2.  Make a throwaway worktree (`git worktree add --detach /tmp/<wt> <sha>`) at a
    merged single-package `[bazel_migration]` commit newer than the last change
    to the shac check, its allowlist, and `bazel_shac_lint.sh` (a batched commit
    adds `out_of_scope_package_modified` findings unless you pass all of its
    dirs). Use the dir in its subject as `<target_dir>`. Copy the whole
    `machinery/` directory (checks import libraries from their own directory)
    from origin/main, the parent of your commit (the baseline), and from your
    commit (the edit), and run each copy on the commit's target dir:

    ```sh
    mkdir -p /tmp/<copy>
    git archive <commit> build/beads/planter/machinery | tar -x -C /tmp/<copy>
    PLANTER_WORKDIR=/tmp/<wt> PLANTER_SKIP_FETCH=1 \
      bash /tmp/<copy>/build/beads/planter/machinery/tools/run_checks.sh \
      --only <check>,bazel_shac_lint --json <target_dir>
    ```

    `run_checks.sh` runs the copy's own checks with the environment and change
    base Planter uses (`HEAD~1` for a merged `[bazel_migration]` commit, so
    uncommitted edits also count); `PLANTER_SKIP_FETCH=1` skips fetching
    origin/main. The findings must be identical except for the removed category.

3.  **Mandatory:** add uncommitted synthetic violations in the throwaway
    worktree, one per detection branch of the rule and per file type it
    inspects, and run both copies again. For every case, the baseline reports
    the category, the edit doesn't, and `bazel_shac_lint` reports the shac check
    at `error` (line numbers may differ for the same statement; for example,
    shac reports an aliased load at the symbol's line). If `bazel_shac_lint`
    reports nothing for a case, coverage isn't proven (see "bazel_shac_lint.sh
    fails open"); stop. Make each synthetic statement textually new (for
    example, use an alias): Planter checks often skip a statement that already
    exists at the change base, so a copied line looks like a pass. Planter only
    inspects its target and dependency dirs, so to add an optional case in an
    allowlisted file that shows the exemption you accept, add that file's dir to
    `<target_dir>` (comma-separated).
4.  For the follow-up CL that disables a check, confirm that the check no longer
    emits any finding category (all were graduated in earlier CLs) and that
    `run_checks.sh --list` shows it as `[disabled]`.
5.  Remove the worktree with `git worktree remove --force /tmp/<wt>`.

### Step 6: Commit

- Subject `[beads][planter] <summary>`, for example `[beads][planter] Graduate
  exports_files() visibility to shac`.
- The body names the rule and check, the shac check with its path, enforcement,
  and level, and that `bazel_shac_lint` reports it in Planter's inner loop. It
  lists what was removed, including any prompt prose (file and what it said),
  and the files that are now unenforced (allowlists, exemptions).
- Cite the commits that introduced the rule, so reviewers can see why the rule
  was added and confirm that shac covers the same case. Find them with `git log
  -S '<category>' --oneline -- build/beads/planter` (and `-S` on any deleted
  helper) and `git grep -n '<category>' -- build/beads/planter/lessons`. A
  lesson the origin commit added may no longer exist at HEAD; read it with `git
  show <origin>:<lesson path>` and cite it by name. `git log -S` stops at the
  import commit `b4381f9bbc6`; if that is the oldest hit, write "origin predates
  import".
- Include the validation results, a `Test:` line, and `No-Try: true`. No
  personal names or emails.
- For several rules, make one commit per rule, each based on origin/main
  (separate CLs, not a stack). Two CLs that edit the same check may conflict;
  rebase the later one after the first lands.

## Pitfalls

- **Allowlists and exemptions are decisions.** "all" doesn't mean everywhere:
  allowlists can exempt whole files or path prefixes (for example
  `bazel_host_constraints` exempts `tools/testing/`), and `policy_exempt_globs`
  exempts paths such as `build/config/**` or `vendor/**`. `bazel_shac_lint.sh`
  honors both, so after a graduation these files have no enforcement anywhere.
  That is intended; don't keep the Planter rule to re-enforce them, but state it
  in the commit message.
- **Target-scoped allowlist entries don't exempt calls without a name.**
  `exports_files()` and `package()` need whole-file or prefix entries.
- **shac only sees some files:** files the change touches, in fuchsia.git,
  matching both `applies_to_globs` and `file_kinds`, minus `_EXCLUDED_GLOBS`
  (`third_party/**/src/**`, `third_party/bazel_vendor/**`). A plain `BUILD` file
  has `FileKind.BUILD` but matches no glob, so CQ never checks it, even though
  `bazel_shac_lint.sh` passes it to `main.py`.
- **`bazel_shac_lint.sh` fails open.** An import error, a timeout, bad JSON, or
  `python3` older than 3.13 drops findings silently. It also runs
  `*.BUILD.bazel` files in one group with `MODULE.bazel` and `WORKSPACE` files,
  and `main.py` rejects the whole group for a check that doesn't accept one of
  them, so a change touching both loses that check's `*.BUILD.bazel` findings.
  After a graduation nothing else in Planter's loop reports the rule.
- **`bazel_shac_lint.sh` ignores enforcement.** It reports every finding in a
  touched file at the check's level, including pre-existing ones that Planter
  rules often skip; under `all`, CQ fails on the error-level ones.
- **Variables shared with other findings.** For example, `has_pkg_default_vis`
  in `visibility_audit.sh` gates other categories. Remove only detection the
  rule alone uses, and search every check and `machinery/tools/` for callers
  before changing a shared helper's return value.
- **Stricter Planter rules aren't duplicates.** Planter's
  `internal_vendor_subpath_visibility` forbids `//vendor/google/<x>` entries
  that shac's `bazel_vendor_paths` allows: PARTIAL by trigger.

## Related skills

- [`add-bazel-shac-check`](../../../../../scripts/shac/bazel_lint/.agents/skills/add-bazel-shac-check/SKILL.md):
  shac Bazel lint internals.
- [`determining_bazel_visibility`](../determining_bazel_visibility/SKILL.md):
  visibility guidance that Planter remediations point to.
