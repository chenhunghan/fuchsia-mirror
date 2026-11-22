#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check that a migration keeps the GN dependency labels and edge
# kinds of every target it converts. For each directory of the change whose
# BUILD.gn existed at $PLANTER_CHANGE_BASE and still exists (a package whose
# BUILD.gn is deleted has no GN dependents left), every GN target that still
# exists under the same name is compared with its original over deps,
# public_deps, non_rust_deps, test_deps, proc_macro_deps, link_deps, data_deps
# and non_test_deps (moving a label between these lists is fine, except out of
# public_deps, see 3.; so is moving a test_deps label to another target of the
# package, e.g. an explicit test target replacing `with_unit_tests`). A label
# the target now reaches through a GN `group` it newly depends on (e.g. a
# package-local group above the BAZEL2GN SENTINEL that a BUILD.bazel entry is
# mapped to with `# @bazel2gn:path_overwrite:`) counts as kept.
# 1. dep_label_substituted (error): a dependency was swapped for another target
#    of the same package that still exists, e.g. a GN-only `<lib>_static`
#    wrapper above the dependency's BAZEL2GN SENTINEL replaced by the `<lib>`
#    it wraps, or a GN `shared_library` / `sdk_shared_library` /
#    `loadable_module` replaced in BUILD.bazel by a sibling static/source_set
#    library (directly or masked by `# @bazel2gn:path_overwrite:`). Pure
#    forwarding groups (whose
#    forwarded targets also do not count as newly added substitutes for other
#    removed dependencies), GN groups replacing a migrated GN test target with
#    `bazel_test_suite` and/or `<package_name>-gn`, `test_deps`-only entries on
#    targets that no longer set `with_unit_tests`, and incompatible
#    `host_test_data` vs. compiled target pairs are exempt.
# 2. dep_link_settings_dropped (error): a removed dependency carries
#    dependent-affecting link settings (public_configs, all_dependent_configs,
#    libs on non-shared targets, complete_static_lib; target-local ldflags and
#    rustflags do not propagate to dependents in GN, and private deps whose
#    public_configs only add C/C++ include_dirs or C/C++ compile flags on Rust
#    targets do not affect linking). A removed test_deps entry of a target that
#    no longer sets with_unit_tests (its unit test moved to Bazel) is exempt:
#    only GN's generated <name>_test executable linked it.
# 3. public_dep_forwarding_lost (error): a dependency with public_configs was a
#    public_deps edge and is now reached only through private edges, and no
#    group the target newly depends on re-exports those configs as
#    all_dependent_configs. GN forwards public_configs to dependents only
#    through public_deps (bazel2gn emits Rust deps as private GN deps).
# 4. dep_label_removed (info): any other dependency that no target of the
#    package depends on anymore (e.g. an unused test_deps entry).
# 5. unexpected_rust_dep_added (error): a migrated Rust target adds an external
#    crate dependency absent from the base BUILD.gn and unreferenced in the
#    target's .rs source files, which fails under `-D unused-crate-dependencies`.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$WORKDIR" "$TARGET_DIR" "$SCRIPT_DIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")
if len(sys.argv) > 3 and sys.argv[3]:
    sys.path.insert(0, os.path.abspath(sys.argv[3]))
import planter_check_lib as pcl

change_base = (
    os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    or (pcl.resolve_change_base(workdir) if pcl is not None else "")
    or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
    or "HEAD"
)

GN_DEP_ATTRS = (
    "deps",
    "public_deps",
    "non_rust_deps",
    "test_deps",
    "args_deps",
    "args_test_deps",
    "proc_macro_deps",
    "link_deps",
    "data_deps",
    "non_test_deps",
    "source_deps",
)
LINK_ATTRS = ("public_configs", "all_dependent_configs", "libs", "complete_static_lib")
FORWARDING_ONLY_ATTRS = {"public_deps", "deps", "visibility", "testonly"}
CONFIG_META_ATTRS = {"visibility", "testonly"}
CC_INCLUDE_CONFIG_ATTRS = {"include_dirs"}
CC_COMPILE_CONFIG_ATTRS = CC_INCLUDE_CONFIG_ATTRS | {
    "asmflags",
    "cflags",
    "cflags_c",
    "cflags_cc",
    "cflags_objc",
    "cflags_objcc",
    "defines",
    "precompiled_header",
    "precompiled_source",
}
GN_TEST_PACKAGE_TEMPLATES = {"fuchsia_unittest_package", "fuchsia_test_package"}
GN_TEST_TEMPLATES = GN_TEST_PACKAGE_TEMPLATES | {
    "fuchsia_unittest_component",
    "fuchsia_test_component",
    "fuchsia_test",
    "python_host_test",
    "host_test",
    "rustc_test",
    "go_test",
    "test",
    "cc_test_executable",
    "bootfs_test",
}
SENTINEL = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)
GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{')
GN_STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from"}


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


_shown = {}


def old_text(rel):
    """The file as of $PLANTER_CHANGE_BASE, or None."""
    if rel not in _shown:
        try:
            _shown[rel] = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{rel}"], stderr=subprocess.DEVNULL, text=True
            )
        except Exception:
            _shown[rel] = None
    return _shown[rel]


def new_text(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def gn_code(text):
    """Blanks out GN comments without moving offsets, so strings and brackets can be scanned."""
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "#":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def closing(code, i):
    """Index of the bracket closing the `{`, `[`, or `(` at code[i]."""
    open_c = code[i]
    close_c = {"{": "}", "[": "]", "(": ")"}[open_c]
    depth, in_str = 0, False
    while i < len(code):
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


FORWARD_VARS_RE = re.compile(
    r'(?<![\w.])forward_variables_from\(\s*([A-Za-z_]\w*)\s*,\s*'
    r'("\*"|\[[^\]]*\])\s*(?:,\s*(\[[^\]]*\]))?\s*\)'
)


# Extract file-level `scope = { ... }` definitions outside GN target blocks.
def gn_scope_literals(code, target_spans):
    """{scope_name: body_code} for file-level GN scope literals in `code`."""
    scopes = {}
    for sm in re.finditer(r"(?m)^\s*([A-Za-z_]\w*)\s*=\s*\{", code):
        pos = sm.start()
        if any(ts <= pos < te for ts, te in target_spans):
            continue
        b_idx = sm.end() - 1
        scopes[sm.group(1)] = code[b_idx + 1 : closing(code, b_idx)]
    return scopes


def _interp_gn_str(s, str_vars):
    if not str_vars or "$" not in s:
        return s
    out = s
    for _ in range(4):
        prev = out
        for k, v in str_vars.items():
            out = out.replace(f"${{{k}}}", v)
            out = re.sub(
                rf"\${re.escape(k)}(?![A-Za-z0-9_])", lambda _m, v=v: v, out
            )
        if out == prev or "$" not in out:
            break
    return out


def _interp_gn_code_strings(chunk, str_vars):
    if not str_vars or "$" not in chunk:
        return chunk
    return GN_STRING.sub(
        lambda sm: f'"{_interp_gn_str(sm.group(1), str_vars)}"', chunk
    )


# Replace bare `name` identifiers used as GN list elements (`deps += [ name ]` inside a
# foreach body) with `"value"`, leaving string literals and member accesses intact.
def _quote_bare_gn_list_ident(code, name, value):
    if not name or name not in code:
        return code
    out = []
    i, n, depth = 0, len(code), 0
    while i < n:
        c = code[i]
        if c == '"':
            j = i + 1
            while j < n and code[j] != '"':
                j += 2 if code[j] == "\\" else 1
            out.append(code[i : j + 1])
            i = j + 1
            continue
        if c == "[":
            depth += 1
        elif c == "]":
            depth = max(0, depth - 1)
        elif depth > 0 and (c.isalpha() or c == "_"):
            tok = re.match(r"[A-Za-z_]\w*", code[i:]).group(0)
            prev = code[i - 1] if i else ""
            end = i + len(tok)
            nxt = code[end] if end < n else ""
            if tok == name and prev not in ".$" and not prev.isalnum() and nxt not in ".([":
                out.append('"' + value.replace('"', '\\"') + '"')
            else:
                out.append(tok)
            i = end
            continue
        out.append(c)
        i += 1
    return "".join(out)


def _outer_foreach_spans(code):
    """Find foreach(...) blocks outside target/template/declare_args bodies."""
    spans = []
    i, n, in_str = 0, len(code), False
    fe_re = re.compile(
        r"foreach\s*\(\s*([A-Za-z_]\w*)\s*,\s*(?:([A-Za-z_]\w*)|(\[[^\]]*\]))\s*\)\s*\{"
    )
    skip_blk_re = re.compile(
        r"(?:template|declare_args|forward_variables_from)\s*\([^)]*\)\s*\{"
    )
    while i < n:
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
            i += 1
            continue
        if c == '"':
            in_str = True
            i += 1
            continue
        if (c.isalpha() or c == "_") and (
            i == 0 or not (code[i - 1].isalnum() or code[i - 1] == "_")
        ):
            fm = fe_re.match(code, i)
            if fm:
                b_open = fm.end() - 1
                b_close = closing(code, b_open)
                spans.append(
                    (i, b_open, b_close, fm.group(1), fm.group(2), fm.group(3))
                )
                i = b_close + 1
                continue
            sm = skip_blk_re.match(code, i)
            if sm:
                b_open = sm.end() - 1
                i = closing(code, b_open) + 1
                continue
            tm = GN_TARGET.match(code, i)
            if tm and tm.group(1) not in NOT_TARGETS:
                b_open = tm.end() - 1
                i = closing(code, b_open) + 1
                continue
        i += 1
    return spans


def gn_targets(text, pkg="", version="new"):
    """{name: {"tmpl", "start", "line", "bodies", "vars"}} for every `tmpl("name") { ... }` block."""
    code = gn_code(text)
    _gn_target_str_attrs = {
        "name",
        "output_name",
        "crate_name",
        "source_root",
        "edition",
        "package_name",
        "component_name",
        "manifest",
        "test_type",
    }
    _gn_target_list_attrs = (
        set(GN_DEP_ATTRS)
        | set(LINK_ATTRS)
        | {
            "sources",
            "inputs",
            "outputs",
            "data",
            "visibility",
            "configs",
            "cflags",
            "cflags_c",
            "cflags_cc",
            "asmflags",
            "defines",
            "include_dirs",
            "args",
            "test_environments",
        }
    )

    def _mask_scoped_blocks(chunk, mask_foreach=False):
        chars = list(chunk)
        i, n, in_str = 0, len(chunk), False
        skip_re = re.compile(
            r"(?:template|declare_args|forward_variables_from)\s*\([^)]*\)\s*\{"
        )
        fe_re = re.compile(
            r"foreach\s*\(\s*[A-Za-z_]\w*\s*,\s*(?:[A-Za-z_]\w*|\[[^\]]*\])\s*\)\s*\{"
        )
        while i < n:
            c = chunk[i]
            if in_str:
                if c == "\\":
                    i += 2
                    continue
                in_str = c != '"'
                i += 1
                continue
            if c == '"':
                in_str = True
                i += 1
                continue
            if (c.isalpha() or c == "_") and (
                i == 0 or not (chunk[i - 1].isalnum() or chunk[i - 1] == "_")
            ):
                bm = skip_re.match(chunk, i) or (
                    fe_re.match(chunk, i) if mask_foreach else None
                )
                tm = None if bm else GN_TARGET.match(chunk, i)
                if bm or (tm and tm.group(1) not in NOT_TARGETS):
                    b_open = (bm or tm).end() - 1
                    b_close = closing(chunk, b_open)
                    for k in range(b_open + 1, min(b_close, n)):
                        if chars[k] != "\n":
                            chars[k] = " "
                    i = b_close + 1
                    continue
            i += 1
        return "".join(chars)

    def _collect_scope_vars(
        masked_chunk,
        base_strs=None,
        base_lists=None,
        skip_names=(),
        in_target=False,
    ):
        s_vars = dict(base_strs or {})
        l_vars = {k: list(v) for k, v in (base_lists or {}).items()}
        ev = []
        for sm in re.finditer(
            r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"((?:[^"\\]|\\.)*)"', masked_chunk
        ):
            ev.append((sm.start(), "str", sm))
        for vm in re.finditer(
            r"(?m)^\s*([A-Za-z_]\w*)\s*(\+?=)\s*\[", masked_chunk
        ):
            ev.append((vm.start(), "list", vm))
        ev.sort(key=lambda e: e[0])
        for _, kind, m in ev:
            vname = m.group(1)
            if vname in skip_names:
                continue
            if kind == "str":
                if not (in_target and vname in _gn_target_str_attrs):
                    s_vars[vname] = _interp_gn_str(m.group(2), s_vars)
            elif not (in_target and vname in _gn_target_list_attrs):
                b_idx = m.end() - 1
                vals = [
                    _interp_gn_str(s, s_vars)
                    for s in GN_STRING.findall(
                        masked_chunk[b_idx + 1 : closing(masked_chunk, b_idx)]
                    )
                ]
                if m.group(2) == "+=":
                    l_vars.setdefault(vname, []).extend(vals)
                else:
                    l_vars[vname] = vals
        return s_vars, l_vars

    gni_strs, gni_lists = {}, {}
    for raw_imp in re.findall(r'\bimport\(\s*"([^"$]+)"\s*\)', code):
        imp = raw_imp.strip()
        if not imp:
            continue
        gni_rel = (
            imp[2:].lstrip("/")
            if imp.startswith("//")
            else os.path.normpath(os.path.join(pkg, imp)).replace("\\", "/")
        )
        if not gni_rel or gni_rel.startswith(".."):
            continue
        gni_txt = (
            (old_text(gni_rel) or new_text(gni_rel))
            if version == "old"
            else (new_text(gni_rel) or old_text(gni_rel))
        )
        if gni_txt:
            gni_strs, gni_lists = _collect_scope_vars(
                _mask_scoped_blocks(gn_code(gni_txt), mask_foreach=True),
                gni_strs,
                gni_lists,
            )

    seed_strs, seed_lists = _collect_scope_vars(
        _mask_scoped_blocks(code, mask_foreach=True), gni_strs, gni_lists
    )

    segments = [(0, code, True)]
    for _ in range(2):
        next_segments = []
        any_expanded = False
        for seg_off, seg_txt, is_orig in segments:
            f_spans = _outer_foreach_spans(seg_txt)
            if not f_spans:
                next_segments.append((seg_off, seg_txt, is_orig))
                continue
            any_expanded = True
            seg_strs, seg_lists = _collect_scope_vars(
                _mask_scoped_blocks(seg_txt, mask_foreach=True),
                seed_strs,
                seed_lists,
            )
            pos = 0
            for f_start, b_open, b_close, lvar, list_id, list_lit in f_spans:
                if pos < f_start:
                    next_segments.append(
                        (
                            seg_off + pos if is_orig else seg_off,
                            seg_txt[pos:f_start],
                            is_orig,
                        )
                    )
                vals = (
                    seg_lists.get(list_id, ())
                    if list_id
                    else [
                        _interp_gn_str(s, seg_strs)
                        for s in GN_STRING.findall(list_lit or "")
                    ]
                )
                raw_fbody = seg_txt[b_open + 1 : b_close]
                masked_fbody = _mask_scoped_blocks(raw_fbody, mask_foreach=True)
                nested_lvars = {
                    nfm.group(1)
                    for nfm in re.finditer(
                        r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s*,", raw_fbody
                    )
                    if nfm.group(1) != lvar
                }
                base_iter_strs = {
                    k: v for k, v in seg_strs.items() if k not in nested_lvars
                }
                ev_orig = seg_off + f_start if is_orig else seg_off
                if vals:
                    for val in vals:
                        iter_strs = dict(base_iter_strs)
                        iter_strs[lvar] = val
                        iter_strs, _ = _collect_scope_vars(
                            masked_fbody,
                            iter_strs,
                            skip_names=nested_lvars | {lvar},
                        )
                        next_segments.append(
                            (
                                ev_orig,
                                _quote_bare_gn_list_ident(
                                    _interp_gn_code_strings(raw_fbody, iter_strs),
                                    lvar,
                                    val,
                                ),
                                False,
                            )
                        )
                else:
                    next_segments.append(
                        (
                            ev_orig,
                            _interp_gn_code_strings(raw_fbody, base_iter_strs),
                            False,
                        )
                    )
                pos = b_close + 1
            if pos < len(seg_txt):
                next_segments.append(
                    (
                        seg_off + pos if is_orig else seg_off,
                        seg_txt[pos:],
                        is_orig,
                    )
                )
        segments = next_segments
        if not any_expanded:
            break

    str_vars = dict(gni_strs)
    gn_vars = {k: list(v) for k, v in gni_lists.items()}
    raw_targets = []
    for seg_off, seg_txt, is_orig in segments:
        masked_seg = _mask_scoped_blocks(seg_txt, mask_foreach=True)
        events = []
        for sm in re.finditer(
            r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"((?:[^"\\]|\\.)*)"', masked_seg
        ):
            events.append((sm.start(), "str", sm))
        for vm in re.finditer(
            r"(?m)^\s*([A-Za-z_]\w*)\s*(\+?=)\s*\[", masked_seg
        ):
            events.append((vm.start(), "list", vm))
        for tm in GN_TARGET.finditer(seg_txt):
            if tm.group(1) not in NOT_TARGETS:
                events.append((tm.start(), "tgt", tm))
        events.sort(key=lambda e: e[0])
        tgt_end = -1
        for ev_pos, kind, match in events:
            if kind == "tgt":
                brace = match.end() - 1
                end_brace = closing(seg_txt, brace)
                tgt_end = max(tgt_end, end_brace)
                tname = _interp_gn_str(match.group(2), str_vars)
                if "$" in tname:
                    continue
                orig_start = seg_off + ev_pos if is_orig else seg_off
                raw_body = seg_txt[brace + 1 : end_brace]
                inner_lvars = {
                    fm.group(1)
                    for fm in re.finditer(
                        r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s*,", raw_body
                    )
                }
                masked_body = _mask_scoped_blocks(raw_body, mask_foreach=True)
                body_strs, snap_vars = _collect_scope_vars(
                    masked_body,
                    {k: v for k, v in str_vars.items() if k not in inner_lvars},
                    gn_vars,
                    skip_names=inner_lvars,
                    in_target=True,
                )
                interp_body = _interp_gn_code_strings(raw_body, body_strs)
                raw_targets.append((
                    tname,
                    match.group(1),
                    orig_start,
                    code.count("\n", 0, orig_start) + 1,
                    interp_body,
                    snap_vars,
                ))
            elif ev_pos > tgt_end:
                if kind == "str":
                    str_vars[match.group(1)] = _interp_gn_str(
                        match.group(2), str_vars
                    )
                else:
                    b_idx = match.end() - 1
                    vals = [
                        _interp_gn_str(s, str_vars)
                        for s in GN_STRING.findall(
                            masked_seg[b_idx + 1 : closing(masked_seg, b_idx)]
                        )
                    ]
                    if match.group(2) == "+=":
                        gn_vars.setdefault(match.group(1), []).extend(vals)
                    else:
                        gn_vars[match.group(1)] = vals

    all_target_spans = [
        (tm.start(), closing(code, tm.end() - 1) + 1)
        for tm in GN_TARGET.finditer(code)
        if tm.group(1) not in NOT_TARGETS
    ]
    gn_scopes = gn_scope_literals(code, all_target_spans)
    targets = {}
    for tname, tmpl, t_start, t_line, tbody, tvars in raw_targets:
        t = targets.setdefault(
            tname,
            {
                "tmpl": tmpl,
                "start": t_start,
                "line": t_line,
                "bodies": [],
                "vars": dict(tvars),
                "scopes": gn_scopes,
            },
        )
        t["bodies"].append(tbody)
        t["vars"].update(tvars)
    return targets


_parsed = {}


def parsed(rel, version):
    """(text, gn_targets(text)) of a BUILD.gn as of the base ("old") or on disk ("new"); (None, {}) if absent."""
    key = (rel, version)
    if key not in _parsed:
        text = old_text(rel) if version == "old" else new_text(rel)
        pkg = os.path.dirname(rel).strip("/")
        _parsed[key] = (
            text,
            gn_targets(text, pkg=pkg, version=version) if text else {},
        )
    return _parsed[key]


# Expand a GN string literal containing `$loop_var` or `${loop_var}` across `loop_vals`.
def expand_foreach_str(s, loop_var, loop_vals):
    pat = re.compile(rf"\$\{{{re.escape(loop_var)}\}}|\${re.escape(loop_var)}(?![A-Za-z0-9_])")
    if not loop_vals or not pat.search(s):
        return [s]
    return [pat.sub(lambda _m, v=v: v, s) for v in loop_vals]


def _parse_gn_list_rhs(body, start_idx, gn_vars, foreach_spans=()):
    """List of string items starting at `body[start_idx]` across `[...]` and `+` vars."""
    items = []
    n = len(body)
    idx = start_idx
    while idx < n:
        while idx < n and body[idx].isspace():
            idx += 1
        if idx >= n:
            break
        if body[idx] == "[":
            end_b = closing(body, idx)
            raw_items = GN_STRING.findall(body[idx + 1 : end_b])
            for b_open, b_close, lvar, lvals in foreach_spans:
                if b_open <= idx < b_close:
                    raw_items = [
                        exp
                        for s in raw_items
                        for exp in expand_foreach_str(s, lvar, lvals)
                    ]
            items.extend(raw_items)
            idx = end_b + 1
        else:
            id_m = re.match(r"[A-Za-z_]\w*", body[idx:])
            if not id_m:
                break
            items.extend(gn_vars.get(id_m.group(0), ()))
            idx += id_m.end()
        while idx < n and body[idx].isspace():
            idx += 1
        if idx < n and body[idx] == "+":
            idx += 1
        else:
            break
    return items


def gn_lists(body, attrs, gn_vars=None):
    """{attr: [string literals]} of `attr = ...` / `attr += ...` assignments in body."""
    gn_vars = gn_vars or {}
    foreach_spans = []
    for fm in re.finditer(
        r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s*,\s*(?:([A-Za-z_]\w*)|(\[[^\]]*\]))\s*\)\s*\{",
        body,
    ):
        b_open = fm.end() - 1
        b_close = closing(body, b_open)
        vals = (
            gn_vars.get(fm.group(2), ())
            if fm.group(2)
            else GN_STRING.findall(fm.group(3) or "")
        )
        if vals:
            foreach_spans.append((b_open, b_close, fm.group(1), vals))
    res = {}
    for m in re.finditer(r"(?<![\w.])(%s)\s*\+?=\s*" % "|".join(attrs), body):
        attr = m.group(1)
        res.setdefault(attr, []).extend(
            _parse_gn_list_rhs(body, m.end(), gn_vars, foreach_spans)
        )
    return res


# Blank nested `{ ... }` blocks inside a target body so brace-depth-0 `-=` can be read.
def _blank_nested_braces(body):
    out = []
    i = 0
    n = len(body)
    in_str = False
    while i < n:
        c = body[i]
        if in_str:
            if c == "\\":
                out.append(body[i : i + 2])
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "{":
            end_b = closing(body, i)
            out.append(" " * (end_b + 1 - i))
            i = end_b + 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


# Collect unconditional brace-depth-0 `attr -= ...` list removals in a target body.
def _top_level_gn_removals(body, attrs, gn_vars=None):
    gn_vars = gn_vars or {}
    flat = _blank_nested_braces(body)
    removed = {}
    for m in re.finditer(r"(?<![\w.])(%s)\s*-=\s*" % "|".join(attrs), flat):
        attr = m.group(1)
        removed.setdefault(attr, []).extend(_parse_gn_list_rhs(flat, m.end(), gn_vars))
    return removed


# Expand `forward_variables_from(<scope>, ...)` list assignments from file-level scopes.
def _expand_forwarded_gn_lists(body, attrs, gn_vars, gn_scopes, seen=()):
    fwd_lists = {}
    fwd_bodies = []
    if not gn_scopes or len(seen) >= 5:
        return fwd_lists, fwd_bodies
    for fm in FORWARD_VARS_RE.finditer(body):
        scope_name = fm.group(1)
        if scope_name in seen or scope_name not in gn_scopes:
            continue
        scope_body = gn_scopes[scope_name]
        wanted = None if fm.group(2) == '"*"' else set(GN_STRING.findall(fm.group(2)))
        excluded = set(GN_STRING.findall(fm.group(3) or ""))
        sub_lists, sub_bodies = _expand_forwarded_gn_lists(
            scope_body, attrs, gn_vars, gn_scopes, seen + (scope_name,)
        )
        combined = {}
        for attr, items in sub_lists.items():
            combined.setdefault(attr, []).extend(items)
        for attr, items in gn_lists(scope_body, attrs, gn_vars).items():
            combined.setdefault(attr, []).extend(items)
        for attr, items in combined.items():
            if attr in excluded or (wanted is not None and attr not in wanted):
                continue
            fwd_lists.setdefault(attr, []).extend(items)
        fwd_bodies.extend(sub_bodies)
        fwd_bodies.append(scope_body)
    return fwd_lists, fwd_bodies


# Check whether a boolean GN attribute (`with_unit_tests`, `args_with_unit_tests`) resolves true.
def _gn_bool_flag_enabled(bodies, fwd_bodies, flag_name):
    pat = re.compile(r"\b%s\s*=\s*(true|false)\b" % re.escape(flag_name))
    direct = [m.group(1) for b in bodies for m in pat.finditer(b)]
    if direct:
        return direct[-1] == "true"
    inherited = [m.group(1) for b in fwd_bodies for m in pat.finditer(b)]
    return bool(inherited and inherited[-1] == "true")


def normalize(raw, pkg):
    """Absolute `//path:name` form of a GN label; None for labels built from variables."""
    raw = raw.strip()
    if not raw or "$" in raw or raw.startswith("@"):
        return None
    raw = re.sub(r"\(.*\)$", "", raw)
    if raw.startswith("//"):
        path, _, name = raw[2:].partition(":")
    elif raw.startswith(":"):
        path, name = pkg, raw[1:]
    else:
        rel, _, name = raw.partition(":")
        path = os.path.normpath(os.path.join(pkg, rel))
        if path.startswith(".."):
            return None
    path = path.strip("/")
    name = name or os.path.basename(path)
    return f"//{path}:{name}" if name else None


def gn_deps(target, pkg):
    """{attr: set(labels)} over all the blocks (and forwarded scopes) defining a GN target."""
    res = {}
    gn_vars = target.get("vars") or {}
    gn_scopes = target.get("scopes") or {}
    bodies = target["bodies"]
    all_fwd_lists = {}
    all_fwd_bodies = []
    for body in bodies:
        fl, fb = _expand_forwarded_gn_lists(body, GN_DEP_ATTRS, gn_vars, gn_scopes)
        for attr, items in fl.items():
            all_fwd_lists.setdefault(attr, []).extend(items)
        all_fwd_bodies.extend(fb)
    is_test_tmpl = target.get("tmpl") in GN_TEST_TEMPLATES
    for attr, items in all_fwd_lists.items():
        if (
            attr == "test_deps"
            and not is_test_tmpl
            and not _gn_bool_flag_enabled(bodies, all_fwd_bodies, "with_unit_tests")
        ):
            continue
        if (
            attr == "args_test_deps"
            and not is_test_tmpl
            and not _gn_bool_flag_enabled(bodies, all_fwd_bodies, "args_with_unit_tests")
        ):
            continue
        res.setdefault(attr, set()).update(
            l for l in (normalize(s, pkg) for s in items) if l
        )
    for body in bodies:
        for attr, rem_items in _top_level_gn_removals(body, GN_DEP_ATTRS, gn_vars).items():
            if attr in res:
                res[attr].difference_update(
                    l for l in (normalize(s, pkg) for s in rem_items) if l
                )
        for attr, items in gn_lists(body, GN_DEP_ATTRS, gn_vars).items():
            res.setdefault(attr, set()).update(
                l for l in (normalize(s, pkg) for s in items) if l
            )
    return {a: s for a, s in res.items() if s}


def split(label):
    path, _, name = label[2:].partition(":")
    return path, name


def compact(text):
    return re.sub(r"\s+", " ", text).strip()


_resolved = {}


def resolve(label, now=False):
    """(info or None, exists_now) for the GN target behind `label`, as of $PLANTER_CHANGE_BASE (falling
    back to the file on disk), or as on disk (falling back to the base) when `now` is set."""
    key = (label, now)
    if key in _resolved:
        return _resolved[key]
    path, name = split(label)
    gn_rel = f"{path}/BUILD.gn"
    first, second = ("new", "old") if now else ("old", "new")
    text, targets = parsed(gn_rel, first)
    if text is None:
        text, targets = parsed(gn_rel, second)
    info = None
    t = targets.get(name)
    if t:
        body = "\n".join(t["bodies"])
        sentinel = SENTINEL.search(text)
        is_shared_lib = t["tmpl"] in (
            "shared_library",
            "sdk_shared_library",
            "loadable_module",
            "rustc_dylib",
            "rustc_cdylib",
        ) or (
            t["tmpl"] == "zx_library"
            and bool(re.search(r'\bsdk\s*=\s*"shared"', body))
        )
        link = []
        for attr in LINK_ATTRS:
            if attr == "libs" and is_shared_lib:
                continue
            m = re.search(r"(?<![\w.])%s\s*\+?=\s*(\[[^\]]*\]|true)" % attr, body)
            if m:
                link.append(f"{attr} = {compact(m.group(1))}")
        # Include source_deps so renamed_binary targets generated from fx_packaged_binary
        # forward reachability to their underlying executable/test binary in reach().
        gn_vars = t.get("vars") or {}
        lists = gn_lists(
            body,
            (
                "public_deps",
                "deps",
                "source_deps",
                "public_configs",
                "all_dependent_configs",
            ),
            gn_vars,
        )

        def labels(attr):
            return {l for l in (normalize(s, path) for s in lists.get(attr, [])) if l}

        unexpanded_pc = False
        for pc_m in re.finditer(r"(?<![\w.])public_configs\s*\+?=\s*", body):
            idx = pc_m.end()
            while idx < len(body) and body[idx].isspace():
                idx += 1
            if idx >= len(body):
                unexpanded_pc = True
                break
            if body[idx] == "[":
                end_b = closing(body, idx)
                if body[end_b + 1 :].lstrip().startswith("+"):
                    unexpanded_pc = True
                    break
            else:
                id_m = re.match(r"[A-Za-z_]\w*", body[idx:])
                if not id_m or id_m.group(0) not in gn_vars:
                    unexpanded_pc = True
                    break
        raw_pc = lists.get("public_configs", [])
        has_pc = bool(re.search(r"(?<![\w.])public_configs\s*\+?=", body))
        opaque_pc = unexpanded_pc or (
            has_pc and (not raw_pc or any(normalize(s, path) is None for s in raw_pc))
        )

        pkg_name_m = re.search(r'\bpackage_name\s*=\s*"([^"$]+)"', body)
        attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", body))
        info = {
            "tmpl": t["tmpl"],
            "above_sentinel": bool(sentinel) and t["start"] < sentinel.start(),
            "link": link,
            "public_configs": labels("public_configs"),
            "opaque_public_configs": opaque_pc,
            "all_dependent_configs": labels("all_dependent_configs"),
            "public_deps": labels("public_deps"),
            "deps": labels("deps"),
            "source_deps": labels("source_deps"),
            "forwarding_group": t["tmpl"] == "group" and attrs <= FORWARDING_ONLY_ATTRS and not link,
            "package_name": pkg_name_m.group(1) if pkg_name_m else name,
        }
    now_bazel = new_text(f"{path}/BUILD.bazel") or ""
    in_bazel = bool(re.search(r'\bname\s*=\s*"%s"' % re.escape(name), now_bazel))
    in_gn_now = name in parsed(gn_rel, "new")[1]
    exists_now = in_bazel or in_gn_now
    if info is not None:
        info["gn_only"] = not in_bazel
        info["in_gn_now"] = in_gn_now
        info["in_bazel_now"] = in_bazel
    _resolved[key] = (info, exists_now)
    return _resolved[key]


# Expand a pure GN forwarding group (transitively up to bounded depth) to the
# underlying targets it forwards when all forwarded targets reach `stop_set`.
def expand_forwarding_group(label, stop_set, seen=()):
    if label in stop_set:
        return {label}
    if label in seen or len(seen) >= 6:
        return set()
    info, _ = resolve(label)
    if not info or not info["forwarding_group"]:
        return set()
    direct = info["public_deps"] | info["deps"]
    if not direct:
        return set()
    expanded = set()
    next_seen = seen + (label,)
    for dep in sorted(direct):
        if dep in stop_set:
            expanded.add(dep)
            continue
        sub = expand_forwarding_group(dep, stop_set, next_seen)
        if not sub:
            return set()
        expanded.update(sub)
    return expanded


# Collect removed pure forwarding groups whose forwarded targets are all present
# in `new_all`, along with the set of added labels accounted for by those groups.
def collect_expanded_forwarding_groups(removed_labels, new_all):
    expanded_groups = {}
    accounted = set()
    for rem_lbl in sorted(removed_labels):
        exp = expand_forwarding_group(rem_lbl, new_all)
        if exp:
            expanded_groups[rem_lbl] = exp
            accounted.update(exp)
    return expanded_groups, accounted


_FILE_STAGING_TEMPLATES = (
    "host_test_data",
    "resource",
    "resource_group",
    "resource_tree",
)


# Filter added dependencies to compatible non-test GN targets in the same package
# when checking for sibling library wrapper substitutions.
def non_test_substitutes(added_labels, dep_pkg, removed_info=None):
    removed_tmpl = removed_info["tmpl"] if removed_info else ""
    out = []
    for cand in sorted(added_labels):
        if split(cand)[0] != dep_pkg:
            continue
        cand_info, _ = resolve(cand, now=True)
        cand_tmpl = cand_info["tmpl"] if cand_info else ""
        if cand_tmpl in GN_TEST_TEMPLATES or cand_tmpl in (
            "bazel_test_suite",
            "renamed_binary",
        ):
            continue
        # File-staging targets (`host_test_data`, `resource`, `resource_group`,
        # `resource_tree`) never substitute for compiled library/binary targets
        # (or vice versa).
        if (removed_tmpl in _FILE_STAGING_TEMPLATES) != (
            cand_tmpl in _FILE_STAGING_TEMPLATES
        ):
            continue
        out.append(cand)
    return out


# Check whether a removed dependency is a migrated GN test target (without link-affecting
# settings) or a deleted child forwarding group (whose reachability is verified by
# cq_reachability and tests_json_parity).
def is_migrated_test_or_group_dep(info, in_gn_now):
    if not info:
        return False
    if info["tmpl"] in GN_TEST_TEMPLATES and not info["link"] and not in_gn_now:
        return True
    if info["forwarding_group"] and not in_gn_now:
        return True
    return False


# Check whether a GN group dependency on a migrated test target was replaced by
# bazel_test_suite and/or a renamed dual-build GN test package (<package_name>-gn).
def is_migrated_test_group_dep(group_tmpl, label, info, old_all, subs, new_all):
    if group_tmpl != "group" or not info or not subs:
        return False
    if info["tmpl"] not in GN_TEST_TEMPLATES or info["link"] or info.get("in_gn_now"):
        return False
    dep_pkg = split(label)[0]
    if not new_text(f"{dep_pkg}/BUILD.bazel"):
        return False
    old_pkg_names = set()
    for old_lbl in old_all:
        if split(old_lbl)[0] != dep_pkg:
            continue
        oi, _ = resolve(old_lbl)
        if oi and oi["tmpl"] in GN_TEST_PACKAGE_TEMPLATES:
            old_pkg_names.add(oi["package_name"])
    allowed_gn_pkg_names = old_pkg_names | {f"{p}-gn" for p in old_pkg_names}
    has_suite = False
    for s in subs:
        si, _ = resolve(s, now=True)
        if not si:
            return False
        if si["tmpl"] == "bazel_test_suite":
            has_suite = True
        elif (
            si["tmpl"] in GN_TEST_PACKAGE_TEMPLATES
            and si["package_name"] in allowed_gn_pkg_names
        ):
            continue
        else:
            return False
    if info["tmpl"] in GN_TEST_PACKAGE_TEMPLATES:
        expected_names = {info["package_name"], f"{info['package_name']}-gn"}
        for n_lbl in new_all:
            if split(n_lbl)[0] != dep_pkg:
                continue
            ni, _ = resolve(n_lbl, now=True)
            if (
                ni
                and ni["tmpl"] in GN_TEST_PACKAGE_TEMPLATES
                and ni["package_name"] in expected_names
            ):
                return True
    return bool(info.get("in_bazel_now") or has_suite)


def reach(labels):
    """Labels reached from `labels` through GN groups and `renamed_binary` wrappers as currently
    defined: ({label: [(first hop, whether public configs are forwarded along the way)]},
    all_dependent_configs of those groups)."""
    reached, adc, seen = {}, set(), set()
    todo = [(l, l, True) for l in labels]
    while todo:
        label, hop, forwards = todo.pop()
        if (label, forwards) in seen:
            continue
        seen.add((label, forwards))
        info, _ = resolve(label, now=True)
        if not info or info["tmpl"] not in ("group", "renamed_binary"):
            continue
        if info["tmpl"] == "group":
            adc |= info["all_dependent_configs"]
            edge_kinds = ("public_deps", "deps")
        else:
            edge_kinds = ("source_deps", "deps")
        for kind in edge_kinds:
            for dep in sorted(info.get(kind, ())):
                fwd = forwards and kind == "public_deps"
                reached.setdefault(dep, []).append((hop, fwd))
                todo.append((dep, hop, fwd))
    return reached, adc


def spelled(label, pkg):
    """The ways a label can be written in a BUILD file of package pkg."""
    path, name = split(label)
    forms = [f'"//{path}:{name}"', f"path_overwrite://{path}:{name}"]
    if name == os.path.basename(path):
        forms += [f'"//{path}"', f"path_overwrite://{path}\n"]
    if path == pkg:
        forms += [f'":{name}"', f"path_overwrite::{name}"]
    m = re.match(r"^//third_party/rust_crates:(.+)$", label)
    if m:
        forms.append(f'"//third_party/rust_crates/vendor:{m.group(1)}"')
    return forms


def locate(pkg, labels, fallback_file, fallback_line):
    """File and line in BUILD.bazel (preferred, it is what the coder edits) or BUILD.gn naming a label."""
    for fname in ("BUILD.bazel", "BUILD.gn"):
        text = new_text(f"{pkg}/{fname}")
        if not text:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if any(f in line + "\n" for l in labels for f in spelled(l, pkg)):
                return f"{pkg}/{fname}", n
    return fallback_file, fallback_line


# Check whether a GN config() defines only C/C++ header or compile-time settings
# (and no linker flags, libraries, Rust flags, or empty marker configs).
def is_cc_compile_only_config(cfg_label, allow_cc_flags=False, seen=()):
    if not cfg_label or cfg_label in seen or len(seen) >= 6:
        return False
    cfg_pkg, cfg_name = split(cfg_label)
    gn_rel = f"{cfg_pkg}/BUILD.gn"
    text, targets = parsed(gn_rel, "old")
    if text is None:
        text, targets = parsed(gn_rel, "new")
    cfg_t = targets.get(cfg_name)
    if not cfg_t or cfg_t["tmpl"] != "config":
        return False
    cfg_body = "\n".join(cfg_t["bodies"])
    attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", cfg_body))
    flag_attrs = attrs - CONFIG_META_ATTRS
    if not flag_attrs:
        return False
    allowed = (
        CC_COMPILE_CONFIG_ATTRS if allow_cc_flags else CC_INCLUDE_CONFIG_ATTRS
    ) | {"configs"}
    if not (flag_attrs <= allowed):
        return False
    if "configs" in flag_attrs:
        if re.search(r"(?<![\w.])configs\s*\+?=\s*\[[^\]]*\]\s*\+", cfg_body):
            return False
        raw_sub = gn_lists(cfg_body, ("configs",), cfg_t.get("vars")).get("configs", [])
        if not raw_sub:
            return False
        sub_labels = [normalize(s, cfg_pkg) for s in raw_sub]
        if any(l is None for l in sub_labels):
            return False
        next_seen = seen + (cfg_label,)
        if not all(
            is_cc_compile_only_config(l, allow_cc_flags, next_seen) for l in sub_labels
        ):
            return False
    return True


# Filter a removed dependency's link attributes to those that actually affect linking
# of the migrated target or its GN dependents.
def effective_dropped_link_settings(info, is_public_edge, is_rust_target):
    if not info or not info["link"]:
        return []
    ignore_public_configs = (
        not is_public_edge
        and not info.get("opaque_public_configs")
        and bool(info["public_configs"])
        and all(
            is_cc_compile_only_config(c, allow_cc_flags=is_rust_target)
            for c in info["public_configs"]
        )
    )
    if not ignore_public_configs:
        return info["link"]
    return [item for item in info["link"] if not item.startswith("public_configs =")]


def describe(label, info, link_override=None):
    if info is None:
        return f"`{label}`"
    where = []
    if info["gn_only"]:
        where.append(
            "defined only in GN"
            + (
                f" (above the BAZEL2GN SENTINEL of {split(label)[0]}/BUILD.gn)"
                if info["above_sentinel"]
                else ""
            )
        )
    shown_link = info["link"] if link_override is None else link_override
    if shown_link:
        where.append("carrying " + "; ".join(f"`{l}`" for l in shown_link))
    return (
        f"`{label}` (`{info['tmpl']}`"
        + (", " + ", ".join(where) if where else "")
        + ")"
    )


def rust_forwarding_remedy(label, configs, pkg):
    name = split(label)[1]
    listed = ", ".join(f'"{c}"' for c in sorted(configs)) or "<its public_configs>"
    return (
        "bazel2gn emits Rust deps as private GN `deps` and cannot generate a `public_deps` edge. Keep the configs "
        f"reaching GN dependents with a package-local GN group above the BAZEL2GN SENTINEL of {pkg}/BUILD.gn, e.g. "
        f'`group("{name}") {{ visibility = [ ":*" ] public_deps = [ "{label}" ] all_dependent_configs = [ {listed} ] }}` '
        "with a comment saying why, point the BUILD.bazel entry at it with "
        f"`# @bazel2gn:path_overwrite::{name}`, run `fx bazel2gn -d {pkg}` and mention the group in the CoderReport "
        "summary. Do not approximate it any other way (no extra rustc_flags or -l flags, no edits to other "
        "packages' BUILD files, no source edits)."
    )


# Blank out Rust comments and string/char literals so crate-reference scanning sees only code.
def strip_rust_comments_and_strings(text):
    out, i, n = [], 0, len(text)
    while i < n:
        if text.startswith("//", i):
            j = text.find("\n", i + 2)
            i = n if j < 0 else j
            out.append(" ")
            continue
        if text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth > 0:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            out.append(" ")
            continue
        at_boundary = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        if at_boundary:
            raw_m = re.match(r'(?:b|c)?r(#*)"', text[i:])
            if raw_m:
                close_seq = '"' + raw_m.group(1)
                j = text.find(close_seq, i + raw_m.end())
                i = n if j < 0 else j + len(close_seq)
                out.append(" ")
                continue
        if text[i] == '"' or (at_boundary and text.startswith(('b"', 'c"'), i)):
            i += 2 if text[i] != '"' else 1
            while i < n:
                if text[i] == "\\":
                    i += 2
                    continue
                if text[i] == '"':
                    i += 1
                    break
                i += 1
            out.append(" ")
            continue
        if text[i] == "'":
            ch_m = re.match(r"'(?:\\.|[^'\\])'", text[i:])
            if ch_m:
                i += ch_m.end()
                out.append(" ")
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


RUST_TOKEN_RE = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*|::|\S")
RUST_PATH_KEYWORDS = {"self", "super", "crate"}


def _rust_ident(tok):
    ident = tok[2:] if tok.startswith("r#") else tok
    if ident and (ident[0].isalpha() or ident[0] == "_"):
        return ident
    return None


# Collect root crate identifiers from a `use <tree>;` token stream starting right after `use`.
def _collect_use_root_idents(tokens, idx, used):
    n = len(tokens)
    brace_depth = 0
    at_entry_start = True
    while idx < n:
        tok = tokens[idx]
        if tok == ";" and brace_depth == 0:
            return idx + 1
        if tok == "}" and brace_depth == 0:
            return idx
        if tok == "#" and idx + 1 < n and tokens[idx + 1] == "[":
            b_depth, idx = 1, idx + 2
            while idx < n and b_depth > 0:
                if tokens[idx] == "[":
                    b_depth += 1
                elif tokens[idx] == "]":
                    b_depth -= 1
                idx += 1
            continue
        # Handle `}` and `,` before `at_entry_start` so trailing commas in
        # `use { a, b, };` close `brace_depth` instead of leaving it non-zero.
        if tok == "}" and brace_depth > 0:
            brace_depth -= 1
            at_entry_start = False
            idx += 1
            continue
        if tok == "," and brace_depth > 0:
            at_entry_start = True
            idx += 1
            continue
        if at_entry_start:
            if tok == "::":
                idx += 1
                continue
            if tok == "{":
                brace_depth += 1
                idx += 1
                continue
            ident = _rust_ident(tok)
            if ident and ident not in RUST_PATH_KEYWORDS:
                used.add(ident)
            at_entry_start = False
            idx += 1
            continue
        if tok == "{":
            sub_depth, idx = 1, idx + 1
            while idx < n and sub_depth > 0:
                if tokens[idx] == "{":
                    sub_depth += 1
                elif tokens[idx] == "}":
                    sub_depth -= 1
                idx += 1
            continue
        idx += 1
    return idx


# Extract external crate identifiers referenced in Rust source text.
def extract_rust_used_crates(text, include_cfg_test=True):
    src = text if include_cfg_test else pcl.strip_rust_cfg_test(text)
    tokens = RUST_TOKEN_RE.findall(strip_rust_comments_and_strings(src))
    used = set()
    i, n = 0, len(tokens)
    while i < n:
        tok = tokens[i]
        if tok == "use":
            # Skip Rust 2024 precise-capturing `impl Trait + use<'a, T>` bounds.
            if i + 1 < n and tokens[i + 1] == "<":
                i += 2
                continue
            i = _collect_use_root_idents(tokens, i + 1, used)
            continue
        if tok == "extern" and i + 2 < n and tokens[i + 1] == "crate":
            ident = _rust_ident(tokens[i + 2])
            if ident and ident not in RUST_PATH_KEYWORDS:
                used.add(ident)
            i += 3
            continue
        if tok == "::" and i + 1 < n:
            ident = _rust_ident(tokens[i + 1])
            if ident:
                prev = tokens[i - 1] if i > 0 else ""
                if not _rust_ident(prev) and prev not in (">", ")", "]"):
                    if ident not in RUST_PATH_KEYWORDS:
                        used.add(ident)
                i += 2
                continue
        ident = _rust_ident(tok)
        if ident and i + 1 < n and tokens[i + 1] == "::":
            if (i == 0 or tokens[i - 1] != "::") and ident not in RUST_PATH_KEYWORDS:
                used.add(ident)
        i += 1
    return used


_vendor_aliases = None


# Map //third_party/rust_crates/vendor alias names to their underlying Rust crate identifiers.
def vendor_crate_aliases():
    global _vendor_aliases
    if _vendor_aliases is None:
        _vendor_aliases = {}
        vtext = new_text("third_party/rust_crates/vendor/BUILD.bazel")
        if vtext:
            code = gn_code(vtext)
            for m in re.finditer(r"(?m)^\s*alias\s*\(", code):
                p_open = m.end() - 1
                body = code[p_open + 1 : closing(code, p_open)]
                nm = re.search(r'(?<![\w.])name\s*=\s*"([^"]+)"', body)
                am = re.search(r'(?<![\w.])actual\s*=\s*"[^"]+:([^":]+)"', body)
                if nm and am:
                    _vendor_aliases[nm.group(1)] = am.group(1).replace("-", "_")
    return _vendor_aliases


# Inspect `<dep_pkg>/BUILD.bazel` for an alias target or explicit `crate_name` override.
def bazel_target_crate_info(dep_pkg, t_name):
    bzl_text = new_text(f"{dep_pkg}/BUILD.bazel")
    if not bzl_text:
        return None
    code = gn_code(bzl_text)
    for m in re.finditer(r"(?m)^\s*([A-Za-z_]\w*)\s*\(", code):
        rule_kind = m.group(1)
        p_open = m.end() - 1
        body = code[p_open + 1 : closing(code, p_open)]
        nm = re.search(r'(?<![\w.])name\s*=\s*"([^"]+)"', body)
        if not nm or nm.group(1) != t_name:
            continue
        if rule_kind == "alias":
            am = re.search(r'(?<![\w.])actual\s*=\s*"([^"]+)"', body)
            if am:
                act = normalize(am.group(1), dep_pkg)
                if act:
                    return ("alias", act)
        cm = re.search(r'(?<![\w.])crate_name\s*=\s*"([^"]+)"', body)
        if cm:
            return ("crate", cm.group(1).replace("-", "_"))
        return None
    return None


_crate_idents_cache = {}


# Resolve candidate Rust crate identifiers exported by a normalized GN dependency label.
def rust_crate_idents(label, seen=()):
    if label in _crate_idents_cache:
        return _crate_idents_cache[label]
    if label in seen or len(seen) >= 6:
        return set()
    dep_pkg, t_name = split(label)
    next_seen = seen + (label,)
    idents = set()

    if dep_pkg == "third_party/rust_crates" or dep_pkg.startswith("third_party/rust_crates/"):
        alias_ident = vendor_crate_aliases().get(t_name)
        if alias_ident:
            idents.add(alias_ident)
        base = re.sub(r"-v?\d+[_.]\d+.*$", "", t_name)
        if base:
            idents.add(base.replace("-", "_"))
    elif t_name.endswith("_rust_fdomain_flex"):
        idents.add("flex_" + t_name[:-18].replace(".", "_").replace("-", "_"))
    elif t_name.endswith("_rust_flex"):
        idents.add("flex_" + t_name[:-10].replace(".", "_").replace("-", "_"))
    elif t_name.endswith("_rust_fdomain"):
        idents.add("fdomain_" + t_name[:-13].replace(".", "_").replace("-", "_"))
    elif t_name.endswith("_rust_next"):
        idents.add("fidl_next_" + t_name[:-10].replace(".", "_").replace("-", "_"))
    elif t_name.endswith("_rust_common"):
        idents.add("fidl_" + t_name[:-12].replace(".", "_").replace("-", "_") + "__common")
    elif t_name.endswith("_rust"):
        stem = t_name[:-5].replace(".", "_").replace("-", "_")
        idents.add("fidl_" + stem)
        idents.add("bind_" + stem)
    else:
        bzl_info = bazel_target_crate_info(dep_pkg, t_name)
        if bzl_info and bzl_info[0] == "alias":
            idents |= rust_crate_idents(bzl_info[1], next_seen)
        elif bzl_info and bzl_info[0] == "crate":
            idents.add(bzl_info[1])
        else:
            gn_rel_dep = f"{dep_pkg}/BUILD.gn"
            text_dep, targets_dep = parsed(gn_rel_dep, "new")
            if text_dep is None:
                text_dep, targets_dep = parsed(gn_rel_dep, "old")
            t_dep = targets_dep.get(t_name)
            if t_dep:
                t_body = "\n".join(t_dep["bodies"])
                if t_dep["tmpl"] == "group":
                    grp_lists = gn_lists(t_body, ("public_deps", "deps"), t_dep.get("vars"))
                    for raw_d in grp_lists.get("public_deps", []) + grp_lists.get("deps", []):
                        norm_d = normalize(raw_d, dep_pkg)
                        if norm_d:
                            idents |= rust_crate_idents(norm_d, next_seen)
                else:
                    nm = re.search(r'(?<![\w.])name\s*=\s*"([^"$]+)"', t_body)
                    if nm:
                        idents.add(nm.group(1).replace("-", "_"))
            if not idents:
                idents.add(t_name.replace("-", "_"))

    if not seen:
        _crate_idents_cache[label] = idents
    return idents


_rs_used_cache = {}


# Return external crate identifiers referenced in a repository-relative .rs source file.
def used_crates_in_rs_file(rel_rs, include_cfg_test=True):
    key = (rel_rs, bool(include_cfg_test))
    if key not in _rs_used_cache:
        src = new_text(rel_rs)
        _rs_used_cache[key] = (
            extract_rust_used_crates(src, include_cfg_test=include_cfg_test)
            if src
            else set()
        )
    return _rs_used_cache[key]


def _has_own_build_file(rel_dir):
    return os.path.isfile(os.path.join(workdir, rel_dir, "BUILD.gn")) or os.path.isfile(
        os.path.join(workdir, rel_dir, "BUILD.bazel")
    )


def _within_same_package(pkg, rel_rs):
    cur = os.path.dirname(rel_rs).strip("/")
    while cur and cur != pkg and cur.startswith(pkg + "/"):
        if _has_own_build_file(cur):
            return False
        cur = os.path.dirname(cur).strip("/")
    return cur == pkg


def _submod_candidate_paths(cur_rs, mod_name, explicit_paths):
    base_dir = os.path.dirname(cur_rs)
    stem = os.path.splitext(os.path.basename(cur_rs))[0]
    search_dirs = [base_dir]
    if stem not in ("lib", "main", "mod", "args"):
        search_dirs.append(os.path.join(base_dir, stem))
    cands = []
    if explicit_paths:
        for ep in explicit_paths:
            for sdir in dict.fromkeys([base_dir, *search_dirs]):
                cands.append(os.path.normpath(os.path.join(sdir, ep)).replace("\\", "/"))
    else:
        for sdir in search_dirs:
            cands.append(
                os.path.normpath(os.path.join(sdir, f"{mod_name}.rs")).replace("\\", "/")
            )
            cands.append(
                os.path.normpath(os.path.join(sdir, mod_name, "mod.rs")).replace("\\", "/")
            )
    return cands


# Resolve the repository-relative .rs files compiled for `dep_attrs` on `new_t` in `pkg`.
def target_rust_sources(pkg, new_t, dep_attrs, include_cfg_test=True):
    bodies = new_t["bodies"]
    gn_vars = new_t.get("vars") or {}
    args_only = bool(dep_attrs) and set(dep_attrs) <= {"args_deps", "args_test_deps"}
    raw_paths = []
    for body in bodies:
        if args_only:
            lists = gn_lists(body, ("args_sources",), gn_vars)
            raw_paths.extend(lists.get("args_sources", []))
            rm = re.search(r'\bargs_source_root\s*=\s*"([^"$]+)"', body)
            if rm:
                raw_paths.append(rm.group(1))
        else:
            has_args = bool(set(dep_attrs) & {"args_deps", "args_test_deps"})
            list_attrs = ("sources", "args_sources") if has_args else ("sources",)
            lists = gn_lists(body, list_attrs, gn_vars)
            for attr_name in list_attrs:
                raw_paths.extend(lists.get(attr_name, []))
            rm = re.search(r'\bsource_root\s*=\s*"([^"$]+)"', body)
            if rm:
                raw_paths.append(rm.group(1))

    if not raw_paths:
        if args_only:
            raw_paths.append("src/args.rs")
        elif new_t["tmpl"] in ("rustc_binary", "ffx_tool"):
            raw_paths.append("src/main.rs")
        elif new_t["tmpl"] != "ffx_plugin":
            raw_paths.append("src/lib.rs")

    seed_rels = []
    seen_seeds = set()
    for path_item in raw_paths:
        if not path_item.endswith(".rs") or "$" in path_item:
            continue
        rel = (
            path_item[2:].strip("/")
            if path_item.startswith("//")
            else os.path.normpath(os.path.join(pkg, path_item)).replace("\\", "/")
        )
        if not rel.startswith("..") and os.path.isfile(os.path.join(workdir, rel)):
            if rel not in seen_seeds:
                seen_seeds.add(rel)
                seed_rels.append(rel)

    cfg_test_submod_files = set()
    if not include_cfg_test and seed_rels:
        prod_submod_files = set()
        all_submod_files = set()
        for s_rs in seed_rels:
            s_txt = new_text(s_rs)
            if not s_txt:
                continue
            for m_nm, ex_p in pcl.extract_rust_submodules(s_txt, include_cfg_test=True):
                for cand in _submod_candidate_paths(s_rs, m_nm, ex_p):
                    if os.path.isfile(os.path.join(workdir, cand)):
                        all_submod_files.add(cand)
            for m_nm, ex_p in pcl.extract_rust_submodules(s_txt, include_cfg_test=False):
                for cand in _submod_candidate_paths(s_rs, m_nm, ex_p):
                    if os.path.isfile(os.path.join(workdir, cand)):
                        prod_submod_files.add(cand)
        cfg_test_submod_files = all_submod_files - prod_submod_files

    resolved = []
    seen_rs = set()
    queue = []
    for rel in seed_rels:
        if rel in cfg_test_submod_files:
            continue
        seen_rs.add(rel)
        queue.append(rel)

    # Expand in-package `mod <submod>;` declarations (including `#[path = "..."]`)
    # reachable from root source files.
    while queue:
        cur_rs = queue.pop(0)
        resolved.append(cur_rs)
        src_text = new_text(cur_rs)
        if not src_text:
            continue
        for mod_name, explicit_paths in pcl.extract_rust_submodules(
            src_text, include_cfg_test=include_cfg_test
        ):
            for cand in _submod_candidate_paths(cur_rs, mod_name, explicit_paths):
                if (
                    cand not in seen_rs
                    and cand not in cfg_test_submod_files
                    and not cand.startswith("..")
                    and _within_same_package(pkg, cand)
                    and os.path.isfile(os.path.join(workdir, cand))
                ):
                    seen_rs.add(cand)
                    queue.append(cand)

    if resolved or args_only or new_t["tmpl"] == "ffx_plugin":
        return resolved

    pkg_abs = os.path.join(workdir, pkg)
    for root, dirs, files in os.walk(pkg_abs):
        rel_root = os.path.relpath(root, workdir).replace("\\", "/").strip("/")
        dirs[:] = [d for d in dirs if not _has_own_build_file(f"{rel_root}/{d}")]
        for fname in sorted(files):
            if fname.endswith(".rs"):
                resolved.append(f"{rel_root}/{fname}")
    return resolved


# Check whether any .rs source compiled for `label` on `new_t` references `label`'s crate(s).
def target_uses_rust_dep(pkg, new_t, label):
    dep_attrs = [a for a, s in new_t["deps"].items() if label in s]
    # Non-`rustc_test` targets compile `deps`/`args_deps` without `--test` (even when
    # `with_unit_tests = true`, which only passes `--test` to `<name>_test` receiving
    # `test_deps`/`args_test_deps`), so `#[cfg(test)]`-only usage still fails
    # `-D unused-crate-dependencies` unless `label` is only in test dep attributes.
    include_cfg_test = new_t["tmpl"] == "rustc_test" or (
        bool(dep_attrs) and set(dep_attrs) <= {"test_deps", "args_test_deps"}
    )
    rs_files = target_rust_sources(
        pkg, new_t, dep_attrs, include_cfg_test=include_cfg_test
    )
    if not rs_files:
        return False
    crate_idents = rust_crate_idents(label)
    if not crate_idents:
        return False
    return any(
        crate_idents & used_crates_in_rs_file(rs, include_cfg_test=include_cfg_test)
        for rs in rs_files
    )


# Collect active labels reached from `label` through pure GN forwarding groups.
def forwarded_active_labels(label, active_set, seen=()):
    if label in seen or len(seen) >= 6:
        return set()
    info, _ = resolve(label)
    if not info or not info["forwarding_group"]:
        return set()
    matched = set()
    next_seen = seen + (label,)
    for dep in sorted(info["public_deps"] | info["deps"]):
        if dep in active_set:
            matched.add(dep)
        matched.update(forwarded_active_labels(dep, active_set, next_seen))
    return matched


# Collect base BUILD.gn labels plus targets reached from base deps via pure forwarding groups.
def collect_base_file_and_forwarded_labels(before_text, old_tgts, pkg_dir, active_set):
    labels = {
        norm_l
        for raw_s in GN_STRING.findall(gn_code(before_text))
        for norm_l in (normalize(raw_s, pkg_dir),)
        if norm_l
    }
    non_group_deps = set()
    for ot in old_tgts.values():
        is_group = ot.get("tmpl") == "group"
        for d_set in gn_deps(ot, pkg_dir).values():
            labels.update(d_set)
            if not is_group:
                non_group_deps.update(d_set)
    forwarded = set()
    for lbl in sorted(labels):
        forwarded.update(expand_forwarding_group(lbl, active_set))
    for lbl in sorted(non_group_deps):
        forwarded.update(forwarded_active_labels(lbl, active_set))
    return labels | forwarded


EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
)

candidate_dirs = {target_dir} if target_dir else set()
_env_changed = os.environ.get("PLANTER_CHANGED_FILES")
changed_all = (
    [l.strip() for l in _env_changed.splitlines() if l.strip()]
    if _env_changed is not None
    else (
        pcl.changed_files(workdir, change_base)
        if pcl is not None
        else git_lines(["diff", "--name-only", change_base])
        + git_lines(["ls-files", "--others", "--exclude-standard"])
    )
)
changed_set = set(changed_all)
for p in changed_all:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk", "build")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not (target_dir and d == os.path.dirname(target_dir) and f"{d}/BUILD.bazel" not in changed_set)
        ):
            candidate_dirs.add(d)

findings = []


def _emit(findings, category, severity, file, line, message, remediation=""):
    d = {
        "source": "gn_dep_parity",
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
    }
    if remediation:
        d["remediation"] = remediation
    findings.append(d)


SHARED_LIB_TEMPLATES = {"shared_library", "sdk_shared_library", "loadable_module"}
STATIC_LIB_TEMPLATES = {
    "source_set",
    "static_library",
    "sdk_source_set",
    "sdk_static_library",
    "library_headers",
}
BAZEL_STATIC_LIB_RULES = (
    "fx_cc_library",
    "fx_cc_library_headers",
    "idk_cc_source_set",
    "idk_cc_static_library",
    "cc_library",
)


# Determine whether a label resolves to a static or source_set C/C++ library in GN or Bazel.
def is_static_lib(label):
    info, _ = resolve(label, now=True)
    if info and info["tmpl"] in STATIC_LIB_TEMPLATES:
        return True
    dep_pkg, dep_name = split(label)
    bz = new_text(f"{dep_pkg}/BUILD.bazel") or ""
    if not bz:
        return False
    bz_clean = gn_code(bz)
    for m in re.finditer(r"(?m)^\s*(%s)\s*\(" % "|".join(BAZEL_STATIC_LIB_RULES), bz_clean):
        end_p = closing(bz_clean, m.end() - 1)
        call_body = bz_clean[m.end() : end_p]
        if re.search(r'\bname\s*=\s*"%s"' % re.escape(dep_name), call_body):
            return True
    return False


# Format remediation for an unintended substitution of a static library for a GN shared library.
def shared_to_static_remedy(pkg, gn_lbl, gn_tmpl, bz_lbls):
    dep_pkg = split(gn_lbl)[0]
    bz_str = ", ".join(f"`{b}`" for b in bz_lbls)
    return (
        f"Do not substitute static library target(s) ({bz_str}) for `{gn_lbl}` (`{gn_tmpl}`), "
        f"even with `# @bazel2gn:path_overwrite:`. Unless `{gn_lbl}` itself is migrated to "
        f"`fx_cc_shared_library` or `idk_cc_shared_library` in `//{dep_pkg}/BUILD.bazel`, "
        f"treat `{gn_lbl}` as an unmigrated `{gn_tmpl}` blocker: keep its dependent target(s) "
        f"unmigrated in `{pkg}/BUILD.gn` (or leave `//{pkg}` unmigrated if no other convertible "
        f"targets remain) and revert any visibility widening on {bz_str} in `//{dep_pkg}`."
    )


# Check BUILD.bazel for static library dependencies substituted for a GN shared_library
# (either masked via `# @bazel2gn:path_overwrite:` or in a deleted-BUILD.gn / skipped target).
def check_bazel_shared_to_static_substitutions(pkg, old_targets, reported_pairs):
    bz_rel = f"{pkg}/BUILD.bazel"
    bz_text = new_text(bz_rel)
    if not bz_text:
        return
    old_bz_text = old_text(bz_rel) or ""
    old_bz_flat = {
        re.sub(r"\s+", "", l) for l in old_bz_text.splitlines() if l.strip()
    }
    ow_re = re.compile(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:([^\s#]+)')
    flagged_lines = set()
    for lineno, line_txt in enumerate(bz_text.splitlines(), 1):
        if pkg != target_dir and old_bz_flat and re.sub(r"\s+", "", line_txt) in old_bz_flat:
            continue
        m = ow_re.match(line_txt)
        if not m:
            continue
        bz_raw, gn_raw = m.group(1).strip(), m.group(2).strip().strip('"')
        bz_lbl = normalize(bz_raw, pkg)
        gn_lbl = normalize(gn_raw, pkg)
        if not bz_lbl or not gn_lbl or bz_lbl == gn_lbl:
            continue
        gn_info, _ = resolve(gn_lbl)
        if not gn_info or gn_info["tmpl"] not in SHARED_LIB_TEMPLATES:
            continue
        if not is_static_lib(bz_lbl):
            continue
        flagged_lines.add(lineno)
        reported_pairs.add((gn_lbl, bz_lbl))
        msg = (
            f"`{bz_rel}` depends on static library `{bz_lbl}` in Bazel while overwriting "
            f"the GN dependency to {describe(gn_lbl, gn_info)} via "
            f"`# @bazel2gn:path_overwrite:{gn_raw}`. Linking against a static/source_set "
            f"target in Bazel while GN links dynamically against `{gn_info['tmpl']}` "
            f"`{gn_lbl}` is an unintended linkage and behavior change."
        )
        _emit(
            findings,
            "dep_label_substituted",
            "ERROR",
            bz_rel,
            lineno,
            msg,
            shared_to_static_remedy(pkg, gn_lbl, gn_info["tmpl"], [bz_lbl]),
        )

    old_all_deps = set()
    old_shared_deps = {}
    for ot in old_targets.values():
        for d_set in gn_deps(ot, pkg).values():
            for lbl in d_set:
                old_all_deps.add(lbl)
                gi, _ = resolve(lbl)
                if gi and gi["tmpl"] in SHARED_LIB_TEMPLATES:
                    old_shared_deps[lbl] = gi
    if not old_shared_deps:
        return
    bz_clean = gn_code(bz_text)
    # Collect direct (non-path_overwrite) BUILD.bazel labels and surviving BUILD.gn
    # labels so pre-existing sibling static libraries are not mistaken for shared_library
    # substitutions when the shared_library dependency itself is still preserved.
    bz_direct_labels = set()
    for raw_ln, clean_ln in zip(bz_text.splitlines(), bz_clean.splitlines()):
        if ow_re.match(raw_ln):
            continue
        for s_lit in GN_STRING.findall(clean_ln):
            norm_s = normalize(s_lit, pkg)
            if norm_s:
                bz_direct_labels.add(norm_s)
    after_gn_txt = new_text(f"{pkg}/BUILD.gn")
    retained_gn_deps = set()
    if after_gn_txt:
        for nt in gn_targets(after_gn_txt, pkg=pkg, version="new").values():
            for d_set in gn_deps(nt, pkg).values():
                retained_gn_deps.update(d_set)
    for lineno, (raw_line, clean_line) in enumerate(
        zip(bz_text.splitlines(), bz_clean.splitlines()), 1
    ):
        if lineno in flagged_lines:
            continue
        if pkg != target_dir and old_bz_flat and re.sub(r"\s+", "", raw_line) in old_bz_flat:
            continue
        for raw_s in GN_STRING.findall(clean_line):
            bz_lbl = normalize(raw_s, pkg)
            if not bz_lbl or not is_static_lib(bz_lbl):
                continue
            dep_pkg, bz_name = split(bz_lbl)
            for gn_lbl, gn_info in sorted(old_shared_deps.items()):
                gn_pkg, gn_name = split(gn_lbl)
                if gn_pkg != dep_pkg or gn_name == bz_name:
                    continue
                if gn_lbl in bz_direct_labels:
                    continue
                if bz_lbl in old_all_deps and gn_lbl in retained_gn_deps:
                    continue
                if (gn_lbl, bz_lbl) in reported_pairs:
                    continue
                reported_pairs.add((gn_lbl, bz_lbl))
                msg = (
                    f"`{bz_rel}` depends on static library `{bz_lbl}` in place of "
                    f"{describe(gn_lbl, gn_info)}. Substituting a static/source_set target "
                    f"for a GN `{gn_info['tmpl']}` changes linkage from dynamic to static "
                    "(an unintended behavior change)."
                )
                _emit(
                    findings,
                    "dep_label_substituted",
                    "ERROR",
                    bz_rel,
                    lineno,
                    msg,
                    shared_to_static_remedy(pkg, gn_lbl, gn_info["tmpl"], [bz_lbl]),
                )


for pkg in sorted(candidate_dirs):
    gn_rel = f"{pkg}/BUILD.gn"
    before = old_text(gn_rel)
    if not before:
        continue
    old_targets = gn_targets(before, pkg=pkg, version="old")
    reported_shared_to_static = set()
    after = new_text(gn_rel)
    if after is None:
        # BUILD.gn deleted (full removal): still verify BUILD.bazel did not swap a
        # GN shared_library dependency for a sibling static library target.
        check_bazel_shared_to_static_substitutions(pkg, old_targets, reported_shared_to_static)
        continue
    new_file = gn_rel
    new_targets = {
        n: {
            "line": t["line"],
            "tmpl": t["tmpl"],
            "deps": gn_deps(t, pkg),
            "bodies": t["bodies"],
            "vars": t.get("vars"),
        }
        for n, t in gn_targets(after, pkg=pkg, version="new").items()
    }
    package_now = set().union(*[set().union(*t["deps"].values()) for t in new_targets.values() if t["deps"]])
    has_bazel_test_suite = any(t["tmpl"] == "bazel_test_suite" for t in new_targets.values())

    # Identify pure single-target forwarding `group("<outer>")` targets in `old_targets`
    # whose same-package target `:<inner>` was folded directly into `<outer>` in `new_targets`.
    folded_from_group = {}
    folded_into_group = {}
    for outer, old_g in sorted(old_targets.items()):
        if old_g["tmpl"] != "group" or outer not in new_targets:
            continue
        if new_targets[outer]["tmpl"] == "group":
            continue
        g_body = "\n".join(old_g["bodies"])
        g_attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", g_body))
        if not g_attrs or not (g_attrs <= FORWARDING_ONLY_ATTRS):
            continue
        if any(re.search(r"(?<![\w.])%s\s*\+?=" % a, g_body) for a in LINK_ATTRS):
            continue
        raw_lists = gn_lists(g_body, ("public_deps", "deps"), old_g.get("vars"))
        raw_deps = [s for items in raw_lists.values() for s in items]
        if len(raw_deps) != 1 or "(" in raw_deps[0]:
            continue
        norm_dep = normalize(raw_deps[0], pkg)
        if not norm_dep:
            continue
        dep_pkg, inner = split(norm_dep)
        if dep_pkg != pkg or inner == outer or inner not in old_targets or inner in new_targets:
            continue
        if old_targets[inner]["tmpl"] != new_targets[outer]["tmpl"]:
            continue
        folded_from_group[outer] = inner
        folded_into_group[inner] = outer

    for name, old_t in sorted(old_targets.items()):
        if name in folded_into_group:
            continue
        if name in folded_from_group:
            old_t = old_targets[folded_from_group[name]]
        # Skip targets replaced by a Bazel host binary, ffx tool, test suite, or package bridge in GN.
        if (
            name not in new_targets
            or new_targets[name]["tmpl"]
            in (
                "bazel_host_tool",
                "bazel_py_host_tool",
                "bazel_ffx_tool",
                "bazel_test_suite",
                "bazel_fuchsia_package",
            )
            or (
                has_bazel_test_suite
                and new_targets[name]["tmpl"] == "group"
                and old_t["tmpl"] in ("fuchsia_unittest_package", "fuchsia_test_package")
            )
        ):
            continue
        tmpl = old_t["tmpl"]
        old_deps = gn_deps(old_t, pkg)
        new_deps = new_targets[name]["deps"]
        old_all = set().union(*old_deps.values()) if old_deps else set()
        new_direct = set().union(*new_deps.values()) if new_deps else set()
        new_public = new_deps.get("public_deps", set())
        reached, adc = reach(sorted(new_direct - old_all))
        new_all = new_direct | set(reached)
        removed, added = old_all - new_all, new_all - old_all
        expanded_groups, accounted_by_forwarding = collect_expanded_forwarding_groups(
            removed, new_all
        )
        unaccounted_added = added - accounted_by_forwarding
        target_desc = f'`{tmpl}("{name}")` in //{pkg}'
        rust = tmpl.startswith("rust")

        def with_hops(labels):
            """labels plus the groups through which the target reaches them."""
            return labels + sorted(
                {h for l in labels for h, _ in reached.get(l, ()) if h not in labels}
            )

        for label in sorted(removed):
            # Pure forwarding groups expanded to their forwarded target(s) are preserved.
            if label in expanded_groups:
                continue
            dep_pkg = split(label)[0]
            kinds = [a for a in GN_DEP_ATTRS if label in old_deps.get(a, ())]
            is_test_only_dep = bool(kinds) and set(kinds) <= {"test_deps", "args_test_deps"}
            if is_test_only_dep and (
                label in package_now or bool(expand_forwarding_group(label, package_now))
            ):
                # Moved to another target (or expanded from a pure forwarding group on the
                # decomposed `<name>_test` / `<name>_args_test` target) when `with_unit_tests`
                # or `args_with_unit_tests` is replaced by a standalone test target.
                continue
            # `test_deps` / `args_test_deps` only reach GN's generated `<name>_test` /
            # `<name>_args_test` executable; when GN no longer builds inline unit tests on
            # this target, its production deps neither substitute for nor drop link settings
            # of test-only entries.
            gn_unit_test_gone = is_test_only_dep and not any(
                re.search(r"\b(?:with_unit_tests|args_with_unit_tests)\s*=\s*true\b", b)
                for b in new_targets[name]["bodies"]
            )
            info, exists_now = resolve(label)
            in_gn_now = (
                info["in_gn_now"]
                if info is not None
                else (split(label)[1] in parsed(f"{dep_pkg}/BUILD.gn", "new")[1])
            )
            if is_migrated_test_or_group_dep(info, in_gn_now):
                continue
            # A file-staging `resource` / `resource_group` / `host_test_data` dependency moved
            # from a compiled binary/test target to its companion component/package target in
            # the same package (because Bazel binary rules reject resource groups) is preserved.
            if (
                info
                and info["tmpl"] in _FILE_STAGING_TEMPLATES
                and label in package_now
            ):
                continue
            raw_subs = sorted(l for l in unaccounted_added if split(l)[0] == dep_pkg)
            if is_migrated_test_group_dep(tmpl, label, info, old_all, raw_subs, new_all):
                continue
            subs = non_test_substitutes(unaccounted_added, dep_pkg, info)
            public = "public_deps" in kinds
            file, line = locate(
                pkg, with_hops(subs) or [label], new_file, new_targets[name]["line"]
            )
            # When a GN fuchsia_unittest_package/fuchsia_test_package("<name>") is migrated to
            # fx_package(name = "<name>-package", package_name = "<name>", test_components = ...) +
            # fx_test(name = "<name>"), bazel2gn generates fuchsia_test_package("<name>-package")
            # (and no GN target "<name>"), so group("tests") updates its dep to the generated package.
            new_dep_pkg_targets = parsed(f"{dep_pkg}/BUILD.gn", "new")[1]
            if (
                info
                and info["tmpl"] in ("fuchsia_unittest_package", "fuchsia_test_package")
                and split(label)[1] not in new_dep_pkg_targets
                and any(
                    new_dep_pkg_targets.get(split(s)[1], {}).get("tmpl")
                    in ("fuchsia_test_package", "fuchsia_unittest_package")
                    for s in raw_subs
                )
            ):
                continue
            if subs and in_gn_now and not gn_unit_test_gone:
                is_shared_to_static = (
                    info is not None
                    and info["tmpl"] in SHARED_LIB_TEMPLATES
                    and any(is_static_lib(s) for s in subs)
                )
                if is_shared_to_static:
                    for s in subs:
                        reported_shared_to_static.add((label, s))
                    sub_remedy = shared_to_static_remedy(pkg, label, info["tmpl"], subs)
                elif public and rust and info and info["public_configs"]:
                    sub_remedy = (
                        "Keep the exact original GN label and its public forwarding. "
                        + rust_forwarding_remedy(label, info["public_configs"], pkg)
                    )
                else:
                    sub_remedy = (
                        f"Keep the exact original GN label: in {pkg}/BUILD.bazel depend on the "
                        f"Bazel target that holds the code and put "
                        f"`# @bazel2gn:path_overwrite:{label}` on that list entry, then run "
                        f"`fx bazel2gn -d {pkg}`."
                    )
                _emit(
                    findings,
                    "dep_label_substituted",
                    "ERROR",
                    file,
                    line,
                    f"{target_desc} depended on {describe(label, info)} via `{'`/`'.join(kinds)}`, but the "
                    f"migrated target depends on {', '.join(f'`{s}`' for s in subs)} instead. That is a "
                    "different GN target of the same package: swapping a dependency for a sibling target changes "
                    "what GN compiles and links even though Bazel builds pass (e.g. dropping a wrapper whose "
                    "`public_configs` link the static C++ standard library leaves GN test executables and GN "
                    "dependents with undefined `operator new`/`operator delete`)."
                    + (
                        f" It was a `public_deps` edge, so GN also forwarded its public configs to every "
                        f"dependent of `{name}`."
                        if public
                        else ""
                    ),
                    sub_remedy,
                )
            elif exists_now and not subs and not gn_unit_test_gone and effective_dropped_link_settings(
                info, public, rust or tmpl in ("ffx_plugin", "ffx_tool")
            ):
                dropped_link = effective_dropped_link_settings(
                    info, public, rust or tmpl in ("ffx_plugin", "ffx_tool")
                )
                _emit(
                    findings,
                    "dep_link_settings_dropped",
                    "ERROR",
                    file,
                    line,
                    f"{target_desc} no longer depends on {describe(label, info, dropped_link)} (was "
                    f"`{'`/`'.join(kinds)}`). Its link-affecting settings no longer reach this target"
                    + (" or its GN dependents" if public else "")
                    + ", so GN links of test executables and binaries can fail even though Bazel builds pass.",
                    f"Restore the dependency with its exact GN label (put `# @bazel2gn:path_overwrite:{label}` on "
                    f"the matching BUILD.bazel entry if Bazel needs a different label) and run `fx bazel2gn -d {pkg}`.",
                )
            elif label not in package_now:
                _emit(
                    findings,
                    "dep_label_removed",
                    "INFO",
                    new_file,
                    new_targets[name]["line"],
                    f"{target_desc} no longer depends on `{label}` (was `{'`/`'.join(kinds)}`) "
                    f"and no other target of //{pkg} does. Make sure it was really unused "
                    "(e.g. an unused `test_deps` entry, or a `test_deps` entry moved to a "
                    "`# @bazel2gn:skip` Bazel test) and say so in the CoderReport summary.",
                )

        for label in sorted(old_deps.get("public_deps", set()) & new_all):
            if label in new_public or any(h in new_public and fwd for h, fwd in reached.get(label, ())):
                continue
            info, _ = resolve(label)
            if not info or not info["public_configs"] or info["public_configs"] <= adc:
                continue
            via = sorted({h for h, _ in reached.get(label, ())})
            file, line = locate(pkg, with_hops([label]), new_file, new_targets[name]["line"])
            _emit(
                findings,
                "public_dep_forwarding_lost",
                "ERROR",
                file,
                line,
                f"`{label}` was a `public_deps` edge of {target_desc} and is now reached only through private "
                "edges"
                + (f" (via {', '.join(f'`{h}`' for h in via)})" if via else "")
                + f", but it has public_configs {', '.join(f'`{c}`' for c in sorted(info['public_configs']))}. "
                f"GN forwards those to the dependents of `{name}` only through `public_deps` (or when a dependency "
                f"re-exports them as `all_dependent_configs`), so GN tests and binaries in other packages that use "
                f"`{name}` lose them (e.g. link flags, giving undefined symbols; include dirs).",
                "For C/C++ targets, list it in `deps` rather than `implementation_deps` in BUILD.bazel (bazel2gn "
                "emits those as GN `public_deps`)."
                if not rust
                else rust_forwarding_remedy(label, info["public_configs"], pkg),
            )

    # Also check BUILD.bazel for static library dependencies substituted for a GN shared_library
    # (for example when masked in BUILD.gn by `# @bazel2gn:path_overwrite:`).
    check_bazel_shared_to_static_substitutions(pkg, old_targets, reported_shared_to_static)

    # Check that migrated Rust targets (including decomposed unit test targets) do not
    # add external crate dependencies absent from the base BUILD.gn, which fail compilation
    # under `-D unused-crate-dependencies` (for example after an upstream rebase removes a
    # dependency from BUILD.gn while BUILD.bazel still lists it).
    rust_tmpls = {
        "ffx_plugin",
        "ffx_tool",
        "rustc_library",
        "rustc_binary",
        "rustc_macro",
        "rustc_test",
        "rustc_dylib",
        "rustc_cdylib",
        "rustc_staticlib",
    }
    implicit_rust_deps = {
        "//src/lib/fuchsia:fuchsia",
        "//src/lib/dso/rust:rust",
        "//sdk/rust/zx:zx",
        "//build/config/rust:proc_macro_test",
    }
    old_file_labels = collect_base_file_and_forwarded_labels(
        before, old_targets, pkg, package_now
    )
    old_pkgs = {split(l)[0] for l in old_file_labels}
    reported_extra = set()
    for t_name, new_t in sorted(new_targets.items()):
        if new_t["tmpl"] not in rust_tmpls:
            continue
        t_deps = set().union(*new_t["deps"].values()) if new_t["deps"] else set()
        for label in sorted(t_deps):
            dep_pkg = split(label)[0]
            if (
                dep_pkg == pkg
                or dep_pkg in old_pkgs
                or label in old_file_labels
                or label in implicit_rust_deps
                or dep_pkg.startswith("sdk/categories")
                or label in reported_extra
                or target_uses_rust_dep(pkg, new_t, label)
            ):
                continue
            reported_extra.add(label)
            file, line = locate(pkg, [label], new_file, new_t["line"])
            t_desc = f'`{new_t["tmpl"]}("{t_name}")` in //{pkg}'
            msg = (
                f"{t_desc} depends on `{label}`, which was neither present in `{gn_rel}` "
                "at the change base nor referenced in the target's `.rs` source files. "
                "Adding unreferenced Rust crate dependencies (or keeping a stale dependency "
                "in `BUILD.bazel` after an upstream rebase removed it from `BUILD.gn`) "
                "causes `rustc` to fail with `error: extern crate ... is unused` under "
                "`-D unused-crate-dependencies`."
            )
            remedy = (
                f"Remove `{label}` from `{pkg}/BUILD.bazel` and re-run "
                f"`fx bazel2gn -d {pkg}` so `{gn_rel}` matches the base GN dependencies."
            )
            _emit(findings, "unexpected_rust_dep_added", "ERROR", file, line, msg, remedy)

print(json.dumps(findings, indent=2))
PYEOF
