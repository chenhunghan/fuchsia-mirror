#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Audits new or changed shared Bazel symbolic macros and their bazel2gn GN
# template counterparts for API drift that breaks Bazel/GN parity:
#   - macro_redeclares_inherited_attr (error): a symbolic `macro(...)` declares
#     an inherited common attribute (`applicable_licenses`) or silences the
#     buildifier `attr-applicable_licenses` warning. Package
#     `default_applicable_licenses` already applies to macro-created targets.
#   - macro_list_attr_none_compare (warning): a list/dict attribute of a
#     symbolic macro is compared against `None`. Omitted list attrs arrive as
#     `[]`/`{}`, so `!= None` is always true and forwards an empty override.
#   - gn_template_crate_name_not_forwarded (error): a changed `.gni` template
#     reads `invoker.crate_name` but sets the Rust `crate_name` to a decorated
#     internal target string, so GN and Bazel compile different crate names.

WORKDIR="${PLANTER_WORKDIR:-.}"

python3 - "$WORKDIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
findings = []


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


def emit(category, severity, path, line, message, remediation):
    findings.append(
        {
            "category": category,
            "severity": severity,
            "file": path,
            "line": line,
            "message": message,
            "remediation": remediation,
        }
    )


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


# Collect files added or modified by the change (plus untracked files).
def changed_files():
    files = set(git(["diff", "--name-only", "--diff-filter=AM", change_base]).split())
    files |= set(git(["ls-files", "--others", "--exclude-standard"]).split())
    return sorted(f for f in files if os.path.isfile(os.path.join(workdir, f)))


# Check symbolic macros for redeclared inherited attrs.
def check_inherited_attrs(path, text):
    for m in re.finditer(
        r'"applicable_licenses"\s*:\s*attr\.|disable=attr-applicable_licenses', text
    ):
        emit(
            "macro_redeclares_inherited_attr",
            "error",
            path,
            line_of(text, m.start()),
            "Symbolic macro redeclares the inherited `applicable_licenses` attribute "
            "(or silences its buildifier warning). No sibling Fuchsia rule macro does "
            "this, and package `default_applicable_licenses` already applies to "
            "targets created by symbolic macros.",
            "Drop the attribute, its impl parameter, any `package_metadata` forwarding,\n"
            "the buildifier disable comment, and plumbing added only for it (GN template\n"
            "forward_variables_from entries, bazel2gn attribute cases and test inputs).",
        )


# Check symbolic macros for None comparisons on list/dict attrs.
def check_list_none_compare(path, text):
    list_attrs = set(
        re.findall(
            r'"(\w+)"\s*:\s*attr\.(?:label_list|string_list|string_dict|'
            r"label_keyed_string_dict|string_list_dict|int_list)\(",
            text,
        )
    )
    for name in sorted(list_attrs):
        for m in re.finditer(
            r"\b%s\s*(?:!=|==)\s*None\b|\b%s\s+is\s+(?:not\s+)?None\b"
            % (re.escape(name), re.escape(name)),
            text,
        ):
            emit(
                "macro_list_attr_none_compare",
                "warning",
                path,
                line_of(text, m.start()),
                f"Symbolic macro compares list/dict attribute `{name}` against `None`; "
                "omitted list attributes arrive as an empty value, so the branch "
                "always runs and forwards an empty override.",
                f"Test truthiness instead (`if {name}:`), and make sure every value you\n"
                "forward to the wrapped rule is a list (never `None`) on all code paths.",
            )


# Check GN templates forward invoker.crate_name as the Rust crate name.
def check_gni_crate_name(path, text):
    if "invoker.crate_name" not in text:
        return
    for m in re.finditer(r"^\s*crate_name\s*=\s*([A-Za-z_]\w*)\s*$", text, re.M):
        var = m.group(1)
        decorated = re.search(
            r'^\s*%s\s*=\s*"[^"\n]*\$\{' % re.escape(var), text, re.M
        )
        if decorated:
            emit(
                "gn_template_crate_name_not_forwarded",
                "error",
                path,
                line_of(text, m.start()),
                f"GN template reads `invoker.crate_name` but sets `crate_name = {var}`, "
                "a decorated internal target string. GN then compiles a different "
                "crate name than Bazel (affects module_path!, log targets, "
                "CARGO_CRATE_NAME).",
                "Pass the resolved crate name (invoker.crate_name, else the target name)\n"
                "as `crate_name`, as sibling rustc_library/rustc_cdylib templates do, and\n"
                "keep only `output_name` (or the inner target name) decorated.",
            )


for f in changed_files():
    try:
        text = open(os.path.join(workdir, f), encoding="utf-8").read()
    except Exception:
        continue
    if f.endswith(".bzl") and re.search(r"\bmacro\(", text):
        check_inherited_attrs(f, text)
        check_list_none_compare(f, text)
    elif f.endswith(".gni"):
        check_gni_crate_name(f, text)

print(json.dumps(findings, indent=2))
PYEOF
