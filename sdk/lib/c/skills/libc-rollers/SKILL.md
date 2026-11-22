---
name: libc-rollers
description: >
  Monitors, triages, and retriggers Fuchsia libc rollers (llvm-libc-roller,
  scudo-roller, gwp_asan-roller, and linux-syscall-support-roller) via pure
  CLI and structured JSON output, resolves LUCI Scheduler job IDs dynamically
  from roller names without hardcoded hex hashes, counts pending upstream
  commits against fuchsia.git main, and distinguishes unrelated Fuchsia or
  infra flakes from real libc, scudo, gwp_asan, elfldltl, and dynamic-linker
  failures. Use when checking libc roller status, triaging failed roller
  builds, retriggering rollers after unrelated flakes, calibrating roller SLO
  thresholds, or preparing manual rolls in an isolated Jiri worktree. Do NOT
  use for Clang, Rust, or non-libc third-party roller triage, or for general
  code reviews.
---

# Fuchsia libc rollers triage and CLI automation

Use this skill when checking the state of the Fuchsia libc-related roller bots,
counting pending upstream commits in `llvm-project/libc`, `scudo`, `gwp-asan`,
and `linux-syscall-support`, triaging failed or in-progress roller builds,
retriggering rollers after unrelated Fuchsia flakes, evaluating automated flake
retrigger policy via structured `--json` output, or preparing manual rolls.

For background on how the Fuchsia C library tracks upstream LLVM repositories
and handles manual rolls, see
[`//sdk/lib/c/README.md`](../../README.md#auto-rollers) and
[`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh).

---

## Target roller bots

The LLVM-based libc repositories are discovered dynamically from
[`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh) (`REPOS`), alongside
`linux-syscall-support` (used by `//src/lib/elfldltl` and the `//sdk/lib/ld` /
`//sdk/lib/c/dlfcn/dl` dynamic linker implementation and tests, which are
maintained in concert with `libc`):

<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->
| Roller Builder | LUCI Bucket | Upstream Mirror | Target Path in `fuchsia.git` | Schedule |
| :--- | :--- | :--- | :--- | :--- |
| `llvm-libc-roller` | `global.roller` | `https://llvm.googlesource.com/llvm-project/libc` | `third_party/llvm-libc/src` | `with 4h interval` |
| `scudo-roller` | `global.roller` | `https://llvm.googlesource.com/scudo` | `third_party/scudo/src` | `0 5 * * *` |
| `gwp_asan-roller` | `global.roller` | `https://llvm.googlesource.com/gwp-asan` | `third_party/scudo/gwp_asan` | `0 4 * * *` |
| `linux-syscall-support-roller` | `global.roller` | `https://chromium.googlesource.com/linux-syscall-support` | `third_party/linux-syscall-support/src` | `triggered` |
<!-- END AUTO-GENERATED ROLLERS TABLE -->

### Dynamic LUCI Scheduler job resolution and self-updating

LUCI Scheduler job IDs include generated hex suffixes
(`global.roller-<roller>-<hex>`) that change whenever the trigger or builder
definition in `//integration/infra/config/builders/integration_rollers.star` is
regenerated. Neither `SKILL.md` nor
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) hardcodes
those hex suffixes. Instead, `check_libc_rollers.py` resolves the exact LUCI
Scheduler job ID on the fly from the roller name on every run via:

1.  **Buildbucket build tags and summary links**: Querying recent builds on
    `<project>/global.roller/<roller>` (which uses the plain roller name without
    any hex suffix) and reading the `scheduler_job_id` tag
    (`<project>/global.roller-<roller>-<hex>`) or the
    `https://luci-scheduler.appspot.com/jobs/...` link in `summaryMarkdown`.
2.  **Live LUCI Scheduler pRPC (`scheduler.Scheduler/GetJobs`)**: Matching
    `^<bucket>-<roller>(?:-[0-9a-f]+)?$` against the live job list.
3.  **Local LUCI config
    (`//integration/infra/config/generated/*/luci/luci-scheduler.cfg`)**:
    Mapping upstream Gitiles trigger URLs to `job { id: "..." }` blocks when
    present in a local checkout.

When any of the other LUCI or repository details change (roller builder names,
LUCI buckets, upstream mirror URLs, target paths in `fuchsia.git`, or cron
schedules):

- [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py)
  self-updates the `AUTO-GENERATED ROLLERS TABLE` block in `SKILL.md` and
  `DEFAULT_ROLLERS` in
  [`scripts/libc_rollers/constants.py`](./scripts/libc_rollers/constants.py)
  when run with `--update-skill` in a writable worktree (and reports any
  detected drift without modifying files on routine read-only queries).
- If a broader LUCI implementation change occurs (for example, a new roller
  recipe or a structural change to `//sdk/lib/c/manual-roll.sh` or
  `//integration/infra/config/builders/integration_rollers.star`), update
  [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py), the
  [`scripts/libc_rollers/`](./scripts/libc_rollers/) modules, and `SKILL.md` in
  a dedicated worktree, then verify via `fx build`, `fx format-code`, `fx lint`,
  and `fx test //sdk/lib/c/skills/libc-rollers/scripts:check_libc_rollers_test`.

---

## Checkout-free CLI execution and worktree isolation

- **Pure CLI and checkout-free execution**:
  [`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) and its
  [`scripts/libc_rollers/`](./scripts/libc_rollers/) package use only the Python
  3 standard library and operate both inside a Fuchsia checkout and
  checkout-free from any standalone directory:
  - When no local Fuchsia checkout is present, it fetches
    `//manifests/third_party/all`, `//sdk/lib/c/manual-roll.sh`, and
    `//sdk/lib/c/README.md` directly from `fuchsia.googlesource.com`
    `refs/heads/main`.
  - External LUCI CLIs (`bb`, `luci-auth`) resolve from explicit `--bb-bin` and
    `--luci-auth-bin` flags (or `BB_BIN`, `LUCI_AUTH_BIN`), Fuchsia
    `prebuilt/tools/...` when inside a checkout, or `~/.local/bin`, `~/bin`, and
    `PATH`.
  - LUCI Scheduler and TreeStatus RPCs authenticate via `luci-auth token` when
    installed, or directly via `LUCI_AUTH_CREDENTIAL_HELPER` JSON token output.
- **Never modify a primary/reference Fuchsia checkout** (such as `~/fuchsia`).
  Whenever a local checkout, branch, build, `--update-skill` write, or
  `manual-roll.sh` run is needed to reproduce or fix a non-flake failure,
  **always use a dedicated Jiri worktree** (such as `fuchsia-libc-rollers`):

  ```posix-terminal
  <fuchsia-dir>/.jiri_root/bin/jiri worktree add <worktree-dir>
  <fuchsia-dir>/.jiri_root/bin/jiri worktree sync <worktree-dir>
  ```

---

## Workflow steps

### Step 1: Check pending commits, recent builds, and active or failed CLs

Run the bundled CLI script
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py):

```posix-terminal
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py
```

Pass `--table-only` to print only the 4-column Markdown status table, `--json`
for structured JSON output (run `--help` for the full JSON schema), or
`--update-skill` inside a writable worktree to sync drifted roller metadata. The
script automatically:

1.  Parses `//sdk/lib/c/manual-roll.sh` alongside companion `DEFAULT_ROLLERS`
    repositories and resolves each roller's live LUCI Buildbucket builder and
    LUCI Scheduler job ID on the fly.
2.  Fetches `manifests/third_party/all` from `fuchsia.googlesource.com`
    `refs/heads/main` and resolves when each roller's current pinned revision
    landed via Gerrit (`project:fuchsia status:merged`).
3.  Queries each upstream repo's Gitiles
    `+log/<pinned>..refs/heads/main?format=JSON` endpoint to list all pending
    commits.
4.  Queries the last 3 Buildbucket builds for each roller (plus the most recent
    failed build via `bb ls -n 1 -status failure` if none of the last 3 failed)
    and extracts the live `scheduler_job_id` tag.
5.  Extracts the Gerrit CL number and patchset for any active (`STARTED`) roll
    build as well as for the **last failed run**, and queries `bb ls -cl` to
    summarize passing, failing, and in-progress CQ tryjobs (including
    `run-postsubmit-tryjobs` subbuilds).
6.  Formats the status as a **concise 4-column Markdown table**:
    - Columns: `| Roller / CL & SLO Health | Pinned -> Head (Pending) & Last
      Landed | Pending Attempt | Last Failed Attempt |`
    - **SLO health indicator**: Each roller displays its live Service Level
      Objective (SLO) status (`Within SLO`, `SLO At Risk`, or `SLO Breached`),
      unrolled commit lag vs. target threshold, and rolling P50/P90 convergence
      latency across recent merged rolls.
    - **Approximate local and relative timestamps**: Includes both relative
      (`just now`, `~35m ago`, `~1.5h ago`, `~3.5mo ago`) and approximate local
      (`~1:40pm`, `Jul 14`) times for when each roller's pinned revision landed
      and when each pending or failed attempt ran.
    - **Pending and failed attempt details**: For every roller with `pending >
      0`, populates both `Pending Attempt` and `Last Failed Attempt` with
      clickable Gerrit CL and Buildbucket links, whether each run restored a
      previous roll attempt or added `+N` commits, tryjob counts
      (`passed/total`, `failed`, `running`), and a concise failure summary.

### Step 2: Classify failure cause (unrelated flake vs. real failure)

#### Unrelated Fuchsia or infra flake: auto-retrigger with backoff

A roll failure is an **unrelated Fuchsia flake** when all
core/bringup/libc/sanitizer compilation and unit tests pass, and the only
failure(s) are:

- Hardware provisioning, fastboot, serial, botanist/emulator networking, or OTA
  timeouts (for example, `Fastboot_error: Read error: Timed out`,
  `Timeout_attempting_to_reach_target`,
  `botanist_ERROR:_could_not_resolve_target_IP_address`, `sys_config: ERROR
  failed to read any copy`, `astro_df_release_to_canary_test_to_head.sh`).
- Tree-wide `fuchsia.git` `main` breakages in unrelated subsystems (`This
  failure may be a false rejection`, sibling builders failing on the same
  exonerated Ninja target, Bazel2GN sync checks such as `verify_bazel2gn`,
  unrelated Rust `.clippy` targets, or targets also failing on `global.ci` or
  reverted on `fuchsia.git` `main` after the roll started).
- Isolated board/product E2E or unrelated subsystem test flakes inside
  `run-postsubmit-tryjobs`.
- External downstream SDK presubmit wrappers (`1 downstream presubmit failed` /
  `1 external tryjob failed`) whose leaf Buildbucket subbuilds failed only due
  to `INFRA_FAILURE` (such as transient Gitiles `git fetch` errors) or unrelated
  downstream test flakes.

When an unrelated Fuchsia failure is **transient** (first occurrence on the
roller, not affecting other rollers, and the corresponding `global.ci` builder
is green), retrigger immediately. When rolls repeatedly fail with the **same
unrelated Fuchsia problem** (consecutive runs of the same roller, multiple
rollers hitting the same test or compile signature, or the corresponding
`global.ci` postsubmit builder also failing with that signature),
[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) classifies
the failure as **persistent** and backs off retriggering:

1.  **Wait for `STARTED` builds to finish** before retrying a persistent
    failure.
2.  **Hold off while `fuchsia-stem` is `CLOSED` or `THROTTLED`**, or while the
    corresponding `global.ci` builder's latest completed build is still failing
    with the same signature (up to an 8h cap).
3.  **Exponential backoff cooldown**: `2h` on streak 2, `4h` on streak 3, and
    `8h` on streak 4+ (persisted in `--state-file`).
4.  **Early recovery via tree, CI, and gardener activity**: Backoff clears as
    soon as `check_libc_rollers.py` observes a matching gardener revert or fix
    commit on `fuchsia.git` `refs/heads/main`, a `fuchsia-stem` tree reopen with
    healthy `global.ci` builders, or green (`SUCCESS`) `global.ci` postsubmit
    builds created after the failed roll started (`1` green build for streak 2;
    `2` consecutive green builds for streak 3+).

To manually retrigger a roller via the LUCI Scheduler pRPC API
(`scheduler.Scheduler/EmitTriggers` with a `"buildbucket": {}` trigger):

```posix-terminal
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py \
  --retrigger llvm-libc-roller \
  --reason "Retrigger after unrelated Fuchsia flake"
```

To evaluate automated flake retrigger policy across all rollers and emit
structured `monitor_summary` (`retriggered`, `retrigger_errors`, `held_flakes`,
and `actionable_issues`) on `stdout` as JSON:

```posix-terminal
python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py \
  --monitor \
  --json \
  --state-file /path/to/monitor_state.json \
  --history-file /path/to/roller_health_history.json
```

Pass `--disable-retrigger` to evaluate `--monitor` policy in read-only/dry-run
mode without sending `EmitTriggers` RPCs.

#### Real failure: investigate and report

Do **not** blindly retrigger if the failure involves:

- Compile or link errors in `//sdk/lib/c`, `//sdk/lib/ld`,
  `//sdk/lib/c/dlfcn/dl`, `//src/lib/elfldltl`, `//third_party/llvm-libc/src`,
  `//third_party/scudo/src`, `//third_party/scudo/gwp_asan`,
  `//third_party/linux-syscall-support/src`, `userboot`, or Zircon/core targets.
- Header golden or ABI mismatches under `//sdk/lib/c/include`.
- Test failures in `libc-unittests`, `scudo-unittests`, `gwp-asan-test`,
  `c-ulib-test`, `ld-tests`, `dl-tests`, `elfldltl` tests,
  `zircon/system/utest/core`, or sanitizer/allocator failures across multiple
  builders.
- Coordinated upstream changes across `llvm-libc`, `scudo`, and/or `gwp_asan`
  that must roll together atomically.

When a real failure occurs:

1.  Inspect the failing build step and test logs with `bb log <build_id> <step>`
    or `rdb query`.
2.  Report the exact failing target/test, error excerpt, and candidate upstream
    commit(s) from the pending commit list.
3.  Inside a dedicated worktree (never in the main reference checkout), use
    [`//sdk/lib/c/manual-roll.sh`](../../manual-roll.sh) (for coupled LLVM
    rolls), `jiri edit -project=linux-syscall-support=<rev>
    manifests/third_party/all` (for standalone `linux-syscall-support` rolls),
    or `fx repro <build_id>` to reproduce, bisect, or prepare a manual roll CL.

---

## Roller health SLO and historical calibration

Each roller's health is measured by **unrolled upstream commit lag** (`0h` when
`pending == 0`; otherwise the age of the oldest unrolled upstream commit in
`pinned..HEAD`) and **roll attempt reliability**:

| Roller | Target SLO (`Within SLO`) | Warning Window (`SLO At Risk`) | Breach Threshold (`SLO Breached`) | Rationale |
| :--- | :--- | :--- | :--- | :--- |
| `llvm-libc-roller` | Lag `<= 12h` (`<= 1` flake failure) | `12h < lag <= 24h` (or 2 consecutive flake failures) | `lag > 24h`, `>= 3` consecutive failures, or any actionable failure | `4h` interval + Gitiles trigger (~2.5h-3.8h clean CQ run; allows 1 flake retry within 12h). |
| `scudo-roller` | Lag `<= 18h` (`<= 1` flake failure) | `18h < lag <= 36h` (or 2 consecutive flake failures) | `lag > 36h`, `>= 3` consecutive failures, or any actionable failure | Gitiles trigger + daily `05:00` UTC fallback (allows initial attempt + 1 auto/daily retry within 18h). |
| `gwp_asan-roller` | Lag `<= 18h` (`<= 1` flake failure) | `18h < lag <= 36h` (or 2 consecutive flake failures) | `lag > 36h`, `>= 3` consecutive failures, or any actionable failure | Gitiles trigger + daily `04:00` UTC fallback (0h lag whenever `0` commits are pending upstream). |
| `linux-syscall-support-roller` | Lag `<= 18h` (`<= 1` flake failure) | `18h < lag <= 36h` (or 2 consecutive flake failures) | `lag > 36h`, `>= 3` consecutive failures, or any actionable failure | Gitiles trigger on `refs/heads/main` (0h lag whenever `0` commits are pending upstream). |

### Historical tracking and SLO calibration

[`scripts/check_libc_rollers.py`](./scripts/check_libc_rollers.py) persists
historical roll convergence and build outcomes in `--history-file`
(`roller_health_history.json`):

1.  **Continuous telemetry**: Tracks up to the last 50 merged roll CLs per
    roller from Gerrit (`convergence_hours = (submitted - created) / 3600` and
    `attempts = max(1, patchsets - 1)`) and the last 100 completed Buildbucket
    builds.
2.  **Rolling percentiles and compliance**: Computes `p50_hours`, `p90_hours`,
    `max_hours`, `within_target_pct` (goal `>= 80%`), `within_breach_pct` (goal
    `>= 95%`), `first_try_pct`, `avg_attempts`, and automated
    `calibration_advice`.
3.  **Adjusting SLO thresholds (`--set-slo`)**: Periodically review each
    roller's P50/P90 convergence latency and `within_target_pct`, and tighten or
    widen thresholds as needed:

    ```posix-terminal
    python3 sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py \
      --set-slo llvm-libc-roller=12:24 \
      --set-slo scudo-roller=18:36
    ```
