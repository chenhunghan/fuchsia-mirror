# Bazel Migration and CL Creation

Each **migration group** is migrated by one Planter task and uploaded as one CL. How the Planter tasks run depends on the **execution mode** decided in the [Pre-Migration Checks](pre_migration_checks.md#execution-mode):
* In workbox mode (the default), refer to [Workbox Flow](#workbox-flow) to migrate the **migration groups** in parallel in workboxes, whether there is one **migration group** or several.
* In host mode, refer to [bazel_migration_host_mode.md](bazel_migration_host_mode.md): [One Migration Group](bazel_migration_host_mode.md#one-migration-group) if there is one **migration group**, or [Several Migration Groups](bazel_migration_host_mode.md#several-migration-groups) to migrate them in parallel with the Planter fleet.
* In in-container mode, refer to [One Migration Group](bazel_migration_host_mode.md#one-migration-group) in `/workspace`. If there are several **migration groups**, migrate them one after another with it. A failed **migration group** does not stop the others; handle it as described in [Migration Group Failure](error_handling.md#migration-group-failure). Before the next **migration group**, run `git reset --hard <recorded commit>` and then `git clean -fd -- <package dirs of the previous migration group>`, so that its CL does not include the commit of the previous one. If no CL is created for any **migration group**, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate all the migration groups." This is a **STOP** condition.

The `<planter>` below is the `planter` binary resolved with `"$(command -v planter || echo "$HOME/.local/bin/planter")"`, because `~/.local/bin` may not be on `PATH`.

Follow the [Planter Rules](../SKILL.md#planter-rules) in every step below: keep the **Planter tag** verbatim and never invent one, relay the **Planter warnings** that Planter prints (including the ones `planter fleet` prints while it runs), and do not start new migrations with an out-of-date Planter.

`scripts/planter_task_add_run.sh` checks the Planter freshness with `scripts/planter_freshness.sh` before it adds a task. If it refuses because Planter is out of date by more than the **Planter freshness limit** (its output has a `STALE:` line), fail the Bazel Migration and CL Creation step with the reason: "Planter is out of date by `<age>`, more than the `<limit>` limit for new migrations", and relay the **Planter warnings**. This is a **STOP** condition, not a **PLANTER ERROR** condition: no task was added.

## Planter Task Parameters

For a **migration group**, the `<package_names>` are the GN labels of the packages of the **migration group**, separated by `, `. Create the parameters of its Planter task as below:
* `--target-dir`: The directories of the packages of the **migration group**, separated by `,` without spaces (e.g. `src/lib/foo,src/lib/bar`). Order them as below:
  * If there is a **migration directory** and it is one of the packages of the **migration group**, put it first, followed by the other directories in the order they were selected.
  * Otherwise, if one of the packages of the **migration group** is the sole same-area GN consumer of another package of the **migration group** (pattern A) or the root target of other packages of the **migration group** as its private helpers (pattern B) in the [Grouping Rules](migration_package_selection.md#grouping-rules) (Other packages, such as direct test subdirectories (pattern D), do not prevent this), put its directory first, followed by the other directories in the order they were selected.
  * Otherwise, list the directories in the order they were selected.
* `--title`: Refer to **commit message guidelines** to create the title for the migration of the **migration group** and provide the created title here. Planter's commit subject check (`build/beads/planter/machinery/checks/commit_message_format.sh`) only accepts `[bazel_migration][<area>] //<dir>` or `[bazel_migration] //<dir>`, with a single `//` label and no spaces after it. So if there is more than one package in the **migration group**, use the label of the first directory in `--target-dir` (e.g., `[bazel_migration][starnix] //src/starnix/lib/selinux`), instead of the multi-directory forms in the **commit message guidelines** (e.g., `//src/starnix/lib: selinux, ebpf`). List all the migrated directories in `--desc`.
* `--task-id`: The components of the first directory in `--target-dir` joined by `_` (e.g. the task-id of `src/connectivity/bluetooth/lib/bt-obex/objects` is `src_connectivity_bluetooth_lib_bt-obex_objects`, and the task-id of `src/lib/foo,src/lib/bar` is `src_lib_foo`). The **migration groups** do not share packages, so their task IDs are different.
* `--desc`: Refer to **commit message guidelines** to create the commit message body for the migration of the **migration group** and provide the created message body here.
  * Put the `Fixed:` and `Bazel-Migration-Target:` footers in the last paragraph of `--desc`.
    * Add a `Fixed: <ticket>` footer for each of the **buganizer ticket numbers** of the packages of the **migration group**, so landing the CL closes the tickets created by earlier runs. If several packages have the same ticket number, add it only once. If a ticket number is also one of the **buganizer ticket numbers** of a package in another **migration group**, use `Bug: <ticket>` instead, so the first CL to land does not close it. These tickets are valid and relevant by definition; do not look them up. This overrides the bug ID verification requirement in the **commit message guidelines** for these tickets.
    * Packages without **buganizer ticket numbers** get no `Fixed:` or `Bug:` footer.
  * Leave out the `Test:` footer, because Planter adds its own test footers.
  * Leave out the `TAG:` footer and the `Change-Id:` footer. Planter adds the **Planter tag** and replaces any `TAG:` line in `--desc` with it.
* `--model`, `--model-fallbacks` and `--no-speed-force`: The script passes `--speed-force` to Planter, and no model flags. Add `--model="<Planter model>"` only if the **Planter model** is set, `--model-fallbacks="<Planter model fallbacks>"` only if the **Planter model fallbacks** are set, and `--no-speed-force` only if **Planter speed force** is disabled (see the [Planter Rules](../SKILL.md#planter-rules)).

## Workbox Flow

In workbox mode, `scripts/planter_workbox_run.sh` runs the Planter tasks of the **migration groups** in workboxes, one Planter runner in each workbox, and uploads one CL for each **migration group**. Each workbox is a container with its own Fuchsia checkout and build directory, so Planter never changes the current Fuchsia checkout.

A **migration group** fails when its Planter task cannot be queued, or the task is not uploaded to Gerrit. A failed **migration group** does not stop the other **migration groups**; handle it as described in [Migration Group Failure](error_handling.md#migration-group-failure). If there is one **migration group**, its failure is a **PLANTER ERROR** condition instead. The STOP conditions of this flow, and the actions for each result, are in [Workbox Mode](error_handling.md#workbox-mode).

### Running the Workbox Scripts

`<run>` below is `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_workbox_run.sh`. Run it from the root of the current Fuchsia checkout, with the [Workbox Commands](pre_migration_checks.md#workbox-commands) conventions:
* Start each command with `export PATH="$HOME/.local/bin:$PATH"`, and set `E2E_WB_HELPER` to the **workbox helper command**.
* `prepare`, `upload`, `review`, and `teardown` are long-running. Run each of them outside the terminal sandbox, in the background. End your turn and wait for the completion notification. If you are told that it is still running, check its status once and wait again. Never kill it: a killed run leaves workboxes behind, and only a later orphan sweep can recover them.
* Run the short calls (`touch` and `record-task`) outside the terminal sandbox too.
* If a command prints `STOP_REASON=<reason>` and exits with `12`, fail the Bazel Migration and CL Creation step with that reason. It is a **STOP** condition with the **host-mode rerun hint** (see [Workbox STOP Conditions](error_handling.md#workbox-stop-conditions)).
* Relay the lines between `PLANTER_WARNINGS_BEGIN` and `PLANTER_WARNINGS_END` as **Planter warnings**, and record the `BOXES_LEAKED=` workboxes for the notification. Both are printed by `upload`, `review`, and `teardown`.
* `upload` and `review` start the Planter runners with `--speed-force`, and no model flags. Add `--model="<Planter model>"` to them only if the **Planter model** is set, `--model-fallbacks="<Planter model fallbacks>"` only if the **Planter model fallbacks** are set, and `--no-speed-force` only if **Planter speed force** is disabled.

### Workbox Steps

1. Run `<run> prepare --slots=<K> --tree="$TREE" --option=<option>` to create or reuse the workboxes.
   * `<K>`: The number of **migration groups**, but at most 4, or at most the number of slots the user specifies in the prompt. Builds dominate the run time, so more slots than this slow each other down.
   * `$TREE`: The `TREE=` value printed by `scripts/workbox_preflight.sh check` in [Workbox Readiness and Setup](pre_migration_checks.md#workbox-readiness-and-setup).
   * `<option>`: `A` if **assign reviewer** is disabled, or `C` if it is enabled.
   * Add `--setup-ran` if the **workbox setup script** ran in this migration, so the run records the setup for the notification. Add `--keep-boxes` only if the user asks to keep the workboxes.
   * Record the `RUN_ID=` value as `<run ID>`. If `prepare` stops because no workbox is available, run `<run> teardown --run-id=<run ID>` if a `<run ID>` was printed.

2. For each **migration group**, in order:
   1. Run `<run> touch --run-id=<run ID>`.
   2. From the root of the current Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh --add-only` with the [Planter task parameters](#planter-task-parameters) of the **migration group** to queue its Planter task without running it.
   3. If the script succeeds, run `<run> record-task --run-id=<run ID> --task-id=<task-id>`.
   * If the script refuses because Planter is out of date (its output has a `STALE:` line), do not queue the remaining **migration groups**: delete every task queued in this run with `<planter> delete-task <task-id>` (none of them has a CL yet), run `<run> teardown --run-id=<run ID>`, and fail the Bazel Migration and CL Creation step with the reason given above for an out-of-date Planter. This is a **STOP** condition.
   * If the output contains `is already claimed by task`, another run has already claimed a package of the **migration group** in Planter. Drop the **migration group** with the note: "claimed by Planter task `<task-id>` (another run?)", where `<task-id>` is the task named in the output. Do not update the **control table** rows of its packages, now or in any later error handling step, and do not run `delete-task` for it, because they may belong to another live run. List it in the "Already Claimed Migration Groups" of the notification.
   * If the script returns another error, the **migration group** fails with the reason: "Planter failed to queue the migration of `<package_names>`."
   * If every **migration group** fails or is dropped, run `<run> teardown --run-id=<run ID>`, and fail the Bazel Migration and CL Creation step with the reason: "Planter failed to queue the migration groups." This is a **STOP** condition.

3. Run `<run> upload --run-id=<run ID>` to start one Planter runner in each workbox, and wait until every queued task is uploaded or halted.
   * It can take hours. Do not treat a slow run as a failure.
   * It prints one line for each queued task, in the form `RESULT <task-id> <result> <CL number|-> <workbox|-> <reason>`.
   * If **assign reviewer** is disabled (option `A`), `upload` tears down the workboxes before it exits. If it is enabled (option `C`), it keeps the workboxes of the uploaded tasks for step 6.
   * If it exits with `12`, no Planter runner could be started, and `upload` has already torn down the workboxes. Delete every task queued in this run with `<planter> delete-task <task-id>` (none of them has a CL), and fail the Bazel Migration and CL Creation step with the `STOP_REASON=` reason. This is a **STOP** condition with the **host-mode rerun hint**.

4. Handle the `RESULT` line of each **migration group** as described in [Workbox Results](error_handling.md#workbox-results):
   * If the result is `UPLOADED`, `UPLOADED_WIP`, or `LANDED` and the line has the CL number, the CL of the **migration group** is created.
     * If the result is `UPLOADED` but the line has no CL number, fail the Bazel Migration and CL Creation step with the reason: "Fail to get the generated CL for `<package_names>` migration." This is a **STOP** condition.
   * Otherwise, the **migration group** fails with the reason: "Planter failed to migrate `<package_names>`: `<reason>`", where `<reason>` is the result and the reason of the `RESULT` line. The script has already captured the logs of the task.
   * If no CL is created for any **migration group**, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate all the migration groups." The failed **migration groups** have already been handled, so this is a **STOP** condition, not a **PLANTER ERROR** condition. If there is one **migration group**, its failure is a **PLANTER ERROR** condition instead.

5. For each **migration group** whose CL is created, use "gsheets" skill: Update the "Migration Status" of each package of the **migration group** in the **control table** to "CL_created", write the **CL URL** to its "Fuchsia Gerrit CL" column, and set "Last Status Update" to the **status timestamp**.
   * If it fails to update the **control table**, ignore the failure.

6. If **assign reviewer** is enabled (option `C`), refer to [Assign Reviewers in Workboxes](#assign-reviewers-in-workboxes). If it is disabled, the CLs have no reviewer: record "not assigned, assign reviewer disabled" for the "Completed" notification.

### Assign Reviewers in Workboxes

Planter assigns a human reviewer only after a CQ dry run of the CL passes. So in option `C`, Planter runs a second phase in the workboxes kept by `upload`: it votes Commit-Queue+1, fixes CQ failures and uploads new patchsets (at most 3 rounds), and assigns a reviewer once a CQ dry run passes.

1. Run `<run> touch --run-id=<run ID>`, and then `<run> review --run-id=<run ID>`.
   * A CQ dry run can take hours. Do not treat a slow run as a failure.
   * It runs only the `UPLOADED` tasks. It prints one line for each uploaded task, in the form `REVIEW <task-id> <review result> <CL number> <detail>`, and tears down the workboxes before it exits.
2. For each `REVIEW` line, record the reviewer text for the "Completed" notification as described in the [Notification Templates](../assets/notification_templates.md). A `REVIEW` line never fails the **migration group**: the CL is created, so do not abandon it, delete its task, or change its row from "CL_created".
   * If the result is `REVIEWER_ASSIGNED` and the detail has no reviewer, use "fuchsia-gerrit-cli" skill to get the reviewers of the CL.
   * If the result is `REVIEWER_ASSIGNED`, refer to [Reviewer Availability](reviewer_availability.md) to check the reviewer, swap them if they are out of office, and record the swap for the "Completed" notification.
   * If the result is `REVIEW_STRANDED`, the task is paused and keeps its CL. The user can continue it with `planter resume <task-id>`.
