#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Helper script for the `cipd_prebuilt()` GN template.
#
# Fetches one or more on-demand CIPD prebuilt paths via `jiri fetch-package`
# and stages them into `${cipd_prebuilt_root}/prebuilt/...`.
#
# Usage:
#   fetch_cipd_prebuilt.sh <jiri_bin> <checkout_root> <gen_root> <jiri_package_path>...

set -euo pipefail

if [[ $# -lt 4 ]]; then
  echo "Usage: $0 <jiri_bin> <checkout_root> <gen_root> <jiri_package_path>..." >&2
  exit 1
fi

readonly jiri_bin="$1"
readonly checkout_root="$2"
readonly gen_root="$3"
shift 3

# 1. Ensure the CIPD package(s) backing all requested paths are downloaded into //prebuilt/.
"$jiri_bin" fetch-package "$@"

# 2. Materialize each path under `gen_root` as a copy with a fresh mtime.
#
# Note: We intentionally copy (`cp -RfL`, without `-p`/`-a`) rather than
# creating a symlink or hardlink to `//prebuilt/...`:
# - Ninja uses `stat()` (which follows symlinks) to verify that an action's
#   outputs are newer than its inputs (`.jiri_root/update_history/latest`,
#   `.jiri_root/bin/jiri`, and this script). Because un-rolled packages in
#   `//prebuilt/` have older timestamps than the latest Jiri snapshot or script
#   checkout, a symlink or hardlink would appear stale to Ninja on every build.
# - Conversely, `touch`ing the target inside `//prebuilt/` during the build to
#   freshen a symlink would mutate input timestamps mid-build and break Ninja
#   no-op convergence for any other target referencing `//prebuilt/...` (see
#   //docs/development/build/ninja_no_op.md).
for rel_path in "$@"; do
  prebuilt_path="${checkout_root}/${rel_path}"
  output_path="${gen_root}/${rel_path}"

  if [[ ! -e "$prebuilt_path" ]]; then
    echo "Error: '$prebuilt_path' does not exist after running '$jiri_bin fetch-package $rel_path'" >&2
    exit 1
  fi

  mkdir -p "$(dirname "$output_path")"
  rm -rf "$output_path"
  cp -RfL "$prebuilt_path" "$output_path"
done
