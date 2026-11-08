#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

#### CATEGORY=Build
### print the Fuchsia source directory (checkout root, aka "//")

## usage: fx get-src-dir
##
## Prints the root directory of the Fuchsia source checkout.

set -euo pipefail

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  echo "usage: fx get-src-dir"
  echo ""
  echo "Prints the root directory of the Fuchsia source checkout."
  exit 0
fi

if [[ $# -ne 0 ]]; then
  echo "usage: fx get-src-dir" >&2
  exit 2
fi

if [[ -z "${FUCHSIA_DIR:-}" ]]; then
  echo "ERROR: FUCHSIA_DIR is not set." >&2
  exit 1
fi

echo "${FUCHSIA_DIR}"
