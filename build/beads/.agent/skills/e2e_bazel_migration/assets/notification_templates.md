# Notification Templates

In the templates below:

* `<migrating_target_name>`: The names of the **migration packages**,
  separated by `, `.
* `<paths_of_the_migrating_targets>`: The paths of the **migration packages**,
  separated by `, `.
* `<execution_mode>`: The **execution mode** of this migration: `workbox`,
  `host`, or `in-container`.
* `<workbox_summary>`: In workbox mode, one line with:
  * `helper CL <HELPER_CL>`, the CL of the **workbox helper command** printed
    by `scripts/workbox_preflight.sh version`;
  * the setup: `setup: not run` if Workbox was already set up, or
    `setup: workbox-setup.sh sha256 <first 12 characters of SETUP_SHA256>, head
    snapshot CL <SETUP_SNAPSHOT_CL>, last changed in CL <SETUP_LAST_CHANGE_CL>,
    from <source>` if the **workbox setup script** ran in this migration, where
    `<source>` is `guide`, `prompt guide`, or `prompt path`, and `-` stands for
    an unknown CL;
  * the workboxes created and destroyed by this migration, and the leaked
    workboxes (`BOXES_LEAKED=`) with the command to destroy each of them.

  In host mode and in-container mode, `Not used`. After a Workbox fallback (see
  the **Workbox fallback warning**), `Not used: fell back to host mode:
  <reason>`, with the reason of the warning.
* `<Fuchsia_Gerrit_CLs>`: One line for each **migration group** whose CL is
  created, in the form `<CL number>: <paths of its packages>`. If there is one
  **migration group**, only its CL number. If **assign reviewer** is enabled,
  append ` (reviewer: <assigned reviewer>)`, or ` (reviewer: not assigned,
  <reason>)`. If [Reviewer Availability](../references/reviewer_availability.md)
  swapped the reviewer, append ` (reviewer swapped: <OOO reviewer> is OOO until
  <date>, reassigned to <replacement>)`, or if no replacement was available,
  ` (reviewer <OOO reviewer> is OOO until <date>, no available replacement)`.

  In workbox mode:
  * If **assign reviewer** is disabled, append ` (reviewer: not assigned,
    assign reviewer disabled)`. The user can assign one later with
    `planter assign-reviewer <CL number>`.
  * If **assign reviewer** is enabled, the `REVIEW` line of the CL's task gives
    the reviewer text:

    | `REVIEW` result | Reviewer text |
    | :-------------- | :------------ |
    | `REVIEWER_ASSIGNED` | `reviewer: <assigned reviewer>` |
    | `REVIEW_CQ_FAILED` | `reviewer: not assigned, CQ failed after the fix rounds: <detail>` |
    | `REVIEW_CQ_PASSED_NOT_ASSIGNED` | `reviewer: not assigned, CQ passed but no reviewer was assigned before the deadline` |
    | `REVIEW_TIMEOUT` | `reviewer: not assigned, CQ did not finish before the deadline` |
    | `REVIEW_STRANDED` | `reviewer: not assigned (runner crashed; task paused: planter resume <task-id> to continue)` |
    | `REVIEW_SKIPPED_WIP` | `reviewer: not assigned, awaiting planter ack` |

  * If the result of the task is `UPLOADED_WIP`, append ` (WIP: run planter ack
    <task-id>; the ready upload needs a runner: planter workbox start or
    planter fleet --gerrit)`.
* `<reviewer_swaps>`: One line for each reviewer swapped by
  [Reviewer Availability](../references/reviewer_availability.md), in the form
  `<CL number>: <OOO reviewer> (OOO until <date>) -> <replacement>`.
* `<failed_migration_groups>`: One line for each failed **migration group**, in
  the form `<paths of its packages>: <reason>`. If no **migration group**
  failed, `None`.
* `<already_claimed_migration_groups>`: One line for each **migration group**
  dropped because one of its packages is already claimed by another Planter
  task, in the form `<paths of its packages>: claimed by Planter task
  <task-id> (another run?)`. If no **migration group** was dropped, `None`.
* `<planter_warnings>`: The distinct **Planter warnings** Planter printed during
  this migration, one per line, verbatim, each followed by the command Planter
  suggests (if any). In workbox mode, they include the warnings in the
  workbox logs (`planter workbox logs`), the `WARN` lines of the Workbox
  setup, and the report of the orphan sweep. If Planter printed no warnings,
  `None`.

## Start Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> started`

**Notification Body:**
```
The migration for <migrating_target_name> started at <current_time>.
Mode: <execution_mode>
Migrated Targets: <paths_of_the_migrating_targets>
```

## Completed Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> has been completed`

**Notification Body:**
```
The migration for <migrating_target_name> was completed at <current_time>.
Mode: <execution_mode>
Workbox: <workbox_summary>
Migrated Targets: <paths_of_the_migrating_targets>
Fuchsia Gerrit CLs:
<Fuchsia_Gerrit_CLs>
Failed Migration Groups:
<failed_migration_groups>
Already Claimed Migration Groups:
<already_claimed_migration_groups>
Planter Warnings:
<planter_warnings>
```

## Reviewer Swapped Notification Template

Used only when [Reviewer Availability](../references/reviewer_availability.md)
runs alone, outside a migration run.

**Notification Subject:**
`Bazel Migration reviewers swapped for <CL numbers>`

**Notification Body:**
```
These migration CL reviewers were out of office, so they were swapped at
<current_time>:
<reviewer_swaps>
```

## Interrupted Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> was interrupted`

**Notification Body:**
```
The migration for <migrating_target_name> was interrupted at <current_time>
because of <reason>.
Mode: <execution_mode>
Workbox: <workbox_summary>
Migrated Targets: <paths_of_the_migrating_targets>
Control Table: <the_go_link_of_fuchsia-bazel-migration-control-table>
Already Claimed Migration Groups:
<already_claimed_migration_groups>
Planter Warnings:
<planter_warnings>
```
