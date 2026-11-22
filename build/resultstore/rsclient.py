#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Central definitions and prebuilt path resolution for the rsclient tool suite."""

import pathlib

# Tool binaries and wrapper scripts provided by the prebuilt rsclient suite
# under prebuilt/rsclient/$HOST_PLATFORM/bin/.
BESPROXY_BIN = "besproxy"
BESPROXY_WRAP_SH = "besproxy-wrap.sh"
RS_BAZEL_SH = "rs-bazel.sh"
BUILDMINDER_BIN = "buildminder"
BUILDMINDER_WRAP_SH = "buildminder-wrap.sh"
RSPROXY_BIN = "rsproxy"
RSPROXY_WRAP_SH = "rsproxy-wrap.sh"

# Environment variable used to instruct child Bazel invocations to wrap
# themselves with besproxy. Typically set by the Fuchsia build infrastructure.
ENV_FX_INTERNAL_BAZEL_BESPROXY = "FX_INTERNAL_BAZEL_BESPROXY"


def bin_dir(fuchsia_dir: pathlib.Path, host_tag: str) -> pathlib.Path:
    """Returns the prebuilt rsclient bin directory for the given host platform-arch tag.

    Args:
        fuchsia_dir: Path to the Fuchsia checkout root.
        host_tag: Host platform-arch tag (e.g. 'linux-x64', 'mac-arm64').

    Returns:
        Path to the prebuilt rsclient bin directory.
    """
    return fuchsia_dir / "prebuilt" / "rsclient" / host_tag / "bin"
