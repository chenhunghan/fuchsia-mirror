# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared AST checks enforcing WLAN Python test skip and setup_class rules."""

import ast
import collections
import json
import pathlib
import unittest
from collections.abc import Collection, Mapping, Sequence, Set
from importlib.resources import files

_ABORT_FUNCS = frozenset(
    {"abort_class", "abort_class_if", "abort_all", "abort_all_if"}
)
_ABORT_SIGNALS = frozenset(
    {"TestAbortClass", "TestAbortAll", "TestAbortSignal"}
)
_SKIP_FUNCS = frozenset({"skip", "skip_if"})
_SKIP_SIGNALS = frozenset({"TestSkip"})


def _is_phy_compliance_file(filepath: str) -> bool:
    if not filepath.startswith("src/connectivity/wlan/tests/core/"):
        return False
    filename = filepath.rsplit("/", 1)[-1]
    return "PhyCompliance" in filename and filename.endswith(".py")


def _scan_file(
    filepath: str, source: str
) -> tuple[
    Set[str],
    Mapping[str, Set[str]],
    Sequence[tuple[int, str]],
    Sequence[tuple[int, str]],
]:
    tree = ast.parse(source, filename=filepath)
    is_phy_compliance = _is_phy_compliance_file(filepath)

    imported_aborts = {
        alias.asname or alias.name
        for node in ast.walk(tree)
        if isinstance(node, ast.ImportFrom)
        and node.module in ("mobly.asserts", "mobly.signals")
        for alias in node.names
        if alias.name in (_ABORT_FUNCS | _ABORT_SIGNALS)
    }
    imported_skips = {
        alias.asname or alias.name
        for node in ast.walk(tree)
        if isinstance(node, ast.ImportFrom)
        and node.module in ("mobly.asserts", "mobly.signals")
        for alias in node.names
        if alias.name in (_SKIP_FUNCS | _SKIP_SIGNALS)
    }

    defs: set[str] = set()
    calls: dict[str, set[str]] = collections.defaultdict(set)
    raw_aborts: list[tuple[int, str]] = []
    skips: list[tuple[int, str]] = []

    def walk(
        node: ast.AST, scope: tuple[str, ...], *, check_signals: bool = True
    ) -> None:
        qualname = ".".join(scope) if scope else "<module>"
        if isinstance(node, ast.ClassDef):
            for child in (*node.decorator_list, *node.bases, *node.keywords):
                walk(child, scope, check_signals=check_signals)
            for stmt in node.body:
                walk(stmt, (*scope, node.name), check_signals=check_signals)
            return
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            fn_scope = (*scope, node.name)
            fn_qual = ".".join(fn_scope)
            defs.add(fn_qual)
            if scope:
                calls[qualname].add(node.name)
            for decorator in node.decorator_list:
                walk(decorator, scope, check_signals=check_signals)
            walk(node.args, scope, check_signals=check_signals)
            if node.returns is not None:
                walk(node.returns, scope, check_signals=check_signals)
            for stmt in node.body:
                walk(stmt, fn_scope, check_signals=check_signals)
            return
        if isinstance(node, ast.ExceptHandler):
            if node.type is not None:
                walk(node.type, scope, check_signals=False)
            for stmt in node.body:
                walk(stmt, scope, check_signals=check_signals)
            return

        if isinstance(node, ast.Attribute):
            if not node.attr.startswith("test_"):
                calls[qualname].add(node.attr)
            is_asserts = (
                isinstance(node.value, ast.Name) and node.value.id == "asserts"
            )
            if check_signals:
                if node.attr in _ABORT_SIGNALS or (
                    is_asserts and node.attr in _ABORT_FUNCS
                ):
                    raw_aborts.append((node.lineno, qualname))
                if not is_phy_compliance and (
                    node.attr in _SKIP_SIGNALS
                    or (is_asserts and node.attr in _SKIP_FUNCS)
                ):
                    skips.append((node.lineno, qualname))
        elif isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            if not node.id.startswith("test_"):
                calls[qualname].add(node.id)
            if check_signals:
                if node.id in imported_aborts:
                    raw_aborts.append((node.lineno, qualname))
                if not is_phy_compliance and node.id in imported_skips:
                    skips.append((node.lineno, qualname))

        for child in ast.iter_child_nodes(node):
            walk(child, scope, check_signals=check_signals)

    walk(tree, ())
    return defs, calls, tuple(raw_aborts), tuple(sorted(skips))


def find_violations_in_sources(
    sources: Mapping[str, str],
) -> Mapping[str, tuple[Sequence[tuple[int, str]], Sequence[tuple[int, str]]]]:
    """Returns per-file `(setup_class_aborts, runtime_skips)` across `sources`."""
    scanned = {
        filepath: _scan_file(filepath, source)
        for filepath, source in sources.items()
    }

    public_defs_by_name: dict[
        str, set[tuple[str, str]]
    ] = collections.defaultdict(set)
    reachable: set[tuple[str, str]] = set()
    for filepath, (defs, _, _, _) in scanned.items():
        for qualname in defs:
            parts = qualname.split(".")
            callable_name = (
                parts[-2]
                if parts[-1] == "__init__" and len(parts) >= 2
                else parts[-1]
            )
            if not callable_name.startswith("_"):
                public_defs_by_name[callable_name].add((filepath, qualname))
            if parts[-1] == "setup_class":
                reachable.add((filepath, qualname))

    queue = collections.deque(reachable)
    while queue:
        filepath, qualname = queue.popleft()
        defs, calls, _, _ = scanned[filepath]
        cls_prefix = qualname.split(".", 1)[0] if "." in qualname else ""
        for name in calls.get(qualname, ()):
            local_candidates = (
                f"{qualname}.{name}",
                f"{cls_prefix}.{name}" if cls_prefix else name,
                f"{name}.__init__",
                name,
            )
            targets = {
                (filepath, cand) for cand in local_candidates if cand in defs
            }
            if not name.startswith("_"):
                targets |= public_defs_by_name.get(name, set())
            for target in targets - reachable:
                reachable.add(target)
                queue.append(target)

    return {
        filepath: (
            tuple(
                sorted(
                    (lineno, qualname)
                    for lineno, qualname in raw_aborts
                    if (filepath, qualname) in reachable
                )
            ),
            skips,
        )
        for filepath, (_, _, raw_aborts, skips) in scanned.items()
    }


def find_violations(
    filepath: str, source: str
) -> tuple[Sequence[tuple[int, str]], Sequence[tuple[int, str]]]:
    """Returns `(setup_class_aborts, runtime_skips)` as `(lineno, qualname)` pairs."""
    return find_violations_in_sources({filepath: source})[filepath]


def _load_manifest() -> tuple[pathlib.Path, Set[str]]:
    raw = json.loads(
        files("test_data").joinpath("manifest.json").read_text(encoding="utf-8")
    )
    return (
        pathlib.Path(raw["source_root"]).resolve(),
        frozenset(raw["python_files"]),
    )


def verify_tracked_files_complete(
    test_case: unittest.TestCase,
    *,
    scanned_directories: Collection[str],
    self_rel_path: str,
    build_gn_path: str,
) -> None:
    """Verifies the GN-generated manifest covers all WLAN Python test files."""
    source_root, python_files = _load_manifest()
    test_case.assertTrue(
        python_files,
        f"Manifest generated from {build_gn_path} contains no Python files.",
    )
    for rel_path in python_files:
        test_case.assertTrue(
            (source_root / rel_path).is_file(),
            f"Tracked file '{rel_path}' does not exist under {source_root}.",
        )

    if (source_root / ".jiri_root").is_dir() or (
        source_root / ".fx-root"
    ).is_file():
        discovered_on_disk = frozenset(
            py_path.relative_to(source_root).as_posix()
            for rel_dir in scanned_directories
            for py_path in (source_root / rel_dir).rglob("*.py")
            if py_path.is_file()
        ) - {self_rel_path}
        test_case.assertEqual(
            discovered_on_disk,
            python_files,
            f"WLAN Python files on disk do not match the GN manifest from "
            f"{build_gn_path}. Ensure new Python files are wired into a "
            f"tracked BUILD.gn (or add new subdirectory BUILD.gn / standalone "
            f"files to {build_gn_path}). "
            f"Missing from manifest: "
            f"{sorted(discovered_on_disk - python_files)}; "
            f"Extra in manifest: "
            f"{sorted(python_files - discovered_on_disk)}.",
        )


def verify_no_banned_skips_or_aborts(
    test_case: unittest.TestCase,
    *,
    runtime_skip_allowlist: Set[tuple[str, str]],
) -> None:
    """Verifies tracked Python files have no banned skips or setup_class aborts."""
    source_root, python_files = _load_manifest()
    sources = {
        rel_path: (source_root / rel_path).read_text(encoding="utf-8")
        for rel_path in sorted(python_files)
    }
    violations_by_file = find_violations_in_sources(sources)

    abort_violations = []
    skip_violations = []
    seen_allowlisted_skips = set()

    for rel_path in sorted(python_files):
        aborts, skips = violations_by_file[rel_path]
        for lineno, qualname in aborts:
            abort_violations.append(f"{rel_path}:{lineno} ({qualname})")
        for lineno, qualname in skips:
            allowlist_key = (rel_path, qualname)
            if (
                allowlist_key in runtime_skip_allowlist
                and allowlist_key not in seen_allowlisted_skips
            ):
                seen_allowlisted_skips.add(allowlist_key)
            else:
                skip_violations.append(f"{rel_path}:{lineno} ({qualname})")

    test_case.assertSequenceEqual(
        abort_violations,
        (),
        "Do not use TestAbortClass, TestAbortAll, or asserts.abort_* in "
        "setup_class() or helpers called from it; raise signals.TestError "
        f"instead to ensure proper result reporting (b/543061752): {abort_violations}",
    )
    test_case.assertSequenceEqual(
        skip_violations,
        (),
        "Runtime test skips (asserts.skip*, TestSkip) are not allowed in "
        "WLAN E2E tests because they report green in LUCI and mask broken "
        f"testbeds. Manage testbed exclusions in GN instead: {skip_violations}",
    )
    stale_allowlist = runtime_skip_allowlist - seen_allowlisted_skips
    test_case.assertEqual(
        stale_allowlist,
        frozenset(),
        "Entries in _RUNTIME_SKIP_ALLOWLIST no longer have runtime skips "
        f"and should be removed: {sorted(stale_allowlist)}.",
    )
