#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags test-only data files placed in the runtime `data` attribute of a
# rustc_library/rustc_binary/rustc_proc_macro that defines unit tests
# (with_unit_tests / with_host_unit_tests). Files needed only by the generated
# <name>_test belong in `test_data` (or a host_test_data() target from
# //build/bazel/rules/host_tests/host_test_data.bzl for host tests), not in
# `data`, which ships them with the library and forces rule macros (e.g. host
# test wrappers) to be edited to forward `data`.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import json
import os
import re
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in re.split(r"[,\s]+", sys.argv[2]) if d.strip()]

RULE_RE = re.compile(r"^(rustc_library|rustc_binary|rustc_proc_macro)\(\s*$", re.M)
TESTY = re.compile(r"(^|[/_.-])(test_?data|testdata|tests?|fixtures?|golden)([/_.-]|$)", re.I)

findings = []


def block_at(text, start):
    depth = 0
    for i in range(start, len(text)):
        c = text[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[start:i + 1]
    return text[start:]


def list_attr(block, attr):
    m = re.search(r"^\s+" + attr + r"\s*=\s*\[(.*?)\]", block, re.M | re.S)
    if not m:
        return None
    return re.findall(r'"([^"]+)"', m.group(1))


for d in target_dirs:
    path = os.path.join(workdir, d, "BUILD.bazel")
    if not os.path.isfile(path):
        continue
    text = open(path, encoding="utf-8").read()
    for m in RULE_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        if not re.search(r"^\s+with_(host_)?unit_tests\s*=", block, re.M):
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        data = list_attr(block, "data") or []
        testy = [e for e in data if TESTY.search(e)]
        if not testy:
            continue
        line = text[:m.start()].count("\n") + 1
        findings.append({
            "severity": "ERROR",
            "file": os.path.join(d, "BUILD.bazel"),
            "line": line,
            "rule": "test_only_data_in_data",
            "message": (
                f"{m.group(1)}(name = \"{name}\") defines unit tests but lists test-only "
                f"files in `data`: {', '.join(testy)}. `data` is runtime data of the "
                "library itself and ships to every dependent."
            ),
            "remediation": (
                "Move files used only by the generated <name>_test to `test_data` (for host "
                "tests, a host_test_data() target from "
                "//build/bazel/rules/host_tests/host_test_data.bzl when the test needs them at "
                "a stable runtime path). Do not edit shared rule macros (.bzl) to forward "
                "`data` to work around this, and do not change sources to locate the files. "
                "Example: data = [\"test_data/main.json\"] -> test_data = [\"test_data/main.json\"]."
            ),
        })

print(json.dumps(findings, indent=2))
PYEOF
