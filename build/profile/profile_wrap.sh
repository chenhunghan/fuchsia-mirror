#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Forwarding shim for profile_wrap.py.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
readonly FUCHSIA_DIR="$(cd "$SCRIPT_DIR/../.." >/dev/null 2>&1 && pwd)"

source "$FUCHSIA_DIR/tools/devshell/lib/platform.sh"

exec env PYTHONPATH="$FUCHSIA_DIR" "$PREBUILT_PYTHON3" -S "$SCRIPT_DIR/profile_wrap.py" "$@"
