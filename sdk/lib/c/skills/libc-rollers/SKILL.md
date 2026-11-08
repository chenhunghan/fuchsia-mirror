---
name: libc-rollers
description: >
  Monitors, triages, and retriggers the Fuchsia libc-related roller bots
  (llvm-libc-roller, scudo-roller, and gwp_asan-roller), dynamically resolves
  LUCI Scheduler job IDs on the fly from roller names without hardcoded hex
  hashes and self-updates when LUCI or repository details change, counts
  pending upstream commits against fuchsia.git main, distinguishes unrelated
  Fuchsia/infra flakes from real libc/scudo/gwp_asan failures (with
  persistent-failure backoff and tree/CI/gardener tracking), guides manual
  rolls or fixes in an isolated Jiri worktree, and configures the scheduled
  libc-rollers-monitor sidecar automation.
---

# Fuchsia Libc Rollers Triage & Automation (`libc-rollers`)

Use this skill when checking on the state of the Fuchsia libc-related roller
bots, counting pending upstream commits in `llvm-project/libc`, `scudo`, and
`gwp-asan`, triaging failed or in-progress roller builds, retriggering rollers
after unrelated Fuchsia flakes, preparing manual rolls, or setting up the
scheduled `Libc Rollers Monitor` automation for a toolchain team member.

For background on how the Fuchsia C library tracks upstream LLVM repositories
and handles manual rolls, see
[`//sdk/lib/c/README.md`](../../README.md#auto-rollers) and
[`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh).

---

## 1. Target Roller Bots (Auto-Synced & Resolved On the Fly)

The target repositories are discovered dynamically from
[`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh) (`REPOS`):

<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->
| Roller Builder | LUCI Bucket | Upstream Mirror | Target Path in `fuchsia.git` | Schedule |
| :--- | :--- | :--- | :--- | :--- |
| `llvm-libc-roller` | `global.roller` | `https://llvm.googlesource.com/llvm-project/libc` | `third_party/llvm-libc/src` | `with 4h interval` |
| `scudo-roller` | `global.roller` | `https://llvm.googlesource.com/scudo` | `third_party/scudo/src` | `0 5 * * *` |
| `gwp_asan-roller` | `global.roller` | `https://llvm.googlesource.com/gwp-asan` | `third_party/scudo/gwp_asan` | `0 4 * * *` |
<!-- END AUTO-GENERATED ROLLERS TABLE -->

### On-the-Fly LUCI Scheduler Job Resolution & Self-Updating
LUCI Scheduler job IDs include generated hex suffixes
(`global.roller-<roller>-<hex>`) that change whenever the trigger or builder
definition in `//integration/infra/config/builders/integration_rollers.star` is
regenerated. Neither `SKILL.md` nor
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) hardcodes
those hex suffixes. Instead, `check_libc_rollers.py` resolves the exact LUCI
Scheduler job ID on the fly from the roller name on every run via:

1.  **Buildbucket build tags & summary links**: Querying recent builds on
    `<project>/global.roller/<roller>` (which uses the plain roller name without
    any hex suffix) and reading the `scheduler_job_id` tag
    (`<project>/global.roller-<roller>-<hex>`) or the
    `https://luci-scheduler.appspot.com/jobs/...` link in `summaryMarkdown`.
2.  **Live LUCI Scheduler pRPC (`scheduler.Scheduler/GetJobs`)**: Matching
    `^<bucket>-<roller>(?:-[0-9a-f]+)?$` against the live job list.
3.  **Local LUCI config
    (`//integration/infra/config/generated/*/luci/luci-scheduler.cfg`)**:
    Mapping upstream Gitiles trigger URLs to `job { id: "..." }` blocks when
    present in the checkout.

When any of the other LUCI or repository details change (roller builder names,
LUCI buckets, upstream mirror URLs, target paths in `fuchsia.git`, or cron
schedules):

- [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py)
  self-updates the `AUTO-GENERATED ROLLERS TABLE` block above and
  `DEFAULT_ROLLERS` in `check_libc_rollers.py` when run with `--update-skill` in
  a writable worktree (and reports any detected drift without modifying files on
  routine read-only queries).
- If a broader LUCI implementation change occurs (for example, a new roller
  recipe or a structural change to `//sdk/lib/c/manual-roll.sh` or
  `//integration/infra/config/builders/integration_rollers.star`), update
  [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) and
  `SKILL.md` in the dedicated worktree to match.

---

## 2. Worktree Isolation Rule

- **Never modify the primary/reference Fuchsia checkout** (e.g., `~/fuchsia`).
- Routine monitoring, triage, and retriggering (Steps 1-3 below) are read-only
  against the reference checkout.
- Whenever a local checkout, branch, build, skill self-update, or
  `manual-roll.sh` run is needed (e.g., to reproduce or fix a real non-flake
  roller failure), **always use a dedicated Jiri worktree** (such as
  `fuchsia-libc-rollers`):

  ```bash
  <fuchsia-dir>/.jiri_root/bin/jiri worktree add <worktree-dir>
  <fuchsia-dir>/.jiri_root/bin/jiri worktree sync <worktree-dir>
  ```

---

## 3. Workflow Steps

### Step 1: Check Pending Upstream Commits, Recent Builds & Active/Failed CL Triage

Run the bundled helper script
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py):

```bash
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py --update-skill
```

Pass `--table-only` to print only the 4-column Markdown status table, or
`--json` if you need structured JSON output. The script automatically:

1.  Parses `//sdk/lib/c/manual-roll.sh` and resolves each roller's live LUCI
    Buildbucket builder and LUCI Scheduler job ID on the fly (self-updating
    `SKILL.md` and `DEFAULT_ROLLERS` if any LUCI or repository details changed).
2.  Fetches `manifests/third_party/all` from `fuchsia.googlesource.com`
    `refs/heads/main` and resolves when each roller's current pinned revision
    landed via Gerrit (`project:fuchsia status:merged`).
3.  Queries each upstream repo's Gitiles
    `+log/<pinned>..refs/heads/main?format=JSON` endpoint to list all pending
    commits.
4.  Queries the last 3 Buildbucket builds for each roller (plus the most recent
    failed build via `bb ls -n 1 -status failure` if none of the last 3 failed)
    and extracts the live `scheduler_job_id` tag.
5.  Extracts the Gerrit CL number/patchset for any active (`STARTED`) roll build
    as well as for the **last failed run**, and queries `bb ls -cl` to summarize
    passing, failing, and in-progress CQ tryjobs (including
    `run-postsubmit-tryjobs` subbuilds).
6.  Prints and formats the status as a **concise 4-column Markdown table**:
    - Columns: `| Roller / CL & SLO Health | Pinned → Head (Pending) & Last
      Landed | Pending Attempt | Last Failed Attempt |`
    - **Color-Coded SLO Health Indicator**: Each roller displays its live
      Service Level Objective (SLO) status (`🟢 Within SLO`, `🟡 SLO At Risk`, or
      `🔴 SLO Breached`), unrolled commit lag vs. target threshold, and rolling
      P50/P90 convergence latency across recent merged rolls.
    - **Approximate local + relative timestamps**: Always include both relative
      (`just now`, `~35m ago`, `~1.5h ago`, `~3.5mo ago`) and approximate local
      (`~1:40pm`, `Jul 14`) times for when each roller's pinned revision landed
      and when each pending or failed attempt ran.
    - **For every roller that is not fully up to date (`pending > 0`), always
      populate both `Pending Attempt` and `Last Failed Attempt`** with clickable
      Gerrit CL and Buildbucket links, whether each run **restored a previous
      roll attempt** or is a **new roll (`+N` commits)** relative to the prior
      failed run, current build counts (`passed/total`, `failed`, `running`),
      and a concise failure summary for the last failed attempt.
    - For rollers that are fully up to date (`0` pending commits), show `--` in
      `Pending Attempt` and `-- (up to date)` in `Last Failed Attempt`.

### Step 2: Classify Failure Cause (Unrelated Flake vs. Real Failure)

#### A. Unrelated Fuchsia / Infra Flake -> Auto-Retrigger (with Persistent-Failure Backoff)
A roll failure is an **unrelated Fuchsia flake** when all
core/bringup/libc/sanitizer compilation and unit tests pass, and the only
failure(s) are:

- Hardware provisioning, fastboot, serial, botanist/emulator networking, or OTA
  timeouts (e.g., `Fastboot_error: Read error: Timed out`,
  `Timeout_attempting_to_reach_target`,
  `botanist_ERROR:_could_not_resolve_target_IP_address`, `sys_config: ERROR
  failed to read any copy`, `astro_df_release_to_canary_test_to_head.sh`).
- Tree-wide `fuchsia.git` `main` breakages in unrelated subsystems (`This
  failure may be a false rejection. Similar failures found in try...`).
- Isolated board/product E2E or unrelated subsystem test flakes inside
  `run-postsubmit-tryjobs`.

##### Persistent Unrelated Failure Backoff & Tree / Gardener State Tracking
When an unrelated Fuchsia failure is **transient** (first occurrence on the
roller, not affecting other rollers, and the corresponding `global.ci` builder
is green), you can queue a retrigger immediately (even while
`run-postsubmit-tryjobs` is still `STARTED`).

However, when rolls repeatedly fail with the **same unrelated Fuchsia problem**
(consecutive runs of the same roller, multiple rollers hitting the same test or
compile signature, or the corresponding `global.ci` postsubmit builder also
failing with that signature),
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) classifies
the failure as **persistent** and backs off retriggering so the bots do not
repeatedly waste cycles hitting the exact same failure:

1.  **Wait for `STARTED` builds to finish** before retrying a persistent
    failure.
2.  **Hold off while `fuchsia-stem` is `CLOSED` or `THROTTLED`**, or while the
    corresponding `global.ci` builder's latest completed build is still failing
    with the same signature (up to an 8h cap).
3.  **Exponential Backoff Cooldown**: `2h` on streak 2, `4h` on streak 3, and
    `8h` on streak 4+ (persisted in `monitor_state.json`).
4.  **Early Recovery via Tree / CI / Gardener Activity**: Backoff is cleared
    early as soon as
    [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) observes
    that the persistent problem is likely fixed:
    - A build gardener revert or fix commit matching the failing test/component
      lands on `fuchsia.git` `refs/heads/main` after the failed roll started.
    - The `fuchsia-stem` tree is reopened after a closure that occurred after
      the roll started, and the affected `global.ci` builders are no longer
      failing.
    - The affected `global.ci` postsubmit builder(s) complete green (`SUCCESS`)
      builds created after the failed roll started (`1` green build for streak
      2; `2` consecutive green builds for streak 3+).

To manually retrigger a roller via the LUCI Scheduler pRPC API
(`scheduler.Scheduler/EmitTriggers` with a `"buildbucket": {}` trigger, using
the dynamically resolved scheduler job ID):

```bash
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py \
  --retrigger llvm-libc-roller \
  --reason "Retrigger after unrelated Fuchsia flake"
```

#### B. Real Failure (Not an Unrelated Flake) -> Investigate & Alert
Do **not** blindly retrigger if the failure involves:

- Compile or link errors in `//sdk/lib/c`, `//third_party/llvm-libc/src`,
  `//third_party/scudo/src`, `//third_party/scudo/gwp_asan`, `userboot`, or
  Zircon/core targets.
- Header golden / ABI mismatches under `//sdk/lib/c/include`.
- Test failures in `libc-unittests`, `scudo-unittests`, `gwp-asan-test`,
  `c-ulib-test`, `zircon/system/utest/core`, or sanitizer/allocator failures
  across multiple builders.
- Coordinated upstream changes across `llvm-libc`, `scudo`, and/or `gwp_asan`
  that must roll together atomically.

When a real failure occurs:

1.  Inspect the failing build step and test logs with `bb log <build_id> <step>`
    or `rdb query`.
2.  Alert the user with the exact failing target/test, error excerpt, and
    candidate upstream commit(s) from the pending commit list.
3.  Inside the dedicated worktree (never in the main reference checkout), use
    [`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh) or `fx repro
    <build_id>` to reproduce, bisect, or prepare a manual roll CL.

---

## 4. Pinned Live UI Panel & Scheduled Monitor Sidecar (`Libc Rollers Monitor`)

[`resources/index.html`](./resources/index.html) is a local interactive status
and control page served on `127.0.0.1` by `check_libc_rollers.py --ui-server`.
It can be kept permanently pinned in a personal agent dashboard or Auxiliary
Pane (`sidecar://libc-rollers-monitor/`), brought up on-demand by any developer
or agent in their own environment, or opened directly in a local browser.

To install and start the pinned **Libc Rollers** live status panel in the
right-hand Auxiliary Pane, run:

```bash
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py --install-sidecar
```

(`--fuchsia-dir` and `--worktree-dir` are auto-detected from the current
checkout or Jiri worktree if omitted; pass `--watch-cl <cl_num>` to also track
an open Gerrit CL row in the table. To run the local HTTP UI server directly in
a browser without registering a sidecar, pass `--ui-server`.)

When invoked from an agent conversation, `--install-sidecar` writes
`~/.gemini/config/sidecars/libc-rollers-monitor/sidecar.json` (`has_web_ui:
true`, `SIDECAR_UI_ENTRYPOINT_AUX_PANE`) and starts the sidecar immediately.
Surface the clickable pill in chat so the user can open and pin the panel with
one click:

```markdown
[Libc Rollers](sidecar://libc-rollers-monitor/)
```

The sidecar (`check_libc_rollers.py --ui-server`) provides both:

- **Pinned Live Status Table (`[Libc
  Rollers](sidecar://libc-rollers-monitor/)`)**: Displays the 4-column status
  table (`Roller / CL & SLO Health`, `Pinned → Head (Pending) & Last Landed`,
  `Pending Attempt`, `Last Failed Attempt`) with color-coded SLO health pills
  and row indicators, live CQ progress bars, relative + approximate local
  timestamps, collapsible pending commit lists, a **Refresh** button, and
  per-roller **Retrigger** buttons.
- **Silent Flake Handling & Issue-Specific Conversations**: Healthy runs and
  rolls that fail solely due to unrelated Fuchsia/infra flakes are logged to
  `sidecar.log` and automatically retriggered via LUCI Scheduler (with
  persistent-failure backoff) without creating a new agent conversation. A new
  conversation is spawned in the dedicated worktree (`agentapi new-conversation
  --title="<issue>"`) **only** when a roller encounters a non-flake failure that
  needs developer attention, deduplicated in `monitor_state.json`.

---

## 5. Roller Health SLO & Historical Calibration (`roller_health_history.json`)

Each roller's health is measured by **unrolled upstream commit lag** (`0h` when
`pending == 0`; otherwise the age of the oldest unrolled upstream commit in
`pinned..HEAD`) and **roll attempt reliability**:

| Roller | Target SLO (🟢 `Within SLO`) | Warning Window (🟡 `SLO At Risk`) | Breach Threshold (🔴 `SLO Breached`) | Rationale |
| :--- | :--- | :--- | :--- | :--- |
| `llvm-libc-roller` | Lag `<= 12h` (<= 1 flake failure) | `12h < lag <= 24h` (or 2 consecutive flake failures) | `lag > 24h`, `>= 3` consecutive failures, or any actionable failure | `4h` interval + Gitiles trigger (~2.5h-3.8h clean CQ run; allows 1 flake retry within 12h). |
| `scudo-roller` | Lag `<= 18h` (<= 1 flake failure) | `18h < lag <= 36h` (or 2 consecutive flake failures) | `lag > 36h`, `>= 3` consecutive failures, or any actionable failure | Gitiles trigger + daily `05:00` UTC fallback (allows initial attempt + 1 auto/daily retry within 18h). |
| `gwp_asan-roller` | Lag `<= 18h` (<= 1 flake failure) | `18h < lag <= 36h` (or 2 consecutive flake failures) | `lag > 36h`, `>= 3` consecutive failures, or any actionable failure | Gitiles trigger + daily `04:00` UTC fallback (0h lag whenever `0` commits are pending upstream). |

### Historical Tracking & Ongoing SLO Calibration Plan
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) persists
historical roll convergence and build outcomes in `roller_health_history.json`
(alongside `monitor_state.json`):

1.  **Continuous Telemetry**: Tracks up to the last 50 merged roll CLs per
    roller from Gerrit (`convergence_hours = (submitted - created) / 3600` and
    `attempts = max(1, patchsets - 1)`) and the last 100 completed Buildbucket
    builds.
2.  **Rolling Percentiles & Compliance**: Computes `p50_hours`, `p90_hours`,
    `max_hours`, `within_target_pct` (goal `≥ 80%`), `within_breach_pct` (goal
    `≥ 95%`), `first_try_pct`, `avg_attempts`, and automated
    `calibration_advice` (surfaced in the CLI output and as a hover tooltip on
    each roller's P50/P90 line in the UI panel).
3.  **Adjusting SLO Thresholds (`--set-slo`)**: Periodically review each
    roller's P50/P90 convergence latency and `within_target_pct`. Distinguish
    multi-attempt CQ flake retries (where `avg_attempts` is elevated) from
    baseline roll cadence, and tighten or widen thresholds as needed:

    ```bash
    python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py \
      --set-slo llvm-libc-roller=12:24 \
      --set-slo scudo-roller=18:36
    ```
