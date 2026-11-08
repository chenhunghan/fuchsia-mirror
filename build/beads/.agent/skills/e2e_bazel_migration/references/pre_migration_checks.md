# Pre-Migration Checks

Run the checks to verify if the required criteria for the Bazel migration are fulfilled.

Run them in this order. The checks marked with an **execution mode** run only in that mode:
1. [Required Skills Check](#required-skills-check).
2. [Helper Document](#helper-document).
3. [Authentication Check](#authentication-check).
4. [Execution Mode](#execution-mode), which decides the **execution mode**.
5. *Workbox mode:* [Workbox Commands](#workbox-commands), [Helper Command Check](#helper-command-check), and then [Orphan Sweep](#orphan-sweep).
6. [Planter Installation Check](#planter-installation-check), steps 1 to 4.
7. *Workbox mode:* [Workbox Readiness and Setup](#workbox-readiness-and-setup).
8. [Check the Planter Freshness](#5-check-the-planter-freshness).
9. *Host and in-container modes:* [Check the Antigravity Environment](#6-check-the-antigravity-environment).
10. [Check the Reviewer Assignment Support](#7-check-the-reviewer-assignment-support), only if **assign reviewer** is enabled.
11. [Check the Speed Force Support](#8-check-the-speed-force-support).

In workbox mode, a failed Workbox step does not stop the skill: it falls back to host mode, as described in [Workbox Fallback](#workbox-fallback).

## Required Skills Check

The Bazel migration uses the skills below.

| Skill | Used for | Location |
| :---- | :------- | :------- |
| `gmail` | Sending notifications | Use the `skill_search` tool to find it |
| `gsheets` | Reading and updating the **control table** | Use the `skill_search` tool to find it |
| `fuchsia-gerrit-cli` | Getting CL numbers and reviewers, finding existing migration CLs, and posting reviewer swap comments | `//.agents/skills/fuchsia-gerrit-cli` |
| `go-links` | Resolving go links | Use the `skill_search` tool to find it |
| `gcalendar` | Checking whether the reviewers of a CL are out of office | Use the `skill_search` tool to find it |
| `fx-worktree` | Allocating worktree slots for the Planter fleet (host mode only) | `//.agents/skills/fx-worktree` |

`//` is the root of the Fuchsia checkout.

A skill marked "host mode only" is required only in host mode. Decide the **execution mode** first as described in [Execution Mode](#execution-mode) (it reads only the prompt and the environment), and skip that skill in the other modes.

### 1. Check the Installed Skills

1. For each required skill above, check whether it is listed in your available skills.
   * If all the skills are listed, the check passes.
   * If any skill is not listed, refer to [Install the Missing Skills](#2-install-the-missing-skills) to install it.

### 2. Install the Missing Skills

1. For each missing skill, get the directory that contains its `SKILL.md` from the "Location" column above.
   * If the location is a directory in the Fuchsia checkout (e.g. `//.agents/skills/fx-worktree`), use that directory. Check that its `SKILL.md` exists and has the same skill name.
   * Otherwise, use the `skill_search` tool to search for it by name. The search result must have the same skill name (e.g. `go-links`).
   * If the skill is not found, fail the Pre-Migration Checks step with the reason: "Failed to find the `<skill>` skill".

2. Register the skill in `~/.gemini/config/skills.json` so that it is available in later sessions.
   * Add an entry whose `path` is the parent directory of the skill directory and whose `include_only` lists the skill directory name (e.g. `{"path": "<skills_parent_dir>", "include_only": ["<skill_dir_name>"]}`). The skill directory name can differ from the skill name (e.g. directory `foo_bar` for skill `foo-bar`). If an entry with the same `path` already exists, add the skill directory name to its `include_only` instead.
   * Create the file with `{"entries": []}` if it does not exist. Keep all existing entries.
   * If it fails to update the file, fail the Pre-Migration Checks step with the reason: "Failed to install the `<skill>` skill".

3. Newly registered skills may not be listed until a new session starts. For the current run, read the `SKILL.md` of each installed skill from the directory found in step 1, and follow it when this skill refers to that skill.


## Helper Document

The **auth check command**, the **Planter check script** and the **workbox helper command** are in the **helper document**, next to the **Planter installation guide**. Find and read it once, before the [Authentication Check](#authentication-check). The later steps take their commands from it.

1. Get the **Planter installation guide**.
   * If the user specified a Planter installation guide in the prompt, use it. Do not resolve the link.
   * Otherwise, use the `go-links` skill to resolve the **Planter installation guide** link to its destination.
   * Keep the guide's location: the destination of the link, or the guide from the prompt. [Install Planter](#2-install-planter) and [Find the Workbox Setup Script](#3-find-the-workbox-setup-script) use it, so the link is resolved only once.

2. Find the **helper document**.
   * If the user specified a helper document in the prompt, use it, and skip the rest of this step.
   * The guide must be a file in the source tree (a code browser or documentation URL of a file), or a local file path. The directory that holds it is the Planter source directory.
   * Map that directory to the same directory in the read-only head source tree. A local file path is used as is.
   * The **helper document** is `e2e_bazel_migration_planter_workbox_helper.md` in that directory.

3. Read the **helper document**, and keep it for the later steps.
   * If the link cannot be resolved, the guide is not a file in the source tree, or the **helper document** cannot be read, do not fail here: the commands given in the prompt still apply. Each step that needs a missing command fails or skips as it describes: [Authentication Check](#authentication-check), [Helper Command Check](#helper-command-check), and [Check the Planter Version](#3-check-the-planter-version).


## Authentication Check

1. Get the **auth check command**: the command in the fenced block of the "Auth Check" section of the **helper document**.
   * If the user specified the auth check command in the prompt, skip this step and use that command.
   * If the **helper document** cannot be read, or its "Auth Check" section has no command, fail the Pre-Migration Checks step with the reason: "Failed to get the auth check command".

2. Run the **auth check command**.
   * If it exits non-zero, fail the Pre-Migration Checks step with the reason: "Credentials expired or will expire in an hour".


## Execution Mode

Decide the **execution mode** of this run. Take the first condition that holds:

1. Run `[[ -n "${WORKBOX:-}" ]]`. If it succeeds, the skill runs inside a workbox: the **execution mode** is in-container mode.
2. If the user asks in the prompt to run without Workbox (e.g., "run without workbox", "without workbox", "use host mode"), or `[[ "${E2E_BAZEL_MIGRATION_HOST_MODE:-}" == 1 ]]` succeeds, the **execution mode** is host mode.
3. Otherwise, the **execution mode** is workbox mode.

Tell the user the **execution mode**, and keep it for the notifications, which show it as `Mode: workbox`, `Mode: host` or `Mode: in-container`. It changes only if a Workbox step of these checks fails: then the skill warns the user and falls back to host mode, as described in [Workbox Fallback](#workbox-fallback). Do not change it in any other case.

The **execution mode** decides which checks run:

| Check | Workbox mode | Host mode | In-container mode |
| :---- | :----------- | :-------- | :---------------- |
| The `fx-worktree` skill in the [Required Skills Check](#required-skills-check) | No | Yes | No |
| [Workbox Commands](#workbox-commands), [Helper Command Check](#helper-command-check), [Orphan Sweep](#orphan-sweep), [Workbox Readiness and Setup](#workbox-readiness-and-setup) | Yes | No | No |
| [Planter Installation Check](#planter-installation-check), steps 1 to 5 | Yes | Yes | Yes |
| [Check the Antigravity Environment](#6-check-the-antigravity-environment) | No | Yes | Yes |
| [Check the Reviewer Assignment Support](#7-check-the-reviewer-assignment-support) (if **assign reviewer** is enabled) | Yes | Yes | Yes |

In host and in-container modes, do not use the **workbox helper command** or look for the **workbox setup script**, so that they cannot block these modes.


## Workbox Fallback

In workbox mode, a failed Workbox step of these checks ([Workbox Commands](#workbox-commands), [Helper Command Check](#helper-command-check), [Orphan Sweep](#orphan-sweep) or [Workbox Readiness and Setup](#workbox-readiness-and-setup)) does not stop the skill. When a step says to fall back to host mode with a reason:

1. Show the **Workbox fallback warning** to the user, with that reason. Keep the warning for the notifications: their mode is `host`, and their Workbox line says `Not used: fell back to host mode: <reason>`.
2. Change the **execution mode** to host mode for the rest of the run. Skip the remaining Workbox steps, and do not run any other Workbox command.
3. Check the `fx-worktree` skill, which the [Required Skills Check](#required-skills-check) skipped in workbox mode, as described there. If it cannot be found or installed, fail the Pre-Migration Checks step with the reason: "Failed to find the `fx-worktree` skill". This is a **STOP** condition.
4. Continue with the next check in the order at the top of this document, in host mode. The checks of host mode in the table of [Execution Mode](#execution-mode) apply from now on, including [Check the Antigravity Environment](#6-check-the-antigravity-environment).

The fallback happens at most once, and before any **migration package** is selected, so the **control table** is not changed. A Workbox failure after the Pre-Migration Checks does not fall back: it is a **STOP** condition with the **host-mode rerun hint**.


## Workbox Commands

Run this section only in workbox mode, before the first Workbox command. The Workbox commands are `planter workbox ...`, `build/beads/.agent/skills/e2e_bazel_migration/scripts/workbox_preflight.sh` and `build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_workbox_run.sh`. Below, `workbox_preflight.sh` and `planter_workbox_run.sh` are these scripts, and `<planter>` is the `planter` binary described in the [Planter Installation Check](#planter-installation-check).

Run every Workbox command as below, in this step and in the later steps:
* Run it from the root of the Fuchsia checkout, **outside the terminal sandbox**: workboxes need privileges that the sandbox does not give. Start it with `export PATH="$HOME/.local/bin:$PATH"`.
* Set `E2E_WB_HELPER` to the **workbox helper command** for each call of `workbox_preflight.sh` and `planter_workbox_run.sh` (e.g., `E2E_WB_HELPER="<workbox helper command>" build/beads/.agent/skills/e2e_bazel_migration/scripts/workbox_preflight.sh check`). The scripts run the helper from it. The [Helper Command Check](#helper-command-check) step gets the command.
* `workbox_preflight.sh setup`, and the `prepare`, `upload`, `review`, `teardown` and `sweep` subcommands of `planter_workbox_run.sh`, can run for a long time. Run each of them **outside the terminal sandbox**, **in the background**. End your turn and wait for the completion notification. If you are told that it is still running, check its status once and wait again. Never kill it: a killed command leaves workboxes and Planter tasks behind, which the next run has to clean up. Do not treat a slow run as a failure.

1. Check whether you can run commands **outside the terminal sandbox** in this environment.
   * If you cannot, [fall back to host mode](#workbox-fallback) with the reason: "Workbox commands must run outside the terminal sandbox, and this environment does not allow it". Host mode does not need to leave the sandbox.


## Helper Command Check

Run this check only in workbox mode. The **workbox helper command** checks the Workbox setup of this machine for `workbox_preflight.sh` and `planter_workbox_run.sh`.

1. Get the **workbox helper command** from the "Command" section of the **helper document**: the single line in the first fenced block under "Command".
   * If the user specified the workbox helper command in the prompt, skip this step and use that command.
   * If the **helper document** cannot be read, or it has no "Command" section, or the first fenced block under it is not exactly one line, [fall back to host mode](#workbox-fallback) with the reason: "Failed to get the workbox helper command".

2. Run `workbox_preflight.sh version` with `E2E_WB_HELPER` set to the **workbox helper command**, as described in [Workbox Commands](#workbox-commands).
   * If it exits with `0`, keep the `HELPER_CL=` value of its output for the Workbox line of the notifications, and continue.
   * If its output has `HELPER_STATUS=INCOMPATIBLE`, [fall back to host mode](#workbox-fallback) with the reason: "The workbox helper is incompatible (api `<n>`)", where `<n>` is the `HELPER_API=` value of its output.
   * Otherwise, [fall back to host mode](#workbox-fallback) with the reason: "The workbox helper failed to start".


## Orphan Sweep

Run this step only in workbox mode. An earlier run of this skill that was interrupted (e.g., killed, or its machine rebooted) can leave its workboxes and Planter tasks behind. The sweep cleans up after the earlier runs that are no longer alive: it stops and destroys their workboxes, pauses their Planter tasks that have no CL, and reports the tasks that have a CL. It never deletes a Planter task and never changes the **control table**. It leaves the runs that are still alive, such as a run of this skill in another conversation, untouched.

1. Run `planter_workbox_run.sh sweep` **outside the terminal sandbox**, **in the background**, as described in [Workbox Commands](#workbox-commands).
   * Relay the lines between `PLANTER_WARNINGS_BEGIN` and `PLANTER_WARNINGS_END` of its output (the `SWEEP` report lines and any workbox that could not be destroyed) as **Planter warnings** (see the [Planter Rules](../SKILL.md#planter-rules)). The report prints, for the user to decide, the commands to delete or restart the paused tasks; do not run them yourself.
   * If it exits with `12` with `STOP_REASON=Another skill run holds the Workbox lock`, another run of this skill holds the Workbox lock for too long. [Fall back to host mode](#workbox-fallback) with the reason: "Another skill run holds the Workbox lock".
   * If it fails for another reason, tell the user, with its output, and continue. `planter_workbox_run.sh prepare` sweeps again before it creates the workboxes.


## Planter Installation Check

The `<planter>` below is the `planter` binary resolved with `"$(command -v planter || echo "$HOME/.local/bin/planter")"`, the same way `scripts/planter_task_add_run.sh` finds it. Use it instead of a bare `planter`, because `~/.local/bin` may not be on `PATH`.


### 1. Check the Planter Binary

1. Run `command -v planter || ls -l "$HOME/.local/bin/planter"` to find the `planter` binary.
   * If the binary is found, run `<planter> version` to verify that it starts. If it starts, refer to [Check the Planter Version](#3-check-the-planter-version) to update it if there is a newer version.
   * If the binary is not found, or `<planter> version` fails, refer to [Install Planter](#2-install-planter) to install it.


### 2. Install Planter

1. Read the **Planter installation guide**, at the location that the [Helper Document](#helper-document) step kept (the destination of the link, or the guide from the prompt), and follow its steps to install the `planter` prebuilt.
   * If the link could not be resolved or the guide cannot be read, fail the Pre-Migration Checks step with the reason: "Failed to read the Planter installation guide".
   * The build can take 10 minutes or more. Allow a long timeout or run the command in the background and wait for it to finish; do not treat a slow build as a failure.

2. Run `<planter> version` to verify the installation.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Failed to install Planter".
   * If `command -v planter` still finds nothing, `~/.local/bin` is not on `PATH`. This is fine for the migration: `scripts/planter_task_add_run.sh` falls back to `~/.local/bin/planter`.


### 3. Check the Planter Version

Planter changes often. Use the **Planter check script** to check whether the installed Planter is up to date, and update it if it is not.

1. Get the **Planter check script** from the "Script" section of the **helper document**.
   * If the user specified a different Planter check script in the prompt, use that script instead.
   * If the **helper document** cannot be read, or it has no script, tell the user, skip this check, and continue.
2. Save the script to a file outside the Fuchsia checkout (e.g., `"$(mktemp)"`), and run it with `bash`. It updates Planter if it is out of date; the update can take 10 minutes or more, so allow a long timeout or run it in the background and wait for it to finish.
   * If it exits with `0`, Planter is up to date or was updated. Continue.
   * Otherwise, if `<planter> version` still works, keep using the installed Planter: tell the user that Planter could not be updated, with the output of the script, and continue. Do not fail the Pre-Migration Checks step. [Check the Planter Freshness](#5-check-the-planter-freshness) stops the migration if this Planter is too far out of date.
   * Otherwise, fail the Pre-Migration Checks step with the reason: "Failed to update Planter".


### 4. Check the Shared Machinery

1. Run `ls "$HOME/.planter/beads/build/beads/planter/machinery/prompts/coder.md"` to check whether the Planter shared machinery is set up.
   * If the file does not exist, run `<planter> init-project` from the root of the Fuchsia checkout to set it up.
   * If `<planter> init-project` fails, fail the Pre-Migration Checks step with the reason: "Failed to initialize the Planter shared machinery".

In workbox mode, run [Workbox Readiness and Setup](#workbox-readiness-and-setup) now, and then continue with [Check the Planter Freshness](#5-check-the-planter-freshness). Planter is installed and updated first, so that the Workbox setup finds an up-to-date Planter and does not build it again.


### 5. Check the Planter Freshness

New migrations must not be started when the Planter binary (with a newer binary available) or its shared machinery (with local changes blocking automatic sync) is out of date by more than the **Planter freshness limit** (see the [Planter Rules](../SKILL.md#planter-rules)): it would migrate without the latest checks and lessons. (A clean shared machinery checkout auto-syncs when Planter runs, and [Workspace Branching](workspace_branching.md) updates the Fuchsia checkout in the next step.)

1. From the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_freshness.sh`. If the user specified a different **Planter freshness limit**, also set `PLANTER_MAX_STALE_HOURS` to it in hours.
   * The script prints how out of date the Planter binary and its shared machinery are, followed by the **Planter warnings**. Relay the **Planter warnings** to the user verbatim, and keep them for the "Planter Warnings" section of the notifications.
   * If it exits with `0`, Planter is within the **Planter freshness limit**; continue.
   * If it exits with `2`, fail the Pre-Migration Checks step with the reason: "Planter is out of date by `<age>`, more than the `<limit>` limit for new migrations", where `<age>` and `<limit>` are from the `STALE:` line of its output. Include the **Planter warnings**, which say how to update Planter, in the message to the user. Do not try to get around the check (see the [Planter Rules](../SKILL.md#planter-rules)).
   * Otherwise, fail the Pre-Migration Checks step with the reason: "Failed to check whether Planter is up to date", with the output of the script.


### 6. Check the Antigravity Environment

Run this check only in host and in-container modes. In workbox mode, Planter runs inside the workboxes, and each workbox runs its own agent hub, so skip this check.

Planter drives its coding and reviewer agents through the Antigravity language server. It reads the server address from `ANTIGRAVITY_LS_ADDRESS` (default `localhost:5387`) and the CSRF token from `ANTIGRAVITY_CSRF_TOKEN`, which are set in Antigravity terminals.

1. Run `[[ -n "${ANTIGRAVITY_CSRF_TOKEN:-}" ]]` to check that the command runs in an Antigravity terminal.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Planter must run from an Antigravity terminal".


### 7. Check the Reviewer Assignment Support

Run this check only if **assign reviewer** is enabled.

1. Run `<planter> run --help 2>&1 | grep -q -- -assign-reviewer` to check that Planter supports `--assign-reviewer`.
   * If it fails, Planter is older than the `--assign-reviewer` support. Refer to [Install Planter](#2-install-planter) to rebuild Planter from head, and check again.
   * If it still fails, fail the Pre-Migration Checks step with the reason: "Planter does not support --assign-reviewer".

2. Planter reads the reviewer routing table (go/fuchsia-bazel-migration-reviewers) with the gcloud Application Default Credentials (ADC), which need the Sheets scope. Run `curl -s "https://oauth2.googleapis.com/tokeninfo?access_token=$(gcloud auth application-default print-access-token)"` and check that its `scope` contains `https://www.googleapis.com/auth/spreadsheets`.
   * If the command fails or the scope is missing, fail the Pre-Migration Checks step with the reason: "gcloud ADC lacks the Sheets scope for --assign-reviewer; run `gcloud auth application-default login --scopes=openid,https://www.googleapis.com/auth/userinfo.email,https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/spreadsheets`". Do not run the login command yourself, because it needs the user to sign in in a browser.
     * Keep `openid` in `--scopes`. Google also grants `openid` with these scopes, and without it the login fails with "Scope has changed" after the sign-in.


### 8. Check the Speed Force Support

The Planter commands of this skill always pass `--speed-force`, or `--speed-force=false` if **Planter speed force** is disabled, so run this check in every case.

1. Run `<planter> run --help 2>&1 | grep -q -- -speed-force` to check that Planter supports `--speed-force`.
   * If it fails, Planter is older than the `--speed-force` support. Refer to [Install Planter](#2-install-planter) to rebuild Planter from head, and check again.
   * If it still fails, fail the Pre-Migration Checks step with the reason: "Planter does not support --speed-force".


## Workbox Readiness and Setup

Run this check only in workbox mode, after step 4 of the [Planter Installation Check](#planter-installation-check). Run every command as described in [Workbox Commands](#workbox-commands).

It checks whether Workbox is set up on this machine. If it is, nothing is changed. If it is not, it checks the credentials that the setup needs, finds the **workbox setup script**, and runs it only after the user confirms.

Keep these values for the Workbox line of the notifications: the `HELPER_CL=` value from the [Helper Command Check](#helper-command-check) step, and whether the setup ran. If it ran, also keep the `SETUP_SHA256=`, `SETUP_SNAPSHOT_CL=` and `SETUP_LAST_CHANGE_CL=` values of `workbox_preflight.sh setup`, and where the script was found (`guide`, `prompt-guide` or `prompt-path`, see [Find the Workbox Setup Script](#3-find-the-workbox-setup-script)). If it did not run, the Workbox line says "setup: not run".


### 1. Check the Workbox Readiness

1. Run `workbox_preflight.sh check`. If the user specified a tree backend for the workboxes in the prompt, pass it verbatim as `--tree=<value>`.
   * It prints one `CHECK <id> PASS|FAIL|SKIP <detail>` line for each check, followed by `STATUS=`, `TREE=`, `FAILED=` and one `BLOCKED <reason>` line for each blocking problem. Every failed check is reported at once.
   * `TREE` below is the `TREE=` value of its output. Use it as is; it names the tree backend of the workboxes, and the Workbox commands below take it.
   * If it exits with `0` (`STATUS=READY`), Workbox is set up. Do not look for the **workbox setup script**. Continue to [Stage the Gerrit Credentials](#5-stage-the-gerrit-credentials).
   * If it exits with `3` (`STATUS=NEEDS_SETUP`), Workbox is not set up yet. Keep the `FAILED=` value, and continue to [Check the Setup Credentials](#2-check-the-setup-credentials).
   * If it exits with `4` (`STATUS=BLOCKED`), Workbox cannot be set up on this machine by this skill. [Fall back to host mode](#workbox-fallback) with the reason: "Workbox is blocked: `<reasons>`", where `<reasons>` are the `BLOCKED` lines of its output. If its output has `SANDBOX=BLOCKED`, use the reason of the sandbox check in [Workbox Commands](#workbox-commands) instead.
   * If it exits with `5` (`STATUS=IN_WORKBOX`), the skill runs inside a workbox after all. Use in-container mode instead, as described in [Execution Mode](#execution-mode), and skip the rest of this section.
   * Otherwise, [fall back to host mode](#workbox-fallback) with the reason: "Failed to check the Workbox readiness", with its output.


### 2. Check the Setup Credentials

The setup needs credentials that only the user can create, so they are checked before the user is asked to confirm the setup.

1. Run `workbox_preflight.sh creds --tree="$TREE" --setup`. If **assign reviewer** is enabled, also pass `--need-sheets`.
   * If it exits with `0`, continue to the next step.
   * If it exits with `6`, its output has a `CRED_FAIL <id> <message>` line and a `USER_COMMAND <id> <command>` line for each missing credential. Show each command to the user verbatim, with: "Run this in your own terminal, so that the next run can use Workbox." Then [fall back to host mode](#workbox-fallback) with the reason: "Missing credentials for the Workbox setup: `<messages>`", where `<messages>` are the messages of the `CRED_FAIL` lines.
     * Never run these commands yourself, and never relay a sign-in code through the chat: they need the user to sign in.
   * Otherwise, [fall back to host mode](#workbox-fallback) with the reason: "Failed to check the credentials for the Workbox setup", with its output.


### 3. Find the Workbox Setup Script

Find the **workbox setup script**, and keep its path as `SETUP`, where it was found as `<source>`, and the location of the guide as `<guide location>`. Run these steps in order; each failure below falls back to host mode, as described in [Workbox Fallback](#workbox-fallback).

1. If the user specified a setup script path in the prompt, use that path as is as `SETUP`, with `<source>` `prompt-path`. Skip the rest of these steps: the guide is not used, and there is no cross-check.

2. Get the **Planter installation guide**: the location that the [Helper Document](#helper-document) step kept.
   * `<source>` is `prompt-guide` if the user specified the guide in the prompt, and `guide` otherwise.
   * `<guide location>` is the guide's location: the destination of the link, or the guide from the prompt.
   * If the link could not be resolved or the guide cannot be read, [fall back to host mode](#workbox-fallback) with the reason: "Failed to find the workbox setup script: the Planter installation guide could not be resolved or read".

3. Find the directory that holds the **Planter installation guide**, in the read-only head source tree, as in step 2 of the [Helper Document](#helper-document) step.
   * `SETUP` is `workbox-setup.sh` in that directory.
   * If the guide is not a file in the source tree (e.g., it is a document that is not a file), [fall back to host mode](#workbox-fallback) with the reason: "Failed to find the workbox setup script: the Planter installation guide is not a file in the source tree. Name the setup script in the prompt."

4. Cross-check it with the fleet guide that the installation guide links to. A relative link refers to a file in the same directory as the installation guide.
   * If the fleet guide has a fenced command that runs a file named `workbox-setup.sh` (its one-command bootstrap), the path of that file must be `SETUP`. Arguments after the path do not matter. If they are the same, the cross-check result is `match`.
   * If the fleet guide does not exist, or it has no such command, the cross-check result is `absent`. Continue.
   * If the paths differ, [fall back to host mode](#workbox-fallback) with the reason: "The Planter installation guide and the fleet guide name different setup scripts: `<SETUP>` and `<path in the fleet guide>`. Name the setup script in the prompt."

5. Keep `SETUP` from step 3 as the path of the **workbox setup script**.

[Ask for Consent and Set Up Workbox](#4-ask-for-consent-and-set-up-workbox) checks that the script exists and can be read.


### 4. Ask for Consent and Set Up Workbox

The setup changes this machine (e.g., it needs sudo, and changes the global agent configuration), so it runs only after the user confirms.

1. Run `workbox_preflight.sh consent-text --setup="$SETUP" --source=<source> --guide=<guide location> --fleet-check=<match|absent> --tree="$TREE" --failed=<FAILED>`, where `<FAILED>` is the `FAILED=` value of [Check the Workbox Readiness](#1-check-the-workbox-readiness) and `<match|absent>` is the result of the cross-check. If `<source>` is `prompt-path`, leave out `--guide` and `--fleet-check`.
   * If it exits with `11`, [fall back to host mode](#workbox-fallback) with the reason: "The workbox setup script `<SETUP>` does not exist or cannot be read".
   * If it exits with `0`, show the text between the `--- BEGIN CONSENT TEXT ---` and `--- END CONSENT TEXT ---` lines of its output to the user. Show the text verbatim. Keep the `MANUAL_COMMAND=` value: the command for the user to run the setup in their own terminal.
   * Otherwise, [fall back to host mode](#workbox-fallback) with the reason: "Failed to prepare the Workbox setup", with its output.

2. Get the user's consent.
   * If the user asked in the prompt not to set up Workbox (e.g., "don't set up workbox"), [fall back to host mode](#workbox-fallback) with the reason: "Workbox is not set up", and tell the user the manual command.
   * Otherwise, ask the user to confirm, and wait for an explicit **Proceed**. Anything else is a refusal.
   * If the user refuses, [fall back to host mode](#workbox-fallback) with the reason: "Workbox setup was declined", and tell the user the manual command.

3. On **Proceed**, run `workbox_preflight.sh setup --consented --tree="$TREE"` **outside the terminal sandbox**, **in the background**, as described in [Workbox Commands](#workbox-commands). Pass `--consented` only after the user's explicit **Proceed** in this run.
   * The setup can take 45 minutes, or hours when it has to set up a new tree backend from scratch, as the consent text says. Do not treat a slow setup as a failure.
   * It runs the exact script the user confirmed: it refuses to run without the confirmation recorded by `consent-text`, or if the script changed since then. It does not start any workbox.

4. Check the result. Relay the `WARN` lines of its output as **Planter warnings** in every case (see the [Planter Rules](../SKILL.md#planter-rules)).
   * If it exits with `0` (`SETUP_STATUS=OK`), Workbox is set up. Keep the values for the Workbox line of the notifications, and continue to the next step.
   * If its output has `SETUP_STATUS=CHANGED`, [fall back to host mode](#workbox-fallback) with the reason: "The workbox setup script changed after consent".
   * If its output has `SETUP_STATUS=BACKUP_FAILED`, [fall back to host mode](#workbox-fallback) with the reason: "Failed to back up the agent configuration before setup".
   * If its output has `SETUP_STATUS=FAILED` and `SETUP_EXIT=` is not `0`, [fall back to host mode](#workbox-fallback) with the reason: "Failed to set up Workbox: `<DIE_LINE>`; see `<LOG>`", where `<DIE_LINE>` and `<LOG>` are the `DIE_LINE=` and `LOG=` values of its output.
   * If its output has `SETUP_STATUS=FAILED` and `SETUP_EXIT=0`, Workbox is still not ready after the setup. [Fall back to host mode](#workbox-fallback) with the reason: "Failed to set up Workbox: `<POST_CHECK_FAILED>`; see `<LOG>`", where `<POST_CHECK_FAILED>` is the `POST_CHECK_FAILED=` value (the checks that still fail).
   * Otherwise (e.g., `SETUP_STATUS=CONSENT_REQUIRED`), [fall back to host mode](#workbox-fallback) with the reason in the `STOP_REASON=` line of its output.


### 5. Stage the Gerrit Credentials

Run this step whether or not the setup ran.

1. Run `<planter> workbox creds` to stage the Gerrit credentials for the workboxes.
   * If it fails, [fall back to host mode](#workbox-fallback) with the reason: "Failed to stage Gerrit credentials for Workbox".
   * Relay the **Planter warnings** it prints.

Then continue with [Check the Planter Freshness](#5-check-the-planter-freshness).
