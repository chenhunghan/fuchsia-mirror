#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Blocks the gate when the planter running these checks is too old for them.
#
# These checks rely on planter fixing diffengine false positives itself
# (cl/1010931533) and honoring finding waivers (cl/1010931925). An older
# planter runs them without complaint but raises blocking findings it should
# not. Planter exports PLANTER_RESOLVED_CHANGE_BASE to every check from
# cl/1010931925 on and older planters never do, so its absence in a check that
# planter runs (PLANTER_CHECK_NAME is set) means the planter is too old.
# tools/run_checks.sh sets both variables, so manual runs pass.
#
# Newer planters also refuse to run when they are older than min_planter_cl in
# manifest.json. This check covers the planters that predate that field; delete
# it once none of them are in use.

# Verify that the running planter binary exports PLANTER_RESOLVED_CHANGE_BASE.
emit_planter_too_old() {
  local msg="$1"
  printf '[{"category":"planter_too_old","severity":"ERROR","message":"%s"}]\n' "$msg"
}

if [[ -n "${PLANTER_CHECK_NAME:-}" && -z "${PLANTER_RESOLVED_CHANGE_BASE+set}" ]]; then
  MSG="This planter is too old for the checks in build/beads/planter/machinery: "
  MSG+="it will raise false-positive blocking findings and ignore waivers. "
  MSG+="Sync to cl/1010931925 or later, rebuild planter, and rerun."
  emit_planter_too_old "$MSG"
else
  echo '[]'
fi
