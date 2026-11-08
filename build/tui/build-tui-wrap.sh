#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# build-tui-wrap.sh delegates to build_tui_wrap.py in the Fuchsia Python environment.

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"

source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

export PYTHONPATH="${FUCHSIA_DIR}"

exec "${PREBUILT_PYTHON3}" -S "${SCRIPT_DIR}/build_tui_wrap.py" \
  --fuchsia-dir "${FUCHSIA_DIR}" \
  "$@"
