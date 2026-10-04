#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Common methods for Google Cloud / RBE credentials management and isolation.
#
# This file is sourced by `vars.sh` or devshell commands requiring safe credentials isolation.

function fx-setup-isolated-adc {
  # Isolates the Google Application Default Credentials (ADC) for the Fuchsia development environment,
  # injecting 'rbe-fuchsia-prod' as the quota project if standard user credentials are used.
  # Returns the path to the isolated credentials file, or nothing if not applicable.
  local -r isolate_script="${FUCHSIA_DIR}/build/auth/gcloud_creds.py"
  if [[ ! -f "$isolate_script" ]]; then
    echo "Error: Missing required in-tree credentials management script: $isolate_script" >&2
    return 1
  fi
  local -r local_adc="$("${PREBUILT_PYTHON3}" -S "$isolate_script" isolate --out-dir "${FUCHSIA_DIR}/out" --quota-project rbe-fuchsia-prod)"
  if [[ -n "$local_adc" ]]; then
    echo "$local_adc"
  fi
}
