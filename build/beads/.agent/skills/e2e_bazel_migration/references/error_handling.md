# Error Handling

This document handles error conditions (e.g., STOP). For non-error conditions, refer to the notifications.md file.

## Common Error Handling Steps For All Error Conditions
* Use "gmail" skill: refer to the "Interrupted Notification Template" in the [Notification Templates](../assets/notification_templates.md) to draft the "Interrupted" notification and send it to the user.
* Use the "gsheets" skill: in the **control table**, for each **migration package**, change "Migration Status" to "blocked", write the reason of the failed step to "Failure Reason", and set "Last Status Update" to the **status timestamp**.
  * If no **migration package** has been selected yet, skip this step.
  * If the failed step stopped because Planter is out of date before any migration task ran, revert each claimed **migration package** instead of marking it "blocked": change "Migration Status" back to "ready_for_migration", clear "Owner", and set "Last Status Update" to the **status timestamp**. In workbox mode, also revert them this way when the failed step stopped because no workbox could be used or no Planter runner could be started (see [Workbox STOP Conditions](#workbox-stop-conditions)).
  * A package dropped or blocked during [Migration Package Selection](migration_package_selection.md) is not a **migration package**; do not update its row.
  * Do not update the rows of the packages of a **migration group** dropped because one of its packages is already claimed by another Planter task (its `planter_task_add_run.sh` output contains `is already claimed by task`), in every **execution mode**. They may belong to another live run.
  * Skip the packages already set to "blocked" by [Migration Group Failure](#migration-group-failure), so their "Failure Reason" keeps the reason of their **migration group**.


## Error Handling Steps specific to the "STOP and CLEANUP" Condition
* Common error handling steps.
* In host mode and in-container mode, use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dirs>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_host_mode.md](bazel_migration_host_mode.md), and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces.
* In workbox mode, Planter does not change the current Fuchsia checkout, so do not reset it. Clean up as described in [Workbox STOP Conditions](#workbox-stop-conditions).


## Error Handling Steps specific to the "PLANTER ERROR" condition
* Common error handling steps.
* In host mode and in-container mode, use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dirs>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_host_mode.md](bazel_migration_host_mode.md), and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces.
* Phase 1 only: the two steps below apply to the result of migrating and uploading the **migration group**. In workbox mode, they apply only to a `RESULT` line of `scripts/planter_workbox_run.sh upload`, never to a `REVIEW` line of `scripts/planter_workbox_run.sh review` (see [Workbox Results](#workbox-results)).
  * If CL was not created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task <task-id>` to delete the generated Planter task. If Planter returns an error, ignore it.
  * If CL has been created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task --abandon <task-id>` to delete the generated Planter task and the CL.
  * In workbox mode, if `delete-task` refuses because a process still holds the task, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" pause <task-id>` first, and then run `delete-task` again.


## Migration Group Failure

When there are several **migration groups**, a failed **migration group** is handled by itself, and the other **migration groups** continue (see [Several Migration Groups](bazel_migration_host_mode.md#several-migration-groups) in host mode, and the [Workbox Flow](bazel_migration_planter.md#workbox-flow) in workbox mode). For each failed **migration group**:
* Use the "gsheets" skill: in the **control table**, for each package of the failed **migration group**, change "Migration Status" to "blocked", write the reason of the failure to "Failure Reason", and set "Last Status Update" to the **status timestamp**. If it fails to update the **control table**, ignore the failure.
* Phase 1 only (in workbox mode, only for a `RESULT` line of `upload`, never for a `REVIEW` line of `review`): if its Planter task was queued and has no CL, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task <task-id>` to delete the task. If it has a CL, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task --abandon <task-id>` to delete the task and abandon the CL. If Planter returns an error, ignore it. In workbox mode, if `delete-task` refuses because a process still holds the task, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" pause <task-id>` first, and then run `delete-task` again.
* Do not send the "Interrupted" notification for it; the failed **migration groups** are listed in the "Completed" notification instead.

A **migration group** dropped because one of its packages is already claimed by another Planter task is not a failed **migration group**: do not update its rows, and do not run `delete-task` for it, because its task ID may name the task of the other run. List it in the "Already Claimed Migration Groups" of the notifications instead.

If the Bazel Migration and CL Creation step fails or stops after workboxes were prepared or worktree slots were allocated, release them before handling the error condition:
* In workbox mode, run `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_workbox_run.sh teardown --run-id=<run ID>` as described in [Workbox Flow](bazel_migration_planter.md#workbox-flow), unless `upload` or `review` has already torn the workboxes down.
* In host mode, refer to [Release Worktree Slots](bazel_migration_host_mode.md#release-worktree-slots) to release them.


## Workbox Mode

In workbox mode, the error conditions of the Workbox steps of the [Pre-Migration Checks](pre_migration_checks.md) and of the [Workbox Flow](bazel_migration_planter.md#workbox-flow) are handled as described below, in addition to the steps above.

A STOP condition "with the **host-mode rerun hint**" ends its reason with the **host-mode rerun hint**:

> Workbox could not be used: `<reason>`. To migrate without Workbox, rerun the skill with "run without workbox" in the prompt, or with `E2E_BAZEL_MIGRATION_HOST_MODE=1`.

A failed Workbox step of the Pre-Migration Checks does not stop the run: the skill shows the **Workbox fallback warning** and continues in host mode, as described in [Workbox Fallback](pre_migration_checks.md#workbox-fallback). After the Pre-Migration Checks, never switch to host mode by yourself, even if the host-mode prerequisites happen to be met. If the run stops after Planter tasks were queued, clean them up first as described below, so that a rerun in host mode starts clean.

### Workbox STOP Conditions

| Condition | Condition type | Control table | Cleanup |
| :-------- | :------------- | :------------ | :------ |
| Failed to get the workbox helper command, the workbox helper failed to start, or it is incompatible; Workbox is blocked (`BLOCKED` reasons); Workbox commands cannot run outside the terminal sandbox; a credential check failed (`USER_COMMAND`); the **workbox setup script** was not found (the **Planter installation guide** could not be resolved or read, is not a file in the source tree, or names a different setup script than the fleet guide, or the script does not exist or cannot be read); Workbox setup was declined; the workbox setup script changed after consent; failed to back up the agent configuration before setup; failed to set up Workbox; the check after the setup failed; failed to stage Gerrit credentials for Workbox (`planter workbox creds`); another skill run holds the Workbox lock during the [Orphan Sweep](pre_migration_checks.md#orphan-sweep) | Workbox fallback: show the **Workbox fallback warning** and continue in host mode (not a STOP) | Unchanged: no **migration package** is selected yet | None |
| `prepare` times out on the Workbox lock (`STOP_REASON=Another skill run holds the Workbox lock`) | STOP with the **host-mode rerun hint** | Revert the claimed **migration packages** to "ready_for_migration" | None |
| The Planter freshness check exits with `2` in the Pre-Migration Checks | STOP | Skipped | None |
| `prepare` finds no usable workbox | STOP with the **host-mode rerun hint** | Revert the claimed **migration packages** to "ready_for_migration" | `teardown` |
| Planter is out of date while queuing (`STALE:`) | STOP | Revert the claimed **migration packages** to "ready_for_migration" | `delete-task` for every task queued in this run, then `teardown` |
| Every **migration group** failed to queue or was dropped | STOP | As in [Common Error Handling Steps](#common-error-handling-steps-for-all-error-conditions), without the rows of the dropped **migration groups** | `teardown` |
| `upload` exits with `12` (no Planter runner could be started) | STOP with the **host-mode rerun hint** | Revert the claimed **migration packages** to "ready_for_migration" | `delete-task` for every task queued in this run (none has a CL), then `teardown` |
| A `RESULT` line other than `UPLOADED`, `UPLOADED_WIP`, or `LANDED` | PLANTER ERROR if there is one **migration group**; otherwise [Migration Group Failure](#migration-group-failure) for that **migration group** | See [Workbox Results](#workbox-results) | See [Workbox Results](#workbox-results) |
| Several **migration groups** and no CL is created for any of them | STOP | The **migration groups** are already handled | None |
| `UPLOADED` but no CL number | STOP | "blocked" | None |

### Workbox Results

The table below applies only to the `RESULT` lines of `upload` (phase 1). The `REVIEW` lines of `review` (phase 2) never reach it: they only set the reviewer text of the notification (see the [Notification Templates](../assets/notification_templates.md)). A phase-2 result never abandons the CL, never deletes the task, and never changes the row from "CL_created".

| `RESULT` | Control table | Reviewer | Task cleanup |
| :------- | :------------ | :------- | :----------- |
| `UPLOADED` | "CL_created", with the **CL URL** | Not assigned if **assign reviewer** is disabled; phase 2 (`review`) if it is enabled | Keep |
| `UPLOADED_WIP` | "CL_created", with the **CL URL** | No (`REVIEW_SKIPPED_WIP`) | Keep |
| `LANDED` | "CL_created", with the **CL URL** | No | Keep |
| `FAILED`, `STRANDED`, `TIMEOUT`, `NOT_FINISHED`, `PAUSED` | "blocked", with the reason of the `RESULT` line | - | The script captured the logs first. Then, if the task has a CL, `delete-task --abandon <task-id>` (phase 1 only); otherwise `delete-task <task-id>`. Run `pause <task-id>` first if a process still holds the task |
| `NOT_STARTED` | "blocked", with the reason "no Workbox slot ran it" | - | `delete-task <task-id>` |
