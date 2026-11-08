# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Typed library fixture for pyrefly_aspect tests."""


def greet(name: str) -> str:
    return f"hello, {name}"
