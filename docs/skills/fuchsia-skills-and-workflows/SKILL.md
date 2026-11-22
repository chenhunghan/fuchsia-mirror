---
name: fuchsia-skills-and-workflows
description: >
  Authors, names, places, and reviews Fuchsia AI agent skills (SKILL.md),
  saved dynamic workflows (workflow.json and workflow.py), and discovery
  configs (skills.json and workflows.json). Use when creating, updating,
  auditing, or code-reviewing skills, workflows, or .agents configurations in
  fuchsia.git or vendor/google. Don't use for general C++, Rust, or Python
  code outside agent skills and workflows, or for running standalone
  evaluations.
---

# Fuchsia skills and workflows guidelines

Use these rules when authoring, updating, or code-reviewing AI agent skills
(`SKILL.md`), saved dynamic workflows (`.agents/workflows/<name>/`), agent
discovery configurations
(`.agents/{skills,workflows,plugins,agents,rules}.json`), and
`.agents/code-review/agent_configs.textproto` across `fuchsia.git` and
`vendor/google`. For `agent_configs.textproto` matching and skill-resolution
rules, see [review notes](//.agents/skills/review/SKILL.md#notes).

## Skill naming and folder placement

Check every new or modified skill against these naming and placement rules:

1.  **Case and characters:**
    - Use `kebab-case` with lowercase letters, numbers, and single hyphens
      (`[a-z0-9-]`). Never use `snake_case`, `camelCase`, spaces, or uppercase
      letters in the YAML `name`.
2.  **Length and specificity:**
    - Use 2 to 4 words (or up to 5 for multi-language suites) and keep `name` at
      or under 64 characters.
    - Avoid 1-word names (except `review`, which powers `/review`) and vague
      words (`helper`, `utils`, `tools`).
3.  **Word order and no filler suffixes:**
    - Use `<domain>-<action>` (`netstack3-unit-tests`, `device-bug-fix`),
      `<subsystem>-<action>-<language>` for multi-language suites
      (`driver-interrupt-impl-cpp`, `driver-fidl-server-impl-rust`),
      `<verb>-<noun>` (`edit-fuchsia-doc`, `verify-snapshot`), CLI prefixes
      (`fx-worktree`, `ffx-inspect`), migration pairs (`migrate-dfv1-to-dfv2`),
      or `fuchsia-<topic>` (`fuchsia-cpp-guidelines`).
    - Avoid verb-only names without a domain noun (for example, prefer
      `device-bug-fix` over `investigate-fix`).
    - Do not include filler words (`skill`, `guide`, `playbook`, `how-to`,
      `checklist`).
4.  **Folder match and plural `.agents`:**
    - Match the enclosing folder name to the YAML `name` in `kebab-case`, except
      for nested subsystem suites
      (`//src/<area>/skills/<topic>/<task>[/<lang>]/SKILL.md`, such as
      `//src/devices/skills/driver-interrupt/implementation/cpp/SKILL.md`),
      which group folders by topic, task, and language in `kebab-case`.
    - Always use the plural `.agents/` directory, never singular `.agent/`.
5.  **Stage 1 (subteam) vs. Stage 2 (global) placement:**
    - **Stage 2 (`//.agents/skills/<name>/SKILL.md`):** Injects YAML frontmatter
      into every Fuchsia developer's session by default and requires
      `//.agents/OWNERS` approval. Reserve Stage 2 strictly for broadly used,
      repo-wide workflows.
    - **Stage 1 (`//src/<area>/.agents/skills/`, `//src/<area>/skills/`,
      `//docs/skills/`, or `vendor/google/<area>/skills/`):** Use Stage 1 for
      subsystem rubrics, audit campaigns, hardware-specific runbooks, and vendor
      workflows. Users opt in with `fx manage-skills --add=<path>`.
6.  **Public repository hygiene (`fuchsia.git`):**
    - Keep `fuchsia.git` skills free of non-public board or chip codenames and
      private shortlinks (`go/`, `b/`).
    - Do not rely on Google-internal-only search or build tools (such as `cs
      --corpus` or `blaze`) without a portable open-source primary command (such
      as `git ls-files` scoped to target directories, excluding `out/`).

## YAML frontmatter, progressive disclosure, and size limits

1.  **Triggering contract only (`description`):**
    - Keep `description` at or under 1,024 characters with no raw `<` or `>`
      angle brackets (wrap placeholders like `` `<target>` `` in backticks in
      the body, and omit angle brackets in YAML frontmatter).
    - Structure `description` strictly as: third-person capability summary +
      positive triggers (`Use when...`) + explicit negative scope boundaries
      (`Don't use for...` or `Do NOT use for...`) to prevent co-triggering
      collisions with `/review` or sibling CLI skills.
    - Never summarize body execution rules or include executable binary paths
      (`/google/bin/releases/...`) or CLI flag syntax in `description`. Exposing
      commands in frontmatter incentivizes agents to skip reading `SKILL.md`
      safety gates and wastes global prompt tokens on every turn.
2.  **Unique keys and clean prose:**
    - Never repeat keys in YAML frontmatter (Jetski's Go `yaml.v3` loader
      rejects duplicate keys even if Python's `SafeLoader` accepts them).
    - Remove PRD pitch prose (such as "Major Beneficiary") and unmeasured
      quantitative claims (such as `"~95% context reduction"`), which agents
      repeat as verified fact.
3.  **Cardinality and modular `references/`:**
    - Keep `SKILL.md` at or under 500 lines (hard `view_file` ceiling: 800 lines
      / 45 KiB, past which instructions are truncated). Move detailed rubrics,
      schemas, and report templates into `references/*.md`.
    - Do not add `README.md` or `CHANGELOG.md` inside a skill directory.
    - Link to companion files with standard relative Markdown paths
      (`[rubric](references/rubric.md)`). Never use `file://references/...` (RFC
      8089 parses `references` as a hostname) or machine-local paths
      (`/usr/local/google/home/...`, `/google/src/cloud/...`).

## Single source of truth, portability, and execution gates

1.  **No duplication (DRY):**
    - Do not repeat baseline `//AGENTS.md` instructions (such as basic `fx test`
      usage or general doc navigation) inside skills.
    - Keep subcommand documentation canonical in one skill and cross-reference
      it rather than maintaining duplicate command tables across sibling skills.
    - Do not duplicate checklists, search tokens, redaction keys, or routing
      matrices across `SKILL.md`, `references/*.md`, and `workflow.py`; load one
      canonical JSON or Markdown file instead.
    - Delegate code self-review steps in bug-fix or authoring skills to
      `//.agents/skills/review/SKILL.md` (`deep` mode) rather than embedding
      custom zero-context review prompts. Keep only task-specific post-review
      gates (such as hunk reversibility or no symptom masking).
2.  **Subagent and tool portability:**
    - Never invoke unregistered `invoke_subagent` `TypeName` values (such as
      `"DeepInvestigator"` or `"DeepCoder"`) or non-existent tools/models. Use
      `TypeName="self"` or register custom subagents via `define_subagent`
      before `invoke_subagent`, and provide a single-agent fallback when
      subagent tools are unavailable.
    - Resolve checkout paths safely across both `fuchsia.git` and standalone
      `vendor/google` or Cog workspaces without assuming `$FUCHSIA_DIR` is set.
3.  **Workspace hygiene, verification loops, and human confirmation:**
    - Never write generated reports, intermediate logs, or `.bak` files inside
      `$FUCHSIA_DIR`; write to `/tmp` or the conversation scratch/artifact
      directory unless the user explicitly specifies an output path.
    - Any skill that modifies source or build files must require a **Build ->
      Lint -> Test** loop (`fx build`, `fx format-code`, `fx lint`, and `fx
      test`) before committing.
    - Delegate commit message formatting to `fuchsia-commit-message-auditor`.
      Never place free-text prose inside the contiguous Git `Key: Value` trailer
      block alongside `Bug:`, `Test:`, or `Change-Id:`.
    - Require explicit user confirmation before running `jiri upload`, `git
      push`, or remote Gerrit/issue tracker mutations unless the user's prompt
      explicitly asked to upload or mutate.
4.  **Prebuilt tool rollout dependency:**
    - When documenting newly added CLI flags or subcommands on prebuilt binaries
      (for example under `/google/bin/releases/...` or `fx gh`), verify the
      production binary release includes those flags before merging, or note the
      release dependency in the CL description.

## Discovery configs

1.  **Repository-managed vs. personal configs:**
    - `//.agents/{skills,workflows,plugins,agents,rules}.json` and their
      `vendor/google/.agents/` counterparts are tracked repository files
      (4-space indented via `fx format-code`) using `entries`, `inherits`,
      `exclude`, and `comment` keys with valid checkout paths. Never instruct
      users or scripts to edit workspace `.agents/*.json` for personal
      selection; personal configs belong in `~/.gemini/config/skills.json` via
      `fx manage-skills`, preserving any existing `"inherits"` list.
2.  **Dual-path registration in `vendor/google`:**
    - Because `//.agents/skills.json` inherits
      `vendor/google/.agents/skills.json` and entries scan only one level deep,
      non-default skill or workflow paths in `vendor/google` must register both
      `"<area>/skills/<name>"` (resolved from `vendor/google`) and
      `"vendor/google/<area>/skills/<name>"` (resolved from the Fuchsia checkout
      root).

## Saved dynamic workflows (.agents/workflows/)

Saved workflows execute deterministic Python orchestration via `run_workflow`:

1.  **Manifest-first documentation (`workflow.json` vs. `README.md`):**
    - A saved workflow requires `workflow.json` (`name`, `description`, optional
      `entry` defaulting to `workflow.py`, `args` JSON Schema, or `library`) and
      its entry script.
    - Jetski injects **only** `workflow.json`'s `description` and compacted
      `args` schema into the agent's system prompt; agents never read
      `README.md` automatically. Put critical usage rules (`dry_run` defaults,
      device attachment preconditions, offline archive flags) in `workflow.json`
      `description` and per-property `description` fields. Omit `README.md` or
      keep it to a short architecture overview without duplicating the `args`
      table.
2.  **Valid invocation and `call_tool` allowlist:**
    - Saved workflows expose `/<workflow-name>` directly (never `/run-workflow
      <workflow-name>` or `fx workflow`). Direct `run_workflow` tool calls
      require an absolute `ScriptPath`.
    - Inside `workflow.py`, `call_tool()` only supports `read_file`,
      `write_file`, `edit_file`, `search_web`, `invoke_subagent`,
      `send_message`, `manage_task`, and `swarm`. Calling
      `call_tool("run_workflow", ...)` or `call_tool("run_command", ...)` raises
      `ToolError`; spawn an `agent()` or run deterministic Python helpers
      instead.
3.  **Cross-repo workflow discovery:**
    - Workflows placed in `vendor/google/.agents/workflows/` are only discovered
      from `vendor/google` unless registered via `workflows.json` inheritance
      (`//.agents/workflows.json` inheriting
      `vendor/google/.agents/workflows.json`) or packaged in a plugin registered
      in `plugins.json`.
4.  **Schema sync, error semantics, and mutation gates in `workflow.py`:**
    - Keep every `args.get("<key>")` / `args["<key>"]` read in `workflow.py`
      synchronized with `workflow.json` `args.properties` and `required`.
    - Built-in `parallel()` cancels all sibling tasks if any child fails, making
      `results[i] or {}` fallbacks dead code. Use `from advanced import
      parallel` with `return_exceptions=True` when graceful degradation is
      needed.
    - Do not swallow CLI or parse failures with silent `except Exception: pass`
      or `return ""` blocks that emit `"N/A"` bug fields; log failures via
      `log()`. Check attached hardware (`adb devices`, `ffx target list`) up
      front and fail fast with actionable fallback instructions.
    - Direct `subprocess` mutations (`issues mutate`, `gerrit`, `git push`,
      `jiri upload`) bypass Jetski's per-tool confirmation prompt once the
      workflow starts. Default `dry_run` to `true` in `workflow.json` and
      require explicit opt-in before mutating external state.

## Fuchsia style rules for skills

- Run `fx format-code` and `fx lint` before uploading; wrap prose lines in
  `SKILL.md` at or under 80 characters and avoid Unicode em/en dashes (`--` or
  `-` instead).
- Use `posix-terminal` without a leading `$` prompt for Fuchsia shell examples,
  and write complete `fx` and `ffx` commands (`fx test //src/...`, `ffx
  component show`).
- Format FIDL identifiers as `library/Protocol.Method` (`UpperCamelCase` types
  and protocols, `lower_snake_case` methods and fields), and use sentence case
  for headings without backticks or emojis.

## Code review severity calibration

When reviewing changes to skills, workflows, or `.agents` configs:

- **Flag as `blocking` (`changes_requested`):**
  - Invalid `call_tool()` names, unregistered `invoke_subagent` `TypeName`s,
    `workflow.json` <-> `workflow.py` argument mismatches, dead file/skill
    links, broken `file://` URIs, or unrolled prebuilt CLI flags that fail at
    runtime.
  - Stage 2 root `//.agents/skills/` placement of narrow/subteam workflows,
    `vendor/google` skills or workflows missing root discovery wiring, or
    frontmatter `description` bloat/binary paths that break Progressive
    Disclosure.
  - Missing `dry_run: true` or user confirmation gates on external mutations,
    writing untracked outputs into `$FUCHSIA_DIR`, missing Build -> Lint -> Test
    steps on code-modifying skills, or non-public leaks in `fuchsia.git`.
  - Active `skill_linter` / SHAC errors or substantial DRY duplication across
    `AGENTS.md`, sibling skills, `references/`, and `workflow.py`.
- **Flag as `suggestion`:**
  - Minor naming refinements, tightening negative trigger phrasing, extracting
    shared templates, or trimming redundant `README.md` sections.
