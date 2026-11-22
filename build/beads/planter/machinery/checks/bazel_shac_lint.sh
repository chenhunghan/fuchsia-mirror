#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Deterministic check mirroring the CQ static-checks shac Bazel lint rules
# (bazel_bzl_require_load_visibility, bazel_configurable_attributes,
# bazel_disallowed_workspace_root_package_labels, bazel_target_visibility,
# bazel_load_visibility, ...). It runs the checkout's own
# scripts/shac/bazel_lint/main.py for every registered rule on each Bazel file
# (BUILD.bazel, BUILD, *.bzl, *.bazel) the change adds or modifies, so these
# failures surface in seconds even when the full shac run in build_verification
# is skipped. Prints a JSON array of findings on stdout.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"

python3 - "$WORKDIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"
lint_dir = os.path.join(workdir, "scripts", "shac", "bazel_lint")
main_py = os.path.join(lint_dir, "main.py")
bazel_star = os.path.join(workdir, "scripts", "shac", "bazel.star")

HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")

REMEDIATION_BY_RULE = {
    "bazel_bzl_require_load_visibility": (
        "Add a top-level load visibility() call after the load() statements of the .bzl\n"
        "file: visibility(\"private\") when only BUILD files of its own package load it,\n"
        "otherwise the narrowest list of loading packages or subtrees."
    ),
    "bazel_configurable_attributes": (
        "Do not evaluate configurable symbolic macro attributes or `select()` values as\n"
        "booleans (`if attr:`, `attr or []`, `bool(attr)`), compare them to non-`None`\n"
        "values, place them in list/tuple/dict literals (`[attr]`), iterate/index/format\n"
        "them, or pass them to non-configurable attributes (`name`, `tags`, `testonly`,\n"
        "`visibility`). Use `+` / `|` (and `!= None` / `== None` on `None`-defaulted\n"
        "`attr.label()` or `inherit_attrs` parameters), or set `configurable = False` in\n"
        "`macro(attrs = {...})` per //docs/development/languages/bazel/style/rules_macros.md."
    ),
    "bazel_disallowed_workspace_root_package_labels": (
        "Never reference a file in a subdirectory through the root package ('//:<dir>/<file>').\n"
        "Use the label of the package that owns the file ('//<dir>:<file>'), adding a\n"
        "BUILD.bazel with exports_files([...], visibility = [...]) or a filegroup there\n"
        "(in a separate CL below the migration when the directory is outside its scope)."
    ),
    "bazel_target_visibility": (
        "Replace \"//visibility:public\" with the visibility find_rdeps recommends\n"
        "(per_target_recommended_visibility); test entry points need no visibility."
    ),
    "bazel_test_suite_target_compatible_with": (
        "Remove target_compatible_with from the test_suite() and set it on its leaf\n"
        "tests instead (host_*_test macros already default to HOST_OS_CONSTRAINTS)."
    ),
}
DEFAULT_REMEDIATION = (
    "Fix the reported Bazel lint finding in the file (the same rule fails CQ\n"
    "static-checks); run `fx lint --only=<rule> <file>` to confirm."
)


# Collect Bazel files added or modified by the change (tracked and untracked).
def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


def git_ok(args):
    try:
        res = subprocess.run(
            ["git", "-C", workdir] + args,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return res.returncode == 0
    except Exception:
        return False


# Read rules registered with `enforcement = "unchanged_lines_warning_only"` in
# scripts/shac/bazel.star so unchanged pre-existing lines downgrade to non-blocking
# `info` notes in Planter (matching shac downgrading them to non-blocking warnings).
def load_unchanged_warning_rules():
    rules = set()
    if not os.path.isfile(bazel_star):
        return rules
    try:
        content = open(bazel_star, encoding="utf-8").read()
    except OSError:
        return rules
    for m in re.finditer(
        r'_make_bazel_lint_check\(\s*"([^"]+)"([^)]*)\)', content, re.S
    ):
        if "unchanged_lines_warning_only" in m.group(2) or (
            "_ENFORCEMENT_UNCHANGED_LINES_WARNING_ONLY" in m.group(2)
        ):
            rules.add(m.group(1))
    return rules


UNCHANGED_WARNING_RULES = load_unchanged_warning_rules()
changed_lines_cache = {}


def get_changed_lines(rel_path):
    if rel_path in changed_lines_cache:
        return changed_lines_cache[rel_path]
    if not git_ok(["cat-file", "-e", f"{change_base}:{rel_path}"]):
        changed_lines_cache[rel_path] = None  # Newly added file: all lines changed.
        return None
    changed = set()
    for raw in git_lines(["diff", "-U0", change_base, "--", rel_path]):
        m = HUNK_RE.match(raw)
        if not m:
            continue
        start = int(m.group(1))
        count = 1 if m.group(2) is None else int(m.group(2))
        if count > 0:
            changed.update(range(start, start + count))
    changed_lines_cache[rel_path] = changed
    return changed


# Read ignored directory prefixes from shac.textproto so bazel_shac_lint matches shac.
def load_shac_ignored_prefixes():
    prefixes = ["third_party/bazel_vendor/", "third_party/rust_crates/vendor/"]
    shac_cfg = os.path.join(workdir, "shac.textproto")
    if os.path.isfile(shac_cfg):
        try:
            with open(shac_cfg, encoding="utf-8") as cf:
                for raw_line in cf:
                    line = raw_line.split("#", 1)[0].strip()
                    if line.startswith("ignore:"):
                        val = line.split(":", 1)[1].strip().strip('"').strip("/")
                        if val:
                            prefixes.append(val + "/")
        except OSError:
            pass
    return tuple(sorted(set(prefixes)))


SHAC_IGNORED_PREFIXES = load_shac_ignored_prefixes()


def is_bazel_file(path):
    base = os.path.basename(path)
    # shac skips paths listed in shac.textproto ignore entries, vendored Bazel
    # repositories, upstream third-party source trees, and symlinks.
    if path.startswith(SHAC_IGNORED_PREFIXES):
        return False
    if path.startswith("third_party/") and "/src/" in path:
        return False
    full = os.path.join(workdir, path)
    if os.path.islink(full):
        return False
    return base in ("BUILD.bazel", "BUILD") or base.endswith(".bzl") or base.endswith(".bazel")


def changed_bazel_files():
    changed = set(git_lines(["diff", "--name-only", "--diff-filter=AMR", change_base]))
    changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
    return sorted(
        p for p in changed if is_bazel_file(p) and os.path.isfile(os.path.join(workdir, p))
    )


# Discover the rule names main.py registers (its --check choices).
def registered_rules():
    sys.path.insert(0, lint_dir)
    try:
        import importlib

        importlib.import_module("checks")
        from framework import REGISTRY

        return sorted(REGISTRY.keys())
    except Exception:
        return []


# Run one rule on the files; returns main.py's JSON findings (empty on tool error).
def run_rule(rule, files):
    try:
        res = subprocess.run(
            [sys.executable, main_py, "--check", rule, "--root", workdir, "--json"] + files,
            cwd=workdir, capture_output=True, text=True, timeout=120,
        )
    except Exception:
        return []
    if res.returncode not in (0, 42):
        return []
    try:
        data = json.loads(res.stdout or "[]")
    except ValueError:
        return []
    return data if isinstance(data, list) else []


findings = []
files = changed_bazel_files()


# main.py rejects a whole invocation when one file has a kind the rule does not
# apply to, so run each rule separately on .bzl files, BUILD files and others.
def file_kind(path):
    base = os.path.basename(path)
    if base.endswith(".bzl"):
        return "bzl"
    return "build" if base in ("BUILD.bazel", "BUILD") else "other"


groups = {}
for p in files:
    groups.setdefault(file_kind(p), []).append(p)

if files and os.path.isfile(main_py):
    for rule in registered_rules():
        rule_findings = []
        for group in groups.values():
            rule_findings.extend(run_rule(rule, group))
        for f in rule_findings:
            level = f.get("level", "error")
            if level not in ("error", "warning"):
                continue
            filepath = f.get("filepath", "")
            raw_line = f.get("line")
            if (
                rule in UNCHANGED_WARNING_RULES
                and level == "error"
                and raw_line
                and filepath
            ):
                changed_set = get_changed_lines(filepath)
                if changed_set is not None:
                    end_ln = f.get("end_line") or raw_line
                    if not any(
                        ln in changed_set for ln in range(raw_line, end_ln + 1)
                    ):
                        level = "info"
            message = f.get("message", "")
            if f.get("url"):
                message += f" See {f['url']}."
            findings.append({
                "source": "bazel_shac_lint",
                "category": rule,
                "severity": level,
                "file": filepath,
                "line": raw_line or 1,
                "message": f"[shac {rule}] {message}",
                "remediation": REMEDIATION_BY_RULE.get(rule, DEFAULT_REMEDIATION),
            })

print(json.dumps(findings, indent=2))
PYEOF
