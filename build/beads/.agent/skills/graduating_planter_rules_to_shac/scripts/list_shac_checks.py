#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Lists the shac Bazel lint checks with the facts needed to judge coverage.

For each shac Bazel lint check, prints its enforcement level, implementation
file, the files it runs on (`applies_to_globs` and `file_kinds`),
`policy_exempt_globs`, allowlist support, and allowlist size. Also prints the
globs excluded from every check (`_EXCLUDED_GLOBS`) and the native
`shac.check()` functions registered in `register_bazel_checks()` in
//scripts/shac/bazel.star.

Check metadata and allowlists come from shac's own code: the script imports
//scripts/shac/bazel_lint/checks to populate `framework.REGISTRY` and loads
allowlists with `allowlist.load_allowlist()`. //scripts/shac/bazel.star is
parsed (with Python's `ast`, as the bazel_lint framework parses Starlark) only
for each check's enforcement level, `applies_to_globs`, its registration, and
the native checks.

The script verifies that the checks defined by `_make_bazel_lint_check()` in
bazel.star, the checks registered in `register_bazel_checks()`, and the checks
in `framework.REGISTRY` (imported directly in checks/__init__.py) are the same
set. It exits non-zero if they differ, if an enforcement level or glob list
can't be resolved, or if an allowlist is invalid, and then prints only the
errors to stderr. So a report on stdout (exit 0) means every listed check is
registered in bazel.star and imported in checks/__init__.py.

Usage:
  list_shac_checks.py [--json] [--fuchsia-dir DIR]

If the `fuchsia-vendored-python` shebang fails (it isn't on $PATH, or the
current directory is outside a checkout), run the script with `python3` 3.13
or newer, which shac's code needs.
The checkout defaults to the one containing this script, then $FUCHSIA_DIR.
The script can run from any directory. Paths are relative to the checkout.

JSON output (`--json`):
  {
    "lint_checks": [
      {
        "name": str,                 # e.g. "bazel_target_visibility"
        "enforcement": str | null,   # "all" or "unchanged_lines_warning_only"
                                     # (bazel.star _VALID_ENFORCEMENT_MODES);
                                     # null if not in bazel.star
        "impl": str | null,          # implementation file; null if missing
        "applies_to_globs": [str],   # resolved; shac only passes files
                                     # matching these to the check
        "file_kinds": [str],         # framework.FileKind values, e.g. "build"
        "policy_exempt_globs": [str],
        "supports_allowlist": bool,
        "supports_targets_allowlist": bool,
        "allowlist": null | {        # null if the allowlist file is absent
          "file": str,
          "entries": int,            # total number of entries
          "whole_file": int,         # entries with "path" and no "targets"
          "path_prefixes": [str],    # "path_prefix" values
          "target_scoped": int       # entries with "targets"
        }
      }
    ],
    "excluded_globs": [str],         # _EXCLUDED_GLOBS, added to every check
    "native_checks": [str]           # shac.check() functions in bazel.star
  }
"""

import argparse
import ast
import dataclasses
import importlib
import inspect
import json
import os
import sys
from pathlib import Path
from types import ModuleType
from typing import Any, TypeGuard

# Importing shac's checks must not leave __pycache__ directories in the tree.
sys.dont_write_bytecode = True

# This file is <root>/build/beads/.agent/skills/<skill>/scripts/<this file>.
_SCRIPT_ROOT = Path(__file__).resolve().parents[6]
_BAZEL_STAR = Path("scripts/shac/bazel.star")
_BAZEL_LINT_DIR = Path("scripts/shac/bazel_lint")
_CHECKS_DIR = _BAZEL_LINT_DIR / "checks"
_ALLOWLISTS_DIR = _BAZEL_LINT_DIR / "allowlists"
_MAKE_LINT_CHECK = "_make_bazel_lint_check"
_REGISTER_FN = "register_bazel_checks"
_VALID_MODES_CONST = "_VALID_ENFORCEMENT_MODES"
_EXCLUDED_GLOBS_CONST = "_EXCLUDED_GLOBS"


class ShacListError(Exception):
    """Raised when the shac sources can't be read or are inconsistent."""


@dataclasses.dataclass
class StarLintCheck:
    """A `_make_bazel_lint_check()` call in bazel.star.

    Attributes:
        name: The check name (first argument).
        var: The variable the check is assigned to, or None if not assigned.
        enforcement: The resolved enforcement level.
        globs: The resolved `applies_to_globs`.
        registered: Whether `register_bazel_checks()` registers it.
    """

    name: str
    var: str | None
    enforcement: str
    globs: list[str]
    registered: bool = False


@dataclasses.dataclass
class StarInfo:
    """Facts parsed from bazel.star.

    Attributes:
        lint_checks: Lint checks in definition order.
        excluded_globs: The resolved `_EXCLUDED_GLOBS`.
        native_checks: Native `shac.check()` functions, in registration order.
    """

    lint_checks: list[StarLintCheck]
    excluded_globs: list[str]
    native_checks: list[str]


@dataclasses.dataclass
class _Constants:
    """Top-level string and string-list constants in bazel.star."""

    strings: dict[str, str]
    lists: dict[str, ast.expr]


def _find_root(cli_dir: Path | None) -> Path:
    """Returns the Fuchsia checkout root to inspect.

    Uses `cli_dir` if given, else the checkout containing this script, else
    $FUCHSIA_DIR. The script's own checkout comes before $FUCHSIA_DIR so that
    running the script from a worktree inspects that worktree.
    """
    if cli_dir is not None:
        candidates = [cli_dir]
    else:
        candidates = [_SCRIPT_ROOT]
        if os.environ.get("FUCHSIA_DIR"):
            candidates.append(Path(os.environ["FUCHSIA_DIR"]))
    for candidate in candidates:
        if (candidate / _BAZEL_STAR).is_file():
            return candidate.resolve()
    tried = ", ".join(str(c) for c in candidates)
    raise ShacListError(
        f"no Fuchsia checkout with {_BAZEL_STAR} found (tried: {tried}); "
        "pass --fuchsia-dir"
    )


def _simple_assign(node: ast.AST) -> tuple[str, ast.expr] | None:
    """Returns `(name, value)` if `node` is `NAME = value`, else None."""
    if (
        isinstance(node, ast.Assign)
        and len(node.targets) == 1
        and isinstance(node.targets[0], ast.Name)
    ):
        return node.targets[0].id, node.value
    return None


def _constants(tree: ast.Module) -> _Constants:
    """Returns the top-level string and list constants in `tree`."""
    consts = _Constants(strings={}, lists={})
    for node in tree.body:
        assign = _simple_assign(node)
        if assign is None:
            continue
        name, value = assign
        if isinstance(value, ast.Constant) and isinstance(value.value, str):
            consts.strings[name] = value.value
        elif isinstance(value, (ast.List, ast.Tuple, ast.BinOp, ast.Name)):
            consts.lists[name] = value
    return consts


def _resolve_string(expr: ast.expr, consts: _Constants, what: str) -> str:
    """Resolves `expr` to a string literal or a known string constant.

    Raises `ShacListError` naming `what` if `expr` is anything else.
    """
    if isinstance(expr, ast.Constant) and isinstance(expr.value, str):
        return expr.value
    if isinstance(expr, ast.Name) and expr.id in consts.strings:
        return consts.strings[expr.id]
    raise ShacListError(
        f"{_BAZEL_STAR}:{expr.lineno}: can't resolve {what} "
        f"`{ast.unparse(expr)}` to a string literal or a top-level string "
        "constant"
    )


def _resolve_list(expr: ast.expr, consts: _Constants, what: str) -> list[str]:
    """Resolves `expr` to a list of strings.

    Accepts list or tuple literals, top-level list constants such as
    `_BUILD_FILES`, `list(...)` of these, and `+` concatenations of these.
    """
    if isinstance(expr, (ast.List, ast.Tuple)):
        return [_resolve_string(e, consts, what) for e in expr.elts]
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
        return _resolve_list(expr.left, consts, what) + _resolve_list(
            expr.right, consts, what
        )
    if isinstance(expr, ast.Name) and expr.id in consts.lists:
        return _resolve_list(consts.lists[expr.id], consts, what)
    if _is_call_to(expr, "list") and len(expr.args) == 1:
        return _resolve_list(expr.args[0], consts, what)
    raise ShacListError(
        f"{_BAZEL_STAR}:{expr.lineno}: can't resolve {what} "
        f"`{ast.unparse(expr)}` to a list of strings"
    )


def _find_function(tree: ast.Module, name: str) -> ast.FunctionDef:
    """Returns the top-level function `name` in bazel.star."""
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name == name:
            return node
    raise ShacListError(f"{_BAZEL_STAR}: function {name}() not found")


def _default_enforcement(
    make_fn: ast.FunctionDef, consts: _Constants
) -> str | None:
    """Returns the default of the `enforcement` parameter, if it has one.

    Most checks don't pass `enforcement` and get this default ("all"), so
    reading it from bazel.star keeps the script right if the default changes.
    """
    args = make_fn.args
    positional = args.posonlyargs + args.args
    params = list(
        zip(positional[len(positional) - len(args.defaults) :], args.defaults)
    )
    params += [
        (arg, default)
        for arg, default in zip(args.kwonlyargs, args.kw_defaults)
        if default is not None
    ]
    for arg, default in params:
        if arg.arg == "enforcement":
            return _resolve_string(
                default, consts, f"default enforcement of {_MAKE_LINT_CHECK}"
            )
    return None


def _is_call_to(node: ast.AST, dotted: str) -> TypeGuard[ast.Call]:
    """Returns whether `node` is a call to `dotted` (e.g. "shac.check")."""
    return isinstance(node, ast.Call) and ast.unparse(node.func) == dotted


def _is_within(node: ast.expr, scope: ast.stmt) -> bool:
    """Returns whether `node` is inside `scope` (by line range)."""
    return scope.lineno <= node.lineno <= (scope.end_lineno or scope.lineno)


def _check_name(call: ast.Call, consts: _Constants) -> str:
    """Returns the check name of a `_make_bazel_lint_check()` call."""
    kwargs = {kw.arg: kw.value for kw in call.keywords if kw.arg}
    if call.args:
        return _resolve_string(call.args[0], consts, "check name")
    if "check_name" in kwargs:
        return _resolve_string(kwargs["check_name"], consts, "check name")
    raise ShacListError(
        f"{_BAZEL_STAR}:{call.lineno}: {_MAKE_LINT_CHECK}() has no check name"
    )


def _parse_lint_check(
    call: ast.Call,
    var: str | None,
    consts: _Constants,
    default: str | None,
    valid_modes: set[str],
) -> StarLintCheck:
    """Parses one `_make_bazel_lint_check()` call."""
    where = f"{_BAZEL_STAR}:{call.lineno}"
    kwargs = {kw.arg: kw.value for kw in call.keywords if kw.arg}
    name = _check_name(call, consts)
    if "enforcement" in kwargs:
        enforcement = _resolve_string(
            kwargs["enforcement"], consts, f"enforcement of check '{name}'"
        )
    elif default is not None:
        enforcement = default
    else:
        raise ShacListError(
            f"{where}: check '{name}' has no enforcement argument and "
            f"{_MAKE_LINT_CHECK}() has no default for it"
        )
    if valid_modes and enforcement not in valid_modes:
        raise ShacListError(
            f"{where}: check '{name}' has enforcement '{enforcement}', which "
            f"is not in {_VALID_MODES_CONST} {sorted(valid_modes)}"
        )
    if "applies_to_globs" not in kwargs:
        raise ShacListError(f"{where}: check '{name}' has no applies_to_globs")
    globs = _resolve_list(
        kwargs["applies_to_globs"], consts, f"applies_to_globs of '{name}'"
    )
    return StarLintCheck(
        name=name, var=var, enforcement=enforcement, globs=globs
    )


def _parse_lint_checks(
    tree: ast.Module, consts: _Constants
) -> list[StarLintCheck]:
    """Returns the `_make_bazel_lint_check()` calls in source order."""
    make_fn = _find_function(tree, _MAKE_LINT_CHECK)
    default = _default_enforcement(make_fn, consts)
    valid_modes = set()
    if _VALID_MODES_CONST in consts.lists:
        valid_modes = set(
            _resolve_list(
                consts.lists[_VALID_MODES_CONST], consts, _VALID_MODES_CONST
            )
        )

    call_vars: dict[int, str] = {}
    for node in ast.walk(tree):
        assign = _simple_assign(node)
        if assign and _is_call_to(assign[1], _MAKE_LINT_CHECK):
            call_vars[id(assign[1])] = assign[0]

    # ast.walk() is breadth-first, so sort the calls into source order.
    calls = sorted(
        (
            node
            for node in ast.walk(tree)
            if _is_call_to(node, _MAKE_LINT_CHECK)
            and not _is_within(node, make_fn)
        ),
        key=lambda node: (node.lineno, node.col_offset),
    )
    checks = [
        _parse_lint_check(
            call, call_vars.get(id(call)), consts, default, valid_modes
        )
        for call in calls
    ]
    seen: set[str] = set()
    for check in checks:
        if check.name in seen:
            raise ShacListError(
                f"{_BAZEL_STAR}: check '{check.name}' is defined twice"
            )
        seen.add(check.name)
    return checks


def _parse_registrations(
    tree: ast.Module, consts: _Constants, checks: list[StarLintCheck]
) -> list[str]:
    """Marks registered lint checks and returns the native checks.

    Only registrations in the body of `register_bazel_checks()` count.
    """
    by_var = {c.var: c for c in checks if c.var}
    by_name = {c.name: c for c in checks}
    native: list[str] = []
    for node in ast.walk(_find_function(tree, _REGISTER_FN)):
        if not _is_call_to(node, "shac.register_check") or not node.args:
            continue
        arg = node.args[0]
        if isinstance(arg, ast.Name) and arg.id in by_var:
            by_var[arg.id].registered = True
        elif _is_call_to(arg, "shac.check") and arg.args:
            native.append(ast.unparse(arg.args[0]))
        elif _is_call_to(arg, _MAKE_LINT_CHECK):
            by_name[_check_name(arg, consts)].registered = True
        else:
            raise ShacListError(
                f"{_BAZEL_STAR}:{node.lineno}: unrecognized registration "
                f"`{ast.unparse(node)}` in {_REGISTER_FN}()"
            )
    return native


def _parse_bazel_star(path: Path) -> StarInfo:
    """Parses lint checks, globs, and registrations from bazel.star."""
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except SyntaxError as e:
        raise ShacListError(f"{_BAZEL_STAR}: can't parse: {e}") from e
    consts = _constants(tree)
    if _EXCLUDED_GLOBS_CONST not in consts.lists:
        raise ShacListError(f"{_BAZEL_STAR}: {_EXCLUDED_GLOBS_CONST} not found")
    excluded = _resolve_list(
        consts.lists[_EXCLUDED_GLOBS_CONST], consts, _EXCLUDED_GLOBS_CONST
    )
    checks = _parse_lint_checks(tree, consts)
    native = _parse_registrations(tree, consts, checks)
    return StarInfo(
        lint_checks=checks, excluded_globs=excluded, native_checks=native
    )


def _checks_init_modules(init_path: Path) -> set[str]:
    """Returns the modules imported by `from . import ...` in `init_path`."""
    tree = ast.parse(init_path.read_text(encoding="utf-8"))
    modules: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom) and node.level == 1:
            if node.module is None:
                modules.update(alias.name for alias in node.names)
            else:
                modules.add(node.module.split(".")[0])
    return modules


def _import_shac_checks(root: Path) -> tuple[dict[str, Any], ModuleType]:
    """Imports shac's bazel_lint checks from `root`.

    Returns `(framework.REGISTRY, allowlist module)`.

    The modules are loaded with importlib rather than import statements so
    that mypy doesn't try to resolve them: bazel_lint has no python_library,
    and a same-named module elsewhere (e.g. a `checks` directory) could be
    picked up instead.
    """
    sys.path.insert(0, str(root / _BAZEL_LINT_DIR))
    try:
        allowlist = importlib.import_module("allowlist")
        # Importing checks populates framework.REGISTRY.
        importlib.import_module("checks")
        framework = importlib.import_module("framework")
    except Exception as e:  # Any import-time failure in shac's code.
        raise ShacListError(
            f"can't import shac bazel_lint checks from {_BAZEL_LINT_DIR}: "
            f"{type(e).__name__}: {e}"
        ) from e
    registry: dict[str, Any] = framework.REGISTRY
    return registry, allowlist


def _relative(path: Path, root: Path) -> str:
    """Returns `path` relative to `root` if it is under it."""
    try:
        return str(path.resolve().relative_to(root))
    except ValueError:
        return str(path)


def _allowlist_stats(
    check: Any, root: Path, allowlist_mod: ModuleType
) -> dict[str, Any] | None:
    """Returns allowlist stats for `check`, or None if it has no file."""
    path = root / _ALLOWLISTS_DIR / f"{check.name}.json"
    if not path.is_file():
        return None
    if not check.supports_allowlist:
        raise ShacListError(
            f"{_relative(path, root)}: allowlist file exists but check "
            f"'{check.name}' has supports_allowlist = False"
        )
    try:
        loaded = allowlist_mod.load_allowlist(
            path, supports_targets_allowlist=check.supports_targets_allowlist
        )
    except allowlist_mod.AllowlistError as e:
        message = str(e).replace(str(path), _relative(path, root))
        raise ShacListError(f"invalid allowlist: {message}") from e
    entries = loaded.entries
    return {
        "file": _relative(path, root),
        "entries": len(entries),
        "whole_file": sum(
            1 for e in entries if e.path is not None and e.targets is None
        ),
        "path_prefixes": [e.path_prefix for e in entries if e.path_prefix],
        "target_scoped": sum(1 for e in entries if e.targets is not None),
    }


def _check_problems(
    name: str,
    star_check: StarLintCheck | None,
    impl_stem: str | None,
    init_modules: set[str],
) -> list[str]:
    """Returns the inconsistencies for one check."""
    init = _CHECKS_DIR / "__init__.py"
    problems = []
    if impl_stem is None:
        problems.append(
            f"check '{name}' is defined in {_BAZEL_STAR} but has no "
            f"implementation in framework.REGISTRY (is its module imported "
            f"in {init}?)"
        )
    elif impl_stem not in init_modules:
        problems.append(
            f"check '{name}': module {impl_stem} is not imported directly in "
            f"{init}"
        )
    if star_check is None:
        problems.append(
            f"check '{name}' is in framework.REGISTRY but has no "
            f"{_MAKE_LINT_CHECK}() call in {_BAZEL_STAR}"
        )
    elif not star_check.registered:
        problems.append(
            f"check '{name}' is defined in {_BAZEL_STAR} but not registered "
            f"in {_REGISTER_FN}()"
        )
    return problems


def _collect(root: Path) -> tuple[dict[str, Any], list[str]]:
    """Returns `(report, problems)` for the checkout at `root`.

    `report` follows the JSON schema in the module docstring. `problems` lists
    inconsistencies between bazel.star, checks/__init__.py, and the registry.
    """
    star = _parse_bazel_star(root / _BAZEL_STAR)
    registry, allowlist_mod = _import_shac_checks(root)
    init_modules = _checks_init_modules(root / _CHECKS_DIR / "__init__.py")

    problems: list[str] = []
    lint_checks: list[dict[str, Any]] = []
    star_by_name = {c.name: c for c in star.lint_checks}
    names = [c.name for c in star.lint_checks]
    names += sorted(set(registry) - set(star_by_name))
    for name in names:
        star_check = star_by_name.get(name)
        check = registry.get(name)
        impl_path = Path(inspect.getfile(type(check))) if check else None
        problems += _check_problems(
            name,
            star_check,
            impl_path.stem if impl_path else None,
            init_modules,
        )
        lint_checks.append(
            {
                "name": name,
                "enforcement": star_check.enforcement if star_check else None,
                "impl": _relative(impl_path, root) if impl_path else None,
                "applies_to_globs": star_check.globs if star_check else [],
                "file_kinds": (
                    sorted(k.value for k in check.file_kinds) if check else []
                ),
                "policy_exempt_globs": (
                    list(check.policy_exempt_globs) if check else []
                ),
                "supports_allowlist": bool(check and check.supports_allowlist),
                "supports_targets_allowlist": bool(
                    check and check.supports_targets_allowlist
                ),
                "allowlist": (
                    _allowlist_stats(check, root, allowlist_mod)
                    if check
                    else None
                ),
            }
        )
    report = {
        "lint_checks": lint_checks,
        "excluded_globs": star.excluded_globs,
        "native_checks": star.native_checks,
    }
    return report, problems


def _plural(count: int, singular: str) -> str:
    """Returns e.g. "1 entry", "2 entries", or "2 targets"."""
    if count == 1:
        return f"{count} {singular}"
    if singular.endswith("y"):
        return f"{count} {singular[:-1]}ies"
    return f"{count} {singular}s"


def _print_text(report: dict[str, Any]) -> None:
    """Prints `report` in a human-readable form."""
    for c in report["lint_checks"]:
        print(f"{c['name']}: enforcement={c['enforcement']}")
        print(f"  impl: {c['impl']}")
        print(f"  applies_to_globs: {', '.join(c['applies_to_globs'])}")
        print(f"  file_kinds: {', '.join(c['file_kinds'])}")
        print(
            f"  supports_allowlist={c['supports_allowlist']}, "
            f"supports_targets_allowlist={c['supports_targets_allowlist']}"
        )
        if c["policy_exempt_globs"]:
            globs = ", ".join(c["policy_exempt_globs"])
            print(f"  policy_exempt_globs: {globs}")
        a = c["allowlist"]
        if a:
            prefixes = a["path_prefixes"]
            # Target-scoped entries can list several targets each; the counts
            # are always entries.
            print(
                f"  allowlist: {_plural(a['entries'], 'entry')} in "
                f"{a['file']}: {a['whole_file']} whole-file, "
                f"{len(prefixes)} path_prefix, "
                f"{_plural(a['target_scoped'], 'target-scoped entry')}"
            )
            if prefixes:
                print(f"    path_prefixes: {', '.join(prefixes)}")
    print(f"excluded from every check: {', '.join(report['excluded_globs'])}")
    native = ", ".join(report["native_checks"]) or "(none)"
    print(f"native shac.check() functions in bazel.star: {native}")


def main() -> int:
    """Runs the script and returns its exit code."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--json", action="store_true", help="print JSON")
    parser.add_argument(
        "--fuchsia-dir",
        type=Path,
        help="Fuchsia checkout root (default: this script's checkout, then "
        "$FUCHSIA_DIR)",
    )
    args = parser.parse_args()

    try:
        root = _find_root(args.fuchsia_dir)
        report, problems = _collect(root)
    except ShacListError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    # Print the report only if it is complete and consistent.
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    if problems:
        return 1
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        _print_text(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
