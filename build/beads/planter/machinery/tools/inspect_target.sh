#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
# Helper tool for inspecting GN and Bazel build files in the target directory.
ls -la "${PLANTER_WORKDIR:-.}/${PLANTER_TARGET_DIR:-.}"
