# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A host_py_test() main whose main_callable is a coroutine function."""

import asyncio


async def main() -> None:
    await asyncio.sleep(0)
    print("async main called")
