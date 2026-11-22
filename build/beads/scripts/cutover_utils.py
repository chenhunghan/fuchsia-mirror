# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Detects GN-to-Bazel package cutovers and compares the two packages.

A cutover is the change that moves a Fuchsia package from a GN
`fuchsia_package` to a Bazel `fx_package`. This module holds the parts of the
cutover check that need only the Python standard library, so that Planter
check scripts can import it directly:

  * the cutover detector (`detect_cutovers`),
  * package facts: the blob set, `meta.far` contents, decoded `.cm` files and
    ELF facts of a built package (`package_facts`),
  * the package content comparison (`compare_facts`),
  * the normalization engine (`load_normalizations`, `apply_normalizations`),
  * Planter findings (`findings_from_diffs`), and
  * the phase-1 record I/O (`record_path`, `write_record`, `read_record`).

The build command comparison needs the in-tree `compare_utils` and
lives in `//build/beads/verifications/verify_package_cutover.py`.
"""

import ast
import dataclasses
import difflib
import hashlib
import json
import os
import platform
import re
import shutil
import struct
import subprocess
import time
import typing as T

# Version 2 adds the GN package facts and build commands (`gn_facts`), so the
# final phase can compare the Bazel package with the GN package again.
RECORD_SCHEMA_VERSION = 2
# Version 1 records are still read; the final phase replays them.
READABLE_RECORD_SCHEMA_VERSIONS = (1, 2)
# When set, the only directory phase-1 records are read from and written to.
# Planter sets it to a per-task directory that it keeps in the task's ledger,
# so a record survives moving the task to another checkout. Use an absolute
# path: a relative one resolves against the checkout the check runs in.
RECORD_DIR_ENV = "PLANTER_CUTOVER_RECORD_DIR"

BAZEL_PACKAGE_RULES = (
    "fx_package",
    "fx_test_package",
    "fx_unittest_package",
    "fuchsia_package",
    "fuchsia_test_package",
    "fuchsia_unittest_package",
)
# Macros whose package is built by an inner target: `<name><suffix>`.
INNER_PACKAGE_SUFFIXES = {
    "fx_unittest_package": ".package",
    # @rules_fuchsia: the package is `<name>_fuchsia_package`; `<name>` is
    # the package's tasks target.
    "fuchsia_package": "_fuchsia_package",
    "fuchsia_test_package": "_fuchsia_package",
    "fuchsia_unittest_package": "_fuchsia_package",
}
GN_PACKAGE_TEMPLATES = (
    "fuchsia_package",
    "fuchsia_test_package",
    "fuchsia_package_with_single_component",
    "fuchsia_unittest_package",
    "fuchsia_system_package",
    "fuchsia_shell_package",
    "fuchsia_driver_package",
    "fuchsia_fuzzer_package",
    "ctf_fuchsia_package",
)
# GN templates that re-export a Bazel package (`bazel_target`) under a GN
# label.
GN_BRIDGE_TEMPLATES = (
    "bazel_fuchsia_package",
    "bazel_fuchsia_test_package",
    "bazel_driver_package",
)
ASSEMBLY_BUILD = "bundles/assembly/BUILD.bazel"
BAZEL_INPUTS_PREFIX = "bundles/assembly/bazel_inputs/"

# Default relative tolerance for the code size comparison, and the smallest
# absolute change (bytes) worth reporting.
TEXT_SIZE_TOLERANCE = 0.10
TEXT_SIZE_MIN_DELTA = 4096

# What each difference kind is, and how to fix it. What it can do on a device
# and its escalation stage live in //build/beads/verifications/cutover_categories.json.
DIFF_KINDS: dict[str, tuple[str, str]] = {
    # kind: (summary, fix hint)
    "package_name": (
        "the package name differs",
        "set `package_name` on the Bazel package to the GN package name",
    ),
    "missing_blob": (
        "a file in the GN package is missing from the Bazel package",
        "add the missing resource, binary or component to the Bazel package",
    ),
    "extra_blob": (
        "the Bazel package has a file the GN package does not",
        "remove the extra file, or explain why it is needed",
    ),
    "blob_content": (
        "a non-binary file has different content",
        "point the Bazel resource at the same source file as GN",
    ),
    "cm": (
        "the compiled component manifest differs",
        "compare the `.cml` sources and `includes` (shards) of the two manifests",
    ),
    "config": (
        "the structured config values differ",
        "use the same config values file or `values` as GN",
    ),
    "abi_revision": (
        "the ABI revision differs",
        "build the Bazel package at the same API level as GN",
    ),
    "subpackages": (
        "the subpackage set differs",
        "list the same subpackages in the Bazel package",
    ),
    "elf_arch": (
        "the ELF machine type differs",
        "build the binary for the same CPU as GN",
    ),
    "elf_needed": (
        "the DT_NEEDED entries differ",
        "match the GN deps and linkage (static vs shared) in Bazel",
    ),
    "elf_soname": (
        "the SONAME differs",
        "set the same output name as GN",
    ),
    "elf_interp": (
        "the program interpreter differs",
        "match the GN toolchain and variant",
    ),
    "elf_stripped": (
        "only one of the binaries is stripped",
        "ship the stripped binary, like GN",
    ),
    "elf_sections": (
        "the ELF section names differ",
        "compare the compile and link flags of the two binaries",
    ),
    "elf_relro": (
        "RELRO differs",
        "match the GN link flags (`-z relro`)",
    ),
    "elf_stack": (
        "the PT_GNU_STACK flags differ",
        "match the GN link flags (`-z noexecstack`)",
    ),
    "elf_dynsym": (
        "the exported dynamic symbols differ",
        "match the GN visibility and export flags",
    ),
    "elf_text_size": (
        "the executable code size differs beyond the tolerance",
        "compare the compile and link flags (build command comparison findings)",
    ),
    "cmd_flags": (
        "the compile or link flags differ",
        "map the GN configs to Bazel `build_flags` or attributes",
    ),
    "dylib_renamed": (
        "a shared library ships under a different name",
        "give the Bazel library the GN output name, or confirm that every user moved to the new name",
    ),
    # The Bazel package changed after phase 1 and now differs in ways phase 1
    # did not report (or, for version 1 records, which cannot be compared
    # again, it changed at all).
    "guard": (
        "the Bazel package changed after phase 1 was verified",
        "revert the Bazel input changes made after phase 1, or rerun phase 1",
    ),
}

# Names of the comparisons, as used in normalization entries and messages.
COMPARISONS = {
    "package_contents": "package content comparison",
    "build_commands": "build command comparison",
    "assembly": "assembly comparison",
}

CATEGORIES_JSON = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "verifications",
    "cutover_categories.json",
)
_categories_cache: dict[str, dict[str, dict[str, T.Any]]] = {}


def load_categories(path: str = CATEGORIES_JSON) -> dict[str, dict[str, T.Any]]:
    """Returns {category: {"stage": 0|1|2, "device_impact": str}}.

    Stage 0 and 1 findings are INFO (the Planter harness reads stage 1
    itself); stage 2 categories are emitted as WARNING.
    """
    if path not in _categories_cache:
        with open(path, encoding="utf-8") as f:
            _categories_cache[path] = json.load(f)
    return _categories_cache[path]


def device_impact(category: str) -> str:
    return str(
        load_categories().get(category, {}).get("device_impact", "unknown")
    )


def category_severity(category: str) -> str:
    return (
        "WARNING"
        if load_categories().get(category, {}).get("stage", 0) >= 2
        else "INFO"
    )


NORMALIZATION_HINT = (
    "If the difference is expected, give it the `expected` disposition and a "
    "`proposed_normalization` for "
    "`//build/beads/verifications/cutover_normalizations.json`."
)


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------


def git(workdir: str, args: list[str]) -> str:
    """Returns the stripped stdout of a git command, or "" on failure."""
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def git_show(workdir: str, rev: str, path: str) -> T.Optional[str]:
    """Returns the content of `path` at `rev`, or None if it does not exist."""
    try:
        return subprocess.check_output(
            ["git", "-C", workdir, "show", f"{rev}:{path}"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None


def read_text(path: str) -> T.Optional[str]:
    try:
        with open(path, encoding="utf-8") as f:
            return f.read()
    except OSError:
        return None


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# ---------------------------------------------------------------------------
# BUILD file parsing
# ---------------------------------------------------------------------------


def bazel_targets(text: T.Optional[str]) -> dict[str, dict[str, T.Any]]:
    """Returns {name: {"rule": ..., <string or string-list kwargs>...}}."""
    if not text:
        return {}
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return {}
    out: dict[str, dict[str, T.Any]] = {}
    for stmt in tree.body:
        node = stmt.value if isinstance(stmt, ast.Expr) else None
        if not isinstance(node, ast.Call) or not isinstance(
            node.func, ast.Name
        ):
            continue
        attrs: dict[str, T.Any] = {"rule": node.func.id, "_line": node.lineno}
        for kw in node.keywords:
            v = kw.value
            if isinstance(v, ast.Constant) and isinstance(v.value, str):
                attrs[kw.arg or ""] = v.value
            elif isinstance(v, ast.List):
                attrs[kw.arg or ""] = [
                    e.value
                    for e in v.elts
                    if isinstance(e, ast.Constant) and isinstance(e.value, str)
                ]
        if isinstance(attrs.get("name"), str):
            out[attrs["name"]] = attrs
    return out


def _strip_gn_comments(text: str) -> str:
    return "\n".join(
        re.sub(r'#(?=(?:[^"]*"[^"]*")*[^"]*$).*', "", line)
        for line in text.splitlines()
    )


_GN_CALL = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
_GN_IDENT_RE = re.compile(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?")
_GN_VAR_TOKEN_RE = re.compile(
    r"\$\{([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?)\}|\$([A-Za-z_]\w*)|(\$)"
)


def _skip_gn_string(code: str, i: int) -> int:
    i += 1
    while i < len(code):
        if code[i] == "\\":
            i += 2
        elif code[i] == '"':
            return i + 1
        else:
            i += 1
    return i


def _skip_gn_balanced(code: str, i: int, open_ch: str, close_ch: str) -> int:
    depth = 1
    i += 1
    while i < len(code) and depth:
        ch = code[i]
        if ch == '"':
            i = _skip_gn_string(code, i)
            continue
        if ch == open_ch:
            depth += 1
        elif ch == close_ch:
            depth -= 1
        i += 1
    return i


def _gn_calls(
    text: T.Optional[str], templates: T.Container[str]
) -> T.Iterator[tuple[str, str]]:
    """Yields (target name, body) for calls of `templates` in a BUILD.gn."""
    if not text:
        return
    code = _strip_gn_comments(text)
    for m in _GN_CALL.finditer(code):
        if m.group(1) not in templates:
            continue
        end = _skip_gn_balanced(code, m.end() - 1, "{", "}")
        yield m.group(2), code[m.end() : end]


def _gn_string(body: str, name: str) -> T.Optional[str]:
    m = re.search(rf'^\s*{name}\s*=\s*"([^"$]+)"', body, re.M)
    return m.group(1) if m else None


def _eval_gn_str(
    expr: str, env: T.Mapping[str, str], inv: T.Mapping[str, str]
) -> T.Optional[str]:
    """Evaluates a GN string expression (`"..."`, `$var`, `+`, `invoker.x`)."""
    expr = expr.strip()
    if not expr:
        return None
    parts = []
    start, i, depth = 0, 0, 0
    while i < len(expr):
        ch = expr[i]
        if ch == '"':
            i = _skip_gn_string(expr, i)
            continue
        if ch in "([":
            depth += 1
        elif ch in ")]":
            depth -= 1
        elif ch == "+" and depth == 0:
            parts.append(expr[start:i].strip())
            start = i + 1
        i += 1
    parts.append(expr[start:].strip())
    evaluated = []
    for part in parts:
        if not part:
            return None
        if part.startswith('"') and part.endswith('"') and len(part) >= 2:
            resolved = []
            pos = 0
            raw = part[1:-1]
            for m in _GN_VAR_TOKEN_RE.finditer(raw):
                if m.group(3):
                    return None
                resolved.append(raw[pos : m.start()])
                var = m.group(1) or m.group(2)
                val = (
                    inv.get(var[8:])
                    if var.startswith("invoker.")
                    else env.get(var)
                )
                if val is None:
                    return None
                resolved.append(val)
                pos = m.end()
            resolved.append(raw[pos:])
            evaluated.append("".join(resolved))
        elif re.fullmatch(r"invoker\.[A-Za-z_]\w*", part):
            val = inv.get(part[8:])
            if val is None:
                return None
            evaluated.append(val)
        elif re.fullmatch(r"[A-Za-z_]\w*", part):
            val = env.get(part)
            if val is None:
                return None
            evaluated.append(val)
        else:
            return None
    return "".join(evaluated)


def _walk_gn_block(
    code: str,
    env: dict[str, str],
    inv: T.Mapping[str, str],
    packages: list[tuple[str, str]],
) -> tuple[bool, set[str]]:
    """Evaluates unconditional statements in a GN block.

    Appends `(package_name, target_name)` for each unconditional package rule
    whose target and `package_name` resolve statically, and returns
    `(forwards_package_name, assigned_vars)`.
    """
    forwards_pkg_name = False
    assigned: set[str] = set()
    i, n = 0, len(code)
    while i < n:
        while i < n and code[i].isspace():
            i += 1
        if i >= n:
            break
        m = _GN_IDENT_RE.match(code, i)
        if not m:
            i += 1
            continue
        ident = m.group(0)
        j = m.end()
        while j < n and code[j] in " \t":
            j += 1
        if j < n and (
            code[j : j + 2] in ("+=", "-=")
            or (code[j] == "=" and code[j : j + 2] != "==")
        ):
            op = code[j : j + 2] if code[j : j + 2] in ("+=", "-=") else "="
            k = j + len(op)
            rhs_start = k
            last_non_ws = "="
            while k < n:
                ch = code[k]
                if ch == '"':
                    k = _skip_gn_string(code, k)
                    last_non_ws = '"'
                    continue
                if ch == "(":
                    k = _skip_gn_balanced(code, k, "(", ")")
                    last_non_ws = ")"
                    continue
                if ch == "[":
                    k = _skip_gn_balanced(code, k, "[", "]")
                    last_non_ws = "]"
                    continue
                if ch == "{":
                    k = _skip_gn_balanced(code, k, "{", "}")
                    last_non_ws = "}"
                    continue
                if ch == "\n":
                    if last_non_ws not in ("=", "+"):
                        k += 1
                        break
                elif not ch.isspace():
                    last_non_ws = ch
                k += 1
            assigned.add(ident)
            if "." not in ident:
                val = (
                    _eval_gn_str(code[rhs_start:k], env, inv)
                    if op == "="
                    else None
                )
                if val is not None:
                    env[ident] = val
                else:
                    env.pop(ident, None)
            i = k
            continue
        k = j
        while k < n and code[k].isspace():
            k += 1
        if k < n and code[k] == "(":
            end_paren = _skip_gn_balanced(code, k, "(", ")")
            arg_expr = code[k + 1 : end_paren - 1].strip()
            b = end_paren
            while b < n and code[b].isspace():
                b += 1
            if ident == "if" and b < n and code[b] == "{":
                branches: list[tuple[str, str]] = []
                end_blk = _skip_gn_balanced(code, b, "{", "}")
                branches.append((arg_expr, code[b + 1 : end_blk - 1]))
                cur = end_blk
                while True:
                    tmp = cur
                    while tmp < n and code[tmp].isspace():
                        tmp += 1
                    em = re.match(r"else\b", code[tmp:])
                    if not em:
                        break
                    tmp += em.end()
                    while tmp < n and code[tmp].isspace():
                        tmp += 1
                    im = re.match(r"if\b", code[tmp:])
                    if im:
                        tmp += im.end()
                        while tmp < n and code[tmp].isspace():
                            tmp += 1
                        if tmp < n and code[tmp] == "(":
                            ep = _skip_gn_balanced(code, tmp, "(", ")")
                            cond = code[tmp + 1 : ep - 1].strip()
                            tmp = ep
                            while tmp < n and code[tmp].isspace():
                                tmp += 1
                            if tmp < n and code[tmp] == "{":
                                eb = _skip_gn_balanced(code, tmp, "{", "}")
                                branches.append((cond, code[tmp + 1 : eb - 1]))
                                cur = eb
                                continue
                    if tmp < n and code[tmp] == "{":
                        eb = _skip_gn_balanced(code, tmp, "{", "}")
                        branches.append(("", code[tmp + 1 : eb - 1]))
                        cur = eb
                        break
                    break
                for cond, blk in branches:
                    if cond == "is_fuchsia":
                        _, sub_assigned = _walk_gn_block(
                            blk, env, inv, packages
                        )
                        assigned |= sub_assigned
                    else:
                        for am in re.finditer(
                            r"\b([A-Za-z_]\w*)\s*[+-]?=(?!=)", blk
                        ):
                            assigned.add(am.group(1))
                            env.pop(am.group(1), None)
                i = cur
                continue
            if b < n and code[b] == "{":
                end_blk = _skip_gn_balanced(code, b, "{", "}")
                body = code[b + 1 : end_blk - 1]
                if ident == "foreach":
                    for am in re.finditer(
                        r"\b([A-Za-z_]\w*)\s*[+-]?=(?!=)", body
                    ):
                        assigned.add(am.group(1))
                        env.pop(am.group(1), None)
                elif ident in GN_PACKAGE_TEMPLATES:
                    target = _eval_gn_str(arg_expr, env, inv)
                    if target:
                        call_env = dict(env)
                        fwd, call_assigned = _walk_gn_block(
                            body, call_env, inv, []
                        )
                        if "package_name" in call_assigned:
                            pkg_name = call_env.get("package_name")
                        elif fwd:
                            pkg_name = (
                                None
                                if "__non_literal_package_name__" in inv
                                else inv.get("package_name") or target
                            )
                        else:
                            pkg_name = target
                        if pkg_name:
                            packages.append((pkg_name, target))
                i = end_blk
                continue
            if ident == "forward_variables_from" and re.match(
                r"invoker\s*,", arg_expr
            ):
                rest = arg_expr.split(",", 1)[1].strip()
                if rest.startswith('"*"'):
                    excl = rest[3:].strip()
                    if not (excl.startswith(",") and '"package_name"' in excl):
                        forwards_pkg_name = True
                elif '"package_name"' in rest:
                    forwards_pkg_name = True
            i = end_paren
            continue
        i = j
    return forwards_pkg_name, assigned


def _template_packages(
    template_body: str, inv_target: str, inv_body: str
) -> list[tuple[str, str]]:
    """`(package_name, target_name)` pairs defined by one template invocation."""
    inv_env: dict[str, str] = {}
    _, inv_assigned = _walk_gn_block(inv_body, inv_env, {}, [])
    inv = {k: inv_env[k] for k in inv_assigned if k in inv_env}
    if "package_name" in inv_assigned and "package_name" not in inv_env:
        inv["__non_literal_package_name__"] = ""
    packages: list[tuple[str, str]] = []
    _walk_gn_block(template_body, {"target_name": inv_target}, inv, packages)
    return packages


def gn_packages(text: T.Optional[str]) -> dict[str, list[str]]:
    """Returns {package_name: [target names]} for GN package targets.

    Several targets can build packages with the same name (e.g. a debug and a
    release variant). Invocations of a same-file `template()` expand the
    unconditional package targets in that template's body (with `target_name`
    and string `invoker` bindings substituted).
    """
    if not text:
        return {}
    code = _strip_gn_comments(text)
    templates: dict[str, str] = {}
    masked = list(code)
    for m in _GN_CALL.finditer(code):
        if m.group(1) != "template":
            continue
        end = _skip_gn_balanced(code, m.end() - 1, "{", "}")
        templates[m.group(2)] = code[m.end() : end - 1]
        for idx in range(m.start(), end):
            if masked[idx] != "\n":
                masked[idx] = " "
    outer = "".join(masked)
    out: dict[str, list[str]] = {}
    for target, body in _gn_calls(outer, GN_PACKAGE_TEMPLATES):
        out.setdefault(_gn_string(body, "package_name") or target, []).append(
            target
        )
    pkg_call_re = re.compile(
        r"\b(?:" + "|".join(map(re.escape, GN_PACKAGE_TEMPLATES)) + r")\s*\("
    )
    pkg_templates = {
        name: body
        for name, body in templates.items()
        if pkg_call_re.search(body)
    }
    if pkg_templates:
        for m in _GN_CALL.finditer(outer):
            tbody = pkg_templates.get(m.group(1))
            if tbody is None:
                continue
            end = _skip_gn_balanced(outer, m.end() - 1, "{", "}")
            inv_body = outer[m.end() : end - 1]
            for pkg_name, pkg_target in _template_packages(
                tbody, m.group(2), inv_body
            ):
                out.setdefault(pkg_name, []).append(pkg_target)
    return out


def gn_bridges(text: T.Optional[str]) -> dict[str, str]:
    """Returns {GN target: Bazel target name} for package bridges.

    A `bazel_fuchsia_package("x")` bridge (or another GN_BRIDGE_TEMPLATES
    template) makes the Bazel package `bazel_target` (default `:x`)
    available to GN under the label `:x`.
    """
    out = {}
    for target, body in _gn_calls(text, GN_BRIDGE_TEMPLATES):
        bazel = _gn_string(body, "bazel_target") or f":{target}"
        out[target] = bazel.split(":")[-1]
    return out


_SENTINEL_RE = re.compile(r"^[ \t]*## BAZEL2GN SENTINEL", re.M)


def gn_package_state(
    text: T.Optional[str], target: str, package_name: str
) -> str:
    """Returns where the GN package `target` lives in a BUILD.gn.

    "hand" when it is hand-written (above the bazel2gn sentinel), "mirror"
    when only a bazel2gn-generated copy exists below the sentinel (the same
    target, or a `<package_name>-gn` package), "bridge" when a
    `bazel_fuchsia_package` with the same target name replaced it (the
    package is cut over; GN only re-exports the Bazel package), "gone"
    otherwise.
    """
    text = text or ""
    m = _SENTINEL_RE.search(text)
    above, below = (text[: m.start()], text[m.start() :]) if m else (text, "")
    if any(target in t for t in gn_packages(above).values()):
        return "hand"
    mirrors = gn_packages(below)
    if (
        any(target in t for t in mirrors.values())
        or f"{package_name}-gn" in mirrors
    ):
        return "mirror"
    if target in gn_bridges(text):
        return "bridge"
    return "gone"


def strong_gn_match(
    bazel_name: str, candidates: T.Sequence[str], bridges: T.Mapping[str, str]
) -> str:
    """The candidate with the same target name, or the one bridged to it."""
    if bazel_name in candidates:
        return bazel_name
    bridged = [t for t in candidates if bridges.get(t) == bazel_name]
    return bridged[0] if len(bridged) == 1 else ""


def pair_gn_package(
    bazel_name: str,
    candidates: T.Sequence[str],
    bridges: T.Mapping[str, str],
    taken: T.Container[str] = (),
) -> tuple[str, list[str]]:
    """Picks the GN package target a new Bazel package `bazel_name` replaces.

    `candidates` are the GN package targets with the Bazel package's
    `package_name`. A candidate with the same target name wins, then one
    that a bridge now maps to `bazel_name`, then a unique candidate not
    `taken` by another Bazel package. Returns (target, []) or, when the
    GN package cannot be paired uniquely, ("", list(candidates)).
    """
    strong = strong_gn_match(bazel_name, candidates, bridges)
    if strong and strong not in taken:
        return strong, []
    if len(candidates) == 1 and candidates[0] not in taken:
        return candidates[0], []
    return "", list(candidates)


def bazel_package_name(attrs: dict[str, T.Any]) -> str:
    pn = attrs.get("package_name")
    return pn if isinstance(pn, str) and pn else str(attrs["name"])


def bazel_package_target(label: str, rule: str) -> str:
    """The label of the target that builds the package of `rule` `label`."""
    return label + INNER_PACKAGE_SUFFIXES.get(rule, "")


def resolve_label(label: str, pkg_dir: str) -> str:
    """Resolves `:name` or `//dir:name` (or `//dir`) to `//dir:name`."""
    label = label.lstrip("@")
    if label.startswith(":"):
        return f"//{pkg_dir}{label}"
    if label.startswith("//") and ":" not in label:
        return f"{label}:{label.rsplit('/', 1)[-1]}"
    return label


def label_dir(label: str) -> str:
    return label.lstrip("@").lstrip("/").split(":", 1)[0]


# ---------------------------------------------------------------------------
# Cutover detector
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Cutover:
    """One detected package cutover."""

    package_name: str
    gn_label: str
    bazel_label: str
    # Where the GN package lives in the working tree: "hand" (still
    # hand-written: parallel state), "mirror" (only a bazel2gn copy remains:
    # the package stays dual-built), "bridge" (replaced by a
    # `bazel_fuchsia_package` with the same label: final state), "gone"
    # (deleted: final state) or "ambiguous" (several GN packages could be
    # the one replaced; see `ambiguous`).
    gn_state: str
    triggers: list[str] = dataclasses.field(default_factory=list)
    # The target that builds the package; differs from `bazel_label` for
    # macros such as `fx_unittest_package` (`<name>.package`).
    bazel_package_label: str = ""
    # With gn_state "ambiguous": the GN package labels it could replace.
    ambiguous: list[str] = dataclasses.field(default_factory=list)
    # The `[<key>:...]` prefix of findings. The package name, plus
    # `--<Bazel target>` when two cutovers in the change share a name.
    key: str = ""

    def __post_init__(self) -> None:
        self.bazel_package_label = self.bazel_package_label or self.bazel_label
        self.key = self.key or self.package_name

    @property
    def bazel_build_file(self) -> str:
        return f"{label_dir(self.bazel_label)}/BUILD.bazel"

    @property
    def bazel_target(self) -> str:
        return self.bazel_label.split(":")[-1]

    @property
    def gn_exists(self) -> bool:
        return self.gn_state not in ("gone", "bridge")

    @property
    def state(self) -> str:
        """The phase to run without an explicit phase.

        A package counts as cut over once its GN package is gone or the
        assembly points at the Bazel label. Otherwise it is only dual-built
        (e.g. a migrated test package next to its bazel2gn mirror) and is not
        compared, unless Planter asks for the parallel phase.
        """
        if self.gn_state == "ambiguous":
            return "ambiguous"
        if not self.gn_exists or "assembly_label_replaced" in self.triggers:
            return "final"
        return "dual_build"


def detect_cutovers(
    workdir: str,
    base: str,
    changed: T.Sequence[str],
    head: T.Optional[str] = None,
) -> list[Cutover]:
    """Detects package cutovers in the change from `base` to the working tree.

    A cutover is either:
      * an added Bazel package rule whose package name matches a GN package
        target that exists at `base` in the same directory, or
      * a `bazel_inputs/...` label in `//bundles/assembly/BUILD.bazel`
        replaced with a direct Bazel label.

    With `head`, files are read at that revision instead of the working tree.
    """
    found: dict[str, Cutover] = {}

    def now(path: str) -> T.Optional[str]:
        if head:
            return git_show(workdir, head, path)
        return read_text(os.path.join(workdir, path))

    def gn_state_now(gn_dir: str, gn_name: str, package_name: str) -> str:
        return gn_package_state(
            now(f"{gn_dir}/BUILD.gn"), gn_name, package_name
        )

    for path in changed:
        if os.path.basename(path) != "BUILD.bazel":
            continue
        pkg_dir = os.path.dirname(path)
        if pkg_dir.startswith(BAZEL_INPUTS_PREFIX):
            continue
        head_targets = bazel_targets(now(path))
        old = bazel_targets(git_show(workdir, base, path))
        base_gn = gn_packages(git_show(workdir, base, f"{pkg_dir}/BUILD.gn"))
        bridges = gn_bridges(now(f"{pkg_dir}/BUILD.gn"))
        new_packages = {
            name: attrs
            for name, attrs in head_targets.items()
            if attrs["rule"] in BAZEL_PACKAGE_RULES
            and name not in old
            and bazel_package_name(attrs) in base_gn
        }
        # GN packages that a Bazel package claims by target name (or by a
        # bridge) are not candidates for the other Bazel packages.
        claims = {
            n: strong_gn_match(n, base_gn[bazel_package_name(a)], bridges)
            for n, a in new_packages.items()
        }
        for name, attrs in new_packages.items():
            pkg_name = bazel_package_name(attrs)
            gn_name, ambiguous = pair_gn_package(
                name,
                base_gn[pkg_name],
                bridges,
                {t for n, t in claims.items() if n != name and t},
            )
            label = f"//{pkg_dir}:{name}"
            found[label] = Cutover(
                package_name=pkg_name,
                gn_label=f"//{pkg_dir}:{gn_name}" if gn_name else "",
                bazel_label=label,
                gn_state=(
                    gn_state_now(pkg_dir, gn_name, pkg_name)
                    if gn_name
                    else "ambiguous"
                ),
                triggers=["bazel_package_added"],
                bazel_package_label=bazel_package_target(label, attrs["rule"]),
                ambiguous=[f"//{pkg_dir}:{t}" for t in ambiguous],
            )

    if ASSEMBLY_BUILD in changed:
        revs = [base, head] if head else [base]
        diff = git(workdir, ["diff", "-U0"] + revs + ["--", ASSEMBLY_BUILD])
        removed = re.findall(
            r'^-.*"//bundles/assembly/bazel_inputs/([^":]+):([^"]+)"',
            diff,
            re.M,
        )
        added = re.findall(r'^\+.*"//([^":]+):([^"]+)"', diff, re.M)
        for d, _ in removed:
            for ad, aname in added:
                if ad != d:
                    continue
                label = f"//{ad}:{aname}"
                bridge = git_show(
                    workdir, base, f"{BAZEL_INPUTS_PREFIX}{d}/BUILD.gn"
                )
                m = re.search(r'package\s*=\s*"([^"]+)"', bridge or "")
                gn_label = resolve_label(m.group(1), d) if m else ""
                if label in found:
                    found[label].triggers.append("assembly_label_replaced")
                    continue
                new_attrs = bazel_targets(now(f"{ad}/BUILD.bazel")).get(aname)
                if not new_attrs or not gn_label:
                    continue
                gn_dir, gn_name = label_dir(gn_label), gn_label.split(":")[-1]
                found[label] = Cutover(
                    package_name=bazel_package_name(new_attrs),
                    gn_label=gn_label,
                    bazel_label=label,
                    gn_state=gn_state_now(
                        gn_dir, gn_name, bazel_package_name(new_attrs)
                    ),
                    triggers=["assembly_label_replaced"],
                    bazel_package_label=bazel_package_target(
                        label, new_attrs["rule"]
                    ),
                )
    cutovers = sorted(found.values(), key=lambda c: c.bazel_label)
    names = [c.package_name for c in cutovers]
    for c in cutovers:
        if names.count(c.package_name) > 1:
            c.key = f"{c.package_name}--{c.bazel_target}"
    return cutovers


@dataclasses.dataclass(frozen=True)
class UnpairedGnPackage:
    """A GN package at the change base that the change deletes or bridges
    without a detected Bazel package replacing it."""

    package_name: str
    gn_label: str
    # "gone" (deleted) or "bridge" (re-exports a Bazel package the change
    # did not add, so the check could not pair it).
    gn_state: str

    @property
    def gn_build_file(self) -> str:
        return f"{label_dir(self.gn_label)}/BUILD.gn"


def unpaired_gn_packages(
    workdir: str,
    base: str,
    changed: T.Sequence[str],
    cutovers: T.Sequence[Cutover],
    head: T.Optional[str] = None,
) -> list[UnpairedGnPackage]:
    """GN packages at `base` that the change deletes or bridges unpaired.

    `detect_cutovers` only pairs a GN package with a Bazel package that the
    change adds in the same directory (or that the assembly now names). A GN
    package that is deleted or replaced by a bridge otherwise (the Bazel
    package landed earlier, moved directory, was renamed, or names its
    package indirectly) is cut over without any comparison; this lists them
    so they can be reported as not compared.
    """
    paired = set()
    for c in cutovers:
        paired.add(c.gn_label)
        paired.update(c.ambiguous)
    out = []
    for path in sorted(set(changed)):
        if os.path.basename(path) != "BUILD.gn":
            continue
        base_text = git_show(workdir, base, path)
        base_gn = gn_packages(base_text)
        if not base_gn:
            continue
        if head:
            text = git_show(workdir, head, path)
        else:
            text = read_text(os.path.join(workdir, path))
        pkg_dir = os.path.dirname(path)
        for package_name, targets in sorted(base_gn.items()):
            for target in sorted(set(targets)):
                label = f"//{pkg_dir}:{target}"
                if label in paired:
                    continue
                if (
                    gn_package_state(base_text, target, package_name)
                    == "mirror"
                ):
                    # A bazel2gn copy of a Bazel package: nothing to compare.
                    continue
                state = gn_package_state(text, target, package_name)
                if state in ("gone", "bridge"):
                    out.append(UnpairedGnPackage(package_name, label, state))
    return out


# ---------------------------------------------------------------------------
# FAR archives and package facts
# ---------------------------------------------------------------------------

FAR_MAGIC = b"\xc8\xbf\x0b\x48\xad\xab\xc5\x11"


def read_far(path: str) -> dict[str, bytes]:
    """Returns {entry name: content} for a FAR archive (e.g. meta.far)."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != FAR_MAGIC:
        raise ValueError(f"{path}: not a FAR archive")
    (index_len,) = struct.unpack_from("<Q", data, 8)
    chunks = {}
    for off in range(16, 16 + index_len, 24):
        ctype, coff, clen = struct.unpack_from("<8sQQ", data, off)
        chunks[ctype] = (coff, clen)
    dir_off, dir_len = chunks[b"DIR-----"]
    names_off, _ = chunks[b"DIRNAMES"]
    out = {}
    for off in range(dir_off, dir_off + dir_len, 32):
        name_off, name_len, _, data_off, data_len, _ = struct.unpack_from(
            "<IHHQQQ", data, off
        )
        name = data[names_off + name_off : names_off + name_off + name_len]
        out[name.decode()] = data[data_off : data_off + data_len]
    return out


@dataclasses.dataclass
class Tools:
    """Host tools used to decode package contents. None when unavailable."""

    cmc: T.Optional[str] = None
    readelf: T.Optional[str] = None

    @staticmethod
    def find(workdir: str, build_dir: T.Optional[str]) -> "Tools":
        """Finds the tools through the build's `tool_paths.json`."""
        found: dict[str, str] = {}
        host_cpu = (
            "arm64" if platform.machine() in ("aarch64", "arm64") else "x64"
        )
        try:
            with open(os.path.join(build_dir or "", "tool_paths.json")) as f:
                entries = json.load(f)
        except (OSError, ValueError):
            entries = []
        for e in entries:
            if (
                e.get("name") in ("cmc", "llvm-readelf")
                and e.get("os") == "linux"
                and e.get("cpu") == host_cpu
            ):
                path = os.path.normpath(
                    os.path.join(build_dir or "", e["path"])
                )
                if os.access(path, os.X_OK):
                    found.setdefault(e["name"], path)
        return Tools(cmc=found.get("cmc"), readelf=found.get("llvm-readelf"))


def decode_cm(cmc: T.Optional[str], content: bytes, name: str) -> str:
    """Returns the `cmc debug-print-cm` text of a `.cm`, or its sha256."""
    digest = "sha256:" + hashlib.sha256(content).hexdigest()
    if not cmc:
        return digest
    import tempfile

    with tempfile.NamedTemporaryFile(suffix=".cm") as tf:
        tf.write(content)
        tf.flush()
        try:
            return subprocess.check_output(
                [cmc, "debug-print-cm", tf.name],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError):
            return digest


_LOAD_RE = re.compile(
    r"^\s*LOAD\s+0x[0-9a-f]+\s+0x[0-9a-f]+\s+0x[0-9a-f]+\s+(0x[0-9a-f]+)\s+0x[0-9a-f]+\s+([RWE ]+?)\s+0x"
)
_SECTION_RE = re.compile(
    r"^\s*\[\s*\d+\]\s+(\S*)\s+([A-Z_0-9]+|LOOS\+\S+|0x\S+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)"
)


def elf_facts(readelf: str, path: str) -> dict[str, T.Any]:
    """Returns structural facts of an ELF file from `llvm-readelf`."""
    out = subprocess.check_output(
        [readelf, "-h", "-l", "-S", "-d", "--dyn-syms", "-W", path],
        stderr=subprocess.DEVNULL,
        text=True,
        errors="replace",
    )
    facts: dict[str, T.Any] = {
        "arch": "",
        "needed": [],
        "soname": "",
        "interp": "",
        "relro": False,
        "stack": "",
        "sections": [],
        "exports": [],
        "text_size": 0,
    }
    in_syms = False
    for line in out.splitlines():
        s = line.strip()
        if s.startswith("Machine:"):
            facts["arch"] = s.split(":", 1)[1].strip()
        elif "(NEEDED)" in s:
            facts["needed"].append(s.rsplit("[", 1)[-1].rstrip("]"))
        elif "(SONAME)" in s:
            facts["soname"] = s.rsplit("[", 1)[-1].rstrip("]")
        elif "Requesting program interpreter:" in s:
            facts["interp"] = s.split(":", 1)[1].strip().rstrip("]")
        elif _LOAD_RE.match(line):
            m = _LOAD_RE.match(line)
            assert m
            if "E" in m.group(2):
                # Executable segments; section headers may be stripped.
                facts["text_size"] += int(m.group(1), 16)
        elif s.startswith("GNU_RELRO"):
            facts["relro"] = True
        elif s.startswith("GNU_STACK"):
            m = re.search(r"\s([RWE ]{1,3})\s+0x[0-9a-f]+$", s)
            facts["stack"] = m.group(1).replace(" ", "") if m else s.split()[-2]
        elif s.startswith("Symbol table '.dynsym'"):
            in_syms = True
            continue
        m = _SECTION_RE.match(line)
        if m and m.group(1):
            facts["sections"].append(m.group(1))
            continue
        if in_syms:
            cols = s.split()
            # Num: Value Size Type Bind Vis Ndx Name
            if len(cols) >= 8 and cols[0].endswith(":") and cols[0] != "Num:":
                if cols[4] in ("GLOBAL", "WEAK") and cols[6] != "UND":
                    facts["exports"].append(cols[7].split("@")[0])
            elif not s:
                in_syms = False
    facts["needed"] = sorted(set(facts["needed"]))
    facts["sections"] = sorted(set(facts["sections"]))
    facts["exports"] = sorted(set(facts["exports"]))
    # GN strips section headers from executables (`--strip-sections`).
    facts["has_sections"] = bool(facts["sections"])
    facts["stripped"] = ".symtab" not in facts["sections"] and not any(
        n.startswith(".debug_") for n in facts["sections"]
    )
    return facts


def is_elf(path: str) -> bool:
    try:
        with open(path, "rb") as f:
            return f.read(4) == b"\x7fELF"
    except OSError:
        return False


def _blob_source(
    manifest_path: str, manifest: dict[str, T.Any], src: str
) -> str:
    if os.path.isabs(src):
        return src
    if manifest.get("blob_sources_relative") == "file":
        return os.path.normpath(
            os.path.join(os.path.dirname(manifest_path), src)
        )
    return os.path.abspath(src)


META_SPECIAL = (
    "meta/contents",
    "meta/package",
    "meta/fuchsia.abi/abi-revision",
    "meta/fuchsia.pkg/subpackages",
)


def package_facts(
    manifest_path: str, tools: Tools, cwd: T.Optional[str] = None
) -> dict[str, T.Any]:
    """Collects comparable facts from a built package's package_manifest.json.

    Relative `source_path`s of manifests without
    `"blob_sources_relative": "file"` are resolved against `cwd` (the build
    directory for GN packages).
    """
    manifest_path = os.path.abspath(manifest_path)
    with open(manifest_path, encoding="utf-8") as f:
        manifest = json.load(f)
    old_cwd = os.getcwd()
    if cwd:
        os.chdir(cwd)
    try:
        blobs = {
            b["path"]: dict(
                b,
                source=_blob_source(manifest_path, manifest, b["source_path"]),
            )
            for b in manifest.get("blobs", [])
        }
    finally:
        os.chdir(old_cwd)
    missing = sorted(
        p for p, b in blobs.items() if not os.path.exists(b["source"])
    )
    if missing:
        raise ValueError(
            f"{manifest_path}: blob sources not found: {missing[:5]}"
        )
    meta = blobs.pop("meta/")
    far = read_far(meta["source"])
    facts: dict[str, T.Any] = {
        "package_name": json.loads(far.get("meta/package", b"{}")).get(
            "name", ""
        ),
        "meta_far_merkle": meta["merkle"],
        "abi_revision": far.get("meta/fuchsia.abi/abi-revision", b"").hex(),
        "subpackages": sorted(
            json.loads(far.get("meta/fuchsia.pkg/subpackages", b"{}"))
            .get("subpackages", {})
            .keys()
        ),
        "meta_listing": sorted(far),
        "blobs": {},
        "meta": {},
        "cm": {},
        "cvf": {},
        "sources": {},
    }
    for name, content in sorted(far.items()):
        if name in META_SPECIAL:
            continue
        if name.endswith(".cm"):
            facts["cm"][name] = decode_cm(tools.cmc, content, name)
        elif name.endswith(".cvf"):
            facts["cvf"][name] = hashlib.sha256(content).hexdigest()
        else:
            facts["meta"][name] = hashlib.sha256(content).hexdigest()
    for path, b in sorted(blobs.items()):
        entry: dict[str, T.Any] = {
            "merkle": b["merkle"],
            "size": b.get("size", 0),
        }
        if is_elf(b["source"]):
            entry["elf"] = (
                elf_facts(tools.readelf, b["source"]) if tools.readelf else {}
            )
        facts["blobs"][path] = entry
        facts["sources"][path] = b["source"]
    return facts


# ---------------------------------------------------------------------------
# Package content comparison
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Diff:
    """One elementary difference between the GN and Bazel packages."""

    kind: str  # A key of DIFF_KINDS.
    key: str  # Blob path, meta path or package name.
    gn: str
    bazel: str
    item: str = ""  # Sub-item: a section, library, symbol, flag or line.
    comparison: str = "package_contents"  # A key of COMPARISONS.
    # For `.cm` lines: the line as printed (sign and indentation) and the
    # top-level manifest section it belongs to, for a readable diff.
    detail: str = ""
    section: str = ""

    @property
    def category(self) -> str:
        return f"cutover_diff_{self.kind}"


def _set_diffs(
    kind: str, key: str, gn: T.Iterable[str], bazel: T.Iterable[str]
) -> list[Diff]:
    g, b = set(gn), set(bazel)
    return [
        Diff(kind, key, "present", "absent", item=i) for i in sorted(g - b)
    ] + [Diff(kind, key, "absent", "present", item=i) for i in sorted(b - g)]


def compare_elf(
    key: str, g: dict[str, T.Any], b: dict[str, T.Any], tolerance: float
) -> list[Diff]:
    if not g or not b:
        return []
    diffs = []
    fields = [
        ("arch", "elf_arch"),
        ("soname", "elf_soname"),
        ("interp", "elf_interp"),
        ("relro", "elf_relro"),
        ("stack", "elf_stack"),
    ]
    # Section names (and with them strip state) are only comparable when both
    # binaries kept their section headers.
    both_sections = g.get("has_sections", True) and b.get("has_sections", True)
    if both_sections:
        fields.append(("stripped", "elf_stripped"))
    for field, kind in fields:
        if g.get(field) != b.get(field):
            diffs.append(Diff(kind, key, str(g.get(field)), str(b.get(field))))
    diffs += _set_diffs("elf_needed", key, g["needed"], b["needed"])
    if both_sections:
        diffs += _set_diffs("elf_sections", key, g["sections"], b["sections"])
    diffs += _set_diffs("elf_dynsym", key, g["exports"], b["exports"])
    gt, bt = g.get("text_size", 0), b.get("text_size", 0)
    if (
        gt
        and bt
        and abs(bt - gt) / gt > tolerance
        and abs(bt - gt) >= TEXT_SIZE_MIN_DELTA
    ):
        diffs.append(
            Diff(
                "elf_text_size",
                key,
                str(gt),
                f"{bt} ({(bt - gt) * 100 / gt:+.1f}%)",
            )
        )
    return diffs


def compare_facts(
    gn: dict[str, T.Any],
    bazel: dict[str, T.Any],
    tolerance: float = TEXT_SIZE_TOLERANCE,
) -> list[Diff]:
    """The package content comparison of two packages' facts."""
    diffs: list[Diff] = []
    # A bazel2gn mirror of a dual-built package is named `<name>-gn`.
    if gn["package_name"] not in (
        bazel["package_name"],
        f"{bazel['package_name']}-gn",
    ):
        diffs.append(
            Diff(
                "package_name",
                gn["package_name"],
                gn["package_name"],
                bazel["package_name"],
            )
        )
    if gn["abi_revision"] != bazel["abi_revision"]:
        diffs.append(
            Diff(
                "abi_revision",
                "meta/fuchsia.abi/abi-revision",
                gn["abi_revision"],
                bazel["abi_revision"],
            )
        )
    diffs += _set_diffs(
        "subpackages",
        "meta/fuchsia.pkg/subpackages",
        gn["subpackages"],
        bazel["subpackages"],
    )

    def presence(path: str, g: T.Any, b: T.Any) -> bool:
        if b is None:
            diffs.append(Diff("missing_blob", path, "present", "absent"))
        elif g is None:
            diffs.append(Diff("extra_blob", path, "absent", "present"))
        return g is not None and b is not None

    for path in sorted(set(gn["blobs"]) | set(bazel["blobs"])):
        g, b = gn["blobs"].get(path), bazel["blobs"].get(path)
        if not presence(path, g, b):
            continue
        if ("elf" in g) != ("elf" in b):
            diffs.append(
                Diff(
                    "blob_content",
                    path,
                    "ELF" if "elf" in g else "data",
                    "ELF" if "elf" in b else "data",
                )
            )
        elif "elf" in g:
            diffs += compare_elf(path, g["elf"], b["elf"], tolerance)
        elif g["merkle"] != b["merkle"]:
            diffs.append(
                Diff("blob_content", path, g["merkle"][:16], b["merkle"][:16])
            )
    for path in sorted(set(gn["meta"]) | set(bazel["meta"])):
        g, b = gn["meta"].get(path), bazel["meta"].get(path)
        if presence(path, g, b) and g != b:
            diffs.append(Diff("blob_content", path, g[:16], b[:16]))
    for path in sorted(set(gn["cvf"]) | set(bazel["cvf"])):
        g, b = gn["cvf"].get(path), bazel["cvf"].get(path)
        if presence(path, g, b) and g != b:
            diffs.append(
                Diff("config", path, f"sha256:{g[:16]}", f"sha256:{b[:16]}")
            )
    for path in sorted(set(gn["cm"]) | set(bazel["cm"])):
        g, b = gn["cm"].get(path), bazel["cm"].get(path)
        if not presence(path, g, b) or g == b:
            continue
        diffs += _cm_diffs(path, g, b)
    return _group_dylib_renames(diffs)


def _cm_section(lines: list[str], index: int) -> str:
    """Returns the top-level field of `cmc debug-print-cm` output at `index`."""
    for line in reversed(lines[: index + 1]):
        m = re.match(r"^ {4}(\w+):", line)
        if m:
            return m.group(1)
    return ""


def _cm_diffs(path: str, g: str, b: str) -> list[Diff]:
    """One Diff per changed `cmc debug-print-cm` line, with its section."""
    gl, bl = g.splitlines(), b.splitlines()
    diffs = []
    matcher = difflib.SequenceMatcher(a=gl, b=bl, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        for i in range(i1, i2):
            diffs.append(
                Diff(
                    "cm",
                    path,
                    gl[i].strip(),
                    "",
                    item=gl[i].strip(),
                    detail="-" + gl[i],
                    section=_cm_section(gl, i),
                )
            )
        for j in range(j1, j2):
            diffs.append(
                Diff(
                    "cm",
                    path,
                    "",
                    bl[j].strip(),
                    item=bl[j].strip(),
                    detail="+" + bl[j],
                    section=_cm_section(bl, j),
                )
            )
    return diffs


def _lib_stem(path: str) -> str:
    name = re.sub(r"\.so(\.\d+)*$", "", os.path.basename(path))
    name = re.sub(r"^lib", "", name)
    while True:
        # `_rust` crate suffixes and hashed Bazel suffixes (`-1069634238`).
        stripped = re.sub(r"(_rust|-\d{6,})$", "", name)
        if stripped == name:
            return name
        name = stripped


_IS_LIB = re.compile(r"\.so(\.\d+)*$")


def _group_dylib_renames(diffs: list[Diff]) -> list[Diff]:
    """Folds a shared library shipped under a new name into one Diff.

    A rename shows up as a missing blob, an extra blob and DT_NEEDED changes
    in its users; report it once as `dylib_renamed`. A DT_NEEDED change is
    folded only where the same binary swapped the old name for the new one,
    so a binary that dropped the library stays reported.
    """
    missing = [
        d.key
        for d in diffs
        if d.kind == "missing_blob" and _IS_LIB.search(d.key)
    ]
    extra = [
        d.key for d in diffs if d.kind == "extra_blob" and _IS_LIB.search(d.key)
    ]
    renames: dict[str, str] = {}
    for m in sorted(missing):
        for e in sorted(extra):
            if e not in renames.values() and _lib_stem(m) == _lib_stem(e):
                renames[m] = e
                break
    if not renames:
        return diffs
    needed: dict[str, dict[str, set[str]]] = {}
    for d in diffs:
        if d.kind == "elf_needed":
            sides = needed.setdefault(d.key, {"gn": set(), "bazel": set()})
            sides["gn" if d.gn == "present" else "bazel"].add(d.item)
    swapped: set[tuple[str, str]] = set()
    for m, e in renames.items():
        old, new = os.path.basename(m), os.path.basename(e)
        for key, sides in needed.items():
            if old in sides["gn"] and new in sides["bazel"]:
                swapped |= {(key, old), (key, new)}
    out = []
    for d in diffs:
        if d.key in renames or d.key in renames.values():
            continue
        if d.kind == "elf_needed" and (d.key, d.item) in swapped:
            continue
        out.append(d)
    out += [Diff("dylib_renamed", m, m, e) for m, e in sorted(renames.items())]
    return out


# ---------------------------------------------------------------------------
# Normalizations
# ---------------------------------------------------------------------------

NORMALIZATION_FIELDS = ("category", "key", "item", "gn", "bazel")


def load_normalizations(path: str) -> list[dict[str, T.Any]]:
    with open(path, encoding="utf-8") as f:
        entries = json.load(f)
    for e in entries:
        for field in ("id", "comparison", "match", "reason", "source"):
            if field not in e:
                raise ValueError(
                    f"normalization {e.get('id', '?')}: missing {field!r}"
                )
    return entries


def normalization_matches(entry: dict[str, T.Any], diff: Diff) -> bool:
    """True if `diff` matches every regex in `entry["match"]`.

    `category`, `key` and `item` must match fully; `gn` and `bazel` match
    anywhere. Entries matching on `code` describe rules applied inside
    `normalize_clang_args.py` / `normalize_rustc_args.py` and never match here.
    """
    match = entry["match"]
    if "code" in match or not any(f in match for f in NORMALIZATION_FIELDS):
        return False
    for field in NORMALIZATION_FIELDS:
        if field not in match:
            continue
        value = diff.category if field == "category" else getattr(diff, field)
        fn = re.search if field in ("gn", "bazel") else re.fullmatch
        try:
            if not fn(match[field], value):
                return False
        except (re.error, TypeError):
            # A malformed entry never matches; it must not hide differences.
            return False
    return True


def apply_normalizations(
    diffs: list[Diff], entries: list[dict[str, T.Any]]
) -> tuple[list[Diff], dict[str, int]]:
    """Drops normalized diffs. Returns (remaining diffs, {id: count})."""
    kept: list[Diff] = []
    applied: dict[str, int] = {}
    for d in diffs:
        for e in entries:
            if e["comparison"] == d.comparison and normalization_matches(e, d):
                applied[e["id"]] = applied.get(e["id"], 0) + 1
                break
        else:
            kept.append(d)
    return kept, applied


def registry_markdown(
    entries: list[dict[str, T.Any]], categories: dict[str, dict[str, T.Any]]
) -> str:
    """Renders CUTOVER_NORMALIZATIONS.md from the two registries."""
    lines = [
        "# Package cutover normalizations and categories",
        "",
        "<!-- Generated from cutover_normalizations.json and cutover_categories.json",
        "     by `verify_package_cutover.py --list-normalizations --markdown`.",
        "     Do not edit by hand. -->",
        "",
        "## Normalizations",
        "",
        "Differences between a GN package and its Bazel replacement that the",
        "package cutover check (`verify_package_cutover.py`) ignores. The package",
        "content comparison compares the built packages; the build command",
        "comparison compares the compile and link commands. Every report lists",
        "the normalizations it applied.",
        "",
        "| ID | Comparison | Match | Reason | Source |",
        "|---|---|---|---|---|",
    ]

    def row(cells: list[str]) -> str:
        return "| " + " | ".join(c.replace("|", "\\|") for c in cells) + " |"

    for e in entries:
        match = ", ".join(f"`{k}={v}`" for k, v in sorted(e["match"].items()))
        lines.append(
            row(
                [
                    f"`{e['id']}`",
                    COMPARISONS[e["comparison"]],
                    match,
                    e["reason"],
                    e["source"],
                ]
            )
        )
    lines += [
        "",
        "## Categories",
        "",
        "Each finding category, its escalation stage and what it can do on a",
        "device. Stage 0 and 1 findings are INFO (Planter reads stage 1",
        "itself); stage 2 findings are WARNING. `cutover_info_phase1_missing`",
        "and `cutover_info_not_compared` mark a cutover that was not verified.",
        "",
        "| Category | Stage | Device impact |",
        "|---|---|---|",
    ]
    for name, c in sorted(categories.items()):
        lines.append(row([f"`{name}`", str(c["stage"]), c["device_impact"]]))
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Findings
# ---------------------------------------------------------------------------


def new_finding(
    source: str,
    category: str,
    file: str,
    message: str,
    remediation: str = "",
    line: int = 0,
    severity: str = "INFO",
    fingerprint: str = "",
) -> dict[str, T.Any]:
    f = {
        "source": source,
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
        "remediation": remediation,
    }
    if fingerprint:
        f["fingerprint"] = fingerprint
    return f


# Out-dir prefixes (`out/<build dir>/`, possibly behind a checkout path) differ
# between checkouts and build dirs, so they are left out of fingerprints.
_VOLATILE_OUT_DIR_RE = re.compile(
    r"(?:[^\s\"'=:;,()]*/)?\bout/[^/\s\"'=:;,()]+/"
)


def fingerprint(parts: T.Iterable[T.Sequence[str]]) -> str:
    """A stable content fingerprint of a finding's sorted string tuples.

    Planter matches the uploader's decision on a finding by its ID and this
    fingerprint, so a decision stops covering the finding once its content
    changes. Out-dir paths are normalized away; order does not matter.
    """
    rows = sorted(
        json.dumps([_VOLATILE_OUT_DIR_RE.sub("<out>/", str(x)) for x in p])
        for p in parts
    )
    return hashlib.sha256("\n".join(rows).encode()).hexdigest()[:16]


def _fingerprint_row(d: Diff) -> tuple[str, str, str]:
    """The (item, gn, bazel) of a diff, without its volatile values."""
    if d.kind == "cm":
        # The item is the stripped line: keep its section and indentation
        # (and side, in `detail`), so a line moving between sections counts.
        return f"{d.section}\t{d.detail}", d.gn, d.bazel
    if d.kind == "elf_text_size":
        # The exact sizes move with any code change; keep the direction only.
        try:
            grew = int(d.bazel.split()[0]) > int(d.gn)
        except (ValueError, IndexError):
            return d.item, d.gn, d.bazel
        return d.item, "", "larger" if grew else "smaller"
    return d.item, d.gn, d.bazel


def diffs_fingerprint(diffs: T.Iterable[Diff]) -> str:
    """The fingerprint of a `cutover_diff_*` finding's grouped diffs."""
    return fingerprint(_fingerprint_row(d) for d in diffs)


def unverified_fingerprint(key: str, category: str, reason: str) -> str:
    """The fingerprint of a note that a cutover was not compared."""
    return fingerprint([(key, category, reason)])


def _describe_side(diffs: list[Diff], side: str) -> str:
    values = []
    for d in diffs:
        v = getattr(d, side)
        if d.item:
            if v in ("present", "absent"):
                if v == "present":
                    values.append(d.item)
                continue
            values.append(f"{d.item}: {v}" if v and d.kind != "cm" else v)
        elif v:
            values.append(v)
    values = [v for v in values if v]
    if not values:
        return "(none)"
    shown = values[:20]
    more = f" (+{len(values) - 20} more)" if len(values) > 20 else ""
    return "; ".join(shown) + more


def _cm_unified(group: list[Diff], limit: int = 60) -> list[str]:
    """Renders `.cm` line diffs as a unified diff, one hunk per section."""
    out: list[str] = []
    section = None
    for d in group:
        if d.section != section:
            section = d.section
            out.append(f"@@ {section or '(top)'} @@")
        out.append(d.detail or ("-" if d.gn else "+") + d.item)
    if len(out) > limit:
        out = out[:limit] + [f"... ({len(out) - limit} more lines)"]
    return out


# Where a finding anchors: (BUILD.bazel path, line or 0, target label).
Anchor: T.TypeAlias = tuple[str, int, str]


def findings_from_diffs(
    diffs: list[Diff],
    source: str,
    file: str,
    package_name: str,
    extra_impact: T.Callable[[Diff], str] = lambda d: "",
    anchor: T.Optional[T.Callable[[Diff], Anchor]] = None,
) -> list[dict[str, T.Any]]:
    """Groups diffs by (kind, key) into one finding each.

    The first message line is `[<package_name>:<key>] <summary>`. The key is
    unique per category and stable across patchsets. Planter's separate
    cutover review matches the uploader's decision on a finding by
    `<category>:<key>` and its `fingerprint` (a hash of the grouped
    `(item, gn, bazel)` values), so a decision is asked again once the
    content under the key changes. `anchor` names the BUILD.bazel
    target the finding is about (the reviewer sees it as an inline comment
    there); without it the finding anchors at `file`. The severity comes from
    the category's stage in cutover_categories.json.
    """
    groups: dict[tuple[str, str], list[Diff]] = {}
    for d in diffs:
        groups.setdefault((d.kind, d.key), []).append(d)
    findings = []
    for (kind, key), group in sorted(groups.items()):
        summary, fix = DIFF_KINDS[kind]
        category = f"cutover_diff_{kind}"
        impact = device_impact(category)
        extra = sorted({x for x in (extra_impact(d) for d in group) if x})
        if extra:
            impact += f" ({', '.join(extra)})"
        where, line, label = anchor(group[0]) if anchor else (file, 0, "")
        lines = [
            f"[{package_name}:{key}] {summary[0].upper() + summary[1:]} ({COMPARISONS[group[0].comparison]}).",
        ]
        if label:
            lines.append(f"Target: {label}")
        if kind == "cm":
            n_gn = sum(1 for d in group if d.detail.startswith("-"))
            lines += [
                f"GN: {n_gn} line(s) only in GN",
                f"Bazel: {len(group) - n_gn} line(s) only in Bazel",
                "```diff",
                *_cm_unified(group),
                "```",
            ]
        else:
            lines += [
                f"GN: {_describe_side(group, 'gn')}",
                f"Bazel: {_describe_side(group, 'bazel')}",
            ]
        lines.append(f"Device impact: {impact}")
        message = "\n".join(lines)
        findings.append(
            new_finding(
                source,
                category,
                where,
                message,
                f"Fix: {fix}. {NORMALIZATION_HINT}",
                line=line,
                severity=category_severity(category),
                fingerprint=diffs_fingerprint(group),
            )
        )
    return findings


# ---------------------------------------------------------------------------
# Phase-1 record
# ---------------------------------------------------------------------------


RECORDS_DIRNAME = "planter-cutover"


def harness_records_dir() -> str:
    """The absolute `$PLANTER_CUTOVER_RECORD_DIR`, or "" when it is unset."""
    d = os.environ.get(RECORD_DIR_ENV, "").strip()
    return os.path.abspath(d) if d else ""


def local_records_dir(workdir: str) -> str:
    """Returns `<git-common-dir>/planter-cutover/`, shared by fx worktrees."""
    common = git(
        workdir, ["rev-parse", "--path-format=absolute", "--git-common-dir"]
    )
    if not common:
        raise ValueError(
            f"{workdir}: not a git checkout; cannot place the phase-1 record"
        )
    return os.path.join(common, RECORDS_DIRNAME)


def records_dir(workdir: str) -> str:
    """Returns the directory that holds phase-1 records.

    `$PLANTER_CUTOVER_RECORD_DIR` when set (Planter keeps it per task),
    otherwise `local_records_dir()` (local runs).
    """
    return harness_records_dir() or local_records_dir(workdir)


def record_path(
    workdir: str,
    package_name: str,
    bazel_target: str = "",
    local: bool = False,
) -> str:
    """Returns `<records dir>/<package_name>--<bazel_target>.json`.

    The records dir is `records_dir()`, or `local_records_dir()` with
    `local`. Without `bazel_target`, returns the name records had before
    they were keyed by target (`<package_name>.json`), still read for
    migration. The record is not keyed by revision, so that it survives a
    rebase; the change base it was written against is stored inside it.
    """
    records = local_records_dir(workdir) if local else records_dir(workdir)
    stem = check_package_name(package_name)
    if bazel_target:
        stem += f"--{check_target_name(bazel_target)}"
    path = os.path.join(records, f"{stem}.json")
    check_inside(path, records)
    return path


# Bazel target names that can name a file: no `/`, and not `.` or `..`.
_TARGET_NAME_RE = re.compile(r"[0-9A-Za-z\-_.+=,@~]{1,255}")


def check_target_name(name: str) -> str:
    """Returns `name` if it can be part of a record file name, else raises."""
    if (
        not isinstance(name, str)
        or name in (".", "..")
        or not _TARGET_NAME_RE.fullmatch(name)
    ):
        raise ValueError(f"invalid target name {name!r}")
    return name


# Fuchsia package names: lowercase letters, digits, `-`, `_` and `.`, at most
# 255 characters (//docs/concepts/packages/package_url.md).
_PACKAGE_NAME_RE = re.compile(r"[0-9a-z\-_.]{1,255}")


def check_package_name(name: str) -> str:
    """Returns `name` if it is a valid Fuchsia package name, else raises.

    Package names come from BUILD files in the change under review and name
    files on disk, so they must not contain path separators or `..`.
    """
    if (
        not isinstance(name, str)
        or name in (".", "..")
        or not _PACKAGE_NAME_RE.fullmatch(name)
    ):
        raise ValueError(f"invalid package name {name!r}")
    return name


def check_inside(path: str, directory: str) -> None:
    """Raises unless `path` resolves to a location strictly inside `directory`."""
    root = os.path.realpath(directory)
    real = os.path.realpath(path)
    if real == root or os.path.commonpath([real, root]) != root:
        raise ValueError(f"{path} is outside {directory}")


RECORD_MAX_AGE_SECS = 14 * 24 * 3600


def prune_records(record_dir: str, now: T.Optional[float] = None) -> list[str]:
    """Deletes phase-1 records older than 14 days or whose worktree is gone.

    The git common dir is shared by all fx worktrees, so records outlive the
    tasks that wrote them. Not for `$PLANTER_CUTOVER_RECORD_DIR`, whose
    records Planter keeps per task. Returns the deleted record paths.
    """
    now = time.time() if now is None else now
    deleted: list[str] = []
    try:
        names = sorted(os.listdir(record_dir))
    except OSError:
        return deleted
    for name in names:
        if not name.endswith(".json"):
            continue
        path = os.path.join(record_dir, name)
        try:
            record = read_record(path)
        except Exception:  # Not a JSON object, or otherwise unreadable.
            record = None
        stale = (
            record is None
            or now - record.get("timestamp", 0) > RECORD_MAX_AGE_SECS
            or (record.get("workdir") and not os.path.isdir(record["workdir"]))
        )
        if stale:
            try:
                os.remove(path)
                shutil.rmtree(
                    path[: -len(".json")] + ".bazel", ignore_errors=True
                )
                deleted.append(path)
            except OSError:
                pass
    return deleted


def resolve_commit(workdir: str, rev: str) -> str:
    return (
        git(workdir, ["rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}"])
        or rev
    )


def write_record(
    path: str,
    cutover: Cutover,
    bazel_facts: dict[str, T.Any],
    findings: list[dict[str, T.Any]],
    applied: dict[str, int],
    change_base: str,
    bazel_manifest: T.Optional[str] = None,
    workdir: str = "",
    package_cpu: str = "",
    summary: T.Optional[dict[str, T.Any]] = None,
    gn_facts: T.Optional[dict[str, T.Any]] = None,
    build_cpu: str = "",
) -> None:
    """Writes the phase-1 record plus a copy of the Bazel package manifest.

    `gn_facts` (the GN package facts and build commands) let the final phase
    compare the Bazel package with the GN package again after the GN package
    is deleted. `build_cpu` is the CPU whose command keys have no suffix.
    """
    stem = path[: -len(".json")]
    records = _checked_records_dir(path)
    check_inside(stem + ".bazel", records)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    # Planter stores every file in $PLANTER_CUTOVER_RECORD_DIR in its ledger
    # (capped per task); the manifest copy is only for people debugging.
    harness = harness_records_dir()
    in_harness = bool(harness) and os.path.realpath(
        records
    ) == os.path.realpath(harness)
    if bazel_manifest and os.path.exists(bazel_manifest) and not in_harness:
        os.makedirs(stem + ".bazel", exist_ok=True)
        with open(bazel_manifest, encoding="utf-8") as src:
            _write_new_file(
                os.path.join(stem + ".bazel", "package_manifest.json"),
                records,
                src.read(),
            )
        _write_new_file(
            os.path.join(stem + ".bazel", "meta_far_listing.txt"),
            records,
            "\n".join(bazel_facts.get("meta_listing", [])) + "\n",
        )
    record = {
        "schema_version": RECORD_SCHEMA_VERSION,
        "timestamp": int(time.time()),
        "gn_label": cutover.gn_label,
        "bazel_label": cutover.bazel_label,
        "package_name": cutover.package_name,
        "change_base": change_base,
        # The git common dir is shared by worktrees; remember which one wrote
        # it (not checked for records in $PLANTER_CUTOVER_RECORD_DIR).
        "workdir": workdir,
        # The final phase rebuilds the Bazel package for the CPU phase 1
        # compared.
        "package_cpu": package_cpu,
        # Phase 1's per-comparison counts (or reasons) and coverage gaps.
        "summary": summary or {},
        "bazel_meta_far_merkle": bazel_facts.get("meta_far_merkle", ""),
        "findings": findings,
        "normalizations_applied": applied,
        "bazel_facts": _recorded_facts(bazel_facts, elf=gn_facts is None),
    }
    if gn_facts is not None:
        record["gn_facts"] = _recorded_facts(gn_facts)
        record["build_cpu"] = build_cpu
    store_record(path, record)


def store_record(path: str, record: dict[str, T.Any]) -> None:
    """Writes a record as compact JSON, replacing any file at `path`."""
    records = _checked_records_dir(path)
    os.makedirs(records, exist_ok=True)
    tmp = path + ".tmp"
    _write_new_file(
        tmp,
        records,
        json.dumps(record, sort_keys=True, separators=(",", ":")),
    )
    os.replace(tmp, path)


def _checked_records_dir(path: str) -> str:
    """Returns the records dir of `path`, raising unless it is a valid one.

    `path` comes from record_path(); re-check it before writing anything.
    """
    records = os.path.dirname(os.path.abspath(path))
    harness = harness_records_dir()
    if os.path.basename(records) != RECORDS_DIRNAME and not (
        harness and os.path.realpath(records) == os.path.realpath(harness)
    ):
        raise ValueError(f"{path} is not in a {RECORDS_DIRNAME} directory")
    stem = os.path.basename(path)[: -len(".json")]
    package_name, _, target = stem.partition("--")
    check_package_name(package_name)
    if target:
        check_target_name(target)
    check_inside(path, records)
    return records


def _recorded_facts(
    facts: dict[str, T.Any], elf: bool = True
) -> dict[str, T.Any]:
    """Package facts without the local blob source paths.

    Without `elf`, blobs keep only their merkle and size: version 2 records
    compare the GN facts again, and need the Bazel blobs only to say which
    files changed since phase 1. ELF facts (mostly exported symbols) are
    the bulk of a record.
    """
    out = {k: v for k, v in facts.items() if k != "sources"}
    if not elf and isinstance(out.get("blobs"), dict):
        out["blobs"] = {
            p: {k: v for k, v in b.items() if k != "elf"}
            for p, b in out["blobs"].items()
        }
    return out


def _write_new_file(path: str, directory: str, text: str) -> None:
    """Writes `text` to a fresh file at `path`, which must be inside `directory`.

    An existing file or symlink at `path` is removed first, and the file is
    created with O_EXCL | O_NOFOLLOW, so a planted symlink cannot redirect
    the write.
    """
    check_inside(path, directory)  # Also refuses a symlink that points out.
    if os.path.lexists(path):
        os.unlink(path)
    fd = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o644,
    )
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)


def read_record(path: str) -> T.Optional[dict[str, T.Any]]:
    try:
        with open(path, encoding="utf-8") as f:
            record = json.load(f)
    except (OSError, ValueError):
        return None
    if (
        not isinstance(record, dict)
        or record.get("schema_version") not in READABLE_RECORD_SCHEMA_VERSIONS
    ):
        return None
    return record


def rekey_commands(
    commands: dict[str, T.Any], from_cpu: str, to_cpu: str
) -> dict[str, T.Any]:
    """Re-keys build commands recorded with build CPU `from_cpu` for `to_cpu`.

    Command keys are `<blob> <action>` for the build directory's CPU and
    `<blob> <action> <cpu>` for others, so a record from a checkout with
    another build CPU needs its keys renamed before they can be compared.
    """
    return {
        rekey_command_key(key, from_cpu, to_cpu): value
        for key, value in commands.items()
    }


def rekey_command_key(key: str, from_cpu: str, to_cpu: str) -> str:
    """Renames one `<blob> <action>[ <cpu>]` command key (see
    `rekey_commands`)."""
    if not from_cpu or from_cpu == to_cpu:
        return key
    parts = key.split(" ")
    if len(parts) < 2:
        return key
    cpu = parts[2] if len(parts) > 2 else from_cpu
    return " ".join(parts[:2] + ([] if cpu == to_cpu else [cpu]))


def blob_changes(
    old: dict[str, T.Any], new: dict[str, T.Any], limit: int = 20
) -> list[str]:
    """Lists the files that differ between two builds of one package.

    Compares blob merkles, `meta.far` entries, the ABI revision and the
    subpackages, e.g.
    `bin/foo 1a2b3c4d5e6f7a8b -> 9c8d7e6f5a4b3c2d`, `lib/x.so added`.
    """
    changes = []
    if old.get("abi_revision") != new.get("abi_revision"):
        changes.append("meta/fuchsia.abi/abi-revision changed")
    if sorted(old.get("subpackages", [])) != sorted(new.get("subpackages", [])):
        changes.append("meta/fuchsia.pkg/subpackages changed")
    for field in ("blobs", "meta", "cvf", "cm"):
        o, n = old.get(field, {}), new.get(field, {})
        for path in sorted(set(o) | set(n)):
            if path not in n:
                changes.append(f"{path} removed")
            elif path not in o:
                changes.append(f"{path} added")
            elif field == "blobs" and o[path].get("merkle") != n[path].get(
                "merkle"
            ):
                changes.append(
                    f"{path} {str(o[path].get('merkle'))[:16]} -> "
                    f"{str(n[path].get('merkle'))[:16]}"
                )
            elif field != "blobs" and o[path] != n[path]:
                changes.append(f"{path} changed")
    if len(changes) > limit:
        return changes[:limit] + [f"and {len(changes) - limit} more"]
    return changes
