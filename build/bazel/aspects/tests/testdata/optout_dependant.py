# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fixture depending on a no-pyrefly library, verifying the opt-out does not
affect the library's dependants."""

import type_error_optout


def main():
    value: int = type_error_optout.mistyped()
    print(value)


if __name__ == "__main__":
    main()
