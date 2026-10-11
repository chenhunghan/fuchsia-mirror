#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Creates the Planter task that migrates the specified directory to
# Bazel with a commit message that follows
# //build/beads/references/migration/commit_message/guidelines.md.
# Then, execute the Planter task in two steps:
# 1. `planter run` (no --gerrit): Planter migrates the directory, runs the
#    checks and the review panel, and stops in UPLOADING.
# 2. `planter run --gerrit`: Planter commits and uploads the CL to Gerrit,
#    and stops in MONITORING_REVIEW.
#
# Planter turns:
# --title into the commit subject,
# --desc into the commit body; footer lines (e.g. `Bug:`,
# `Bazel-Migration-Target:`) in the last paragraph of --desc become footers.
# Planter itself adds the `Test:` footers, TAG: and Change-Id.

set -euo pipefail

usage() {
  cat <<EOF
Usage: $(basename "$0") --target-dir=<DIR> --title=<TITLE> [--desc=<DESC>]
         [--task-id=<ID>]

Required:
  --target-dir=<DIR>   Directory to migrate, relative to \$FUCHSIA_DIR
                       (e.g. src/connectivity/bluetooth/lib/bt-obex/objects).
  --title=<TITLE>      Commit subject, at most 65 chars. Guideline form:
                       "[bazel_migration][<area>] <reference>", without
                       "Migrate".

Optional:
  --desc=<DESC>        Commit body. Footer lines in its last paragraph
                       (e.g. Bug:, Bazel-Migration-Target:) become
                       footers. Planter adds Test:, TAG: and Change-Id.
  --task-id=<ID>       Planter task ID. Defaults to all components of
                       --target-dir joined by "_" (e.g.
                       src_connectivity_bluetooth_lib_bt-obex_objects).
  -h, --help           Show this help.

Environment:
  FUCHSIA_DIR          Fuchsia checkout (default: \$HOME/fuchsia).
  DRY_RUN              If set, print the planter command instead of running it.
EOF
}

die() { echo "error: $*" >&2; exit 1; }

TARGET_DIR=""
TITLE=""
DESC=""
TASK_ID=""

# Accepts both "--flag=value" and "--flag value".
while (( $# > 0 )); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --target-dir=*) TARGET_DIR="${1#*=}" ;;
    --title=*) TITLE="${1#*=}" ;;
    --desc=*) DESC="${1#*=}" ;;
    --task-id=*) TASK_ID="${1#*=}" ;;
    --target-dir|--title|--desc|--task-id)
      (( $# >= 2 )) || die "$1 requires a value"
      case "$1" in
        --target-dir) TARGET_DIR="$2" ;;
        --title) TITLE="$2" ;;
        --desc) DESC="$2" ;;
        --task-id) TASK_ID="$2" ;;
      esac
      shift ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done

[[ -n "$TARGET_DIR" ]] || { usage >&2; die "--target-dir is required"; }
[[ -n "$TITLE" ]] || { usage >&2; die "--title is required"; }

# Normalize the target dir: strip a leading "//" and trailing "/".
TARGET_DIR="${TARGET_DIR#//}"
TARGET_DIR="${TARGET_DIR%/}"

if [[ -z "$TASK_ID" ]]; then
  # All components of the target dir joined by "_", as defined in
  # references/bazel_migration_planter.md.
  TASK_ID="${TARGET_DIR//\//_}"
fi

FUCHSIA_DIR="${FUCHSIA_DIR:-$HOME/fuchsia}"
PLANTER="$(command -v planter || echo "$HOME/.local/bin/planter")"

(( ${#TITLE} <= 65 )) || die "subject is ${#TITLE} chars; Planter allows at most 65"
[[ -x "$PLANTER" ]] || die "planter not found (install it with publish_prebuilt.sh)"
[[ -f "$HOME/.planter/beads/build/beads/planter/machinery/prompts/coder.md" ]] \
  || die "run 'planter init-project' first"
[[ -f "$FUCHSIA_DIR/$TARGET_DIR/BUILD.gn" ]] \
  || die "no BUILD.gn in $FUCHSIA_DIR/$TARGET_DIR"

printf 'Commit message (Planter appends Test:, TAG: and Change-Id):\n\n%s\n\n' \
  "$TITLE"
[[ -z "$DESC" ]] || printf '%s\n\n' "$DESC"

add_cmd=("$PLANTER" add-task --workdir="$FUCHSIA_DIR" --id="$TASK_ID"
         --target-dir="$TARGET_DIR" --title="$TITLE")
[[ -z "$DESC" ]] || add_cmd+=(--desc="$DESC")
# Step 1, no --gerrit: Planter migrates the directory, runs the checks and the
# review panel, and stops in UPLOADING without committing or pushing.
run_cmd=("$PLANTER" run --repo-root="$FUCHSIA_DIR" --task="$TASK_ID")
# Step 2, --gerrit: Planter commits, uploads the CL and stops in
# MONITORING_REVIEW.
upload_cmd=("${run_cmd[@]}" --gerrit)
if [[ -n "${DRY_RUN:-}" ]]; then
  echo "DRY_RUN set; would run:"
  printf '%q ' "${add_cmd[@]}"
  echo
  printf '%q ' "${run_cmd[@]}"
  echo
  printf '%q ' "${upload_cmd[@]}"
  echo
  exit 0
fi

# `planter run` works on top of HEAD, so it needs a clean checkout with no
# unrelated local commits on top of origin/main.
if [[ -n "$(git -C "$FUCHSIA_DIR" status --porcelain)" ||
      "$(git -C "$FUCHSIA_DIR" rev-list --count origin/main..HEAD)" != 0 ]]; then
  echo "warning: $FUCHSIA_DIR is dirty or has local commits;" \
       "'planter run' may refuse to start or build on top of them." >&2
fi

# Prints "<status>/<phase>" of the task from `planter status`, or nothing if
# the task is not listed.
task_status() {
  "$PLANTER" status 2>/dev/null | awk -v id="$TASK_ID" '
    $1 == "-" && $2 == id {
      for (i = 3; i <= NF; i++) {
        if ($i ~ /^status=/) { status = substr($i, 8) }
        if ($i ~ /^phase=/) { phase = substr($i, 7) }
      }
      print status "/" phase
      exit
    }' || true
}

"${add_cmd[@]}"
# Run from the checkout: Planter prefers the Fuchsia checkout containing pwd.
(cd "$FUCHSIA_DIR" && "${run_cmd[@]}")

status="$(task_status)"
if [[ -n "$status" && "$status" != */UPLOADING ]]; then
  die "task $TASK_ID is $status after the migration, not ready to upload;" \
      "see 'planter inspect --task=$TASK_ID'"
fi

echo "=== Migration done; uploading the CL to Gerrit ==="
(cd "$FUCHSIA_DIR" && "${upload_cmd[@]}")

cat <<EOF

Planter uploaded the CL ($(task_status)). Next:
  planter status                              # shows the change number
EOF