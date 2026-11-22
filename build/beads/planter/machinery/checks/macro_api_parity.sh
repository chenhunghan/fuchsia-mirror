#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Enforces //docs/development/languages/bazel/style/rules_macros.md and
# GN/Bazel macro API parity on added or modified `.bzl` and `.gni` files:
#   - macro_redeclares_inherited_attr (error): symbolic `macro(...)` declares
#     `applicable_licenses` or silences `attr-applicable_licenses`.
#   - macro_configurable_attr_misuse (error/info): configurable symbolic macro
#     attributes or `select()` values are evaluated as booleans, compared to
#     non-`None` values, placed in container literals, iterated/indexed/formatted,
#     or passed to non-configurable target attributes (mirrors upstream
#     `bazel_configurable_attributes` when not yet registered in `bazel_lint`).
#   - macro_non_none_default_none_compare (error): configurable symbolic macro
#     attribute with a non-`None` default (`attr.bool`, `attr.string`,
#     `attr.*_list`, `attr.*_dict`, `attr.int`, or `attr.label` with a non-`None`
#     default or `mandatory = True`) is compared against `None` outside an
#     empty-identity coalescing ternary (`x if x != None else []/{}/""`), where
#     Bazel wraps the resolved default in `select()` so `== None` is always
#     `False` and `!= None` is always `True`.
#   - macro_impl_default_arg (error): symbolic `macro()` `implementation`
#     function specifies default parameter values instead of defining defaults
#     exclusively in `macro(attrs = {...})`.
#   - rule_or_macro_impl_name (error): newly added `rule()` or `macro()` uses an
#     `implementation` function not named `_<name>_impl`.
#   - missing_bzl_documentation (error): newly added `rule()`, `macro()`,
#     non-private `attr.*()`, or public `.bzl` function omits documentation.
#   - bzl_docstring_concat (error): `doc = ...` appends strings with `+` instead
#     of using a single-line string or triple-quoted multiline string.
#   - bzl_public_func_positional_defaults (error): newly added public `.bzl`
#     function or legacy macro declares defaulted arguments before `*`.
#   - legacy_macro_missing_target_visibility (error): newly added legacy `def`
#     macro omits explicit `visibility` on inner helper targets (or fails to
#     forward `visibility` to the main target).
#   - macro_kwargs_attr_extraction (error): macro extracts attributes via
#     `kwargs.get(...)` / `kwargs[...]` instead of declaring explicit parameters.
#   - macro_inline_target_name_duplication (error): macro constructs a derived
#     target name inline in both `name = ...` and another argument instead of
#     using a local variable.
#   - bzl_fuchsia_prefix_outside_sdk (error): newly added platform wrapper rule,
#     macro, or public function uses `fuchsia_` instead of `fx_` outside SDK/driver
#     directories.
#   - bzl_antipattern (error): `.bzl` code references `bazel-bin` or upstream
#     `_solib_` paths, passes a bare string `executable` to `ctx.actions.run`,
#     or defines a custom `transition()` outside `//build/bazel/`.
#   - gn_template_crate_name_not_forwarded (error): `.gni` template reads
#     `invoker.crate_name` but sets Rust `crate_name` to a decorated target name.
#   - starlark_call_unexpected_kwarg / starlark_call_missing_kwarg (error): a
#     changed `.bzl` or `BUILD.bazel` file calls a loaded Starlark `def`
#     function (without `**kwargs`) with an undeclared keyword argument or
#     omits a required keyword-only argument.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$WORKDIR" "$CHANGE_BASE" "$SCRIPT_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_change_base = sys.argv[2].strip() or "HEAD"
script_dir = os.path.abspath(sys.argv[3]) if len(sys.argv) > 3 else ""
if script_dir and os.path.isdir(script_dir) and script_dir not in sys.path:
    sys.path.insert(0, script_dir)
try:
    import planter_check_lib as pcl

    change_base = pcl.resolve_change_base(workdir, raw_change_base)
except Exception:
    change_base = raw_change_base
findings = []

STYLE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/rules_macros"
CONFIG_ATTR_URL = STYLE_URL + "#configurable-attributes"

HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")

UNIVERSAL_NONCONFIGURABLE_TARGET_ATTRS = frozenset(
    (
        "applicable_licenses compatible_with deprecation distribs "
        "exec_compatible_with exec_group_compatible_with generator_function "
        "generator_location generator_name licenses name package_metadata "
        "restricted_to tags testonly visibility"
    ).split()
)

INHERITED_NONCONFIGURABLE_ATTRS = (
    UNIVERSAL_NONCONFIGURABLE_TARGET_ATTRS
    | frozenset(("flaky", "local", "size", "timeout"))
)

SEQUENCE_BUILTINS = frozenset(
    "all any enumerate len list max min reversed set sorted tuple zip".split()
)

CONCRETE_VALUE_BUILTINS = frozenset(
    "Label chr float hash int ord package_relative_label range repr str".split()
)

NON_TARGET_CALLABLES = frozenset(
    "aspect fail macro module_extension print provider repository_rule rule struct visibility".split()
)

NON_NONE_DEFAULT_ATTR_METHODS = frozenset(
    (
        "bool int int_list label_keyed_string_dict label_list "
        "string string_dict string_keyed_label_dict string_list string_list_dict"
    ).split()
)

LIST_ATTR_METHODS = frozenset(("int_list", "label_list", "string_list"))
DICT_ATTR_METHODS = frozenset(
    (
        "label_keyed_string_dict",
        "string_dict",
        "string_keyed_label_dict",
        "string_list_dict",
    )
)

MESSAGE_BODIES = {
    "bool": (
        "cannot be evaluated as a boolean (`select()` is always truthy in "
        "Starlark)."
    ),
    "compare": (
        "cannot be compared to non-`None` values (`select() == value` always "
        "evaluates to `False` in Starlark)."
    ),
    "list": (
        "cannot be placed inside a list or tuple literal. Concatenate a "
        "single-element list with `+` if the destination attribute accepts a list."
    ),
    "dict": (
        "cannot be used as a dict key or value (except as a value in a "
        "`**kwargs` dict forwarded to a rule or macro)."
    ),
    "nested_select": "cannot be nested inside another `select()`.",
    "iter": (
        "cannot be iterated over, unpacked, or passed to sequence functions "
        "(`len()`, `list()`, etc.)."
    ),
    "lookup": "cannot be indexed, sliced, or used as a dict lookup key.",
    "membership": "cannot be used with `in` or `not in`.",
    "format": (
        "cannot be used in string formatting (`%` or `.format()`). Concatenate "
        "strings with `+` if the destination attribute is configurable."
    ),
    "attr_access": "does not support attribute or method access.",
    "conversion": (
        "cannot be passed to functions or operators that require concrete "
        "values (such as `str()`, `Label()`, `paths.*()`, or arithmetic "
        "operators other than `+` and `|`)."
    ),
    "dest": "cannot be passed to non-configurable attribute `{dest_attr}`.",
}

ONLY_NONE = "Only `== None` and `!= None` are supported"
ATTR_REMEDIES = {
    "bool": (
        "Use `== None` or `!= None` if `{origin}` defaults to `None` "
        "(such as `attr.label()` or an inherited attribute), or declare "
        "`{origin}` with `configurable = False`."
    ),
    "compare": f"{ONLY_NONE}, or declare `{{origin}}` with `configurable = False`.",
}
DEFAULT_ATTR_REMEDY = (
    "Declare `{origin}` with `configurable = False` if it must be concrete at "
    "macro expansion time."
)
SELECT_REMEDIES = {"compare": f"{ONLY_NONE}."}


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


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


def emit(category, severity, path, line, message, remediation):
    findings.append(
        {
            "source": "macro_api_parity",
            "category": category,
            "severity": severity,
            "file": path,
            "line": max(int(line or 1), 1),
            "message": message,
            "remediation": remediation,
        }
    )


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def load_ignored_prefixes():
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


IGNORED_PREFIXES = load_ignored_prefixes()


def is_checked_path(path):
    if path.startswith(IGNORED_PREFIXES):
        return False
    if path.startswith("third_party/") and "/src/" in path:
        return False
    full = os.path.join(workdir, path)
    return os.path.isfile(full) and not os.path.islink(full)


def changed_files():
    env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
    if env_changed:
        files = {l.strip() for l in env_changed.splitlines() if l.strip()}
    else:
        files = set(
            git(["diff", "--name-only", "--diff-filter=AMR", change_base]).split()
        )
        files |= set(git(["ls-files", "--others", "--exclude-standard"]).split())
    return sorted(f for f in files if is_checked_path(f))


def get_diff_info(path, line_count):
    if not git_ok(["cat-file", "-e", f"{change_base}:{path}"]):
        return True, "", set(range(1, line_count + 1))
    base_text = git(["show", f"{change_base}:{path}"])
    diff_out = git(["diff", "-U0", change_base, "--", path])
    changed = set()
    for raw in diff_out.splitlines():
        m = HUNK_RE.match(raw)
        if not m:
            continue
        start = int(m.group(1))
        count = 1 if m.group(2) is None else int(m.group(2))
        if count > 0:
            changed.update(range(start, start + count))
    return False, base_text, changed


def touches_changed(node, changed_lines):
    lineno = getattr(node, "lineno", 0)
    if not lineno:
        return False
    end_lineno = getattr(node, "end_lineno", None) or lineno
    return any(ln in changed_lines for ln in range(lineno, end_lineno + 1))


def sig_touches_changed(fn, changed_lines):
    sig_end = (fn.body[0].lineno - 1) if fn.body else (fn.end_lineno or fn.lineno)
    sig_end = max(sig_end, fn.lineno)
    return any(ln in changed_lines for ln in range(fn.lineno, sig_end + 1))


def is_none(expr):
    return isinstance(expr, ast.Constant) and expr.value is None


def call_name(func):
    if isinstance(func, ast.Name):
        return func.id
    if (
        isinstance(func, ast.Attribute)
        and isinstance(func.value, ast.Name)
        and func.value.id == "native"
    ):
        return func.attr
    return None


def target_names(target):
    if isinstance(target, ast.Name):
        return [target.id]
    if isinstance(target, (ast.Tuple, ast.List)):
        out = []
        for elt in target.elts:
            out.extend(target_names(elt))
        return out
    return []


def collect_docstrings(tree):
    docs = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef)):
            if (
                node.body
                and isinstance(node.body[0], ast.Expr)
                and isinstance(node.body[0].value, ast.Constant)
                and isinstance(node.body[0].value.value, str)
            ):
                docs.add(node.body[0].value)
    return docs


def extract_attrs_dict(expr, module_vars, defs_by_name, depth=0):
    if expr is None or depth > 6:
        return {}
    if isinstance(expr, ast.Dict):
        res = {}
        for k, v in zip(expr.keys, expr.values):
            if k is None:
                res |= extract_attrs_dict(v, module_vars, defs_by_name, depth + 1)
            elif isinstance(k, ast.Constant) and isinstance(k.value, str):
                res[k.value] = v
        return res
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.BitOr):
        return extract_attrs_dict(
            expr.left, module_vars, defs_by_name, depth + 1
        ) | extract_attrs_dict(expr.right, module_vars, defs_by_name, depth + 1)
    if isinstance(expr, ast.IfExp):
        return extract_attrs_dict(
            expr.body, module_vars, defs_by_name, depth + 1
        ) | extract_attrs_dict(expr.orelse, module_vars, defs_by_name, depth + 1)
    if isinstance(expr, ast.Name) and expr.id in module_vars:
        return extract_attrs_dict(
            module_vars[expr.id], module_vars, defs_by_name, depth + 1
        )
    if (
        isinstance(expr, ast.Call)
        and isinstance(expr.func, ast.Name)
        and expr.func.id in defs_by_name
    ):
        res = {}
        for node in ast.walk(defs_by_name[expr.func.id]):
            if isinstance(node, ast.Return) and node.value is not None:
                res |= extract_attrs_dict(
                    node.value, module_vars, defs_by_name, depth + 1
                )
        return res
    return {}


def resolve_attr_call(expr, module_vars):
    if isinstance(expr, ast.Name) and expr.id in module_vars:
        expr = module_vars[expr.id]
    if (
        isinstance(expr, ast.Call)
        and isinstance(expr.func, ast.Attribute)
        and isinstance(expr.func.value, ast.Name)
        and expr.func.value.id == "attr"
    ):
        return expr
    return None


def is_attr_configurable(expr, module_vars):
    call = resolve_attr_call(expr, module_vars)
    if call is None or call.func.attr in ("output", "output_list"):
        return False
    return not any(
        kw.arg == "configurable"
        and isinstance(kw.value, ast.Constant)
        and kw.value.value is False
        for kw in call.keywords
    )


def classify_non_none_default_attr(expr, module_vars):
    """Returns (is_non_none_default, empty_identity_kind) for a configurable attr."""
    if not is_attr_configurable(expr, module_vars):
        return False, None
    call = resolve_attr_call(expr, module_vars)
    if call is None:
        return False, None
    method = call.func.attr
    kw_map = {kw.arg: kw.value for kw in call.keywords if kw.arg}
    if method in NON_NONE_DEFAULT_ATTR_METHODS:
        if method in LIST_ATTR_METHODS:
            return True, "list"
        if method in DICT_ATTR_METHODS:
            return True, "dict"
        if method == "string":
            def_val = kw_map.get("default")
            if def_val is None or (
                isinstance(def_val, ast.Constant) and def_val.value == ""
            ):
                return True, "empty_str"
        return True, None
    if method == "label":
        mand = kw_map.get("mandatory")
        if isinstance(mand, ast.Constant) and mand.value is True:
            return True, None
        if "default" in kw_map and not is_none(kw_map["default"]):
            return True, None
    return False, None


class MacroInfo:
    def __init__(
        self,
        name,
        call_node,
        impl_name,
        attrs,
        non_none_defaults,
        inherits_open_schema,
    ):
        self.name = name
        self.call_node = call_node
        self.impl_name = impl_name
        self.attrs = attrs
        # attr_name -> empty_identity_kind ("list", "dict", "empty_str", or None)
        self.non_none_defaults = non_none_defaults
        self.inherits_open_schema = inherits_open_schema

    def nonconfigurable(self):
        return frozenset(k for k, v in self.attrs.items() if not v)


def top_dicts(expr, var_dicts, depth=0):
    if depth > 6:
        return []
    if isinstance(expr, ast.Dict):
        return [expr]
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.BitOr):
        return top_dicts(expr.left, var_dicts, depth + 1) + top_dicts(
            expr.right, var_dicts, depth + 1
        )
    if isinstance(expr, ast.IfExp):
        return top_dicts(expr.body, var_dicts, depth + 1) + top_dicts(
            expr.orelse, var_dicts, depth + 1
        )
    if isinstance(expr, ast.Name):
        return var_dicts.get(expr.id, [])
    return []


def format_configurable_msg(kind, origin, dest_attr=None):
    body = MESSAGE_BODIES[kind].format(dest_attr=dest_attr)
    if not origin:
        remedy = SELECT_REMEDIES.get(kind)
        return (
            f"A `select()` value {body}"
            + (f" {remedy}" if remedy else "")
            + f" See {CONFIG_ATTR_URL}."
        )
    remedy = ATTR_REMEDIES.get(kind, DEFAULT_ATTR_REMEDY).format(origin=origin)
    return (
        f"Configurable attribute `{origin}` may contain a `select()` and "
        f"{body} {remedy} See {CONFIG_ATTR_URL}."
    )


def configurable_remediation(kind, origin, dest_attr=None):
    target_desc = f"`{origin}`" if origin else "the `select()` expression"
    if kind == "bool":
        return (
            f"Do not test truthiness on {target_desc} (`if attr:`, `not attr`, `attr or []`, `bool(attr)`).\n"
            "If the attribute defaults to `None` (`attr.label()` without a non-`None` default or an\n"
            "inherited attribute via `inherit_attrs`), compare with `!= None` or `== None` (for example,\n"
            "`(deps if deps != None else []) + [...]`). If it has a non-`None` default (`attr.bool()`,\n"
            "`attr.string()`, `attr.*_list()`), forward it directly with `+`/`|` or set\n"
            "`configurable = False` in `macro(attrs = {...})`."
        )
    if kind == "dest":
        return (
            f"Do not pass {target_desc} to non-configurable attribute `{dest_attr}`.\n"
            "Declare the macro attribute with `configurable = False` in `macro(attrs = {...})`."
        )
    return (
        f"Keep {target_desc} `select()`-compatible (use `+` to concatenate lists/strings and `|` to\n"
        "merge dicts), or declare the attribute with `configurable = False` in `macro(attrs = {...})`\n"
        "per //docs/development/languages/bazel/style/rules_macros.md#configurable-attributes."
    )


class ConfigurableAttrChecker:
    def __init__(
        self,
        path,
        parents,
        defs_by_name,
        macros_by_name,
        is_added_file,
        changed_lines,
    ):
        self.path = path
        self.parents = parents
        self.defs_by_name = defs_by_name
        self.macros_by_name = macros_by_name
        self.is_added_file = is_added_file
        self.changed_lines = changed_lines
        self.reported = set()
        self.visited = set()
        self.tainted = {}
        self.kwarg_param = None
        self.nonconfigurable_attrs = frozenset()
        self.active_macro_call = None

    def _report(self, kind, node, origin, dest_attr=None):
        msg = format_configurable_msg(kind, origin, dest_attr)
        lineno = getattr(node, "lineno", 1) or 1
        col = getattr(node, "col_offset", 0)
        key = (lineno, col, msg)
        if key in self.reported:
            return
        self.reported.add(key)
        is_changed = (
            self.is_added_file
            or touches_changed(node, self.changed_lines)
            or (
                self.active_macro_call is not None
                and touches_changed(self.active_macro_call, self.changed_lines)
            )
        )
        sev = "error" if is_changed else "info"
        emit(
            "macro_configurable_attr_misuse",
            sev,
            self.path,
            lineno,
            msg,
            configurable_remediation(kind, origin, dest_attr),
        )

    def _origin(self, *exprs):
        origins = [
            o for e in exprs if e is not None and (o := self._expr_origin(e)) is not None
        ]
        return max(origins, key=bool, default=None)

    def _is_kwargs(self, expr):
        return bool(
            self.kwarg_param
            and isinstance(expr, ast.Name)
            and expr.id == self.kwarg_param
        )

    def _kwargs_origin(self, key):
        if (
            isinstance(key, ast.Constant)
            and isinstance(key.value, str)
            and key.value not in self.nonconfigurable_attrs
        ):
            return key.value
        return None

    def _dest_nonconfig(self, call):
        callee = call_name(call.func)
        if (
            callee is None
            or callee in NON_TARGET_CALLABLES
            or callee in self.defs_by_name
        ):
            return frozenset()
        macro = self.macros_by_name.get(callee)
        return UNIVERSAL_NONCONFIGURABLE_TARGET_ATTRS | (
            macro.nonconfigurable() if macro else frozenset()
        )

    def _parent_field_is(self, node, parent_type, field):
        parent = self.parents.get(node)
        if isinstance(parent, parent_type) and getattr(parent, field, None) is node:
            return parent
        return None

    def _is_unconditional_in_fn(self, node):
        cur = self.parents.get(node)
        while cur is not None and not isinstance(cur, ast.FunctionDef):
            if isinstance(cur, (ast.If, ast.For, ast.While)):
                return False
            cur = self.parents.get(cur)
        return True

    def _has_tainted_subexpr(self, expr):
        return any(
            self._expr_origin(n) is not None
            for n in ast.walk(expr)
            if isinstance(n, ast.expr)
        )

    def analyze_function(
        self,
        fn,
        initial_tainted,
        kwarg_param=None,
        nonconfigurable_attrs=frozenset(),
        macro_call=None,
    ):
        macro_touched = bool(
            macro_call is not None
            and touches_changed(macro_call, self.changed_lines)
        )
        key = (
            fn.name,
            frozenset(initial_tainted.items()),
            kwarg_param,
            frozenset(nonconfigurable_attrs),
            macro_touched,
        )
        if key in self.visited:
            return
        self.visited.add(key)
        prev = (
            self.tainted,
            self.kwarg_param,
            self.nonconfigurable_attrs,
            self.active_macro_call,
        )
        self.tainted = dict(initial_tainted)
        self.kwarg_param = kwarg_param
        self.nonconfigurable_attrs = nonconfigurable_attrs
        self.active_macro_call = macro_call
        self.check_stmts(fn.body)
        (
            self.tainted,
            self.kwarg_param,
            self.nonconfigurable_attrs,
            self.active_macro_call,
        ) = prev

    def check_stmts(self, stmts):
        var_dicts = {}
        for stmt in stmts:
            for n in ast.walk(stmt):
                if (
                    isinstance(n, ast.Assign)
                    and len(n.targets) == 1
                    and isinstance(n.targets[0], ast.Name)
                ):
                    dicts = top_dicts(n.value, var_dicts)
                    if dicts:
                        var_dicts.setdefault(n.targets[0].id, []).extend(dicts)

        unpacked_dicts = {}
        for stmt in stmts:
            for n in ast.walk(stmt):
                if not isinstance(n, ast.Call):
                    continue
                dest = self._dest_nonconfig(n)
                for kw in n.keywords:
                    if kw.arg is not None:
                        continue
                    for d in top_dicts(kw.value, var_dicts):
                        unpacked_dicts.setdefault(d, set()).update(dest)

        for stmt in stmts:
            self._visit(stmt, unpacked_dicts)

    def _expr_origin(self, expr):
        if isinstance(expr, ast.Name):
            return self.tainted.get(expr.id)
        if isinstance(expr, ast.BinOp) and isinstance(
            expr.op, (ast.Add, ast.BitOr)
        ):
            return self._origin(expr.left, expr.right)
        if isinstance(expr, ast.BoolOp):
            return self._origin(*expr.values)
        if isinstance(expr, ast.IfExp):
            return self._origin(expr.body, expr.orelse)
        if isinstance(expr, ast.Subscript):
            return (
                self._kwargs_origin(expr.slice)
                if self._is_kwargs(expr.value)
                else None
            )
        if isinstance(expr, ast.Call):
            if isinstance(expr.func, ast.Name) and expr.func.id == "select":
                return ""
            if (
                isinstance(expr.func, ast.Attribute)
                and self._is_kwargs(expr.func.value)
                and expr.func.attr in ("get", "pop")
                and expr.args
            ):
                return self._kwargs_origin(expr.args[0])
        return None

    def _check_bool_leaf(self, expr):
        if isinstance(expr, ast.BoolOp):
            self._check_bool_leaf(expr.values[-1])
        elif not isinstance(expr, ast.UnaryOp):
            if (orig := self._origin(expr)) is not None:
                self._report("bool", expr, orig)

    def _visit(self, node, unpacked_dicts):
        if isinstance(node, ast.Assign):
            self._visit(node.value, unpacked_dicts)
            for target in node.targets:
                self._visit(target, unpacked_dicts)
            orig = self._origin(node.value)
            if (
                len(node.targets) == 1
                and isinstance(node.targets[0], ast.Name)
                and orig is not None
            ):
                self.tainted[node.targets[0].id] = orig
            elif (
                orig is None
                and self._is_unconditional_in_fn(node)
                and not self._has_tainted_subexpr(node.value)
            ):
                for target in node.targets:
                    for name in target_names(target):
                        self.tainted.pop(name, None)
            return

        if isinstance(node, ast.AugAssign):
            self._visit(node.target, unpacked_dicts)
            self._visit(node.value, unpacked_dicts)
            if isinstance(node.op, (ast.Add, ast.BitOr)):
                if (
                    isinstance(node.target, ast.Name)
                    and (orig := self._origin(node.value)) is not None
                ):
                    self.tainted[node.target.id] = orig
            elif (orig := self._origin(node.target, node.value)) is not None:
                self._report("conversion", node, orig)
            return

        if isinstance(node, ast.For):
            if (orig := self._origin(node.iter)) is not None:
                self._report("iter", node.iter, orig)
            self._visit(node.iter, unpacked_dicts)
            self._visit(node.target, unpacked_dicts)
            saved = dict(self.tainted)
            bound = target_names(node.target)
            for name in bound:
                self.tainted.pop(name, None)
            for stmt in node.body:
                self._visit(stmt, unpacked_dicts)
            if not (
                self._is_unconditional_in_fn(node)
                and isinstance(node.iter, (ast.List, ast.Tuple))
                and node.iter.elts
            ):
                for name in bound:
                    if name not in self.tainted and name in saved:
                        self.tainted[name] = saved[name]
            for stmt in node.orelse:
                self._visit(stmt, unpacked_dicts)
            return

        if isinstance(
            node, (ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)
        ):
            saved = dict(self.tainted)
            for gen in node.generators:
                if (orig := self._origin(gen.iter)) is not None:
                    self._report("iter", gen.iter, orig)
                self._visit(gen.iter, unpacked_dicts)
                self._visit(gen.target, unpacked_dicts)
                for name in target_names(gen.target):
                    self.tainted.pop(name, None)
                for if_clause in gen.ifs:
                    self._check_bool_leaf(if_clause)
                    self._visit(if_clause, unpacked_dicts)
            if isinstance(node, ast.DictComp):
                if (orig := self._origin(node.key)) is not None:
                    self._report("dict", node.key, orig)
                self._visit(node.key, unpacked_dicts)
                if (orig := self._origin(node.value)) is not None and orig != "":
                    self._report("dict", node.value, orig)
                self._visit(node.value, unpacked_dicts)
            else:
                if (orig := self._origin(node.elt)) is not None:
                    self._report("list", node.elt, orig)
                self._visit(node.elt, unpacked_dicts)
            self.tainted = saved
            return

        if isinstance(node, (ast.If, ast.While, ast.IfExp)):
            self._check_bool_leaf(node.test)
        elif isinstance(node, ast.BoolOp):
            for v in node.values[:-1]:
                self._check_bool_leaf(v)
        elif isinstance(node, ast.UnaryOp):
            if isinstance(node.op, ast.Not):
                if isinstance(node.operand, ast.BoolOp):
                    self._check_bool_leaf(node.operand)
                elif not (
                    isinstance(node.operand, ast.UnaryOp)
                    and isinstance(node.operand.op, ast.Not)
                ):
                    if (orig := self._origin(node.operand)) is not None:
                        self._report("bool", node, orig)
            elif (orig := self._origin(node.operand)) is not None:
                self._report("conversion", node, orig)
        elif isinstance(node, ast.JoinedStr):
            vals = [
                v.value for v in node.values if isinstance(v, ast.FormattedValue)
            ]
            if (orig := self._origin(*vals)) is not None:
                self._report("format", node, orig)
        elif isinstance(node, ast.BinOp):
            if isinstance(node.op, ast.Mod):
                rhs = (
                    node.right.elts
                    if isinstance(node.right, ast.Tuple)
                    else [node.right]
                )
                if (orig := self._origin(node.left, *rhs)) is not None:
                    self._report("format", node, orig)
            elif not isinstance(node.op, (ast.Add, ast.BitOr)):
                if (orig := self._origin(node.left, node.right)) is not None:
                    self._report("conversion", node, orig)
        elif isinstance(node, ast.Compare):
            left = node.left
            for op, right in zip(node.ops, node.comparators):
                if (orig := self._origin(left, right)) is not None:
                    if isinstance(op, (ast.In, ast.NotIn)):
                        self._report("membership", node, orig)
                    elif not isinstance(
                        op, (ast.Eq, ast.NotEq, ast.Is, ast.IsNot)
                    ) or (not is_none(left) and not is_none(right)):
                        self._report("compare", node, orig)
                left = right
        elif isinstance(node, (ast.List, ast.Tuple, ast.Set)) and isinstance(
            getattr(node, "ctx", ast.Load()), ast.Load
        ):
            fmt = self._parent_field_is(node, ast.BinOp, "right")
            if not (fmt and isinstance(fmt.op, ast.Mod)):
                for elt in node.elts:
                    if isinstance(elt, ast.Starred):
                        if (orig := self._origin(elt.value)) is not None:
                            self._report("iter", elt, orig)
                    elif (orig := self._origin(elt)) is not None:
                        self._report("list", elt, orig)
        elif isinstance(node, ast.Dict):
            parent = self.parents.get(node)
            in_select = (
                isinstance(parent, ast.Call)
                and isinstance(parent.func, ast.Name)
                and parent.func.id == "select"
                and parent.args[:1] == [node]
            )
            unpacked_nonconfig = unpacked_dicts.get(node)
            for k, v in zip(node.keys, node.values):
                if k is None:
                    if (
                        unpacked_nonconfig is None
                        and (orig := self._origin(v)) is not None
                    ):
                        self._report("iter", v, orig)
                    continue
                if (orig := self._origin(k)) is not None:
                    self._report("dict", k, orig)
                if (orig := self._origin(v)) is None:
                    continue
                if in_select:
                    self._report("nested_select", v, orig)
                elif unpacked_nonconfig is None:
                    if orig != "":
                        self._report("dict", v, orig)
                elif isinstance(k, ast.Constant) and k.value in unpacked_nonconfig:
                    self._report("dest", v, orig, dest_attr=k.value)
        elif isinstance(node, ast.Subscript) and isinstance(node.ctx, ast.Load):
            if not self._is_kwargs(node.value):
                if (orig := self._origin(node.value, node.slice)) is not None:
                    self._report("lookup", node, orig)
        elif isinstance(node, ast.Attribute):
            if not self._parent_field_is(node, ast.Call, "func"):
                if (orig := self._origin(node.value)) is not None:
                    self._report("attr_access", node, orig)
        elif isinstance(node, ast.Call):
            self._check_call(node)

        for child in ast.iter_child_nodes(node):
            self._visit(child, unpacked_dicts)

    def _check_call(self, call):
        if isinstance(call.func, ast.Name) and call.func.id == "select":
            return

        all_args = [
            a.value if isinstance(a, ast.Starred) else a for a in call.args
        ] + [kw.value for kw in call.keywords]

        if isinstance(call.func, ast.Attribute):
            recv, method = call.func.value, call.func.attr
            if (orig := self._origin(recv)) is not None:
                self._report("attr_access", call, orig)
            elif (
                method == "format"
                and (orig := self._origin(*all_args)) is not None
            ):
                self._report("format", call, orig)
            elif method in ("append", "insert") and call.args:
                if (orig := self._origin(call.args[-1])) is not None:
                    self._report("list", call, orig)
            elif method in ("extend", "update") and call.args:
                if (orig := self._origin(call.args[0])) is not None:
                    self._report("iter", call, orig)
            elif method in ("get", "pop", "setdefault") and call.args:
                if (orig := self._origin(call.args[0])) is not None:
                    self._report("lookup", call, orig)
                if method == "setdefault" and len(call.args) > 1:
                    if (orig := self._origin(call.args[1])) is not None:
                        self._report("dict", call, orig)
            elif isinstance(recv, ast.Name) and recv.id in ("paths", "json"):
                if (orig := self._origin(*all_args)) is not None:
                    self._report("conversion", call, orig)
        elif isinstance(call.func, ast.Name):
            fn_name = call.func.id
            if fn_name == "bool" and call.args:
                if (orig := self._origin(call.args[0])) is not None:
                    self._report("bool", call, orig)
                return
            if fn_name in SEQUENCE_BUILTINS:
                if (orig := self._origin(*all_args)) is not None:
                    self._report("iter", call, orig)
            elif fn_name in CONCRETE_VALUE_BUILTINS:
                if (orig := self._origin(*all_args)) is not None:
                    self._report("conversion", call, orig)
            elif callee := self.defs_by_name.get(fn_name):
                self._analyze_helper_call(call, callee)

        for arg in call.args:
            if (
                isinstance(arg, ast.Starred)
                and (orig := self._origin(arg.value)) is not None
            ):
                self._report("iter", arg.value, orig)

        dest_nonconfig = self._dest_nonconfig(call)
        for kw in call.keywords:
            if (orig := self._origin(kw.value)) is None:
                continue
            if kw.arg is None:
                self._report("iter", kw.value, orig)
            elif kw.arg in dest_nonconfig:
                self._report("dest", kw, orig, dest_attr=kw.arg)

    def _analyze_helper_call(self, call, callee):
        callee_tainted = {
            param.arg: orig
            for param, arg in zip(callee.args.args, call.args)
            if (orig := self._origin(arg)) is not None
        } | {
            kw.arg: orig
            for kw in call.keywords
            if kw.arg is not None
            and (orig := self._origin(kw.value)) is not None
        }
        forwards_kwargs = any(
            kw.arg is None and self._is_kwargs(kw.value) for kw in call.keywords
        )
        callee_kwarg = (
            callee.args.kwarg.arg
            if callee.args.kwarg and forwards_kwargs
            else None
        )
        self.analyze_function(
            callee,
            callee_tainted,
            callee_kwarg,
            self.nonconfigurable_attrs,
            self.active_macro_call,
        )


def is_empty_identity_expr(expr, empty_kind):
    if empty_kind == "list":
        return isinstance(expr, ast.List) and len(expr.elts) == 0
    if empty_kind == "dict":
        return isinstance(expr, ast.Dict) and len(expr.keys) == 0
    if empty_kind == "empty_str":
        return isinstance(expr, ast.Constant) and expr.value == ""
    return False


def is_harmless_empty_coalesce(cmp_node, param_name, empty_kind, parents):
    if empty_kind is None or len(cmp_node.ops) != 1:
        return False
    parent = parents.get(cmp_node)
    if not isinstance(parent, ast.IfExp) or parent.test is not cmp_node:
        return False
    op = cmp_node.ops[0]
    if isinstance(op, (ast.NotEq, ast.IsNot)):
        return (
            isinstance(parent.body, ast.Name)
            and parent.body.id == param_name
            and is_empty_identity_expr(parent.orelse, empty_kind)
        )
    if isinstance(op, (ast.Eq, ast.Is)):
        return (
            isinstance(parent.orelse, ast.Name)
            and parent.orelse.id == param_name
            and is_empty_identity_expr(parent.body, empty_kind)
        )
    return False


# Check symbolic macros for redeclared inherited attrs.
def check_inherited_attrs(path, text):
    for m in re.finditer(
        r'"applicable_licenses"\s*:\s*attr\.|disable=attr-applicable_licenses',
        text,
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


def parse_base_symbols(base_text):
    if not base_text:
        return set(), set(), set()
    try:
        btree = ast.parse(base_text)
    except SyntaxError:
        return set(), set(), set()
    b_defs = {s.name for s in btree.body if isinstance(s, ast.FunctionDef)}
    b_vars = {
        s.targets[0].id
        for s in btree.body
        if isinstance(s, ast.Assign)
        and len(s.targets) == 1
        and isinstance(s.targets[0], ast.Name)
    }
    b_undoc_attrs = set()
    for n in ast.walk(btree):
        if isinstance(n, ast.Dict):
            for k, v in zip(n.keys, n.values):
                if (
                    isinstance(k, ast.Constant)
                    and isinstance(k.value, str)
                    and isinstance(v, ast.Call)
                    and isinstance(v.func, ast.Attribute)
                    and isinstance(v.func.value, ast.Name)
                    and v.func.value.id == "attr"
                ):
                    if not any(kw.arg == "doc" for kw in v.keywords):
                        b_undoc_attrs.add(k.value)
    return b_defs, b_vars, b_undoc_attrs


def has_nonempty_doc_kw(call):
    for kw in call.keywords:
        if kw.arg == "doc":
            if isinstance(kw.value, ast.Constant) and isinstance(
                kw.value.value, str
            ):
                return bool(kw.value.value.strip())
            return not is_none(kw.value)
    return False


def derived_target_key(expr, strip_colon=False):
    """Normalizes common inline target-name derivations (e.g. `name + '_bin'`)."""
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
        # ":" + (name + "_suffix")
        if (
            strip_colon
            and isinstance(expr.left, ast.Constant)
            and expr.left.value == ":"
        ):
            return derived_target_key(expr.right, strip_colon=False)
        # (":" + name) + "_suffix"
        if (
            strip_colon
            and isinstance(expr.left, ast.BinOp)
            and isinstance(expr.left.op, ast.Add)
            and isinstance(expr.left.left, ast.Constant)
            and expr.left.left.value == ":"
            and isinstance(expr.left.right, ast.Name)
            and expr.left.right.id == "name"
            and isinstance(expr.right, ast.Constant)
            and isinstance(expr.right.value, str)
        ):
            return f"name+{expr.right.value}"
        # name + "_suffix"
        if (
            isinstance(expr.left, ast.Name)
            and expr.left.id == "name"
            and isinstance(expr.right, ast.Constant)
            and isinstance(expr.right.value, str)
            and expr.right.value
        ):
            return f"name+{expr.right.value}"
        # "prefix_" + name (or ":prefix_" + name)
        if (
            isinstance(expr.left, ast.Constant)
            and isinstance(expr.left.value, str)
            and isinstance(expr.right, ast.Name)
            and expr.right.id == "name"
        ):
            prefix = expr.left.value
            if strip_colon:
                if not prefix.startswith(":") or len(prefix) <= 1:
                    return None
                prefix = prefix[1:]
            elif prefix.startswith(":") or not prefix:
                return None
            return f"{prefix}+name"
    # "%s_suffix" % name  or  ":%s_suffix" % name
    if (
        isinstance(expr, ast.BinOp)
        and isinstance(expr.op, ast.Mod)
        and isinstance(expr.left, ast.Constant)
        and isinstance(expr.left.value, str)
        and isinstance(expr.right, ast.Name)
        and expr.right.id == "name"
    ):
        fmt = expr.left.value
        if strip_colon:
            if not fmt.startswith(":"):
                return None
            fmt = fmt[1:]
        elif fmt.startswith(":"):
            return None
        if "%s" in fmt and fmt != "%s":
            return fmt.replace("%s", "{}")
    # "{}_suffix".format(name)  or  ":{}_suffix".format(name)
    if (
        isinstance(expr, ast.Call)
        and isinstance(expr.func, ast.Attribute)
        and expr.func.attr == "format"
        and isinstance(expr.func.value, ast.Constant)
        and isinstance(expr.func.value.value, str)
        and len(expr.args) == 1
        and not expr.keywords
        and isinstance(expr.args[0], ast.Name)
        and expr.args[0].id == "name"
    ):
        fmt = expr.func.value.value
        if strip_colon:
            if not fmt.startswith(":"):
                return None
            fmt = fmt[1:]
        elif fmt.startswith(":"):
            return None
        if "{}" in fmt and fmt != "{}":
            return fmt
    return None


def check_bzl_ast(path, text):
    if re.search(r"\bmacro\(", text):
        check_inherited_attrs(path, text)

    try:
        tree = ast.parse(text, filename=path)
    except SyntaxError:
        return

    lines = text.splitlines()
    is_added_file, base_text, changed_lines = get_diff_info(path, len(lines))
    base_defs, base_vars, base_undoc_attrs = parse_base_symbols(base_text)

    parents = {}
    for parent in ast.walk(tree):
        for child in ast.iter_child_nodes(parent):
            parents[child] = parent
    docstrings = collect_docstrings(tree)

    defs_by_name = {
        s.name: s for s in tree.body if isinstance(s, ast.FunctionDef)
    }
    module_vars = {}
    top_assigns = {}
    for s in tree.body:
        if (
            isinstance(s, ast.Assign)
            and len(s.targets) == 1
            and isinstance(s.targets[0], ast.Name)
        ):
            module_vars[s.targets[0].id] = s.value
            top_assigns[s.targets[0].id] = s

    # Count `implementation = fn` uses across rule/macro/aspect/repository_rule
    # and direct function calls `fn(...)` in the module.
    impl_ref_counts = {}
    called_directly = set()
    for n in ast.walk(tree):
        if not isinstance(n, ast.Call):
            continue
        cname = call_name(n.func)
        if cname in ("macro", "rule", "aspect", "repository_rule"):
            for kw in n.keywords:
                if kw.arg == "implementation" and isinstance(kw.value, ast.Name):
                    impl_ref_counts[kw.value.id] = (
                        impl_ref_counts.get(kw.value.id, 0) + 1
                    )
        elif isinstance(n.func, ast.Name):
            called_directly.add(n.func.id)

    # Discover symbolic macros and their attribute schemas.
    macros_by_name = {}
    for macro_name, call in module_vars.items():
        if not (
            isinstance(call, ast.Call)
            and isinstance(call.func, ast.Name)
            and call.func.id == "macro"
        ):
            continue
        kw_map = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        impl_expr = kw_map.get("implementation")
        if not isinstance(impl_expr, ast.Name):
            continue

        attrs = {}
        inherits_open = False
        inherit_expr = kw_map.get("inherit_attrs")
        if inherit_expr is not None and not is_none(inherit_expr):
            parent = (
                macros_by_name.get(inherit_expr.id)
                if isinstance(inherit_expr, ast.Name)
                else None
            )
            if parent is None:
                inherits_open = True
            else:
                attrs |= parent.attrs
                inherits_open = parent.inherits_open_schema

        non_none_defaults = {}
        for attr_name, attr_expr in extract_attrs_dict(
            kw_map.get("attrs"), module_vars, defs_by_name
        ).items():
            attrs[attr_name] = is_attr_configurable(attr_expr, module_vars)
            is_non_none, empty_kind = classify_non_none_default_attr(
                attr_expr, module_vars
            )
            if is_non_none:
                non_none_defaults[attr_name] = empty_kind

        macros_by_name[macro_name] = MacroInfo(
            macro_name,
            call,
            impl_expr.id,
            attrs,
            non_none_defaults,
            inherits_open,
        )

    # 1. Configurable attributes check (skip if upstream bazel_shac_lint already
    #    registers `configurable_attributes.py` in the workspace).
    upstream_config_check = os.path.join(
        workdir,
        "scripts",
        "shac",
        "bazel_lint",
        "checks",
        "configurable_attributes.py",
    )
    upstream_lint_main = os.path.join(
        workdir, "scripts", "shac", "bazel_lint", "main.py"
    )
    if not (
        os.path.isfile(upstream_config_check)
        and os.path.isfile(upstream_lint_main)
    ):
        shared_nonconfig_by_impl = {}
        for m in macros_by_name.values():
            shared_nonconfig_by_impl.setdefault(m.impl_name, set()).update(
                m.nonconfigurable()
            )

        checker = ConfigurableAttrChecker(
            path, parents, defs_by_name, macros_by_name, is_added_file, changed_lines
        )
        for m in macros_by_name.values():
            fn = defs_by_name.get(m.impl_name)
            if fn is None:
                continue
            shared_nonconfig = (
                INHERITED_NONCONFIGURABLE_ATTRS
                | shared_nonconfig_by_impl[m.impl_name]
            )
            initial_tainted = {
                arg.arg: arg.arg
                for arg in fn.args.args + fn.args.kwonlyargs
                if m.attrs.get(arg.arg)
                or (m.inherits_open_schema and arg.arg not in shared_nonconfig)
            }
            kwarg_param = (
                fn.args.kwarg.arg
                if m.inherits_open_schema and fn.args.kwarg
                else None
            )
            checker.analyze_function(
                fn,
                initial_tainted,
                kwarg_param,
                frozenset(INHERITED_NONCONFIGURABLE_ATTRS - m.attrs.keys())
                | m.nonconfigurable(),
                macro_call=m.call_node,
            )

        analyzed = {key[0] for key in checker.visited}
        for fn in defs_by_name.values():
            if fn.name not in analyzed:
                checker.analyze_function(fn, {})
        checker.check_stmts(
            [s for s in tree.body if not isinstance(s, ast.FunctionDef)]
        )

    # 2. Non-None-defaulted configurable macro attributes compared against None.
    macros_by_impl = {}
    for m in macros_by_name.values():
        macros_by_impl.setdefault(m.impl_name, []).append(m)

    for impl_name, m_list in macros_by_impl.items():
        if impl_name in called_directly:
            continue
        fn = defs_by_name.get(impl_name)
        if fn is None:
            continue
        common_non_none = set(m_list[0].non_none_defaults.keys())
        for m in m_list[1:]:
            common_non_none &= set(m.non_none_defaults.keys())
        if not common_non_none:
            continue
        empty_kinds = {
            k: m_list[0].non_none_defaults[k] for k in common_non_none
        }

        first_assign_line = {}
        for n in ast.walk(fn):
            if isinstance(n, ast.Assign):
                for t in n.targets:
                    for nm in target_names(t):
                        first_assign_line[nm] = min(
                            first_assign_line.get(nm, n.lineno), n.lineno
                        )

        any_macro_changed = any(
            touches_changed(m.call_node, changed_lines) for m in m_list
        )
        for n in ast.walk(fn):
            if not isinstance(n, ast.Compare):
                continue
            left = n.left
            for op, right in zip(n.ops, n.comparators):
                if isinstance(op, (ast.Eq, ast.NotEq, ast.Is, ast.IsNot)) and (
                    is_none(left) or is_none(right)
                ):
                    other = right if is_none(left) else left
                    if (
                        isinstance(other, ast.Name)
                        and other.id in common_non_none
                        and first_assign_line.get(other.id, n.lineno + 1) > n.lineno
                        and not is_harmless_empty_coalesce(
                            n, other.id, empty_kinds.get(other.id), parents
                        )
                    ):
                        is_changed = (
                            is_added_file
                            or touches_changed(n, changed_lines)
                            or any_macro_changed
                        )
                        sev = "error" if is_changed else "info"
                        emit(
                            "macro_non_none_default_none_compare",
                            sev,
                            path,
                            n.lineno,
                            f"Symbolic macro implementation `{impl_name}` compares configurable "
                            f"non-`None`-defaulted attribute `{other.id}` against `None`. In Bazel "
                            "symbolic macros, omitted or `None` arguments resolve to the attribute's "
                            "non-`None` default (`False`, `0`, `\"\"`, `[]`, `{}`) and are wrapped in "
                            f"`select()`, so `{other.id} == None` is always `False` and "
                            f"`{other.id} != None` is always `True`.",
                            f"Forward `{other.id}` directly (combining with `+` for lists/strings or `|`\n"
                            f"for dicts), set the desired `default = ...` on `\"{other.id}\"` in `attrs`,\n"
                            f"or declare `\"{other.id}\"` with `configurable = False` if macro-expansion\n"
                            f"branching is required (never use `if {other.id}:` while configurable,\n"
                            f"because `select()` is always truthy). See {CONFIG_ATTR_URL}.",
                        )
                left = right

    # 3. Symbolic macro implementation parameter defaults (`macro_impl_default_arg`).
    for m in macros_by_name.values():
        if (
            impl_ref_counts.get(m.impl_name, 0) != 1
            or m.impl_name in called_directly
        ):
            continue
        fn = defs_by_name.get(m.impl_name)
        if fn is None:
            continue
        has_defaults = bool(fn.args.defaults) or any(
            d is not None for d in fn.args.kw_defaults
        )
        if not has_defaults:
            continue
        if (
            is_added_file
            or sig_touches_changed(fn, changed_lines)
            or touches_changed(m.call_node, changed_lines)
        ):
            emit(
                "macro_impl_default_arg",
                "error",
                path,
                fn.lineno,
                f"Symbolic macro `{m.name}` implementation `{fn.name}` specifies default "
                "parameter value(s). Symbolic macro defaults are defined in `attrs` (and "
                "`inherit_attrs` defaults are overridden to `None`), so Bazel always "
                "provides a value for every parameter.",
                f"Remove all `= ...` default values from `def {fn.name}(...)` and define "
                "defaults on the `attr.*(...)` entries in `macro(attrs = {...})` per\n"
                "//docs/development/languages/bazel/style/rules_macros.md.",
            )

    # 4. Newly added `rule()` / `macro()` naming, docstrings, and `fuchsia_` prefix.
    allow_fuchsia_prefix = path.startswith(
        ("build/bazel_sdk/", "sdk/", "build/bazel/rules/drivers/")
    )
    for var_name, assign_stmt in top_assigns.items():
        call = assign_stmt.value
        if not (
            isinstance(call, ast.Call)
            and isinstance(call.func, ast.Name)
            and call.func.id in ("macro", "rule")
        ):
            continue
        kind = call.func.id
        is_new_def = var_name not in base_vars and touches_changed(
            assign_stmt, changed_lines
        )
        kw_map = {kw.arg: kw.value for kw in call.keywords if kw.arg}

        if is_new_def:
            if not allow_fuchsia_prefix and var_name.startswith("fuchsia_"):
                suggested = "fx_" + var_name[len("fuchsia_") :]
                emit(
                    "bzl_fuchsia_prefix_outside_sdk",
                    "error",
                    path,
                    assign_stmt.lineno,
                    f"Platform {kind} `{var_name}` uses the `fuchsia_` prefix outside the "
                    "Fuchsia Bazel SDK.",
                    f"Rename `{var_name}` to `{suggested}` (`fx_` prefix); `fuchsia_*` symbols\n"
                    "are reserved for the Fuchsia Bazel SDK (see\n"
                    "//docs/development/languages/bazel/style/rules_macros.md).",
                )

            impl_expr = kw_map.get("implementation")
            if (
                isinstance(impl_expr, ast.Name)
                and impl_ref_counts.get(impl_expr.id, 0) == 1
                and impl_expr.id not in called_directly
            ):
                expected = f"_{var_name}_impl"
                allowed = {expected}
                if var_name.startswith("_"):
                    allowed.add(f"{var_name}_impl")
                if impl_expr.id not in allowed:
                    accept_name = (
                        f"{var_name}_impl"
                        if var_name.startswith("_")
                        else expected
                    )
                    emit(
                        "rule_or_macro_impl_name",
                        "error",
                        path,
                        impl_expr.lineno,
                        f"`{kind}()` `{var_name}` uses implementation function `{impl_expr.id}` "
                        f"instead of `{accept_name}`.",
                        f"Name the `implementation` function `{accept_name}` (leading underscore,\n"
                        f"{kind} name, and `_impl` suffix) per\n"
                        "//docs/development/languages/bazel/style/rules_macros.md.",
                    )

            if not has_nonempty_doc_kw(call):
                emit(
                    "missing_bzl_documentation",
                    "error",
                    path,
                    assign_stmt.lineno,
                    f"Newly defined `{kind}()` `{var_name}` omits a `doc` string.",
                    f"Add a concise `doc = \"...\"` (or multiline `doc = \"\"\"...`) argument to `{var_name}`\n"
                    "per //docs/development/languages/bazel/style/rules_macros.md.",
                )

    # Check non-private `attr.*(...)` entries for `doc` strings and `doc = a + b`.
    for n in ast.walk(tree):
        if isinstance(n, ast.Dict):
            for k, v in zip(n.keys, n.values):
                if not (
                    isinstance(k, ast.Constant)
                    and isinstance(k.value, str)
                    and not k.value.startswith("_")
                    and k.value not in base_undoc_attrs
                    and isinstance(v, ast.Call)
                    and isinstance(v.func, ast.Attribute)
                    and isinstance(v.func.value, ast.Name)
                    and v.func.value.id == "attr"
                    and touches_changed(v, changed_lines)
                ):
                    continue
                if not has_nonempty_doc_kw(v):
                    emit(
                        "missing_bzl_documentation",
                        "error",
                        path,
                        v.lineno,
                        f"Attribute `{k.value}` (`attr.{v.func.attr}(...)`) omits a `doc` string.",
                        f"Provide `doc = \"...\"` on attribute `{k.value}` (only private `_`-prefixed\n"
                        "attributes with self-evident defaults may omit `doc`) per\n"
                        "//docs/development/languages/bazel/style/rules_macros.md.",
                    )
        elif isinstance(n, ast.Call):
            for kw in n.keywords:
                if (
                    kw.arg == "doc"
                    and isinstance(kw.value, ast.BinOp)
                    and isinstance(kw.value.op, ast.Add)
                    and touches_changed(kw.value, changed_lines)
                ):
                    emit(
                        "bzl_docstring_concat",
                        "error",
                        path,
                        kw.value.lineno,
                        "`doc` string concatenates strings with `+`.",
                        "Write single-sentence `doc = \"...\"` on one line (Bazel has no strict line-length\n"
                        "limit) or use a triple-quoted multiline string `doc = \"\"\"...` per\n"
                        "//docs/development/languages/bazel/style/rules_macros.md.",
                    )

    # 5. Public `.bzl` functions and legacy macros.
    for fn in defs_by_name.values():
        param_names = [a.arg for a in fn.args.args + fn.args.kwonlyargs]
        is_new_fn = fn.name not in base_defs and sig_touches_changed(
            fn, changed_lines
        )

        if not fn.name.startswith("_") and is_new_fn:
            if not allow_fuchsia_prefix and fn.name.startswith("fuchsia_"):
                suggested = "fx_" + fn.name[len("fuchsia_") :]
                emit(
                    "bzl_fuchsia_prefix_outside_sdk",
                    "error",
                    path,
                    fn.lineno,
                    f"Public `.bzl` function/macro `{fn.name}` uses the `fuchsia_` prefix "
                    "outside the Fuchsia Bazel SDK.",
                    f"Rename `{fn.name}` to `{suggested}` (`fx_` prefix) per\n"
                    "//docs/development/languages/bazel/style/rules_macros.md.",
                )

            has_docstring = (
                bool(fn.body)
                and isinstance(fn.body[0], ast.Expr)
                and isinstance(fn.body[0].value, ast.Constant)
                and isinstance(fn.body[0].value.value, str)
                and bool(fn.body[0].value.value.strip())
            )
            has_header_comment = (
                fn.lineno >= 2
                and lines[fn.lineno - 2].strip().startswith("#")
            )
            if not has_docstring and not has_header_comment:
                emit(
                    "missing_bzl_documentation",
                    "error",
                    path,
                    fn.lineno,
                    f"Public `.bzl` function `{fn.name}` lacks a docstring or function-level comment.",
                    f"Add a function-level comment or docstring documenting `{fn.name}` per\n"
                    "//docs/development/languages/bazel/style/rules_macros.md.",
                )

            if fn.args.defaults:
                emit(
                    "bzl_public_func_positional_defaults",
                    "error",
                    path,
                    fn.lineno,
                    f"Public `.bzl` function or legacy macro `{fn.name}` declares arguments "
                    "with default values before `*`.",
                    f"Require keyword-only arguments using `*` (e.g. `def {fn.name}(*, ...):`);\n"
                    "boolean arguments and arguments with defaults must always appear after `*`\n"
                    "per //docs/development/languages/bazel/style/rules_macros.md.",
                )

            # Legacy target-creating macro visibility check.
            if fn.name not in impl_ref_counts and "name" in param_names:
                for sub in ast.walk(fn):
                    if not (
                        isinstance(sub, ast.Expr)
                        and isinstance(sub.value, ast.Call)
                        and touches_changed(sub.value, changed_lines)
                    ):
                        continue
                    tcall = sub.value
                    callee = call_name(tcall.func)
                    if callee is None or callee in NON_TARGET_CALLABLES:
                        continue
                    tkw = {kw.arg: kw.value for kw in tcall.keywords if kw.arg}
                    if "name" not in tkw:
                        continue
                    is_main_target = (
                        isinstance(tkw["name"], ast.Name)
                        and tkw["name"].id == "name"
                    )
                    if is_main_target:
                        forwards_kwargs = any(
                            kw.arg is None for kw in tcall.keywords
                        )
                        if (
                            "visibility" in param_names
                            and "visibility" not in tkw
                            and not forwards_kwargs
                        ):
                            emit(
                                "legacy_macro_missing_target_visibility",
                                "error",
                                path,
                                tcall.lineno,
                                f"Legacy macro `{fn.name}` creates main target `{callee}(name = name)` "
                                "without forwarding `visibility`.",
                                "Prefer converting the legacy `def` macro to a symbolic `macro()`, or\n"
                                "forward `visibility = visibility` to the main target per\n"
                                "//docs/development/languages/bazel/style/rules_macros.md.",
                            )
                    elif "visibility" not in tkw:
                        emit(
                            "legacy_macro_missing_target_visibility",
                            "error",
                            path,
                            tcall.lineno,
                            f"Legacy macro `{fn.name}` creates internal target via `{callee}(...)` "
                            "without an explicit `visibility` attribute (allowing package "
                            "`default_visibility` to leak).",
                            "Prefer converting the legacy `def` macro to a symbolic `macro()` (where\n"
                            "helper targets default to private), or pass\n"
                            "`visibility = [\"//visibility:private\"]` explicitly on all non-public\n"
                            "targets per //docs/development/languages/bazel/style/rules_macros.md.",
                        )

        # 6. Macro `kwargs.get(...)` / `kwargs[...]` extraction and inline target-name duplication.
        is_macro_fn = fn.name in macros_by_impl or (
            not fn.name.startswith("_")
            and fn.name not in impl_ref_counts
            and "name" in param_names
        )
        if not is_macro_fn:
            continue

        if fn.args.kwarg is not None:
            kw_name = fn.args.kwarg.arg
            uses_build_flags_wrapper = any(
                isinstance(sub, ast.Call)
                and isinstance(sub.func, ast.Name)
                and sub.func.id.startswith("wrap_")
                and sub.func.id.endswith("_with_build_flags")
                for sub in ast.walk(fn)
            )
            if not uses_build_flags_wrapper:
                fail_guard_nodes = set()
                for sub in ast.walk(fn):
                    if isinstance(sub, ast.If) and any(
                        isinstance(c, ast.Call)
                        and isinstance(c.func, ast.Name)
                        and c.func.id == "fail"
                        for stmt in sub.body
                        for c in ast.walk(stmt)
                    ):
                        fail_guard_nodes.update(ast.walk(sub.test))

                for sub in ast.walk(fn):
                    if sub in fail_guard_nodes or not touches_changed(
                        sub, changed_lines
                    ):
                        continue
                    extracted_attr = None
                    if (
                        isinstance(sub, ast.Call)
                        and isinstance(sub.func, ast.Attribute)
                        and sub.func.attr == "get"
                        and isinstance(sub.func.value, ast.Name)
                        and sub.func.value.id == kw_name
                        and sub.args
                        and isinstance(sub.args[0], ast.Constant)
                        and isinstance(sub.args[0].value, str)
                    ):
                        extracted_attr = sub.args[0].value
                    elif (
                        isinstance(sub, ast.Subscript)
                        and isinstance(sub.ctx, ast.Load)
                        and isinstance(sub.value, ast.Name)
                        and sub.value.id == kw_name
                        and isinstance(sub.slice, ast.Constant)
                        and isinstance(sub.slice.value, str)
                    ):
                        extracted_attr = sub.slice.value
                    if extracted_attr:
                        emit(
                            "macro_kwargs_attr_extraction",
                            "error",
                            path,
                            sub.lineno,
                            f"Macro `{fn.name}` extracts `{extracted_attr}` from `**{kw_name}` "
                            "instead of declaring it as an explicit parameter.",
                            f"Declare `{extracted_attr}` in `{fn.name}`'s parameter list (and in\n"
                            "`macro(attrs = {...})` if not inherited via `inherit_attrs`) and pass\n"
                            f"`{extracted_attr}` explicitly to inner rules/macros per\n"
                            "//docs/development/languages/bazel/style/rules_macros.md.",
                        )

        # Inline target-name duplication (`name = name + "_foo"` and `":" + name + "_foo"`).
        defined_inline = {}
        for sub in ast.walk(fn):
            if not isinstance(sub, ast.Call):
                continue
            callee = call_name(sub.func)
            if callee is None or callee in NON_TARGET_CALLABLES:
                continue
            for kw in sub.keywords:
                if kw.arg == "name":
                    key = derived_target_key(kw.value, strip_colon=False)
                    if key is not None:
                        defined_inline.setdefault(key, []).append(kw.value)

        if defined_inline:
            reported_keys = set()
            for sub in ast.walk(fn):
                if not isinstance(sub, ast.expr):
                    continue
                ref_key = derived_target_key(
                    sub, strip_colon=True
                ) or derived_target_key(sub, strip_colon=False)
                if ref_key not in defined_inline or ref_key in reported_keys:
                    continue
                def_nodes = defined_inline[ref_key]
                if any(sub is dn for dn in def_nodes) and len(def_nodes) == 1:
                    continue
                def_node = def_nodes[0]
                if touches_changed(def_node, changed_lines) or touches_changed(
                    sub, changed_lines
                ):
                    reported_keys.add(ref_key)
                    emit(
                        "macro_inline_target_name_duplication",
                        "error",
                        path,
                        sub.lineno,
                        f"Macro `{fn.name}` constructs derived target name "
                        f"`{ast.unparse(def_node)}` inline in multiple places.",
                        "Assign the derived target name to a local variable (e.g. `helper_name = ...`)\n"
                        "and use that variable for both `name = helper_name` and `\":\" + helper_name`\n"
                        "per //docs/development/languages/bazel/style/rules_macros.md.",
                    )

    # 7. `.bzl` antipatterns (`bazel-bin`, `_solib_`, `PATH` string executable, `transition()`).
    fail_subtrees = set()
    for n in ast.walk(tree):
        if (
            isinstance(n, ast.Call)
            and isinstance(n.func, ast.Name)
            and n.func.id == "fail"
        ):
            fail_subtrees.update(ast.walk(n))

    solib_token = "_so" + "lib_"
    for n in ast.walk(tree):
        if not touches_changed(n, changed_lines):
            continue
        if (
            isinstance(n, ast.Constant)
            and isinstance(n.value, str)
            and n not in docstrings
            and n not in fail_subtrees
        ):
            if "bazel-bin" in n.value:
                emit(
                    "bzl_antipattern",
                    "error",
                    path,
                    n.lineno,
                    "`.bzl` code references the `bazel-bin` output symlink.",
                    "Do not locate build outputs through `bazel-bin` (which points at a single\n"
                    "configuration). Pass outputs through providers or `File` objects instead\n"
                    "per //docs/development/languages/bazel/style/rules_macros.md.",
                )
            elif solib_token in n.value and not path.startswith(
                "build/bazel_sdk/"
            ):
                emit(
                    "bzl_antipattern",
                    "error",
                    path,
                    n.lineno,
                    f"`.bzl` code relies on upstream ruleset `{solib_token}` directory layout.",
                    "Pass shared libraries and runtime files via providers (`CcInfo`, runfiles)\n"
                    f"instead of hardcoding `{solib_token}` implementation details per\n"
                    "//docs/development/languages/bazel/style/rules_macros.md.",
                )
        elif isinstance(n, ast.Call):
            if (
                isinstance(n.func, ast.Attribute)
                and n.func.attr == "run"
                and isinstance(n.func.value, ast.Attribute)
                and n.func.value.attr == "actions"
            ):
                for kw in n.keywords:
                    if (
                        kw.arg == "executable"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        emit(
                            "bzl_antipattern",
                            "error",
                            path,
                            kw.value.lineno,
                            f"`ctx.actions.run` passes string `{kw.value.value}` as `executable` "
                            "(relying on `PATH` or host filesystem paths).",
                            "Declare the tool attribute with `attr.label(executable = True, cfg = \"exec\")`\n"
                            "and pass `executable = ctx.executable.<tool>` per\n"
                            "//docs/development/languages/bazel/style/rules_macros.md.",
                        )
            elif (
                isinstance(n.func, ast.Name)
                and n.func.id == "transition"
                and not path.startswith(("build/bazel/", "build/bazel_sdk/"))
                and (is_added_file or n.lineno in changed_lines)
            ):
                emit(
                    "bzl_antipattern",
                    "error",
                    path,
                    n.lineno,
                    "`.bzl` file defines a custom configuration `transition()` outside `//build/bazel/`.",
                    "Fuchsia platform targets should already build in the right configuration;\n"
                    "do not add custom transitions without Build team sign-off (see\n"
                    "//docs/development/languages/bazel/style/rules_macros.md).",
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


_bzl_func_sig_cache = {}


# Resolve a workspace-local Starlark load label to a checkout-relative file path.
def _resolve_bzl_load_path(caller_path, load_label):
    lbl = (load_label or "").strip()
    if lbl.startswith("@//"):
        lbl = lbl[1:]
    if lbl.startswith("@"):
        return None
    if lbl.startswith("//"):
        pkg, _, rel = lbl[2:].partition(":")
        pkg = pkg.strip("/")
        rel = rel.lstrip("/")
        cand = f"{pkg}/{rel}" if (pkg and rel) else (pkg or rel)
    elif lbl.startswith(":"):
        caller_dir = os.path.dirname(caller_path)
        rel = lbl[1:].lstrip("/")
        cand = f"{caller_dir}/{rel}" if caller_dir else rel
    else:
        return None
    return cand if os.path.isfile(os.path.join(workdir, cand)) else None


# Parse top-level `def <name>(...)` parameter signatures in a `.bzl` file.
def _get_bzl_func_sigs(bzl_rel):
    if bzl_rel in _bzl_func_sig_cache:
        return _bzl_func_sig_cache[bzl_rel]
    sigs = {}
    try:
        src = open(os.path.join(workdir, bzl_rel), encoding="utf-8").read()
        tree = ast.parse(src, filename=bzl_rel)
        for node in tree.body:
            if not isinstance(node, ast.FunctionDef):
                continue
            has_var_kw = node.args.kwarg is not None
            pos_names = [a.arg for a in (node.args.posonlyargs + node.args.args)]
            kwonly_names = [a.arg for a in node.args.kwonlyargs]
            all_names = set(pos_names + kwonly_names)
            req_kwonly = {
                a.arg
                for a, d in zip(node.args.kwonlyargs, node.args.kw_defaults)
                if d is None
            }
            sigs[node.name] = (has_var_kw, all_names, req_kwonly, node.lineno)
    except Exception:
        pass
    _bzl_func_sig_cache[bzl_rel] = sigs
    return sigs


# Check calls to loaded Starlark `def` functions for unexpected or missing kwargs.
def check_loaded_starlark_func_calls(path, text):
    try:
        tree = ast.parse(text, filename=path)
    except Exception:
        return
    loaded_funcs = {}
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not (isinstance(call.func, ast.Name) and call.func.id == "load"):
            continue
        if not call.args or not (
            isinstance(call.args[0], ast.Constant)
            and isinstance(call.args[0].value, str)
        ):
            continue
        bzl_rel = _resolve_bzl_load_path(path, call.args[0].value)
        if not bzl_rel:
            continue
        sigs = _get_bzl_func_sigs(bzl_rel)
        if not sigs:
            continue
        for arg in call.args[1:]:
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                sym = arg.value
                if sym in sigs:
                    loaded_funcs[sym] = (bzl_rel, sym, sigs[sym])
        for kw in call.keywords:
            if (
                kw.arg
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                sym = kw.value.value
                if sym in sigs:
                    loaded_funcs[kw.arg] = (bzl_rel, sym, sigs[sym])
    if not loaded_funcs:
        return
    for node in ast.walk(tree):
        if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)):
            continue
        local_sym = node.func.id
        if local_sym not in loaded_funcs:
            continue
        bzl_rel, orig_sym, (has_var_kw, all_names, req_kwonly, def_line) = (
            loaded_funcs[local_sym]
        )
        has_star_kwargs = any(kw.arg is None for kw in node.keywords)
        passed_kwargs = [kw for kw in node.keywords if kw.arg is not None]
        if not has_var_kw:
            for kw in passed_kwargs:
                if kw.arg not in all_names:
                    allowed_str = ", ".join(f"`{n}`" for n in sorted(all_names))
                    emit(
                        "starlark_call_unexpected_kwarg",
                        "error",
                        path,
                        getattr(kw, "lineno", node.lineno),
                        f"Call to `{local_sym}(...)` passes unexpected keyword argument "
                        f"`{kw.arg}`, which is not declared in `def {orig_sym}(...)` in "
                        f"'{bzl_rel}:{def_line}' (declared parameters: {allowed_str}).",
                        f"Either add `{kw.arg}` to `def {orig_sym}(...)` in '{bzl_rel}' "
                        f"and handle it in `{orig_sym}`, or remove `{kw.arg} = ...` from "
                        f"the `{local_sym}(...)` call in '{path}'.",
                    )
        if not has_star_kwargs and req_kwonly:
            passed_names = {kw.arg for kw in passed_kwargs}
            missing = sorted(req_kwonly - passed_names)
            if missing:
                miss_str = ", ".join(f"`{m}`" for m in missing)
                emit(
                    "starlark_call_missing_kwarg",
                    "error",
                    path,
                    node.lineno,
                    f"Call to `{local_sym}(...)` is missing required keyword-only "
                    f"argument(s) {miss_str} declared in `def {orig_sym}(...)` in "
                    f"'{bzl_rel}:{def_line}'.",
                    f"Pass {miss_str} to `{local_sym}(...)` in '{path}', or give "
                    f"{miss_str} a default value in `def {orig_sym}(...)` in '{bzl_rel}'.",
                )


for f in changed_files():
    try:
        text = open(os.path.join(workdir, f), encoding="utf-8").read()
    except Exception:
        continue
    if f.endswith(".bzl") or os.path.basename(f) in ("BUILD.bazel", "BUILD"):
        check_loaded_starlark_func_calls(f, text)
    if f.endswith(".bzl"):
        check_bzl_ast(f, text)
    elif f.endswith(".gni"):
        check_gni_crate_name(f, text)

print(json.dumps(findings, indent=2))
PYEOF
