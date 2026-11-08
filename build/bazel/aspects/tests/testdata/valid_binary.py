# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Typed binary fixture importing valid_lib for pyrefly_aspect tests."""

from typing import TYPE_CHECKING

import valid_lib

# NOTE(nathaniel): "if TYPE_CHECKING:" blocks are very strongly discouraged. One
# appears here in this test input to verify that they work, but please do not
# casually add any more around the codebase.
if TYPE_CHECKING:
    import valid_stub

    _tag: str = valid_stub.STUB_TAG


def main():
    print(valid_lib.greet("fuchsia"))


if __name__ == "__main__":
    main()
