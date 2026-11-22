# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A host_py_test() main without any test, which must fail with the default main_callable."""


def unused() -> int:
    return 0
