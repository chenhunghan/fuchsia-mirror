# `fx worktree` Telemetry & Metrics

`fx worktree` tracks developer worktree utilization over time to understand
average checkout usage, capacity requirements, resource consumption, and the
proportion of checkouts built recently vs. not built recently.

To avoid running background daemons while providing continuous statistical
precision, `fx worktree` uses **local piecewise-constant time integration**
combined with a **24-hour periodic snapshot upload**.

---

## Architecture Overview

1. **Local State Accumulation (`.fx/metrics/worktree_metrics.json`):**
   - Whenever any `fx worktree` command executes, the tracker records the
     current number of total, leased, built recently (built in the last 7 days),
     and not built recently worktrees.
   - The elapsed time since the previous state change is integrated into a
     running Riemann sum.
   - Running minimum and maximum (peak) counts are updated across all four
     dimensions.

2. **Daily Snapshot Upload:**
   - On the first `fx worktree` command executed after 24 hours have passed
     since `period_start_ts`, the tracker finalizes the 24-hour time-weighted
     statistics.
   - A single custom telemetry event is dispatched to Fuchsia's Google
     Analytics 4 (GA4) pipeline via
     `tools/devshell/lib/metrics_custom_report.sh`.
   - The metrics file is rotated with a fresh 24-hour window starting at the
     current timestamp and state.

---

## Mathematical Formulation: Time-Weighted Average

Worktree counts change only at discrete events (`add`, `remove`, `lease`,
`release`, build activity). Between events, the count remains constant. Over
any interval $[T_0, T_{\text{upload}}]$, the time-weighted average $\bar{N}$
is given by:

$$
\bar{N} = \frac{1}{T_{\text{upload}} - T_0}
          \int_{T_0}^{T_{\text{upload}}} N(t)\,dt
        = \frac{\sum_{i=1}^{k} N_i \cdot \Delta t_i}{T_{\text{upload}} - T_0}
$$

Where:
- $N_i$ is the count during step $i$.
- $\Delta t_i = t_i - t_{i-1}$ is the duration spent in that state.
- $T_{\text{upload}} - T_0$ is the total elapsed time of the measurement window.

This ensures accurate representation of developer usage across overnight and
multi-day periods without active polling.

---

## Local State Schema

The state file is stored at `.fx/metrics/worktree_metrics.json` in the root
checkout directory, ensuring that all worktrees in the pool share and update the
same state:

```json
{
  "period_start_ts": 1756740000.0,
  "last_change_ts": 1756783200.0,
  "current_total": 4,
  "current_leased": 2,
  "current_built_recently": 3,
  "current_not_built_recently": 1,
  "accumulated_total_seconds": 345600.0,
  "accumulated_leased_seconds": 172800.0,
  "accumulated_built_recently_seconds": 259200.0,
  "accumulated_not_built_recently_seconds": 86400.0,
  "min_total": 2,
  "max_total": 5,
  "min_leased": 0,
  "max_leased": 3,
  "min_built_recently": 1,
  "max_built_recently": 4,
  "min_not_built_recently": 0,
  "max_not_built_recently": 2
}
```

### Field Descriptions

- `period_start_ts`: Epoch timestamp (seconds) when the current 24-hour
  measurement window began.
- `last_change_ts`: Epoch timestamp (seconds) when the count was last updated or
  sampled.
- `current_total`: Current total number of physical worktree slots in the pool.
- `current_leased`: Current number of worktrees actively leased by a task or
  developer.
- `current_built_recently`: Current number of worktrees that have been built
  within the last 7 days.
- `current_not_built_recently`: Current number of worktrees that are unbuilt or
  whose last build occurred > 7 days ago (`current_total - current_built_recently`).
- `accumulated_total_seconds`: Integrated time-area product
  ($\sum N_{\text{total}} \cdot \Delta t$) in second-units.
- `accumulated_leased_seconds`: Integrated time-area product
  ($\sum N_{\text{leased}} \cdot \Delta t$) in second-units.
- `accumulated_built_recently_seconds`: Integrated time-area product
  ($\sum N_{\text{built\_recently}} \cdot \Delta t$) in second-units.
- `accumulated_not_built_recently_seconds`: Integrated time-area product
  ($\sum N_{\text{not\_built\_recently}} \cdot \Delta t$) in second-units.
- `min_*` / `max_*`: Minimum and peak counts observed across each category
  within the measurement window.

---

## Telemetry Event & GA4 Attribution

When the 24-hour window expires, `fx worktree` emits a custom event via
`tools/devshell/lib/metrics_custom_report.sh`:

- **Subcommand:** `worktree`
- **Action:** `daily_snapshot`
- **Label Structure (compact JSON with short keys):**
  ```json
  {"at":3.0,"mnt":2,"mxt":4,"al":2.0,"mnl":1,"mxl":3,"abr":2.0,"mnbr":1,"mxbr":3,"anbr":1.0,"mnnbr":1,"mxnbr":1}
  ```
  - `at`, `mnt`, `mxt`: Average, min, and max total worktrees
  - `al`, `mnl`, `mxl`: Average, min, and max leased worktrees
  - `abr`, `mnbr`, `mxbr`: Average, min, and max worktrees built recently
  - `anbr`, `mnnbr`, `mxnbr`: Average, min, and max worktrees not built recently

### Seamless Pipeline Attribution

Because `metrics_custom_report.sh` forwards the open metrics stream (file
descriptor 10) from `scripts/fx`, the event is automatically joined in the same
batch with:
- **Client ID (`client_id`):** Persistent anonymized `METRICS_UUID`.
- **Cross-Tool ID (`other_uuid`):** Correlation UUID for `ffx`, `zxdb`, and
  VS Code tools.
- **User Properties:** `internal` (Googler vs external), `is_cog` (Cloud Cog vs
  local workstation), `os`, `arch`, and `nproc`.

---

## Privacy and Data Governance

In compliance with Fuchsia's [Analytics Policy][analytics-policy]:
- Only aggregate statistical numbers (averages, minimums, peaks) are uploaded.
- No local paths, branch names, worktree identifiers, or task IDs are collected
  or transmitted.
- Telemetry strictly honors user opt-out preferences (`fx metrics disable` or
  `ffx config analytics disable`).

[analytics-policy]:
  //docs/contribute/governance/policy/analytics_collected_fuchsia_tools.md
