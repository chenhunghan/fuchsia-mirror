# Pre-Migration Checks

Run the checks to verify if the required criteria for the Bazel migration are fulfilled.

## Authentication Check

1. Run `gcertstatus` to get the remaining time before expiration.
   * If `gcert` expires in less than one hour, fail the Pre-Migration Checks step with the reason: "gcert expired or will expire in an hour".


## Planter Installation Check

The `<planter>` below is the `planter` binary resolved with `"$(command -v planter || echo "$HOME/.local/bin/planter")"`, the same way `scripts/planter_task_add_run.sh` finds it. Use it instead of a bare `planter`, because `~/.local/bin` may not be on `PATH`.


### 1. Check the Planter Binary

1. Run `command -v planter || ls -l "$HOME/.local/bin/planter"` to find the `planter` binary.
   * If the binary is found, run `<planter> version` to verify that it starts.
   * If the binary is not found, or `<planter> version` fails, refer to [Install Planter](#2-install-planter) to install it, then repeat this check.


### 2. Install Planter

Planter is built from google3 (`//experimental/users/lindkvist/planter`) with `blaze`, so it must be built in a CitC client. Use the dedicated `planter-prebuilt` client for this, so the build never touches a client that holds the user's own pending changes.

1. Run `ls -d "/google/src/cloud/$USER/planter-prebuilt/google3"` to check whether the `planter-prebuilt` CitC client exists.
   * If it does not exist, run `g4 citc planter-prebuilt` to create it.
   * If it exists, run `cd "/google/src/cloud/$USER/planter-prebuilt/google3" && g4 sync` to pick up the latest Planter.
   * If `g4 citc` or `g4 sync` fails (e.g. sync conflicts), fail the Pre-Migration Checks step with the reason: "Failed to prepare the planter-prebuilt CitC client".

2. Run `cd "/google/src/cloud/$USER/planter-prebuilt/google3" && bash experimental/users/lindkvist/planter/scripts/publish_prebuilt.sh` to build Planter and install `planter` and `planter-rev` into `~/.local/bin`.
   * The `blaze build` can take 10 minutes or more. Allow a long timeout or run the command in the background and wait for it to finish; do not treat a slow build as a failure.
   * If the script fails, fail the Pre-Migration Checks step with the reason: "Failed to install Planter".

3. Run `"$HOME/.local/bin/planter" version` to verify the installation.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Failed to install Planter".
   * If `command -v planter` still finds nothing, `~/.local/bin` is not on `PATH`. This is fine for the migration: `scripts/planter_task_add_run.sh` falls back to `~/.local/bin/planter`.


### 3. Check the Shared Machinery

1. Run `ls "$HOME/.planter/beads/build/beads/planter/machinery/prompts/coder.md"` to check whether the Planter shared machinery is set up.
   * If the file does not exist, run `<planter> init-project` from the root of the Fuchsia checkout to set it up.
   * If `<planter> init-project` fails, fail the Pre-Migration Checks step with the reason: "Failed to initialize the Planter shared machinery".


### 4. Check the Jetski Environment

Planter drives its coding and reviewer agents through the Jetski (Antigravity) language server. It reads the server address from `ANTIGRAVITY_LS_ADDRESS` (default `localhost:5387`) and the CSRF token from `ANTIGRAVITY_CSRF_TOKEN`, which are set in Jetski terminals.

1. Run `[[ -n "${ANTIGRAVITY_CSRF_TOKEN:-}" ]]` to check that the command runs in a Jetski terminal.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Planter must run from a Jetski terminal".


## Required Skills Check

The Bazel migration uses the skills below.

| Skill | Used for | Location |
| :---- | :------- | :------- |
| `gmail` | Sending notifications | google3 `learning/gemini/agents/skills/gmail` |
| `gsheets` | Reading and updating the **control table** | google3 `learning/gemini/agents/skills/gsheets` |
| `fuchsia-gerrit-cli` | Getting the CL number and posting CL review results | Fuchsia checkout `.agents/skills/fuchsia-gerrit-cli` |
| `buganizer-cli` | Creating the Buganizer ticket | google3 `learning/gemini/agents/skills/buganizer_cli` |

### 1. Check the Installed Skills

1. For each skill above, check whether it is listed in your available skills.
   * If all the skills are listed, the check passes.
   * If any skill is not listed, refer to [Install the Missing Skills](#2-install-the-missing-skills) to install it.

### 2. Install the Missing Skills

1. For each missing skill, use the `skill_search` tool to search for it by name, and get the directory that contains its `SKILL.md`. The search result must have the same skill name (e.g. `buganizer-cli`).
   * If the skill is not found, fail the Pre-Migration Checks step with the reason: "Failed to find the `<skill>` skill".

2. Register the skill in `~/.gemini/config/skills.json` so that it is available in later sessions.
   * Add an entry whose `path` is the parent directory of the skill directory and whose `include_only` lists the skill directory name (e.g. `{"path": "/google/src/files/head/depot/google3/learning/gemini/agents/skills", "include_only": ["buganizer_cli"]}`). If an entry with the same `path` already exists, add the skill directory name to its `include_only` instead.
   * Create the file with `{"entries": []}` if it does not exist. Keep all existing entries.
   * If it fails to update the file, fail the Pre-Migration Checks step with the reason: "Failed to install the `<skill>` skill".

3. Newly registered skills may not be listed until a new session starts. For the current run, read the `SKILL.md` of each installed skill from the directory found in step 1, and follow it when this skill refers to that skill.
