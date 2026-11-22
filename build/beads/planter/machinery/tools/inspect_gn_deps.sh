#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Batch helper tool that inspects a GN package's direct dependencies, test_deps,
# and inputs against BUILD.bazel files on disk in a single call, reporting:
# - Ready Bazel dependency labels (including third_party/rust_crates mappings)
# - External targets or exports_files() needing visibility/export updates
# - Host-constrained direct dependencies (requiring HOST_OS_CONSTRAINTS)
# - Missing in-scope or external BUILD.bazel packages
# - Nearest ancestor BUILD.bazel test_suite for test wiring

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKS_DIR="$(cd "${SCRIPT_DIR}/../checks" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"

python3 - "$CHECKS_DIR" "$WORKDIR" "$@" <<'PYEOF'
import ast
import json
import os
import re
import sys

sys.path.insert(0, os.path.abspath(sys.argv[1]))
import planter_check_lib as pcl

workdir = os.path.abspath(sys.argv[2])
cli_args = sys.argv[3:]

DEP_LIST_ATTRS = (
    "deps",
    "public_deps",
    "non_sdk_deps",
    "non_sdk_public_deps",
    "test_deps",
    "data_deps",
    "proc_macro_deps",
    "non_rust_deps",
    "args_deps",
    "args_test_deps",
    "plugin_deps",
)

IMPLICIT_HOST_RULES = {
    "cc_binary_host_tool",
    "ffx_plugin",
    "ffx_tool",
    "go_binary_host_tool",
    "golden_test",
    "host_go_test",
    "host_py_test",
    "host_rustc_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
    "host_test_data_map",
    "idk_cc_binary_host_tool",
    "idk_ffx_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "py_binary_host_tool",
    "py_zipapp_binary",
    "rustc_binary_host_tool",
    "wrap_host_rust_test",
}


# Collect target directories from CLI args or PLANTER_TARGET_DIRS / PLANTER_TARGET_DIR.
def resolve_requested_dirs(raw_args):
    tokens = []
    for item in raw_args:
        tokens.extend(t for t in re.split(r"[\s,]+", item) if t)
    if not tokens:
        env_dirs = os.environ.get("PLANTER_TARGET_DIRS", "").strip()
        if not env_dirs:
            env_dirs = os.environ.get("PLANTER_TARGET_DIR", "").strip()
        tokens.extend(t for t in re.split(r"[\s,]+", env_dirs) if t)
    out = []
    for tok in tokens:
        norm = pcl.normalize_target_dir(tok, workdir)
        if norm and norm not in out:
            out.append(norm)
    return out


FIDL_UNIFIED_CPP_SUFFIXES = {
    "_cpp": "_cpp",
    "_cpp_common": "_cpp",
    "_cpp_driver": "_cpp",
    "_cpp_driver_headers": "_cpp_driver_headers",
    "_cpp_driver_wire": "_cpp",
    "_cpp_natural_ostream": "_cpp",
    "_cpp_natural_types": "_cpp",
    "_cpp_testing": "_cpp_testing",
    "_cpp_type_conversions": "_cpp",
    "_cpp_wire": "_cpp",
    "_cpp_wire_testing": "_cpp_testing",
    "_cpp_driver_testing": "_cpp_testing",
}

FIDL_RUST_SUFFIXES = {
    "_rust",
    "_rust_common",
    "_rust_fdomain",
    "_rust_flex",
    "_rust_fdomain_flex",
}

FIDL_RUST_NEXT_RESTRICTED_SUFFIXES = {
    "_rust_next",
    "_rust_next_common",
    "_rust_fdomain_next",
}

ZITHER_BACKENDS = {
    "asm",
    "c",
    "go",
    "kernel",
    "legacy_syscall_cdecl",
    "rust",
    "rust_syscall",
    "syscall_docs",
    "zircon_ifs",
}


# Parse top-level/imported GN string/list variables and expand foreach() loops in BUILD.gn.
def expand_gn_foreach_and_vars(clean_gn, pkg_dir="", wdir=""):
    str_lit_re = re.compile(r'"((?:[^"\\]|\\.)*)"')
    gn_target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{')
    skip_blk_re = re.compile(
        r"(?:template|declare_args|forward_variables_from)\s*\([^)]*\)\s*\{"
    )
    not_targets = {
        "if",
        "else",
        "template",
        "declare_args",
        "foreach",
        "forward_variables_from",
    }
    target_str_attrs = {
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
    foreach_re = re.compile(
        r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s*,\s*(\[[^\]]*\]|[A-Za-z_]\w*)\s*\)\s*\{"
    )
    var_re = re.compile(r"(?m)^\s*([A-Za-z_]\w*)\s*(\+?=)\s*\[")

    def interp_str(s, svars):
        if not svars or "$" not in s:
            return s
        out = s
        for _ in range(4):
            prev = out
            for k, v in svars.items():
                out = out.replace(f"${{{k}}}", v)
                out = re.sub(
                    rf"\${re.escape(k)}(?![A-Za-z0-9_])",
                    lambda _m, v=v: v,
                    out,
                )
            if out == prev or "$" not in out:
                break
        return out

    def interp_code(txt, svars):
        if not svars or "$" not in txt:
            return txt
        return str_lit_re.sub(
            lambda sm: f'"{interp_str(sm.group(1), svars)}"', txt
        )

    def mask_blocks(txt, mask_targets=True, mask_foreach=True):
        chars = list(txt)
        i, n, in_str = 0, len(txt), False
        while i < n:
            c = txt[i]
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
                i == 0 or not (txt[i - 1].isalnum() or txt[i - 1] == "_")
            ):
                fm = foreach_re.match(txt, i) if mask_foreach else None
                sm = (
                    skip_blk_re.match(txt, i)
                    if not fm and mask_targets
                    else None
                )
                tm = (
                    gn_target_re.match(txt, i)
                    if not fm and not sm and mask_targets
                    else None
                )
                if fm or sm or (tm and tm.group(1) not in not_targets):
                    b_open = (fm or sm or tm).end() - 1
                    b_close = pcl.find_closing_delim(txt, b_open, "{", "}")
                    for k in range(b_open + 1, min(b_close, n)):
                        if chars[k] != "\n":
                            chars[k] = " "
                    i = b_close + 1
                    continue
            i += 1
        return "".join(chars)

    def collect_str_vars(masked_txt, base=None, skip_names=(), in_target=False):
        sv = dict(base or {})
        for sm in re.finditer(
            r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"((?:[^"\\]|\\.)*)"', masked_txt
        ):
            vname = sm.group(1)
            if vname in skip_names or (in_target and vname in target_str_attrs):
                continue
            sv[vname] = interp_str(sm.group(2), sv)
        return sv

    def collect_list_vars(txt, svars, base=None, union_on_assign=False):
        lvars = {k: list(v) for k, v in (base or {}).items()}
        for m in var_re.finditer(txt):
            vname = m.group(1)
            if vname in DEP_LIST_ATTRS or vname == "inputs":
                continue
            close_idx = pcl.find_closing_delim(txt, m.end() - 1, "[", "]")
            vals = [
                interp_str(v, svars)
                for v in str_lit_re.findall(txt[m.end() : close_idx])
            ]
            if m.group(2) == "+=" or union_on_assign:
                bucket = lvars.setdefault(vname, [])
                for v in vals:
                    if v not in bucket:
                        bucket.append(v)
            else:
                lvars[vname] = list(dict.fromkeys(vals))
        return lvars

    def outer_foreach_spans(txt):
        spans = []
        i, n, in_str = 0, len(txt), False
        while i < n:
            c = txt[i]
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
                i == 0 or not (txt[i - 1].isalnum() or txt[i - 1] == "_")
            ):
                fm = foreach_re.match(txt, i)
                if fm:
                    b_open = fm.end() - 1
                    b_close = pcl.find_closing_delim(txt, b_open, "{", "}")
                    spans.append(
                        (i, b_open, b_close, fm.group(1), fm.group(2).strip())
                    )
                    i = b_close + 1
                    continue
            i += 1
        return spans

    imp_str_vars, imp_list_vars = {}, {}
    if wdir and hasattr(pcl, "read_imported_gni_text"):
        for raw_imp in re.findall(r'\bimport\(\s*"([^"$]+)"\s*\)', clean_gn):
            gni_txt, _ = pcl.read_imported_gni_text(wdir, pkg_dir, raw_imp)
            if gni_txt:
                masked_gni = mask_blocks(pcl.blank_comments(gni_txt))
                imp_str_vars = collect_str_vars(masked_gni, imp_str_vars)
                imp_list_vars = collect_list_vars(
                    masked_gni, imp_str_vars, base=imp_list_vars
                )

    str_vars = collect_str_vars(mask_blocks(clean_gn), imp_str_vars)
    for _ in range(3):
        spans = outer_foreach_spans(clean_gn)
        if not spans:
            break
        top_masked = mask_blocks(clean_gn, mask_targets=True, mask_foreach=True)
        masked_fe_only = mask_blocks(
            clean_gn, mask_targets=False, mask_foreach=True
        )
        target_spans = []
        t_cursor = 0
        for tm in gn_target_re.finditer(masked_fe_only):
            if tm.start() < t_cursor or tm.group(1) in not_targets:
                continue
            tb_open = tm.end() - 1
            tb_close = pcl.find_closing_delim(clean_gn, tb_open, "{", "}")
            t_cursor = tb_close + 1
            target_spans.append((tb_open, tb_close))
        pieces = []
        pos = 0
        for f_start, b_open, b_close, loop_var, domain in spans:
            pieces.append(clean_gn[pos:f_start])
            cur_str_vars = collect_str_vars(top_masked[:f_start], imp_str_vars)
            cur_list_vars = collect_list_vars(
                top_masked[:f_start], cur_str_vars, base=imp_list_vars
            )
            for tb_open, tb_close in target_spans:
                if tb_open < f_start < tb_close:
                    t_masked = mask_blocks(
                        clean_gn[tb_open + 1 : f_start],
                        mask_targets=True,
                        mask_foreach=True,
                    )
                    cur_str_vars = collect_str_vars(
                        t_masked, cur_str_vars, in_target=True
                    )
                    cur_list_vars = collect_list_vars(
                        t_masked, cur_str_vars, base=cur_list_vars
                    )
                    break
            body = clean_gn[b_open + 1 : b_close]
            items = (
                [interp_str(v, cur_str_vars) for v in str_lit_re.findall(domain)]
                if domain.startswith("[")
                else cur_list_vars.get(domain, ())
            )
            nested_lvars = {
                nfm.group(1)
                for nfm in foreach_re.finditer(body)
                if nfm.group(1) != loop_var
            }
            masked_body = mask_blocks(body)
            base_iter = {
                k: v for k, v in cur_str_vars.items() if k not in nested_lvars
            }
            expanded = []
            if items:
                for val in items:
                    iter_vars = dict(base_iter)
                    iter_vars[loop_var] = val
                    iter_vars = collect_str_vars(
                        masked_body,
                        iter_vars,
                        skip_names=nested_lvars | {loop_var},
                    )
                    expanded.append(
                        pcl.quote_bare_gn_list_ident(
                            interp_code(body, iter_vars), loop_var, val
                        )
                    )
            else:
                expanded.append(interp_code(body, base_iter))
            pieces.append("\n".join(expanded))
            pos = b_close + 1
        pieces.append(clean_gn[pos:])
        clean_gn = "".join(pieces)

    str_vars = collect_str_vars(
        mask_blocks(clean_gn, mask_targets=True, mask_foreach=False), str_vars
    )
    out_parts = []
    tpos = 0
    for tm in gn_target_re.finditer(clean_gn):
        if tm.start() < tpos or tm.group(1) in not_targets:
            continue
        b_open = tm.end() - 1
        b_close = pcl.find_closing_delim(clean_gn, b_open, "{", "}")
        out_parts.append(interp_code(clean_gn[tpos:b_open], str_vars))
        tbody = clean_gn[b_open : b_close + 1]
        tsv = collect_str_vars(
            mask_blocks(tbody[1:-1], mask_targets=True, mask_foreach=True),
            str_vars,
            in_target=True,
        )
        out_parts.append(interp_code(tbody, tsv))
        tpos = b_close + 1
    out_parts.append(interp_code(clean_gn[tpos:], str_vars))
    clean_gn = "".join(out_parts)
    list_vars = collect_list_vars(
        clean_gn, str_vars, base=imp_list_vars, union_on_assign=True
    )
    return clean_gn, list_vars


# Extract direct GN dependency labels and input paths from a package's BUILD.gn.
def extract_gn_deps_and_inputs(gn_text, pkg_dir):
    clean_gn, list_vars = expand_gn_foreach_and_vars(
        pcl.blank_comments(gn_text),
        pkg_dir=pkg_dir,
        wdir=globals().get("workdir", ""),
    )
    # Ignore child aggregation edges inside `group("tests")` / `group("drivers")` when
    # the package defines convertible library/binary/test targets.
    if re.search(
        r'\b(?!group\b|config\b|template\b|declare_args\b|foreach\b|if\b|else\b)[A-Za-z_]\w*\(\s*"[^"]+"\s*\)\s*\{',
        clean_gn,
    ):
        chars = list(clean_gn)
        for gm in re.finditer(r'\bgroup\(\s*"(?:tests|drivers)"\s*\)\s*\{', clean_gn):
            b_close = pcl.find_closing_delim(clean_gn, gm.end() - 1, "{", "}")
            for k in range(gm.start(), min(b_close + 1, len(chars))):
                if chars[k] != "\n":
                    chars[k] = " "
        clean_gn = "".join(chars)

    def collect_attr_strings(attr_names):
        found = []
        pat = "|".join(re.escape(a) for a in attr_names)
        assign_re = re.compile(rf"(?<![\w.])(?:{pat})\s*\+?=\s*")
        n = len(clean_gn)
        for m in assign_re.finditer(clean_gn):
            idx = m.end()
            while idx < n:
                while idx < n and clean_gn[idx].isspace():
                    idx += 1
                if idx >= n:
                    break
                if clean_gn[idx] == "[":
                    end_i = pcl.find_closing_delim(clean_gn, idx, "[", "]")
                    found.extend(
                        re.findall(r'"((?:[^"\\]|\\.)*)"', clean_gn[idx + 1 : end_i])
                    )
                    idx = end_i + 1
                else:
                    vm = re.match(r"([A-Za-z_]\w*)", clean_gn[idx:])
                    if not vm:
                        break
                    found.extend(list_vars.get(vm.group(1), ()))
                    idx += vm.end()
                while idx < n and clean_gn[idx].isspace():
                    idx += 1
                if idx < n and clean_gn[idx] == "+":
                    idx += 1
                else:
                    break
        return found

    raw_deps = collect_attr_strings(DEP_LIST_ATTRS)
    raw_inputs = collect_attr_strings(("inputs",))
    norm_deps = []
    for raw in raw_deps:
        lbl = re.sub(r"\([^()]*\)$", "", raw.strip()).strip()
        if not lbl or "$" in lbl or lbl.startswith(":"):
            continue
        if lbl.startswith("//"):
            body = re.sub(r"/+", "/", lbl[2:])
        else:
            body = os.path.normpath(os.path.join(pkg_dir, lbl)).replace("\\", "/")
            if body.startswith(".."):
                continue
        path, sep, tname = body.partition(":")
        path = path.strip("/")
        if not path or path == pkg_dir:
            continue
        tname = tname if sep else os.path.basename(path)
        canon = f"//{path}:{tname}"
        if canon not in norm_deps:
            norm_deps.append(canon)

    norm_inputs = []
    for raw in raw_inputs:
        inp = raw.strip()
        if not inp or "$" in inp:
            continue
        if inp.startswith("//"):
            rel = re.sub(r"/+", "/", inp[2:]).strip("/")
        else:
            rel = (
                os.path.normpath(os.path.join(pkg_dir, inp.lstrip(":")))
                .replace("\\", "/")
                .strip("/")
            )
            if rel in ("", ".") or rel.startswith(".."):
                continue
        if rel and f"//{rel}" not in norm_inputs:
            norm_inputs.append(f"//{rel}")
    return norm_deps, norm_inputs


# Parse top-level AST assignments in a workspace `.bzl` file for constant resolution.
def load_bzl_var_table(bzl_rel, cache):
    key = f"__bzl__:{bzl_rel}"
    if key in cache:
        return cache[key]
    bpath = os.path.join(workdir, bzl_rel)
    if not os.path.isfile(bpath):
        cache[key] = {}
        return {}
    try:
        tree = ast.parse(open(bpath, encoding="utf-8").read(), filename=bpath)
    except Exception:
        cache[key] = {}
        return {}
    vtab = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            vtab[node.targets[0].id] = node.value
    cache[key] = vtab
    return vtab


# Extract a boolean keyword value from a Starlark AST node, resolving local constants.
def extract_ast_bool(node, var_table, default, depth=0):
    if node is None or depth > 5:
        return default
    if isinstance(node, ast.Constant) and isinstance(node.value, bool):
        return node.value
    if isinstance(node, ast.Name) and node.id in var_table:
        return extract_ast_bool(var_table[node.id], var_table, default, depth + 1)
    return default


# Load the `fidl_rust_next_allowlist` visibility list from //build/rust:fidl_rust_next.bzl.
def load_fidl_rust_next_allowlist(cache):
    key = "__fidl_rust_next_allowlist__"
    if key in cache:
        return cache[key]
    vtab = load_bzl_var_table("build/rust/fidl_rust_next.bzl", cache)
    node = vtab.get("fidl_rust_next_allowlist")
    if node is None:
        cache[key] = None
        return None
    allowlist = pcl._extract_ast_strings(node, vtab)
    for extra in ("//build/bazel/rules/fidl:__subpackages__",):
        if extra not in allowlist:
            allowlist.append(extra)
    cache[key] = allowlist
    return allowlist


# Parse a BUILD.bazel or BUILD file for targets, visibility, package_groups, and exports_files.
def load_bazel_package_info(dep_pkg, cache):
    if dep_pkg in cache:
        return cache[dep_pkg]
    rel_bpath = f"{dep_pkg}/BUILD.bazel" if dep_pkg else "BUILD.bazel"
    bpath = os.path.join(workdir, rel_bpath)
    if not os.path.isfile(bpath):
        alt_rel = f"{dep_pkg}/BUILD" if dep_pkg else "BUILD"
        alt_bpath = os.path.join(workdir, alt_rel)
        if os.path.isfile(alt_bpath):
            rel_bpath = alt_rel
            bpath = alt_bpath
        elif not dep_pkg and os.path.isfile(
            os.path.join(workdir, "build", "bazel", "toplevel.BUILD.bazel")
        ):
            rel_bpath = "build/bazel/toplevel.BUILD.bazel"
            bpath = os.path.join(workdir, rel_bpath)
        else:
            cache[dep_pkg] = None
            return None
    try:
        with open(bpath, encoding="utf-8") as bf:
            src = bf.read()
        tree = ast.parse(src, filename=bpath)
    except Exception:
        info = {
            "exists": True,
            "parse_error": True,
            "build_file": rel_bpath,
            "targets": {},
            "exports": {},
            "pkg_groups": {},
        }
        cache[dep_pkg] = info
        return info

    var_table = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            m_bzl = re.match(r"^@?//([^:]+):(.+\.bzl)$", node.value.args[0].value)
            if m_bzl:
                bzl_rel = f"{m_bzl.group(1).strip('/')}/{m_bzl.group(2).lstrip('/')}"
                bzl_vars = load_bzl_var_table(bzl_rel, cache)
                for l_arg in node.value.args[1:]:
                    if (
                        isinstance(l_arg, ast.Constant)
                        and isinstance(l_arg.value, str)
                        and l_arg.value in bzl_vars
                    ):
                        var_table[l_arg.value] = bzl_vars[l_arg.value]
                for l_kw in node.value.keywords:
                    if (
                        l_kw.arg
                        and isinstance(l_kw.value, ast.Constant)
                        and isinstance(l_kw.value.value, str)
                        and l_kw.value.value in bzl_vars
                    ):
                        var_table[l_kw.arg] = bzl_vars[l_kw.value.value]
        elif (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            var_table[node.targets[0].id] = node.value

    default_vis = None
    targets = {}
    exports = {}
    pkg_groups = {}

    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}

        if rule == "package" and "default_visibility" in kws:
            default_vis = pcl._extract_ast_strings(kws["default_visibility"], var_table)
            continue

        if rule == "exports_files":
            files = []
            if call.args:
                files.extend(pcl._extract_ast_strings(call.args[0], var_table))
            if "srcs" in kws:
                files.extend(pcl._extract_ast_strings(kws["srcs"], var_table))
            vis = (
                pcl._extract_ast_strings(kws["visibility"], var_table)
                if "visibility" in kws
                else ["//visibility:public"]
            )
            for fn in files:
                exports[fn.lstrip(":")] = vis
            continue

        tname_vals = pcl._extract_ast_strings(kws.get("name"), var_table)
        if not tname_vals:
            continue
        tname = tname_vals[0]

        if rule == "package_group":
            specs = pcl._extract_ast_strings(kws.get("packages"), var_table)
            pkg_groups[tname] = specs
            continue

        vis = (
            pcl._extract_ast_strings(kws["visibility"], var_table)
            if "visibility" in kws
            else (list(default_vis) if default_vis is not None else [f"//{dep_pkg}:__pkg__"])
        )
        if rule.startswith("idk_") and "//sdk:__pkg__" not in vis:
            vis = list(vis) + ["//sdk:__pkg__"]

        tcw_seg = (
            ast.get_source_segment(src, kws["target_compatible_with"]) or ""
            if "target_compatible_with" in kws
            else ""
        )
        host_only = (
            rule in IMPLICIT_HOST_RULES
            or "HOST_OS_CONSTRAINTS" in tcw_seg
            or "HOST_CONSTRAINTS" in tcw_seg
        )
        tmeta = {
            "rule": rule,
            "visibility": vis,
            "host_only": host_only,
        }
        if not dep_pkg:
            refs = []
            for rattr in ("srcs", "hdrs", "includes", "data", "source"):
                if rattr in kws:
                    refs.extend(pcl._extract_ast_strings(kws[rattr], var_table))
            tmeta["path_refs"] = refs
        if rule in ("fidl_library", "zither_fidl_library"):
            cat_vals = pcl._extract_ast_strings(kws.get("category"), var_table)
            tmeta["fidl_flags"] = {
                "category": cat_vals[0] if cat_vals else "",
                "enable_cpp": extract_ast_bool(kws.get("enable_cpp"), var_table, True),
                "enable_hlcpp": extract_ast_bool(
                    kws.get("enable_hlcpp"), var_table, False
                ),
                "enable_rust": extract_ast_bool(kws.get("enable_rust"), var_table, True),
                "enable_rust_next": extract_ast_bool(
                    kws.get("enable_rust_next"), var_table, True
                ),
                "enable_zither": extract_ast_bool(
                    kws.get("enable_zither"),
                    var_table,
                    rule == "zither_fidl_library",
                ),
            }
        elif rule == "ffx_plugin":
            tmeta["has_plugin_deps"] = bool(
                pcl._extract_ast_strings(kws.get("plugin_deps"), var_table)
            )
        targets[tname] = tmeta

    info = {
        "exists": True,
        "parse_error": False,
        "build_file": rel_bpath,
        "targets": targets,
        "exports": exports,
        "pkg_groups": pkg_groups,
    }
    cache[dep_pkg] = info
    return info


# Check whether a package_group `packages` spec list covers caller_pkg.
def package_group_allows(specs, caller_pkg):
    pos_matched = False
    for raw in specs:
        neg = raw.startswith("-")
        spec = raw[1:] if neg else raw
        if spec == "public":
            matched = True
        elif spec == "private" or not spec.startswith("//"):
            continue
        else:
            body = spec[2:].strip("/")
            if spec.endswith("/...") or body == "...":
                prefix = body[:-3].strip("/") if body.endswith("...") else body
                matched = (
                    not prefix
                    or caller_pkg == prefix
                    or caller_pkg.startswith(prefix + "/")
                )
            else:
                matched = caller_pkg == body
        if matched:
            if neg:
                return False
            pos_matched = True
    return pos_matched


# Evaluate whether a Bazel visibility list grants access to caller_pkg.
def visibility_allows_caller(vis_list, dep_pkg, caller_pkg, cache):
    if dep_pkg == caller_pkg:
        return True
    for entry in vis_list:
        if entry in ("//visibility:public", "//:__subpackages__"):
            return True
        if entry == "//visibility:private":
            continue
        if entry == ":__pkg__":
            if caller_pkg == dep_pkg:
                return True
            continue
        if entry == ":__subpackages__":
            if not dep_pkg or caller_pkg == dep_pkg or caller_pkg.startswith(dep_pkg + "/"):
                return True
            continue
        if entry.startswith(":"):
            ginfo = load_bazel_package_info(dep_pkg, cache)
            gspecs = (ginfo or {}).get("pkg_groups", {}).get(entry[1:])
            if gspecs and package_group_allows(gspecs, caller_pkg):
                return True
            continue
        if entry.startswith("//"):
            gpkg, sep, gtgt = entry[2:].partition(":")
            gpkg = gpkg.strip("/")
            if not sep:
                continue
            if gtgt == "__pkg__" and caller_pkg == gpkg:
                return True
            if gtgt == "__subpackages__" and (
                not gpkg or caller_pkg == gpkg or caller_pkg.startswith(gpkg + "/")
            ):
                return True
            if gtgt not in ("__pkg__", "__subpackages__"):
                ginfo = load_bazel_package_info(gpkg, cache)
                gspecs = (ginfo or {}).get("pkg_groups", {}).get(gtgt)
                if gspecs and package_group_allows(gspecs, caller_pkg):
                    return True
    return False


# Build a visibility_edits_needed entry, routing package_group-gated targets (such as restricted
# third_party/rust_crates/vendor aliases) to the owning package_group's `packages` list rather than
# a generated alias file.
def build_visibility_edit_entry(canon, dep_pkg, target_name, target_meta, caller_pkg, cache):
    vis_list = target_meta["visibility"]
    for entry in vis_list:
        gpkg, gtgt = None, None
        if entry.startswith(":") and entry not in (":__pkg__", ":__subpackages__"):
            gpkg, gtgt = dep_pkg, entry[1:]
        elif entry.startswith("//"):
            cand_pkg, sep, cand_tgt = entry[2:].partition(":")
            if sep and cand_tgt not in ("__pkg__", "__subpackages__", "public", "private"):
                gpkg, gtgt = cand_pkg.strip("/"), cand_tgt
        if gpkg is not None and gtgt:
            ginfo = load_bazel_package_info(gpkg, cache)
            gspecs = (ginfo or {}).get("pkg_groups", {}).get(gtgt)
            if gspecs is not None:
                return {
                    "gn_label": canon,
                    "bazel_build": (ginfo or {}).get("build_file")
                    or (f"{gpkg}/BUILD.bazel" if gpkg else "BUILD.bazel"),
                    "target": gtgt,
                    "rule": "package_group",
                    "current_visibility": gspecs,
                    "add_visibility": f"//{caller_pkg}",
                }
    dep_info = load_bazel_package_info(dep_pkg, cache)
    return {
        "gn_label": canon,
        "bazel_build": (dep_info or {}).get("build_file")
        or (f"{dep_pkg}/BUILD.bazel" if dep_pkg else "BUILD.bazel"),
        "target": target_name,
        "rule": target_meta["rule"],
        "current_visibility": vis_list,
        "add_visibility": f"//{caller_pkg}:__pkg__",
    }


# Resolve a third_party/rust_crates:<crate> GN dependency to its Bazel label and target metadata.
def resolve_rust_crate_dep(crate_name, cache):
    vendor_pkg = "third_party/rust_crates/vendor"
    vinfo = load_bazel_package_info(vendor_pkg, cache)
    if vinfo and crate_name in vinfo["targets"]:
        return f"//{vendor_pkg}:{crate_name}", vendor_pkg, vinfo["targets"][crate_name]
    root_pkg = "third_party/rust_crates"
    rinfo = load_bazel_package_info(root_pkg, cache)
    if rinfo and crate_name in rinfo["targets"]:
        return f"//{root_pkg}:{crate_name}", root_pkg, rinfo["targets"][crate_name]
    for subdir in ("forks", "ask2patch"):
        sub_root = os.path.join(workdir, "third_party", "rust_crates", subdir)
        if not os.path.isdir(sub_root):
            continue
        for entry in sorted(os.listdir(sub_root)):
            fpkg = f"third_party/rust_crates/{subdir}/{entry}"
            finfo = load_bazel_package_info(fpkg, cache)
            if finfo and crate_name in finfo["targets"]:
                display = (
                    f"//{fpkg}:{crate_name} "
                    f"(# @bazel2gn:path_overwrite://third_party/rust_crates:{crate_name})"
                )
                return display, fpkg, finfo["targets"][crate_name]
    return f"//{vendor_pkg}:{crate_name} (unverified)", None, None


# Load GN-to-Bazel third_party target mappings from third_party_target_map.json.
def load_third_party_target_map(cache):
    key = "__third_party_target_map__"
    if key in cache:
        return cache[key]
    tp_path = os.path.join(workdir, "build/tools/bazel2gn/third_party_target_map.json")
    rev = {}
    if os.path.isfile(tp_path):
        try:
            with open(tp_path, encoding="utf-8") as tf:
                raw = json.load(tf)
            for bz_lbl, gn_lbl in raw.items():
                if isinstance(bz_lbl, str) and isinstance(gn_lbl, str):
                    rev.setdefault(gn_lbl, bz_lbl)
        except Exception:
            pass
    cache[key] = rev
    return rev


# Resolve a third_party/<pkg> GN dependency without <pkg>/BUILD.bazel via third_party_target_map
# or root-package targets in build/bazel/toplevel.BUILD.bazel.
def resolve_unmigrated_third_party_dep(canon, dep_pkg, tname, caller_pkg, cache):
    if canon == "//third_party/googletest:gtest_no_testonly":
        return (
            "mapped",
            f"{canon} -> @com_google_googletest//:gtest "
            "(# @bazel2gn:path_overwrite://third_party/googletest:gtest_no_testonly) "
            "(external repo)",
        )
    tp_map = load_third_party_target_map(cache)
    leaf = os.path.basename(dep_pkg)
    for gn_key in (canon, f"//{dep_pkg}") if tname == leaf else (canon,):
        if gn_key in tp_map:
            return "mapped", f"{canon} -> {tp_map[gn_key]} (third_party_target_map)"
    tl_info = load_bazel_package_info("", cache)
    if not tl_info or tl_info.get("parse_error"):
        return None, None
    cand_names = {tname, leaf} if tname else {leaf}
    matched_name, matched_meta = None, None
    for cname, cmeta in tl_info["targets"].items():
        refs = cmeta.get("path_refs") or ()
        refs_tp = any(
            isinstance(r, str) and (r == dep_pkg or r.startswith(dep_pkg + "/"))
            for r in refs
        )
        if cname in cand_names and (refs_tp or cname == tname):
            matched_name, matched_meta = cname, cmeta
            break
        if refs_tp and tname == leaf and matched_meta is None:
            matched_name, matched_meta = cname, cmeta
    if not matched_meta:
        return None, None
    if visibility_allows_caller(matched_meta["visibility"], "", caller_pkg, cache):
        return (
            "ready",
            f"{canon} -> //:{matched_name} ({matched_meta['rule']} in "
            f"{tl_info['build_file']}; requires # @bazel2gn:path_overwrite:{canon})",
        )
    vis_entry = build_visibility_edit_entry(
        canon, "", matched_name, matched_meta, caller_pkg, cache
    )
    if str(vis_entry.get("bazel_build", "")).startswith("build/"):
        vis_entry["macro_change_needed"] = True
    return "visibility_blocked", vis_entry


# Locate the owning Bazel package and check exports_files() for a GN input path.
def inspect_gn_input_file(gn_input, caller_pkg, cache):
    rel_all = gn_input[2:].strip("/")
    cur_dir = os.path.dirname(rel_all)
    owner_pkg = None
    while True:
        if (
            os.path.isfile(os.path.join(workdir, cur_dir, "BUILD.bazel"))
            or os.path.isfile(os.path.join(workdir, cur_dir, "BUILD"))
            or os.path.isfile(os.path.join(workdir, cur_dir, "BUILD.gn"))
            or (
                cur_dir
                and os.path.isfile(
                    os.path.join(workdir, "build/secondary", cur_dir, "BUILD.gn")
                )
            )
        ):
            owner_pkg = cur_dir
            break
        if not cur_dir:
            break
        cur_dir = os.path.dirname(cur_dir)

    if owner_pkg is None:
        return {"gn_input": gn_input, "status": "owner_package_not_found"}

    export_rel = rel_all[len(owner_pkg) :].lstrip("/") if owner_pkg else rel_all
    if owner_pkg == caller_pkg:
        return {
            "gn_input": gn_input,
            "status": "ready",
            "bazel_label": f":{export_rel}",
        }
    info = load_bazel_package_info(owner_pkg, cache)
    if not info:
        return {
            "gn_input": gn_input,
            "status": "missing_owner_bazel_build",
            "owner_package": f"//{owner_pkg}",
        }
    owner_build = info.get("build_file") or (
        f"{owner_pkg}/BUILD.bazel" if owner_pkg else "BUILD.bazel"
    )
    vis = info["exports"].get(export_rel)
    if vis is None:
        return {
            "gn_input": gn_input,
            "status": "export_needed",
            "owner_bazel_build": owner_build,
            "export_path": export_rel,
            "add_visibility": f"//{caller_pkg}:__pkg__",
        }
    if not visibility_allows_caller(vis, owner_pkg, caller_pkg, cache):
        return {
            "gn_input": gn_input,
            "status": "visibility_edit_needed",
            "owner_bazel_build": owner_build,
            "export_path": export_rel,
            "current_visibility": vis,
            "add_visibility": f"//{caller_pkg}:__pkg__",
        }
    return {
        "gn_input": gn_input,
        "status": "ready",
        "bazel_label": f"//{owner_pkg}:{export_rel}",
    }


# Walk parent directories to find the nearest ancestor BUILD.bazel test_suite.
def find_ancestor_test_suite(pkg_dir, cache):
    cur = os.path.dirname(pkg_dir)
    suite_rules = {"test_suite", "fuchsia_test_suite", "host_test_suite"}
    while True:
        info = load_bazel_package_info(cur, cache)
        if info and not info.get("parse_error"):
            suites = sorted(
                tname
                for tname, tmeta in info["targets"].items()
                if tmeta["rule"] in suite_rules
                or tname in ("tests", "host_tests", "target_tests")
            )
            if suites:
                pkg_lbl = f"//{cur}" if cur else "//"
                return {
                    "bazel_build": info.get("build_file")
                    or (f"{cur}/BUILD.bazel" if cur else "BUILD.bazel"),
                    "package": pkg_lbl,
                    "suite_targets": suites,
                    "required_test_visibility": f"{pkg_lbl}:__pkg__",
                }
        if not cur:
            break
        cur = os.path.dirname(cur)
    return None


# Match a requested GN target name against targets in dep_pkg/BUILD.bazel (including FIDL, bind, and ffx_plugin).
def match_bazel_target(tname, info):
    targets = info["targets"]
    if tname in targets:
        return tname, targets[tname]
    if tname.endswith("_cpp"):
        cc_bind_name = f"{tname[:-4]}_cc"
        cc_bind_meta = targets.get(cc_bind_name)
        if cc_bind_meta and cc_bind_meta["rule"] == "fuchsia_bind_cc_library":
            smeta = dict(cc_bind_meta)
            smeta["decl_name"] = cc_bind_name
            smeta["path_overwrite"] = True
            return cc_bind_name, smeta
    for cand_name, cand_meta in targets.items():
        if cand_meta["rule"] == "bind_library" and tname in (
            f"{cand_name}_cpp",
            f"{cand_name}_rust",
        ):
            smeta = dict(cand_meta)
            smeta["decl_name"] = cand_name
            return tname, smeta
        if cand_meta["rule"] == "ffx_plugin" and tname.startswith(cand_name + "_"):
            suf = tname[len(cand_name) :]
            if suf in ("_args", "_suite", "_config") or (
                suf == "_sub_command" and cand_meta.get("has_plugin_deps")
            ):
                smeta = dict(cand_meta)
                smeta["decl_name"] = cand_name
                if suf == "_config":
                    smeta["host_only"] = False
                return f"{cand_name}{suf}", smeta
        if cand_meta["rule"] not in ("fidl_library", "zither_fidl_library"):
            continue
        flags = cand_meta.get("fidl_flags") or {}
        cat = flags.get("category", "")
        is_partner = cat == "partner"
        en_cpp = flags.get("enable_cpp", True)
        en_hlcpp = flags.get("enable_hlcpp", False)
        en_rust = flags.get("enable_rust", True)
        en_rust_next = flags.get("enable_rust_next", True)
        en_zither = flags.get("enable_zither", False)

        def _sub(resolved_name, restricted=False):
            smeta = dict(cand_meta)
            smeta["decl_name"] = cand_name
            if restricted:
                smeta["rust_next_restricted"] = True
            return resolved_name, smeta

        if tname.startswith(cand_name + "_"):
            suf = tname[len(cand_name) :]
            if suf == "_idk" and cat:
                return _sub(f"{cand_name}_idk")
            if en_cpp and suf in FIDL_UNIFIED_CPP_SUFFIXES:
                return _sub(f"{cand_name}{FIDL_UNIFIED_CPP_SUFFIXES[suf]}")
            if en_cpp and is_partner and suf in ("_cpp_idk", "_cpp_testing_idk"):
                return _sub(f"{cand_name}{suf}")
            if en_cpp and en_hlcpp and suf == "_cpp_hlcpp_conversion":
                return _sub(f"{cand_name}_cpp_hlcpp_conversion")
            if en_cpp and en_hlcpp and is_partner and suf == "_cpp_hlcpp_conversion_idk":
                return _sub(f"{cand_name}_cpp_hlcpp_conversion_idk")
            if en_hlcpp and suf == "_hlcpp":
                return _sub(f"{cand_name}_hlcpp")
            if en_hlcpp and is_partner and suf == "_hlcpp_idk":
                return _sub(f"{cand_name}_hlcpp_idk")
            if en_rust and suf in FIDL_RUST_SUFFIXES:
                return _sub(f"{cand_name}{suf}")
            if en_rust_next and suf in FIDL_RUST_NEXT_RESTRICTED_SUFFIXES:
                return _sub(f"{cand_name}{suf}", restricted=True)
            if en_rust and en_rust_next and suf == "_rust_next_convert":
                return _sub(f"{cand_name}_rust_next_convert")
            if en_zither and suf.startswith("_zither."):
                backend = suf[len("_zither.") :]
                if backend in ZITHER_BACKENDS:
                    return _sub(f"{cand_name}_zither.{backend}")
        elif en_zither and tname.startswith(cand_name + ".zither."):
            backend = tname[len(cand_name + ".zither.") :]
            if backend in ZITHER_BACKENDS:
                return _sub(f"{cand_name}_zither.{backend}")
    return None, None


# Summarize GN dependency and input readiness for a single target package.
def inspect_target_package(pkg_dir, in_scope_set, cache):
    gn_path = (
        os.path.join(workdir, pkg_dir, "BUILD.gn")
        if pkg_dir
        else os.path.join(workdir, "BUILD.gn")
    )
    if (
        not os.path.isfile(gn_path)
        and pkg_dir
        and not pkg_dir.startswith("build/secondary/")
    ):
        sec_gn_path = os.path.join(workdir, "build", "secondary", pkg_dir, "BUILD.gn")
        if os.path.isfile(sec_gn_path):
            gn_path = sec_gn_path
    bazel_pkg_dir = (
        pkg_dir[len("build/secondary/") :].strip("/")
        if pkg_dir.startswith("build/secondary/")
        and pkg_dir[len("build/secondary/") :].strip("/")
        else pkg_dir
    )
    bazel_path = (
        os.path.join(workdir, bazel_pkg_dir, "BUILD.bazel")
        if bazel_pkg_dir
        else os.path.join(workdir, "BUILD.bazel")
    )
    alt_bazel_path = (
        os.path.join(workdir, bazel_pkg_dir, "BUILD")
        if bazel_pkg_dir
        else os.path.join(workdir, "BUILD")
    )
    result = {
        "target_package": f"//{bazel_pkg_dir}" if bazel_pkg_dir else "//",
        "has_build_gn": os.path.isfile(gn_path),
        "has_build_bazel": os.path.isfile(bazel_path) or os.path.isfile(alt_bazel_path),
    }
    if not os.path.isfile(gn_path):
        return result

    with open(gn_path, encoding="utf-8") as gf:
        gn_text = gf.read()
    norm_deps, norm_inputs = extract_gn_deps_and_inputs(gn_text, bazel_pkg_dir)

    rust_crates = []
    ready_deps = []
    host_constrained_deps = []
    visibility_edits_needed = []
    target_name_mismatches = []
    missing_in_scope = []
    missing_external = []

    for canon in norm_deps:
        dep_pkg, _, tname = canon[2:].partition(":")
        if dep_pkg == "third_party/rust_crates":
            crate_display, crate_pkg, crate_meta = resolve_rust_crate_dep(tname, cache)
            rust_crates.append(crate_display)
            if (
                crate_pkg
                and crate_meta
                and not visibility_allows_caller(
                    crate_meta["visibility"], crate_pkg, bazel_pkg_dir, cache
                )
            ):
                visibility_edits_needed.append(
                    build_visibility_edit_entry(
                        canon, crate_pkg, tname, crate_meta, bazel_pkg_dir, cache
                    )
                )
            continue

        info = load_bazel_package_info(dep_pkg, cache)
        if not info:
            if dep_pkg.startswith("third_party/"):
                tp_kind, tp_val = resolve_unmigrated_third_party_dep(
                    canon, dep_pkg, tname, bazel_pkg_dir, cache
                )
                if tp_kind in ("mapped", "ready"):
                    ready_deps.append(tp_val)
                    continue
                if tp_kind == "visibility_blocked":
                    visibility_edits_needed.append(tp_val)
                    continue
            if dep_pkg in in_scope_set:
                if f"//{dep_pkg}" not in missing_in_scope:
                    missing_in_scope.append(f"//{dep_pkg}")
            else:
                if f"//{dep_pkg}" not in missing_external:
                    missing_external.append(f"//{dep_pkg}")
            continue

        matched_name, matched_meta = match_bazel_target(tname, info)
        if not matched_meta:
            target_name_mismatches.append(
                {
                    "gn_label": canon,
                    "bazel_build": f"{dep_pkg}/BUILD.bazel",
                    "available_targets": sorted(info["targets"].keys()),
                }
            )
            continue

        bz_lbl = (
            f"//{dep_pkg}"
            if matched_name == os.path.basename(dep_pkg)
            else f"//{dep_pkg}:{matched_name}"
        )
        if matched_meta["host_only"] and bz_lbl not in host_constrained_deps:
            host_constrained_deps.append(bz_lbl)

        pkg_vis_ok = visibility_allows_caller(
            matched_meta["visibility"], dep_pkg, bazel_pkg_dir, cache
        )
        rust_next_ok = True
        rust_next_allowlist = None
        if matched_meta.get("rust_next_restricted"):
            rust_next_allowlist = load_fidl_rust_next_allowlist(cache)
            if rust_next_allowlist is not None:
                rust_next_ok = visibility_allows_caller(
                    rust_next_allowlist, "build/rust", bazel_pkg_dir, cache
                )

        if pkg_vis_ok and rust_next_ok:
            overwrite_note = (
                f" (# @bazel2gn:path_overwrite:{canon})"
                if matched_meta.get("path_overwrite")
                else ""
            )
            ready_deps.append(
                f"{canon} -> {bz_lbl}{overwrite_note} ({matched_meta['rule']})"
            )
        else:
            if not pkg_vis_ok:
                visibility_edits_needed.append(
                    build_visibility_edit_entry(
                        canon,
                        dep_pkg,
                        matched_meta.get("decl_name", matched_name),
                        matched_meta,
                        bazel_pkg_dir,
                        cache,
                    )
                )
            if not rust_next_ok and rust_next_allowlist is not None:
                visibility_edits_needed.append(
                    {
                        "gn_label": canon,
                        "bazel_build": "build/rust/fidl_rust_next.bzl",
                        "target": "fidl_rust_next_allowlist",
                        "rule": "bzl_allowlist",
                        "current_visibility": rust_next_allowlist,
                        "add_visibility": f"//{bazel_pkg_dir}:__subpackages__",
                    }
                )

    result.update(
        {
            "third_party_rust_crates": sorted(set(rust_crates)),
            "ready_deps": ready_deps,
            "host_constrained_deps": host_constrained_deps,
            "visibility_edits_needed": visibility_edits_needed,
            "target_name_mismatches": target_name_mismatches,
            "missing_in_scope_bazel_packages": missing_in_scope,
            "missing_external_bazel_packages": missing_external,
            "inputs_status": [
                inspect_gn_input_file(inp, bazel_pkg_dir, cache)
                for inp in norm_inputs
            ],
            "nearest_ancestor_test_suite": find_ancestor_test_suite(
                bazel_pkg_dir, cache
            ),
        }
    )
    return result


req_dirs = resolve_requested_dirs(cli_args)
if not req_dirs:
    print("Usage: inspect_gn_deps.sh <target_package_dir> [dir2 ...]", file=sys.stderr)
    sys.exit(1)

in_scope = set(
    pcl.with_overlaid_target_dirs(list(resolve_requested_dirs([])) + list(req_dirs))
)
pkg_cache = {}
reports = [inspect_target_package(d, in_scope, pkg_cache) for d in req_dirs]
print(json.dumps(reports[0] if len(reports) == 1 else {"packages": reports}, indent=2))
PYEOF
