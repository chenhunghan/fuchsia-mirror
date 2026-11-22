#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# py_library_data_package: Verifies that Bazel py_library, py_binary, and
# py_binary_host_tool targets migrated from GN python_library/python_binary
# targets with `data_package_name` and `data_sources` stage an importable
# `<data_package_name>/__init__.py` module in `srcs` (via
# `@bazel_skylib//rules:write_file.bzl`) and stage each data file under
# `<data_package_name>/<basename>` in `data` (via
# `@bazel_skylib//rules:copy_file.bzl`, with `is_executable = True` when
# staging an executable target), rather than only emitting GN
# `# @bazel2gn:raw_attribute:` annotations that leave Bazel imports broken.
# Also verifies that `py_zipapp_binary` targets migrated from GN
# `python_binary` retain native `data_package_name` and `data_sources`
# attributes (since `py_zipapp_binary` packages data packages internally and
# does not accept `data`).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$SCRIPT_DIR" "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

script_dir = os.path.abspath(sys.argv[1])
sys.path.insert(0, script_dir)
try:
    import planter_check_lib as pcl
except ImportError:
    pcl = None

workdir = os.path.abspath(sys.argv[2])
raw_target_dir = sys.argv[3].strip()
change_base = (
    os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    or (pcl.resolve_change_base(workdir) if pcl is not None else "")
    or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
    or "HEAD"
)

SENTINEL = "## BAZEL2GN SENTINEL"
GN_TARGET_RE = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')

EXECUTABLE_BAZEL_RULES = {
    "cc_binary",
    "cc_binary_host_tool",
    "ffx_tool",
    "fx_cc_binary",
    "go_binary",
    "go_binary_host_tool",
    "idk_cc_binary_host_tool",
    "idk_ffx_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "py_binary",
    "py_binary_host_tool",
    "py_zipapp_binary",
    "rust_binary",
    "rustc_binary",
    "rustc_binary_host_tool",
}


# Emit a structured finding dictionary for planter.
def _emit(findings, category, severity, file_path, line, message, remediation=""):
    entry = {
        "source": "py_library_data_package",
        "category": category,
        "severity": severity,
        "file": file_path,
        "line": line,
        "message": message,
    }
    if remediation:
        entry["remediation"] = remediation
    findings.append(entry)


# Run a read-only git command in workdir and return stdout (or "" on failure).
def _git(args):
    try:
        proc = subprocess.run(
            ["git", "-C", workdir] + args,
            capture_output=True,
            text=True,
            check=False,
        )
        return proc.stdout if proc.returncode == 0 else ""
    except OSError:
        return ""


# Read a UTF-8 file from disk, returning "" if missing or unreadable.
def _read_file_text(path):
    if not os.path.isfile(path):
        return ""
    try:
        with open(path, encoding="utf-8") as fh:
            return fh.read()
    except OSError:
        return ""


# Read a file at a git revision, returning "" if unavailable.
def _read_git_file(rev, rel_path):
    if not rev:
        return ""
    return _git(["show", f"{rev}:{rel_path}"])


# Normalize a package-relative file path without stripping leading dots from names.
def _norm_rel_path(path):
    stripped = (path or "").strip()
    if not stripped:
        return ""
    norm = os.path.normpath(stripped).replace("\\", "/")
    if norm == ".":
        return ""
    while norm.startswith("./"):
        norm = norm[2:]
    return norm.lstrip("/")


# Collect package directories containing BUILD.bazel from TARGET_DIR and git diff.
def _collect_target_pkg_dirs():
    dirs = []
    if raw_target_dir:
        for tok in re.split(r"[\s,]+", raw_target_dir):
            cleaned = tok.strip().strip("/")
            if cleaned:
                dirs.append(cleaned)
    changed_out = _git(["diff", "--name-only", change_base])
    untracked_out = _git(["ls-files", "--others", "--exclude-standard"])
    for rel in (changed_out + "\n" + untracked_out).splitlines():
        rel = rel.strip()
        if not rel:
            continue
        if os.path.basename(rel) in ("BUILD.bazel", "BUILD.gn"):
            pdir = os.path.dirname(rel).strip("/")
            if pdir:
                dirs.append(pdir)
    unique_dirs = list(dict.fromkeys(dirs))
    return [
        d
        for d in unique_dirs
        if os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))
    ]


# Strip GN `#` comments while preserving string literals.
def _strip_gn_comments(text):
    return "\n".join(
        re.sub(r'("(?:\\.|[^"\\])*")|#.*$', lambda m: m.group(1) or "", ln)
        for ln in text.splitlines()
    )


# Extract simple top-level GN string-list variable assignments in a BUILD.gn file.
def _extract_gn_file_string_lists(text):
    cleaned = _strip_gn_comments(text)
    lists = {}
    for m in re.finditer(r"(?m)^[ \t]*([A-Za-z_]\w*)\s*=\s*\[([^\]]*)\]", cleaned):
        lists[m.group(1)] = re.findall(r'"([^"]*)"', m.group(2))
    return lists


# Parse `data_package_name` and `data_sources` from a GN target body or raw_attribute lines.
def _parse_gn_data_attrs(snippet, file_lists=None):
    cleaned = _strip_gn_comments(snippet)
    pkg_m = re.search(r'(?<![\w.])data_package_name\s*=\s*"([^"]+)"', cleaned)
    data_pkg = pkg_m.group(1).strip() if pkg_m else ""
    literal_sources = []
    has_dynamic_sources = False
    for m in re.finditer(
        r"(?<![\w.])data_sources\s*\+?=\s*\[([^\]]*)\]", cleaned, re.S
    ):
        inner = m.group(1)
        literal_sources.extend(re.findall(r'"([^"]*)"', inner))
        no_str = re.sub(r'"(?:\\.|[^"\\])*"', "", inner)
        for var in re.findall(r"\b([A-Za-z_]\w*)\b", no_str):
            if file_lists and var in file_lists:
                literal_sources.extend(file_lists[var])
            else:
                has_dynamic_sources = True
    for m in re.finditer(
        r"(?<![\w.])data_sources\s*\+?=\s*([A-Za-z_]\w*)\b", cleaned
    ):
        var = m.group(1)
        if file_lists and var in file_lists:
            literal_sources.extend(file_lists[var])
        else:
            has_dynamic_sources = True
    return data_pkg, literal_sources, has_dynamic_sources


# Extract literal runtime basenames from GN `data_sources` entries.
def _extract_data_basenames(sources, has_dynamic_in):
    basenames = []
    has_dynamic = bool(has_dynamic_in)
    for src in sources:
        entry = src.strip().rstrip("/")
        if not entry:
            continue
        base = os.path.basename(entry)
        if not base or "$" in base or "{" in base or "}" in base:
            has_dynamic = True
            continue
        if base not in basenames:
            basenames.append(base)
    return basenames, has_dynamic


# Parse GN python_library / python_binary targets and their data_package_name/data_sources.
def _parse_gn_python_targets(gn_text):
    out = {}
    if not gn_text:
        return out
    file_lists = _extract_gn_file_string_lists(gn_text)
    sentinel_pos = gn_text.find(SENTINEL)
    for m in GN_TARGET_RE.finditer(gn_text):
        tmpl = m.group(1)
        if tmpl not in ("python_library", "python_binary"):
            continue
        tname = m.group(2)
        depth, i = 1, m.end()
        in_str = False
        escape = False
        while i < len(gn_text) and depth > 0:
            ch = gn_text[i]
            if in_str:
                if escape:
                    escape = False
                elif ch == "\\":
                    escape = True
                elif ch == '"':
                    in_str = False
            else:
                if ch == '"':
                    in_str = True
                elif ch == "#":
                    nl = gn_text.find("\n", i)
                    if nl == -1:
                        break
                    i = nl
                elif ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
            i += 1
        body = gn_text[m.end() : i - 1]
        data_pkg, sources, has_dyn = _parse_gn_data_attrs(body, file_lists)
        below_sentinel = sentinel_pos != -1 and m.start() > sentinel_pos
        out[tname] = {
            "tmpl": tmpl,
            "data_package_name": data_pkg,
            "data_sources": sources,
            "has_dynamic_sources": has_dyn,
            "below_sentinel": below_sentinel,
        }
    return out


# Extract aliased load() symbol bindings so aliased rules map to their canonical names.
def _extract_load_aliases(tree):
    aliases = {}
    for stmt in tree.body:
        if not isinstance(stmt, ast.Expr) or not isinstance(stmt.value, ast.Call):
            continue
        call = stmt.value
        if not (isinstance(call.func, ast.Name) and call.func.id == "load"):
            continue
        for kw in call.keywords:
            if (
                kw.arg
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                aliases[kw.arg] = kw.value.value
    return aliases


# Recursively evaluate string literals from list/tuple/concat/select/variable AST nodes.
def _eval_str_list(node, env):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, (ast.List, ast.Tuple)):
        vals = []
        for elem in node.elts:
            vals.extend(_eval_str_list(elem, env))
        return vals
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _eval_str_list(node.left, env) + _eval_str_list(node.right, env)
    if isinstance(node, ast.Name):
        return list(env.get(node.id, []))
    if isinstance(node, ast.Call):
        if (
            isinstance(node.func, ast.Name)
            and node.func.id == "select"
            and node.args
            and isinstance(node.args[0], ast.Dict)
        ):
            vals = []
            for dval in node.args[0].values:
                vals.extend(_eval_str_list(dval, env))
            return vals
    return []


# Report whether `_eval_str_list()` drops part of a node (e.g. `glob()` or a loaded symbol).
def _has_unevaluable_str_list(node, env):
    if node is None:
        return False
    if isinstance(node, ast.Constant):
        return not isinstance(node.value, str)
    if isinstance(node, (ast.List, ast.Tuple)):
        return any(_has_unevaluable_str_list(elem, env) for elem in node.elts)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _has_unevaluable_str_list(node.left, env) or _has_unevaluable_str_list(
            node.right, env
        )
    if isinstance(node, ast.Name):
        return node.id not in env
    if (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "select"
        and node.args
        and isinstance(node.args[0], ast.Dict)
    ):
        return any(_has_unevaluable_str_list(dval, env) for dval in node.args[0].values)
    return True


# Evaluate a single string literal or string variable reference.
def _eval_str(node, env):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.Name):
        vals = env.get(node.id, [])
        return vals[0] if len(vals) == 1 else ""
    return ""


# Evaluate a boolean literal node.
def _eval_bool(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, bool):
        return node.value
    return None


# Collect simple top-level string and string-list variable bindings from a BUILD.bazel AST.
def _collect_top_level_env(tree):
    env = {}
    for stmt in tree.body:
        if not isinstance(stmt, ast.Assign) or len(stmt.targets) != 1:
            continue
        target = stmt.targets[0]
        if isinstance(target, ast.Name):
            vals = _eval_str_list(stmt.value, env)
            if vals:
                env[target.id] = vals
    return env


# Collect contiguous `# @bazel2gn:raw_attribute:...` lines attached above a target call.
def _attached_raw_attributes(lines, call_lineno):
    raw_lines = []
    idx = call_lineno - 2
    while idx >= 0:
        stripped = lines[idx].strip()
        if not stripped.startswith("#"):
            break
        m = re.match(r"^#\s*@bazel2gn:raw_attribute:(.*)$", stripped)
        if m:
            raw_lines.append(m.group(1).strip())
        idx -= 1
    raw_lines.reverse()
    return "\n".join(raw_lines)


# Parse a BUILD.bazel file for py_library/py_binary/py_binary_host_tool/py_zipapp_binary, write_file, copy_file, genrule, filegroup.
def _parse_bazel_package(bazel_text):
    if not bazel_text:
        return None
    try:
        tree = ast.parse(bazel_text)
    except SyntaxError:
        return None
    lines = bazel_text.splitlines()
    aliases = _extract_load_aliases(tree)
    env = _collect_top_level_env(tree)
    py_targets = {}
    zipapp_targets = {}
    target_outputs = {}
    filegroup_srcs = {}
    copy_files = {}
    rule_kinds = {}

    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        if isinstance(node.func, ast.Name):
            raw_rule = node.func.id
        elif isinstance(node.func, ast.Attribute):
            raw_rule = node.func.attr
        else:
            continue
        rule = aliases.get(raw_rule, raw_rule)
        kw_map = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        tname = _eval_str(kw_map.get("name"), env)
        if not tname:
            continue
        rule_kinds[tname] = rule
        if rule == "write_file":
            out_path = _eval_str(kw_map.get("out"), env)
            if out_path:
                target_outputs[tname] = [_norm_rel_path(out_path)]
        elif rule == "copy_file":
            out_path = _eval_str(kw_map.get("out"), env)
            src_val = _eval_str(kw_map.get("src"), env)
            is_exec = _eval_bool(kw_map.get("is_executable"))
            if out_path:
                target_outputs[tname] = [_norm_rel_path(out_path)]
            copy_files[tname] = {
                "src": src_val,
                "out": out_path,
                "is_executable": is_exec,
                "line": node.lineno,
            }
        elif rule == "genrule":
            outs = [_norm_rel_path(o) for o in _eval_str_list(kw_map.get("outs"), env)]
            outs = [o for o in outs if o]
            if outs:
                target_outputs[tname] = outs
        elif rule == "filegroup":
            filegroup_srcs[tname] = _eval_str_list(kw_map.get("srcs"), env)
        elif rule in ("py_library", "py_binary", "py_binary_host_tool"):
            raw_block = _attached_raw_attributes(lines, node.lineno)
            raw_pkg, raw_srcs, raw_dyn = _parse_gn_data_attrs(raw_block)
            tcw_node = kw_map.get("target_compatible_with")
            is_host_compat = bool(
                tcw_node is not None
                and any(
                    isinstance(n, ast.Name)
                    and n.id in ("HOST_OS_CONSTRAINTS", "HOST_CONSTRAINTS")
                    for n in ast.walk(tcw_node)
                )
            )
            py_targets[tname] = {
                "rule": rule,
                "line": node.lineno,
                "srcs": _eval_str_list(kw_map.get("srcs"), env),
                "data": _eval_str_list(kw_map.get("data"), env),
                "deps": _eval_str_list(kw_map.get("deps"), env),
                "imports": _eval_str_list(kw_map.get("imports"), env),
                "testonly": _eval_bool(kw_map.get("testonly")) is True,
                "is_host_compat": is_host_compat,
                "raw_data_pkg": raw_pkg,
                "raw_data_sources": raw_srcs,
                "raw_has_dyn": raw_dyn,
            }
        elif rule == "py_zipapp_binary":
            ds_node = kw_map.get("data_sources")
            zipapp_targets[tname] = {
                "line": node.lineno,
                "data_package_name": _eval_str(kw_map.get("data_package_name"), env),
                "data_sources": _eval_str_list(ds_node, env),
                "has_data_sources_attr": ds_node is not None,
                "has_unevaluated_data_sources": _has_unevaluable_str_list(ds_node, env),
            }

    return {
        "py_targets": py_targets,
        "zipapp_targets": zipapp_targets,
        "target_outputs": target_outputs,
        "filegroup_srcs": filegroup_srcs,
        "copy_files": copy_files,
        "rule_kinds": rule_kinds,
    }


# Parse an in-repo Bazel label into (pkg, target_name), or None for files/external repos.
def _parse_inrepo_label(label, current_pkg):
    s = (label or "").strip()
    if not s or s.startswith("@"):
        return None
    if s.startswith(":"):
        return current_pkg, s[1:]
    if s.startswith("//"):
        rest = s[2:]
        if ":" in rest:
            pkg, name = rest.split(":", 1)
            return pkg.strip("/"), name
        pkg = rest.strip("/")
        return pkg, os.path.basename(pkg)
    return None


# Resolve srcs/data entries to normalized package-relative file paths and local copy_files.
def _resolve_local_file_paths(entries, pkg_dir, parsed_pkg, seen=None):
    if seen is None:
        seen = set()
    paths = set()
    used_copy_files = set()
    target_outputs = parsed_pkg["target_outputs"]
    filegroup_srcs = parsed_pkg["filegroup_srcs"]
    copy_files = parsed_pkg["copy_files"]

    for entry in entries:
        parsed_lbl = _parse_inrepo_label(entry, pkg_dir)
        if parsed_lbl is not None:
            lpkg, lname = parsed_lbl
            if lpkg != pkg_dir or lname in seen:
                continue
            seen.add(lname)
            if lname in target_outputs:
                for out_p in target_outputs[lname]:
                    if out_p:
                        paths.add(out_p)
            if lname in copy_files:
                used_copy_files.add(lname)
            if lname in filegroup_srcs:
                sub_paths, sub_cfs = _resolve_local_file_paths(
                    filegroup_srcs[lname], pkg_dir, parsed_pkg, seen
                )
                paths.update(sub_paths)
                used_copy_files.update(sub_cfs)
        elif not entry.strip().startswith(("@", "//", ":")):
            norm = _norm_rel_path(entry)
            if norm:
                paths.add(norm)
    return paths, used_copy_files


# Follow same-package py_library deps to collect helper-provided init and data paths.
def _collect_py_dep_paths(dep_entries, pkg_dir, parsed_pkg):
    dep_src_paths = set()
    dep_data_paths = set()
    dep_copy_files = set()
    queue = list(dep_entries)
    seen = set()
    py_targets = parsed_pkg["py_targets"]

    while queue:
        entry = queue.pop(0)
        parsed_lbl = _parse_inrepo_label(entry, pkg_dir)
        if parsed_lbl is None or parsed_lbl[0] != pkg_dir:
            continue
        lname = parsed_lbl[1]
        if lname in seen or lname not in py_targets:
            continue
        seen.add(lname)
        dep_info = py_targets[lname]
        sp, scf = _resolve_local_file_paths(dep_info["srcs"], pkg_dir, parsed_pkg)
        dp, dcf = _resolve_local_file_paths(dep_info["data"], pkg_dir, parsed_pkg)
        dep_src_paths.update(sp)
        dep_data_paths.update(dp)
        dep_copy_files.update(scf)
        dep_copy_files.update(dcf)
        queue.extend(dep_info["deps"])

    return dep_src_paths, dep_data_paths, dep_copy_files


# Collect the basenames py_zipapp_binary packages for `data_sources` entries, and whether
# any entry (in another package or repository, or a target whose outputs aren't parsed)
# can't be resolved.
def _zipapp_data_basenames(entries, pkg_dir, parsed_pkg):
    basenames = set()
    unresolvable = False
    for entry in entries:
        parsed_lbl = _parse_inrepo_label(entry, pkg_dir)
        if parsed_lbl is None:
            if entry.strip().startswith("@"):
                unresolvable = True
            elif norm := _norm_rel_path(entry):
                basenames.add(os.path.basename(norm))
            continue
        lpkg, lname = parsed_lbl
        if lpkg != pkg_dir:
            unresolvable = True
        elif lname in parsed_pkg["rule_kinds"]:
            paths, _ = _resolve_local_file_paths([entry], pkg_dir, parsed_pkg)
            if paths:
                basenames.update(os.path.basename(p) for p in paths)
            else:
                unresolvable = True
        else:
            # A same-package source file label, such as `:files/item.txt`.
            basenames.add(os.path.basename(lname))
    return basenames, unresolvable


# Infer the package directory prefix inside the Bazel package for data_package_name.
def _infer_pkg_prefix(rule_name, raw_srcs, data_pkg_dir):
    if rule_name in ("py_binary", "py_binary_host_tool"):
        return ""
    init_paths = []
    for entry in raw_srcs:
        if entry.strip().startswith(("@", "//", ":")):
            continue
        norm = _norm_rel_path(entry)
        if norm == "__init__.py" or norm.endswith("/__init__.py"):
            init_paths.append(norm)
    if not init_paths:
        return ""
    non_data_inits = [
        p
        for p in init_paths
        if p != f"{data_pkg_dir}/__init__.py"
        and not p.endswith(f"/{data_pkg_dir}/__init__.py")
    ]
    candidates = non_data_inits if non_data_inits else init_paths
    shallowest = min(candidates, key=lambda p: (p.count("/"), p))
    parent = os.path.dirname(shallowest)
    return f"{parent}/" if parent else ""


# Look up the Bazel rule kind of an in-repo target (cached per package).
def _lookup_target_rule_kind(pkg, target_name, pkg_cache):
    if pkg not in pkg_cache:
        bpath = (
            os.path.join(workdir, pkg, "BUILD.bazel")
            if pkg
            else os.path.join(workdir, "BUILD.bazel")
        )
        pkg_cache[pkg] = _parse_bazel_package(_read_file_text(bpath))
    parsed = pkg_cache[pkg]
    if not parsed:
        return ""
    return parsed["rule_kinds"].get(target_name, "")


# Format a reusable remediation snippet for staging data_package_name/__init__.py.
def _format_init_remediation(
    bazel_rel, tname, expected_init, data_pkg_dir, has_gn_file, is_host_compat
):
    init_target = f"{data_pkg_dir.replace('/', '_')}_init"
    host_kw = (
        ", target_compatible_with = HOST_OS_CONSTRAINTS" if is_host_compat else ""
    )
    if has_gn_file:
        return (
            f"In '{bazel_rel}', load `write_file` from "
            '`@bazel_skylib//rules:write_file.bzl`, add a documented `# @bazel2gn:skip` '
            f'`write_file(name = "{init_target}", out = "{expected_init}", '
            f'content = [""]{host_kw})`, and add `":{init_target}",  # @bazel2gn:skip` '
            f'to `{tname}` `srcs` while keeping `# @bazel2gn:raw_attribute:` for GN '
            "`data_package_name`/`data_sources`, then run `fx bazel2gn`."
        )
    return (
        f"In '{bazel_rel}', load `write_file` from "
        "`@bazel_skylib//rules:write_file.bzl`, add "
        f'`write_file(name = "{init_target}", out = "{expected_init}", '
        f'content = [""]{host_kw})`, and add `":{init_target}"` to `{tname}` `srcs`.'
    )


# Format a reusable remediation snippet for staging data_sources via copy_file.
def _format_data_remediation(
    bazel_rel,
    pkg_dir,
    tname,
    pkg_prefix,
    data_pkg_dir,
    missing_names,
    has_gn_file,
    is_host_compat,
    is_testonly,
):
    sample = missing_names[0] if missing_names else "<file>"
    out_sample = f"{pkg_prefix}{data_pkg_dir}/{sample}"
    host_kw = (
        ", target_compatible_with = HOST_OS_CONSTRAINTS" if is_host_compat else ""
    )
    testonly_kw = ", testonly = True" if is_testonly else ""
    skip_note = (
        " with an explanatory comment and `# @bazel2gn:skip` (and `# @bazel2gn:skip` "
        "on `data = [...]` while keeping `# @bazel2gn:raw_attribute:` for GN "
        "`data_package_name`, `data_sources`, and any non-Python `library_deps`, "
        "then re-running `fx bazel2gn`)"
        if has_gn_file
        else ""
    )
    return (
        f"In '{bazel_rel}', load `copy_file` from `@bazel_skylib//rules:copy_file.bzl` "
        f"and stage each data file under `{pkg_prefix}{data_pkg_dir}/` via "
        f'`copy_file(name = "<name>_data"{testonly_kw}, src = "<label>", '
        f'out = "{out_sample}", is_executable = True{host_kw})` (set '
        "`is_executable = True` for executable binaries/tools) and list the "
        f"`copy_file` target(s) in `{tname}` `data`{skip_note}. If a data file is "
        "produced by an already-migrated target in another package lacking visibility, "
        f'add `"//{pkg_dir}:__pkg__"` to that target\'s `visibility` and run '
        "`fx bazel2gn`."
    )


# Check a single migrated package for py_library data_package_name/data_sources parity.
def check_py_library_data_package(pkg_dir, findings, pkg_cache):
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")
    bazel_text = _read_file_text(os.path.join(workdir, bazel_rel))
    parsed = _parse_bazel_package(bazel_text)
    if not parsed:
        return
    pkg_cache[pkg_dir] = parsed
    if not parsed["py_targets"] and not parsed["zipapp_targets"]:
        return

    gn_abs = os.path.join(workdir, gn_rel)
    has_gn_file = os.path.isfile(gn_abs)
    cur_gn = _parse_gn_python_targets(_read_file_text(gn_abs)) if has_gn_file else {}
    base_gn = _parse_gn_python_targets(_read_git_file(change_base, gn_rel))

    # Verify py_zipapp_binary targets retain native data_package_name / data_sources
    # from baseline GN python_binary (without requiring write_file/copy_file staging).
    for tname, zinfo in parsed["zipapp_targets"].items():
        bg = base_gn.get(tname, {})
        exp_pkg = bg.get("data_package_name", "")
        if exp_pkg and zinfo["data_package_name"] != exp_pkg:
            _emit(
                findings,
                "py_zipapp_missing_data_package",
                "error",
                bazel_rel,
                zinfo["line"],
                (
                    f'py_zipapp_binary(name = "{tname}") is missing or mismatched on '
                    f'`data_package_name = "{exp_pkg}"` compared to baseline `{gn_rel}`.'
                ),
                (
                    f'Set `data_package_name = "{exp_pkg}"` natively on '
                    f'`py_zipapp_binary(name = "{tname}")` in \'{bazel_rel}\'.'
                ),
            )
        if bg.get("data_sources") or bg.get("has_dynamic_sources"):
            if zinfo["has_unevaluated_data_sources"]:
                continue
            z_basenames, z_unresolvable = _zipapp_data_basenames(
                zinfo["data_sources"], pkg_dir, parsed
            )
            req_basenames, has_dyn_base = _extract_data_basenames(
                bg.get("data_sources", []), bg.get("has_dynamic_sources", False)
            )
            missing_names = (
                [b for b in req_basenames if b not in z_basenames]
                if not z_unresolvable
                else []
            )
            if (
                not zinfo["has_data_sources_attr"]
                or missing_names
                or (not zinfo["data_sources"] and (req_basenames or has_dyn_base))
            ):
                missing_desc = (
                    ", ".join(f"`{n}`" for n in missing_names)
                    if missing_names
                    else "GN `data_sources` entries"
                )
                _emit(
                    findings,
                    "py_zipapp_missing_data_source",
                    "error",
                    bazel_rel,
                    zinfo["line"],
                    (
                        f'py_zipapp_binary(name = "{tname}") is missing {missing_desc} '
                        f"in `data_sources` compared to baseline `{gn_rel}`."
                    ),
                    (
                        f'Set `data_sources = [...]` natively on '
                        f'`py_zipapp_binary(name = "{tname}")` in \'{bazel_rel}\'.'
                    ),
                )

    for tname, pinfo in parsed["py_targets"].items():
        cg = cur_gn.get(tname, {})
        bg = base_gn.get(tname, {})

        # Verify dual-build GN python_library targets did not drop data_package_name/data_sources.
        if has_gn_file and cg.get("below_sentinel") and bg:
            base_has_pkg = bool(bg.get("data_package_name"))
            cur_has_pkg = bool(cg.get("data_package_name"))
            base_has_src = bool(
                bg.get("data_sources") or bg.get("has_dynamic_sources")
            )
            cur_has_src = bool(
                cg.get("data_sources") or cg.get("has_dynamic_sources")
            )
            if (base_has_pkg and not cur_has_pkg) or (
                base_has_src and not cur_has_src
            ):
                _emit(
                    findings,
                    "py_library_gn_data_package_dropped",
                    "error",
                    bazel_rel,
                    pinfo["line"],
                    (
                        f"{pinfo['rule']}(name = \"{tname}\") dropped GN "
                        "`data_package_name` / `data_sources` below "
                        "`## BAZEL2GN SENTINEL` compared to the baseline BUILD.gn."
                    ),
                    (
                        f"Preserve GN `data_package_name`, `data_sources`, and any "
                        "non-Python `library_deps` via documented "
                        "`# @bazel2gn:raw_attribute:` annotations above "
                        f'`{pinfo["rule"]}(name = "{tname}")` in \'{bazel_rel}\' '
                        "and re-run `fx bazel2gn`."
                    ),
                )

        data_pkg = (
            pinfo["raw_data_pkg"]
            or cg.get("data_package_name", "")
            or bg.get("data_package_name", "")
        )
        if pinfo["raw_data_sources"] or pinfo["raw_has_dyn"]:
            data_sources = pinfo["raw_data_sources"]
            has_dyn = pinfo["raw_has_dyn"]
        elif cg.get("data_sources") or cg.get("has_dynamic_sources"):
            data_sources = cg.get("data_sources", [])
            has_dyn = cg.get("has_dynamic_sources", False)
        else:
            data_sources = bg.get("data_sources", [])
            has_dyn = bg.get("has_dynamic_sources", False)

        if not data_pkg:
            continue

        data_pkg_dir = data_pkg.strip("/").replace(".", "/")
        pkg_prefix = _infer_pkg_prefix(pinfo["rule"], pinfo["srcs"], data_pkg_dir)
        expected_init = f"{pkg_prefix}{data_pkg_dir}/__init__.py"

        src_paths, src_cfs = _resolve_local_file_paths(
            pinfo["srcs"], pkg_dir, parsed
        )
        data_paths, data_cfs = _resolve_local_file_paths(
            pinfo["data"], pkg_dir, parsed
        )
        dep_src_paths, dep_data_paths, dep_cfs = _collect_py_dep_paths(
            pinfo["deps"], pkg_dir, parsed
        )

        all_src_paths = src_paths | dep_src_paths
        all_staged_paths = src_paths | data_paths | dep_src_paths | dep_data_paths
        used_copy_files = src_cfs | data_cfs | dep_cfs

        # Check 1: `<data_package_name>/__init__.py` must be present in Bazel srcs.
        if expected_init not in all_src_paths:
            _emit(
                findings,
                "py_library_missing_data_package_init",
                "error",
                bazel_rel,
                pinfo["line"],
                (
                    f'{pinfo["rule"]}(name = "{tname}") uses GN '
                    f'`data_package_name = "{data_pkg}"`, but Bazel `srcs` does not '
                    f"provide `{expected_init}`. Importing `{data_pkg}` via "
                    "`importlib.resources` under Bazel will raise ImportError."
                ),
                _format_init_remediation(
                    bazel_rel,
                    tname,
                    expected_init,
                    data_pkg_dir,
                    has_gn_file,
                    pinfo["is_host_compat"],
                ),
            )

        # Check 2: Each `data_sources` file must be staged under `<data_package_name>/`.
        req_basenames, has_dyn_base = _extract_data_basenames(data_sources, has_dyn)
        missing_names = [
            base
            for base in req_basenames
            if f"{pkg_prefix}{data_pkg_dir}/{base}" not in all_staged_paths
        ]
        staged_under_pkg = [
            p
            for p in all_staged_paths
            if p.startswith(f"{pkg_prefix}{data_pkg_dir}/") and p != expected_init
        ]
        if missing_names or (not req_basenames and has_dyn_base and not staged_under_pkg):
            missing_desc = (
                ", ".join(f"`{pkg_prefix}{data_pkg_dir}/{n}`" for n in missing_names)
                if missing_names
                else f"files under `{pkg_prefix}{data_pkg_dir}/`"
            )
            _emit(
                findings,
                "py_library_missing_data_source",
                "error",
                bazel_rel,
                pinfo["line"],
                (
                    f'{pinfo["rule"]}(name = "{tname}") uses GN '
                    f'`data_package_name = "{data_pkg}"` with `data_sources`, but '
                    f"Bazel `data` does not stage {missing_desc}. "
                    f'`importlib.resources.files("{data_pkg}")` will fail under Bazel.'
                ),
                _format_data_remediation(
                    bazel_rel,
                    pkg_dir,
                    tname,
                    pkg_prefix,
                    data_pkg_dir,
                    missing_names,
                    has_gn_file,
                    pinfo["is_host_compat"],
                    pinfo["testonly"],
                ),
            )

        # Check 3: copy_file staging an executable target must set is_executable = True.
        for cf_name in sorted(used_copy_files):
            cf = parsed["copy_files"].get(cf_name)
            if not cf:
                continue
            cf_out = _norm_rel_path(cf["out"])
            if not cf_out.startswith(f"{pkg_prefix}{data_pkg_dir}/"):
                continue
            if cf["is_executable"] is True:
                continue
            src_lbl = _parse_inrepo_label(cf["src"], pkg_dir)
            if src_lbl is None:
                continue
            src_rule = _lookup_target_rule_kind(src_lbl[0], src_lbl[1], pkg_cache)
            if src_rule in EXECUTABLE_BAZEL_RULES:
                _emit(
                    findings,
                    "py_library_data_copy_not_executable",
                    "error",
                    bazel_rel,
                    cf["line"],
                    (
                        f'copy_file(name = "{cf_name}") stages executable '
                        f'`{src_rule}` target `{cf["src"]}` into `{cf_out}` '
                        "without `is_executable = True`."
                    ),
                    (
                        f'Set `is_executable = True` on `copy_file(name = "{cf_name}")` '
                        f"in '{bazel_rel}' so the staged binary retains execute "
                        "permissions in Bazel runfiles."
                    ),
                )


def main():
    findings = []
    pkg_cache = {}
    for pkg_dir in _collect_target_pkg_dirs():
        check_py_library_data_package(pkg_dir, findings, pkg_cache)
    print(json.dumps(findings, indent=2))


main()
PYEOF
