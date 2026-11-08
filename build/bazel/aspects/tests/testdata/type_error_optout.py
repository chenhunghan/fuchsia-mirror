# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fixture with a type error, verifying the tags = ["no-pyrefly"] opt-out."""


def mistyped() -> int:
    return "not an int"
