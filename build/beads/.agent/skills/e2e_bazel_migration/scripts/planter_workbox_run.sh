#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Runs the Planter tasks of one e2e-bazel-migration run in workboxes
# (workbox mode) and keeps the workboxes, the Planter ledger and the run
# state in sync:
#   prepare      choose the run's workboxes (reuse idle ones, create new ones)
#   record-task  record a queued Planter task in the run state
#   touch        renew the run's lease
#   upload       start one Planter runner per workbox, wait until every task
#                is uploaded or has failed, print RESULT lines, tear down
#   review       option C phase 2: CQ and reviewer assignment
#   teardown     stop the run's runners and remove its workboxes
#   sweep        tear down the workboxes of runs that are no longer live
#
# Every run has a state file, ~/.planter/e2e-workbox/runs/<run_id>.json. A run
# is live while the process of its last subcommand runs, or until its lease
# expires; the sweep only touches runs that are not live. A host-wide lock
# serializes the sweep, box selection and teardown decisions.
#
# The script never runs `planter fleets unpin` and never deletes tasks: a task
# that must not continue is paused, and its slot assignment is removed with
# `planter fleets unassign`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<EOF
Usage: $(basename "$0") <subcommand> [flags]

Subcommands:
  prepare --slots=<K> [--tree=<T>] [--fx-set=<ARGS>] [--keep-boxes]
          [--option=A|C] [--setup-ran]
      Sweep runs that are no longer live, then choose up to K workboxes for a
      new run: idle workboxes on tree T are reused, new ones are named e2e<n>
      (within the mount budget). Runs \`planter workbox start <box>
      --create-only\` for each. Prints RUN_ID=, BOXES=, SLOTS= (same order as
      BOXES=), TREE=, RUN_DIR= and a CAPACITY line.
        --tree        Tree backend (default: the one the workbox helper reports).
        --fx-set      \`fx set\` arguments for new workboxes.
        --keep-boxes  Never destroy the workboxes this run creates.
        --option      A (default): upload tears down. C: upload keeps the
                      workboxes of uploaded tasks for \`review\`.
        --setup-ran   Workbox setup ran in this skill run: copy its setup record
                      into the run state.
  record-task --run-id=<ID> --task-id=<TASK>
      Record a queued Planter task (call it right after each successful
      add-task). Prints TASK_RECORDED=.
  touch --run-id=<ID>
      Renew the run's lease. Prints LEASE_UNTIL=.
  upload --run-id=<ID> [--timeout-secs=<N>] [--model=<MODELS>]
         [--model-fallbacks=<MODELS>] [--no-speed-force]
      Start one runner per workbox for the recorded tasks, wait until each task
      is done, print RESULT lines and tear down (option C keeps the workboxes
      of uploaded tasks).
        --model            Comma-separated models for Planter's agents, passed
                           to \`planter workbox start\` only if given. By
                           default, Planter picks the models itself.
        --model-fallbacks  Comma-separated models to fall back to after
                           --model, passed only if given. An empty value
                           disables the fallbacks.
        --no-speed-force   Pass --speed-force=false to \`planter workbox
                           start\` instead of --speed-force, so that the
                           runners do not use speed force even if Planter's
                           own settings turn it on. By default, the runners
                           run the coder, review assistant and explainer
                           agents on a faster model first.
  review --run-id=<ID> [--cq-timeout-secs=<N>] [--model=<MODELS>]
         [--model-fallbacks=<MODELS>] [--no-speed-force]
      Option C phase 2: restart the kept workboxes with CQ and reviewer
      assignment, wait, print REVIEW lines, and tear down.
      --model, --model-fallbacks and --no-speed-force are the same as for
      upload.
  teardown --run-id=<ID>
      Stop the run's runners and remove its workboxes (reused workboxes are
      only released).
  sweep
      Tear down the workboxes of runs that are no longer live. Prints SWEEP
      lines.

Flags accept both "--flag=value" and "--flag value".

Output:
  RESULT <task_id> <UPLOADED|UPLOADED_WIP|LANDED|FAILED|STRANDED|TIMEOUT|NOT_STARTED|NOT_FINISHED|PAUSED> <cl|-> <box|-> <reason>
  REVIEW <task_id> <REVIEWER_ASSIGNED|REVIEW_CQ_FAILED|REVIEW_CQ_PASSED_NOT_ASSIGNED|REVIEW_TIMEOUT|REVIEW_STRANDED|REVIEW_SKIPPED_WIP> <cl> <detail>
  SWEEP <text>                      (orphan sweep report)
  PLANTER_WARNINGS_BEGIN ... PLANTER_WARNINGS_END
  BOXES_LEAKED=<names|none>
  KEPT_BOXES=<names|none>           (upload, option C)
  RESULT_JSON=<file> / REVIEW_JSON=<file>
  STOP_REASON=<text>                (with exit 12)

Exit codes:
  0   success (upload: every task UPLOADED, UPLOADED_WIP or LANDED)
  1   internal error
  2   usage error
  10  upload: some tasks did not produce a CL
  11  upload: no task produced a CL
  12  STOP: no workbox available, every runner failed to start, or the lock
      could not be taken (see STOP_REASON=)

Environment:
  E2E_WB_HELPER               The workbox helper command (required, except
                              for record-task and touch).
  DRY_RUN                     If set, print mutating commands instead of
                              running them.
  E2E_WB_LEASE_SECS           Lease length (default: 3600).
  E2E_WB_UPLOAD_TIMEOUT_SECS  upload deadline (default: 21600).
  E2E_WB_CQ_TIMEOUT_SECS      review deadline (default: 28800).
  E2E_WB_STOP_GRACE_SECS      How long to wait for a runner to stop
                              (default: 900).
  E2E_WB_LOCK_WAIT_SECS       How long to wait for the lock (default: 900).
  E2E_WB_POLL_SECS            upload poll interval (default: 30).
  E2E_WB_REVIEW_POLL_SECS     review poll interval (default: 60).
  E2E_WB_REGISTER_SECS        How long to wait for a started runner to
                              register (default: 60).
EOF
}

die() { echo "error: $*" >&2; exit 1; }
usage_error() {
  echo "error: $*" >&2
  echo "Run '$(basename "$0") --help' for usage." >&2
  exit 2
}
log() { echo "[$(date -u +%H:%M:%S)] $*" >&2; }
is_dry() { [[ -n "${DRY_RUN:-}" ]]; }
now() { date +%s; }

WB_HOME="$HOME/.planter/e2e-workbox"
RUNS_DIR="$WB_HOME/runs"
LOCK_FILE="$WB_HOME/lock"
SETUP_RECORD="$WB_HOME/setup/last.json"
LOG_ROOT="$HOME/.planter/logs/e2e"

LEASE_SECS="${E2E_WB_LEASE_SECS:-3600}"
UPLOAD_TIMEOUT="${E2E_WB_UPLOAD_TIMEOUT_SECS:-21600}"
CQ_TIMEOUT="${E2E_WB_CQ_TIMEOUT_SECS:-28800}"
STOP_GRACE="${E2E_WB_STOP_GRACE_SECS:-900}"
LOCK_WAIT="${E2E_WB_LOCK_WAIT_SECS:-900}"
POLL_SECS="${E2E_WB_POLL_SECS:-30}"
REVIEW_POLL_SECS="${E2E_WB_REVIEW_POLL_SECS:-60}"
REGISTER_SECS="${E2E_WB_REGISTER_SECS:-60}"

BOOT_ID="$(cat /proc/sys/kernel/random/boot_id 2>/dev/null || echo unknown)"
HOST="$(hostname -s 2>/dev/null || hostname)"
EMPTY='{}'

# ---------------------------------------------------------------------------
# Commands

PLANTER="planter"
resolve_planter() {
  PLANTER="$(command -v planter || echo "$HOME/.local/bin/planter")"
}

# Runs planter with the lock's file descriptor closed, so that no process it
# leaves behind (for example a detached runner) can hold the Workbox lock.
pl() { "$PLANTER" "$@" 9>&-; }

# A planter call that changes something. With DRY_RUN it is printed instead.
# Its output goes to stderr, so that stdout carries only this script's result.
quote_cmd() {
  local s
  s="$(printf '%q ' "$@")"
  echo "${s% }"
}

pl_mut() {
  if is_dry; then
    echo "DRY_RUN: planter $(quote_cmd "$@")"
    return 0
  fi
  pl "$@" >&2
}

# helper <subcommand> [args...]: runs the workbox helper and leaves its JSON
# result in HELPER_JSON. Returns the helper's exit code, or 1 if its last line
# is not a JSON object with "api": 1.
HELPER_JSON='{}'
helper() {
  [[ -n "${E2E_WB_HELPER:-}" ]] \
    || die "E2E_WB_HELPER is not set; set it to the workbox helper command"
  local quoted="" arg out rc=0 last
  for arg in "$@"; do
    quoted+=" $(printf '%q' "$arg")"
  done
  out="$(bash -c "$E2E_WB_HELPER$quoted" </dev/null 9>&-)" || rc=$?
  last="$(printf '%s\n' "$out" | tail -n 1)"
  HELPER_JSON='{}'
  if ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$last"; then
    echo "error: the workbox helper ($1) printed no JSON result" >&2
    return 1
  fi
  if [[ "$(jq -r '.api' <<<"$last")" != 1 ]]; then
    echo "error: the workbox helper is incompatible" \
         "(api $(jq -r '.api' <<<"$last"))" >&2
    return 1
  fi
  HELPER_JSON="$last"
  return "$rc"
}
hj() { jq -r "$@" <<<"$HELPER_JSON"; }

# ---------------------------------------------------------------------------
# Run state

STATE=""
RUN_ID=""
RUN_DIR=""
WARN_FILE=""
LEAK_FILE=""

sget() { jq -r "$@" "$STATE"; }

# sset [jq args...] <filter>: updates the state file atomically.
sset() {
  local tmp
  tmp="$(mktemp "$STATE.XXXXXX")"
  if jq "$@" "$STATE" >"$tmp"; then
    mv -f "$tmp" "$STATE"
  else
    rm -f "$tmp"
    die "failed to update $STATE"
  fi
}

renew_lease() {
  [[ -n "$STATE" && -f "$STATE" ]] || return 0
  sset --argjson now "$(now)" --argjson lease "$LEASE_SECS" \
    '.updated_at = $now | .lease_until = ($now + $lease)'
}

# Prints the start time of process $1 (field 22 of /proc/<pid>/stat). The
# fields are counted after the last ")", because the command name can contain
# spaces and ")".
proc_start() {
  local stat rest
  stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  rest="${stat##*)}"
  local -a fields
  read -r -a fields <<<"$rest"
  echo "${fields[19]:-}"
}

pid_alive() {
  local pid="$1" start="$2" cur
  [[ "$pid" =~ ^[0-9]+$ && "$pid" != 0 ]] || return 1
  cur="$(proc_start "$pid")" || return 1
  [[ -n "$cur" && "$cur" == "$start" ]]
}

# is_live <state file>: the run's boot matches, and its process is alive or
# its lease has not expired.
is_live() {
  local boot pid start lease
  IFS='|' read -r boot pid start lease < <(jq -r \
    '"\(.boot_id // "-")|\(.pid // 0)|\(.pid_start // "-")|\(.lease_until // 0 | floor)"' \
    "$1" 2>/dev/null || echo "-|0|-|0")
  [[ "$boot" == "$BOOT_ID" ]] || return 1
  pid_alive "$pid" "$start" && return 0
  (( $(now) < lease ))
}

# Records this process as the run's process. Short subcommands pass "soft":
# they don't replace another live process of the run.
claim_process() {
  local pid start
  if [[ "${1:-}" == soft ]]; then
    pid="$(sget '.pid // 0')"
    start="$(sget '.pid_start // ""')"
    if [[ "$pid" != "$$" ]] && pid_alive "$pid" "$start"; then
      return 0
    fi
  fi
  sset --argjson pid "$$" --arg start "$(proc_start $$)" --arg boot "$BOOT_ID" \
    '.pid = $pid | .pid_start = $start | .boot_id = $boot'
}

set_phase() { sset --arg p "$1" '.phase = $p'; }

on_exit() {
  renew_lease 2>/dev/null || true
}

valid_run_id() { [[ "$1" =~ ^[A-Za-z0-9._-]+$ ]]; }

# Loads the state of run $1. Returns 1 if it has no state file.
load_run() {
  valid_run_id "$1" || usage_error "invalid run ID: $1"
  RUN_ID="$1"
  STATE="$RUNS_DIR/$RUN_ID.json"
  [[ -f "$STATE" ]] || { STATE=""; return 1; }
  RUN_DIR="$(sget --arg d "$LOG_ROOT/$RUN_ID" '.run_dir // $d')"
  mkdir -p "$RUN_DIR"
  WARN_FILE="$RUN_DIR/warnings.txt"
  touch "$WARN_FILE"
  local prefix
  prefix="$(sget '.path_prefix // ""')"
  [[ -z "$prefix" ]] || PATH="$prefix:$PATH"
  resolve_planter
}

# ---------------------------------------------------------------------------
# Lock

lock_acquire() {
  mkdir -p "$WB_HOME"
  exec 9>"$LOCK_FILE"
  if ! flock -w "$LOCK_WAIT" 9; then
    exec 9>&-
    echo "STOP_REASON=Another skill run holds the Workbox lock"
    exit 12
  fi
}

lock_release() {
  flock -u 9 2>/dev/null || true
  exec 9>&-
}

# ---------------------------------------------------------------------------
# Warnings

warn() {
  echo "warning: $*" >&2
  [[ -z "$WARN_FILE" ]] || printf '%s\n' "$*" >>"$WARN_FILE"
}

# Reads a Planter log on stdin and appends its new Planter warnings to
# WARN_FILE: lines with ⚠️ (plus the indented command line after each) and
# "[machinery] warning:" lines, without the retry and halt lines.
extract_warnings() {
  [[ -n "$WARN_FILE" ]] || return 0
  local new
  new="$(awk '
    function flush() { if (rec != "") print rec; rec = "" }
    {
      if (want_cmd && $0 ~ /^[ \t]+[^ \t]/) {
        rec = rec "\037" $0; want_cmd = 0; next
      }
      want_cmd = 0
      if ($0 ~ /⚠️/ || $0 ~ /\[machinery\] warning:/) {
        if ($0 ~ /Attempt .* failed \(will retry\)/ || $0 ~ /Task .* failed/) next
        flush(); rec = $0; want_cmd = ($0 ~ /⚠️/); next
      }
      flush()
    }
    END { flush() }')"
  [[ -n "$new" ]] || return 0
  local old
  old="$(awk '
    function flush() { if (rec != "") print rec; rec = "" }
    /^[ \t]/ && rec != "" { rec = rec "\037" $0; next }
    { flush(); rec = $0 }
    END { flush() }' "$WARN_FILE")"
  awk -v old="$old" '
    BEGIN { n = split(old, o, "\n"); for (i = 1; i <= n; i++) seen[o[i]] = 1 }
    !seen[$0]++ { gsub("\037", "\n"); print }' <<<"$new" >>"$WARN_FILE"
}

print_warnings() {
  echo "PLANTER_WARNINGS_BEGIN"
  [[ -z "$WARN_FILE" || ! -f "$WARN_FILE" ]] || cat "$WARN_FILE"
  echo "PLANTER_WARNINGS_END"
}

leaked_list() {
  if [[ -n "$LEAK_FILE" && -s "$LEAK_FILE" ]]; then
    sort -u "$LEAK_FILE" | paste -sd, -
  else
    echo none
  fi
}

# ---------------------------------------------------------------------------
# Reading boxes and the fleet

declare -A WB_STATE=() WB_RUNNER=() WB_TASK=() WB_TREE=() WB_SLOT=()
WB_NAMES=()

# Parses `planter workbox list`. The runner column contains a space
# ("<id> (pid=<n>)"), so columns are cut at the next key. A slot= column is
# used if a later Planter prints one.
wb_refresh() {
  local out name state runner task tree slot
  out="$(pl workbox list 2>/dev/null)" || {
    echo "warning: planter workbox list failed" >&2
    return 1
  }
  WB_STATE=() WB_RUNNER=() WB_TASK=() WB_TREE=() WB_SLOT=() WB_NAMES=()
  while IFS='|' read -r name state runner task tree slot; do
    [[ -n "$name" ]] || continue
    WB_NAMES+=("$name")
    WB_STATE[$name]="$state"
    WB_RUNNER[$name]="$runner"
    WB_TASK[$name]="$task"
    WB_TREE[$name]="$tree"
    WB_SLOT[$name]="$slot"
  done < <(awk '
    function col(s, key, stop,   i, r, j) {
      i = index(s, " " key "=")
      if (i == 0) return ""
      r = substr(s, i + length(key) + 2)
      j = (stop != "") ? index(r, " " stop "=") : index(r, " ")
      if (j > 0) r = substr(r, 1, j - 1)
      gsub(/^ +| +$/, "", r)
      return r
    }
    /^  - / {
      runner = col($0, "runner", "task"); split(runner, rr, " ")
      runner = (rr[1] == "") ? "-" : rr[1]
      slot = col($0, "slot", ""); sub(/,$/, "", slot)
      print $2 "|" col($0, "state", "") "|" runner "|" col($0, "task", "") \
        "|" col($0, "tree", "") "|" slot
    }' <<<"$out")
}

box_exists() { [[ -n "${WB_STATE[$1]+x}" ]]; }
box_runner() { echo "${WB_RUNNER[$1]:--}"; }

declare -A FLEET_MEMBER=()
FLEET_RUNNERS=()

# Parses `planter fleets list`: member lines (slot IDs) and the Runners
# section ("<id> pid=<n> <host | workbox <box> [on <host>]> ... slots=<ids>").
fleet_refresh() {
  local out kind a b c
  out="$(pl fleets list 2>/dev/null)" || {
    echo "warning: planter fleets list failed" >&2
    return 1
  }
  FLEET_MEMBER=() FLEET_RUNNERS=()
  while IFS='|' read -r kind a b c; do
    case "$kind" in
      M) FLEET_MEMBER[$a]=1 ;;
      R) FLEET_RUNNERS+=("$a|$b|$c") ;;
    esac
  done < <(awk '
    /^Members \(/ { sec = "m"; next }
    /^Runners \(/ { sec = "r"; next }
    /^[^ ]/ { sec = ""; next }
    sec == "m" && /^  - / { print "M|" $2; next }
    sec == "r" && /^  - / {
      box = "-"; slots = ""
      if ($4 == "workbox" && NF >= 5) box = $5
      for (i = 3; i <= NF; i++) if ($i ~ /^slots=/) slots = substr($i, 7)
      print "R|" $2 "|" box "|" slots
    }' <<<"$out")
}

# inspect_task <id>: prints the fields of `planter inspect --task` this script
# uses, as one JSON object.
inspect_task() {
  local out
  out="$(pl inspect --task="$1" 2>/dev/null)" || return 1
  jq -c '.task | {
      state: (.state // ""), phase: (.phase // ""),
      change_id: (.change_id // ""), current_patchset: (.current_patchset // 0),
      local_work: (.local_work // false), wip: (.wip // false),
      paused: (.paused // false), reviewer: (.assigned_reviewer // ""),
      cq_patchset: (.cq_patchset // 0),
      hint: (.last_validation_hint // "" | gsub("[\r\n]+"; " ") | .[0:300]),
      workdir: (.workdir // ""), assigned_slot: (.assigned_slot // ""),
      awaiting_ack: ((.phase // "") == "MONITORING_REVIEW" and
        ((.pending_ack // {}) as $p | ($p.needs_ack // false) and
          (($p.record.acknowledgment.patchset // -1) != ($p.record.patchset // 0))))
    }' <<<"$out" 2>/dev/null
}

# Prints the CL number in an inspect object, or nothing.
task_cl() { jq -r 'if (.change_id | test("^[0-9]+$")) then .change_id else "" end' <<<"$1"; }

pause_task() {
  local id="$1"
  pl_mut pause "$id" || warn "⚠️ planter pause $id failed"
}

assign_task() {
  local task="$1" slot="$2" out rc=0
  if is_dry; then
    echo "DRY_RUN: planter fleets assign $slot $task"
  else
    out="$(pl fleets assign "$slot" "$task" 2>&1)" || rc=$?
    if (( rc != 0 )) || [[ "$out" != *"Assigned $task to slot $slot."* ]]; then
      warn "⚠️ planter fleets assign $slot $task did not confirm the assignment: ${out//$'\n'/ }"
      return 1
    fi
  fi
  sset --arg t "$task" --arg s "$slot" '.assigned[$t] = $s'
}

unassign_task() {
  local task="$1"
  pl_mut fleets unassign "$task" || warn "⚠️ planter fleets unassign $task failed"
  sset --arg t "$task" 'del(.assigned[$t])'
}

# Waits until workbox $1 has no runner, up to $2 seconds.
wait_no_runner() {
  local box="$1" deadline=$(( $(now) + $2 ))
  is_dry && return 0
  while :; do
    wb_refresh || true
    [[ "$(box_runner "$box")" == - ]] && return 0
    (( $(now) < deadline )) || return 1
    sleep 5
  done
}

# Waits until workbox $1 has a runner, up to REGISTER_SECS.
wait_runner() {
  local box="$1" deadline=$(( $(now) + REGISTER_SECS ))
  is_dry && return 0
  while :; do
    wb_refresh || true
    [[ "$(box_runner "$box")" != - ]] && return 0
    (( $(now) < deadline )) || return 1
    sleep 2
  done
}

stop_box() {
  local box="$1"
  pl_mut workbox stop "$box" || warn "⚠️ planter workbox stop $box failed"
  wait_no_runner "$box" "$STOP_GRACE" \
    || warn "⚠️ The runner in workbox $box did not stop within ${STOP_GRACE}s"
}

# ---------------------------------------------------------------------------
# Teardown (per box)

# Prints the run's tasks that belong to workbox $1: tasks it ran (results),
# its current task, tasks whose checkout is its tree, and tasks assigned to
# its slot.
box_tasks() {
  local box="$1" slot t j
  slot="$(sget --arg b "$box" '.slots[$b] // ""')"
  {
    sget --arg b "$box" '(.results // {}) | to_entries[] | select(.value.box == $b) | .key'
    if [[ -n "${WB_TASK[$box]:-}" && "${WB_TASK[$box]}" != idle ]]; then
      sget --arg t "${WB_TASK[$box]}" '(.task_ids // [])[] | select(. == $t)'
    fi
    [[ -z "$slot" ]] || sget --arg s "$slot" \
      '(.assigned // {}) | to_entries[] | select(.value == $s) | .key'
    if [[ -n "${WB_TREE[$box]:-}" ]]; then
      for t in $(sget '(.task_ids // [])[]'); do
        j="$(inspect_task "$t")" || continue
        [[ "$(jq -r '.workdir' <<<"$j")" != "${WB_TREE[$box]}" ]] || echo "$t"
      done
    fi
  } | awk 'NF && !seen[$0]++'
}

capture_task() {
  local t="$1"
  pl inspect --task="$t" >"$RUN_DIR/$t.inspect.json" 2>&1 || true
  pl logs "$t" -n 0 >"$RUN_DIR/$t.log" 2>&1 || true
}

# teardown_box <box>: stop, capture, pause the tasks left in it, unassign,
# destroy (created boxes) or release (reused boxes), verify. Removes the box
# from the state unless it could not be destroyed.
teardown_box() {
  local box="$1" slot created keep exists=0 t j st cl paused outcome rc
  slot="$(sget --arg b "$box" '.slots[$b] // ""')"
  created="$(sget --arg b "$box" '(.created // []) | index($b) != null')"
  keep="$(sget '.keep_boxes // false')"
  log "Tearing down workbox $box"
  wb_refresh || true
  if box_exists "$box"; then
    exists=1
  elif helper boxes; then
    [[ "$(hj --arg b "$box" '[.boxes[]? | select(.name == $b)] | length')" == 0 ]] \
      || exists=1
  fi

  # 1. Stop the runner.
  if [[ "$(box_runner "$box")" != - ]]; then
    stop_box "$box"
  fi

  # 2. Capture the logs before anything is destroyed.
  if (( exists )); then
    pl workbox logs "$box" -n 0 >"$RUN_DIR/$box.planter.log" 2>&1 || true
    extract_warnings <"$RUN_DIR/$box.planter.log"
  fi

  # 3. Tasks left in the box: pause them (never unpin).
  for t in $(box_tasks "$box"); do
    outcome="$(sget --arg t "$t" '.results[$t].outcome // ""')"
    case "$outcome" in
      UPLOADED|UPLOADED_WIP|LANDED) ;;
      *) capture_task "$t" ;;
    esac
    j="$(inspect_task "$t")" || continue
    st="$(jq -r '.state' <<<"$j")"
    paused="$(jq -r '.paused' <<<"$j")"
    cl="$(jq -r '.change_id' <<<"$j")"
    [[ "$paused" != true ]] || continue
    if [[ -z "$cl" && ( "$st" == QUEUED || "$st" == ACTIVE ) ]] \
        || [[ -n "$cl" && "$st" == ACTIVE ]]; then
      pause_task "$t"
    fi
  done

  # 4. Remove the slot assignments of the run's tasks.
  if [[ -n "$slot" ]]; then
    for t in $(sget --arg s "$slot" \
        '(.assigned // {}) | to_entries[] | select(.value == $s) | .key'); do
      unassign_task "$t"
    done
  fi

  # 5. Destroy created boxes; reused boxes are only released.
  if [[ "$created" == true && "$keep" != true ]]; then
    if (( exists )); then
      pl_mut workbox destroy "$box" || warn "⚠️ planter workbox destroy $box failed"
    fi
    # 6. Verify.
    if ! is_dry; then
      rc=0
      helper box-gone "$box" || rc=$?
      if (( rc != 0 )); then
        warn "⚠️ Workbox $box could not be destroyed; run: planter workbox destroy $box"
        [[ -z "$LEAK_FILE" ]] || echo "$box" >>"$LEAK_FILE"
        return 0
      fi
    fi
  fi
  sset --arg b "$box" \
    '.created = ((.created // []) - [$b]) | .reused = ((.reused // []) - [$b])
     | del(.slots[$b])'
}

run_boxes() { sget '((.created // []) + (.reused // []))[]'; }

# teardown_run [keep...]: tears down every box of the run except the given
# ones. The decision is taken under the lock. Removes the state file when no
# box is left, unless KEEP_STATE=1.
KEEP_STATE=0
teardown_run() {
  local -a keep=("$@") boxes=()
  local box k skip
  lock_acquire
  claim_process
  set_phase tearing_down
  for box in $(run_boxes); do
    skip=0
    for k in "${keep[@]}"; do [[ "$k" != "$box" ]] || skip=1; done
    (( skip )) || boxes+=("$box")
  done
  lock_release
  for box in "${boxes[@]}"; do
    teardown_box "$box"
  done
  lock_acquire
  if (( ! KEEP_STATE )) && [[ -z "$(run_boxes)" ]]; then
    rm -f "$STATE"
    STATE=""
  fi
  lock_release
}

# ---------------------------------------------------------------------------
# Orphan sweep (the caller holds the lock)

# sweep_orphans <report file>: tears down every run that is not live and
# appends the report to the file.
sweep_orphans() {
  local report="$1" f own="${STATE:-}"
  mkdir -p "$RUNS_DIR"
  for f in "$RUNS_DIR"/*.json; do
    [[ -f "$f" && "$f" != "$own" ]] || continue
    if ! jq -e 'type == "object"' "$f" >/dev/null 2>&1; then
      warn "⚠️ Ignoring unreadable run state $f"
      continue
    fi
    is_live "$f" && continue
    ( sweep_one "$f" "$report" ) || warn "⚠️ Failed to sweep run state $f"
  done
}

sweep_one() {
  local f="$1" report="$2" t j cl st box
  STATE="$f"
  RUN_ID="$(sget '.run_id // "?"')"
  RUN_DIR="$(sget --arg d "$LOG_ROOT/$RUN_ID" '.run_dir // $d')"
  mkdir -p "$RUN_DIR"
  WARN_FILE="$report"
  local prefix
  prefix="$(sget '.path_prefix // ""')"
  [[ -z "$prefix" ]] || PATH="$prefix:$PATH"
  echo "Orphan sweep: run $RUN_ID (phase $(sget '.phase // "?"')) is not live;" \
       "tearing down its workboxes: $(run_boxes | paste -sd, - | sed 's/^$/none/')" \
       >>"$report"
  for box in $(run_boxes); do
    teardown_box "$box"
  done
  for t in $(sget '(.task_ids // [])[]'); do
    j="$(inspect_task "$t")" || continue
    cl="$(task_cl "$j")"
    st="$(jq -r '.state' <<<"$j")"
    if [[ -n "$cl" ]]; then
      echo "Orphan sweep: orphaned run $RUN_ID: $t -> cl $cl" >>"$report"
    elif [[ "$st" == QUEUED || "$st" == ACTIVE ]]; then
      [[ "$(jq -r '.paused' <<<"$j")" == true ]] || pause_task "$t"
      [[ -z "$(jq -r '.assigned_slot' <<<"$j")" ]] || unassign_task "$t"
      if [[ "$(jq -r '.local_work' <<<"$j")" == true ]]; then
        echo "Orphan sweep: orphaned run $RUN_ID: $t paused (no CL); to decide:" \
             "planter fleets unpin $t --yes (its local work was in a removed" \
             "workbox), or planter delete-task $t" >>"$report"
      else
        echo "Orphan sweep: orphaned run $RUN_ID: $t paused (no CL); to decide:" \
             "planter delete-task $t" >>"$report"
      fi
    fi
  done
  if [[ -z "$(run_boxes)" ]]; then
    rm -f "$f"
  fi
}

# Prints the report lines of the sweep as SWEEP lines, and copies the whole
# report (with the warnings from the swept workboxes) into WARN_FILE.
emit_sweep_report() {
  local report="$1" line
  [[ -s "$report" ]] || return 0
  while IFS= read -r line; do
    [[ "$line" != "Orphan sweep: "* ]] || echo "SWEEP ${line#Orphan sweep: }"
  done <"$report"
  [[ -z "$WARN_FILE" || "$report" == "$WARN_FILE" ]] || cat "$report" >>"$WARN_FILE"
}

# ---------------------------------------------------------------------------
# Flag parsing

declare -A FLAGS=()
parse_flags() {
  local allowed=" $1 " arg name value
  shift
  while (( $# > 0 )); do
    arg="$1"
    shift
    case "$arg" in
      -h|--help) usage; exit 0 ;;
      --*=*) name="${arg%%=*}"; name="${name#--}"; value="${arg#*=}" ;;
      --*)
        name="${arg#--}"
        if [[ "$allowed" == *" $name! "* ]]; then
          value=1
        else
          (( $# > 0 )) || usage_error "--$name needs a value"
          value="$1"
          shift
        fi
        ;;
      *) usage_error "unexpected argument: $arg" ;;
    esac
    [[ "$allowed" == *" $name "* || "$allowed" == *" $name! "* ]] \
      || usage_error "unknown flag: --$name"
    FLAGS[$name]="$value"
  done
}
flag() { echo "${FLAGS[$1]:-${2:-}}"; }

require_int() {
  [[ "$2" =~ ^[0-9]+$ ]] || usage_error "--$1 must be a non-negative integer"
}

need_run() {
  local id
  id="$(flag run-id)"
  [[ -n "$id" ]] || usage_error "--run-id is required"
  load_run "$id" || die "no run state for $id ($RUNS_DIR/$id.json)"
}

# ---------------------------------------------------------------------------
# prepare

cmd_prepare() {
  parse_flags "slots tree fx-set keep-boxes! option setup-ran!" "$@"
  local k tree fx_set keep option rc=0 status prefix
  k="$(flag slots)"
  [[ -n "$k" ]] || usage_error "--slots is required"
  require_int slots "$k"
  (( k > 0 )) || usage_error "--slots must be at least 1"
  tree="$(flag tree)"
  fx_set="$(flag fx-set)"
  keep=false
  [[ -z "$(flag keep-boxes)" ]] || keep=true
  option="$(flag option A)"
  [[ "$option" == A || "$option" == C ]] || usage_error "--option must be A or C"

  # The helper reports the directory that holds the box tools, and the tree.
  if [[ -n "$tree" ]]; then
    helper check --tree="$tree" || rc=$?
  else
    helper check || rc=$?
  fi
  status="$(hj '.status // ""')"
  [[ -n "$status" ]] || die "workbox helper check failed (exit $rc)"
  [[ "$status" == READY ]] \
    || die "Workbox is not ready (helper check status $status); run the Pre-Migration Checks first"
  prefix="$(hj '.path_prefix // ""')"
  tree="$(hj '.tree // ""')"
  [[ -n "$tree" ]] || die "the workbox helper reported no tree"
  [[ -z "$prefix" ]] || PATH="$prefix:$PATH"
  resolve_planter

  RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
  STATE="$RUNS_DIR/$RUN_ID.json"
  RUN_DIR="$LOG_ROOT/$RUN_ID"
  mkdir -p "$RUNS_DIR" "$RUN_DIR"
  WARN_FILE="$RUN_DIR/warnings.txt"
  LEAK_FILE="$RUN_DIR/leaked.txt"
  : >"$WARN_FILE"
  : >"$LEAK_FILE"
  local report="$RUN_DIR/sweep.txt"
  : >"$report"

  local setup_json=null
  if [[ -n "$(flag setup-ran)" ]]; then
    if jq -e 'type == "object"' "$SETUP_RECORD" >/dev/null 2>&1; then
      setup_json="$(jq -c '{path, real, sha256, snapshot_cl, last_change_cl,
        source, guide, exit, log}' "$SETUP_RECORD")"
    else
      warn "⚠️ --setup-ran was given, but there is no setup record"
    fi
  fi

  # Under the lock: sweep, capacity, box snapshot, selection, reservation and
  # the first state-file write.
  lock_acquire
  local saved_warn="$WARN_FILE"
  sweep_orphans "$report"
  WARN_FILE="$saved_warn"

  helper capacity --tree="$tree" || die "workbox helper capacity failed"
  local budgeted used max
  budgeted="$(hj '.budgeted // false')"
  used="$(hj '.used // 0')"
  max="$(hj '.max // 0')"
  helper boxes || die "workbox helper boxes failed"
  local boxes_json="$HELPER_JSON"
  wb_refresh || die "planter workbox list failed"
  fleet_refresh || true

  # Names held by other live runs, and names reserved by them that the
  # helper does not list yet (those count against the mount budget).
  declare -A live_name=() any_name=() helper_box=()
  local f n inflight=0
  local -a helper_names=()
  mapfile -t helper_names < <(jq -r '.boxes[]?.name' <<<"$boxes_json")
  for n in "${helper_names[@]}"; do helper_box[$n]=1; done
  for f in "$RUNS_DIR"/*.json; do
    [[ -f "$f" ]] || continue
    for n in $(jq -r '((.created // []) + (.reused // []))[]' "$f" 2>/dev/null); do
      any_name[$n]=1
    done
    is_live "$f" || continue
    for n in $(jq -r '((.created // []) + (.reused // []))[]' "$f" 2>/dev/null); do
      live_name[$n]=1
    done
    for n in $(jq -r '(.created // [])[]' "$f" 2>/dev/null); do
      [[ -n "${helper_box[$n]:-}" ]] || inflight=$(( inflight + 1 ))
    done
  done
  local free=-1
  if [[ "$budgeted" == true ]]; then
    free=$(( max - used - inflight ))
    (( free > 0 )) || free=0
    echo "CAPACITY budgeted=true used=$used max=$max inflight=$inflight free=$free"
  else
    echo "CAPACITY budgeted=false"
  fi

  # Reuse: no runner, idle, not draining or stopping, same tree, not held by
  # a live run.
  local -a reuse=() create=()
  local box btree
  for box in "${WB_NAMES[@]}"; do
    (( ${#reuse[@]} < k )) || break
    [[ "${WB_RUNNER[$box]}" == - && "${WB_TASK[$box]}" == idle ]] || continue
    [[ "${WB_STATE[$box]}" != draining && "${WB_STATE[$box]}" != stopping ]] || continue
    [[ -z "${live_name[$box]:-}" ]] || continue
    btree="$(jq -r --arg b "$box" '[.boxes[]? | select(.name == $b) | .tree][0] // ""' \
      <<<"$boxes_json")"
    [[ "$btree" == "$tree" ]] || continue
    reuse+=("$box")
  done

  # New boxes: e2e<n>, within the budget.
  local want=$(( k - ${#reuse[@]} ))
  if (( free >= 0 && want > free )); then
    want=$free
  fi
  local i=1 id taken
  while (( ${#create[@]} < want )); do
    box="e2e$i"
    i=$(( i + 1 ))
    (( ${#box} <= 12 )) || break
    taken=0
    if box_exists "$box" || [[ -n "${helper_box[$box]:-}" || -n "${any_name[$box]:-}" ]]; then
      taken=1
    fi
    for id in "${!FLEET_MEMBER[@]}"; do
      [[ "$id" != "$box" && "$id" != *"-$box" ]] || taken=1
    done
    (( taken )) || create+=("$box")
  done

  if (( ${#reuse[@]} + ${#create[@]} == 0 )); then
    lock_release
    WARN_FILE="$saved_warn"
    emit_sweep_report "$report"
    if [[ "$budgeted" == true ]]; then
      echo "STOP_REASON=No Workbox slot available: $(( used + inflight ))/$max mounts in use"
    else
      echo "STOP_REASON=No Workbox slot available"
    fi
    STATE=""
    exit 12
  fi

  jq -n --arg run_id "$RUN_ID" --arg host "$HOST" --arg tree "$tree" \
    --arg boot "$BOOT_ID" --argjson pid "$$" --arg pid_start "$(proc_start $$)" \
    --arg option "$option" --argjson keep "$keep" --arg run_dir "$RUN_DIR" \
    --arg prefix "$prefix" --arg fx_set "$fx_set" --argjson setup "$setup_json" \
    --argjson now "$(now)" --argjson lease "$LEASE_SECS" \
    --args '{
      run_id: $run_id, host: $host, tree: $tree, boot_id: $boot, pid: $pid,
      pid_start: $pid_start, phase: "preparing", option: $option,
      keep_boxes: $keep, run_dir: $run_dir, path_prefix: $prefix,
      fx_set: $fx_set, created: [], reused: [], task_ids: [], slots: {},
      assigned: {}, edit: {}, results: {}, setup: $setup,
      created_at: $now, updated_at: $now, lease_until: ($now + $lease)
    }' >"$STATE.tmp"
  mv -f "$STATE.tmp" "$STATE"
  local b
  for b in "${reuse[@]}"; do sset --arg b "$b" '.reused += [$b]'; done
  for b in "${create[@]}"; do sset --arg b "$b" '.created += [$b]'; done
  lock_release
  WARN_FILE="$saved_warn"
  emit_sweep_report "$report"

  # Outside the lock: create (or check) each box and take its slot ID from
  # the "[workbox:<box>] Ready (slot=<id>, ...)" line.
  local out slot
  local -a cmd
  for b in "${reuse[@]}" "${create[@]}"; do
    cmd=(workbox start "$b" --create-only --tree="$tree")
    [[ -z "$fx_set" ]] || cmd+=(--fx-set="$fx_set")
    slot=""
    if is_dry; then
      echo "DRY_RUN: planter $(quote_cmd "${cmd[@]}")"
      slot="$b"
    else
      rc=0
      out="$(pl "${cmd[@]}" 2>&1)" || rc=$?
      printf '%s\n' "$out" >"$RUN_DIR/$b.create.log"
      printf '%s\n' "$out" >&2
      slot="$(sed -n "s/^\[workbox:$b\] Ready (slot=\([^,)]*\).*/\1/p" <<<"$out" | tail -n 1)"
      if (( rc != 0 )) || [[ -z "$slot" ]]; then
        warn "⚠️ planter workbox start $b --create-only failed (exit $rc); see $RUN_DIR/$b.create.log"
        teardown_box "$b"
        continue
      fi
      fleet_refresh || true
      if [[ -z "${FLEET_MEMBER[$slot]:-}" ]]; then
        warn "⚠️ Slot $slot of workbox $b is not a member in planter fleets list; not using it"
        teardown_box "$b"
        continue
      fi
    fi
    sset --arg b "$b" --arg s "$slot" '.slots[$b] = $s'
    renew_lease
  done

  local boxes slots
  boxes="$(sget '[(.created + .reused)[] as $b | select(.slots[$b]) | $b] | join(",")')"
  slots="$(sget '[(.created + .reused)[] as $b | .slots[$b] // empty] | join(",")')"
  if [[ -z "$boxes" ]]; then
    teardown_run
    echo "STOP_REASON=No Workbox slot available: every workbox failed to start"
    exit 12
  fi
  set_phase prepared
  echo "RUN_ID=$RUN_ID"
  echo "BOXES=$boxes"
  echo "SLOTS=$slots"
  echo "TREE=$tree"
  echo "RUN_DIR=$RUN_DIR"
}

# ---------------------------------------------------------------------------
# record-task / touch

cmd_record_task() {
  parse_flags "run-id task-id" "$@"
  need_run
  local task
  task="$(flag task-id)"
  [[ -n "$task" ]] || usage_error "--task-id is required"
  claim_process soft
  sset --arg t "$task" \
    '.task_ids = ((.task_ids // []) | if index($t) then . else . + [$t] end)
     | if .phase == "prepared" then .phase = "queuing" else . end'
  renew_lease
  echo "TASK_RECORDED=$task"
}

cmd_touch() {
  parse_flags "run-id" "$@"
  need_run
  claim_process soft
  renew_lease
  echo "LEASE_UNTIL=$(sget '.lease_until')"
}

# ---------------------------------------------------------------------------
# upload (phase 1)

declare -A OUT=() CL=() BOX=() REASON=() LASTJ=() BOX_OF=()
declare -A NORUNNER=() RESTARTS=() LIVE_BOX=() KICKS=()
SLOT_RR=0

record_result() {
  local t="$1"
  OUT[$t]="$2"; CL[$t]="${3:--}"; BOX[$t]="${4:--}"; REASON[$t]="${5:-}"
  sset --arg t "$t" --arg o "$2" --arg c "${3:--}" --arg b "${4:--}" --arg r "${5:-}" \
    '.results[$t] = {outcome: $o, cl: $c, box: $b, reason: $r}'
}

# Prints the slots of the run's boxes whose runner is alive.
live_slots() {
  local box
  for box in "${!LIVE_BOX[@]}"; do
    sget --arg b "$box" '.slots[$b] // empty'
  done | sort
}

# Assigns task $1 round-robin to a live slot of the run. Returns 1 if none.
reassign_task() {
  local task="$1"
  local -a slots=()
  mapfile -t slots < <(live_slots)
  (( ${#slots[@]} > 0 )) || return 1
  assign_task "$task" "${slots[$(( SLOT_RR % ${#slots[@]} ))]}" || true
  SLOT_RR=$(( SLOT_RR + 1 ))
}

# Moves the run's unfinished tasks assigned to slot $1 to live run slots.
move_assignments() {
  local slot="$1" t
  for t in $(sget --arg s "$slot" \
      '(.assigned // {}) | to_entries[] | select(.value == $s) | .key'); do
    [[ -z "${OUT[$t]:-}" ]] || continue
    unassign_task "$t"
    reassign_task "$t" || true
  done
}

PHASE_FLAGS=()
FLEET_FLAGS=()
MODEL_FLAGS=()
# Sets MODEL_FLAGS from --model, --model-fallbacks and --no-speed-force.
# `planter workbox start` passes them on to the runner. --model and
# --model-fallbacks are passed only if given, so that Planter picks the models
# by default; an empty --model-fallbacks disables the fallbacks. Speed force is
# always passed, as --speed-force=false with --no-speed-force: without the
# flag, the runner would take it from Planter's own settings.
set_model_flags() {
  MODEL_FLAGS=()
  [[ -z "$(flag model)" ]] || MODEL_FLAGS+=(--model="$(flag model)")
  [[ -z "${FLAGS[model-fallbacks]+set}" ]] \
    || MODEL_FLAGS+=(--model-fallbacks="${FLAGS[model-fallbacks]}")
  if [[ -n "$(flag no-speed-force)" ]]; then
    MODEL_FLAGS+=(--speed-force=false)
  else
    MODEL_FLAGS+=(--speed-force)
  fi
}
start_runner() {
  local box="$1" ids="$2"
  pl_mut workbox start "$box" "${PHASE_FLAGS[@]}" "${MODEL_FLAGS[@]}" \
    --task="$ids" -- "${FLEET_FLAGS[@]}"
}

# Classifies a phase-1 inspect object: prints "<outcome>|<cl>|<reason>", or
# nothing while the task is not done.
classify1() {
  jq -r --arg id "$2" '
    def cl: if (.change_id | test("^[0-9]+$")) then .change_id else "-" end;
    if .state == "LANDED" then "LANDED|\(cl)|landed"
    elif .state == "AWAITING_HUMAN_REVIEW" then
      if cl == "-" then "UPLOADED|-|no CL number (change_id \(.change_id))"
      elif .awaiting_ack then "UPLOADED_WIP|\(cl)|awaiting planter ack \($id)"
      elif .wip then "UPLOADED_WIP|\(cl)|WIP until its dependencies land"
      else "UPLOADED|\(cl)|uploaded patchset \(.current_patchset)" end
    elif .state == "FAILED" then
      "FAILED|\(cl)|\(if .hint == "" then "failed" else .hint end)"
    elif .paused then "PAUSED|\(cl)|paused by someone else"
    else "" end' <<<"$1"
}

# Prints a box's Planter warnings into WARN_FILE.
poll_warnings() {
  local box="$1" out
  out="$(pl workbox logs "$box" -n 500 2>/dev/null)" || return 0
  extract_warnings <<<"$out"
}

cmd_upload() {
  parse_flags "run-id timeout-secs model model-fallbacks no-speed-force!" "$@"
  need_run
  set_model_flags
  local timeout
  timeout="$(flag timeout-secs "$UPLOAD_TIMEOUT")"
  require_int timeout-secs "$timeout"
  LEAK_FILE="$RUN_DIR/leaked.txt"
  : >"$LEAK_FILE"
  claim_process
  set_phase uploading
  renew_lease

  local -a tasks=() boxes=()
  mapfile -t tasks < <(sget '(.task_ids // [])[]')
  (( ${#tasks[@]} > 0 )) || die "run $RUN_ID has no recorded tasks (use record-task)"
  mapfile -t boxes < <(sget '[(.created + .reused)[] as $b | select(.slots[$b]) | $b][]')
  local ids box slot t j line outcome cl reason
  ids="$(IFS=,; echo "${tasks[*]}")"

  # Foreign runners: any runner that is not one of the run's boxes may claim
  # the run's tasks with its own settings, so pin them to the run's slots.
  local -a own_slots=()
  mapfile -t own_slots < <(sget '.slots[]')
  fleet_refresh || true
  local -a foreign=()
  local rid rbox rslots s own
  for line in "${FLEET_RUNNERS[@]}"; do
    IFS='|' read -r rid rbox rslots <<<"$line"
    own=0
    for box in "${boxes[@]}"; do [[ "$rbox" != "$box" ]] || own=1; done
    if (( ! own )) && [[ -n "$rslots" ]]; then
      own=1
      for s in ${rslots//,/ }; do
        [[ " ${own_slots[*]} " == *" $s "* ]] || own=0
      done
    fi
    (( own )) || foreign+=("$rid")
  done
  for box in "${boxes[@]}"; do LIVE_BOX[$box]=1; done
  if (( ${#foreign[@]} > 0 )); then
    warn "⚠️ Other Planter runners are running (${foreign[*]}); assigning this run's tasks to its own workbox slots"
    for t in "${tasks[@]}"; do
      [[ -n "$(sget --arg t "$t" '.assigned[$t] // ""')" ]] || reassign_task "$t" || true
    done
  fi
  LIVE_BOX=()

  # Start one runner per box.
  PHASE_FLAGS=(--gerrit --cq=false --no-new=false --simulate-review=false
               --keep-head=false --until-uploaded)
  FLEET_FLAGS=(--assign-reviewer=false)
  local -a failed=()
  for box in "${boxes[@]}"; do
    start_runner "$box" "$ids" || true
    if wait_runner "$box"; then
      LIVE_BOX[$box]=1
    else
      pl workbox logs "$box" -n 0 >"$RUN_DIR/$box.start_failed.log" 2>&1 || true
      warn "⚠️ The runner in workbox $box did not start (START_FAILED); see $RUN_DIR/$box.start_failed.log"
      failed+=("$box")
    fi
    renew_lease
  done
  for box in "${failed[@]}"; do
    slot="$(sget --arg b "$box" '.slots[$b] // ""')"
    [[ -z "$slot" ]] || move_assignments "$slot"
    teardown_box "$box"
  done

  if (( ${#LIVE_BOX[@]} == 0 )); then
    for t in "${tasks[@]}"; do
      record_result "$t" NOT_STARTED - - "every workbox runner failed to start"
    done
    print_results "${tasks[@]}"
    teardown_run
    finish_upload "${tasks[@]}"
    echo "RESULT_JSON=$RUN_DIR/result.json"
    echo "STOP_REASON=Every workbox runner failed to start"
    exit 12
  fi

  # Poll the ledger.
  local deadline=$(( $(now) + timeout )) all_done any_runner active_in_box restarted
  while :; do
    renew_lease
    wb_refresh || true
    for box in "${boxes[@]}"; do
      t="${WB_TASK[$box]:-}"
      [[ -z "$t" || "$t" == idle ]] || BOX_OF[$t]="$box"
    done
    all_done=1
    for t in "${tasks[@]}"; do
      [[ -z "${OUT[$t]:-}" ]] || continue
      j="$(inspect_task "$t")" || { all_done=0; continue; }
      LASTJ[$t]="$j"
      if [[ -z "${BOX_OF[$t]:-}" ]]; then
        for box in "${boxes[@]}"; do
          [[ -z "${WB_TREE[$box]:-}" || "$(jq -r '.workdir' <<<"$j")" != "${WB_TREE[$box]}" ]] \
            || BOX_OF[$t]="$box"
        done
      fi
      line="$(classify1 "$j" "$t")"
      if [[ -n "$line" ]]; then
        IFS='|' read -r outcome cl reason <<<"$line"
        record_result "$t" "$outcome" "$cl" "${BOX_OF[$t]:--}" "$reason"
      else
        all_done=0
      fi
    done
    for box in "${boxes[@]}"; do poll_warnings "$box"; done
    (( ! all_done )) || break

    # Liveness: a box that holds an ACTIVE run task and has had no runner for
    # 2 polls is restarted once; if it dies again, its tasks are paused.
    restarted=0
    for box in "${!LIVE_BOX[@]}"; do
      local has_active=0
      for t in "${tasks[@]}"; do
        [[ -z "${OUT[$t]:-}" && "${BOX_OF[$t]:-}" == "$box" ]] || continue
        [[ "$(jq -r '.state' <<<"${LASTJ[$t]:-$EMPTY}")" == ACTIVE ]] && has_active=1
      done
      if (( has_active )) && [[ "$(box_runner "$box")" == - ]]; then
        NORUNNER[$box]=$(( ${NORUNNER[$box]:-0} + 1 ))
      else
        NORUNNER[$box]=0
      fi
      (( ${NORUNNER[$box]} >= 2 )) || continue
      NORUNNER[$box]=0
      if (( ${RESTARTS[$box]:-0} == 0 )); then
        RESTARTS[$box]=1
        warn "⚠️ The runner in workbox $box died; restarting it once"
        start_runner "$box" "$ids" || true
        restarted=1
        continue
      fi
      pl workbox logs "$box" -n 0 >"$RUN_DIR/$box.crash.log" 2>&1 || true
      warn "⚠️ The runner in workbox $box died twice; its tasks are paused (see $RUN_DIR/$box.crash.log)"
      unset 'LIVE_BOX[$box]'
      for t in "${tasks[@]}"; do
        [[ -z "${OUT[$t]:-}" && "${BOX_OF[$t]:-}" == "$box" ]] || continue
        [[ "$(jq -r '.state' <<<"${LASTJ[$t]}")" == ACTIVE ]] || continue
        pause_task "$t"
        record_result "$t" STRANDED "$(task_cl "${LASTJ[$t]}")" "$box" \
          "the workbox runner died twice; task paused"
      done
      slot="$(sget --arg b "$box" '.slots[$b] // ""')"
      [[ -z "$slot" ]] || move_assignments "$slot"
    done

    # A live box whose runner already exited (--until-uploaded) gets a new
    # runner when queued tasks were reassigned to its slot.
    for box in "${!LIVE_BOX[@]}"; do
      [[ "$(box_runner "$box")" == - && ${KICKS[$box]:-0} -lt 3 ]] || continue
      slot="$(sget --arg b "$box" '.slots[$b] // ""')"
      local waiting=0 busy=0
      for t in "${tasks[@]}"; do
        [[ -z "${OUT[$t]:-}" ]] || continue
        # A box that holds an ACTIVE task is handled by the liveness check.
        if [[ "${BOX_OF[$t]:-}" == "$box" &&
              "$(jq -r '.state' <<<"${LASTJ[$t]:-$EMPTY}")" == ACTIVE ]]; then
          busy=1
        fi
        [[ "$(sget --arg t "$t" '.assigned[$t] // ""')" == "$slot" ]] || continue
        [[ "$(jq -r '.state' <<<"${LASTJ[$t]:-$EMPTY}")" != QUEUED ]] || waiting=1
      done
      (( waiting && ! busy )) || continue
      KICKS[$box]=$(( ${KICKS[$box]:-0} + 1 ))
      log "Starting a runner in workbox $box for the tasks reassigned to it"
      start_runner "$box" "$ids" || true
      restarted=1
    done

    if is_dry; then
      break
    fi
    if (( ! restarted )); then
      any_runner=0
      active_in_box=0
      for box in "${boxes[@]}"; do
        [[ "$(box_runner "$box")" == - ]] || any_runner=1
      done
      for t in "${tasks[@]}"; do
        [[ -z "${OUT[$t]:-}" && -n "${BOX_OF[$t]:-}" ]] || continue
        [[ "$(jq -r '.state' <<<"${LASTJ[$t]:-$EMPTY}")" != ACTIVE ]] || active_in_box=1
      done
      (( any_runner || active_in_box )) || break
    fi
    if (( $(now) >= deadline )); then
      warn "⚠️ upload timed out after ${timeout}s; stopping the run's workboxes"
      for box in "${boxes[@]}"; do
        [[ "$(box_runner "$box")" == - ]] || stop_box "$box"
      done
      for t in "${tasks[@]}"; do
        [[ -z "${OUT[$t]:-}" ]] || continue
        record_result "$t" TIMEOUT "$(task_cl "${LASTJ[$t]:-$EMPTY}")" "${BOX_OF[$t]:--}" \
          "not done after ${timeout}s"
      done
      break
    fi
    sleep "$POLL_SECS"
  done

  # Tasks that are not done when every runner is gone.
  local st lw where
  for t in "${tasks[@]}"; do
    [[ -z "${OUT[$t]:-}" ]] || continue
    j="${LASTJ[$t]:-}"
    if [[ -z "$j" ]]; then
      record_result "$t" NOT_FINISHED - - "planter inspect failed"
      continue
    fi
    st="$(jq -r '.state' <<<"$j")"
    lw="$(jq -r '.local_work' <<<"$j")"
    where=""
    [[ -n "${BOX_OF[$t]:-}" ]] \
      || where=" (claimed outside this run's workboxes: $(jq -r '.assigned_slot // .workdir' <<<"$j"))"
    if [[ "$st" == QUEUED ]]; then
      record_result "$t" NOT_STARTED - - "no Workbox slot ran it"
    elif [[ "$st" == ACTIVE && "$lw" == true ]]; then
      record_result "$t" STRANDED "$(task_cl "$j")" "${BOX_OF[$t]:--}" \
        "no runner is left for its local work$where"
    else
      record_result "$t" NOT_FINISHED "$(task_cl "$j")" "${BOX_OF[$t]:--}" \
        "state $st/$(jq -r '.phase' <<<"$j") when every runner was gone$where"
    fi
  done

  print_results "${tasks[@]}"

  # Teardown: option C keeps the boxes of UPLOADED tasks for review.
  local -a kept=()
  local option
  option="$(sget '.option // "A"')"
  if [[ "$option" == C ]]; then
    for t in "${tasks[@]}"; do
      [[ "${OUT[$t]}" == UPLOADED && "${BOX[$t]}" != - && "${CL[$t]}" != - ]] || continue
      [[ " ${kept[*]} " == *" ${BOX[$t]} "* ]] || kept+=("${BOX[$t]}")
    done
    KEEP_STATE=1
    teardown_run "${kept[@]}"
    KEEP_STATE=0
    set_phase awaiting_review
  else
    teardown_run
  fi
  finish_upload "${tasks[@]}"
  if [[ "$option" == C ]]; then
    if (( ${#kept[@]} > 0 )); then
      echo "KEPT_BOXES=$(IFS=,; echo "${kept[*]}")"
    else
      echo "KEPT_BOXES=none"
    fi
  fi
  local ok=0 total=${#tasks[@]}
  for t in "${tasks[@]}"; do
    case "${OUT[$t]}" in
      UPLOADED|UPLOADED_WIP|LANDED) [[ "${CL[$t]}" == - ]] || ok=$(( ok + 1 )) ;;
    esac
  done
  echo "RESULT_JSON=$RUN_DIR/result.json"
  if (( ok == total )); then
    exit 0
  elif (( ok == 0 )); then
    exit 11
  fi
  exit 10
}

print_results() {
  local t
  for t in "$@"; do
    echo "RESULT $t ${OUT[$t]} ${CL[$t]} ${BOX[$t]} ${REASON[$t]}"
  done
}

# Prints the warnings block and BOXES_LEAKED, and writes result.json.
finish_upload() {
  local t leaked
  leaked="$(leaked_list)"
  print_warnings
  echo "BOXES_LEAKED=$leaked"
  {
    for t in "$@"; do
      jq -n --arg t "$t" --arg o "${OUT[$t]}" --arg c "${CL[$t]}" --arg b "${BOX[$t]}" \
        --arg r "${REASON[$t]}" '{task: $t, outcome: $o, cl: $c, box: $b, reason: $r}'
    done
  } | jq -s --arg run "$RUN_ID" --arg leaked "$leaked" --rawfile w "$WARN_FILE" \
    '{run_id: $run, results: ., warnings: ($w | split("\n") | map(select(. != ""))),
      boxes_leaked: (if $leaked == "none" then [] else ($leaked | split(",")) end)}' \
    >"$RUN_DIR/result.json"
}

# ---------------------------------------------------------------------------
# review (option C phase 2)

declare -A ROUT=() RDETAIL=() RCL=() RBOX=()

record_review() {
  local t="$1"
  ROUT[$t]="$2"; RDETAIL[$t]="$3"
  sset --arg t "$t" --arg o "$2" --arg d "$3" '.review[$t] = {outcome: $o, detail: $d}'
}

cmd_review() {
  parse_flags "run-id cq-timeout-secs model model-fallbacks no-speed-force!" "$@"
  need_run
  set_model_flags
  local timeout
  timeout="$(flag cq-timeout-secs "$CQ_TIMEOUT")"
  require_int cq-timeout-secs "$timeout"
  LEAK_FILE="$RUN_DIR/leaked.txt"
  : >"$LEAK_FILE"
  local phase
  phase="$(sget '.phase')"
  [[ "$phase" == awaiting_review ]] \
    || die "run $RUN_ID is in phase $phase, not awaiting_review (run upload with --option=C first)"
  claim_process
  set_phase reviewing
  renew_lease

  # Scope: UPLOADED tasks that are not WIP.
  local -a tasks=() scope=() boxes=()
  local t j box outcome
  mapfile -t tasks < <(sget '(.results // {}) | to_entries[]
    | select(.value.outcome == "UPLOADED" or .value.outcome == "UPLOADED_WIP") | .key')
  for t in "${tasks[@]}"; do
    RCL[$t]="$(sget --arg t "$t" '.results[$t].cl')"
    RBOX[$t]="$(sget --arg t "$t" '.results[$t].box')"
    outcome="$(sget --arg t "$t" '.results[$t].outcome')"
    j="$(inspect_task "$t" || echo '{}')"
    if [[ "$outcome" == UPLOADED_WIP || "$(jq -r '.awaiting_ack // false' <<<"$j")" == true ]]; then
      record_review "$t" REVIEW_SKIPPED_WIP "awaiting planter ack $t"
    elif [[ "$(jq -r '.wip // false' <<<"$j")" == true ]]; then
      record_review "$t" REVIEW_SKIPPED_WIP "WIP until its dependencies land"
    elif [[ -z "$(sget --arg b "${RBOX[$t]}" '.slots[$b] // ""')" ]]; then
      record_review "$t" REVIEW_TIMEOUT "no workbox was kept for phase 2"
    else
      scope+=("$t")
      [[ " ${boxes[*]} " == *" ${RBOX[$t]} "* ]] || boxes+=("${RBOX[$t]}")
    fi
  done

  # Restart one runner per kept box, scoped to its uploaded tasks.
  PHASE_FLAGS=(--gerrit --cq=true --no-new=true --simulate-review=false --keep-head=false)
  FLEET_FLAGS=(--assign-reviewer)
  declare -A box_ids=() deaths=() norunner=()
  for box in "${boxes[@]}"; do
    box_ids[$box]="$(for t in "${scope[@]}"; do [[ "${RBOX[$t]}" != "$box" ]] || echo "$t"; done \
      | paste -sd, -)"
    deaths[$box]=0
    norunner[$box]=0
    start_runner "$box" "${box_ids[$box]}" || true
    if ! wait_runner "$box"; then
      deaths[$box]=1
      warn "⚠️ The phase-2 runner in workbox $box did not start; trying once more"
      start_runner "$box" "${box_ids[$box]}" || true
      if ! wait_runner "$box"; then
        deaths[$box]=2
        strand_review_box "$box"
      fi
    fi
    renew_lease
  done

  local deadline=$(( $(now) + timeout )) pending
  while :; do
    renew_lease
    wb_refresh || true
    pending=0
    for t in "${scope[@]}"; do
      [[ -z "${ROUT[$t]:-}" ]] || continue
      j="$(inspect_task "$t")" || { pending=1; continue; }
      LASTJ[$t]="$j"
      if [[ -n "$(jq -r '.reviewer' <<<"$j")" ]]; then
        record_review "$t" REVIEWER_ASSIGNED "$(jq -r '.reviewer' <<<"$j")"
      elif [[ "$(jq -r '.state' <<<"$j")" == FAILED ]]; then
        record_review "$t" REVIEW_CQ_FAILED "$(jq -r 'if .hint == "" then "CQ failed" else .hint end' <<<"$j")"
      else
        pending=1
      fi
    done
    for box in "${boxes[@]}"; do poll_warnings "$box"; done
    (( pending )) || break
    is_dry && break

    for box in "${boxes[@]}"; do
      (( ${deaths[$box]} < 2 )) || continue
      local open=0
      for t in "${scope[@]}"; do
        [[ -z "${ROUT[$t]:-}" && "${RBOX[$t]}" == "$box" ]] && open=1
      done
      if (( open )) && [[ "$(box_runner "$box")" == - ]]; then
        norunner[$box]=$(( ${norunner[$box]} + 1 ))
      else
        norunner[$box]=0
      fi
      (( ${norunner[$box]} >= 2 )) || continue
      norunner[$box]=0
      deaths[$box]=$(( ${deaths[$box]} + 1 ))
      if (( ${deaths[$box]} == 1 )); then
        warn "⚠️ The phase-2 runner in workbox $box died; restarting it once"
        start_runner "$box" "${box_ids[$box]}" || true
      else
        pl workbox logs "$box" -n 0 >"$RUN_DIR/$box.review_crash.log" 2>&1 || true
        strand_review_box "$box"
      fi
    done

    if (( $(now) >= deadline )); then
      for t in "${scope[@]}"; do
        [[ -z "${ROUT[$t]:-}" ]] || continue
        j="${LASTJ[$t]:-$EMPTY}"
        if jq -e '.cq_patchset > 0 and .cq_patchset >= .current_patchset and .reviewer == ""' \
            >/dev/null 2>&1 <<<"$j"; then
          record_review "$t" REVIEW_CQ_PASSED_NOT_ASSIGNED \
            "CQ passed on patchset $(jq -r '.cq_patchset' <<<"$j"); no reviewer assigned within ${timeout}s"
        else
          record_review "$t" REVIEW_TIMEOUT "no CQ result and no reviewer within ${timeout}s"
        fi
      done
      break
    fi
    sleep "$REVIEW_POLL_SECS"
  done
  if is_dry; then
    for t in "${scope[@]}"; do
      [[ -n "${ROUT[$t]:-}" ]] || record_review "$t" REVIEW_TIMEOUT "DRY_RUN: not polled"
    done
  fi

  # Stop every runner before the teardown starts.
  for box in "${boxes[@]}"; do
    wb_refresh || true
    [[ "$(box_runner "$box")" == - ]] || stop_box "$box"
  done

  teardown_run

  for t in "${tasks[@]}"; do
    echo "REVIEW $t ${ROUT[$t]} ${RCL[$t]} ${RDETAIL[$t]}"
  done
  local leaked
  leaked="$(leaked_list)"
  print_warnings
  echo "BOXES_LEAKED=$leaked"
  {
    for t in "${tasks[@]}"; do
      jq -n --arg t "$t" --arg o "${ROUT[$t]}" --arg c "${RCL[$t]}" --arg d "${RDETAIL[$t]}" \
        '{task: $t, outcome: $o, cl: $c, detail: $d}'
    done
  } | jq -s --arg run "$RUN_ID" --arg leaked "$leaked" \
    '{run_id: $run, reviews: .,
      boxes_leaked: (if $leaked == "none" then [] else ($leaked | split(",")) end)}' \
    >"$RUN_DIR/review.json"
  echo "REVIEW_JSON=$RUN_DIR/review.json"
}

# The phase-2 runner of box $1 died twice: pause its open tasks and keep their
# CLs (never unpin a task that has a CL).
strand_review_box() {
  local box="$1" t
  warn "⚠️ The phase-2 runner in workbox $box died twice; its tasks are paused"
  for t in "${scope[@]}"; do
    [[ -z "${ROUT[$t]:-}" && "${RBOX[$t]}" == "$box" ]] || continue
    pause_task "$t"
    record_review "$t" REVIEW_STRANDED \
      "runner crashed; task paused: planter resume $t to continue"
  done
}

# ---------------------------------------------------------------------------
# teardown / sweep

cmd_teardown() {
  parse_flags "run-id" "$@"
  local id
  id="$(flag run-id)"
  [[ -n "$id" ]] || usage_error "--run-id is required"
  if ! load_run "$id"; then
    echo "No run state for $id; nothing to tear down." >&2
    print_warnings
    echo "BOXES_LEAKED=none"
    return 0
  fi
  LEAK_FILE="$RUN_DIR/leaked.txt"
  : >"$LEAK_FILE"
  teardown_run
  print_warnings
  echo "BOXES_LEAKED=$(leaked_list)"
}

cmd_sweep() {
  parse_flags "" "$@"
  resolve_planter
  local tmp
  tmp="$(mktemp -d)"
  LEAK_FILE="$tmp/leaked.txt"
  WARN_FILE="$tmp/warnings.txt"
  : >"$LEAK_FILE"
  : >"$WARN_FILE"
  lock_acquire
  sweep_orphans "$WARN_FILE"
  lock_release
  emit_sweep_report "$WARN_FILE"
  print_warnings
  echo "BOXES_LEAKED=$(leaked_list)"
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------

main() {
  (( $# > 0 )) || { usage >&2; exit 2; }
  local sub="$1"
  shift
  mkdir -p "$RUNS_DIR"
  trap on_exit EXIT
  case "$sub" in
    prepare) cmd_prepare "$@" ;;
    record-task) cmd_record_task "$@" ;;
    touch) cmd_touch "$@" ;;
    upload) cmd_upload "$@" ;;
    review) cmd_review "$@" ;;
    teardown) cmd_teardown "$@" ;;
    sweep) cmd_sweep "$@" ;;
    -h|--help|help) usage ;;
    *) usage_error "unknown subcommand: $sub" ;;
  esac
}

main "$@"
