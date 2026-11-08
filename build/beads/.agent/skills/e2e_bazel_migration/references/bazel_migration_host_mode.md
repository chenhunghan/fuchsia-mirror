# Bazel Migration and CL Creation: Host Mode

Used when the user opts in (see [Execution Mode](pre_migration_checks.md#execution-mode)), or when a Workbox step of the Pre-Migration Checks fails and the skill falls back to host mode (see [Workbox Fallback](pre_migration_checks.md#workbox-fallback)). In host mode, the Planter tasks run in the current Fuchsia checkout on the **migration branch** and, for several **migration groups**, in worktree slots with the Planter fleet. Host mode is never entered automatically: if Workbox cannot be used, the skill stops and the user reruns it in host mode. In in-container mode, [One Migration Group](#one-migration-group) is also used, in `/workspace` (see [Bazel Migration and CL Creation](bazel_migration_planter.md)).

The `<planter>`, the [Planter Rules](../SKILL.md#planter-rules), and the out-of-date Planter rule in [Bazel Migration and CL Creation](bazel_migration_planter.md) apply in host mode too.

The `<model flags>` below are `--speed-force` if **Planter speed force** is enabled or `--speed-force=false` if it is disabled, `--model="<Planter model>"` if the **Planter model** is set, and `--model-fallbacks="<Planter model fallbacks>"` if the **Planter model fallbacks** are set. Leave out `--model` and `--model-fallbacks` when they are not set.

The steps below need the host-mode checks of the [Pre-Migration Checks](pre_migration_checks.md) and the [Workspace Branching](workspace_branching.md): the "fx-worktree" skill, the Antigravity terminal, and the **migration branch**. [Assign a Reviewer](#assign-a-reviewer) runs `<planter> run --cq` on the host.

## Planter Task Parameters

The Planter task parameters are the same in every **execution mode**: create them as described in [Planter Task Parameters](bazel_migration_planter.md#planter-task-parameters).

## Already Claimed Packages

If the output of `scripts/planter_task_add_run.sh` contains `is already claimed by task` (in [One Migration Group](#one-migration-group) step 2 or [Several Migration Groups](#several-migration-groups) step 3), another run has already claimed a package of the **migration group** in Planter. This takes precedence over the other error rules of that step:
* Drop the **migration group** with the note: "claimed by Planter task `<task-id>` (another run?)", where `<task-id>` is the task named in the output.
* Do not update the **control table** rows of its packages, now or in any later error handling step, because they may belong to another live run.
* Do not run `<planter> delete-task` for it: no task was added for it, and its task ID may name the task of the other run.
* List it in the "Already Claimed Migration Groups" of the notification (see the [Notification Templates](../assets/notification_templates.md)).
* If every **migration group** fails or is dropped, fail the Bazel Migration and CL Creation step as the step says, without updating the rows of the dropped **migration groups**.

## One Migration Group

1. Record the current commit with `git rev-parse HEAD`.

2. From the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh` with the [Planter task parameters](#planter-task-parameters) of the **migration group** to migrate it.
   * `FUCHSIA_DIR`: The root of the Fuchsia checkout on the **migration branch**. Always set it, because the script otherwise defaults to `$HOME/fuchsia`, which may be a different checkout.
   * If the script refuses because Planter is out of date (its output has a `STALE:` line), fail the Bazel Migration and CL Creation step with the reason given above for an out-of-date Planter, and relay the **Planter warnings**. This is a **STOP** condition.
   * Otherwise, if the script returns error, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate `<package_names>`."

3. Run `git log -1 --format=%B` to get the commit message of the commit Planter just made, and then use "fuchsia-gerrit-cli" skill to get the CL number with the `Change-Id` in the retrieved commit message.
   * If it fails to get the CL number, fail the Bazel Migration and CL Creation step with the reason: "Fail to get the generated CL for `<package_names>` migration."

4. Use "gsheets" skill: Update the "Migration Status" of each **migration package** in the **control table** to "CL_created", write the **CL URL** to its "Fuchsia Gerrit CL" column, and set "Last Status Update" to the **status timestamp**.
   * If it fails to update the **control table**, ignore the failure.

5. If **assign reviewer** is enabled, refer to [Assign a Reviewer](#assign-a-reviewer) to assign a reviewer to the CL.

## Several Migration Groups

The Planter fleet runs the Planter tasks of the **migration groups** in parallel, one task in each registered Fuchsia checkout (slot) at a time, and uploads one CL for each **migration group**. See the fleet guide that the **Planter installation guide** links to for how the fleet works.

A **migration group** fails when its Planter task cannot be queued, or the task is not uploaded to Gerrit. A failed **migration group** does not stop the other **migration groups**; handle it as described in [Migration Group Failure](error_handling.md#migration-group-failure).

1. Record the current commit with `git rev-parse HEAD`.

2. Run `<planter> slot list` to check the registered slots.
   * If slots are registered, use them, and do not allocate worktree slots.
   * If no slot is registered, refer to [Allocate Worktree Slots](#allocate-worktree-slots) to allocate the slots with the "fx-worktree" skill.

3. For each **migration group**, from the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh --add-only` with the [Planter task parameters](#planter-task-parameters) of the **migration group** to queue its Planter task without running it.
   * If the script refuses because Planter is out of date (its output has a `STALE:` line), do not queue the remaining **migration groups**: delete the tasks already queued in this step with `<planter> delete-task <task-id>`, release the worktree slots allocated in step 2 as described in [Release Worktree Slots](#release-worktree-slots), and fail the Bazel Migration and CL Creation step with the reason given above for an out-of-date Planter. This is a **STOP** condition.
   * If the script returns error, the **migration group** fails with the reason: "Planter failed to queue the migration of `<package_names>`."
   * If every **migration group** fails, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to queue the migration groups."

4. From the root of the Fuchsia checkout, run `<planter> fleet --gerrit --until-uploaded <model flags> --task=<task IDs>` to migrate the queued **migration groups** in parallel and upload their CLs. The `<task IDs>` are the task IDs of the queued **migration groups**, separated by `,`.
   * The fleet exits when every task is uploaded or halted. It can take hours; run it in the background and wait for it to exit. Do not treat a slow run as a failure.
   * If the fleet fails to start, every queued **migration group** fails with the reason: "Planter fleet failed to start."

5. Run `<planter> status` to get the result of each queued **migration group**. The line of each task has the form `- <task ID> target=... status=<status> phase=<phase> cl=<CL number> ...`.
   * If the status is `AWAITING_HUMAN_REVIEW` or `LANDED` and the `cl=` field has the CL number, the CL of the **migration group** is created.
   * Otherwise, the **migration group** fails with the reason: "Planter failed to migrate `<package_names>`." Run `<planter> inspect --task=<task ID>` to find the cause and add it to the reason.

6. For each **migration group** whose CL is created:
   1. Use "gsheets" skill: Update the "Migration Status" of each package of the **migration group** in the **control table** to "CL_created", write the **CL URL** to its "Fuchsia Gerrit CL" column, and set "Last Status Update" to the **status timestamp**.
      * If it fails to update the **control table**, ignore the failure.
   2. If **assign reviewer** is enabled, refer to [Assign a Reviewer](#assign-a-reviewer) to assign a reviewer to the CL. Run it for the CLs in parallel, at most one for each slot, before the slots are released in the next step, because fixing CQ failures needs a slot.

7. If worktree slots were allocated in step 2, refer to [Release Worktree Slots](#release-worktree-slots) to release them.

8. Run `git reset --hard <recorded commit>` and then `git clean -fd -- <migration package dirs>` to remove what the fleet left in the current Fuchsia checkout. The `<recorded commit>` is the commit recorded in step 1, and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces. The created CLs are on Gerrit, so no work is lost.

9. If no CL is created for any **migration group**, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate all the migration groups." The failed **migration groups** have already been handled, so this is a **STOP** condition, not a **PLANTER ERROR** condition.

### Allocate Worktree Slots

Each slot needs its own Fuchsia checkout with a working build directory, because Planter builds in it. Use the "fx-worktree" skill to claim one worktree for each slot from the worktree pool of the current Fuchsia checkout; `fx worktree add` provisions a new worktree when no free one is available.

1. The number of worktree slots is the number of **migration groups**, but at most 4, or at most the number of slots the user specifies in the prompt. Builds dominate the run time, so more slots than this slow each other down.
2. For each worktree slot, choose the name `planter-<n>`, where `<n>` is 1, 2, ... (e.g., `planter-1`), skipping the names already used by `fx worktree list`.
   1. Run `fx worktree add <name>` from the root of the current Fuchsia checkout, and `fx worktree locate <name>` to get its path.
   2. If the worktree has no build directory (`fx get-build-dir` in the worktree fails or prints a directory without `args.gn`), configure it like the current Fuchsia checkout: run `fx status` in the current Fuchsia checkout to get its product and board, and run `fx set <product>.<board>` in the worktree.
   3. Run `<planter> slot add <path> --id=<name>` to register the worktree as a slot.
   * If any of these fails, run `fx worktree remove <name>` and skip this worktree slot. If the worktree failed to be configured in step 2, also report the error to the user.
3. If no worktree slot can be allocated, the fleet uses one slot in the current Fuchsia checkout and migrates the **migration groups** one after another. Tell the user, and continue.

### Release Worktree Slots

Release the worktree slots allocated in [Allocate Worktree Slots](#allocate-worktree-slots), after the failed **migration groups** are handled as described in [Migration Group Failure](error_handling.md#migration-group-failure), so that no Planter task holds a slot. For each allocated worktree slot `<name>`:

1. Run `<planter> slot remove <name>` to unregister the slot.
2. Run `fx worktree remove <name>` to return the worktree to the pool.
* If either command fails, ignore the failure, and report the slots that could not be released to the user.

## Assign a Reviewer

Assign a human reviewer to a created CL with Planter's `--assign-reviewer`. Planter assigns the reviewer only after a CQ dry run of the CL passes, so this also runs CQ on the CL and fixes CQ failures.

1. From the root of the Fuchsia checkout, run `<planter> run --gerrit --cq --assign-reviewer <model flags> --task=<task ID>`, where `<task ID>` is the task ID of the **migration group** of the CL. Planter votes Commit-Queue+1, fixes CQ failures and uploads new patchsets (at most 3 rounds), and assigns a reviewer once a CQ dry run passes.
   * A CQ dry run can take hours; run the command in the background and wait for it to exit. Do not treat a slow run as a failure.
2. Use "fuchsia-gerrit-cli" skill to get the reviewers of the CL, and record the assigned reviewer for the "Completed" notification.
   * If the command fails or no reviewer is assigned (e.g., CQ still fails after the fix rounds), do not fail the Bazel Migration and CL Creation step: the CL is created. Record "not assigned" with the reason for the "Completed" notification, and continue. The user can assign one later with `<planter> assign-reviewer <CL number>`.
3. If a reviewer is assigned, refer to [Reviewer Availability](reviewer_availability.md) to check the reviewer, swap them if they are out of office, and record the swap for the "Completed" notification.
