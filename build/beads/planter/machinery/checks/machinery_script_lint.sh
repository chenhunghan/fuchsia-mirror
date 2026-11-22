#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic static check for Planter shell scripts, inline Python heredocs,
# Python modules, and JSON manifests across both active `$PLANTER_PROJECT_DIR`
# machinery (enabled by default; disable with `--no-project-dir` or
# `PLANTER_LINT_PROJECT_DIR=0`) and modified `build/beads/planter/` repo files:
# 1. Validates shell syntax via `bash -n` (`machinery_shell_syntax_error`).
# 2. Validates Python AST syntax (`machinery_python_syntax_error`).
# 3. Checks lexical scopes via `symtable` and local helper module imports/attrs
#    for undefined names (`machinery_undefined_python_name`).
# 4. Checks module-level statement, module-scope helper-call, and function-local
#    execution order for variables read before assignment
#    (`machinery_python_use_before_def`).
# 5. Checks `checks/*.sh` inline Python blocks for uncalled top-level `check_*`
#    functions (`machinery_uncalled_check_function`), duplicate function/class
#    definitions (`machinery_duplicate_python_def`), and duplicate dict keys
#    (`machinery_duplicate_dict_key`).
# 6. Validates JSON syntax for manifests (`machinery_json_syntax_error`).

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${PLANTER_PROJECT_DIR:-$(dirname "$SCRIPT_DIR")}"
LINT_PROJECT_DIR="${PLANTER_LINT_PROJECT_DIR:-1}"
EXPLICIT_INVOCATION=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --project-dir)
      LINT_PROJECT_DIR="1"
      EXPLICIT_INVOCATION=1
      shift
      ;;
    --no-project-dir)
      LINT_PROJECT_DIR="0"
      EXPLICIT_INVOCATION=1
      shift
      ;;
    --)
      EXPLICIT_INVOCATION=1
      shift
      break
      ;;
    *)
      EXPLICIT_INVOCATION=1
      break
      ;;
  esac
done

# Skip duplicate project-wide runs when Planter invokes checks once per target directory.
norm_target_dir="${TARGET_DIR#"${TARGET_DIR%%[![:space:]/]*}"}"
norm_target_dir="${norm_target_dir%"${norm_target_dir##*[![:space:]/]}"}"
trimmed_dirs="${TARGET_DIRS#"${TARGET_DIRS%%[![:space:],]*}"}"
first_dir="${trimmed_dirs%%[[:space:],]*}"
first_dir="${first_dir#"${first_dir%%[![:space:]/]*}"}"
first_dir="${first_dir%"${first_dir##*[![:space:]/]}"}"
if [[ "$EXPLICIT_INVOCATION" -eq 0 && -n "$norm_target_dir" && -n "$first_dir" \
      && "$norm_target_dir" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" "$CHANGE_BASE" "$PROJECT_DIR" "$LINT_PROJECT_DIR" "$@" <<'PYEOF'
import ast
import builtins
import glob
import json
import os
import re
import subprocess
import symtable
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"
project_dir = os.path.abspath(sys.argv[3])
lint_project_dir = sys.argv[4].strip() == "1"
extra_args = sys.argv[5:]

BUILTIN_NAMES = set(dir(builtins)) | {
    "__annotate__",
    "__annotations__",
    "__builtins__",
    "__conditional_annotations__",
    "__doc__",
    "__file__",
    "__loader__",
    "__name__",
    "__package__",
    "__spec__",
}

HEREDOC_RE = re.compile(
    r"(?:^|\n)[ \t]*(?!#)[^\n]*?\bpython3(?:\s+[^\n<#]*)?"
    r"\s*<<-?\s*['\"\\]?([A-Za-z_][A-Za-z0-9_]*)['\"]?[^\n]*\n"
    r"(.*?)\n[ \t]*\1(?=\n|$)",
    re.S,
)

EAGER_GENEXP_CALLS = {
    "all",
    "any",
    "dict",
    "enumerate",
    "frozenset",
    "list",
    "max",
    "min",
    "next",
    "set",
    "sorted",
    "sum",
    "tuple",
}
EAGER_GENEXP_METHODS = {"extend", "join", "update"}


# Resolve the machinery root whether `$PLANTER_PROJECT_DIR` points directly at
# `checks/` + `prompts/` or at a parent directory containing `machinery/`.
def resolve_machinery_root(pdir):
    alt_root = os.path.join(pdir, "machinery")
    if (
        os.path.isdir(alt_root)
        and not os.path.islink(alt_root)
        and os.path.exists(os.path.join(alt_root, "prompts", "coder.md"))
        and not os.path.exists(os.path.join(pdir, "prompts", "coder.md"))
    ):
        return alt_root
    return pdir


mroot = resolve_machinery_root(project_dir)


# Collect target files to audit across modified repo paths and `$PLANTER_PROJECT_DIR`.
def collect_target_files():
    seen_real = set()
    items = []

    def add_item(full, rel):
        if not os.path.isfile(full):
            return
        real = os.path.realpath(full)
        if real in seen_real:
            return
        seen_real.add(real)
        items.append((full, rel))

    if extra_args:
        mroot_prefix = os.path.realpath(mroot) + os.sep
        for arg in extra_args:
            if os.path.isabs(arg):
                in_mroot = os.path.realpath(arg).startswith(mroot_prefix)
                base = mroot if in_mroot else workdir
                add_item(arg, os.path.relpath(arg, base))
                continue
            for base in (mroot, project_dir, workdir):
                cand = os.path.join(base, arg)
                if os.path.isfile(cand):
                    add_item(cand, os.path.relpath(cand, base))
                    break
        return sorted(items, key=lambda x: x[1])

    env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
    if env_changed:
        changed = {ln.strip() for ln in env_changed.splitlines() if ln.strip()}
    else:
        changed = set()
        for cmd in (
            ["diff", "--name-only", change_base],
            ["ls-files", "--others", "--exclude-standard"],
        ):
            try:
                out = subprocess.check_output(
                    ["git", "-C", workdir] + cmd,
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                out = ""
            for ln in out.splitlines():
                if ln.strip():
                    changed.add(ln.strip())

    for rel in sorted(changed):
        if rel.startswith("build/beads/planter/") and rel.endswith((".sh", ".py", ".json")):
            add_item(os.path.join(workdir, rel), rel)

    if lint_project_dir:
        patterns = (
            "checks/*.sh",
            "checks/*.py",
            "checks/manifest.json",
            "tools/*.sh",
            "tools/*.py",
            "tools/manifest.json",
            "prompts/panel.json",
        )
        for pat in patterns:
            for full in sorted(glob.glob(os.path.join(mroot, pat))):
                add_item(full, os.path.relpath(full, mroot))

    return items


# Check shell syntax of `.sh` files with `bash -n`.
def check_shell_syntax(full_path, rel_path, findings):
    proc = subprocess.run(
        ["bash", "-n", full_path], capture_output=True, text=True, check=False
    )
    if proc.returncode == 0:
        return
    err = (proc.stderr or proc.stdout or "bash -n failed").strip()
    m_ln = re.search(r"\bline\s+(\d+):", err)
    lineno = int(m_ln.group(1)) if m_ln else 1
    findings.append({
        "source": "machinery_script_lint",
        "category": "machinery_shell_syntax_error",
        "severity": "error",
        "file": rel_path,
        "line": lineno,
        "message": f"Shell syntax error in '{rel_path}': {err}",
        "remediation": (
            f"Fix the shell syntax error reported by `bash -n` in '{rel_path}' "
            "so the script parses cleanly."
        ),
    })


# Check JSON syntax of manifest files.
def check_json_syntax(full_path, rel_path, findings):
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            json.load(f)
    except Exception as exc:
        lineno = getattr(exc, "lineno", 1) or 1
        findings.append({
            "source": "machinery_script_lint",
            "category": "machinery_json_syntax_error",
            "severity": "error",
            "file": rel_path,
            "line": lineno,
            "message": f"Invalid JSON in '{rel_path}': {exc}",
            "remediation": (
                f"Fix the JSON syntax in '{rel_path}' so `json.load()` succeeds."
            ),
        })


# Extract inline Python heredocs from `.sh` scripts or full source from `.py` files.
def extract_python_blocks(full_path, text):
    if full_path.endswith(".py"):
        yield 1, text
        return
    for m in HEREDOC_RE.finditer(text):
        start_line = text[: m.start(2)].count("\n") + 1
        yield start_line, m.group(2)


# Map identifier names to their first `ast.Load` line numbers for accurate diagnostics.
def collect_load_lines(tree):
    lines_by_name = {}
    for node in ast.walk(tree):
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            lines_by_name.setdefault(node.id, getattr(node, "lineno", 1))
    return lines_by_name


# Collect `global <name>` symbols that are assigned inside any function body.
def collect_assigned_globals(tree):
    assigned_globals = set()

    def walk_func(fn_node):
        declared = set()
        assigned = set()

        def visit(sub):
            if sub is not fn_node and isinstance(
                sub, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                walk_nested(sub)
                return
            if isinstance(sub, ast.Global):
                declared.update(sub.names)
            elif isinstance(sub, ast.Name) and isinstance(sub.ctx, ast.Store):
                assigned.add(sub.id)
            for child in ast.iter_child_nodes(sub):
                visit(child)

        visit(fn_node)
        assigned_globals.update(declared & assigned)

    def walk_nested(node):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            walk_func(node)
            return
        for child in ast.iter_child_nodes(node):
            walk_nested(child)

    walk_nested(tree)
    return assigned_globals


# Index top-level exported symbols from shared `checks/*.py` helper modules.
def load_local_module_exports(checks_dirs):
    exports_by_mod = {}
    for cdir in checks_dirs:
        if not os.path.isdir(cdir):
            continue
        for py_path in sorted(glob.glob(os.path.join(cdir, "*.py"))):
            mod_name = os.path.splitext(os.path.basename(py_path))[0]
            if mod_name.endswith("_test"):
                continue
            try:
                with open(py_path, "r", encoding="utf-8", errors="replace") as f:
                    mod_tree = ast.parse(f.read(), filename=py_path)
            except (OSError, SyntaxError):
                continue
            names = set(collect_assigned_globals(mod_tree))

            def bind_export(target):
                if isinstance(target, ast.Name):
                    names.add(target.id)
                elif isinstance(target, (ast.Tuple, ast.List)):
                    for elt in target.elts:
                        bind_export(elt)
                elif isinstance(target, ast.Starred):
                    bind_export(target.value)

            def walk_mod_stmts(stmts):
                for node in stmts:
                    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                        names.add(node.name)
                    elif isinstance(node, ast.Assign):
                        for t in node.targets:
                            bind_export(t)
                    elif isinstance(node, ast.AnnAssign) and node.value is not None:
                        bind_export(node.target)
                    elif isinstance(node, ast.AugAssign):
                        bind_export(node.target)
                    elif isinstance(node, ast.Import):
                        for alias in node.names:
                            names.add((alias.asname or alias.name).split(".")[0])
                    elif isinstance(node, ast.ImportFrom):
                        for alias in node.names:
                            names.add(alias.asname or alias.name)
                    elif isinstance(node, (ast.For, ast.AsyncFor)):
                        bind_export(node.target)
                        walk_mod_stmts(node.body)
                        walk_mod_stmts(node.orelse)
                    elif isinstance(node, (ast.If, ast.While)):
                        walk_mod_stmts(node.body)
                        walk_mod_stmts(node.orelse)
                    elif isinstance(node, (ast.With, ast.AsyncWith)):
                        for item in node.items:
                            if item.optional_vars is not None:
                                bind_export(item.optional_vars)
                        walk_mod_stmts(node.body)
                    elif isinstance(node, ast.Try) or (
                        hasattr(ast, "TryStar") and isinstance(node, ast.TryStar)
                    ):
                        walk_mod_stmts(node.body)
                        for handler in node.handlers:
                            if handler.name:
                                names.add(handler.name)
                            walk_mod_stmts(handler.body)
                        walk_mod_stmts(node.orelse)
                        walk_mod_stmts(node.finalbody)

            walk_mod_stmts(mod_tree.body)
            exports_by_mod[mod_name] = names
    return exports_by_mod


MROOT_CHECKS_DIR = os.path.join(mroot, "checks")
WORKDIR_PLANTER_PREFIX = os.path.realpath(
    os.path.join(workdir, "build", "beads", "planter")
) + os.sep
WORKDIR_CHECKS_DIR = os.path.join(
    workdir, "build", "beads", "planter", "machinery", "checks"
)
MROOT_MODULE_EXPORTS = load_local_module_exports((MROOT_CHECKS_DIR,))
WORKDIR_MODULE_EXPORTS = (
    load_local_module_exports((MROOT_CHECKS_DIR, WORKDIR_CHECKS_DIR))
    if os.path.isdir(WORKDIR_CHECKS_DIR)
    and os.path.realpath(WORKDIR_CHECKS_DIR) != os.path.realpath(MROOT_CHECKS_DIR)
    else MROOT_MODULE_EXPORTS
)


# Select helper module exports matching the tree (`workdir` vs `mroot`) of `full_path`.
def module_exports_for_file(full_path):
    if os.path.realpath(full_path).startswith(WORKDIR_PLANTER_PREFIX):
        return WORKDIR_MODULE_EXPORTS
    return MROOT_MODULE_EXPORTS


# Verify imports and attribute lookups against shared `checks/*.py` helper modules.
def check_local_module_refs(tree, rel_path, base_line, local_exports, findings):
    if not local_exports:
        return
    reported = set()

    def walk_scope(scope_node, parent_aliases, parent_shadow):
        scope_aliases = dict(parent_aliases)
        local_shadow = set(parent_shadow)
        none_assign_nodes = set()

        def collect_scope_stmts(stmts):
            for stmt in stmts:
                if isinstance(
                    stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
                ):
                    continue
                if isinstance(stmt, ast.Import):
                    for alias in stmt.names:
                        if alias.name in local_exports:
                            as_id = alias.asname or alias.name
                            scope_aliases[as_id] = alias.name
                            local_shadow.discard(as_id)
                elif isinstance(stmt, ast.ImportFrom):
                    if stmt.module in local_exports:
                        valid = local_exports[stmt.module]
                        for alias in stmt.names:
                            if alias.name != "*" and alias.name not in valid:
                                key = ("import", stmt.module, alias.name)
                                if key not in reported:
                                    reported.add(key)
                                    lineno = (
                                        base_line
                                        + max(getattr(stmt, "lineno", 1), 1)
                                        - 1
                                    )
                                    findings.append({
                                        "source": "machinery_script_lint",
                                        "category": "machinery_undefined_python_name",
                                        "severity": "error",
                                        "file": rel_path,
                                        "line": lineno,
                                        "message": (
                                            f"`from {stmt.module} import {alias.name}` in "
                                            f"'{rel_path}' references missing symbol `{alias.name}`."
                                        ),
                                        "remediation": (
                                            f"Export `{alias.name}` in `{stmt.module}.py` or "
                                            f"import an existing helper in '{rel_path}'."
                                        ),
                                    })
                    for alias in stmt.names:
                        if alias.name != "*":
                            as_id = alias.asname or alias.name
                            if as_id in scope_aliases:
                                local_shadow.add(as_id)
                elif (
                    isinstance(stmt, ast.Assign)
                    and isinstance(stmt.value, ast.Constant)
                    and stmt.value.value is None
                ):
                    for tgt in stmt.targets:
                        if isinstance(tgt, ast.Name):
                            none_assign_nodes.add(id(tgt))
                elif (
                    isinstance(stmt, ast.AnnAssign)
                    and isinstance(stmt.value, ast.Constant)
                    and stmt.value.value is None
                    and isinstance(stmt.target, ast.Name)
                ):
                    none_assign_nodes.add(id(stmt.target))
                for attr in ("body", "orelse", "finalbody"):
                    sub = getattr(stmt, attr, None)
                    if isinstance(sub, list):
                        collect_scope_stmts(sub)
                for handler in getattr(stmt, "handlers", None) or ():
                    collect_scope_stmts(handler.body)
                for case in getattr(stmt, "cases", None) or ():
                    collect_scope_stmts(case.body)

        collect_scope_stmts(scope_node.body)
        if scope_node is not tree:
            for sub in ast.iter_child_nodes(scope_node):
                if isinstance(sub, ast.arguments):
                    for arg in sub.posonlyargs + sub.args + sub.kwonlyargs:
                        local_shadow.add(arg.arg)
                    if sub.vararg:
                        local_shadow.add(sub.vararg.arg)
                    if sub.kwarg:
                        local_shadow.add(sub.kwarg.arg)

        def visit(node):
            if node is not scope_node and isinstance(
                node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                walk_scope(node, scope_aliases, local_shadow)
                return
            if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
                if not (
                    node.id in scope_aliases and id(node) in none_assign_nodes
                ):
                    local_shadow.add(node.id)
            elif (
                isinstance(node, ast.Attribute)
                and isinstance(node.ctx, ast.Load)
                and isinstance(node.value, ast.Name)
                and node.value.id in scope_aliases
                and node.value.id not in local_shadow
            ):
                mod_name = scope_aliases[node.value.id]
                valid = local_exports[mod_name]
                if not node.attr.startswith("__") and node.attr not in valid:
                    key = ("attr", mod_name, node.attr)
                    if key not in reported:
                        reported.add(key)
                        lineno = base_line + max(getattr(node, "lineno", 1), 1) - 1
                        findings.append({
                            "source": "machinery_script_lint",
                            "category": "machinery_undefined_python_name",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": (
                                f"`{node.value.id}.{node.attr}` in '{rel_path}' "
                                f"is not defined in `{mod_name}.py`."
                            ),
                            "remediation": (
                                f"Define `{node.attr}` in `{mod_name}.py` or use "
                                f"an existing `{mod_name}` helper in '{rel_path}'."
                            ),
                        })
            for child in ast.iter_child_nodes(node):
                visit(child)

        visit(scope_node)

    walk_scope(tree, {}, set())


# Check lexical scopes via `symtable` so undefined names in cold branches are caught.
def check_lexical_undefined_names(code, tree, rel_path, base_line, findings):
    if any(
        isinstance(n, ast.ImportFrom) and any(a.name == "*" for a in n.names)
        for n in ast.walk(tree)
    ):
        return
    try:
        root_tbl = symtable.symtable(code, rel_path, "exec")
    except SyntaxError:
        return

    load_lines = collect_load_lines(tree)
    extra_globals = collect_assigned_globals(tree)
    reported = set()

    def walk_scope(tbl, enclosing_defs):
        if tbl.get_type() == "module":
            local_defs = set(extra_globals)
        elif tbl.get_type() == "class":
            local_defs = {"__module__", "__qualname__"}
        else:
            local_defs = set()
        for sym in tbl.get_symbols():
            if (
                sym.is_assigned()
                or sym.is_imported()
                or sym.is_parameter()
                or sym.is_namespace()
            ):
                local_defs.add(sym.get_name())
        visible = enclosing_defs | local_defs
        for sym in tbl.get_symbols():
            name = sym.get_name()
            if (
                sym.is_referenced()
                and not (
                    sym.is_assigned()
                    or sym.is_imported()
                    or sym.is_parameter()
                    or sym.is_namespace()
                )
                and not name.startswith(".")
                and name not in visible
                and name not in BUILTIN_NAMES
                and (tbl.get_name(), name) not in reported
            ):
                reported.add((tbl.get_name(), name))
                py_ln = load_lines.get(name, tbl.get_lineno() or 1)
                lineno = base_line + max(py_ln, 1) - 1
                scope_desc = (
                    "module scope"
                    if tbl.get_type() == "module"
                    else f"scope `{tbl.get_name()}`"
                )
                findings.append({
                    "source": "machinery_script_lint",
                    "category": "machinery_undefined_python_name",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Undefined Python name `{name}` referenced in {scope_desc} "
                        f"of '{rel_path}', which raises `NameError` at runtime."
                    ),
                    "remediation": (
                        f"Define or import `{name}` before referencing it in "
                        f"'{rel_path}', or replace `{name}` with the intended "
                        "in-scope helper or variable."
                    ),
                })
        for child in tbl.get_children():
            if tbl.get_type() != "class":
                child_enclosing = visible
            elif child.get_type() == "annotation":
                child_enclosing = visible | {"__class__", "__classdict__"}
            else:
                child_enclosing = enclosing_defs | {"__class__"}
            walk_scope(child, child_enclosing)

    walk_scope(root_tbl, set())


# Compute module-global names read (directly or transitively) by module-scope
# functions, including helpers nested inside top-level control-flow blocks.
def collect_top_func_globals(code, tree, rel_path):
    try:
        root_tbl = symtable.symtable(code, rel_path, "exec")
    except SyntaxError:
        return {}
    guarded_fns = set()
    top_fn_names = set()

    def visit_mod_stmts(stmts):
        for stmt in stmts:
            if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                top_fn_names.add(stmt.name)
                for sub in ast.walk(stmt):
                    if (
                        isinstance(sub, ast.ExceptHandler)
                        and isinstance(sub.type, ast.Name)
                        and sub.type.id in ("NameError", "Exception", "BaseException")
                    ):
                        guarded_fns.add(stmt.name)
            elif isinstance(
                stmt,
                (ast.For, ast.AsyncFor, ast.If, ast.While, ast.With, ast.AsyncWith),
            ):
                visit_mod_stmts(stmt.body)
                visit_mod_stmts(getattr(stmt, "orelse", ()))
            elif isinstance(stmt, ast.Try) or (
                hasattr(ast, "TryStar") and isinstance(stmt, ast.TryStar)
            ):
                visit_mod_stmts(stmt.body)
                for handler in stmt.handlers:
                    visit_mod_stmts(handler.body)
                visit_mod_stmts(stmt.orelse)
                visit_mod_stmts(stmt.finalbody)
            elif hasattr(ast, "Match") and isinstance(stmt, ast.Match):
                for case in stmt.cases:
                    visit_mod_stmts(case.body)

    visit_mod_stmts(tree.body)

    inline_scope_names = {"listcomp", "dictcomp", "setcomp", "genexpr", "lambda"}

    def read_globals(tbl, seen=None):
        if seen is None:
            seen = set()
        tid = tbl.get_id()
        if tid in seen:
            return set()
        seen.add(tid)
        refs = set()
        called_children = {
            sym.get_name()
            for sym in tbl.get_symbols()
            if sym.is_namespace() and sym.is_referenced()
        }
        for desc in tbl.get_children():
            for dsym in desc.get_symbols():
                if dsym.is_free() and dsym.is_referenced():
                    called_children.add(dsym.get_name())
        for sym in tbl.get_symbols():
            if sym.is_global() and sym.is_referenced():
                name = sym.get_name()
                if name not in BUILTIN_NAMES:
                    refs.add(name)
        for child in tbl.get_children():
            cname = child.get_name()
            if cname in inline_scope_names or (
                child.get_type() == "function" and cname in called_children
            ):
                refs.update(read_globals(child, seen))
        return refs

    direct = {}
    for child in root_tbl.get_children():
        fn_name = child.get_name()
        if (
            child.get_type() == "function"
            and fn_name in top_fn_names
            and fn_name not in guarded_fns
        ):
            direct.setdefault(fn_name, set()).update(read_globals(child))

    trans = {fn: set(gs) for fn, gs in direct.items()}
    changed = True
    while changed:
        changed = False
        for gs in trans.values():
            extra = set()
            for g in gs:
                if g in trans:
                    extra.update(trans[g] - gs)
            if extra:
                gs.update(extra)
                changed = True
    return trans


# Check module-level execution order for variables read before assignment.
def check_module_order(code, tree, rel_path, base_line, findings):
    future_annotations = any(
        isinstance(s, ast.ImportFrom)
        and s.module == "__future__"
        and any(a.name == "annotations" for a in s.names)
        for s in tree.body
    )
    defined = set(BUILTIN_NAMES) | collect_assigned_globals(tree)
    func_globals = collect_top_func_globals(code, tree, rel_path)
    reported = set()

    def bind_target(target, bound_set):
        if isinstance(target, ast.Name):
            bound_set.add(target.id)
        elif isinstance(target, (ast.Tuple, ast.List)):
            for elt in target.elts:
                bind_target(elt, bound_set)
        elif isinstance(target, ast.Starred):
            bind_target(target.value, bound_set)

    def bind_pattern(pat, bound_set):
        for sub in ast.walk(pat):
            if hasattr(ast, "MatchAs") and isinstance(sub, ast.MatchAs) and sub.name:
                bound_set.add(sub.name)
            elif hasattr(ast, "MatchStar") and isinstance(sub, ast.MatchStar) and sub.name:
                bound_set.add(sub.name)
            elif hasattr(ast, "MatchMapping") and isinstance(sub, ast.MatchMapping) and sub.rest:
                bound_set.add(sub.rest)

    def check_expr(node, local_bound, eager_genexp=False):
        if node is None:
            return
        if isinstance(node, ast.NamedExpr):
            check_expr(node.value, local_bound)
            bind_target(node.target, defined)
            bind_target(node.target, local_bound)
            return
        if isinstance(node, ast.Lambda):
            for d in node.args.defaults:
                check_expr(d, local_bound)
            for d in node.args.kw_defaults:
                if d is not None:
                    check_expr(d, local_bound)
            return
        if isinstance(node, ast.Call):
            check_expr(node.func, local_bound)
            is_eager = (
                isinstance(node.func, ast.Name) and node.func.id in EAGER_GENEXP_CALLS
            ) or (
                isinstance(node.func, ast.Attribute)
                and node.func.attr in EAGER_GENEXP_METHODS
            )
            for arg in node.args:
                check_expr(arg, local_bound, eager_genexp=is_eager)
            for kw in node.keywords:
                check_expr(kw.value, local_bound)
            if (
                isinstance(node.func, ast.Name)
                and node.func.id in func_globals
                and node.func.id not in local_bound
            ):
                fn_id = node.func.id
                for gname in sorted(func_globals[fn_id]):
                    key = ("call", fn_id, gname)
                    if gname not in defined and key not in reported:
                        reported.add(key)
                        lineno = base_line + max(getattr(node, "lineno", 1), 1) - 1
                        findings.append({
                            "source": "machinery_script_lint",
                            "category": "machinery_python_use_before_def",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": (
                                f"Module-level call to `{fn_id}()` in '{rel_path}' "
                                f"reads `{gname}` before `{gname}` is assigned, "
                                "raising `NameError` on startup."
                            ),
                            "remediation": (
                                f"Define `{gname}` before calling `{fn_id}()` around "
                                f"line {lineno} in '{rel_path}'."
                            ),
                        })
            return
        if isinstance(
            node, (ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)
        ):
            comp_bound = set(local_bound)
            for idx, gen in enumerate(node.generators):
                check_expr(gen.iter, comp_bound)
                if isinstance(node, ast.GeneratorExp) and not eager_genexp and idx == 0:
                    return
                bind_target(gen.target, comp_bound)
                for cond in gen.ifs:
                    check_expr(cond, comp_bound)
            if isinstance(node, ast.DictComp):
                check_expr(node.key, comp_bound)
                check_expr(node.value, comp_bound)
            else:
                check_expr(node.elt, comp_bound)
            return
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            if (
                node.id not in defined
                and node.id not in local_bound
                and node.id not in reported
            ):
                reported.add(node.id)
                lineno = base_line + max(getattr(node, "lineno", 1), 1) - 1
                findings.append({
                    "source": "machinery_script_lint",
                    "category": "machinery_python_use_before_def",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Module-level variable `{node.id}` is referenced before "
                        f"assignment in '{rel_path}', raising `NameError` on startup."
                    ),
                    "remediation": (
                        f"Move the definition of `{node.id}` above line {lineno} in "
                        f"'{rel_path}' so it is assigned before module-level use."
                    ),
                })
            return
        for child in ast.iter_child_nodes(node):
            check_expr(child, local_bound)

    def handle_assign_target(target):
        if isinstance(target, ast.Name):
            defined.add(target.id)
        elif isinstance(target, (ast.Tuple, ast.List)):
            for elt in target.elts:
                handle_assign_target(elt)
        elif isinstance(target, ast.Starred):
            handle_assign_target(target.value)
        elif isinstance(target, ast.Attribute):
            check_expr(target.value, set())
        elif isinstance(target, ast.Subscript):
            check_expr(target.value, set())
            check_expr(target.slice, set())

    def walk_stmts(stmts):
        for stmt in stmts:
            if isinstance(stmt, ast.Import):
                for alias in stmt.names:
                    defined.add((alias.asname or alias.name).split(".")[0])
            elif isinstance(stmt, ast.ImportFrom):
                if any(a.name == "*" for a in stmt.names):
                    return
                for alias in stmt.names:
                    defined.add(alias.asname or alias.name)
            elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                for dec in stmt.decorator_list:
                    check_expr(dec, set())
                for d in stmt.args.defaults:
                    check_expr(d, set())
                for d in stmt.args.kw_defaults:
                    if d is not None:
                        check_expr(d, set())
                if not future_annotations:
                    for arg in (
                        stmt.args.posonlyargs
                        + stmt.args.args
                        + stmt.args.kwonlyargs
                    ):
                        check_expr(arg.annotation, set())
                    if stmt.args.vararg:
                        check_expr(stmt.args.vararg.annotation, set())
                    if stmt.args.kwarg:
                        check_expr(stmt.args.kwarg.annotation, set())
                    check_expr(stmt.returns, set())
                defined.add(stmt.name)
            elif isinstance(stmt, ast.ClassDef):
                for dec in stmt.decorator_list:
                    check_expr(dec, set())
                for base in stmt.bases:
                    check_expr(base, set())
                for kw in stmt.keywords:
                    check_expr(kw.value, set())
                defined.add(stmt.name)
            elif isinstance(stmt, ast.Assign):
                check_expr(stmt.value, set())
                for t in stmt.targets:
                    handle_assign_target(t)
            elif isinstance(stmt, ast.AnnAssign):
                if not future_annotations:
                    check_expr(stmt.annotation, set())
                if stmt.value is not None:
                    check_expr(stmt.value, set())
                    handle_assign_target(stmt.target)
            elif isinstance(stmt, ast.AugAssign):
                check_expr(stmt.target, set())
                check_expr(stmt.value, set())
                handle_assign_target(stmt.target)
            elif isinstance(stmt, (ast.For, ast.AsyncFor)):
                check_expr(stmt.iter, set(), eager_genexp=True)
                handle_assign_target(stmt.target)
                walk_stmts(stmt.body)
                walk_stmts(stmt.orelse)
            elif isinstance(stmt, (ast.While, ast.If)):
                check_expr(stmt.test, set())
                walk_stmts(stmt.body)
                walk_stmts(stmt.orelse)
            elif isinstance(stmt, (ast.With, ast.AsyncWith)):
                for item in stmt.items:
                    check_expr(item.context_expr, set())
                    if item.optional_vars is not None:
                        handle_assign_target(item.optional_vars)
                walk_stmts(stmt.body)
            elif isinstance(stmt, ast.Try) or (
                hasattr(ast, "TryStar") and isinstance(stmt, ast.TryStar)
            ):
                walk_stmts(stmt.body)
                for handler in stmt.handlers:
                    check_expr(handler.type, set())
                    if handler.name:
                        defined.add(handler.name)
                    walk_stmts(handler.body)
                walk_stmts(stmt.orelse)
                walk_stmts(stmt.finalbody)
            elif hasattr(ast, "Match") and isinstance(stmt, ast.Match):
                check_expr(stmt.subject, set())
                for case in stmt.cases:
                    bind_pattern(case.pattern, defined)
                    check_expr(case.guard, set())
                    walk_stmts(case.body)
            else:
                for child in ast.iter_child_nodes(stmt):
                    check_expr(child, set())

    walk_stmts(tree.body)


# Check function scopes for local variables referenced before any assignment path
# (`UnboundLocalError` at runtime, which `symtable` treats as assigned in scope).
def check_function_local_order(code, tree, rel_path, base_line, findings):
    try:
        root_tbl = symtable.symtable(code, rel_path, "exec")
    except SyntaxError:
        return
    future_annotations = any(
        isinstance(s, ast.ImportFrom)
        and s.module == "__future__"
        and any(a.name == "annotations" for a in s.names)
        for s in tree.body
    )
    reported = set()

    def bind_target(target, bound_set):
        if isinstance(target, ast.Name):
            bound_set.add(target.id)
        elif isinstance(target, (ast.Tuple, ast.List)):
            for elt in target.elts:
                bind_target(elt, bound_set)
        elif isinstance(target, ast.Starred):
            bind_target(target.value, bound_set)

    def bind_pattern(pat, bound_set):
        for sub in ast.walk(pat):
            if hasattr(ast, "MatchAs") and isinstance(sub, ast.MatchAs) and sub.name:
                bound_set.add(sub.name)
            elif hasattr(ast, "MatchStar") and isinstance(sub, ast.MatchStar) and sub.name:
                bound_set.add(sub.name)
            elif hasattr(ast, "MatchMapping") and isinstance(sub, ast.MatchMapping) and sub.rest:
                bound_set.add(sub.rest)

    def has_local_error_guard(fn_node):
        found = False

        def scan(sub):
            nonlocal found
            if found:
                return
            if sub is not fn_node and isinstance(
                sub, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                return
            if isinstance(sub, ast.ExceptHandler):
                if sub.type is None:
                    found = True
                    return
                for tnode in ast.walk(sub.type):
                    if isinstance(tnode, ast.Name) and tnode.id in (
                        "UnboundLocalError",
                        "NameError",
                        "Exception",
                        "BaseException",
                    ):
                        found = True
                        return
            for child in ast.iter_child_nodes(sub):
                scan(child)

        scan(fn_node)
        return found

    def collect_scope_decls(fn_node):
        decls = set()

        def scan(sub):
            if sub is not fn_node and isinstance(
                sub, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                return
            if isinstance(sub, (ast.Global, ast.Nonlocal)):
                decls.update(sub.names)
            for child in ast.iter_child_nodes(sub):
                scan(child)

        scan(fn_node)
        return decls

    def audit_fn(fn_node, fn_tbl):
        if has_local_error_guard(fn_node):
            return
        local_vars = {
            sym.get_name()
            for sym in fn_tbl.get_symbols()
            if sym.is_local()
            and sym.is_assigned()
            and not sym.is_global()
            and not sym.is_free()
        } - collect_scope_decls(fn_node)
        if not local_vars:
            return
        may_assigned = {
            arg.arg
            for arg in (
                fn_node.args.posonlyargs
                + fn_node.args.args
                + fn_node.args.kwonlyargs
            )
        }
        if fn_node.args.vararg:
            may_assigned.add(fn_node.args.vararg.arg)
        if fn_node.args.kwarg:
            may_assigned.add(fn_node.args.kwarg.arg)

        def check_expr(node, local_bound, eager_genexp=False):
            if node is None:
                return
            if isinstance(node, ast.NamedExpr):
                check_expr(node.value, local_bound)
                bind_target(node.target, may_assigned)
                bind_target(node.target, local_bound)
                return
            if isinstance(node, ast.Lambda):
                for d in node.args.defaults:
                    check_expr(d, local_bound)
                for d in node.args.kw_defaults:
                    if d is not None:
                        check_expr(d, local_bound)
                return
            if isinstance(node, ast.Call):
                check_expr(node.func, local_bound)
                is_eager = (
                    isinstance(node.func, ast.Name)
                    and node.func.id in EAGER_GENEXP_CALLS
                ) or (
                    isinstance(node.func, ast.Attribute)
                    and node.func.attr in EAGER_GENEXP_METHODS
                )
                for arg in node.args:
                    check_expr(arg, local_bound, eager_genexp=is_eager)
                for kw in node.keywords:
                    check_expr(kw.value, local_bound)
                return
            if isinstance(
                node, (ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)
            ):
                comp_bound = set(local_bound)
                for idx, gen in enumerate(node.generators):
                    check_expr(gen.iter, comp_bound)
                    if (
                        isinstance(node, ast.GeneratorExp)
                        and not eager_genexp
                        and idx == 0
                    ):
                        return
                    bind_target(gen.target, comp_bound)
                    for cond in gen.ifs:
                        check_expr(cond, comp_bound)
                if isinstance(node, ast.DictComp):
                    check_expr(node.key, comp_bound)
                    check_expr(node.value, comp_bound)
                else:
                    check_expr(node.elt, comp_bound)
                return
            if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
                key = (fn_node.name, fn_node.lineno, node.id)
                if (
                    node.id in local_vars
                    and node.id not in may_assigned
                    and node.id not in local_bound
                    and key not in reported
                ):
                    reported.add(key)
                    lineno = base_line + max(getattr(node, "lineno", 1), 1) - 1
                    findings.append({
                        "source": "machinery_script_lint",
                        "category": "machinery_python_use_before_def",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Local variable `{node.id}` in `{fn_node.name}()` of "
                            f"'{rel_path}' is referenced before assignment, raising "
                            "`UnboundLocalError` at runtime."
                        ),
                        "remediation": (
                            f"Assign or initialize `{node.id}` before referencing "
                            f"it around line {lineno} in `{fn_node.name}()` of "
                            f"'{rel_path}'."
                        ),
                    })
                return
            for child in ast.iter_child_nodes(node):
                check_expr(child, local_bound)

        def handle_target(target):
            if isinstance(target, ast.Name):
                may_assigned.add(target.id)
            elif isinstance(target, (ast.Tuple, ast.List)):
                for elt in target.elts:
                    handle_target(elt)
            elif isinstance(target, ast.Starred):
                handle_target(target.value)
            elif isinstance(target, ast.Attribute):
                check_expr(target.value, set())
            elif isinstance(target, ast.Subscript):
                check_expr(target.value, set())
                check_expr(target.slice, set())

        def walk_fn_stmts(stmts):
            for stmt in stmts:
                if isinstance(stmt, ast.Import):
                    for alias in stmt.names:
                        may_assigned.add((alias.asname or alias.name).split(".")[0])
                elif isinstance(stmt, ast.ImportFrom):
                    for alias in stmt.names:
                        if alias.name != "*":
                            may_assigned.add(alias.asname or alias.name)
                elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    for dec in stmt.decorator_list:
                        check_expr(dec, set())
                    for d in stmt.args.defaults:
                        check_expr(d, set())
                    for d in stmt.args.kw_defaults:
                        if d is not None:
                            check_expr(d, set())
                    if not future_annotations:
                        for arg in (
                            stmt.args.posonlyargs
                            + stmt.args.args
                            + stmt.args.kwonlyargs
                        ):
                            check_expr(arg.annotation, set())
                        if stmt.args.vararg:
                            check_expr(stmt.args.vararg.annotation, set())
                        if stmt.args.kwarg:
                            check_expr(stmt.args.kwarg.annotation, set())
                        check_expr(stmt.returns, set())
                    may_assigned.add(stmt.name)
                elif isinstance(stmt, ast.ClassDef):
                    for dec in stmt.decorator_list:
                        check_expr(dec, set())
                    for base in stmt.bases:
                        check_expr(base, set())
                    for kw in stmt.keywords:
                        check_expr(kw.value, set())
                    may_assigned.add(stmt.name)
                elif isinstance(stmt, ast.Assign):
                    check_expr(stmt.value, set())
                    for t in stmt.targets:
                        handle_target(t)
                elif isinstance(stmt, ast.AnnAssign):
                    if not future_annotations:
                        check_expr(stmt.annotation, set())
                    if stmt.value is not None:
                        check_expr(stmt.value, set())
                        handle_target(stmt.target)
                elif isinstance(stmt, ast.AugAssign):
                    check_expr(stmt.target, set())
                    check_expr(stmt.value, set())
                    handle_target(stmt.target)
                elif isinstance(stmt, (ast.For, ast.AsyncFor)):
                    check_expr(stmt.iter, set(), eager_genexp=True)
                    handle_target(stmt.target)
                    walk_fn_stmts(stmt.body)
                    walk_fn_stmts(stmt.orelse)
                elif isinstance(stmt, (ast.While, ast.If)):
                    check_expr(stmt.test, set())
                    walk_fn_stmts(stmt.body)
                    walk_fn_stmts(stmt.orelse)
                elif isinstance(stmt, (ast.With, ast.AsyncWith)):
                    for item in stmt.items:
                        check_expr(item.context_expr, set())
                        if item.optional_vars is not None:
                            handle_target(item.optional_vars)
                    walk_fn_stmts(stmt.body)
                elif isinstance(stmt, ast.Try) or (
                    hasattr(ast, "TryStar") and isinstance(stmt, ast.TryStar)
                ):
                    walk_fn_stmts(stmt.body)
                    for handler in stmt.handlers:
                        check_expr(handler.type, set())
                        if handler.name:
                            may_assigned.add(handler.name)
                        walk_fn_stmts(handler.body)
                    walk_fn_stmts(stmt.orelse)
                    walk_fn_stmts(stmt.finalbody)
                elif hasattr(ast, "Match") and isinstance(stmt, ast.Match):
                    check_expr(stmt.subject, set())
                    for case in stmt.cases:
                        bind_pattern(case.pattern, may_assigned)
                        check_expr(case.guard, set())
                        walk_fn_stmts(case.body)
                else:
                    for child in ast.iter_child_nodes(stmt):
                        check_expr(child, set())

        walk_fn_stmts(fn_node.body)

    def walk_scopes(scope_ast, scope_tbl):
        child_tbls = {
            (c.get_type(), c.get_name(), c.get_lineno()): c
            for c in scope_tbl.get_children()
            if c.get_type() in ("function", "class")
        }

        def descend(node):
            for child in ast.iter_child_nodes(node):
                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    ctbl = child_tbls.get(("function", child.name, child.lineno))
                    if ctbl is not None:
                        audit_fn(child, ctbl)
                        walk_scopes(child, ctbl)
                elif isinstance(child, ast.ClassDef):
                    ctbl = child_tbls.get(("class", child.name, child.lineno))
                    if ctbl is not None:
                        walk_scopes(child, ctbl)
                else:
                    descend(child)

        descend(scope_ast)

    walk_scopes(tree, root_tbl)


# Check `checks/*.sh` inline Python heredocs for top-level `check_*` functions that
# are never invoked anywhere outside their own definition.
def check_uncalled_check_functions(tree, full_path, rel_path, base_line, findings):
    if (
        not full_path.endswith(".sh")
        or os.path.basename(os.path.dirname(full_path)) != "checks"
    ):
        return
    check_defs = [
        stmt
        for stmt in tree.body
        if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef))
        and stmt.name.startswith("check_")
    ]
    if not check_defs:
        return
    for stmt in check_defs:
        own_ids = {id(sub) for sub in ast.walk(stmt)}
        called_outside = any(
            isinstance(node, ast.Name)
            and isinstance(node.ctx, ast.Load)
            and node.id == stmt.name
            and id(node) not in own_ids
            for node in ast.walk(tree)
        )
        if called_outside:
            continue
        lineno = base_line + max(getattr(stmt, "lineno", 1), 1) - 1
        findings.append({
            "source": "machinery_script_lint",
            "category": "machinery_uncalled_check_function",
            "severity": "error",
            "file": rel_path,
            "line": lineno,
            "message": (
                f"Top-level check function `{stmt.name}` in '{rel_path}' is "
                "never called, leaving its check logic dead at runtime."
            ),
            "remediation": (
                f"Invoke `{stmt.name}(...)` in the module-level check driver "
                f"below its definition in '{rel_path}', or remove `{stmt.name}` "
                "if obsolete."
            ),
        })


# Check module, function, and class bodies for duplicate function or class
# definitions (such as 3-way merge conflicts leaving two helper definitions).
def check_duplicate_python_defs(tree, rel_path, base_line, findings):
    # Ignore typing.overload stubs and key property setter/deleter/getter methods
    # separately so legitimate overloads and descriptors do not collide.
    def def_entry_key(stmt):
        if isinstance(stmt, ast.ClassDef):
            return ("def", stmt.name)
        for dec in stmt.decorator_list:
            target = dec.func if isinstance(dec, ast.Call) else dec
            if isinstance(target, ast.Name) and target.id == "overload":
                return None
            if isinstance(target, ast.Attribute):
                if target.attr == "overload":
                    return None
                if target.attr in ("setter", "deleter", "getter"):
                    return (f"accessor:{target.attr}", stmt.name)
        return ("def", stmt.name)

    def check_stmts(stmts):
        seen = {}
        for stmt in stmts:
            if not isinstance(
                stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                continue
            key = def_entry_key(stmt)
            py_ln = max(getattr(stmt, "lineno", 1), 1)
            lineno = base_line + py_ln - 1
            if key is not None:
                if key in seen:
                    prev_ln = seen[key]
                    kind = (
                        "class" if isinstance(stmt, ast.ClassDef) else "function"
                    )
                    findings.append({
                        "source": "machinery_script_lint",
                        "category": "machinery_duplicate_python_def",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Duplicate {kind} definition `{stmt.name}` in "
                            f"'{rel_path}' at line {lineno} (already defined "
                            f"at line {prev_ln}), silently overwriting the "
                            "earlier definition."
                        ),
                        "remediation": (
                            f"Remove or merge the duplicate `{stmt.name}` "
                            f"definition at lines {prev_ln} and {lineno} in "
                            f"'{rel_path}'."
                        ),
                    })
                else:
                    seen[key] = lineno
            check_stmts(stmt.body)

    check_stmts(tree.body)


# Check Python dict literals for duplicate constant keys (such as 3-way merge
# conflicts leaving two `"remediation":` or `"message":` entries in a finding dict).
def check_duplicate_dict_keys(tree, rel_path, base_line, findings):
    for node in ast.walk(tree):
        if not isinstance(node, ast.Dict):
            continue
        seen = {}
        for k in node.keys:
            if not isinstance(k, ast.Constant):
                continue
            val_key = (type(k.value), k.value)
            py_ln = max(getattr(k, "lineno", 1), 1)
            lineno = base_line + py_ln - 1
            if val_key in seen:
                prev_ln = seen[val_key]
                findings.append({
                    "source": "machinery_script_lint",
                    "category": "machinery_duplicate_dict_key",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Duplicate dictionary key `{k.value!r}` in '{rel_path}' "
                        f"at line {lineno} (already defined at line {prev_ln}), "
                        "silently overwriting the earlier entry."
                    ),
                    "remediation": (
                        f"Remove or merge the duplicate `{k.value!r}` key at "
                        f"lines {prev_ln} and {lineno} in '{rel_path}'."
                    ),
                })
            else:
                seen[val_key] = lineno


# Audit a single machinery script or JSON manifest.
def audit_file(full_path, rel_path, findings):
    if full_path.endswith(".json"):
        check_json_syntax(full_path, rel_path, findings)
        return
    if full_path.endswith(".sh"):
        check_shell_syntax(full_path, rel_path, findings)
    try:
        with open(full_path, "r", encoding="utf-8", errors="replace") as f:
            text = f.read()
    except OSError:
        return
    local_exports = module_exports_for_file(full_path)
    for base_line, py_code in extract_python_blocks(full_path, text):
        try:
            tree = ast.parse(py_code, filename=rel_path)
        except SyntaxError as exc:
            lineno = base_line + max(exc.lineno or 1, 1) - 1
            findings.append({
                "source": "machinery_script_lint",
                "category": "machinery_python_syntax_error",
                "severity": "error",
                "file": rel_path,
                "line": lineno,
                "message": f"Python syntax error in '{rel_path}': {exc.msg}",
                "remediation": (
                    f"Fix the Python syntax error around line {lineno} in '{rel_path}'."
                ),
            })
            continue
        check_lexical_undefined_names(py_code, tree, rel_path, base_line, findings)
        check_local_module_refs(tree, rel_path, base_line, local_exports, findings)
        check_module_order(py_code, tree, rel_path, base_line, findings)
        check_function_local_order(py_code, tree, rel_path, base_line, findings)
        check_uncalled_check_functions(tree, full_path, rel_path, base_line, findings)
        check_duplicate_python_defs(tree, rel_path, base_line, findings)
        check_duplicate_dict_keys(tree, rel_path, base_line, findings)


findings = []
for full_f, rel_f in collect_target_files():
    audit_file(full_f, rel_f, findings)

print(json.dumps(findings, indent=2))
PYEOF
