#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Workbox readiness, credential checks, consent and setup for the
# e2e-bazel-migration skill (workbox execution mode).
#
# Every check is done by the workbox helper (the command in the "Command"
# section of the helper document next to the Planter installation guide, or
# the command the user gave in the prompt), passed in E2E_WB_HELPER. This
# script runs it as
# `bash -c "$E2E_WB_HELPER <subcommand> ..."`, reads the JSON object on the
# last line of its stdout, refuses any helper API other than 1, and prints
# KEY=value lines that the skill reads.
#
# The only thing this script runs itself is the workbox setup script
# (`workbox-setup.sh`), and only with `setup --consented`, which the agent
# passes only after the user explicitly answered Proceed to the consent text
# that `consent-text` printed. The script path is never written in this
# file: the agent derives it at runtime (from the directory that holds the
# Planter installation guide, or from the prompt) and passes it to
# `consent-text`, which records the exact file, its sha256 and where it came
# from. `setup` runs exactly that file and re-checks its sha256 first.
#
# Exit status (shared with the helper):
#   0   OK / READY
#   1   internal error (including a missing helper, a helper that prints no
#       JSON, or an incompatible helper API)
#   2   usage error
#   3   NEEDS_SETUP (check)
#   4   BLOCKED (check)
#   5   IN_WORKBOX (check)
#   6   CREDENTIALS (creds)
#   7   CONSENT_REQUIRED (setup)
#   8   SETUP_FAILED (setup; SETUP_STATUS=CHANGED, BACKUP_FAILED or FAILED)
#   11  SETUP_SCRIPT_MISSING (consent-text)

set -euo pipefail

usage() {
  cat <<EOF
Usage: $(basename "$0") <subcommand> [options]

Subcommands:
  version
      Checks that the workbox helper starts and speaks API 1.
      Prints HELPER_STATUS=OK|START_FAILED|INCOMPATIBLE, HELPER_API=,
      HELPER_CL=.
  check [--tree=<T>]
      Read-only readiness checks. Prints one
      "CHECK <id> PASS|FAIL|SKIP <detail>" line per check, then STATUS=,
      TREE=, BUDGETED=, PATH_PREFIX=, FAILED=<ids>, one "BLOCKED <reason>"
      line per blocked reason, and SANDBOX=BLOCKED if commands cannot run
      outside the terminal sandbox. Exit 0 (READY), 3 (NEEDS_SETUP),
      4 (BLOCKED) or 5 (IN_WORKBOX).
  creds --tree=<T> [--setup] [--need-sheets]
      Credential checks. Never runs a login. Prints
      "CRED_FAIL <id> <message>" and "USER_COMMAND <id> <command>" per
      failure, then CREDS=OK|FAILED. Exit 0 or 6.
  consent-text --setup=<path> --source=<guide|prompt-guide|prompt-path>
               [--guide=<location>] [--fleet-check=<match|absent>]
               --tree=<T> --failed=<ids>
      Checks that <path> is a readable regular file (else exit 11), asks
      the helper for the consent text, writes the consent record and prints
      SETUP_PATH=, SETUP_REAL=, SETUP_SHA256=, SETUP_SNAPSHOT_CL=,
      SETUP_LAST_CHANGE_CL=, MANUAL_COMMAND=, CONSENT_RECORD= and the text
      between "$BEGIN_TEXT" and "$END_TEXT".
      --guide and --fleet-check are required unless --source=prompt-path.
  setup --consented --tree=<T> [--setup=<path>]
      Runs the setup script recorded by consent-text, once, with
      --no-start. Refuses (exit 7) without --consented, without a consent
      record, with a record older than 1 hour, for another tree, or when
      --setup differs from the recorded path. Exit 8 when the script
      changed after consent (SETUP_STATUS=CHANGED), the agent configuration
      backup failed (BACKUP_FAILED), or the script or the post-setup check
      failed (FAILED). Long-running: run it outside the terminal sandbox,
      in the background.
  backup-config
      Backs up the global agent configuration (setup does this itself).
      Prints BACKUP=<path|->.

Every STOP also prints STOP_REASON=<text>.

Environment:
  E2E_WB_HELPER   The workbox helper command (required).
  DRY_RUN         If set, setup validates the consent record and prints the
                  commands it would run instead of running them.
EOF
}

BEGIN_TEXT="--- BEGIN CONSENT TEXT ---"
END_TEXT="--- END CONSENT TEXT ---"
STATE_DIR="$HOME/.planter/e2e-workbox/setup"
CONSENT_RECORD="$STATE_DIR/consent.json"
SETUP_RECORD="$STATE_DIR/last.json"
CONSENT_MAX_AGE_SECS=3600
# The setup script's die() prefix.
DIE_PREFIX="workbox-setup.sh: "

die() { echo "error: $*" >&2; exit 1; }
usage_error() { usage >&2; echo "error: $*" >&2; exit 2; }
stop() {  # <exit code> <reason>
  echo "STOP_REASON=$2"
  exit "$1"
}
now_iso() { date -u +%Y-%m-%dT%H:%M:%SZ; }
# Prints the value of a JSON field of $HJSON, "-" for null or missing.
hj() { jq -r "($1) // \"-\" | tostring" <<<"$HJSON"; }

command -v jq >/dev/null || die "jq not found"

# Runs the helper subcommand "$@". Sets HJSON (the JSON object on the last
# line of stdout) and HRC (the helper's exit code). Returns 0 if the helper
# printed a JSON object with api 1, 1 if it printed no JSON object (or
# E2E_WB_HELPER is unset), and 3 for an incompatible API (HAPI is set).
HJSON=""
HRC=0
HAPI=""
helper_call() {
  HJSON=""
  HRC=0
  HAPI=""
  if [[ -z "${E2E_WB_HELPER:-}" ]]; then
    echo "error: E2E_WB_HELPER is not set; use the workbox helper command" >&2
    return 1
  fi
  local cmd out
  cmd="$E2E_WB_HELPER$(printf ' %q' "$@")"
  out="$(bash -c "$cmd" </dev/null)" || HRC=$?
  HJSON="$(printf '%s\n' "$out" | tail -n 1)"
  if ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$HJSON"; then
    echo "error: the workbox helper printed no JSON result for '$1'" \
         "(exit $HRC)" >&2
    HJSON=""
    return 1
  fi
  HAPI="$(jq -r '.api // "missing" | tostring' <<<"$HJSON")"
  if [[ "$HAPI" != 1 ]]; then
    echo "error: the workbox helper is incompatible (api $HAPI)" >&2
    return 3
  fi
  return 0
}

# Calls helper_call and exits 1 on any failure.
helper_or_die() {
  local rc=0
  helper_call "$@" || rc=$?
  case "$rc" in
    0) ;;
    3) stop 1 "The workbox helper is incompatible (api $HAPI)" ;;
    *) stop 1 "The workbox helper failed to start" ;;
  esac
}

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

# Writes JSON from stdin to "$1" atomically.
write_json() {
  local tmp
  mkdir -p "$(dirname "$1")"
  chmod 700 "$(dirname "$1")" 2>/dev/null || true
  tmp="$(mktemp "$1.XXXXXX")"
  cat >"$tmp"
  mv -f "$tmp" "$1"
}

# Prints the check lines and returns the exit code for the check status in
# $HJSON.
print_check() {
  jq -r '.checks[]? |
    "CHECK \(.id) \(.result) \((.detail // "") | tostring | gsub("\n"; " "))"' \
    <<<"$HJSON"
  local status
  status="$(hj .status)"
  echo "STATUS=$status"
  echo "TREE=$(hj .tree)"
  echo "BUDGETED=$(hj .budgeted)"
  echo "PATH_PREFIX=$(hj .path_prefix)"
  echo "FAILED=$(jq -r '[.checks[]? | select(.result == "FAIL") | .id] | join(",")' \
    <<<"$HJSON")"
  jq -r '.blocked_reasons[]? | "BLOCKED \(tostring | gsub("\n"; " "))"' \
    <<<"$HJSON"
  if jq -e 'any(.checks[]?; .id == "R8" and .result == "FAIL")' \
       >/dev/null <<<"$HJSON"; then
    echo "SANDBOX=BLOCKED"
  fi
  case "$status" in
    READY) return 0 ;;
    NEEDS_SETUP) return 3 ;;
    BLOCKED) return 4 ;;
    IN_WORKBOX) return 5 ;;
    *) echo "error: unknown check status '$status'" >&2; return 1 ;;
  esac
}

cmd_version() {
  (( $# == 0 )) || usage_error "version takes no arguments"
  local rc=0
  helper_call version || rc=$?
  if (( rc == 3 )); then
    echo "HELPER_STATUS=INCOMPATIBLE"
    echo "HELPER_API=$HAPI"
    stop 1 "The workbox helper is incompatible (api $HAPI)"
  fi
  if (( rc != 0 || HRC != 0 )) \
       || ! jq -e '.ok == true' >/dev/null <<<"$HJSON"; then
    echo "HELPER_STATUS=START_FAILED"
    stop 1 "The workbox helper failed to start"
  fi
  echo "HELPER_STATUS=OK"
  echo "HELPER_API=$HAPI"
  echo "HELPER_CL=$(hj .helper_cl)"
}

cmd_check() {
  local tree=""
  while (( $# > 0 )); do
    case "$1" in
      --tree=*) tree="${1#*=}" ;;
      *) usage_error "check: unknown argument: $1" ;;
    esac
    shift
  done
  local args=()
  [[ -z "$tree" ]] || args+=(--tree="$tree")
  helper_or_die check "${args[@]}"
  local rc=0
  print_check || rc=$?
  case "$rc" in
    4) echo "STOP_REASON=Workbox is blocked on this machine" ;;
  esac
  return "$rc"
}

cmd_creds() {
  local tree="" args=()
  while (( $# > 0 )); do
    case "$1" in
      --tree=*) tree="${1#*=}" ;;
      --setup|--need-sheets) args+=("$1") ;;
      *) usage_error "creds: unknown argument: $1" ;;
    esac
    shift
  done
  [[ -n "$tree" ]] || usage_error "creds: --tree is required"
  helper_or_die creds --tree="$tree" "${args[@]}"
  jq -r '.failures[]? |
    "CRED_FAIL \(.id) \((.message // "") | tostring | gsub("\n"; " "))",
    "USER_COMMAND \(.id) \((.user_command // "") | tostring)"' <<<"$HJSON"
  if jq -e '(.failures // []) | length > 0' >/dev/null <<<"$HJSON"; then
    echo "CREDS=FAILED"
    stop 6 "Credentials are missing; run the USER_COMMAND lines in your own terminal, then run the skill again"
  fi
  jq -e '.ok == true' >/dev/null <<<"$HJSON" \
    || stop 1 "The workbox helper credential check failed without a reason"
  echo "CREDS=OK"
}

cmd_consent_text() {
  local setup="" source="" guide="" fleet="" tree="" failed="" have_failed=""
  while (( $# > 0 )); do
    case "$1" in
      --setup=*) setup="${1#*=}" ;;
      --source=*) source="${1#*=}" ;;
      --guide=*) guide="${1#*=}" ;;
      --fleet-check=*) fleet="${1#*=}" ;;
      --tree=*) tree="${1#*=}" ;;
      --failed=*) failed="${1#*=}"; have_failed=1 ;;
      *) usage_error "consent-text: unknown argument: $1" ;;
    esac
    shift
  done
  [[ -n "$setup" ]] || usage_error "consent-text: --setup is required"
  [[ -n "$tree" ]] || usage_error "consent-text: --tree is required"
  [[ -n "$have_failed" ]] || usage_error "consent-text: --failed is required"
  case "$source" in
    guide|prompt-guide)
      [[ -n "$guide" ]] \
        || usage_error "consent-text: --guide is required for --source=$source"
      case "$fleet" in
        match|absent) ;;
        *) usage_error "consent-text: --fleet-check must be match or absent" ;;
      esac
      ;;
    prompt-path)
      # No guide is read and there is no fleet guide cross-check.
      fleet="skipped"
      ;;
    *) usage_error "consent-text: --source must be guide, prompt-guide or prompt-path" ;;
  esac

  # A path from the prompt may start with "~/" or be relative.
  [[ "$setup" != "~/"* ]] || setup="$HOME/${setup#\~/}"
  [[ "$setup" == /* ]] || setup="$PWD/$setup"
  if [[ ! -f "$setup" || ! -r "$setup" ]]; then
    echo "SETUP_MISSING=$setup"
    stop 11 "The workbox setup script \`$setup\` does not exist or cannot be read"
  fi

  local args=(--tree="$tree" --failed="$failed" --setup="$setup"
              --source="$source")
  [[ -z "$guide" ]] || args+=(--guide="$guide")
  [[ "$fleet" == skipped ]] || args+=(--fleet-check="$fleet")
  helper_or_die consent-text "${args[@]}"
  if [[ "$HRC" == 11 || "$(hj .status)" == SETUP_SCRIPT_MISSING ]]; then
    echo "SETUP_MISSING=$setup"
    stop 11 "The workbox setup script \`$setup\` does not exist or cannot be read"
  fi
  (( HRC == 0 )) && jq -e '.ok == true' >/dev/null <<<"$HJSON" \
    || stop 1 "The workbox helper failed to build the consent text"

  # The helper resolves the path (an immutable snapshot when the path goes
  # through the read-only head source tree) and hashes it. Check both, so
  # that setup runs exactly the file whose hash the user saw. The head tree
  # moves on to a new snapshot every few seconds, so resolving the path again
  # here can give another snapshot of the same file: compare the content.
  local real sha
  real="$(hj .setup_real)"
  [[ -f "$real" && -r "$real" ]] \
    || stop 1 "The workbox helper resolved the setup script to an unreadable file: $real"
  sha="$(sha256_of "$real")"
  [[ "$sha" == "$(sha256_of "$setup")" ]] \
    || stop 1 "The workbox helper resolved \`$setup\` to a different file: $real"
  [[ "$sha" == "$(hj .setup_sha256)" ]] \
    || stop 1 "The workbox helper reported a different sha256 for \`$real\`"

  local now_epoch
  now_epoch="$(date +%s)"
  jq -n \
    --argjson h "$HJSON" \
    --arg setup_path "$setup" \
    --arg setup_real "$real" \
    --arg setup_sha256 "$sha" \
    --arg source "$source" \
    --arg guide "$guide" \
    --arg fleet_check "$fleet" \
    --arg tree "$tree" \
    --arg failed "$failed" \
    --arg created_at "$(date -u -d "@$now_epoch" +%Y-%m-%dT%H:%M:%SZ)" \
    --argjson created_epoch "$now_epoch" \
    '{setup_path: $setup_path, setup_real: $setup_real,
      setup_sha256: $setup_sha256,
      setup_snapshot_cl: $h.setup_snapshot_cl,
      setup_last_change_cl: $h.setup_last_change_cl,
      source: $source,
      guide: (if $guide == "" then null else $guide end),
      fleet_check: $fleet_check, tree: $tree, failed: $failed,
      manual_command: $h.manual_command,
      created_at: $created_at, created_epoch: $created_epoch}' \
    | write_json "$CONSENT_RECORD"

  echo "SETUP_PATH=$setup"
  echo "SETUP_REAL=$real"
  echo "SETUP_SHA256=$sha"
  echo "SETUP_SNAPSHOT_CL=$(hj .setup_snapshot_cl)"
  echo "SETUP_LAST_CHANGE_CL=$(hj .setup_last_change_cl)"
  echo "MANUAL_COMMAND=$(hj .manual_command)"
  echo "CONSENT_RECORD=$CONSENT_RECORD"
  echo "$BEGIN_TEXT"
  jq -r '.markdown // ""' <<<"$HJSON"
  echo "$END_TEXT"
}

cmd_backup_config() {
  (( $# == 0 )) || usage_error "backup-config takes no arguments"
  helper_or_die backup-config
  if (( HRC != 0 )) || ! jq -e '.ok == true' >/dev/null <<<"$HJSON"; then
    echo "BACKUP=-"
    return 1
  fi
  echo "BACKUP=$(hj .backup)"
}

cmd_setup() {
  local consented="" tree="" setup_arg=""
  while (( $# > 0 )); do
    case "$1" in
      --consented) consented=1 ;;
      --tree=*) tree="${1#*=}" ;;
      --setup=*) setup_arg="${1#*=}" ;;
      *) usage_error "setup: unknown argument: $1" ;;
    esac
    shift
  done
  # Never run the setup script without the user's explicit consent.
  if [[ -z "$consented" ]]; then
    echo "SETUP_STATUS=CONSENT_REQUIRED"
    stop 7 "Workbox setup needs the user's consent (--consented)"
  fi
  [[ -n "$tree" ]] || usage_error "setup: --tree is required"

  # (a) The consent record: exactly the values the user saw.
  if [[ ! -f "$CONSENT_RECORD" ]] \
       || ! jq -e 'type == "object"' >/dev/null 2>&1 <"$CONSENT_RECORD"; then
    echo "SETUP_STATUS=CONSENT_REQUIRED"
    stop 7 "No consent record; show the consent text (consent-text) and ask again"
  fi
  local rec
  rec="$(cat "$CONSENT_RECORD")"
  rj() { jq -r "($1) // \"-\" | tostring" <<<"$rec"; }
  local created age
  created="$(rj .created_epoch)"
  [[ "$created" =~ ^[0-9]+$ ]] || created=0
  age=$(( $(date +%s) - created ))
  if (( age > CONSENT_MAX_AGE_SECS || age < -300 )); then
    echo "SETUP_STATUS=CONSENT_REQUIRED"
    stop 7 "The consent record is older than 1 hour; show the consent text and ask again"
  fi
  if [[ "$(rj .tree)" != "$tree" ]]; then
    echo "SETUP_STATUS=CONSENT_REQUIRED"
    stop 7 "The consent was given for tree $(rj .tree), not $tree; show the consent text and ask again"
  fi
  local setup_path setup_real
  setup_path="$(rj .setup_path)"
  setup_real="$(rj .setup_real)"
  if [[ -n "$setup_arg" ]]; then
    [[ "$setup_arg" != "~/"* ]] || setup_arg="$HOME/${setup_arg#\~/}"
    if [[ "$setup_arg" != "$setup_path" && "$setup_arg" != "$setup_real" ]]; then
      echo "SETUP_STATUS=CONSENT_REQUIRED"
      stop 7 "The consent was given for \`$setup_path\`, not \`$setup_arg\`; show the consent text and ask again"
    fi
  fi
  local sha snap last
  sha="$(rj .setup_sha256)"
  snap="$(rj .setup_snapshot_cl)"
  last="$(rj .setup_last_change_cl)"

  # (b) The code that runs must be the code the user saw.
  if [[ ! -f "$setup_real" || ! -r "$setup_real" ]] \
       || [[ "$(sha256_of "$setup_real")" != "$sha" ]]; then
    [[ -n "${DRY_RUN:-}" ]] || rm -f "$CONSENT_RECORD"
    echo "SETUP_STATUS=CHANGED"
    echo "SETUP_SHA256=$sha"
    stop 8 "The workbox setup script changed after consent"
  fi

  local log
  log="$HOME/.planter/logs/e2e-workbox-setup-$(date -u +%Y%m%dT%H%M%SZ).log"
  if [[ -n "${DRY_RUN:-}" ]]; then
    echo "DRY_RUN set; would run:"
    printf '%q ' "$0" backup-config
    echo
    printf '%q ' GIT_SSH_COMMAND='ssh -o BatchMode=yes' bash "$setup_real" \
      --no-start --tree "$tree"
    printf '</dev/null 2>&1 | tee %q\n' "$log"
    echo "SETUP_STATUS=DRY_RUN"
    echo "SETUP_SHA256=$sha"
    echo "SETUP_SNAPSHOT_CL=$snap"
    echo "SETUP_LAST_CHANGE_CL=$last"
    return 0
  fi

  # Consent is one-shot: from here on, a new attempt needs a new consent.
  rm -f "$CONSENT_RECORD"

  # (c) Back up the global agent configuration that setup changes.
  local backup rc=0
  helper_call backup-config || rc=$?
  if (( rc != 0 || HRC != 0 )) || ! jq -e '.ok == true' >/dev/null <<<"$HJSON"; then
    echo "SETUP_STATUS=BACKUP_FAILED"
    stop 8 "Failed to back up the agent configuration before setup"
  fi
  backup="$(hj .backup)"

  # (d) Run the script under the pipefail wrapper. stdin is /dev/null, so a
  # login or other prompt in the script fails fast instead of hanging.
  local started finished
  started="$(now_iso)"
  mkdir -p "${log%/*}"
  echo "Running the workbox setup script: bash $setup_real --no-start" \
       "--tree $tree (log: $log)" >&2
  set +e
  (
    set -uo pipefail
    export PATH="$HOME/.local/bin:$PATH"
    GIT_SSH_COMMAND='ssh -o BatchMode=yes' \
      bash "$setup_real" --no-start --tree "$tree" </dev/null 2>&1 \
      | tee "$log" >&2
    exit "${PIPESTATUS[0]}"
  )
  rc=$?
  set -e
  finished="$(now_iso)"

  local die_line
  die_line="$(awk -v p="$DIE_PREFIX" 'index($0, p) == 1 { l = $0 } END { print l }' "$log")"

  # (f) The post-setup check.
  local post="ERROR" post_failed="" crc=0
  helper_call check --tree="$tree" || crc=$?
  if (( crc == 0 )); then
    post="$(hj .status)"
    post_failed="$(jq -r '[.checks[]? | select(.result == "FAIL") | .id] | join(",")' \
      <<<"$HJSON")"
  fi

  # (e) The setup record (copied into the run state by planter_workbox_run.sh).
  jq -n \
    --argjson rec "$rec" \
    --argjson exit "$rc" \
    --arg log "$log" \
    --arg backup "$backup" \
    --arg post_check "$post" \
    --arg started_at "$started" \
    --arg finished_at "$finished" \
    '{path: $rec.setup_path, real: $rec.setup_real, sha256: $rec.setup_sha256,
      snapshot_cl: $rec.setup_snapshot_cl,
      last_change_cl: $rec.setup_last_change_cl,
      source: $rec.source, guide: $rec.guide, fleet_check: $rec.fleet_check,
      tree: $rec.tree, exit: $exit, log: $log,
      backup: (if $backup == "-" then null else $backup end),
      post_check: $post_check,
      started_at: $started_at, finished_at: $finished_at}' \
    | write_json "$SETUP_RECORD"

  local status=OK
  (( rc == 0 )) && [[ "$post" == READY ]] || status=FAILED
  echo "SETUP_STATUS=$status"
  echo "SETUP_EXIT=$rc"
  echo "LOG=$log"
  echo "DIE_LINE=$die_line"
  echo "SETUP_SHA256=$sha"
  echo "SETUP_SNAPSHOT_CL=$snap"
  echo "SETUP_LAST_CHANGE_CL=$last"
  echo "BACKUP=$backup"
  awk '/⚠️|WARNING/ { print "WARN " $0 }' "$log"
  echo "POST_CHECK=$post"
  echo "POST_CHECK_FAILED=$post_failed"
  echo "SETUP_RECORD=$SETUP_RECORD"
  if (( rc != 0 )); then
    stop 8 "Failed to set up Workbox: ${die_line:-exit $rc}; see $log"
  fi
  if [[ "$post" != READY ]]; then
    stop 8 "Failed to set up Workbox: post-setup check is $post${post_failed:+ (failed: $post_failed)}; see $log"
  fi
}

(( $# > 0 )) || usage_error "missing subcommand"
sub="$1"
shift
case "$sub" in
  -h|--help|help) usage ;;
  version) cmd_version "$@" ;;
  check) cmd_check "$@" ;;
  creds) cmd_creds "$@" ;;
  consent-text) cmd_consent_text "$@" ;;
  backup-config) cmd_backup_config "$@" ;;
  setup) cmd_setup "$@" ;;
  *) usage_error "unknown subcommand: $sub" ;;
esac
