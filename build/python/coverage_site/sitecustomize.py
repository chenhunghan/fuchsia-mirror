# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Python sitecustomize hook that activates Fuchsia Python coverage collection."""

import os

if os.environ.get("FUCHSIA_PYTHON_COVERAGE_DIR"):
    import _fuchsia_pycov

    _fuchsia_pycov.start()
