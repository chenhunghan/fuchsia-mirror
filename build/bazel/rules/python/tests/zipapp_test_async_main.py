#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Main source of a py_zipapp_binary() test binary with an async main()."""

import asyncio

import zipapp_test_lib


async def main() -> None:
    await asyncio.sleep(0)
    print(f"async: {zipapp_test_lib.message()}")


if __name__ == "__main__":
    asyncio.run(main())
