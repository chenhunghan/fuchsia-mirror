#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Helper tool for machinery evolution: statically audits shell scripts, inline
# Python heredocs, Python helper modules, and JSON manifests in
# $PLANTER_PROJECT_DIR for syntax errors, undefined Python names, and
# module-level use-before-assignment without running any builds.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${PLANTER_PROJECT_DIR:-$(dirname "$SCRIPT_DIR")}"

LINT_CHECK="$PROJECT_DIR/checks/machinery_script_lint.sh"
if [[ ! -f "$LINT_CHECK" && -f "$PROJECT_DIR/machinery/checks/machinery_script_lint.sh" ]]; then
  LINT_CHECK="$PROJECT_DIR/machinery/checks/machinery_script_lint.sh"
fi

exec env PLANTER_PROJECT_DIR="$PROJECT_DIR" \
  bash "$LINT_CHECK" --project-dir "$@"
