#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# idk_attributes: Checks that idk_cc_* targets set api_area, and correctly map deps/no_headers.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import re
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = re.split(r"[\s,]+", sys.argv[2].strip().strip("/"))
findings = []

def _emit(category, severity, file, line, message, remediation):
    findings.append({
        "source": "idk_attributes",
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
        "remediation": remediation,
    })

for tdir in target_dirs:
    if not tdir:
        continue
    bz_rel = f"{tdir}/BUILD.bazel"
    bz_abs = os.path.join(workdir, bz_rel)
    if not os.path.isfile(bz_abs):
        continue
    try:
        src = open(bz_abs, encoding="utf-8").read()
        tree = ast.parse(src, filename=bz_rel)
    except Exception:
        continue
        
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id in ("idk_cc_shared_library", "idk_cc_shared_library_zx", "idk_cc_static_library", "idk_cc_source_library"):
                kwargs = {kw.arg: kw.value for kw in node.keywords if kw.arg}
                name_node = kwargs.get("name")
                tname = name_node.value if isinstance(name_node, ast.Constant) else "?"
                
                # Check api_area
                if "api_area" not in kwargs:
                    _emit(
                        "missing_api_area",
                        "ERROR",
                        bz_rel,
                        node.lineno,
                        f"Target '{tname}' ({node.func.id}) is missing the mandatory `api_area` attribute.",
                        f"Set `api_area` on '{tname}'. If the original GN target did not have `sdk_area`, use `api_area = \"Unknown\"`."
                    )
                
                # Check deps vs implementation_deps for no_headers=True
                no_headers = False
                nh_node = kwargs.get("no_headers")
                if nh_node and isinstance(nh_node, ast.Constant) and nh_node.value is True:
                    no_headers = True
                    
                if no_headers and "deps" in kwargs:
                    _emit(
                        "no_headers_has_deps",
                        "ERROR",
                        bz_rel,
                        node.lineno,
                        f"Target '{tname}' has `no_headers = True` but uses `deps` (which bazel2gn maps to GN `public_deps`). GN asserts that `public_deps` must be empty when `no_headers = true`.",
                        f"Move compilation dependencies from `deps` to `implementation_deps` (which bazel2gn maps to GN `deps`). If any of these dependencies were originally in GN's `runtime_deps` (e.g. `//sdk/lib/fdio`), you must also add them to Bazel's `runtime_deps = [...]` AND use `# @bazel2gn:raw_attribute:runtime_deps = [...]` above the target so they appear correctly in the generated GN file."
                    )

# Check GN runtime_deps parity: every label in an origin/main GN target's
# `runtime_deps` must still be in the generated GN target's `runtime_deps`,
# and should not silently become a link/compile dep instead.
import subprocess

_GN_BLOCK_RE = re.compile(r'^\s*(\w+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)


def _gn_blocks(text):
    """Returns {target_name: block_body} for top-level GN target calls."""
    blocks = {}
    for m in _GN_BLOCK_RE.finditer(text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        blocks.setdefault(m.group(2), text[m.end():i - 1])
    return blocks


def _gn_list(body, attr):
    """Returns the quoted labels assigned or appended to `attr` in a GN body."""
    labels = set()
    pat = re.compile(r'(?<![\w.])' + attr + r'\s*\+?=\s*\[(.*?)\]', re.S)
    for m in pat.finditer(re.sub(r'#[^\n]*', '', body)):
        labels.update(re.findall(r'"([^"]+)"', m.group(1)))
    return labels


def _origin_gn(rel):
    try:
        return subprocess.run(
            ["git", "-C", workdir, "show", f"origin/main:{rel}"],
            capture_output=True, text=True, check=True).stdout
    except Exception:
        return ""


RUNTIME_DEPS_REMEDIATION = """\
GN `runtime_deps` only ships a library at runtime; it does not link or compile
against it. Carry every original label over unchanged with
`# @bazel2gn:raw_attribute:runtime_deps = [...]` above the Bazel target (list
all of them, not a subset), then rerun `fx bazel2gn`. Only add the label to
`deps`/`implementation_deps` too when a source file of this target #includes
its headers directly; otherwise remove it from the link deps."""

for tdir in target_dirs:
    if not tdir:
        continue
    gn_rel = f"{tdir}/BUILD.gn"
    gn_abs = os.path.join(workdir, gn_rel)
    if not os.path.isfile(gn_abs) or not os.path.isfile(
            os.path.join(workdir, tdir, "BUILD.bazel")):
        continue
    old_blocks = _gn_blocks(_origin_gn(gn_rel))
    new_blocks = _gn_blocks(open(gn_abs, encoding="utf-8").read())
    for tname, old_body in old_blocks.items():
        old_rt = _gn_list(old_body, "runtime_deps")
        if not old_rt or tname not in new_blocks:
            continue
        new_body = new_blocks[tname]
        new_rt = _gn_list(new_body, "runtime_deps")
        old_link = _gn_list(old_body, "deps") | _gn_list(old_body, "public_deps")
        new_link = _gn_list(new_body, "deps") | _gn_list(new_body, "public_deps")
        dropped = sorted(old_rt - new_rt)
        if dropped:
            _emit("runtime_deps_dropped", "ERROR", gn_rel, 0,
                  f"GN target '{tname}' lost runtime_deps {dropped} from origin/main.",
                  RUNTIME_DEPS_REMEDIATION)
        promoted = sorted((old_rt & new_link) - old_link)
        if promoted:
            _emit("runtime_dep_promoted_to_link_dep", "WARNING", gn_rel, 0,
                  f"GN target '{tname}' now links {promoted}, which origin/main had "
                  "only in runtime_deps.",
                  RUNTIME_DEPS_REMEDIATION)

print(json.dumps(findings, indent=2))
PYEOF
