# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Deprecated compatibility alias for the consolidated Tracing affordance."""

from honeydew.affordances.tracing.tracing import Tracing as TracingUsingFfx

# TODO(https://fxbug.dev/495898947): Remove once downstream callers migrate to
# `tracing.Tracing`.
__all__ = ["TracingUsingFfx"]
