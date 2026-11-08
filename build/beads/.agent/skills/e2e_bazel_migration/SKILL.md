---
name: e2e-bazel-migration
description: >
  Performs an end-to-end migration of GN packages to Bazel using Planter,
  covering pre-migration checks, workspace branching, migration package
  selection from the control table, package migration/verification, CL
  creation, and status reporting. Use this skill whenever the user
  asks to migrate targets using Planter (e.g., "migrate X using planter",
  "migrate the next X targets", "migrate targets in using planter"), run the
  automated GN-to-Bazel package migration workflow, or migrate packages from
  the control table.
---

## Terminology

* **control table** : The Google Sheet for tracking migration status, at
  go/fuchsia-bazel-migration-control-table. Use the `go-links` skill to resolve
  it to the sheet URL before using the `gsheets` skill. If the user specifies a
  different table in the prompt, use that one instead and do not resolve the
  link.

* **execution mode** : Where this skill runs the migrations. The [Pre-Migration
  Checks](#pre-migration-checks) decide it, in this order:
  * **in-container mode**: The skill itself runs inside a workbox (`WORKBOX` is
    set). The migrations run in `/workspace`, the checkout of that workbox, one
    **migration group** after another.
  * **host mode**: The user asks for it in the prompt (e.g., "run without
    workbox", "without workbox", "use host mode"), or
    `E2E_BAZEL_MIGRATION_HOST_MODE=1` is set. The migrations run on the host: in
    the current Fuchsia checkout, or with the Planter fleet in worktree slots
    when there are several **migration groups**. It is also the fallback when
    Workbox cannot be set up (see the **Workbox fallback warning**).
  * **workbox mode** (the default): Otherwise. Planter migrates each **migration
    group** in a workbox (a container with its own Fuchsia checkout, managed
    with `planter workbox`), and the host checkout is only read.

  The **execution mode** changes in one case only: if a Workbox step of the
  Pre-Migration Checks fails, the skill shows the **Workbox fallback warning**
  and continues in host mode. After the Pre-Migration Checks, it never changes.

* **Workbox fallback warning** : The warning shown to the user when a Workbox
  step of the Pre-Migration Checks fails and the skill falls back to host mode,
  with `<reason>` replaced by the reason of the failure:

  > ⚠️ Failed to set up Workbox: `<reason>`. Falling back to host mode (using
  > `fx-worktree`).

* **host-mode rerun hint** : The text added to the message of a workbox mode
  **STOP** condition that is caused by Workbox after the Pre-Migration Checks,
  with `<reason>` replaced by the reason of the failure:

  > Workbox could not be used: `<reason>`. To migrate without Workbox, rerun
  > the skill with "run without workbox" in the prompt, or with
  > `E2E_BAZEL_MIGRATION_HOST_MODE=1`.

* **migration branch** : The branch used for the Bazel migration in host and
  in-container modes. If the user does not specify a branch, it defaults to
  `migration_branch_default`. Workbox mode does not use it: the host checkout is
  not changed, and each workbox migrates from the latest `origin/main`.

* **batching by default** : The policy of grouping as many cohesive
  packages/targets as make sense into a single CL (typically up to 5 packages
  from the same area and target type) to minimize the total number of CLs
  created per engineer, decrease reviewer burden, and preserve CQ capacity. A
  single-target CL is used only when no other cohesive ready packages can be
  grouped with it under the grouping rules, or when the user explicitly asks to
  migrate a single package (e.g., "migrate one package").

* **migration package count** : The number of **migration packages** in this
  migration. If the user specifies the number of packages in the prompt (e.g.,
  "migrate one package" means 1, "migrate three packages" means 3, "migrate the
  next 4 targets in //src/foo" means 4), use that number as the target or
  maximum package count. If the user does not specify a count (e.g., "migrate
  ready targets", "migrate the next ready targets", or "migrate packages under
  //src/lib/foo"), the default is **batching by default**: select as many
  cohesive packages as make sense to migrate in one CL (up to 5 packages of the
  same area and target type according to the grouping rules). When more than one
  package is selected at the [Migration Package
  Selection](#migration-package-selection) step, it is the total number of
  selected packages, including the first one, not the number of additional
  packages. It is checked only before selecting a new package: the packages that
  have to be migrated together with a selected package are added even if the
  number of **migration packages** then exceeds it. When there is a **migration
  directory** and a count is specified, it caps the candidate packages selected
  under that directory.

* **migration directory** : The directory the user asks to migrate in the prompt
  (e.g., "migrate all the packages under `//src/lib/foo`", "migrate the next 3
  targets in `src/connectivity/wlan`"), relative to the root of the Fuchsia
  checkout, without a leading `//` or a trailing `/` (e.g., `src/lib/foo`). If
  the user does not specify a directory, there is no **migration directory**.

* **migration packages** : The packages selected at the [Migration Package
  Selection](#migration-package-selection) step: the first package found in the
  **control table** and, up to the **migration package count**, more packages.
  By default, they are closely related packages of the same area and target type
  that can be migrated together in the same CL (up to 5 packages). If there is a
  **migration directory**, they are the candidate packages under it that can be
  claimed (up to the **migration package count** if specified). Each **migration
  package** is one row of the **control table**.

* **migration groups** : The groups the **migration packages** are split into at
  the [Migration Package Selection](#migration-package-selection) step,
  according to the grouping rules in
  [scoping_changelists.md](../../../references/migration/scoping_changelists.md).
  The targets in the packages of a **migration group** and their package
  internal dependencies are migrated together by one Planter task and uploaded
  as one CL. Each **migration group** contains at most 5 cohesive packages of
  the same area and target type and adds at most roughly 500 new lines of code.
  The **migration packages** are split into as few **migration groups** as
  possible that satisfy the grouping rules (forming a single **migration group**
  when 5 or fewer packages already satisfy the grouping rules together), one CL
  for each **migration group**. They are migrated in parallel in workboxes in
  workbox mode, or by the Planter fleet in host mode, and one after another in
  in-container mode (see **execution mode**).

* **commit message guidelines** : The guidelines in this
  [migration_commit_message_guidelines.md](../../../references/migration/commit_message/guidelines.md)
  file.

* **buganizer ticket numbers** : For a **migration package**, the numeric issue
  IDs in the "Buganizer Ticket" column of its row in the **control table**.
  Accept bare numbers and any URL form ending in the numeric ID (e.g., `b/`,
  `fxbug.dev/`, `issues/`), and split on commas, semicolons, and whitespace
  (e.g., "http://b/1234567" gives "1234567"). Drop 571374004, the former parent
  tracking bug that earlier runs wrote when ticket creation failed. A package
  may have none. This skill never creates Buganizer tickets and never writes the
  "Buganizer Ticket" column.

* **status timestamp** : The current time in UTC, formatted as `YYYY-MM-DD HH:MM
  UTC` (e.g., `2026-10-16 04:31 UTC`).

* **CL URL** : The URL of a migration CL, `https://fxrev.dev/<CL number>` (e.g.,
  `https://fxrev.dev/1234567`). As soon as Planter creates the CL of a
  **migration group**, write it to the "Fuchsia Gerrit CL" column of each
  package of the **migration group** in the **control table**.

* **auth check command** : The command in the "Auth Check" section of the
  **helper document**. It exits non-zero if the user's credentials expire within
  one hour. If the user specifies a different command in the prompt, use that
  one instead.

* **workbox helper command** : The command in the "Command" section of the
  **helper document**. It checks and reports the Workbox setup of this machine
  for the workbox mode steps of this skill. If the user specifies a different
  command in the prompt, use that one instead. It is used only in workbox mode.

* **assign reviewer** : Whether Planter assigns a human reviewer to each created
  CL with `--assign-reviewer`, once a CQ dry run of the CL passes. Planter picks
  the reviewer by the reviewer routing table at
  go/fuchsia-bazel-migration-reviewers. It is enabled if the user asks for it in
  the prompt (e.g., "assign reviewers", "use --assign-reviewer"). Otherwise, it
  is disabled.

* **reviewer OOO threshold** : The number of working days a reviewer of a
  migration CL can be out of office before they are swapped for another reviewer
  from the reviewer routing table at go/fuchsia-bazel-migration-reviewers. If
  the user specifies it in the prompt, use that number. Otherwise, it defaults
  to 1.

* **Planter check script** : The script in the "Script" section of the **helper
  document**. It checks whether the installed Planter is up to date and updates
  it if it is not. If the user specifies a different script in the prompt, use
  that one instead.

* **Planter installation guide** : The document with the steps to install
  Planter, at go/planter_installation_guide. If the user specifies a different
  guide in the prompt, use that one instead.

* **helper document** : `e2e_bazel_migration_planter_workbox_helper.md` in the
  directory that holds the **Planter installation guide**, read through the
  read-only head source tree. It holds the **auth check command**, the **Planter
  check script** and the **workbox helper command**. If the user specifies a
  different helper document in the prompt, use that one instead.

* **workbox setup script** : `workbox-setup.sh` in the directory that holds the
  **Planter installation guide**, read through the read-only head source tree.
  It sets up Workbox on this machine. If the user specifies a different setup
  script in the prompt, use that one instead. It is used only in workbox mode,
  and only when Workbox is not set up yet.

* **Planter tag** : The `TAG: planter planter-<freshness>` footer that Planter
  writes into the commit message of every CL it creates (e.g., `TAG: planter
  planter-20261008-633b8b4b94a`). `<freshness>` identifies the Planter machinery
  that generated the CL. Only Planter knows it.

* **Planter warnings** : The environment, checkout, and machinery staleness
  lines Planter prints that start with `⚠️` (e.g., "This planter binary is out
  of date", "Fuchsia checkout ... is out of date", "Shared machinery ... has N
  uncommitted learning/machinery file(s)"), together with any indented command
  line that follows them, and `[machinery] warning:` lines. Exclude per-task
  retry or halt progress lines (such as `Attempt ... failed (will retry)` or
  `Task ... failed`). `planter version`, `planter run`, `planter status`, and
  `planter fleet` print them, and so does
  `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_freshness.sh`.

* **Planter freshness limit** : How long Planter may be out of date before this
  skill refuses to start new migrations. It is 24 hours, unless the user
  specifies a different limit in the prompt.
  `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_freshness.sh`
  evaluates the Planter binary and its shared machinery separately: new
  migrations are refused when a newer Planter binary is available and the
  installed binary's build baseline is older than this limit (or is a legacy
  unstamped release), or when the shared machinery checkout is behind
  `origin/main` by more than this limit and has uncommitted edits or local
  commits that prevent a clean fast-forward sync. If no newer binary exists and
  the shared machinery is up to date (or clean so `planter run` will auto-sync
  it), new migrations may start.

* **Planter model** : The model that Planter runs its agents on, or several
  models in order of preference, separated by commas. It is set only if the user
  specifies it in the prompt, and then passed to Planter with `--model`.
  Otherwise, do not pass `--model`: Planter picks the models from its own
  settings.

* **Planter model fallbacks** : The models that Planter falls back to when the
  models it runs on fail or run out of quota, separated by commas. It is set
  only if the user specifies it in the prompt, and then passed to Planter with
  `--model-fallbacks`. If the user asks for no fallbacks, it is set to empty,
  which is passed as `--model-fallbacks=""`. Otherwise, do not pass
  `--model-fallbacks`: Planter uses its own fallback models.

* **Planter speed force** : Whether Planter runs its coder, review assistant and
  explainer agents on a faster model first, with `--speed-force`. They fall back
  to the models of the other agents when the faster model fails or runs out of
  quota. The reviewers never use the faster model. It is enabled, unless the
  user turns it off in the prompt.

## Planter Rules

These rules apply to every step of this skill, and to any later work on a
migration CL (e.g., addressing review comments, fixing CQ failures, rebasing or
re-uploading).

1.  **Preserve the Planter tag. Never invent one.**
    * Planter generates the commit message of every migration CL. Commit and
      upload migration CLs with Planter (`scripts/planter_task_add_run.sh`,
      `scripts/planter_workbox_run.sh`, `planter run --gerrit` or `planter fleet
      --gerrit`); do not create the commits of new migration CLs yourself.
    * When you amend a commit message of a migration CL, keep the **Planter
      tag** exactly as Planter wrote it: copy the line verbatim from the current
      commit message (`git log -1 --format=%B`) or from the CL on Gerrit. Do not
      remove it, move it out of the footer block, or change any character of it.
      It goes right before the `Change-Id:` line.
    * In workbox mode, the migration CLs are not in the host checkout. If you
      edit the commit message of one of them, edit only the body: never change
      the subject (the first line) or the footers (the last paragraph, e.g.,
      `Test:`, `Bug:`, `Fixed:`, the **Planter tag** and `Change-Id:`). Planter
      reverts a changed subject and regenerates the `Test:` lines when it
      uploads the CL again. If the subject has an error, do not edit it; tell
      the user instead.
    * Never write a `TAG:` line yourself: do not make up a `<freshness>` value,
      copy one from another CL, or add a `TAG:` line to get past a check. If a
      migration CL has no **Planter tag**, leave it without one and tell the
      user that the CL has no **Planter tag** and was probably not generated or
      uploaded by Planter.

2.  **Relay the Planter warnings to the user.**
    * Whenever Planter or
      `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_freshness.sh`
      prints **Planter warnings**, relay them to the user verbatim, with the
      command Planter suggests (if any), in your next message to the user. Also
      list them in the "Planter Warnings" section of the notifications (see the
      [Notification Templates](assets/notification_templates.md)). Relay each
      distinct warning once; do not drop a warning because it looks unimportant.
    * In workbox mode, Planter runs inside the workboxes, so its **Planter
      warnings** are in the workbox logs (`planter workbox logs <box>`).
      `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_workbox_run.sh`
      collects them from the workboxes of this run and prints them in its
      warnings block, together with the warnings of its cleanup (e.g., a workbox
      that could not be destroyed, or the report of its orphan sweep). The
      Workbox setup
      (`build/beads/.agent/skills/e2e_bazel_migration/scripts/workbox_preflight.sh
      setup`) prints the warnings of the **workbox setup script** as `WARN`
      lines. Relay all of them as **Planter warnings**, in the same way.
    * Do not run the suggested commands yourself unless this skill or the user
      tells you to: they rewrite the user's checkouts (e.g., `jiri update`, `git
      rebase`) or upload machinery for review (`planter upload-machinery`).

3.  **Do not start new migrations with an out-of-date Planter.**
    * Before starting a new migration (adding a new Planter task), run
      `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_freshness.sh`
      from the root of the Fuchsia checkout. If the user specified a different
      **Planter freshness limit**, pass it in hours as `PLANTER_MAX_STALE_HOURS`
      to this script and to
      `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh`.
      If it exits with `2`, Planter or its shared machinery is out of date by
      more than the **Planter freshness limit**: do not start the migration.
      Tell the user how out of date Planter is and relay the **Planter
      warnings**, which say how to update it.
      `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh`
      runs this check itself and refuses to add the task.
    * Never bypass the check on your own: do not set `PLANTER_ALLOW_STALE`, or
      raise `PLANTER_MAX_STALE_HOURS` above the **Planter freshness limit**, and
      do not run `planter add-task` directly to get around it. Do so only if the
      user explicitly asks to start new migrations with an out-of-date Planter.
    * The check does not apply to existing migrations: you may still amend
      existing migration CLs (e.g., address review comments, fix CQ failures,
      fix the commit message, rebase and re-upload) with an out-of-date Planter.
      Relay the **Planter warnings** while doing so.

4.  **Never add `NO_IFTTT` lines. Preserve human-added `NO_IFTTT` lines.**
    * Never add a `NO_IFTTT=...` (or `NO_IFTTT:`) line to a commit message
      yourself. Humans must review any `LINT.IfChange`/`LINT.ThenChange` (IFTTT)
      warnings or errors, evaluate them, and add their own `NO_IFTTT=<reason>`
      message as appropriate.
    * When amending an existing migration CL where a human has already added a
      `NO_IFTTT=<reason>` line, preserve that line verbatim on a single
      unwrapped line in its own paragraph above a blank line before the
      contiguous footer block (`Bug:`, `Test:`, `Bazel-Migration-Target:`,
      `TAG:`, `Change-Id:`).

5.  **Wire package `test_suite()` targets into an aggregation `test_suite()`
    (never add per-package `bazel_test_suite()`).**
    * Every package that defines Bazel test targets exposes them via a package
      `test_suite(name = "tests" | "host_tests", tests = [":<test_target>",
      ...], ...)` as the first target in `BUILD.bazel` (always listing explicit
      test targets in `tests = [...]`, never `tests = []` or omitting `tests`,
      without `# @bazel2gn:skip` (`bazel2gn` ignores `test_suite()`), and with
      package-private individual `*_test` targets) and wires `"//<pkg>:tests"`
      or `"//<pkg>:host_tests"` into the area's aggregation `test_suite()`. If
      modifying an existing `test_suite()` that had an empty (or omitted)
      `tests` attribute, add all existing test targets in the package to `tests
      = [...]` in addition to any targets being added (or in the case of
      removal, start from the existing test targets in the package and remove
      the deleted entries), and raise a dispute/comment
      (`test_suite_layout/expanded_empty_test_suite`) requesting that a human
      ensure that all relevant tests have been included (agents must never
      resolve that comment). When a package defines both `"tests"` and
      `"host_tests"`, reference both in aggregation `test_suite()` targets in
      the same aggregation package (same `BUILD.bazel` file), adding the
      companion aggregation `test_suite()` there and in its `BUILD.gn`
      `bazel_test_suite()` if only one type exists yet.
    * Never add a per-package `bazel_test_suite("bazel_tests")` in an individual
      package's `BUILD.gn`. If no existing aggregation `test_suite()` can be
      found for the package's area, define the package `test_suite()` in
      `BUILD.bazel` (omitting `visibility` until the aggregation package is
      known; never guess an aggregation directory). In interactive sessions, ask
      the user which aggregation `test_suite()` (or where a new one, with a
      matching `bazel_test_suite()` in `BUILD.gn`, should be added if none
      exists) the package's `test_suite()`(s) should be added to, and suggest
      asking the Build team if they do not know; in unattended Planter runs,
      report the unresolved finding in `CoderReport.disputed_findings` and
      `summary` for the reviewer.

6.  **Pass the model flags to Planter only as the user asks.**
    * Every `planter run` and `planter fleet` command that runs Planter's agents
      gets `--speed-force` if **Planter speed force** is enabled, or
      `--speed-force=false` if it is disabled. Never leave it out: without it,
      Planter's own settings decide whether it uses speed force.
    * These commands get `--model="<Planter model>"` only if the **Planter
      model** is set, and `--model-fallbacks="<Planter model fallbacks>"` only
      if the **Planter model fallbacks** are set. Never choose a model yourself:
      if the user did not specify the models, do not pass `--model` or
      `--model-fallbacks`.
    * `scripts/planter_task_add_run.sh` and `scripts/planter_workbox_run.sh`
      pass `--speed-force` by default, and no model flags. Pass `--model` and
      `--model-fallbacks` to these scripts only if the **Planter model** and the
      **Planter model fallbacks** are set, and `--no-speed-force` only if
      **Planter speed force** is disabled: the scripts then pass
      `--speed-force=false`.

## End-to-End Bazel Migration

If any step fails, refer to [Error Handling](#error-handling) to handle the
error condition.

### Pre-Migration Checks

Refer to `references/pre_migration_checks.md` to verify if the required criteria
for the Bazel migration are fulfilled. It also decides the **execution mode**,
and in workbox mode checks Workbox and sets it up with the user's consent.

  * If this step fails, it's a **STOP** condition.
  * In workbox mode, if a Workbox step fails (e.g., the **workbox helper
    command** cannot be found or started, commands cannot run outside the
    terminal sandbox, Workbox is blocked, a credential for the setup is missing,
    the **workbox setup script** cannot be found or read or changed after
    consent, the user declines the setup, the setup fails, or the Gerrit
    credentials cannot be staged for Workbox), the skill does not stop: it shows
    the **Workbox fallback warning** and continues in host mode. An out-of-date
    Planter is a **STOP** condition in every **execution mode**.

### Workspace Branching

Refer to `references/workspace_branching.md` to prepare the Fuchsia checkout for
the **execution mode**: in host and in-container modes, to ensure you are on the
**migration branch**; in workbox mode, to check the host checkout without
changing it (Host Checkout Readiness), because the migrations run in the
checkouts of the workboxes.

  * If this step fails, it's a **STOP** condition.

### Migration Package Selection

Refer to `references/migration_package_selection.md` to select the **migration
packages** and split them into **migration groups**.

  * Never select packages under the excluded directories listed in
    `references/migration_package_selection.md` (`//zircon` and
    `//src/zircon`).
  * If this step fails, it's a **STOP** condition.

### Bazel Migration and CL Creation

Refer to `references/bazel_migration_planter.md` to migrate the targets within
the **migration packages**, one CL for each **migration group**, in the
**execution mode** decided by the Pre-Migration Checks.

Planter generates the commit message of each CL from the Planter task
parameters, which follow the **commit message guidelines**. After Planter
creates a CL, do not review its commit message against the **commit message
guidelines** or edit it, in any **execution mode**.

  * If this step fails because it failed to get the generated CL number, because
    no CL is created for any of several **migration groups**, or because Planter
    is out of date by more than the **Planter freshness limit**, it's a **STOP**
    condition.
  * In workbox mode, if this step fails because every **migration group** failed
    to queue or was dropped, it's a **STOP** condition. If it fails because no
    workbox can be used, because no Planter runner can be started in any
    workbox, or because another run of this skill holds the Workbox lock for too
    long, it's a **STOP** condition with the **host-mode rerun hint**.
  * If this step fails because of other reasons, it's a **PLANTER ERROR**
    condition.
  * In every **execution mode**, if Planter refuses to queue a **migration
    group** because one of its packages is already claimed by another Planter
    task (its output contains `is already claimed by task`), drop that
    **migration group**, probably claimed by another run of this skill. Never
    change the rows of its packages in the **control table**, in this step or in
    any later error handling, because they may belong to that other run. List
    the dropped **migration groups** in the notification.

## Reviewer Availability

Refer to `references/reviewer_availability.md` to check the human reviewers of
each created CL, and swap each reviewer who is out of office for longer than the
**reviewer OOO threshold** for an available reviewer from the reviewer routing
table.

  * If this step fails, ignore the failure and continue.

If the user asks only about the reviewers of existing migration CLs (e.g., "my
migration CL's reviewer is OOO", "check the reviewers of my migration CLs"),
skip all the other steps and run only this step. Use the CL numbers given in the
prompt, or find the user's open migration CLs with `curl -s
"https://fuchsia-review.googlesource.com/changes/?q=owner:<ldap>@google.com+status:open+intitle:bazel_migration"`,
where `<ldap>` is the user's LDAP.

## Error Handling

Refer to `references/error_handling.md` to handle the reported error condition.

## Notifications

Refer to `references/notifications.md` to notify user.
