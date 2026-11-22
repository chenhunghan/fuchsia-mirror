# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Sticky opt-in setting for the Python forkserver."""

import pathlib
from collections.abc import Mapping

from build.scripts import properties

# Path relative to the Fuchsia source root for the sticky setting.
_CONFIG = pathlib.Path(".fx/config/python_forkserver")

_ENV_PYTHON_FORKSERVER = "FX_BUILD_INTERNAL_PYTHON_FORKSERVER"


def load(source_dir: pathlib.Path, environ: Mapping[str, str]) -> bool:
    """Returns the forkserver setting from the environment or //.fx/config.

    The environment takes precedence over the config file, which holds a line
    like FX_BUILD_INTERNAL_PYTHON_FORKSERVER=1.
    """
    value = environ.get(_ENV_PYTHON_FORKSERVER) or properties.load_properties(
        source_dir / _CONFIG
    ).get(_ENV_PYTHON_FORKSERVER, "")
    return properties.str_to_bool(value, default=False)
