#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""A toy build action, for trying the Python forkserver by hand.

It does what a typical Python build action does: parses its arguments, reads
its inputs, and writes an output and, optionally, a depfile.
"""

import argparse
import hashlib
import json
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(__doc__ or "").splitlines()[0]
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--depfile", type=Path)
    parser.add_argument("inputs", nargs="*", type=Path)
    args = parser.parse_args()
    digests = {
        str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in args.inputs
    }
    args.output.write_text(json.dumps(digests, indent=2, sort_keys=True) + "\n")
    if args.depfile:
        inputs = " ".join(str(p) for p in args.inputs)
        args.depfile.write_text(f"{args.output}: {inputs}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
