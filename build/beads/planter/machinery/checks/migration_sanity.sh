#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check for GN-to-Bazel migration completeness and dual-build invariants:
# 1. skipped_convertible_target (error): flags any convertible library/binary/code target in
#    BUILD.bazel (e.g., fx_cc_library, cc_library, rustc_library, go_library, fidl_library)
#    annotated with `# @bazel2gn:skip` when BUILD.gn is present (especially reference or
#    bitrot-prevention libraries depended upon by `group("tests")` or duplicated above the
#    `## BAZEL2GN SENTINEL` marker in BUILD.gn), except package-private `testonly = True`
#    helper targets whose only consumers are Bazel-only tests in the same package, `filegroup`
#    targets (including those paired with an `inputs`-only GN `source_set`), and pre-existing
#    skipped targets in ancillary packages outside `PLANTER_TARGET_DIRS`.
# 2. duplicate_target_above_sentinel (error): flags any convertible target in BUILD.gn defined
#    above `## BAZEL2GN SENTINEL` when a target of the same name is defined in BUILD.bazel or
#    should be generated below the sentinel by `fx bazel2gn` (except `inputs`-only GN
#    `source_set` wrappers for non-code files and pre-existing pre-sentinel targets in
#    ancillary packages outside `PLANTER_TARGET_DIRS`). Partial directory migrations that leave
#    targets above the sentinel due to unmigrated external dependencies are rejected. bazel2gn
#    generates the GN copy of a Bazel test, so a hand-written GN test (`rustc_test`, `go_test`,
#    `test`, `cc_test_executable`) above the sentinel is duplicate_test_above_sentinel (error)
#    when BUILD.bazel defines the same test (name, output name or `rustc_*` `<name>_test`), or
#    gn_test_not_migrated (error) whenever a test binary target remains above the sentinel
#    without being migrated to BUILD.bazel.
# 3. gn_test_package_not_migrated / duplicate_gn_test_package (error): when the checkout has
#    `//build/bazel/rules/testing:fx_test.bzl`, flags a GN `fuchsia_unittest_package` /
#    `fuchsia_test_package` that packages C/C++ or Rust tests and uses nothing Bazel device
#    tests cannot express yet (other test_specs than log_settings.{max,min}_severity, test_type,
#    subpackages, data_deps, ...), or that duplicates a Bazel `fx_package`. Rust device tests
#    migrate the same way (explicit `.cml` with the Rust test runner shard,
#    `fx_packaged_binary` of the `rustc_*` `:<name>_test` or a `rustc_test`). Tests whose
#    language is unknown never trigger it, and test packages are not skipped when kept in GN
#    due to unmigrated external dependencies (unavoidable local exceptions must be reported via
#    `disputed_findings`). A GN package named `<bazel package>-gn` is the hand-written GN copy
#    of a Bazel test package and is not flagged.
# 4. defs_bzl_reexport_load (error): flags any BUILD.bazel `load()` from a `*:defs.bzl` file
#    (such as `//build/bazel/rules/rust:defs.bzl` or `@rules_cc//cc:defs.bzl`) of a symbol that
#    `defs.bzl` only re-exports from another `.bzl` file in the same directory. Symbols defined
#    directly in `defs.bzl` (including `//build/bazel/rules/host:defs.bzl`) or loaded from a
#    `private/` or other subdirectory are allowed.
# While a library has users in both build systems its tests run in both: tests are defined in
# Bazel and bazel2gn generates the GN copy (docs/development/build/bazel_concepts/tests.md,
# "Testing libraries in both build systems"), so tests without `# @bazel2gn:skip` are expected.
# 5. unjustified_test_skip (error): flags `# @bazel2gn:skip` on a test target or test attribute
#    (`with_unit_tests`, `with_host_unit_tests`, `test_deps`) in a sentinel-synced BUILD.bazel
#    unless one of the valid conditions holds: (T1) the target under test in the same
#    BUILD.bazel is itself skipped, (T2) `fx_unittest_package` sets `resources = [...]`,
#    (T3) a host test sets `data`/`test_data`/`test_label`, (T4) the rule is an unsupported
#    test binary rule (`cc_test`, `host_cc_test`, `host_py_test`, `host_shac_test`, `host_test`)
#    or `rustc_test` with cross-package `srcs`/`crate` or conflicting `rustc_env`, or (T5) a
#    human explanatory comment precedes `# @bazel2gn:skip` or the skip already existed at
#    PLANTER_CHANGE_BASE.
# 6. test_skip_needs_reviewer_attention (info): a test skip added in this change whose comment
#    starts with `# Needs reviewer attention (bazel2gn skip):` uses the dual-testing cost or
#    infra-incompatibility exception (e.g. `testsharder` `duplicatedShardNames` under
#    `use_test_pilot`), surfaced for a human reviewer in Gerrit to confirm (never rejected).
# 7. unjustified_test_skip (error) also covers a test skip that did not exist at
#    PLANTER_CHANGE_BASE, has no technical condition (T1-T4) and is justified only by an unmarked
#    comment: agent-added skips must use the cost marker or meet T1-T4. Skips that existed at the
#    change base (code owner annotations) stay silent.
# 8. sdk_library_test_skip (error): a new marked or commented test skip whose target under test
#    (followed through same-package references, e.g. fx_unittest_package -> fx_cc_binary ->
#    library) is an sdk_*() library other than sdk_source_set() (Bazel `idk_cc_shared_library`,
#    `idk_cc_static_library` and their `_zx` variants), whose tests stay dual-tested. When the
#    target under test lives in another package, the INFO finding says this can't be verified.
# 9. generic_test_output_name (error): flags any `rustc_test` or `host_rustc_test` in BUILD.bazel
#    whose effective GN binary output name is an undisambiguated generic name (such as `lib_test`,
#    `bin_test`, `core_test`, etc.) that collides in `${root_out_dir}` across packages.
# 10. unsanctioned_gn_construct_below_sentinel (error): a GN-only construct (exclude_toolchain_tags,
#     configs -=, disable_syslog_backend, forward_variables_from, concurrent_jobs.<pool>, or a
#     "<label>($<x>_toolchain)" label) that the change adds to a bazel2gn-generated target below the
#     sentinel without a BUILD.bazel source (annotation, native attribute or a dedicated tag the
#     checkout's bazel2gn implements; see check_gn_only_constructs_below_sentinel). Sanctioned lines
#     are accepted, so targets with such attributes can be generated instead of kept above the
#     sentinel; gn_only_attr_target_kept_above_sentinel (info) points at that recipe.
# 11. When the checkout's bazel2gn converts `fx_compiled_manifest()` to GN `cm()` and passes
#     `restricted_features` through (detected from literals in build/tools/bazel2gn/*.go),
#     `fx_compiled_manifest` is a convertible Bazel rule and `cm` a convertible GN template (with
#     the attributes bazel2gn emits: manifest, output_name, config_values_package_path,
#     restricted_features), and `restricted_features` no longer blocks converting a
#     `fuchsia_component_manifest`: checks 1 and 2 then apply to them.
# 12. dropped_preexisting_bazel_export / dropped_preexisting_bazel_target (error): when a
#     package's BUILD.bazel already existed at PLANTER_CHANGE_BASE, flags pre-existing
#     `exports_files(...)` entries (or their visibility scopes) and pre-existing named targets
#     that were dropped when adding newly migrated targets to BUILD.bazel (except when a nested
#     path is now inside a newly migrated Bazel subpackage, which forbids parent package exports
#     across the subpackage boundary and exports the file from the subpackage instead).
# 13. test_component_not_test_root / non_test_component_in_test_components (error): a new
#     `fx_test_component` that no `fx_package` lists in `test_components` (helper components use
#     `fx_component(testonly = True)`), a non-test `fx_component` listed in `fx_package`
#     `test_components` (which requires `fx_test_component`), and skipped_test_helper_component
#     (error): a skipped `fx_component` kept hand-written above the sentinel as a
#     `fuchsia_test_component` without `test_type`.
# 14. gcc_compiler_select (warning): a new `@rules_cc//cc/compiler:gcc` select() branch, which
#     never matches because Fuchsia has no GCC toolchain in Bazel.
# 15. missing_target_bazel_file (error) / target_blocked_by_unmigrated_deps (info): requires an
#     assigned target directory with BUILD.gn to define BUILD.bazel unless any convertible or
#     test target in BUILD.gn is blocked by unmigrated external packages outside CL scope or
#     GN-blocked constructs (complete-directory unblocking is required before migrating a
#     directory).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" "$SCRIPT_DIR" <<'PYEOF'
import ast
import itertools
import json
import os
import re
import subprocess
import sys

# Normalize target directory tokens (stripping leading '//' or './' and splitting comma lists).
def _norm_target_dir_token(raw_tok):
    d = (raw_tok or "").strip()
    while d.startswith("//") or d.startswith("./"):
        d = d[2:]
    d = d.strip("/")
    return "" if d == "." else d


workdir = os.path.abspath(sys.argv[1])
_td_tokens = [
    _norm_target_dir_token(tok)
    for tok in re.split(r"[\s,]+", sys.argv[2] or "")
    if _norm_target_dir_token(tok)
]
target_dir = _td_tokens[0] if _td_tokens else ""
target_dirs = list(
    dict.fromkeys(
        _norm_target_dir_token(d)
        for d in re.split(r"[\s,]+", f"{sys.argv[3]} {sys.argv[2]}")
        if _norm_target_dir_token(d)
    )
)
checks_script_dir = os.path.abspath(sys.argv[4]) if len(sys.argv) > 4 else ""


def _emit(findings, category, severity, file, line, message, remediation=""):
    d = {
        "source": "migration_sanity",
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
    }
    if remediation:
        d["remediation"] = remediation
    findings.append(d)


def _read_bazel2gn_go_sources():
    """Concatenated non-test .go sources of the checkout's build/tools/bazel2gn (feature detection)."""
    src = []
    b2gn_dir = os.path.join(workdir, "build", "tools", "bazel2gn")
    try:
        names = sorted(os.listdir(b2gn_dir))
    except OSError:
        names = []
    for fn in names:
        if fn.endswith(".go") and not fn.endswith("_test.go"):
            try:
                src.append(open(os.path.join(b2gn_dir, fn), encoding="utf-8").read())
            except OSError:
                pass
    return "\n".join(src)


BAZEL2GN_GO_SOURCES = _read_bazel2gn_go_sources()
NO_GN_EQUIV_BAZEL_RULES = set(
    re.findall(r'"([A-Za-z0-9_]+)"\s*:\s*noGNEquivalent\b', BAZEL2GN_GO_SOURCES)
)


# Extract the set of Bazel rule names supported by bazelRuleToGNTemplate in constants.go.
def _extract_known_bazel2gn_rules(go_src):
    m = re.search(
        r"var\s+bazelRuleToGNTemplate\s*=\s*map\[string\]string\s*\{(.*?)\n\}",
        go_src,
        re.S,
    )
    if not m:
        return set()
    return set(re.findall(r'"([A-Za-z0-9_]+)"\s*:', m.group(1)))


KNOWN_BAZEL2GN_RULES = _extract_known_bazel2gn_rules(BAZEL2GN_GO_SOURCES)

CONVERTIBLE_BAZEL_RULES = {
    "fx_cc_library",
    "cc_library",
    "fx_cc_library_headers",
    "cc_binary",
    "cc_shared_library_zx",
    "cc_source_library_zx",
    "cc_static_library_zx",
    "idk_cc_shared_library",
    "idk_cc_shared_library_zx",
    "idk_cc_source_library",
    "idk_cc_source_library_zx",
    "idk_cc_static_library",
    "idk_cc_static_library_zx",
    "rustc_library",
    "rust_library",
    "rustc_binary",
    "rust_binary",
    "rustc_proc_macro",
    "rust_proc_macro",
    "rustc_check_cfg_build_flags",
    "go_library",
    "go_binary",
    "py_library",
    "py_binary",
    "fidl_library",
    "zither_fidl_library",
    "fuchsia_structured_config_rust_lib",
    "fx_component",
    "fx_component_manifest",
    "fx_compiled_manifest",
    "ffx_plugin",
}

# Plain GN `shared_library()` is intentionally excluded: `build/tools/bazel2gn/constants.go`
# maps `cc_library` and `fx_cc_library` only to `static_library` or `source_set` (and
# `idk_cc_shared_library` / `cc_shared_library_zx` only to `sdk_shared_library` / `zx_library`),
# so a plain `shared_library()` in a dual-build package has no bazel2gn equivalent and must
# remain above `## BAZEL2GN SENTINEL` to preserve GN target type parity.
CONVERTIBLE_GN_TEMPLATES = {
    "static_library",
    "source_set",
    "executable",
    "library_headers",
    "rustc_library",
    "rustc_binary",
    "rustc_macro",
    "rustc_dylib",
    "go_library",
    "go_binary",
    "python_library",
    "python_binary",
    "fidl",
    "ffx_plugin",
    "ffx_tool",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
    "fuchsia_structured_config_rust_lib",
    "fuchsia_component",
    "fuchsia_component_manifest",
    "cm",
}
if "rustc_staticlib" in KNOWN_BAZEL2GN_RULES:
    CONVERTIBLE_BAZEL_RULES.update(("rustc_staticlib", "rust_static_library"))
    CONVERTIBLE_GN_TEMPLATES.add("rustc_staticlib")
# Bazel manifest rules bazel2gn converts, and their GN templates.
FX_MANIFEST_RULE_TO_GN = {
    "fx_component_manifest": "fuchsia_component_manifest",
    "fx_compiled_manifest": "cm",
}

# A hand-written GN test above the sentinel either duplicates the GN copy bazel2gn generates
# from the Bazel test or is an unmigrated GN test binary target
# (duplicate_test_above_sentinel / gn_test_not_migrated).
GN_TEST_TEMPLATES = {"rustc_test", "go_test", "test", "cc_test_executable"}

BAZEL_TEST_RULES = {
    "rustc_test",
    "go_test",
    "host_go_test",
    "host_rustc_test",
    "fx_unittest_package",
    "cc_test",
    "host_cc_test",
    "host_py_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
}
UNSUPPORTED_TEST_BINARY_RULES = {
    "cc_test",
    "host_cc_test",
    "host_py_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
}
COMMENT_REASON_RE = re.compile(r"^#(?!\s*@bazel2gn\b)\s*\S")
# Marker for a `# @bazel2gn:skip` on a test that a coder justified with exception (c) of
# docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems (dual-testing cost
# outweighs the risk). It is never rejected, but always surfaced as INFO for a human to confirm.
# Bazel rules that bazel2gn emits as GN sdk_*() libraries other than sdk_source_set() (see
# //build/tools/bazel2gn/constants.go). Their tests stay dual-tested.
SDK_DUAL_TESTED_BAZEL_RULES = {
    "idk_cc_shared_library",
    "idk_cc_shared_library_zx",
    "idk_cc_static_library",
    "idk_cc_static_library_zx",
}
# Attributes whose labels name the code a test exercises.
TUT_REF_ATTRS = {"deps", "crate", "unit_tests", "binary", "binary_name", "embed", "implementation_deps", "test_deps"}
NEEDS_ATTENTION_RE = re.compile(r"^#\s*Needs reviewer attention \(bazel2gn skip\):", re.IGNORECASE)

EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
    "third_party/rust_crates",
)

RUST_UNIT_TEST_OWNERS = {"rustc_library", "rustc_binary", "rustc_proc_macro", "rust_library", "rust_binary", "rust_proc_macro"}
UNIT_TEST_ATTRS = ("with_unit_tests", "with_host_unit_tests", "test_deps", "test_compile_data", "test_rustc_flags")

_fidl_bzl_path = os.path.join(workdir, "build/bazel/rules/fidl/fidl_library.bzl")
# fidl_library() generates rust_next bindings through fidl_rust_next_library.bzl, so look in
# both files. Only a fidl_library.bzl with neither mentioning rust_fdomain_next is the stub.
_fidl_rust_next_bzl_path = os.path.join(workdir, "build/bazel/rules/fidl/fidl_rust_next_library.bzl")


def _fidl_rust_next_unimplemented():
    if not os.path.isfile(_fidl_bzl_path):
        return False
    texts = []
    for p in (_fidl_bzl_path, _fidl_rust_next_bzl_path):
        if os.path.isfile(p):
            with open(p, encoding="utf-8") as fh:
                texts.append(fh.read())
    return not any("rust_fdomain_next" in t for t in texts)


FIDL_RUST_NEXT_UNIMPLEMENTED = _fidl_rust_next_unimplemented()
FIDL_RUST_NEXT_DEP_RE = re.compile(r":([A-Za-z0-9_.-]+_rust_(?:fdomain_)?next)\b")


_bazel_pkg_targets_cache = {}


# Parse BUILD.bazel targets and local/external label references in a package directory.
def _get_pkg_bazel_targets_map(pdir):
    if pdir in _bazel_pkg_targets_cache:
        return _bazel_pkg_targets_cache[pdir]
    bpath = os.path.join(workdir, pdir, "BUILD.bazel")
    if not os.path.isfile(bpath):
        _bazel_pkg_targets_cache[pdir] = {}
        return {}
    try:
        txt = open(bpath, encoding="utf-8").read()
        tree = ast.parse(txt, filename=bpath)
    except Exception:
        _bazel_pkg_targets_cache[pdir] = {}
        return {}
    var_strs = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            vname = node.targets[0].id
            var_strs[vname] = [
                sub.value
                for sub in ast.walk(node.value)
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
            ]
    tmap = {}
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        tname = None
        refs = []
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                tname = kw.value.value
            elif kw.arg not in (
                "visibility",
                "package_name",
                "component_name",
                "test_deps",
                "test_compile_data",
            ):
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        s = sub.value
                        if s.startswith(":") or s.startswith("//"):
                            refs.append(s)
                    elif isinstance(sub, ast.Name) and sub.id in var_strs:
                        for s in var_strs[sub.id]:
                            if s.startswith(":") or s.startswith("//"):
                                refs.append(s)
        if tname:
            tmap[tname] = (rule, refs)
    _bazel_pkg_targets_cache[pdir] = tmap
    return tmap


# Collect candidate cross-package directories for `clean_pkg`: `//` packages referenced in its
# BUILD.bazel (e.g. visibility), optionally its ancestors, its sibling directories, and
# optionally its child / grandchild directories.
def _collect_cross_pkg_dirs(clean_pkg, include_ancestors=False, include_children=False):
    cross_dirs = []
    bz_full = os.path.join(workdir, clean_pkg, "BUILD.bazel")
    if os.path.isfile(bz_full):
        try:
            bz_txt = open(bz_full, encoding="utf-8").read()
            for m in re.finditer(r'"//([A-Za-z0-9_./-]+)(?::[^"]*)?"', bz_txt):
                vp = m.group(1).strip("/")
                if vp and vp != clean_pkg and not vp.startswith("visibility"):
                    cross_dirs.append(vp)
        except OSError:
            pass
    if include_ancestors:
        anc = os.path.dirname(clean_pkg)
        while anc:
            cross_dirs.append(anc)
            next_anc = os.path.dirname(anc)
            if next_anc == anc:
                break
            anc = next_anc
    parent_dir = os.path.dirname(clean_pkg)
    parent_abs = os.path.join(workdir, parent_dir) if parent_dir else ""
    if parent_abs and os.path.isdir(parent_abs):
        try:
            for entry in sorted(os.listdir(parent_abs)):
                if entry.startswith("."):
                    continue
                sub = f"{parent_dir}/{entry}" if parent_dir else entry
                if sub != clean_pkg:
                    cross_dirs.append(sub)
        except OSError:
            pass
    clean_abs = os.path.join(workdir, clean_pkg) if clean_pkg else ""
    if include_children and clean_abs and os.path.isdir(clean_abs):
        try:
            for entry in sorted(os.listdir(clean_abs)):
                if entry.startswith("."):
                    continue
                child_rel = f"{clean_pkg}/{entry}"
                child_abs = os.path.join(clean_abs, entry)
                if os.path.isdir(child_abs):
                    cross_dirs.append(child_rel)
                    for sub_entry in sorted(os.listdir(child_abs)):
                        if not sub_entry.startswith(".") and os.path.isdir(
                            os.path.join(child_abs, sub_entry)
                        ):
                            cross_dirs.append(f"{child_rel}/{sub_entry}")
        except OSError:
            pass
    return list(dict.fromkeys(cross_dirs))


# Map each named top-level BUILD.bazel rule call to (rule, {kwarg: value_node}, lineno).
def _collect_named_top_level_calls(tree):
    named_calls = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
        ):
            kws = {kw.arg: kw.value for kw in node.value.keywords if kw.arg}
            n = kws.get("name")
            if isinstance(n, ast.Constant) and isinstance(n.value, str):
                named_calls[n.value] = (node.value.func.id, kws, node.lineno)
    return named_calls


# Extract same-package local target names (`:name`, `//pkg_dir:name`, `//pkg_dir`) referenced
# in a GN target body, ignoring comments.
def _gn_body_local_refs(raw_body, pkg_dir):
    cleaned = re.sub(r"#[^\n]*", "", raw_body)
    refs = set()
    for match in re.finditer(r'"([^"$\s(]+)"', cleaned):
        tok = match.group(1)
        if tok.startswith(":"):
            refs.add(tok[1:])
        elif pkg_dir and tok.startswith(f"//{pkg_dir}:"):
            refs.add(tok.split(":", 1)[1])
        elif pkg_dir and tok == f"//{pkg_dir}":
            refs.add(os.path.basename(pkg_dir))
    return refs


# Check whether seed targets in pdir/BUILD.bazel transitively depend on a rustc_dylib /
# rust_dylib_library target that is still dual-built in BUILD.gn (which duplicates the
# shared dylib in BlobFS across GN and Bazel packages) or lacks debug symbol collection.
def find_uncollected_rust_dylib_dep(pdir, seed_names):
    aspects_bzl = os.path.join(
        workdir, "build", "bazel", "debug_symbols", "aspects.bzl"
    )
    has_dylib_debug_symbols = False
    try:
        has_dylib_debug_symbols = (
            "rust_dylib_library" in open(aspects_bzl, encoding="utf-8").read()
        )
    except OSError:
        pass
    queue = [(pdir, name, None) for name in seed_names if name]
    visited = set()
    visited_pkgs = set()
    while queue:
        cur_pkg, cur_name, entry_lbl = queue.pop(0)
        if (cur_pkg, cur_name) in visited:
            continue
        visited.add((cur_pkg, cur_name))
        visited_pkgs.add(cur_pkg)
        if len(visited_pkgs) > 250:
            break
        tmap = _get_pkg_bazel_targets_map(cur_pkg)
        if cur_name not in tmap:
            continue
        rule, refs = tmap[cur_name]
        if rule in ("rustc_dylib", "rust_dylib_library") and (
            not has_dylib_debug_symbols
            or os.path.isfile(os.path.join(workdir, cur_pkg, "BUILD.gn"))
        ):
            return f"//{cur_pkg}:{cur_name}"
        cur_entry = entry_lbl if cur_pkg != pdir else None
        for r in refs:
            if r.startswith(":"):
                queue.insert(0, (cur_pkg, r[1:], cur_entry or f"//{cur_pkg}:{cur_name}"))
            elif r.startswith("//") and not r[2:].startswith(
                ("conditions:", "third_party/", "prebuilt/", "build/", "sdk/fidl/")
            ):
                body = r[2:]
                if ":" in body:
                    sub_pkg, sub_t = body.split(":", 1)
                else:
                    sub_pkg = body.strip("/")
                    sub_t = os.path.basename(sub_pkg)
                sub_pkg = sub_pkg.strip("/")
                if sub_pkg:
                    next_entry = cur_entry or f"//{sub_pkg}:{sub_t}"
                    queue.append((sub_pkg, sub_t, next_entry))
    return None
GN_TEST_PACKAGE_TEMPLATES = ("fuchsia_unittest_package", "fuchsia_test_package")
# Every GN template that is inherently a test package (including CTF and bootfs test packages).
GN_ALL_TEST_PACKAGE_TEMPLATES = frozenset(
    GN_TEST_PACKAGE_TEMPLATES
    + (
        "fuchsia_test",
        "ctf_fuchsia_package",
        "ctf_fuchsia_test_package",
        "ctf_fuchsia_unittest_package",
        "bootfs_test",
    )
)
# C/C++ and Rust device tests migrate (Rust via fx_packaged_binary of a rust_test).
MIGRATABLE_TEST_LANGUAGES = ("cc", "rust")
CC_TEST_BAZEL_RULES = {"fx_cc_binary", "cc_binary", "cc_test", "fx_cc_test_binary"}
GN_CC_TEST_TEMPLATES = {"executable", "test", "cc_test_executable"}
CC_SOURCE_SUFFIXES = (".cc", ".cpp", ".cxx", ".c", ".h", ".hpp")
GN_TEST_PACKAGE_BLOCKERS = (
    "test_type",
    "subpackages",
    "renameable_subpackages",
    "data_deps",
    "required_offers",
    "required_uses",
    "restricted_features",
    "deprecated_legacy_test_execution",
    "manifest_deps",
)


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


candidate_dirs = set()
if target_dir:
    candidate_dirs.add(target_dir)

def normalize_git_rev(rev):
    """Map jj relative revsets (@, @-, @--) to git HEAD equivalents for git commands."""
    rev = (rev or "").strip() or "HEAD"
    m = re.fullmatch(r"@(-*)", rev)
    if m:
        return f"HEAD~{len(m.group(1))}" if m.group(1) else "HEAD"
    return rev


changed = set()
# The task's change: everything since PLANTER_CHANGE_BASE ("HEAD~1" when HEAD is the task's own
# commit, "HEAD" when HEAD is unrelated upstream history) plus untracked files. When HEAD is
# already a [bazel_migration] commit (e.g. jj @ vs @-), also include files changed in HEAD~1.
if checks_script_dir and os.path.isdir(checks_script_dir) and checks_script_dir not in sys.path:
    sys.path.insert(0, checks_script_dir)
try:
    import planter_check_lib as _pcl

    change_base = normalize_git_rev(_pcl.resolve_change_base(workdir))
except Exception:
    change_base = normalize_git_rev(
        os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "")
    )
# Shared Fuchsia-incompatible external repository detection and remediation, also used by
# cq_reachability and build_verification so all three give one verdict.
try:
    import planter_check_lib as _pcl_ext
except ImportError:
    _pcl_ext = None
env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
if env_changed:
    changed.update(p.strip() for p in env_changed.splitlines() if p.strip())
else:
    changed.update(git_lines(["diff", "--name-only", change_base]))
    changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
head_subj = git_lines(["log", "-1", "--format=%s", "HEAD"])
head_body = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
head_is_merged_cl = bool(re.search(r"(?m)^Reviewed-on:\s*\S", head_body))
parent_subj = git_lines(["log", "-1", "--format=%s", "HEAD~1"])
mig_prefixes = ("[bazel_migration]", "[bazel2gn]", "[bazel]")
if head_subj and head_subj[0].startswith(mig_prefixes) and not head_is_merged_cl:
    head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
    # Match HEAD's files against every assigned target directory (multi-directory batches).
    if not target_dirs or any(
        any(p == td or p.startswith(td + "/") for td in target_dirs)
        for p in head_files
    ):
        # Step back to HEAD~1 when the working tree has no changes of its own (HEAD is the
        # task commit, whatever change_base resolved to), when change_base is literal HEAD,
        # or when HEAD's parent is not itself a migration commit.
        if (
            not changed
            or change_base == "HEAD"
            or (parent_subj and not parent_subj[0].startswith(mig_prefixes))
        ):
            change_base = "HEAD~1"
        if not changed:
            changed.update(head_files)
all_changed_build_dirs = set()
for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if d:
            all_changed_build_dirs.add(d)
        if (
            d
            and d not in ("tools", "src", "sdk", "build")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
        ):
            candidate_dirs.add(d)


_changed_lines_cache = {}


def get_file_added_lines(rel_path):
    """Returns a set of 1-based line numbers added or modified in rel_path relative to change_base.

    Returns None if the file did not exist at change_base (i.e. every line is new) or if
    change_base cannot be resolved.
    """
    if rel_path in _changed_lines_cache:
        return _changed_lines_cache[rel_path]
    if not change_base:
        _changed_lines_cache[rel_path] = None
        return None
    try:
        subprocess.check_call(
            ["git", "-C", workdir, "cat-file", "-e", f"{change_base}:{rel_path}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        _changed_lines_cache[rel_path] = None
        return None

    diff_lines = git_lines(["diff", "-U0", change_base, "--", rel_path])
    added_lines = set()
    hunk_re = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
    for dl in diff_lines:
        m = hunk_re.match(dl)
        if m:
            start_ln = int(m.group(1))
            count = int(m.group(2)) if m.group(2) is not None else 1
            for ln in range(start_ln, start_ln + count):
                added_lines.add(ln)
    _changed_lines_cache[rel_path] = added_lines
    return added_lines


def parse_bazel_targets(bazel_path, pkg_dir="", text=None):
    if text is None:
        try:
            text = open(bazel_path, encoding="utf-8").read()
        except Exception:
            return []
    lines = text.splitlines()
    targets = []
    try:
        tree = ast.parse(text, filename=bazel_path)
    except Exception:
        return []
    for node in tree.body:
        call = None
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            call = node.value
        if not call or not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        tname = None
        binary_ref = None
        pkg_name = None
        unit_tests = False
        unit_tests_refs = []
        host_only = False
        testonly = False
        vis_items = []
        dep_refs = set()
        gn_dep_refs = set()
        ext_tut_refs = set()
        is_test_pkg = rule == "fx_unittest_package"
        has_test_components = False
        has_resources = False
        has_test_data = False
        has_rustc_env = False
        crate_ref = None
        cross_pkg_srcs_or_crate = False
        local_test_refs = []
        skipped_test_attrs = []
        attention_test_attrs = []
        for kw in call.keywords:
            if kw.arg == "target_compatible_with":
                tcw_seg = ast.get_source_segment(text, kw.value) or ""
                if "HOST_OS_CONSTRAINTS" in tcw_seg or "HOST_CONSTRAINTS" in tcw_seg:
                    host_only = True
            if kw.arg == "test_components" and not (
                isinstance(kw.value, ast.List) and not kw.value.elts
            ):
                is_test_pkg = True
                if not has_skip_comment(lines, kw.lineno) and (
                    not isinstance(kw.value, ast.List)
                    or any(not has_skip_comment(lines, elt.lineno) for elt in kw.value.elts)
                ):
                    has_test_components = True
            if (
                kw.arg == "testonly"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value is True
            ):
                is_test_pkg = True
            attr_skipped = kw.arg in UNIT_TEST_ATTRS and has_skip_comment(lines, kw.lineno)
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                tname = kw.value.value
            elif kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
                testonly = True
            elif kw.arg == "visibility" and isinstance(kw.value, ast.List):
                for elt in kw.value.elts:
                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                        vis_items.append(elt.value)
            elif kw.arg == "binary" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                binary_ref = kw.value.value.lstrip(":")
            elif kw.arg == "package_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                pkg_name = kw.value.value
            elif kw.arg == "resources":
                has_resources = True
            elif kw.arg in ("data", "test_data", "test_label"):
                has_test_data = True
            elif kw.arg == "rustc_env":
                has_rustc_env = True
            elif kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
                testonly = True
            elif kw.arg == "crate":
                crate_seg = ast.get_source_segment(text, kw.value) or ""
                if '"//' in crate_seg:
                    cross_pkg_srcs_or_crate = True
                if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    if kw.value.value.startswith(":"):
                        crate_ref = kw.value.value[1:]
                        local_test_refs.append(crate_ref)
            elif kw.arg == "srcs":
                srcs_seg = ast.get_source_segment(text, kw.value) or ""
                if '"//' in srcs_seg:
                    cross_pkg_srcs_or_crate = True
            elif kw.arg == "unit_tests":
                if isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            unit_tests_refs.append(elt.value.lstrip(":"))
                            if elt.value.startswith(":"):
                                local_test_refs.append(elt.value[1:])
                elif isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    unit_tests_refs.append(kw.value.value.lstrip(":"))
                    if kw.value.value.startswith(":"):
                        local_test_refs.append(kw.value.value[1:])
            elif kw.arg in ("with_unit_tests", "with_host_unit_tests") and isinstance(kw.value, ast.Constant):
                unit_tests = unit_tests or kw.value.value is True or kw.value.value in ("fuchsia", "host", "both")
            if kw.arg not in ("name", "visibility", "package_name"):
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        sval = sub.value
                        local_ref = None
                        if sval.startswith(":"):
                            local_ref = sval[1:]
                        elif pkg_dir and sval.startswith(f"//{pkg_dir}:"):
                            local_ref = sval.split(":", 1)[1]
                        elif sval.startswith("//") and kw.arg in TUT_REF_ATTRS:
                            ext_tut_refs.add(sval)
                        if local_ref:
                            dep_refs.add(local_ref)
                            if not attr_skipped and not has_skip_comment(lines, sub.lineno):
                                gn_dep_refs.add(local_ref)
            if kw.arg in ("embed", "deps"):
                dep_seg = ast.get_source_segment(text, kw.value) or ""
                local_test_refs.extend(re.findall(r'":([^"]+)"', dep_seg))
            if kw.arg in UNIT_TEST_ATTRS:
                attr_skip = None
                attr_reason = False
                attr_attention = False
                if 1 <= kw.lineno <= len(lines) and "@bazel2gn:skip" in lines[kw.lineno - 1]:
                    attr_skip = kw.lineno
                kidx = kw.lineno - 2
                while kidx >= node.lineno - 1:
                    ks = lines[kidx].strip()
                    if not ks:
                        kidx -= 1
                        continue
                    if ks.startswith("#"):
                        if "@bazel2gn:skip" in ks:
                            attr_skip = kidx + 1
                        if COMMENT_REASON_RE.search(ks):
                            attr_reason = True
                        if NEEDS_ATTENTION_RE.search(ks):
                            attr_attention = True
                        kidx -= 1
                        continue
                    break
                if attr_skip is not None:
                    skipped_test_attrs.append((kw.arg, attr_skip, attr_reason))
                    if attr_attention:
                        attention_test_attrs.append((kw.arg, attr_skip))
        if not tname:
            continue
        start_line = node.lineno
        skip_line = None
        has_skip_reason = False
        has_attention_marker = False
        idx = start_line - 2
        while idx >= 0:
            s = lines[idx].strip()
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    skip_line = idx + 1
                if COMMENT_REASON_RE.search(s):
                    has_skip_reason = True
                if NEEDS_ATTENTION_RE.search(s):
                    has_attention_marker = True
                idx -= 1
                continue
            break
        targets.append({
            "rule": rule,
            "name": tname,
            "line": start_line,
            "skip_line": skip_line,
            "has_skip_reason": has_skip_reason,
            "has_attention_marker": has_attention_marker,
            "binary_ref": binary_ref,
            "package_name": pkg_name or tname,
            "unit_tests": unit_tests,
            "unit_tests_refs": unit_tests_refs,
            "host_only": host_only,
            "testonly": testonly,
            "vis_items": vis_items,
            "dep_refs": dep_refs,
            "ext_tut_refs": ext_tut_refs,
            "gn_dep_refs": gn_dep_refs,
            "is_test_pkg": is_test_pkg,
            "has_test_components": has_test_components,
            "has_resources": has_resources,
            "has_test_data": has_test_data,
            "has_rustc_env": has_rustc_env,
            "crate_ref": crate_ref,
            "cross_pkg_srcs_or_crate": cross_pkg_srcs_or_crate,
            "local_test_refs": local_test_refs,
            "skipped_test_attrs": skipped_test_attrs,
            "attention_test_attrs": attention_test_attrs,
            # Source text of the call, used to find external-repository deps per target.
            "raw": ast.get_source_segment(text, call) or "",
        })
    return targets


# Check whether a convertible Bazel target is a helper or sub-library
# whose only consumers in BUILD.bazel are Bazel-only targets (leaving any
# bazel2gn-generated GN target orphaned in BUILD.gn).
def has_technical_test_skip_condition(t, targets_by_name):
    """T1-T4 of unjustified_test_skip: a skip that bazel2gn limits or Bazel-only code require."""
    rule = t["rule"]
    tut_refs = [t["crate_ref"]] if t.get("crate_ref") else t.get("local_test_refs", [])
    local_refs = [r for r in tut_refs if r in targets_by_name]
    t1_tut_skipped = bool(local_refs) and all(targets_by_name[r]["skip_line"] is not None for r in local_refs)
    t2_fx_unittest_resources = rule == "fx_unittest_package" and t.get("has_resources")
    t3_host_test_data = rule in ("host_go_test", "go_test", "host_rustc_test") and t.get("has_test_data")
    t4_unsupported_rule = rule in UNSUPPORTED_TEST_BINARY_RULES or (
        rule == "rustc_test"
        and (
            t.get("cross_pkg_srcs_or_crate")
            or (
                t.get("has_rustc_env")
                and t.get("crate_ref") in targets_by_name
                and targets_by_name[t["crate_ref"]].get("has_rustc_env")
            )
        )
    )
    return bool(t1_tut_skipped or t2_fx_unittest_resources or t3_host_test_data or t4_unsupported_rule)


def resolve_targets_under_test(seed_names, targets_by_name):
    """Follows same-package references from a test to the target under test.

    Expands test and testonly targets (package -> test binary -> ...) and stops at the first
    non-test, non-testonly target: that is the target under test, and its own deps are not.
    Returns (sdk_dual_tested_targets, local_library_targets, external_refs).
    """
    seen = set()
    queue = list(seed_names)
    sdk, libs, ext = set(), set(), set()
    while queue:
        name = queue.pop()
        if name in seen or name not in targets_by_name:
            continue
        seen.add(name)
        tgt = targets_by_name[name]
        if tgt["rule"] not in BAZEL_TEST_RULES and not tgt.get("testonly"):
            # The target under test (or, for a skipped test attribute, the seed library itself).
            if tgt["rule"] in SDK_DUAL_TESTED_BAZEL_RULES:
                sdk.add(name)
            if name not in seed_names:
                libs.add(name)
            continue
        ext.update(tgt.get("ext_tut_refs", ()))
        refs = set(tgt.get("dep_refs", ())) | set(tgt.get("local_test_refs", ())) | set(tgt.get("unit_tests_refs", ()))
        for extra in (tgt.get("crate_ref"), tgt.get("binary_ref")):
            if extra:
                refs.add(extra)
        queue.extend(refs)
    return sorted(sdk), sorted(libs), sorted(ext)


def is_bazel_only_private_test_dep(
    target, bazel_targets, gn_info, pkg_dir, packaged_binaries, gn_kept_pkg_test_deps=()
):
    tname = target["name"]
    vis = target.get("vis_items") or []
    priv_vis = {"//visibility:private", ":__pkg__", f"//{pkg_dir}:__pkg__"}
    if not (target["skip_line"] is not None and target.get("has_skip_reason")) and (
        not target.get("testonly") or not vis or not all(v in priv_vis for v in vis)
    ):
        return False
    if tname in gn_kept_pkg_test_deps:
        return False
    if gn_info:
        if tname in gn_info["pre_targets"]:
            return False
        ref_pat = re.compile(
            r'"(?::|//'
            + re.escape(pkg_dir)
            + r':)'
            + re.escape(tname)
            + r'(?:_[A-Za-z0-9_]+)?(?:"|\()'
        )
        for ptmpl, _, pbody in gn_info["pre_targets"].values():
            if (
                ptmpl
                not in (
                    "bazel_test_suite",
                    "bazel_host_tool",
                    "bazel_ffx_tool",
                    "bazel_fuchsia_package",
                )
                and ref_pat.search(pbody)
            ):
                return False
    has_bazel_test_consumer = False
    for other in bazel_targets:
        if other["name"] == tname:
            continue
        is_gn_converted_consumer = (
            other["skip_line"] is None
            and (other["rule"] in CONVERTIBLE_BAZEL_RULES or other["rule"] in BAZEL_TEST_RULES)
            and (other["name"] not in packaged_binaries or other["rule"] in BAZEL_TEST_RULES)
        )
        if is_gn_converted_consumer and any(
            r == tname or r.startswith(tname + "_")
            for r in other.get("gn_dep_refs", ())
        ):
            return False
        if any(
            r == tname or r.startswith(tname + "_")
            for r in other.get("dep_refs", ())
        ):
            has_bazel_test_consumer = True
    return has_bazel_test_consumer


# GN-only target attributes and constructs that bazel2gn does not translate from a plain
# BUILD.bazel attribute. A convertible target using them may stay hand-written above the
# sentinel, but it may also be generated below it when BUILD.bazel names them with a sanctioned
# annotation or tag (`# @bazel2gn:raw_attribute:`, `raw_overwrite`, `value_overwrite`,
# `path_overwrite`, tags such as `exclude_toolchain_tags=<tag>` or
# `assert_no_deps_current_toolchain=<label>`): see check_gn_only_constructs_below_sentinel.
GN_ONLY_ATTR_RE = re.compile(
    r"(?<![\w.])(?:exclude_toolchain_tags\s*\+?=|configs\s*-=|disable_syslog_backend\s*=|forward_variables_from\s*\(|concurrent_jobs\.)"
)
GN_BLOCKED_CONSTRUCT_RE = re.compile(
    # bazel2gn translates fuchsia_structured_config_rust_lib (//build/tools/bazel2gn/constants.go),
    # so only the other structured config templates are GN-only.
    r"\b(?:loadable_module|network_unified_binary|fuchsia_structured_config_(?!rust_lib\b)[a-z0-9_]+)\s*\("
    r"|\(\$[a-zA-Z0-9_]*toolchain\)"
)
GN_SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)


def gn_hand_written_text(text):
    """The part of a BUILD.gn above the bazel2gn sentinel (all of it without a sentinel).

    GN-only constructs below the sentinel were generated from BUILD.bazel annotations, so they
    show that the package is already migrated rather than that it is blocked on GN-only code.
    """
    m = GN_SENTINEL_RE.search(text)
    return text[: m.start()] if m else text


# Check whether a pre-sentinel GN component or manifest target is consumed only by a
# testonly fuchsia_package kept above the sentinel.
def is_packaged_testonly_gn_binary(gname, gtmpl, gbody, pre_targets, bazel_names):
    if gname in bazel_names:
        return False
    only_test_pkg_tmpls = (
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "bootfs_test",
    )
    if gtmpl in (
        "fuchsia_component",
        "fuchsia_component_manifest",
        "fuchsia_structured_config_rust_lib",
    ):
        visited_c = {gname}
        queue_c = [gname]
        saw_test_or_blocked_consumer = False
        saw_non_test_consumer = False
        while queue_c:
            cur_c = queue_c.pop(0)
            c_ref_re = (
                re.compile(
                    r'"(?::|//' + re.escape(target_dir) + r':)' + re.escape(cur_c) + r'(?:"|\()'
                )
                if target_dir
                else re.compile(r'":' + re.escape(cur_c) + r'(?:"|\()')
            )
            c_short_re = re.compile(r'":' + re.escape(cur_c) + r'(?:"|\()')
            for pname, (ptmpl, _, pbody) in pre_targets.items():
                if pname in visited_c or not (c_short_re.search(pbody) or c_ref_re.search(pbody)):
                    continue
                visited_c.add(pname)
                # A testonly fuchsia_package kept above the sentinel (e.g. a subpackage of a GN
                # test package) is a test consumer: bazel2gn never translates a non-test
                # fx_package, so its components stay hand-written next to it.
                if ptmpl in (
                    "fuchsia_package",
                    "fuchsia_package_with_single_component",
                ) and re.search(r"\btestonly\s*=\s*true\b", pbody):
                    saw_test_or_blocked_consumer = True
                elif ptmpl in ("fuchsia_package", "fuchsia_package_with_single_component"):
                    saw_non_test_consumer = True
                elif (
                    ptmpl in only_test_pkg_tmpls
                    or re.search(r"\btestonly\s*=\s*true\b", pbody)
                    or ptmpl.startswith("fuchsia_structured_config_values")
                ):
                    saw_test_or_blocked_consumer = True
                elif pname in bazel_names:
                    saw_non_test_consumer = True
                else:
                    queue_c.append(pname)
        if saw_test_or_blocked_consumer and not saw_non_test_consumer:
            return True
    return False


_EXT_COMPONENT_DEP_CACHE = {}


# Check whether an external GN label resolves to a Bazel component-packageable target
# (`fx_packaged_binary`, `resource`, or `fuchsia_package_resource_group`) that `bazel2gn`
# passes through verbatim in `fx_component(deps = [...])`.
def _has_cross_pkg_bazel_component_dep(d_lbl):
    if not d_lbl.startswith("//") or "$" in d_lbl:
        return False
    raw = d_lbl[2:].split("(", 1)[0]
    ext_pkg, sep, ext_target = raw.partition(":")
    ext_pkg = ext_pkg.strip("/")
    if not sep:
        ext_target = os.path.basename(ext_pkg)
    if not ext_pkg or not ext_target:
        return False
    if ext_pkg not in _EXT_COMPONENT_DEP_CACHE:
        rules_by_name = {}
        ext_bz = os.path.join(workdir, ext_pkg, "BUILD.bazel")
        try:
            ext_tree = ast.parse(open(ext_bz, encoding="utf-8").read(), filename=ext_bz)
            for node in ext_tree.body:
                if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
                    continue
                call = node.value
                fn = call.func.id if isinstance(call.func, ast.Name) else ""
                for kw in call.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        rules_by_name[kw.value.value] = fn
        except Exception:
            rules_by_name = {}
        _EXT_COMPONENT_DEP_CACHE[ext_pkg] = rules_by_name
    return _EXT_COMPONENT_DEP_CACHE[ext_pkg].get(ext_target) in {
        "fx_packaged_binary",
        "resource",
        "fuchsia_package_resource_group",
    }


# Check whether a pre-sentinel GN `fuchsia_component_manifest`, `fuchsia_component` or (when the
# checkout's bazel2gn converts `fx_compiled_manifest()`) `cm` uses attributes, dependencies, or
# pre-sentinel `get_target_outputs()` consumers that `bazel2gn` cannot convert.
def is_gn_component_target_blocked(
    gname, gtmpl, gbody, pre_targets, bazel_targets, pdir, _check_consumers=True
):
    if gtmpl not in ("fuchsia_component_manifest", "fuchsia_component", "cm"):
        return False
    cleaned = re.sub(r"#[^\n]*", "", gbody)
    if (
        "$" in cleaned
        or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
        or GN_ONLY_ATTR_RE.search(cleaned)
    ):
        return True
    attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", cleaned))
    lbl_alt = (
        r"(?::"
        + re.escape(gname)
        + (r"|//" + re.escape(pdir) + r":" + re.escape(gname) if pdir else "")
        + r")"
    )
    if gtmpl in ("fuchsia_component_manifest", "cm"):
        if gtmpl == "cm":
            allowed = {
                "manifest",
                "output_name",
                "config_values_package_path",
                "restricted_features",
                "testonly",
                "visibility",
            }
        else:
            allowed = {
                "manifest",
                "component_name",
                "restricted_features",
                "testonly",
                "visibility",
            }
        if not attrs or not (attrs <= allowed) or "manifest" not in attrs:
            return True
        if not _check_consumers:
            return False
        cm_ref_re = re.compile(rf'\bcm_label\s*=\s*"{lbl_alt}"')
        gto_ref_re = re.compile(rf'\bget_target_outputs\(\s*"{lbl_alt}"\s*\)')
        for pname, (ptmpl, _, pbody) in pre_targets.items():
            if pname == gname:
                continue
            pclean = re.sub(r"#[^\n]*", "", pbody)
            uses_gto = bool(gto_ref_re.search(pclean)) or (
                (
                    ptmpl in ("fuchsia_component", "fuchsia_test_component")
                    or ptmpl.startswith("fuchsia_structured_config_")
                )
                and bool(cm_ref_re.search(pclean))
            )
            if not uses_gto:
                continue
            if ptmpl == "fuchsia_component":
                if is_gn_component_target_blocked(
                    pname,
                    ptmpl,
                    pbody,
                    pre_targets,
                    bazel_targets,
                    pdir,
                    _check_consumers=False,
                ):
                    return True
            elif ptmpl == "fuchsia_structured_config_rust_lib":
                if (
                    "$" in pclean
                    or re.search(r"\b(?:if|foreach)\s*\(", pclean)
                    or GN_ONLY_ATTR_RE.search(pclean)
                ):
                    return True
            else:
                return True
        return False
    allowed = {
        "cm_label",
        "manifest",
        "component_name",
        "check_includes",
        "deps",
        "testonly",
        "visibility",
    }
    if not attrs or not (attrs <= allowed) or not (attrs & {"cm_label", "manifest"}):
        return True
    if _check_consumers:
        tc_item_re = re.compile(rf'"{lbl_alt}"')
        comp_attr_re = re.compile(rf'(?<![\w.])component\s*=\s*"{lbl_alt}"')
        gto_ref_re = re.compile(rf'\bget_target_outputs\(\s*"{lbl_alt}"\s*\)')
        for pname, (ptmpl, _, pbody) in pre_targets.items():
            if pname == gname:
                continue
            pclean = re.sub(r"#[^\n]*", "", pbody)
            if gto_ref_re.search(pclean):
                return True
            if ptmpl == "fuchsia_test" and comp_attr_re.search(pclean):
                return True
            if ptmpl == "fuchsia_test_package":
                for tcm in re.finditer(r"\btest_components\s*\+?=\s*\[([^\]]*)\]", pclean):
                    if tc_item_re.search(tcm.group(1)):
                        return True
    cm_m = re.search(r'\bcm_label\s*=\s*"([^"]+)"', cleaned)
    if cm_m:
        cm_lbl = cm_m.group(1)
        cm_local = (
            cm_lbl[1:]
            if cm_lbl.startswith(":")
            else (
                cm_lbl.split(":", 1)[1]
                if pdir and cm_lbl.startswith(f"//{pdir}:")
                else ""
            )
        )
        if not cm_local:
            return True
        if cm_local in pre_targets:
            mtmpl, _, mbody = pre_targets[cm_local]
            if mtmpl != "fuchsia_component_manifest" or is_gn_component_target_blocked(
                cm_local,
                mtmpl,
                mbody,
                pre_targets,
                bazel_targets,
                pdir,
                _check_consumers=False,
            ):
                return True
    bz_bin_rules = {
        "rustc_binary",
        "rust_binary",
        "cc_binary",
        "fx_cc_binary",
        "fx_cc_test_binary",
        "go_binary",
        "cc_binary_host_tool",
        "go_binary_host_tool",
        "idk_cc_binary_host_tool",
        "idk_go_binary_host_tool",
        "idk_rustc_binary_host_tool",
        "rustc_test",
        "fx_packaged_binary",
    }
    bz_map = {t["name"]: t for t in bazel_targets}
    gn_bin_tmpls = {
        "rustc_binary",
        "executable",
        "go_binary",
        "renamed_binary",
        "rustc_test",
    }
    for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", cleaned):
        for d_lbl in re.findall(r'"([^"]+)"', dm.group(1)):
            d_local = (
                d_lbl[1:]
                if d_lbl.startswith(":")
                else (
                    d_lbl.split(":", 1)[1]
                    if pdir and d_lbl.startswith(f"//{pdir}:")
                    else ""
                )
            )
            if not d_local:
                if _has_cross_pkg_bazel_component_dep(d_lbl):
                    continue
                return True
            if (
                d_local in bz_map
                and bz_map[d_local]["rule"] in bz_bin_rules
                and bz_map[d_local]["skip_line"] is None
            ):
                continue
            if (
                d_local in pre_targets
                and pre_targets[d_local][0] in gn_bin_tmpls
                and not GN_ONLY_ATTR_RE.search(pre_targets[d_local][2])
            ):
                continue
            return True
    return False


# Check whether a dependency's BUILD.bazel defines the target directly
# or via a macro that generates <base>_<suffix> sub-targets (such as
# fidl_library language bindings), rather than unrelated exports_files.
# Packages where both BUILD.gn and a partial or exports_files-only BUILD.bazel
# exist (such as //src/devices/bin/driver_runtime:driver_runtime when BUILD.bazel
# only has exports_files) still count as unmigrated when the referenced target is
# defined pre-sentinel in BUILD.gn. Legacy OOT BUILD.bazel files that depend on
# @fuchsia_sdk// while their BUILD.gn has no BAZEL2GN SENTINEL or bazel_* bridge
# are not usable under --config=fuchsia_platform and count as unmigrated.
def _bazel_pkg_has_target(dep_bz_txt, dep_target, dep_gn_path):
    if "@fuchsia_sdk//" in dep_bz_txt and os.path.isfile(dep_gn_path):
        try:
            gn_raw = open(dep_gn_path, encoding="utf-8").read()
            if not any(
                tok in gn_raw
                for tok in (
                    "BAZEL2GN SENTINEL",
                    "bazel_test_suite",
                    "bazel_fuchsia_package",
                    "bazel_host_tool",
                    "bazel_ffx_tool",
                )
            ):
                return False
        except Exception:
            pass
    norm_target = dep_target.replace("-", "_")
    bz_names = [
        m.group(1)
        for m in re.finditer(r'\bname\s*=\s*"([^"]+)"', dep_bz_txt)
    ]
    gn_defined = set()
    if os.path.isfile(dep_gn_path):
        try:
            gn_pre = open(dep_gn_path, encoding="utf-8").read().split(
                "BAZEL2GN SENTINEL"
            )[0]
            gn_defined = {
                m.group(1)
                for m in re.finditer(
                    r'\b(?!bazel_)[a-zA-Z_][a-zA-Z0-9_]*\s*\(\s*"([^"]+)"\s*\)',
                    gn_pre,
                )
            }
        except Exception:
            pass
    if any(
        n == dep_target or n.replace("-", "_") == norm_target
        for n in bz_names
    ):
        return True
    if (
        dep_target.endswith("_rust_next")
        or dep_target.endswith("_rust_fdomain_next")
        or dep_target in gn_defined
    ):
        return False
    return any(
        dep_target.startswith(n + "_")
        or norm_target.startswith(n.replace("-", "_") + "_")
        for n in bz_names
    )


# Inspect a pre-sentinel GN target's local and external dependencies for unmigrated
# packages, declare_args() packages, and GN-only constructs that block Bazel conversion.
def inspect_gn_target_deps(gbody, pkg_dir, pre_targets, bazel_names):
    unmigrated_deps = []
    declare_args_deps = []
    gn_blocked_dep = False
    vis_re = re.compile(r"\bvisibility\s*\+?=\s*\[[^\]]*\]")
    local_ref_re = re.compile(
        r'"(?::([A-Za-z0-9_.-]+)|//'
        + re.escape(pkg_dir)
        + r'(?::([A-Za-z0-9_.-]+))?)"'
    )
    seen_local = set()
    body_queue = [gbody]
    # Resolve a local GN label to its owning pre-sentinel target key, including `<owner>_test`
    # unit-test labels and `<owner>.<subtarget>` subtargets synthesized by custom GN templates.
    def _resolve_pre_sentinel_target_key(local_name, pre_map):
        if local_name in pre_map:
            return local_name
        stripped_test = re.sub(r"_test$", "", local_name)
        if stripped_test in pre_map:
            return stripped_test
        if "." in local_name:
            base_owner = local_name.split(".", 1)[0]
            if base_owner in pre_map:
                return base_owner
        return local_name

    while body_queue:
        cur_body = vis_re.sub("", re.sub(r"#[^\n]*", "", body_queue.pop(0)))
        for lm in local_ref_re.finditer(cur_body):
            lname = lm.group(1) or lm.group(2) or os.path.basename(pkg_dir)
            if lname in seen_local or lname in bazel_names:
                continue
            target_key = _resolve_pre_sentinel_target_key(lname, pre_targets)
            if (
                target_key in seen_local
                or (lname == target_key and target_key in bazel_names)
                or target_key not in pre_targets
            ):
                continue
            seen_local.add(lname)
            seen_local.add(target_key)
            ltmpl, _, lbody = pre_targets[target_key]
            if (
                GN_ONLY_ATTR_RE.search(lbody)
                or GN_BLOCKED_CONSTRUCT_RE.search(lbody)
                or (
                    ltmpl not in CONVERTIBLE_GN_TEMPLATES
                    and ltmpl not in GN_TEST_TEMPLATES
                    and ltmpl not in GN_CC_TEST_TEMPLATES
                    and ltmpl not in GN_TEST_PACKAGE_TEMPLATES
                    and ltmpl not in ("group", "fuchsia_test_component", "fuchsia_unittest_component")
                )
            ):
                gn_blocked_dep = True
            else:
                body_queue.append(lbody)

        for dm in re.finditer(r'"//([^":(\s]+)(?::([^"(\s]+))?"', cur_body):
            dep_pkg = dm.group(1).strip("/")
            dep_target = dm.group(2) or os.path.basename(dep_pkg)
            if (
                not dep_pkg
                or dep_pkg == pkg_dir
                or dep_pkg.startswith(("build/", "prebuilt/", "third_party/", "out/"))
            ):
                continue
            dep_gn = os.path.join(workdir, dep_pkg, "BUILD.gn")
            dep_bazel = os.path.join(workdir, dep_pkg, "BUILD.bazel")
            has_bazel_target = False
            if os.path.isfile(dep_bazel):
                try:
                    dep_bz_txt = open(dep_bazel, encoding="utf-8").read()
                    has_bazel_target = _bazel_pkg_has_target(
                        dep_bz_txt, dep_target, dep_gn
                    )
                except Exception:
                    has_bazel_target = True
            if os.path.isfile(dep_gn) and not has_bazel_target:
                if f"//{dep_pkg}" not in unmigrated_deps:
                    unmigrated_deps.append(f"//{dep_pkg}")
                try:
                    dep_gn_txt = open(dep_gn, encoding="utf-8").read()
                    if "declare_args(" in dep_gn_txt and f"//{dep_pkg}" not in declare_args_deps:
                        declare_args_deps.append(f"//{dep_pkg}")
                    dep_hand_txt = gn_hand_written_text(dep_gn_txt)
                    if GN_ONLY_ATTR_RE.search(dep_hand_txt) or GN_BLOCKED_CONSTRUCT_RE.search(
                        dep_hand_txt
                    ):
                        gn_blocked_dep = True
                    else:
                        sub_refs = [
                            (m.group(1).strip("/"), m.group(2) or os.path.basename(m.group(1).strip("/")))
                            for m in re.finditer(
                                r'"//([^":(\s]+)(?::([^"(\s]+))?"', dep_gn_txt
                            )
                        ]
                        # Also resolve relative subpackage labels in pre-sentinel GN targets
                        # (e.g. a hand-written group bundling an unmigrated child component).
                        for dm_rel in re.finditer(
                            r"\b(?:public_)?deps\s*\+?=\s*\[([^\]]*)\]",
                            re.sub(r"#[^\n]*", "", dep_hand_txt),
                        ):
                            for rm in re.finditer(
                                r'"([A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*)(?::([^"(\s]+))?"',
                                dm_rel.group(1),
                            ):
                                rel_sub = os.path.normpath(
                                    os.path.join(dep_pkg, rm.group(1).strip("/"))
                                ).replace("\\", "/")
                                sub_refs.append((rel_sub, rm.group(2) or os.path.basename(rel_sub)))
                        for sub_pkg, sub_target in dict.fromkeys(sub_refs):
                            sub_gn = os.path.join(workdir, sub_pkg, "BUILD.gn")
                            sub_bazel = os.path.join(workdir, sub_pkg, "BUILD.bazel")
                            sub_has_bazel = False
                            if os.path.isfile(sub_bazel):
                                try:
                                    sub_has_bazel = _bazel_pkg_has_target(
                                        open(sub_bazel, encoding="utf-8").read(),
                                        sub_target,
                                        sub_gn,
                                    )
                                except Exception:
                                    sub_has_bazel = True
                            if os.path.isfile(sub_gn) and not sub_has_bazel:
                                sub_txt = open(sub_gn, encoding="utf-8").read()
                                if (
                                    os.path.isfile(dep_bazel)
                                    or GN_ONLY_ATTR_RE.search(sub_txt)
                                    or GN_BLOCKED_CONSTRUCT_RE.search(sub_txt)
                                    or re.search(
                                        r"\b(?:fuchsia_component|sdk_fuchsia_package)\s*\(",
                                        sub_txt,
                                    )
                                ):
                                    gn_blocked_dep = True
                                    break
                except Exception:
                    pass
    return unmigrated_deps, declare_args_deps, gn_blocked_dep


def has_skip_comment(lines, lineno):
    """Whether the 1-based line has a trailing `# @bazel2gn:skip` or the line above is one."""
    if 0 < lineno <= len(lines) and "#" in lines[lineno - 1]:
        if lines[lineno - 1].split("#", 1)[1].strip() == "@bazel2gn:skip":
            return True
    return lineno >= 2 and lines[lineno - 2].strip() == "# @bazel2gn:skip"


def find_closing_delim(text, open_idx, open_ch="{", close_ch="}"):
    depth, in_str, i, n = 0, False, open_idx, len(text)
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == "#":
            nl = text.find("\n", i)
            i = n if nl < 0 else nl
            continue
        elif c == open_ch:
            depth += 1
        elif c == close_ch:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return n


def find_closing_brace(text, open_idx):
    return find_closing_delim(text, open_idx, "{", "}")


def parse_gn_text(text):
    sentinel_re = GN_SENTINEL_RE
    m = sentinel_re.search(text)
    has_sentinel = bool(m)
    sentinel_line = (text.count("\n", 0, m.start()) + 1) if m else 1
    pre_text = text[: m.start()] if m else text
    post_text = text[m.end() :] if m else ""
    pre_targets = {}
    post_targets = {}
    # Match horizontal leading indentation only so `tm.start(1)` is on the target's
    # declaration line rather than a preceding blank line (`\s` matches `\n` under `re.M`).
    target_re = re.compile(r'^[ \t]*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for tm in target_re.finditer(pre_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = pre_text.count("\n", 0, tm.start(1)) + 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(pre_text, brace_idx)
        body = pre_text[brace_idx + 1 : end_idx]
        pre_targets[name] = (tmpl, line_no, body)
    for tm in target_re.finditer(post_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = (text.count("\n", 0, m.end() + tm.start(1)) + 1) if m else 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(post_text, brace_idx)
        body = post_text[brace_idx + 1 : end_idx]
        post_targets[name] = (tmpl, line_no, body)
    return {
        "has_sentinel": has_sentinel,
        "sentinel_line": sentinel_line,
        "pre_text": pre_text,
        "post_text": post_text,
        "pre_targets": pre_targets,
        "post_targets": post_targets,
    }


def parse_gn_file(gn_path):
    try:
        text = open(gn_path, encoding="utf-8").read()
    except Exception:
        return None
    return parse_gn_text(text)


# Check whether an unmigrated external package forms a mutual GN dependency cycle
# with pkg_dir (cycles must be co-migrated together rather than treated as blockers).
def _has_gn_cycle_with_pkg(dep_pkg, pkg_dir):
    dep_gn = os.path.join(workdir, dep_pkg, "BUILD.gn")
    try:
        dep_txt = re.sub(r"#[^\n]*", "", open(dep_gn, encoding="utf-8").read())
    except OSError:
        return False
    return bool(re.search(r'"//' + re.escape(pkg_dir) + r'(?=[:"/\s])', dep_txt))


# Collect third-party GN package prefixes that map to pre-configured Bazel repos/crates.
def _collect_third_party_mapped_pkgs():
    mapped = {"third_party/googletest", "third_party/rust_crates"}
    tp_map = os.path.join(workdir, "build/tools/bazel2gn/third_party_target_map.json")
    try:
        for lbl in json.load(open(tp_map, encoding="utf-8")).values():
            if isinstance(lbl, str) and lbl.startswith("//"):
                mapped.add(lbl[2:].split(":", 1)[0].strip("/"))
    except Exception:
        pass
    return mapped


# Expand top-level GN list variables referenced by a target body so `deps = common_deps`
# resolves external package labels accurately when checking upstream blockers.
def _expand_gn_list_vars(pre_text, body):
    clean_pre = re.sub(r"#[^\n]*", "", pre_text)
    gn_vars = {}
    for vm in re.finditer(r"(?m)^[ \t]*([A-Za-z_]\w*)\s*\+?=\s*\[", clean_pre):
        end_idx = find_closing_delim(clean_pre, vm.end() - 1, "[", "]")
        gn_vars.setdefault(vm.group(1), []).append(clean_pre[vm.end() - 1 : end_idx + 1])
    if not gn_vars:
        return body
    extra = []
    clean_body = re.sub(r"#[^\n]*", "", body)
    for vname, chunks in gn_vars.items():
        if re.search(rf"\b{re.escape(vname)}\b", clean_body):
            extra.extend(chunks)
    return (body + "\n" + "\n".join(extra)) if extra else body


# Scan all target blocks in `pre_text` so fallback `else { group("<name>") }` branches
# do not overwrite convertible `if (is_host)` target definitions of the same name.
def _iter_pre_gn_target_blocks(pre_text):
    target_re = re.compile(r'^[ \t]*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    blocks = []
    for tm in target_re.finditer(pre_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = pre_text.count("\n", 0, tm.start(1)) + 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(pre_text, brace_idx)
        blocks.append((name, tmpl, line_no, pre_text[brace_idx + 1 : end_idx]))
    return blocks


# Determine whether any convertible or test target in an unmigrated BUILD.gn is
# blocked from compiling in Bazel by unmigrated external packages outside scope
# or GN-blocked constructs (complete-directory unblocking is required before
# migrating a directory).
def _inspect_unmigrated_pkg_external_blockers(pkg_dir, gn_abs):
    gn_info = parse_gn_file(gn_abs)
    if not gn_info or gn_info["has_sentinel"]:
        return False, []
    pre_text = gn_info["pre_text"]
    pre_targets = gn_info["pre_targets"]
    all_blocks = _iter_pre_gn_target_blocks(pre_text)
    migratable_gn_kinds = (
        CONVERTIBLE_GN_TEMPLATES
        | GN_TEST_TEMPLATES
        | GN_CC_TEST_TEMPLATES
        | set(GN_TEST_PACKAGE_TEMPLATES)
        | {"python_host_test", "host_test"}
    )
    tmpl_rule_pat = re.compile(
        r"\b(" + "|".join(sorted(migratable_gn_kinds)) + r")\s*\("
    )
    local_templates = {}
    for tname, ttmpl, _, tbody in all_blocks:
        if ttmpl == "template":
            clean_tbody = re.sub(
                r"\bforward_variables_from\s*\(",
                "_gn_tmpl_forward(",
                re.sub(r"#[^\n]*", "", tbody),
            )
            m_inner = tmpl_rule_pat.search(clean_tbody)
            if m_inner:
                local_templates[tname] = (m_inner.group(1), clean_tbody)

    expanded_pre = dict(pre_targets)
    migratable_entries = []
    for gname, gtmpl, gline, gbody in all_blocks:
        if gtmpl == "template":
            continue
        if gtmpl in local_templates:
            eff_tmpl, tmpl_body = local_templates[gtmpl]
            eff_body = tmpl_body + "\n" + gbody
            eff_body = _expand_gn_list_vars(pre_text, eff_body)
            expanded_pre[gname] = (eff_tmpl, gline, eff_body)
            migratable_entries.append((gname, eff_tmpl, eff_body))
        elif gtmpl in migratable_gn_kinds:
            eff_body = _expand_gn_list_vars(pre_text, gbody)
            expanded_pre[gname] = (gtmpl, gline, eff_body)
            migratable_entries.append((gname, gtmpl, eff_body))

    if not migratable_entries:
        non_conv = [
            f"//{pkg_dir}:{name} ({tmpl})"
            for name, (tmpl, _, _) in pre_targets.items()
            if tmpl != "group"
        ]
        return bool(non_conv), non_conv

    in_scope_roots = [d for d in ([pkg_dir] + list(target_dirs)) if d]
    tp_mapped = _collect_third_party_mapped_pkgs()
    rel_subpkg_re = re.compile(
        r'"([A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*)(?::[^"(\s]+)?"'
    )
    vis_strip_re = re.compile(r"\bvisibility\s*\+?=\s*\[[^\]]*\]")
    all_ext_blockers = []
    any_blocked = False

    for tname, ttmpl, eff_body in migratable_entries:
        unmig_deps, _, gn_blocked = inspect_gn_target_deps(
            eff_body, pkg_dir, expanded_pre, set()
        )
        if (
            GN_ONLY_ATTR_RE.search(eff_body)
            or GN_BLOCKED_CONSTRUCT_RE.search(eff_body)
            or (FIDL_RUST_NEXT_UNIMPLEMENTED and FIDL_RUST_NEXT_DEP_RE.search(eff_body))
        ):
            gn_blocked = True
        sub_queue = [(pkg_dir, eff_body)]
        seen_sub = set()
        while sub_queue and len(seen_sub) < 16:
            cur_dir, raw_b = sub_queue.pop(0)
            clean_b = vis_strip_re.sub("", re.sub(r"#[^\n]*", "", raw_b))
            tp_pat = r'"//(third_party/[^":(\s]+)(?::([^"(\s]+))?"'
            for dm in re.finditer(tp_pat, clean_b):
                tp_pkg = dm.group(1).strip("/")
                tp_target = dm.group(2) or os.path.basename(tp_pkg)
                if any(tp_pkg == m or tp_pkg.startswith(m + "/") for m in tp_mapped):
                    continue
                tp_gn = os.path.join(workdir, tp_pkg, "BUILD.gn")
                tp_bzl = os.path.join(workdir, tp_pkg, "BUILD.bazel")
                tp_bld = os.path.join(workdir, tp_pkg, "BUILD")
                tp_has_bazel = False
                for cand_bzl in (tp_bzl, tp_bld):
                    if os.path.isfile(cand_bzl):
                        try:
                            tp_has_bazel = _bazel_pkg_has_target(
                                open(cand_bzl, encoding="utf-8").read(),
                                tp_target,
                                tp_gn,
                            )
                        except Exception:
                            tp_has_bazel = True
                        if tp_has_bazel:
                            break
                if os.path.isfile(tp_gn) and not tp_has_bazel:
                    lbl = f"//{tp_pkg}"
                    if lbl not in unmig_deps:
                        unmig_deps.append(lbl)
            cand_children = set()
            deps_pat = r"\b(?:public_|test_)?deps\s*\+?=\s*\[([^\]]*)\]"
            for deps_m in re.finditer(deps_pat, clean_b):
                for rm in rel_subpkg_re.finditer(deps_m.group(1)):
                    rel_p = rm.group(1).strip("/")
                    joined = os.path.join(cur_dir, rel_p)
                    sub_p = os.path.normpath(joined).replace("\\", "/")
                    if sub_p.startswith(pkg_dir + "/"):
                        cand_children.add(sub_p)
            for d in list(unmig_deps):
                if d[2:].startswith(pkg_dir + "/"):
                    cand_children.add(d[2:])
            for child_p in sorted(cand_children):
                if child_p in seen_sub:
                    continue
                child_gn = os.path.join(workdir, child_p, "BUILD.gn")
                child_bzl = os.path.join(workdir, child_p, "BUILD.bazel")
                child_has_bzl = False
                if os.path.isfile(child_bzl):
                    try:
                        child_has_bzl = bool(
                            re.search(
                                r'\bname\s*=\s*"',
                                open(child_bzl, encoding="utf-8").read(),
                            )
                        )
                    except Exception:
                        child_has_bzl = True
                if os.path.isfile(child_gn) and not child_has_bzl:
                    seen_sub.add(child_p)
                    child_info = parse_gn_file(child_gn)
                    if child_info:
                        c_body = child_info["pre_text"]
                        c_unmig, _, c_blocked = inspect_gn_target_deps(
                            c_body, child_p, child_info["pre_targets"], set()
                        )
                        for cd in c_unmig:
                            if cd not in unmig_deps:
                                unmig_deps.append(cd)
                        if c_blocked:
                            gn_blocked = True
                            c_desc = f"//{child_p} (GN-only target)"
                            if c_desc not in local_blockers:
                                local_blockers.append(c_desc)
                        sub_queue.append((child_p, c_body))

        ext_unmig = [
            d
            for d in unmig_deps
            if not any(
                d[2:] == td or d[2:].startswith(td + "/") for td in in_scope_roots
            )
            and not _has_gn_cycle_with_pkg(d[2:], pkg_dir)
        ]
        if ext_unmig or gn_blocked:
            any_blocked = True
        for d in ext_unmig:
            if d not in all_ext_blockers:
                all_ext_blockers.append(d)

    return any_blocked, all_ext_blockers


findings = []
gn_kept_pkg_test_deps_by_dir = {}

for pkg_dir in sorted(candidate_dirs):
    bazel_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    gn_abs = os.path.join(workdir, pkg_dir, "BUILD.gn")
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")

    if not os.path.isfile(bazel_abs):
        if (
            pkg_dir in target_dirs
            and os.path.isfile(gn_abs)
            and not os.path.isfile(os.path.join(workdir, pkg_dir, "BUILD"))
        ):
            # Allow an assigned target directory to remain unmigrated (INFO) when any
            # convertible or test target in its BUILD.gn depends on unmigrated external
            # packages outside the assigned CL scope or GN-blocked constructs (complete-
            # directory unblocking is required before migrating a directory).
            any_blocked, ext_blockers = _inspect_unmigrated_pkg_external_blockers(
                pkg_dir, gn_abs
            )
            if any_blocked:
                blocker_list = (
                    ", ".join(f"`{d}`" for d in ext_blockers[:6])
                    if ext_blockers
                    else "GN-blocked constructs or dependencies"
                )
                msg = (
                    f"Assigned migration target directory '{pkg_dir}' has no "
                    f"'{bazel_rel}' because one or more convertible or test targets in "
                    f"'{gn_rel}' depend on unmigrated external package(s) outside the "
                    f"assigned scope or GN-blocked constructs ({blocker_list}). Complete-"
                    "directory unblocking is required before migrating a directory."
                )
                rem = (
                    "First check with `git fetch origin main` whether "
                    f"{blocker_list} landed upstream. Co-migrate an unmigrated "
                    "dependency only when its entire directory is a small same-area "
                    "library helper within CL scoping limits (`<= 5` dirs, `<= ~500` "
                    "lines; never cross-area, `third_party/`, or `ffx_tool`/host-tool "
                    "packages alongside libraries). Otherwise do not partially migrate "
                    f"'{pkg_dir}' or create an empty/`# @bazel2gn:skip`-only '{bazel_rel}': "
                    f"migrate only completely unblocked in-scope child packages under "
                    f"`{pkg_dir}/` (if any), or leave '{pkg_dir}' unmigrated "
                    "(`modified_files: []`, `disputed_findings: []`, and "
                    "`test_footers: [\"fx lint\"]` so `test_verifier` "
                    "does not query a missing `BUILD.bazel`) and list the blocking "
                    "packages in `CoderReport.summary`."
                )
                _emit(
                    findings,
                    "target_blocked_by_unmigrated_deps",
                    "INFO",
                    gn_rel,
                    1,
                    msg,
                    rem,
                )
                continue
            msg = (
                f"Assigned migration target directory '{pkg_dir}' has no "
                f"'{bazel_rel}' file in the working tree while '{gn_rel}' "
                "remains unmigrated."
            )
            rem = (
                f"Migrate '{gn_rel}' to '{bazel_rel}' (and sync '{gn_rel}' with "
                f"`fx bazel2gn -d {pkg_dir}` if `{gn_rel}` is retained, or delete "
                f"'{gn_rel}' in Case 1). In a multi-directory or integration round, "
                f"if a parallel worker did not leave edits for '{pkg_dir}' in the "
                f"worktree, author '{bazel_rel}' and update '{gn_rel}' directly in "
                "this round rather than leaving it unmigrated or reporting "
                "`disputed_findings`."
            )
            _emit(findings, "missing_target_bazel_file", "error", gn_rel, 1, msg, rem)
        continue

    bazel_targets = parse_bazel_targets(bazel_abs, pkg_dir)
    gn_info = parse_gn_file(gn_abs) if os.path.isfile(gn_abs) else None
    packaged_binaries = {
        t["binary_ref"]
        for t in bazel_targets
        if t["rule"] in ("fx_packaged_binary", "fx_unittest_component") and t.get("binary_ref")
    }
    for t in bazel_targets:
        if t["rule"] == "fx_unittest_package":
            packaged_binaries.update(t.get("unit_tests_refs", []))
    # Treat host binaries wrapped by a pre-sentinel bazel_host_tool() or
    # bazel_ffx_tool() target as Bazel-only packaged binaries so `# @bazel2gn:skip`
    # on the underlying host binary in BUILD.bazel is allowed even when the GN
    # wrapper uses a different GN target name.
    if gn_info:
        for hname, (htmpl, _, hbody) in gn_info["pre_targets"].items():
            if htmpl in ("bazel_host_tool", "bazel_ffx_tool"):
                packaged_binaries.add(hname)
                m_bt = re.search(r'\bbazel_target\s*=\s*"([^"]+)"', hbody)
                if m_bt:
                    bt_lbl = m_bt.group(1).strip()
                    if bt_lbl.startswith(":"):
                        packaged_binaries.add(bt_lbl[1:])
                    elif bt_lbl.startswith(f"//{pkg_dir}:"):
                        packaged_binaries.add(bt_lbl[len(f"//{pkg_dir}:") :])
    converted_manifest_refs = {
        r
        for t in bazel_targets
        if t["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib")
        and t["skip_line"] is None
        for r in t.get("gn_dep_refs", ())
    }
    bazel_only_manifest_refs = {
        r
        for t in bazel_targets
        if t["rule"] == "fx_test_component"
        or (t["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib") and t["skip_line"] is not None)
        for r in t.get("dep_refs", ())
    }
    auto_skipped_manifests = {
        t["name"]
        for t in bazel_targets
        if t["rule"] == "fx_component_manifest"
        and t["name"] in bazel_only_manifest_refs
        and t["name"] not in converted_manifest_refs
    }
    fx_all_pkgs = [
        t
        for t in bazel_targets
        if t["rule"] in ("fx_package", "fuchsia_package", "fx_unittest_package")
    ]
    # Non-test fx_package targets (excludes fx_unittest_package and test_components packages).
    fx_pkgs = [t for t in fx_all_pkgs if not t.get("is_test_pkg")]

    # Check for cross-tree Fuchsia package_name collisions across BUILD.gn and BUILD.bazel
    # so staging packages into amber-files/repository does not fail with conflicting manifests.
    GN_FUCHSIA_PKG_TEMPLATES = (
        "fuchsia_package",
        "fuchsia_package_with_single_component",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_fuzzer_package",
        "fuchsia_shell_package",
        "fuchsia_driver_package",
    )
    BAZEL_FUCHSIA_PKG_RULES = (
        "fx_package",
        "fx_unittest_package",
        "fuchsia_package",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
    )

    def find_gn_packages_in_file(other_abs):
        try:
            text = open(other_abs, encoding="utf-8").read()
        except Exception:
            return []
        out = []
        target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
        for tm in target_re.finditer(text):
            otmpl, oname = tm.group(1), tm.group(2)
            if otmpl not in GN_FUCHSIA_PKG_TEMPLATES:
                continue
            oline = text.count("\n", 0, tm.start()) + 1
            end_idx = find_closing_brace(text, tm.end() - 1)
            obody = re.sub(r"#[^\n]*", "", text[tm.end() : end_idx])
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"$]+)"', obody)
            eff_name = pn_m.group(1) if pn_m else oname
            out.append((eff_name, otmpl, oname, oline))
        return out

    def find_colliding_fuchsia_package(cand_pkg_name, cur_pkg_dir):
        if not cand_pkg_name:
            return None
        try:
            grep_out = subprocess.run(
                [
                    "git",
                    "-C",
                    workdir,
                    "grep",
                    "-l",
                    "--fixed-strings",
                    "-e",
                    f'"{cand_pkg_name}"',
                    "--",
                    "BUILD.gn",
                    "*/BUILD.gn",
                    "BUILD.bazel",
                    "*/BUILD.bazel",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=30,
                check=False,
            ).stdout
        except Exception:
            return None
        for raw_line in grep_out.splitlines():
            other_rel = raw_line.strip()
            if not other_rel:
                continue
            other_dir = os.path.dirname(other_rel).strip("/")
            if other_dir == cur_pkg_dir or other_rel.startswith(("bundles/assembly/", "out/")):
                continue
            other_abs = os.path.join(workdir, other_rel)
            if other_rel.endswith("BUILD.gn"):
                for eff_name, otmpl, oname, oline in find_gn_packages_in_file(other_abs):
                    if eff_name == cand_pkg_name:
                        return other_rel, f'`{otmpl}("{oname}")`', oline
            elif other_rel.endswith("BUILD.bazel"):
                for ot in parse_bazel_targets(other_abs, other_dir):
                    if (
                        ot["rule"] in BAZEL_FUCHSIA_PKG_RULES
                        and ot.get("package_name") == cand_pkg_name
                    ):
                        return other_rel, f'`{ot["rule"]}(name = "{ot["name"]}")`', ot["line"]
        return None

    def suggest_disambiguated_pkg_name(cur_pkg_dir, orig_name):
        parts = [p.replace("_", "-") for p in cur_pkg_dir.split("/") if p]
        stem = "-".join(parts[-2:]) if len(parts) >= 2 else (parts[-1] if parts else orig_name)
        cand = f"{stem}-tests" if orig_name.endswith(("tests", "test")) else stem
        if cand == orig_name and len(parts) >= 3:
            suffix = "-tests" if orig_name.endswith(("tests", "test")) else ""
            cand = "-".join(parts[-3:]) + suffix
        return cand

    def check_cross_tree_package_name_collisions(all_pkgs, cur_pkg_dir, brel, grel, g_info):
        has_sent = bool(g_info and g_info.get("has_sentinel"))
        for pt in all_pkgs:
            # Non-test fx_package targets (such as drop-in replacement subpackages) have fixed
            # runtime package URLs consumed by .cml manifests or assembly and are not exported to
            # tests.json via bazel_test_suite.
            if not pt.get("is_test_pkg"):
                continue
            p_name = pt["package_name"]
            candidates = [p_name]
            if (
                has_sent
                and pt["rule"] == "fx_unittest_package"
                and pt["skip_line"] is None
                and not pt.get("has_resources")
            ):
                candidates.append(f"{p_name}-gn")
            for cand in candidates:
                hit = find_colliding_fuchsia_package(cand, cur_pkg_dir)
                if not hit:
                    continue
                other_rel, other_desc, other_line = hit
                suggested = suggest_disambiguated_pkg_name(cur_pkg_dir, p_name)
                msg = (
                    f'`{pt["rule"]}(name = "{pt["name"]}")` in \'{brel}\' publishes Fuchsia '
                    f"package `{cand}`, which collides with {other_desc} in '{other_rel}' "
                    f"(line {other_line}); staging both into `amber-files/repository` on CQ "
                    "fails with `conflict between package manifests`."
                )
                rem = (
                    f"Rename `{pt['rule']}` in '{brel}' to a globally unique package name "
                    f"such as `\"{suggested}\"` (or set `package_name = \"{suggested}\"`), "
                    f"re-run `fx bazel2gn -d {cur_pkg_dir}` if '{grel}' has a BAZEL2GN "
                    "SENTINEL, and update `bazel_test_suite`, `group(\"tests\")`, and commit "
                    "`Bazel-Migration-Target:` / `Test:` footers to match."
                )
                _emit(findings, "cross_tree_package_name_collision", "error", brel, pt["line"], msg, rem)
                break

    check_cross_tree_package_name_collisions(fx_all_pkgs, pkg_dir, bazel_rel, gn_rel, gn_info)

    # Check whether a GN target above the sentinel is a check-cfg config() that bazel2gn
    # natively generates from rustc_check_cfg_build_flags().
    def is_convertible_check_cfg_gn_config(gname, gtmpl, gbody, bazel_rules_by_name):
        if gtmpl != "config":
            return False
        if bazel_rules_by_name.get(gname) == "rustc_check_cfg_build_flags":
            return True
        cleaned = re.sub(r"#[^\n]*", "", gbody)
        attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", cleaned))
        if not attrs or not (attrs <= {"rustflags", "visibility"}) or "rustflags" not in attrs:
            return False
        flags = []
        for m in re.finditer(r"\brustflags\s*\+?=\s*\[([^\]]*)\]", cleaned):
            flags.extend(re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(1)))
        return bool(flags) and all(f.startswith("--check-cfg=cfg(") for f in flags)

    # Check for leftover unused `.gni` imports above `## BAZEL2GN SENTINEL` (or in a
    # migrated BUILD.gn without a sentinel) when the templates they provide have been
    # migrated to BUILD.bazel or are already imported below the sentinel by
    # `//build/tools/bazel2gn/bazel_migration.gni`.
    def check_unused_gn_imports(g_info, grel, cur_pkg_dir):
        if not g_info or (grel not in changed and cur_pkg_dir != target_dir):
            return
        has_sent = g_info["has_sentinel"]
        pre_tmpls = {v[0] for v in g_info["pre_targets"].values()}
        if not has_sent and not (
            pre_tmpls
            & {
                "bazel_test_suite",
                "bazel_host_tool",
                "bazel_ffx_tool",
                "bazel_fuchsia_package",
            }
        ):
            return
        pre_txt = g_info["pre_text"]
        post_txt = g_info["post_text"]
        pre_no_comments = re.sub(r"#[^\n]*", "", pre_txt)
        full_no_comments = pre_no_comments + "\n" + re.sub(r"#[^\n]*", "", post_txt)

        # .gni files already covered below the sentinel by bazel_migration.gni:
        # only their usage above the sentinel (pre_text) justifies importing them above the sentinel.
        sentinel_covered_gni_tmpls = {
            "//build/components.gni": (
                "fuchsia_component",
                "fuchsia_component_manifest",
                "fuchsia_package",
                "fuchsia_package_with_single_component",
                "fuchsia_test_component",
                "fuchsia_unittest_component",
                "fuchsia_test_package",
                "fuchsia_unittest_package",
                "fuchsia_structured_config_cpp_elf_lib",
                "fuchsia_structured_config_rust_lib",
                "fuchsia_structured_config_values",
                "resource",
            ),
            "//build/components/fuchsia_component.gni": ("fuchsia_component", "fuchsia_component_manifest"),
            "//build/components/fuchsia_package.gni": ("fuchsia_package",),
            "//build/components/fuchsia_package_with_single_component.gni": (
                "fuchsia_package_with_single_component",
            ),
            "//build/components/fuchsia_test_component.gni": ("fuchsia_test_component",),
            "//build/components/fuchsia_test_package.gni": ("fuchsia_test_package",),
            "//build/components/fuchsia_unittest_component.gni": ("fuchsia_unittest_component",),
            "//build/components/fuchsia_unittest_package.gni": ("fuchsia_unittest_package",),
            "//build/components/fuchsia_structured_config.gni": (
                "fuchsia_structured_config_cpp_elf_lib",
                "fuchsia_structured_config_rust_lib",
                "fuchsia_structured_config_values",
            ),
            "//build/rust/rustc_library.gni": ("rustc_library",),
            "//build/rust/rustc_test.gni": ("rustc_test",),
            "//build/rust/rustc_binary.gni": ("rustc_binary",),
            "//build/rust/rustc_macro.gni": ("rustc_macro",),
            "//build/rust/rustc_dylib.gni": ("rustc_dylib",),
            "//build/rust/rustc_bindgen.gni": ("rustc_bindgen", "rustc_bindgen_golden"),
            "//build/fidl/fidl.gni": ("fidl",),
            "//build/go/go_library.gni": ("go_library",),
            "//build/go/go_binary.gni": ("go_binary",),
            "//build/go/go_test.gni": ("go_test",),
            "//build/cpp/sdk_source_set.gni": ("sdk_source_set",),
            "//build/cpp/sdk_static_library.gni": ("sdk_static_library",),
            "//build/cpp/sdk_shared_library.gni": ("sdk_shared_library",),
            "//build/cpp/library_headers.gni": ("library_headers",),
            "//build/zircon/zx_library.gni": ("zx_library", "library_headers"),
            "//build/compiled_action.gni": ("compiled_action", "compiled_action_foreach"),
            "//build/dist/renamed_binary.gni": ("renamed_binary",),
            "//build/sdk/sdk_host_tool.gni": (
                "sdk_host_tool",
                "sdk_executable_host_tool",
                "sdk_go_binary_host_tool",
                "sdk_rustc_binary_host_tool",
            ),
            "//src/developer/ffx/build/ffx_plugin.gni": ("ffx_plugin",),
            "//third_party/cobalt/metrics_registry.gni": ("metrics_registry",),
            "//tools/fidl/measure-tape/measure_tape.gni": ("measure_tape",),
            "//tools/protobuf/rust_proto_library.gni": ("rust_proto_library",),
        }
        # .gni files not imported by bazel_migration.gni: check the entire BUILD.gn.
        file_wide_gni_syms = {
            "//build/rust/rustc_staticlib.gni": (r"\brustc_staticlib\s*\(",),
            "//build/rust/rustc_library_with_features.gni": (r"\brustc_library_with_features\s*\(",),
            "//build/test.gni": (r"\btest\s*\(",),
            "//build/testing/cc_test_executable.gni": (r"\bcc_test_executable\s*\(",),
            "//build/testing/host_test.gni": (r"\bhost_test\s*\(",),
            "//build/testing/host_test_data.gni": (r"\bhost_test_data\s*\(",),
            "//build/testing/environments.gni": (
                r"\b(?:emu_env|basic_envs|linux_env|mac_env|nuc_env|vim3_env|astro_env|"
                r"sherlock_env|nelson_env|all_fuchsia_envs|environments)\b",
            ),
            "//build/python/python_library.gni": (r"\bpython_library\s*\(",),
            "//build/python/python_binary.gni": (r"\bpython_binary\s*\(",),
            "//build/python/python_host_test.gni": (r"\bpython_host_test\s*\(",),
            "//build/host.gni": (r"\binstall_host_tools\s*\(",),
            "//build/dist/resource.gni": (r"\bresource\s*\(",),
            "//build/bazel/bazel_test_suite.gni": (r"\bbazel_test_(?:suite|package)\s*\(",),
            "//build/bazel/bazel_host_tool.gni": (r"\bbazel_host_tool\s*\(",),
            "//build/bazel/bazel_fuchsia_package.gni": (r"\bbazel_fuchsia_package\s*\(",),
            "//src/developer/ffx/build/ffx_tool.gni": (
                r"\b(?:ffx_tool|bazel_ffx_tool)\s*\(",
            ),
        }

        unused_imports = []
        first_line = 1
        for im in re.finditer(r'^\s*import\(\s*"([^"]+\.gni)"\s*\)', pre_txt, re.M):
            gni_path = im.group(1)
            lineno = pre_txt.count("\n", 0, im.start()) + 1
            if gni_path in sentinel_covered_gni_tmpls:
                tmpls = sentinel_covered_gni_tmpls[gni_path]
                pat = r"\b(?:" + "|".join(re.escape(t) for t in tmpls) + r")\s*\("
                if not re.search(pat, pre_no_comments):
                    if not unused_imports:
                        first_line = lineno
                    unused_imports.append(f'`import("{gni_path}")`')
            elif gni_path in file_wide_gni_syms:
                pats = file_wide_gni_syms[gni_path]
                if not any(re.search(p, full_no_comments) for p in pats):
                    if not unused_imports:
                        first_line = lineno
                    unused_imports.append(f'`import("{gni_path}")`')

        if unused_imports:
            where = "above `## BAZEL2GN SENTINEL` in" if has_sent else "in"
            sent_note = (
                " (`//build/tools/bazel2gn/bazel_migration.gni` already imports the templates "
                "needed below the sentinel)"
                if has_sent
                else ""
            )
            _emit(findings, "unused_gn_import", "error", grel, first_line, (
                    f"Unused `.gni` import(s) {where} '{grel}': "
                    f"{', '.join(unused_imports)} are no longer used by any target {where} "
                    f"'{grel}'{sent_note}."
                ), (
                    f"Remove the unused {', '.join(unused_imports)} line(s) from '{grel}' "
                    "and format with `fx format-code`."
                ))

    def check_ifttt_on_bridge_files(g_info, grel):
        if not g_info or not g_info.get("pre_targets"):
            return
        for gname, (gtmpl, _, gbody) in g_info["pre_targets"].items():
            if gtmpl == "bazel_fuchsia_package" and "LINT.IfChange" in gbody:
                gline = g_info["pre_text"].count("\n", 0, g_info["pre_text"].find(gbody)) + 1
                msg = (
                    f"`bazel_fuchsia_package(\"{gname}\")` in '{grel}' contains a `# LINT.IfChange` "
                    "directive, which is unnecessary and triggers `sentinel_drift` warnings."
                )
                rem = (
                    f"Remove `# LINT.IfChange` / `# LINT.ThenChange(...)` from `{gname}` in '{grel}'."
                )
                _emit(findings, "unnecessary_lint_guard_on_bridge", "error", grel, gline, msg, rem)

    # Check whether an out-of-target package only adds a minimal `# @bazel2gn:skip`
    # bridge-stub BUILD.bazel while leaving its existing BUILD.gn untouched.
    def is_untouched_gn_bridge_stub_pkg(cur_pkg_dir, grel, g_info, b_targets):
        return bool(
            target_dirs
            and cur_pkg_dir not in target_dirs
            and grel not in changed
            and g_info
            and not g_info.get("has_sentinel")
            and b_targets
            and all(t.get("skip_line") is not None for t in b_targets)
        )

    if is_untouched_gn_bridge_stub_pkg(pkg_dir, gn_rel, gn_info, bazel_targets):
        continue

    check_unused_gn_imports(gn_info, gn_rel, pkg_dir)

    def check_unsupported_zircon_only_library(g_info, b_targets):
        if not g_info or not pkg_dir:
            return
        if not pkg_dir.startswith("zircon/"):
            return
        for t in b_targets:
            if "library" in t.get("kind", ""):
                _emit(
                    findings,
                    "unsupported_zircon_only_library",
                    "error",
                    f"{pkg_dir}/BUILD.bazel",
                    t.get("line", 1),
                    f"Library target '{t.get('name', '')}' in zircon/ might only be used inside zircon/. Do not migrate libraries only used inside zircon/kernel or zircon/ unless transitively used outside it.",
                    f"Verify the library has callers outside zircon/. If not, revert its migration."
                )

    check_unsupported_zircon_only_library(gn_info, bazel_targets)

    check_ifttt_on_bridge_files(gn_info, gn_rel)

    if gn_info and gn_info["has_sentinel"]:
        bazel_names = {t["name"] for t in bazel_targets}
        bazel_rules_by_name = {t["name"]: t["rule"] for t in bazel_targets}
        targets_by_name = {t["name"]: t for t in bazel_targets}
        # Skips that already existed at the change base were added by code owners and are kept.
        # The harness always sets PLANTER_CHANGE_BASE (HEAD~1 once the task is committed); like
        # every other check, an unset value falls back to HEAD.
        try:
            base_bazel_text = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_bazel_text = ""
        if not base_bazel_text:
            try:
                base_bazel_text = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"origin/main:{bazel_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                base_bazel_text = ""
        base_targets = parse_bazel_targets(bazel_rel, pkg_dir, text=base_bazel_text) if base_bazel_text else []
        base_target_skips = {bt["name"]: bt["skip_line"] is not None for bt in base_targets}
        base_attr_skips = {(bt["name"], a) for bt in base_targets for a, _, _ in bt.get("skipped_test_attrs", [])}
        base_gn_info_for_sentinel = None
        for rev_cand in dict.fromkeys((change_base, "HEAD~1", "origin/main")):
            if not rev_cand:
                continue
            try:
                rev_gn_txt = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{rev_cand}:{gn_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                continue
            if rev_gn_txt:
                base_gn_info_for_sentinel = parse_gn_text(rev_gn_txt)
                break
        base_had_sentinel = bool(
            base_gn_info_for_sentinel and base_gn_info_for_sentinel.get("has_sentinel")
        )
        base_pre_gn_names = (
            set(base_gn_info_for_sentinel.get("pre_targets", {}).keys())
            if base_had_sentinel
            else set()
        )
        is_ancillary_pkg = bool(target_dirs and pkg_dir not in target_dirs)

        # Pre-existing code-owner skips (target packages included): a BUILD.bazel target that
        # was already `# @bazel2gn:skip` at the change base, whose BUILD.bazel call and
        # hand-written GN definition are both unchanged since then, stays as the owners wrote it.
        # Reviewers ask to keep such targets untouched; flagging them makes the coder either
        # rewrite owner code or dispute the finding.
        def bazel_call_text(text, name):
            try:
                tree = ast.parse(text)
            except Exception:
                return None
            for node in ast.walk(tree):
                if isinstance(node, ast.Call) and any(
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and kw.value.value == name
                    for kw in node.keywords
                ):
                    return ast.get_source_segment(text, node)
            return None

        def find_owner_kept_skips():
            base_gn_all = (
                base_gn_info_for_sentinel.get("pre_targets", {})
                if base_gn_info_for_sentinel
                else {}
            )
            try:
                with open(os.path.join(workdir, bazel_rel), encoding="utf-8") as cur_f:
                    cur_bazel_text = cur_f.read()
            except Exception:
                return set()
            kept = set()
            for bname, was_skipped in base_target_skips.items():
                cur_t = targets_by_name.get(bname)
                base_g = base_gn_all.get(bname)
                cur_g = gn_info["pre_targets"].get(bname)
                if not (was_skipped and cur_t and cur_t["skip_line"] and base_g and cur_g):
                    continue
                if base_g[0] != cur_g[0] or base_g[2].strip() != cur_g[2].strip():
                    continue
                base_call = bazel_call_text(base_bazel_text, bname)
                if base_call and base_call == bazel_call_text(cur_bazel_text, bname):
                    kept.add(bname)
            return kept

        owner_kept_skips = find_owner_kept_skips() if base_bazel_text else set()
        for okname in sorted(owner_kept_skips):
            findings.append({
                "source": "migration_sanity",
                "category": "preexisting_owner_skip_kept",
                "severity": "INFO",
                "file": bazel_rel,
                "line": targets_by_name[okname]["line"],
                "message": (
                    f"Target '{okname}' was already `# @bazel2gn:skip` at the change base and "
                    f"its BUILD.bazel and hand-written BUILD.gn definitions are unchanged. "
                    "Code owners keep it hand-written; this is allowed."
                ),
                "remediation": "None needed. Do not modify or convert it unless asked.",
            })

        # Check whether a GN `source_set` above the sentinel only groups non-code
        # `inputs` files (paired with a `# @bazel2gn:skip` `filegroup` in BUILD.bazel
        # because `bazel2gn` does not translate `filegroup`).
        def is_inputs_only_gn_source_set(gtmpl, gbody, gname):
            if gtmpl != "source_set":
                return False
            cleaned = re.sub(r"#[^\n]*", "", gbody)
            attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", cleaned))
            code_attrs = {
                "sources",
                "public",
                "deps",
                "public_deps",
                "configs",
                "public_configs",
                "cflags",
                "cflags_c",
                "cflags_cc",
                "ldflags",
                "libs",
            }
            if attrs & code_attrs:
                return False
            return "inputs" in attrs or bazel_rules_by_name.get(gname) == "filegroup"
        # Flag redundant # @bazel2gn:skip annotations on rules bazel2gn already ignores.
        # Note: resolveTestPackages and resolveComponentTargets in build/tools/bazel2gn run
        # before bazelRuleToGNTemplate (noGNEquivalent) and convert fx_package targets with
        # test_components (as well as referenced fx_test, fx_test_component,
        # fx_unittest_component, and fx_packaged_binary targets) unless skipped.
        def _is_resolved_before_no_gn_equiv(t_info):
            r_name = t_info["rule"]
            if r_name == "fx_package":
                return bool(
                    t_info.get("has_test_components")
                    or t_info.get("is_test_pkg")
                    or "test_components" in t_info.get("raw", "")
                )
            return r_name in (
                "fx_test",
                "fx_test_component",
                "fx_unittest_component",
                "fx_packaged_binary",
            )

        def _check_redundant_no_gn_equiv_skip(t_info):
            skip_ln = t_info["skip_line"]
            r_name = t_info["rule"]
            t_name = t_info["name"]
            if (
                skip_ln is None
                or r_name not in NO_GN_EQUIV_BAZEL_RULES
                or _is_resolved_before_no_gn_equiv(t_info)
                or base_target_skips.get(t_name)
            ):
                return False
            msg = (
                f"`# @bazel2gn:skip` on `{r_name}(name = \"{t_name}\")` in "
                f"'{bazel_rel}' is redundant because `bazel2gn` already ignores "
                f"`{r_name}` (`noGNEquivalent`)."
            )
            rem = (
                f"Remove `# @bazel2gn:skip` (and any comment whose only purpose was "
                f"explaining the skip) above `{r_name}(name = \"{t_name}\")` in "
                f"'{bazel_rel}'; `bazel2gn` skips `{r_name}` automatically."
            )
            _emit(findings, "redundant_bazel2gn_skip", "error", bazel_rel, skip_ln, msg, rem)
            return True

        for t in bazel_targets:
            if _check_redundant_no_gn_equiv_skip(t):
                continue
            tname = t["name"]
            rule = t["rule"]
            is_test_bin = rule == "fx_cc_test_binary" or (rule in ("fx_cc_binary", "cc_binary") and t.get("testonly"))
            is_test_target = rule in BAZEL_TEST_RULES or is_test_bin
            # Test skips added in this change (not present at the change base) are surfaced for a
            # human, and the dual-testing cost exception never applies to sdk_*() libraries.
            skip_records = []  # (what, line, has_marker, has_comment, technical, tut_seed)
            if t["skip_line"] is not None and is_test_target and not base_target_skips.get(tname):
                skip_records.append((
                    f"`{rule}(name = \"{tname}\")`",
                    t["skip_line"],
                    t.get("has_attention_marker"),
                    t.get("has_skip_reason"),
                    has_technical_test_skip_condition(t, targets_by_name),
                    [tname],
                ))
            marked_attrs = set(t.get("attention_test_attrs", []))
            for a, ln, reason in t.get("skipped_test_attrs", []):
                if t["skip_line"] is None and (tname, a) not in base_attr_skips:
                    skip_records.append((
                        f"`{a}` on `{rule}(name = \"{tname}\")`",
                        ln,
                        (a, ln) in marked_attrs,
                        reason,
                        bool(t.get("has_test_data")),
                        [tname],
                    ))
            for what, ln, has_marker, has_comment, technical, seed in skip_records:
                if technical or not (has_marker or has_comment):
                    # Justified by a technical condition, or a bare skip that unjustified_test_skip
                    # reports below.
                    continue
                sdk_tut, local_libs, ext_refs = resolve_targets_under_test(seed, targets_by_name)
                if sdk_tut:
                    _emit(findings, "sdk_library_test_skip", "error", bazel_rel, ln, (
                            f"{what} in '{bazel_rel}' skips dual-running a test of an sdk_*() library other than "
                            f"sdk_source_set() ({', '.join(sdk_tut)}). Those tests stay dual-tested, so the "
                            "dual-testing cost exception does not apply."
                        ), (
                            "Remove `# @bazel2gn:skip` and its comment, re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire the generated GN test into `group(\"tests\")` "
                            "(see docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)."
                        ))
                    continue
                unverifiable = ""
                if not local_libs and ext_refs:
                    unverifiable = (
                        " The target under test is defined in another package, so this check cannot verify that "
                        "it is not an sdk_*() library other than sdk_source_set() (those stay dual-tested): "
                        + ", ".join(sorted(ext_refs)[:3]) + "."
                    )
                if has_marker:
                    # Surface reviewer-attention skips (cost or infra incompatibility) as INFO.
                    _emit(
                        findings,
                        "test_skip_needs_reviewer_attention",
                        "INFO",
                        bazel_rel,
                        ln,
                        (
                            f"{what} in '{bazel_rel}' skips dual-running its test in GN via "
                            "`# Needs reviewer attention (bazel2gn skip): ...` (dual-testing cost "
                            "or pre-existing infra incompatibility such as `testsharder` "
                            "`duplicatedShardNames` under `use_test_pilot`)." + unverifiable
                        ),
                        (
                            "A human reviewer in Gerrit must confirm this skip (see "
                            "docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems) "
                            'and the CL description must include a "Needs reviewer attention:" '
                            "line. Do not remove the skip just to clear this INFO finding."
                        ),
                    )
                else:
                    # Reject unmarked comment-only skips that meet no technical condition.
                    _emit(
                        findings,
                        "unjustified_test_skip",
                        "error",
                        bazel_rel,
                        ln,
                        (
                            f"{what} in '{bazel_rel}' has a `# @bazel2gn:skip` that did not exist "
                            "at the change base, is justified only by an unmarked comment, and no "
                            "technical skip condition applies. Skips added in a migration must "
                            "meet a technical condition or use the reviewer-attention marker."
                            + unverifiable
                        ),
                        (
                            "If dual-testing cost outweighs risk or dual-running hits a "
                            "pre-existing infra bug (e.g. `testsharder` `duplicatedShardNames` "
                            "under `use_test_pilot`), start the comment with "
                            "`# Needs reviewer attention (bazel2gn skip): <reason>` and add a "
                            'matching "Needs reviewer attention:" line to the commit message. '
                            f"Otherwise remove the skip, re-run `fx bazel2gn -d {pkg_dir}`, and "
                            'wire the generated GN test into `group("tests")` (see '
                            "docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)."
                        ),
                    )
            if t["skip_line"] is None:
                for attr_name, attr_skip_line, attr_reason in t.get("skipped_test_attrs", []):
                    if (
                        (tname, attr_name) in base_attr_skips
                        or t.get("has_test_data")
                        or attr_reason
                    ):
                        continue
                    _emit(findings, "unjustified_test_skip", "error", bazel_rel, attr_skip_line, (
                            f"Test attribute `{attr_name}` on `{rule}(name = \"{tname}\")` in '{bazel_rel}' is annotated "
                            "with `# @bazel2gn:skip`, which prevents bazel2gn from generating the GN unit test while "
                            f"'{gn_rel}' still dual-builds `{tname}`."
                        ), (
                            f"Remove `# @bazel2gn:skip` from `{attr_name}` on `{tname}` in '{bazel_rel}', re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire `:{tname}_test` into `group(\"tests\")` in '{gn_rel}' "
                            "(see \"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md)."
                        ))
            is_test_bin = rule == "fx_cc_test_binary" or (rule in ("fx_cc_binary", "cc_binary") and t.get("testonly"))
            if t["skip_line"] is not None and (rule in BAZEL_TEST_RULES or is_test_bin):
                t5_owner_reason = t.get("has_skip_reason") or bool(base_target_skips.get(tname))
                if not (has_technical_test_skip_condition(t, targets_by_name) or t5_owner_reason):
                    _emit(findings, "unjustified_test_skip", "error", bazel_rel, t["skip_line"], (
                            f"Test target '{tname}' ({rule}) in '{bazel_rel}' is annotated with `# @bazel2gn:skip` in a "
                            "dual-build package without a technical skip condition or explanatory comment (target under "
                            "test is not skipped, and bazel2gn supports translating this test target)."
                        ), (
                            f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in '{bazel_rel}', delete any "
                            f"duplicate hand-written GN definition above `## BAZEL2GN SENTINEL` in '{gn_rel}', re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire the generated GN test into `group(\"tests\")` "
                            "(see \"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md)."
                        ))
                    continue
            in_pre_gn = (
                tname in gn_info["pre_targets"]
                and not is_inputs_only_gn_source_set(
                    gn_info["pre_targets"][tname][0],
                    gn_info["pre_targets"][tname][2],
                    tname,
                )
                and (
                    gn_info["pre_targets"][tname][0] in CONVERTIBLE_GN_TEMPLATES
                    or is_convertible_check_cfg_gn_config(
                        tname,
                        gn_info["pre_targets"][tname][0],
                        gn_info["pre_targets"][tname][2],
                        bazel_rules_by_name,
                    )
                )
            )
            ref_in_pre_gn = rule in CONVERTIBLE_BAZEL_RULES and any(
                ptmpl
                not in (
                    "bazel_test_suite",
                    "bazel_host_tool",
                    "bazel_ffx_tool",
                    "bazel_fuchsia_package",
                )
                and bool(
                    re.search(
                        r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(tname) + r'(?:_[A-Za-z0-9_]+)?(?:"|\()',
                        pbody,
                    )
                )
                for ptmpl, _, pbody in gn_info["pre_targets"].values()
            )
            if (tname in packaged_binaries or tname in auto_skipped_manifests) and not in_pre_gn and not ref_in_pre_gn:
                continue
            if rule in BAZEL_TEST_RULES or is_test_bin or (rule == "filegroup" and not in_pre_gn):
                continue
            if is_ancillary_pkg and base_had_sentinel and base_target_skips.get(tname):
                continue
            if tname in owner_kept_skips:
                continue
            if is_bazel_only_private_test_dep(
                t, bazel_targets, gn_info, pkg_dir, packaged_binaries
            ):
                continue
            if tname in gn_info["pre_targets"] and (
                gn_info["pre_targets"][tname][0]
                not in (
                    CONVERTIBLE_GN_TEMPLATES
                    | GN_TEST_TEMPLATES
                    | set(GN_TEST_PACKAGE_TEMPLATES)
                    | {"fuchsia_package", "fuchsia_package_with_single_component", "fuchsia_component"}
                )
                or GN_ONLY_ATTR_RE.search(gn_info["pre_targets"][tname][2])
                or is_gn_component_target_blocked(
                    tname,
                    gn_info["pre_targets"][tname][0],
                    gn_info["pre_targets"][tname][2],
                    gn_info["pre_targets"],
                    bazel_targets,
                    pkg_dir,
                )
            ):
                continue  # bazel2gn cannot emit its GN template or GN-only attributes (see gn_attr_parity / gn_target_type_parity).
            if (
                (rule in FX_MANIFEST_RULE_TO_GN or rule == "fx_component")
                and not in_pre_gn
                and not ref_in_pre_gn
                and tname not in gn_info.get("post_targets", {})
            ):
                # An fx_compiled_manifest() is consumed by structured config libraries and by
                # binaries that embed the .cm (compile_data / $(execpath)).
                has_unskipped_comp_consumer = rule in FX_MANIFEST_RULE_TO_GN and any(
                    other["name"] != tname
                    and other["skip_line"] is None
                    and (
                        other["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib")
                        or (rule == "fx_compiled_manifest" and other["rule"] in CONVERTIBLE_BAZEL_RULES)
                    )
                    and tname in other.get("gn_dep_refs", ())
                    for other in bazel_targets
                )
                if not has_unskipped_comp_consumer:
                    continue
            if t["skip_line"] is not None and (
                rule in CONVERTIBLE_BAZEL_RULES or in_pre_gn or ref_in_pre_gn
            ):
                if rule == "rustc_check_cfg_build_flags":
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with "
                        "`# @bazel2gn:skip`"
                        + (
                            f" and duplicated as `config(\"{tname}\")` above the "
                            f"BAZEL2GN SENTINEL in '{gn_rel}'"
                            if in_pre_gn
                            else ""
                        )
                        + ". `bazel2gn` natively converts `rustc_check_cfg_build_flags` into "
                        "a GN `config()` below the sentinel, so it must not be skipped."
                    )
                    skip_rem = (
                        f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in "
                        f"'{bazel_rel}', delete any manual `config(\"{tname}\")` definition "
                        f"above `## BAZEL2GN SENTINEL` in '{gn_rel}', and re-run "
                        f"`fx bazel2gn -d {pkg_dir}`."
                    )
                elif rule in FX_MANIFEST_RULE_TO_GN or rule == "fx_component":
                    gn_comp_tmpl = FX_MANIFEST_RULE_TO_GN.get(rule, "fuchsia_component")
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with "
                        "`# @bazel2gn:skip`"
                        + (
                            f" and duplicated as `{gn_comp_tmpl}(\"{tname}\")` above the "
                            f"BAZEL2GN SENTINEL in '{gn_rel}'"
                            if in_pre_gn
                            else ""
                        )
                        + (f" while referenced by `:{tname}` in '{gn_rel}'" if ref_in_pre_gn else "")
                        + f". `bazel2gn` natively converts `{rule}` into `{gn_comp_tmpl}` "
                        "below the sentinel, so it must not be skipped."
                    )
                    skip_rem = (
                        f"If the checkout is behind `origin/main`, first rebase onto "
                        "`origin/main` (`git fetch origin main && git rebase origin/main`) so "
                        f"`fx bazel2gn` includes `{rule}` support. Then remove "
                        f"`# @bazel2gn:skip` (and its comment) above `{rule}(name = \"{tname}\")` "
                        f"in '{bazel_rel}', delete any manual `{gn_comp_tmpl}(\"{tname}\")` "
                        f"definition above `## BAZEL2GN SENTINEL` in '{gn_rel}', and re-run "
                        f"`fx bazel2gn -d {pkg_dir}`."
                    )
                else:
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with `# @bazel2gn:skip`"
                        + (f" and duplicated as `{gn_info['pre_targets'][tname][0]}(\"{tname}\")` above the BAZEL2GN SENTINEL in '{gn_rel}'" if in_pre_gn else "")
                        + (f" while referenced by `:{tname}` in '{gn_rel}'" if ref_in_pre_gn else "")
                        + ". Convertible libraries and reference/bitrot-prevention targets must not be skipped."
                    )
                    skip_rem = (
                        f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in '{bazel_rel}', "
                        f"delete any manual `{tname}` target definition above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                        "(keeping only helper `config(...)` or test package definitions above the sentinel), "
                        "use `# @bazel2gn:raw_overwrite:[ \"<gn_config_1>\", ... ]` on the closing `],` of `copts = [...]` "
                        "if C/C++ compiler flags need to map to GN `configs`, and re-run `fx bazel2gn -d "
                        + pkg_dir
                        + "`."
                    )
                _emit(findings, "skipped_convertible_target", "error", bazel_rel, t["skip_line"], skip_msg, skip_rem)

        # Flag newly added `# @bazel2gn:skip` packaging/component helper targets in BUILD.bazel
        # that duplicate a target kept above the sentinel in BUILD.gn when nothing in BUILD.bazel
        # (and no external package via visibility) depends on them.
        def check_unused_skipped_gn_duplicates(
            btargets, brel, grel, pdir, g_info, preexisting_names=()
        ):
            pre_map = g_info.get("pre_targets", {}) if g_info else {}
            priv_vis = {"//visibility:private", ":__pkg__", f"//{pdir}:__pkg__"}
            helper_skip_rules = (
                "resource",
                "fuchsia_package_resource_group",
                "fx_component_manifest",
                "fx_compiled_manifest",
                "fx_packaged_binary",
                "fx_component",
                "fx_test_component",
                "fx_unittest_component",
            )
            for bt in btargets:
                if bt.get("skip_line") is None or bt["rule"] not in helper_skip_rules:
                    continue
                bname = bt["name"]
                if bname not in pre_map or bname in owner_kept_skips:
                    continue
                if (bt["rule"] in FX_MANIFEST_RULE_TO_GN or bt["rule"] == "fx_component") and not is_gn_component_target_blocked(
                    bname, pre_map[bname][0], pre_map[bname][2], pre_map, btargets, pdir
                ):
                    continue
                # Preserve pre-existing non-convertible or GN-blocked helper targets that
                # already existed in BUILD.bazel at $PLANTER_CHANGE_BASE so partial package
                # migrations do not contradict dropped_preexisting_bazel_target.
                if bname in preexisting_names:
                    continue
                vis = bt.get("vis_items") or []
                if any(v not in priv_vis for v in vis):
                    continue
                if any(
                    bname in other.get("dep_refs", ())
                    for other in btargets
                    if other["name"] != bname
                ):
                    continue
                gtmpl = pre_map[bname][0]
                msg = (
                    f"Target '{bname}' ({bt['rule']}) in '{brel}' is annotated with "
                    f"`# @bazel2gn:skip` and duplicates `{gtmpl}(\"{bname}\")` kept above "
                    f"`## BAZEL2GN SENTINEL` in '{grel}', but no target in '{brel}' "
                    f"depends on `:{bname}`."
                )
                rem = (
                    f"Delete the unused `# @bazel2gn:skip` target "
                    f"`{bt['rule']}(name = \"{bname}\")` (along with any `# @bazel2gn:skip` "
                    "`fx_packaged_binary` helpers and `load()` statements only it used, and "
                    f"any `.shard.cml` `exports_files` added only for it) from '{brel}', and "
                    f"remove any `Bazel-Migration-Target: //{pdir}:{bname}` footer for it "
                    f"while its enclosing package remains in '{grel}'."
                )
                _emit(findings, "unused_skipped_duplicate_target", "error", brel, bt["line"], msg, rem)

        check_unused_skipped_gn_duplicates(
            bazel_targets, bazel_rel, gn_rel, pkg_dir, gn_info, base_target_skips
        )

        rust_unit_test_names = {t["name"] + "_test" for t in bazel_targets if t.get("unit_tests")}

        # Check whether a Bazel test target is legitimately skipped from bazel2gn conversion
        # (e.g. uses test_data/data/test_label or an unsupported test rule) while testing a
        # dual-built target, so its GN copy stays hand-written above the sentinel.
        def is_untranslatable_skipped_bazel_test(bname):
            bt = targets_by_name.get(bname)
            if bt is not None:
                if bt["skip_line"] is None:
                    return False
                if base_target_skips.get(bname) or bt.get("has_skip_reason"):
                    return True
                brule = bt["rule"]
                if brule in ("host_go_test", "go_test", "host_rustc_test") and bt.get(
                    "has_test_data"
                ):
                    return True
                if brule in UNSUPPORTED_TEST_BINARY_RULES:
                    return True
                if brule == "rustc_test" and (
                    bt.get("cross_pkg_srcs_or_crate")
                    or (
                        bt.get("has_rustc_env")
                        and bt.get("crate_ref") in targets_by_name
                        and targets_by_name[bt["crate_ref"]].get("has_rustc_env")
                    )
                ):
                    return True
                return False
            if bname.endswith("_test"):
                owner = targets_by_name.get(bname[: -len("_test")])
                if owner and owner.get("skipped_test_attrs"):
                    return bool(
                        owner.get("has_test_data")
                        or any(
                            (owner["name"], a) in base_attr_skips or r
                            for a, _, r in owner.get("skipped_test_attrs", [])
                        )
                    )
            return False

        # Check whether a pre-sentinel GN rustc_test or go_test that is not in BUILD.bazel
        # has no GN-only blockers and all of its dependencies are available in Bazel (in the
        # working tree or on origin/main), meaning it must be migrated to BUILD.bazel.
        def inspect_migratable_gn_test_above_sentinel(gname, gbody, candidate_names):
            if any(n in bazel_names or n in rust_unit_test_names for n in candidate_names):
                return False, []
            pkg_tmpls = (
                "fuchsia_unittest_package",
                "fuchsia_test_package",
                "fuchsia_test_component",
                "fuchsia_unittest_component",
                "fuchsia_component",
                "bootfs_test",
            )
            ref_re = re.compile(
                r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(gname) + r'(?:"|\()'
            )
            if any(
                ptmpl in pkg_tmpls and ref_re.search(pbody)
                for ptmpl, _, pbody in gn_info["pre_targets"].values()
            ):
                return False, []
            cleaned = re.sub(
                r"\bvisibility\s*\+?=\s*\[[^\]]*\]", "", re.sub(r"#[^\n]*", "", gbody)
            )
            if (
                GN_ONLY_ATTR_RE.search(cleaned)
                or GN_BLOCKED_CONSTRUCT_RE.search(cleaned)
                or re.search(
                    r"\b(?:data_deps|non_rust_deps|inputs|test_specs|environments|args|rustenv)\s*\+?=",
                    cleaned,
                )
                or "$" in cleaned
                or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
            ):
                return False, []
            _, _, gn_blocked_dep = inspect_gn_target_deps(
                gbody, pkg_dir, gn_info["pre_targets"], bazel_names
            )
            if gn_blocked_dep:
                return False, []
            local_ref_re = re.compile(
                r'"(?::([A-Za-z0-9_.-]+)|//'
                + re.escape(pkg_dir)
                + r'(?::([A-Za-z0-9_.-]+))?)"'
            )
            for lm in local_ref_re.finditer(cleaned):
                lname = lm.group(1) or lm.group(2) or os.path.basename(pkg_dir)
                if lname not in bazel_names:
                    return False, []
            rebase_deps = []
            for dm in re.finditer(r'"//([^":(\s]+)(?::([^"(\s]+))?"', cleaned):
                dep_pkg = dm.group(1).strip("/")
                dep_tname = dm.group(2) or os.path.basename(dep_pkg)
                if not dep_pkg or dep_pkg == pkg_dir:
                    continue
                if dep_pkg == "third_party/rust_crates" or dep_pkg.startswith(
                    "third_party/rust_crates/"
                ):
                    continue
                if dep_pkg.startswith(("build/", "prebuilt/", "third_party/", "out/")):
                    if not os.path.isfile(os.path.join(workdir, dep_pkg, "BUILD.bazel")):
                        return False, []
                    continue
                dep_bazel_abs = os.path.join(workdir, dep_pkg, "BUILD.bazel")
                dep_gn_abs = os.path.join(workdir, dep_pkg, "BUILD.gn")
                has_local_bazel = False
                if os.path.isfile(dep_bazel_abs):
                    try:
                        has_local_bazel = _bazel_pkg_has_target(
                            open(dep_bazel_abs, encoding="utf-8").read(),
                            dep_tname,
                            dep_gn_abs,
                        )
                    except Exception:
                        has_local_bazel = False
                if has_local_bazel:
                    continue
                try:
                    origin_bazel_txt = subprocess.check_output(
                        ["git", "-C", workdir, "show", f"origin/main:{dep_pkg}/BUILD.bazel"],
                        stderr=subprocess.DEVNULL,
                        text=True,
                    )
                    has_origin_bazel = _bazel_pkg_has_target(
                        origin_bazel_txt, dep_tname, dep_gn_abs
                    )
                except Exception:
                    has_origin_bazel = False
                if has_origin_bazel:
                    dep_label = f"//{dep_pkg}"
                    if dep_label not in rebase_deps:
                        rebase_deps.append(dep_label)
                else:
                    return False, []
            return True, rebase_deps

        # Build targeted remediation hints for migrating a pre-sentinel GN test binary
        # (resolving cross-package checked-in `inputs` to enclosing `exports_files` labels,
        # `crate = ":<lib>"` reuse on `rustc_test`, and companion `fx_test` packaging).
        def _build_gn_test_extra_guidance(gname, gtmpl, gbody):
            hints = []
            cleaned = re.sub(r"#[^\n]*", "", gbody)
            # 1. Resolve `inputs = ["//..."]` paths to enclosing Bazel package exports_files.
            for im in re.finditer(r"\binputs\s*\+?=\s*\[([^\]]*)\]", cleaned):
                for inp in re.findall(r'"//([^"]+)"', im.group(1)):
                    inp_abs = os.path.join(workdir, inp)
                    if not os.path.isfile(inp_abs):
                        continue
                    cur_d = os.path.dirname(inp)
                    enc_pkg = ""
                    while cur_d:
                        if os.path.isfile(os.path.join(workdir, cur_d, "BUILD.bazel")):
                            enc_pkg = cur_d
                            break
                        parent_d = os.path.dirname(cur_d)
                        if parent_d == cur_d:
                            break
                        cur_d = parent_d
                    if enc_pkg:
                        rel_inp = os.path.relpath(
                            inp_abs, os.path.join(workdir, enc_pkg)
                        ).replace(os.sep, "/")
                        hints.append(
                            f"map `inputs = [\"//{inp}\"]` to "
                            f"`compile_data = [\"//{enc_pkg}:{rel_inp}\"]` on "
                            f"`rustc_test(name = \"{gname}\")` (`//{inp}` is a checked-in "
                            f"file in the `//{enc_pkg}` Bazel package, not an unmigrated "
                            f"subpackage; add `\"//{pkg_dir}:__pkg__\"` to the "
                            f"`exports_files` `visibility` list in `{enc_pkg}/BUILD.bazel` "
                            "if needed)"
                        )
            # 2. Check for a same-package `rustc_library` crate to reference via `crate =`.
            if gtmpl == "rustc_test":
                lib_cand = gname[: -len("_test")] if gname.endswith("_test") else ""
                if lib_cand not in targets_by_name:
                    rust_libs = [
                        t["name"]
                        for t in bazel_targets
                        if t["rule"] in ("rustc_library", "rust_library")
                    ]
                    if len(rust_libs) == 1:
                        lib_cand = rust_libs[0]
                if lib_cand and lib_cand in targets_by_name:
                    out_m = re.search(r'\boutput_name\s*=\s*"([^"]+)"', cleaned)
                    cname_part = (
                        f', crate_name = "{out_m.group(1)}"'
                        if out_m and out_m.group(1) != gname
                        else ""
                    )
                    hints.append(
                        f"set `crate = \":{lib_cand}\"{cname_part}` on "
                        f"`rustc_test(name = \"{gname}\")` with only the test-only `deps` "
                        f"not already in `:{lib_cand}` so `bazel2gn` generates "
                        f"`rustc_test(\"{gname}\")` below the sentinel without duplicating "
                        "the library's `sources` and `deps`"
                    )
                excl_m = re.search(
                    r"\bexclude_toolchain_tags\s*\+?=\s*\[([^\]]*)\]", cleaned
                )
                if excl_m:
                    excl_tags = re.findall(r'"([^"]+)"', excl_m.group(1))
                    if excl_tags:
                        tag_list = ", ".join(
                            f'"exclude_toolchain_tags={t}"' for t in excl_tags
                        )
                        hints.append(
                            f"translate `exclude_toolchain_tags` to `tags = [{tag_list}]` "
                            f"on `rustc_test(name = \"{gname}\")` (keeping any "
                            "`# TODO(...): Re-enable ... in these environments.` comment "
                            "only on the test target, not on `rustc_library`)"
                        )
            # 3. Check for a pre-sentinel GN test package wrapping `:<gname>`.
            ref_re = re.compile(
                r'"(?::|//' + re.escape(pkg_dir) + r":)" + re.escape(gname) + r'(?:"|\()'
            )
            for pname, (ptmpl, _, pbody) in sorted(gn_info["pre_targets"].items()):
                if ptmpl not in GN_TEST_PACKAGE_TEMPLATES or not ref_re.search(pbody):
                    continue
                pclean = re.sub(r"#[^\n]*", "", pbody)
                man_m = re.search(r'\bmanifest\s*=\s*"([^"]+)"', pclean)
                tt_m = re.search(r'\btest_type\s*=\s*"([^"]+)"', pclean)
                has_no_exc = bool(
                    re.search(r"\bcreate_no_exception_channel\s*=\s*true\b", pclean)
                )
                if man_m and (tt_m or has_no_exc):
                    hints.append(
                        f"also migrate `{ptmpl}(\"{pname}\")` to '{bazel_rel}' using "
                        "`fx_packaged_binary`, `fx_component_manifest`, "
                        "`fx_test_component`"
                        + (
                            f' (with `test_type = "{tt_m.group(1)}"`'
                            + (
                                ' and `"//src/starnix/kernel/vdso"` in `deps`'
                                if tt_m.group(1) == "starnix"
                                else ""
                            )
                            + ")"
                            if tt_m
                            else ""
                        )
                        + ", `fx_package`"
                        + (
                            " (annotated with `# @bazel2gn:skip` because `bazel2gn` does "
                            "not translate `create_no_exception_channel` on `fx_test`, "
                            f"keeping `{ptmpl}(\"{pname}\")` above the sentinel in "
                            f"'{gn_rel}' with `package_name = \"{pname}-gn\"` and "
                            f"`component_name = \"{pname}\"`)"
                            if has_no_exc
                            else ""
                        )
                        + f", and `fx_test(name = \"{pname}\", ...)` in '{bazel_rel}', and "
                        f"wire `bazel_test_suite(\"bazel_tests\")` (`target_tests = "
                        f"[\"//{pkg_dir}:{pname}\"]`) into `group(\"tests\")` in "
                        f"'{gn_rel}' (guarded by `if (!is_asan)` when the test excludes "
                        "`asan`)"
                    )
            return (" Specifically: " + "; ".join(hints) + ".") if hints else ""

        for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
            # Record authoritative INFO when a fuchsia_package stays hand-written above the
            # sentinel in a dual-build BUILD.gn without fx_package in BUILD.bazel (bazel2gn does
            # not translate non-test fx_package, so GN subpackages/deps callers still need the GN
            # package, and switching an assembled package with dual-built rustc_dylib deps to
            # Bazel fx_package duplicates the dylib in BlobFS across GN and Bazel packages).
            if gtmpl in ("fuchsia_package", "fuchsia_package_with_single_component") and not fx_pkgs:
                pkg_rem = (
                    f"Keep `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                    "(wrapping the `bazel2gn`-generated `fuchsia_component` below the "
                    f"sentinel) and do not add `fx_package(name = \"{gname}\")` or GN-only "
                    f"caller visibility in '{bazel_rel}'."
                )
                pkg_msg = (
                    f'GN target "{gname}" ({gtmpl} at {gn_rel}:{gline}) remains hand-written '
                    "in BUILD.gn (above the bazel2gn sentinel) and is not migrated to "
                    "BUILD.bazel: `bazel2gn` does not translate non-test `fx_package`, so "
                    f"`{gtmpl}(\"{gname}\")` stays above the sentinel for GN `subpackages`/"
                    "`deps` callers and avoids duplicating dual-built `rustc_dylib` blobs in "
                    "BlobFS across GN and Bazel packages (intra-Bazel `fx_package` runfiles "
                    "deduplication does not deduplicate against GN-built packages)."
                )
                _emit(findings, "gn_package_kept_above_sentinel", "INFO", gn_rel, gline, pkg_msg, pkg_rem)
                continue
            # Reject hand-written check-cfg GN config() targets above the sentinel when
            # bazel2gn natively converts rustc_check_cfg_build_flags() below the sentinel.
            if is_convertible_check_cfg_gn_config(gname, gtmpl, gbody, bazel_rules_by_name):
                _emit(findings, "duplicate_target_above_sentinel", "error", gn_rel, gline, (
                        f"Convertible GN `config(\"{gname}\")` with `--check-cfg=cfg(...)` "
                        f"rustflags remains above `## BAZEL2GN SENTINEL` in '{gn_rel}'"
                        + (
                            f" (duplicating `rustc_check_cfg_build_flags(name = \"{gname}\")` "
                            f"in '{bazel_rel}')"
                            if gname in bazel_names
                            else ""
                        )
                        + ". `bazel2gn` natively converts `rustc_check_cfg_build_flags` into "
                        "a GN `config()` below the sentinel."
                    ), (
                        f"Define `rustc_check_cfg_build_flags(name = \"{gname}\", cfgs = [...], "
                        f"features = [...])` in '{bazel_rel}' without `# @bazel2gn:skip` "
                        "(loaded from `//build/bazel/rules/rust:rustc_check_cfg_build_flags.bzl`), "
                        f"delete `config(\"{gname}\")` from above `## BAZEL2GN SENTINEL` in "
                        f"'{gn_rel}', and run `fx bazel2gn -d {pkg_dir}`."
                    ))
                continue
            if gtmpl in GN_TEST_TEMPLATES:
                out_m = re.search(r'\boutput_name\s*=\s*"([^"]+)"', gbody)
                candidate_names = {gname, out_m.group(1) if out_m else gname}
                bazel_dups = sorted(
                    n
                    for n in candidate_names
                    if (n in bazel_names or n in rust_unit_test_names)
                    and not is_untranslatable_skipped_bazel_test(n)
                )
                # Check if this GN test is consumed by a GN test package/component kept above
                # the sentinel while the Bazel test carries `# @bazel2gn:skip` (e.g. `rustc_test`
                # with `crate = ":<lib>"`, which GN `rustc_test` cannot express).
                kept_for_gn_pkg = any(
                    ptmpl in (
                        "fuchsia_unittest_package",
                        "fuchsia_test_package",
                        "fuchsia_test_component",
                        "fuchsia_unittest_component",
                        "fuchsia_component",
                    )
                    and re.search(
                        r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(gname) + r'(?:"|\()',
                        pbody,
                    )
                    for ptmpl, _, pbody in gn_info["pre_targets"].values()
                ) and all(
                    any(bt["name"] == n and bt["skip_line"] is not None for bt in bazel_targets)
                    for n in bazel_dups
                )
                if bazel_dups and not kept_for_gn_pkg:
                    _emit(findings, "duplicate_test_above_sentinel", "error", gn_rel, gline, (
                            f"GN test `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' duplicates the Bazel test "
                            + ", ".join(f"`:{n}`" for n in bazel_dups)
                            + f" of '{bazel_rel}', and bazel2gn generates the GN copy of that test below the sentinel."
                        ), (
                            f"Delete `{gtmpl}(\"{gname}\")` from '{gn_rel}', re-run `fx bazel2gn -d {pkg_dir}`, list the generated GN "
                            "test in `group(\"tests\")` and export the Bazel test through a package `test_suite()` wired into the "
                            "nearest ancestor aggregation `test_suite()` (never add a per-package `bazel_test_suite()` in `BUILD.gn`; "
                            "see \"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md). "
                            f"If the test is blocked by an out-of-scope external dependency, revert the directory migration (`rm -f {bazel_rel} && git checkout -- {gn_rel}`) to leave "
                            f"'{pkg_dir}' unmigrated (`modified_files: []`), or report unavoidable local exceptions via "
                            "`disputed_findings` in `CoderReport`."
                        ))
                else:
                    _, rebase_deps = inspect_migratable_gn_test_above_sentinel(
                        gname, gbody, candidate_names
                    )
                    rebase_note = (
                        " Dependencies newly migrated on `origin/main`: "
                        + ", ".join(rebase_deps)
                        + "."
                        if rebase_deps
                        else ""
                    )
                    rebase_step = (
                        "First fetch and rebase onto the latest `origin/main` "
                        "(`git fetch origin main && git rebase origin/main`) to pick up the "
                        "Bazel targets for "
                        + ", ".join(rebase_deps)
                        + ". Then m"
                        if rebase_deps
                        else "M"
                    )
                    if gtmpl == "rustc_test":
                        target_rule_hint = (
                            "`host_rustc_test` (from `//build/bazel/rules/host_tests:host_rustc_test.bzl`) "
                            "or `rustc_test` (from `//build/bazel/rules/rust:rustc_test.bzl`)"
                        )
                    elif gtmpl == "go_test":
                        target_rule_hint = (
                            "`host_go_test` (from `//build/bazel/rules/host_tests:host_go_test.bzl`) "
                            "or `go_test` (from `@io_bazel_rules_go//go:def.bzl`)"
                        )
                    else:
                        target_rule_hint = (
                            "`fx_cc_test_binary` (from `//build/bazel/rules/cc:fx_cc_test_binary.bzl`)"
                        )
                    extra_test_guidance = _build_gn_test_extra_guidance(
                        gname, gtmpl, gbody
                    )
                    _emit(findings, "gn_test_not_migrated", "error", gn_rel, gline, (
                            f"GN test `{gtmpl}(\"{gname}\")` remains hand-written above "
                            f"`## BAZEL2GN SENTINEL` in '{gn_rel}'. All test binary targets must be "
                            f"migrated to '{bazel_rel}' rather than kept above the sentinel."
                            + rebase_note
                        ), (
                            f"{rebase_step}igrate `{gtmpl}(\"{gname}\")` to '{bazel_rel}' "
                            f"using {target_rule_hint} without `# @bazel2gn:skip`, delete the "
                            f"hand-written `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` "
                            f"in '{gn_rel}', re-run `fx bazel2gn -d {pkg_dir}` so `bazel2gn` "
                            "generates the GN copy below the sentinel, and wire the test into "
                            "`group(\"tests\")`."
                            + extra_test_guidance
                            + (
                                " Do NOT dispute `gn_test_not_migrated` when `inputs` "
                                "files are already exported by an enclosing Bazel package."
                                if extra_test_guidance
                                else (
                                    f" Export the Bazel test through a package `test_suite()` "
                                    "(`tests` or `host_tests`) wired into the nearest ancestor "
                                    f"aggregation `test_suite()` (never add a per-package "
                                    f"`bazel_test_suite` in '{gn_rel}'). Do not partially "
                                    f"migrate '{pkg_dir}': if `{gname}` is blocked by an "
                                    "out-of-scope external dependency, revert the directory "
                                    f"migration (`rm -f {bazel_rel} && git checkout -- {gn_rel}`) "
                                    f"to leave '{pkg_dir}' completely unmigrated "
                                    "(`modified_files: []`), or report unavoidable local "
                                    "exceptions via `disputed_findings` in `CoderReport`."
                                )
                            )
                        ))
                continue
            gn_only_matches = (
                sorted({m.group(0).strip() for m in GN_ONLY_ATTR_RE.finditer(re.sub(r"#[^\n]*", "", gbody))})
                if gtmpl in CONVERTIBLE_GN_TEMPLATES
                else []
            )
            if gn_only_matches:
                gn_only_rem = (
                    "bazel2gn can also generate it below the sentinel: define it in "
                    f"'{bazel_rel}' and name the GN-only constructs there with "
                    "`# @bazel2gn:raw_attribute:` lines (or the dedicated tags/attributes the "
                    "checkout's build/tools/bazel2gn/README.md documents, e.g. "
                    "`disable_instrumentation`, `disable_build_flags`), as described in "
                    "\"GN-only attributes on generated targets\" in the coder prompt."
                )
                _emit(findings, "gn_only_attr_target_kept_above_sentinel", "INFO", gn_rel, gline, (
                        f"Convertible GN target `{gtmpl}(\"{gname}\")` stays hand-written above "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' because it uses GN-only constructs "
                        f"({', '.join(f'`{x}`' for x in gn_only_matches)}). This is allowed."
                    ), gn_only_rem)
            if gtmpl in CONVERTIBLE_GN_TEMPLATES and not GN_ONLY_ATTR_RE.search(gbody):
                if is_inputs_only_gn_source_set(gtmpl, gbody, gname):
                    continue
                if is_ancillary_pkg and base_had_sentinel and gname in base_pre_gn_names:
                    continue
                if gname in owner_kept_skips:
                    continue
                if is_packaged_testonly_gn_binary(
                    gname, gtmpl, gbody, gn_info["pre_targets"], bazel_names
                ):
                    continue
                if is_gn_component_target_blocked(
                    gname, gtmpl, gbody, gn_info["pre_targets"], bazel_targets, pkg_dir
                ):
                    continue
                unmigrated_deps, declare_args_deps, gn_blocked_dep = inspect_gn_target_deps(
                    gbody, pkg_dir, gn_info["pre_targets"], bazel_names
                )

                attr_hints = []
                if gtmpl == "cm":
                    attr_hints.append(
                        f"declare `fx_compiled_manifest(name = \"{gname}\", manifest = ..., "
                        "component_name = ..., output_name = ...)` (loaded from "
                        "`//build/bazel/rules/components:fx_component.bzl`, with `restricted_features` "
                        "if GN sets them) without `# @bazel2gn:skip`; it generates the same GN `cm()`, "
                        "which is not packaged as a resource"
                    )
                if gtmpl == "fuchsia_component_manifest":
                    attr_hints.append(
                        f"declare `fx_component_manifest(name = \"{gname}\", ...)` "
                        "(loaded from `//build/bazel/rules/components:fx_component.bzl`, "
                        "listing all `.shard.cml` includes in `includes = [...]`) without "
                        "`# @bazel2gn:skip`"
                    )
                elif gtmpl == "fuchsia_component":
                    attr_hints.append(
                        f"declare `fx_component(name = \"{gname}\", compiled_manifest = \":<manifest>\", "
                        "deps = [\":<bin>_packaged_bin\"])` (wrapping its local binary in "
                        "`fx_packaged_binary` from `//build/bazel/rules/packages:fx_packaged_binary.bzl`) "
                        "without `# @bazel2gn:skip`"
                    )
                if gtmpl == "rustc_binary":
                    out_m = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', gbody)
                    if out_m and out_m.group(1) != gname:
                        attr_hints.append(
                            f"set `crate_name = \"{out_m.group(1)}\"` on `rustc_binary(name = \"{gname}\")` in '{bazel_rel}' "
                            f"(bazel2gn maps `crate_name` on `rustc_binary` to `output_name = \"{out_m.group(1)}\"` in BUILD.gn; "
                            "omit dummy `version` strings)"
                        )
                    src_m = re.search(r'\bsource_root\s*=\s*"([^"]+)"', gbody)
                    if src_m and src_m.group(1) != "src/main.rs":
                        attr_hints.append(f"set `crate_root = \"{src_m.group(1)}\"`")
                if gtmpl.startswith("rustc_"):
                    cfg_m = re.search(r'\bconfigs\s*\+=\s*\[\s*"([^"]+)"\s*\]', gbody)
                    if cfg_m:
                        attr_hints.append(f"map `configs += [ \"{cfg_m.group(1)}\" ]` to `lint_config = \"{cfg_m.group(1)}\"`")
                if gtmpl in ("fuchsia_component", "fuchsia_component_manifest"):
                    bz_rule = (
                        "fx_component"
                        if gtmpl == "fuchsia_component"
                        else "fx_component_manifest"
                    )
                    attr_hints.append(
                        f"author `{bz_rule}(name = \"{gname}\", ...)` in '{bazel_rel}' "
                        "(loaded from `//build/bazel/rules/components:fx_component.bzl`, "
                        "wrapping any binary dep in `fx_packaged_binary` for `fx_component`) "
                        "without `# @bazel2gn:skip`, and if `fx bazel2gn` in the current "
                        "checkout does not yet translate it, rebase onto `origin/main` first "
                        "(`git fetch origin main && git rebase origin/main`)"
                    )

                dep_msg = (
                    f" Unmigrated first-party dependencies referenced by `{gname}`: "
                    + ", ".join(unmigrated_deps)
                    + "."
                    if unmigrated_deps
                    else ""
                )
                dep_rem = ""
                if unmigrated_deps or gn_blocked_dep:
                    dep_rem = (
                        f" Do not partially migrate '{pkg_dir}' when out-of-scope external "
                        f"dependencies or GN-blocked constructs block any target in '{gn_rel}': "
                        + (
                            "co-migrate small in-scope dependency package(s) "
                            + ", ".join(unmigrated_deps)
                            + " (create their BUILD.bazel and sync with `fx bazel2gn -d <dep_dir>`) "
                            "if within CL scoping limits, or "
                            if unmigrated_deps
                            else ""
                        )
                        + f"revert the partial migration (`rm -f {bazel_rel} && git checkout -- {gn_rel}`) "
                        f"to leave '{pkg_dir}' completely unmigrated (`modified_files: []`), or "
                        "report unavoidable exceptions via `disputed_findings` in `CoderReport`."
                    )
                    if declare_args_deps:
                        dep_rem += (
                            " For `declare_args()` in "
                            + ", ".join(declare_args_deps)
                            + ", keep `declare_args() { ... }` in a `.gni` file (imported above `## BAZEL2GN SENTINEL` in its BUILD.gn), "
                            "export `<arg>` via `generated_file(\"gn_build_variables_for_bazel\")` in `//build/bazel/BUILD.gn` with "
                            "`# LINT.IfChange` / `# LINT.ThenChange(...)` on both sides, `load(\"@fuchsia_build_info//:args.bzl\", \"<arg>\")` "
                            "in its BUILD.bazel, and translate `if (<arg>) { features = [ ... ] }` as `crate_features = [\"...\"] if <arg> else []`."
                        )
                else:
                    dep_rem = (
                        f" Do not partially migrate '{pkg_dir}' when out-of-scope external "
                        f"dependencies block any target in '{gn_rel}' (revert to leave "
                        f"'{pkg_dir}' unmigrated with `modified_files: []`, or report "
                        "unavoidable exceptions via `disputed_findings` in `CoderReport`)."
                    )
                hint_rem = (" In '" + bazel_rel + "', " + "; ".join(attr_hints) + ".") if attr_hints else ""

                _emit(findings, "duplicate_target_above_sentinel", "error", gn_rel, gline, (
                        f"Convertible GN target `{gtmpl}(\"{gname}\")` remains above `## BAZEL2GN SENTINEL` "
                        f"in '{gn_rel}'"
                        + (" (duplicating the target in BUILD.bazel)" if gname in bazel_names else "")
                        + ". All convertible library/binary/component targets (including `fuchsia_component_manifest`, "
                        "`fuchsia_component`, benchmark binaries, and reference/bitrot-prevention libraries) "
                        "must be migrated to BUILD.bazel and generated below the sentinel by `fx bazel2gn`."
                        + dep_msg
                    ), (
                        f"Define `{gname}` in '{bazel_rel}' without `# @bazel2gn:skip`, delete `{gtmpl}(\"{gname}\")` "
                        f"from above `## BAZEL2GN SENTINEL` in '{gn_rel}', and run `fx bazel2gn -d {pkg_dir}`."
                        + dep_rem
                        + hint_rem
                    ))

    gn_kept_pkg_test_deps = set()
    if gn_info:
        bazel_test_pkg_names = {t["package_name"] for t in fx_all_pkgs}
        bazel_rules = {t["name"]: t["rule"] for t in bazel_targets}
        # bazel2gn translates test `fx_package(test_components = ...)` chains, so only a skipped
        # one (or one with `resources`, which bazel2gn rejects) needs a hand-written GN copy.
        bazel_untranslatable_test_pkg_names = {
            t["package_name"]
            for t in bazel_targets
            if t["rule"] == "fuchsia_package"
            or (
                t["rule"] == "fx_package"
                and (not t.get("has_test_components") or t.get("skip_line") is not None or t.get("has_resources"))
            )
            or (
                t["rule"] == "fx_unittest_package"
                and (
                    t.get("has_resources")
                    or any(
                        bazel_rules.get(u) in UNSUPPORTED_TEST_BINARY_RULES
                        for u in t.get("unit_tests_refs", [])
                    )
                )
            )
        }
        bazel_host_only = {t["name"] for t in bazel_targets if t.get("host_only")}
        rust_unit_tests = {t["name"] + "_test" for t in bazel_targets if t["unit_tests"] and not t.get("host_only")}
        third_party_gn_labels = {
            "//third_party/googletest",
            "//third_party/googletest:gtest",
            "//third_party/googletest:gtest_main",
            "//third_party/googletest:gtest_prod",
            "//third_party/googletest:gmock",
            "//third_party/googletest:gmock_main",
        }
        gn_to_bazel_ext_repo = {}
        try:
            tp_map_path = os.path.join(
                workdir, "build/tools/bazel2gn/third_party_target_map.json"
            )
            with open(tp_map_path, encoding="utf-8") as f:
                tp_map = json.load(f)
            third_party_gn_labels.update(tp_map.values())
            for bz_lbl, gn_lbl in tp_map.items():
                if bz_lbl.startswith("@"):
                    repo_n = bz_lbl[1:].split("//", 1)[0].split(":", 1)[0]
                    if repo_n:
                        gn_to_bazel_ext_repo[gn_lbl] = repo_n
        except Exception:
            pass

        # Discover third-party Bzlmod modules in toplevel.MODULE.bazel whose upstream
        # BUILD.bazel compiles __Fuchsia__ system/FIDL headers without Fuchsia SDK deps
        # (shared with cq_reachability via planter_check_lib.fuchsia_incompatible_ext_repos).
        def _find_fuchsia_incompat_ext_repos():
            if _pcl_ext is None:
                return {}
            return _pcl_ext.fuchsia_incompatible_ext_repos(workdir)

        # Compute Fuchsia-incompatible @<repo> dependencies reachable from each target
        # in BUILD.bazel through same-package :target edges.
        def _collect_local_target_incompat_ext_deps(btargets, incompat_map):
            if not incompat_map:
                return {}
            direct_incompat = {}
            local_edges = {}
            for bt in btargets:
                tname = bt["name"]
                seg_parts = [bt.get("raw", "")]
                call_node = bt.get("call")
                if call_node is not None:
                    try:
                        seg_parts.append(ast.unparse(call_node))
                    except Exception:
                        pass
                raw_seg = "\n".join(seg_parts)
                incompat_hits = set()
                same_pkg = set()
                for lbl in re.findall(r'"([^"]+)"', raw_seg):
                    if lbl.startswith("@"):
                        rname = lbl[1:].split("//", 1)[0].split(":", 1)[0]
                        if rname in incompat_map:
                            incompat_hits.add(f"@{rname}")
                    elif lbl.startswith(":") and "(" not in lbl:
                        same_pkg.add(lbl[1:])
                    elif lbl.startswith(f"//{pkg_dir}:") and "(" not in lbl:
                        same_pkg.add(lbl.split(":", 1)[1])
                direct_incompat[tname] = incompat_hits
                local_edges[tname] = same_pkg
            resolved = {}
            for tname in direct_incompat:
                seen = set()
                stack = [tname]
                acc = set()
                while stack:
                    cur = stack.pop()
                    if cur in seen:
                        continue
                    seen.add(cur)
                    acc.update(direct_incompat.get(cur, set()))
                    stack.extend(local_edges.get(cur, set()) - seen)
                if acc:
                    resolved[tname] = sorted(acc)
            return resolved

        # Check whether a GN test target or its local dependencies reach an external
        # Bazel repository whose upstream BUILD.bazel cannot compile on Fuchsia.
        def _gn_body_reaches_incompat_ext_dep(raw_gn_body):
            if not fuchsia_incompat_ext_repos:
                return False
            for lbl in re.findall(r'"([^"]+)"', raw_gn_body):
                if lbl.startswith(":") and "(" not in lbl:
                    if lbl[1:] in local_target_incompat_ext:
                        return True
                elif lbl.startswith(f"//{pkg_dir}:") and "(" not in lbl:
                    if lbl.split(":", 1)[1] in local_target_incompat_ext:
                        return True
                elif lbl in gn_to_bazel_ext_repo:
                    if gn_to_bazel_ext_repo[lbl] in fuchsia_incompat_ext_repos:
                        return True
            return False

        fuchsia_incompat_ext_repos = _find_fuchsia_incompat_ext_repos()
        local_target_incompat_ext = _collect_local_target_incompat_ext_deps(
            bazel_targets, fuchsia_incompat_ext_repos
        )
        # Package-level upstream repos lacking `includes` for angled headers used by this
        # package (cq_reachability leaves such tests unexported, so they block migration too).
        try:
            pkg_ext_include_blockers = (
                _pcl_ext.ext_repos_missing_angled_includes(
                    workdir, pkg_dir, open(bazel_abs, encoding="utf-8").read()
                )
                if _pcl_ext is not None
                else []
            )
        except OSError:
            pkg_ext_include_blockers = []

        def bazel_buildable(label):
            """Whether a GN dep label names a target with a Bazel build (conservative)."""
            if "(" in label or "$" in label:
                return False
            if label.startswith(":"):
                return label[1:] in bazel_rules
            if not label.startswith("//"):
                return False
            if label in third_party_gn_labels:
                return True
            path, _, name = label[2:].partition(":")
            try:
                with open(os.path.join(workdir, path, "BUILD.bazel"), encoding="utf-8") as f:
                    text = f.read()
            except OSError:
                return False
            dep_gn_path = os.path.join(workdir, path, "BUILD.gn")
            return _bazel_pkg_has_target(
                text, name or os.path.basename(path), dep_gn_path
            )

        def has_gn_only_test_inputs(raw_body):
            """Whether a GN test target body uses Ninja-generated inputs, GN variables/conditionals,
            or depends on a local GN-only generator/action target that has no Bazel equivalent."""
            body = re.sub(r"#[^\n]*", "", raw_body)
            if "$" in body or re.search(r"\b(?:if|foreach)\s*\(", body):
                return True
            if re.search(r"\bsource_root\s*=\s*[^\s\"#]", body):
                return True
            if re.search(r"\b(?:sources|inputs)\s*\+?=\s*[A-Za-z_]", body):
                return True
            for sm in re.finditer(r"\b(?:sources|inputs)\s*\+?=\s*\[([^\]]*)\]", body):
                if re.sub(r'"[^"]*"|,|\s', "", sm.group(1)):
                    return True
            for dm in re.finditer(
                r"\b(?:public_deps|deps|non_rust_deps|data_deps|test_deps)\s*\+?=\s*\[([^\]]*)\]", body
            ):
                for dep in re.findall(r'"([^"]+)"', dm.group(1)):
                    if dep.startswith(":") and "(" not in dep:
                        dname = dep[1:]
                        if (
                            dname in gn_info["pre_targets"]
                            and dname not in bazel_rules
                            and gn_info["pre_targets"][dname][0] not in CONVERTIBLE_GN_TEMPLATES
                            and gn_info["pre_targets"][dname][0] != "resource"
                        ):
                            return True
            return False

        def test_language(label):
            """Returns "rust", "cc" (a C/C++ test), or None if unknown/unbuildable."""
            if not label.startswith(":") and not label.startswith(f"//{pkg_dir}:"):
                return None
            if "(" in label:
                return None
            name = label[1:] if label.startswith(":") else label.split(":", 1)[1]
            if name in rust_unit_tests:
                return "rust"
            if name.endswith("_test"):
                base_n = name[: -len("_test")]
                if bazel_rules.get(base_n, "") in RUST_UNIT_TEST_OWNERS:
                    return None if base_n in bazel_host_only else "rust"
                if (
                    base_n in gn_info["pre_targets"]
                    and gn_info["pre_targets"][base_n][0].startswith("rustc_")
                ):
                    return "rust"
            rule = bazel_rules.get(name)
            if rule:
                if name in bazel_host_only:
                    return None
                if rule.startswith(("rust", "rustc")):
                    return "rust"
                return "cc" if rule in CC_TEST_BAZEL_RULES else None
            if name not in gn_info["pre_targets"]:
                return None
            ttmpl, _, tbody = gn_info["pre_targets"][name]
            tbody = re.sub(r"#[^\n]*", "", tbody)
            if ttmpl in ("fuchsia_test_component", "fuchsia_unittest_component", "fuchsia_component"):
                comp_deps = []
                for cm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", tbody):
                    comp_deps += re.findall(r'"([^"]+)"', cm.group(1))
                comp_langs = {test_language(cd) for cd in comp_deps} - {None}
                if len(comp_langs) == 1:
                    return next(iter(comp_langs))
                return None
            if has_gn_only_test_inputs(tbody):
                return None
            if ttmpl.startswith("rustc_"):
                return "rust"
            if ttmpl not in GN_CC_TEST_TEMPLATES:
                return None
            srcs = []
            for sm in re.finditer(r"\bsources\s*\+?=\s*\[([^\]]*)\]", tbody):
                srcs += re.findall(r'"([^"]+)"', sm.group(1))
            if any(f.endswith(".rs") for f in srcs):
                return "rust"
            if not srcs or not all(f.endswith(CC_SOURCE_SUFFIXES) for f in srcs):
                return None
            return "cc"

        # Collect local target names transitively reachable from kept pre-sentinel GN test packages.
        def _collect_kept_gn_test_pkg_deps(gn_info_dict, pkg_path, unblocked_pkgs):
            testonly_pkg_tmpls = {
                "fuchsia_package",
                "fuchsia_package_with_single_component",
            }
            pre_t = gn_info_dict.get("pre_targets", {})
            post_t = gn_info_dict.get("post_targets", {})
            clean_pkg = pkg_path.strip("/")

            edges = {}
            for tname, (_, _, tbody) in post_t.items():
                edges[tname] = _gn_body_local_refs(tbody, clean_pkg)
            for tname, (_, _, tbody) in pre_t.items():
                edges[tname] = _gn_body_local_refs(tbody, clean_pkg)

            # Check whether a post-sentinel test binary is packaged by an external consumer
            # BUILD.gn listed in its visibility (or pre-existed in a non-target dependency package).
            def _is_packaged_by_external_gn_consumer(tname, ttmpl, raw_body):
                if not clean_pkg:
                    return False
                if clean_pkg not in target_dirs and ttmpl in (
                    "rustc_test",
                    "test",
                    "cc_test_executable",
                ):
                    return True
                cleaned = re.sub(r"#[^\n]*", "", raw_body)
                vis_pat = r'"//([^":\s*]+):(?:\*|__pkg__|__subpackages__)"'
                for ext_pkg in re.findall(vis_pat, cleaned):
                    ext_gn_path = os.path.join(workdir, ext_pkg.strip("/"), "BUILD.gn")
                    if not os.path.isfile(ext_gn_path):
                        continue
                    try:
                        with open(ext_gn_path, encoding="utf-8") as ef:
                            ext_gn = re.sub(r"#[^\n]*", "", ef.read())
                    except OSError:
                        continue
                    ref_pat = (
                        r'"//'
                        + re.escape(clean_pkg)
                        + r":"
                        + re.escape(tname)
                        + r'(?:"|\()'
                    )
                    if re.search(ref_pat, ext_gn):
                        return True
                return False

            reachable = set()
            queue = []
            visited = set()
            for pname, (ptmpl, _, pbody) in sorted(pre_t.items()):
                if pname in unblocked_pkgs:
                    continue
                pclean = re.sub(r"#[^\n]*", "", pbody)
                is_kept_test_pkg = (
                    ptmpl in GN_ALL_TEST_PACKAGE_TEMPLATES
                    or (
                        ptmpl in testonly_pkg_tmpls
                        and bool(re.search(r"\btestonly\s*=\s*true\b", pclean))
                    )
                )
                if is_kept_test_pkg:
                    visited.add(pname)
                    queue.extend(sorted(edges.get(pname, ())))

            for tname, (ttmpl, _, tbody) in sorted(post_t.items()):
                if _is_packaged_by_external_gn_consumer(tname, ttmpl, tbody):
                    queue.append(tname)

            while queue:
                cur = queue.pop(0)
                if not cur or cur in visited:
                    continue
                visited.add(cur)
                reachable.add(cur)
                if cur in edges:
                    queue.extend(sorted(edges[cur]))
            return reachable

        # Check whether a newly skipped test `fx_package(test_components = [...])` in
        # BUILD.bazel paired with a hand-written `-gn` test package above `## BAZEL2GN SENTINEL`
        # is actually convertible by `bazel2gn` (and identify any unused extra GN deps that the
        # runnable Bazel `fx_package` already omits and no package `.cml` manifest references).
        SUPPORTED_FX_TEST_PKG_KWARGS = {
            "name",
            "package_name",
            "test_components",
            "components",
            "subpackages",
            "visibility",
            "tags",
            "testonly",
        }
        SUPPORTED_FX_TEST_KWARGS = {
            "name",
            "package",
            "max_log_severity",
            "min_log_severity",
            "build_only",
            "environments",
            "test_type",
            "tags",
            "testonly",
            "visibility",
        }

        def _collect_pkg_cml_text(pdir):
            chunks = []
            p_abs = os.path.join(workdir, pdir)
            for cur_root, _, files in os.walk(p_abs):
                if cur_root != p_abs and os.path.isfile(os.path.join(cur_root, "BUILD.bazel")):
                    continue
                for fn in files:
                    if fn.endswith(".cml"):
                        try:
                            chunks.append(
                                open(os.path.join(cur_root, fn), encoding="utf-8").read()
                            )
                        except OSError:
                            pass
            return "\n".join(chunks)

        def _gn_test_pkg_has_blocker(clean_gbody):
            if any(
                re.search(r"\b" + k + r"\s*\+?=", clean_gbody)
                for k in GN_TEST_PACKAGE_BLOCKERS
            ):
                return True
            if "$" in clean_gbody or re.search(r"\b(?:if|foreach)\s*\(", clean_gbody):
                return True
            if re.search(r"\b(?:deps|test_components)\s*\+?=\s*[A-Za-z_]", clean_gbody):
                return True
            ts_m = re.search(r"\btest_specs\s*=\s*\{", clean_gbody)
            if ts_m:
                specs = clean_gbody[ts_m.end() : find_closing_brace(clean_gbody, ts_m.end() - 1)]
                log_m = re.search(r"\blog_settings\s*=\s*\{([^{}]*)\}", specs)
                flat = re.sub(r"\{[^{}]*\}", "{}", specs)
                keys = set(re.findall(r"\b([A-Za-z_]\w*)\s*=", flat))
                if keys - {"log_settings"} or (
                    log_m
                    and set(re.findall(r"\b([A-Za-z_]\w*)\s*=", log_m.group(1)))
                    - {"max_severity", "min_severity"}
                ):
                    return True
            return False

        def _inspect_convertible_skipped_test_fx_pkg(base_pkg_name, clean_gbody):
            """Returns (fx_pkg_target, unused_gn_ext_deps) if the skipped fx_package matching
            base_pkg_name is convertible by bazel2gn, or (None, []) when a real blocker exists."""
            if not gn_info.get("has_sentinel") or _gn_test_pkg_has_blocker(clean_gbody):
                return None, []
            cand = [
                t
                for t in bazel_targets
                if t["rule"] == "fx_package"
                and t.get("has_test_components")
                and t.get("skip_line") is not None
                and t["package_name"] == base_pkg_name
            ]
            if len(cand) != 1:
                return None, []
            pt = cand[0]
            pname = pt["name"]
            if pt.get("has_attention_marker"):
                return None, []
            try:
                upstream_bz = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"origin/main:{bazel_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                upstream_bz = ""
            upstream_skips = {
                bt["name"]: bt["skip_line"] is not None
                for bt in (
                    parse_bazel_targets(bazel_rel, pkg_dir, text=upstream_bz)
                    if upstream_bz
                    else []
                )
            }
            if upstream_skips.get(pname):
                return None, []
            bz_path = os.path.join(workdir, bazel_rel)
            try:
                bz_src = open(bz_path, encoding="utf-8").read()
                bz_tree = ast.parse(bz_src, filename=bz_path)
            except Exception:
                return None, []
            pkg_call = None
            fx_test_calls = []
            for node in bz_tree.body:
                if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
                    continue
                c = node.value
                if not isinstance(c.func, ast.Name):
                    continue
                cname = None
                cpkg = None
                for kw in c.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        cname = kw.value.value
                    elif (
                        kw.arg == "package"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        cpkg = kw.value.value
                if c.func.id == "fx_package" and cname == pname:
                    pkg_call = c
                elif c.func.id == "fx_test" and cpkg in (f":{pname}", f"//{pkg_dir}:{pname}"):
                    fx_test_calls.append(c)
            if pkg_call is None:
                return None, []
            pkg_kw_names = {kw.arg for kw in pkg_call.keywords if kw.arg}
            if not (pkg_kw_names <= SUPPORTED_FX_TEST_PKG_KWARGS):
                return None, []
            # Verify all test_components are same-package unskipped fx_test_component /
            # fx_unittest_component targets whose underlying test binaries are unskipped.
            tc_names = []
            pkg_str_refs = set()
            for kw in pkg_call.keywords:
                if kw.arg == "test_components":
                    if not isinstance(kw.value, ast.List) or not kw.value.elts:
                        return None, []
                    for elt in kw.value.elts:
                        if not (
                            isinstance(elt, ast.Constant)
                            and isinstance(elt.value, str)
                            and elt.value.startswith(":")
                        ):
                            return None, []
                        tc_names.append(elt.value[1:])
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        pkg_str_refs.add(sub.value)
            local_targets_by_name = {t["name"]: t for t in bazel_targets}
            for tc_n in tc_names:
                tc_t = local_targets_by_name.get(tc_n)
                if (
                    not tc_t
                    or tc_t["rule"] not in ("fx_test_component", "fx_unittest_component")
                    or tc_t.get("skip_line") is not None
                ):
                    return None, []
                for dep_n in tc_t.get("dep_refs", ()):
                    dep_t = local_targets_by_name.get(dep_n)
                    if dep_t and dep_t.get("skip_line") is not None:
                        return None, []
            # Verify companion fx_test targets use only bazel2gn-supported kwargs and run (not
            # build_only = True).
            for ft_call in fx_test_calls:
                ft_kw_names = {kw.arg for kw in ft_call.keywords if kw.arg}
                if not (ft_kw_names <= SUPPORTED_FX_TEST_KWARGS):
                    return None, []
                for kw in ft_call.keywords:
                    if (
                        kw.arg == "build_only"
                        and isinstance(kw.value, ast.Constant)
                        and kw.value.value is True
                    ):
                        return None, []
                    if kw.arg == "environments":
                        if not isinstance(kw.value, ast.List):
                            return None, []
                        for elt in kw.value.elts:
                            if not (
                                isinstance(elt, ast.Constant)
                                and isinstance(elt.value, str)
                                and elt.value.startswith("//build/testing/environments:")
                            ):
                                return None, []
            # Check external GN deps on the hand-written GN test package that are absent from
            # the runnable Bazel fx_package: if any package .cml manifest references the target,
            # treat it as an active runtime component dep; otherwise record it as unused.
            gn_dep_labels = []
            for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", clean_gbody):
                gn_dep_labels += re.findall(r'"([^"]+)"', dm.group(1))
            cml_blob = _collect_pkg_cml_text(pkg_dir)
            unused_ext_deps = []
            for dlbl in gn_dep_labels:
                if not dlbl.startswith("//") or dlbl.startswith(f"//{pkg_dir}:"):
                    continue
                if dlbl in pkg_str_refs:
                    continue
                dep_body = dlbl[2:].split("(", 1)[0]
                dep_leaf = (
                    dep_body.split(":", 1)[1]
                    if ":" in dep_body
                    else os.path.basename(dep_body.strip("/"))
                )
                if any(
                    r.endswith(f":{dep_leaf}") or r.endswith(f"/{dep_leaf}")
                    for r in pkg_str_refs
                ):
                    continue
                if dep_leaf and re.search(r"\b" + re.escape(dep_leaf) + r"\b", cml_blob):
                    return None, []
                if dlbl not in unused_ext_deps:
                    unused_ext_deps.append(dlbl)
            return pt, unused_ext_deps

        unblocked_gn_test_pkgs = set()
        for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
            if gtmpl not in GN_TEST_PACKAGE_TEMPLATES:
                continue
            gbody = re.sub(r"#[^\n]*", "", gbody)
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"]+)"', gbody)
            gn_pkg_name = pn_m.group(1) if pn_m else gname
            if gn_pkg_name in bazel_test_pkg_names:
                unblocked_gn_test_pkgs.add(gname)
                _emit(findings, "duplicate_gn_test_package", "error", gn_rel, gline, (
                        f"GN `{gtmpl}(\"{gname}\")` in '{gn_rel}' builds package `{gn_pkg_name}`, which "
                        f"`fx_package` in '{bazel_rel}' also builds; both would publish the same test package."
                    ), (
                        f"Delete `{gtmpl}(\"{gname}\")` (and test executables only it used) from '{gn_rel}' and "
                        f"expose the Bazel test in '{bazel_rel}' via a package `test_suite()` (`tests` or `host_tests`) "
                        f"wired into the area's aggregation `test_suite()` (never add a per-package `bazel_test_suite` in '{gn_rel}')."
                    ))
                continue
            if gn_pkg_name.endswith("-gn") and gn_pkg_name[: -len("-gn")] in bazel_test_pkg_names:
                base_pkg_name = gn_pkg_name[: -len("-gn")]
                conv_pt, unused_ext_deps = _inspect_convertible_skipped_test_fx_pkg(
                    base_pkg_name, gbody
                )
                if base_pkg_name in bazel_untranslatable_test_pkg_names and conv_pt is None:
                    # The GN copy of a Bazel test package that bazel2gn can't translate (skipped
                    # fx_package chain, fx_unittest_package with resources, or unsupported test binary).
                    continue
                if gn_info["has_sentinel"]:
                    unblocked_gn_test_pkgs.add(gname)
                    if conv_pt is not None:
                        pname = conv_pt["name"]
                        unused_note = (
                            " Extra GN package dep(s) "
                            + ", ".join(f"`{d}`" for d in unused_ext_deps)
                            + " are already omitted by the runnable Bazel `fx_package` and "
                            "unreferenced in package `.cml` manifests, so they are unused and "
                            "can be dropped."
                            if unused_ext_deps
                            else ""
                        )
                        drop_dep_step = (
                            "drop the unused GN dep(s) "
                            + ", ".join(f"`{d}`" for d in unused_ext_deps)
                            + ", "
                            if unused_ext_deps
                            else ""
                        )
                        _emit(
                            findings,
                            "duplicate_test_above_sentinel",
                            "error",
                            bazel_rel,
                            conv_pt["skip_line"],
                            (
                                f"GN `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in "
                                f"'{gn_rel}' duplicates convertible Bazel "
                                f"`fx_package(name = \"{pname}\")` in '{bazel_rel}' "
                                "(annotated with `# @bazel2gn:skip`), which `bazel2gn` "
                                f"translates directly into `fuchsia_test_package(\"{pname}\")` "
                                "below the sentinel." + unused_note
                            ),
                            (
                                f"In '{bazel_rel}', remove `# @bazel2gn:skip` (and its comment) "
                                f"above `fx_package(name = \"{pname}\")`; in '{gn_rel}', "
                                f"{drop_dep_step}delete `{gtmpl}(\"{gname}\")` above "
                                "`## BAZEL2GN SENTINEL`, update `group(\"tests\")` to list "
                                f"`\":{pname}\"`, and re-run `fx bazel2gn -d {pkg_dir}`."
                            ),
                        )
                        continue
                    _emit(findings, "duplicate_test_above_sentinel", "error", gn_rel, gline, (
                            f"GN `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' duplicates "
                            f"Bazel test package `{base_pkg_name}` in '{bazel_rel}' (an `fx_unittest_package` "
                            "without `resources` or an `fx_package` with `test_components`), which `bazel2gn` "
                            "translates directly below the sentinel."
                        ), (
                            f"Delete `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}', remove any "
                            f"`# @bazel2gn:skip` on the Bazel package of `{base_pkg_name}` in '{bazel_rel}', "
                            f"list the generated package in `group(\"tests\")`, and re-run `fx bazel2gn -d {pkg_dir}`."
                        ))
                    continue
            blockers = [k for k in GN_TEST_PACKAGE_BLOCKERS if re.search(r"\b" + k + r"\s*\+?=", gbody)]
            if "$" in gbody or re.search(r"\b(?:if|foreach)\s*\(", gbody):
                blockers.append("variables/conditionals")
            if re.search(r"\b(?:deps|test_components)\s*\+?=\s*[A-Za-z_]", gbody):
                blockers.append("deps from a variable")
            ts_m = re.search(r"\btest_specs\s*=\s*\{", gbody)
            if ts_m:
                specs = gbody[ts_m.end() : find_closing_brace(gbody, ts_m.end() - 1)]
                log_m = re.search(r"\blog_settings\s*=\s*\{([^{}]*)\}", specs)
                flat = re.sub(r"\{[^{}]*\}", "{}", specs)
                keys = set(re.findall(r"\b([A-Za-z_]\w*)\s*=", flat))
                if keys - {"log_settings"} or (
                    log_m
                    and set(re.findall(r"\b([A-Za-z_]\w*)\s*=", log_m.group(1)))
                    - {"max_severity", "min_severity"}
                ):
                    blockers.append("test_specs")
            labels = []
            for lm in re.finditer(r"\b(?:deps|test_components)\s*\+?=\s*\[([^\]]*)\]", gbody):
                labels += re.findall(r'"([^"]+)"', lm.group(1))
            pkg_test_deps = set()
            # External repositories that keep the migrated Bazel test out of Bazel test suites
            # do not block migrating it (complete-directory policy): the test is still migrated
            # and dual-built, but left unexported (shared external-repo verdict).
            ext_test_blockers = []
            seed_texts = [gbody, open(bazel_abs, encoding="utf-8").read()]
            # Look up the test package's local deps in both pre-sentinel and bazel2gn-generated
            # post-sentinel GN targets.
            all_gn_targets = {
                **gn_info.get("post_targets", {}),
                **gn_info["pre_targets"],
            }
            for l in labels:
                local = ""
                if "(" not in l:
                    if l.startswith(":"):
                        local = l[1:]
                    elif l.startswith(f"//{pkg_dir}:"):
                        local = l.split(":", 1)[1]
                if local:
                    pkg_test_deps.add(local)
                    comp = all_gn_targets.get(local)
                    if comp and comp[0] in ("fuchsia_test_component", "fuchsia_unittest_component", "fuchsia_component"):
                        for cm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", comp[2]):
                            for cl in re.findall(r'"([^"]+)"', cm.group(1)):
                                if "(" not in cl:
                                    if cl.startswith(":"):
                                        pkg_test_deps.add(cl[1:])
                                    elif cl.startswith(f"//{pkg_dir}:"):
                                        pkg_test_deps.add(cl.split(":", 1)[1])
            for td_name in sorted(pkg_test_deps):
                owner = all_gn_targets.get(td_name) or all_gn_targets.get(
                    re.sub(r"_test$", "", td_name)
                )
                if owner:
                    seed_texts.append(owner[2])
                    if GN_ONLY_ATTR_RE.search(owner[2]):
                        blockers.append("GN-only attributes (see gn_attr_parity)")
                    if has_gn_only_test_inputs(owner[2]):
                        blockers.append("Ninja-generated inputs or GN-only generator deps")
                    if _gn_body_reaches_incompat_ext_dep(owner[2]):
                        ext_test_blockers.append(
                            "external Bazel dependency incompatible with @platforms//os:fuchsia"
                        )
                elif td_name in local_target_incompat_ext:
                    ext_test_blockers.append(
                        "external Bazel dependency incompatible with @platforms//os:fuchsia"
                    )
            if _gn_body_reaches_incompat_ext_dep(gbody):
                ext_test_blockers.append(
                    "external Bazel dependency incompatible with @platforms//os:fuchsia"
                )
            if pkg_ext_include_blockers:
                ext_test_blockers.append(
                    "upstream external repository lacking `includes` ("
                    + ", ".join(pkg_ext_include_blockers)
                    + ")"
                )
            # Unknown or unbuildable tests never trigger it.
            langs = {test_language(l) for l in labels}
            if blockers or not labels or not langs <= set(MIGRATABLE_TEST_LANGUAGES):
                gn_kept_pkg_test_deps.update(pkg_test_deps)
                continue
            unblocked_gn_test_pkgs.add(gname)
            is_rust = "rust" in langs
            _emit(findings, "gn_test_package_not_migrated", "error", gn_rel, gline, (
                    f"GN `{gtmpl}(\"{gname}\")` in '{gn_rel}' packages C/C++ or Rust tests "
                    f"({', '.join(labels)}) and uses nothing Bazel device tests cannot express, "
                    "but it stays in GN although this checkout has `//build/bazel/rules/testing:fx_test.bzl`."
                ), (
                    f"Migrate it as in \"Migrating Tests\" of the coder instructions: in '{bazel_rel}' declare "
                    + (
                        "an explicit `rustc_test(...)`, "
                        if is_rust
                        else "`fx_cc_test_binary(...)` for a GN `test()` or `cc_test_executable()`, "
                    )
                    + f"and package it with `fx_unittest_package(name = \"{gname}\", "
                    + (f"package_name = \"{gn_pkg_name}\", " if gn_pkg_name != gname else "")
                    + "unit_tests = [\":<test_bin>\"])` "
                    f"(bazel2gn generates the GN copy as package `{gn_pkg_name}-gn`), delete the GN package and "
                    "executable, and re-run `fx bazel2gn` "
                    "(or `fx_unittest_component` + `fx_package` + `fx_test` when packaging extra `resource()`s/components; "
                    "only unroll into `fx_packaged_binary` + `fx_component_manifest` + `fx_test_component` + `fx_package` + `fx_test` "
                    "with a `meta/<component>.cml` when the test requires a custom `.cml` manifest or non-default capability shards, in "
                    f"which case keep this GN package above the sentinel as the GN copy with `package_name = \"{gn_pkg_name}-gn\"` and its "
                    "executable replaced by the generated test binary while the library has GN users). "
                    + (
                        "Do not export the Bazel test because "
                        + ", ".join(dict.fromkeys(ext_test_blockers))
                        + " blocks it (this external repository does not block migrating the "
                        "directory); list only the GN copy in `group(\"tests\")`. "
                        + (
                            _pcl_ext.external_repo_blocked_test_remediation(
                                pkg_dir, f"`{gname}`"
                            )
                            if _pcl_ext is not None
                            else ""
                        )
                        + " "
                        if ext_test_blockers
                        else "Export the Bazel test through a package `test_suite()` (`tests`) wired into the nearest "
                        f"ancestor aggregation `test_suite()` (never add a per-package `bazel_test_suite` in '{gn_rel}') "
                        "and keep the GN copy in `group(\"tests\")`. "
                    )
                    + f"Do not partially migrate '{pkg_dir}': if a test dependency is "
                    f"blocked by an out-of-scope external package, revert the directory migration (`rm -f {bazel_rel} && git checkout -- {gn_rel}`) "
                    f"to leave '{pkg_dir}' completely unmigrated (`modified_files: []`), or if an unavoidable local exception "
                    "keeps it in GN only, report it via `disputed_findings` in `CoderReport`."
                ))

        # Include local targets transitively packaged by kept pre-sentinel GN test packages.
        gn_kept_pkg_test_deps.update(
            _collect_kept_gn_test_pkg_deps(gn_info, pkg_dir, unblocked_gn_test_pkgs)
        )

        # Flag unskipped Bazel device test targets whose same-package dependency closure
        # reaches an upstream external repository that cannot compile on Fuchsia when the
        # package is not dual-built. For dual-built packages cq_reachability gives the shared
        # verdict (external_repo_incompatible_bazel_test_export when wired into an ancestor
        # test_suite, external_repo_blocked_bazel_test_export INFO when left unexported).
        def _check_unbuildable_ext_dep_device_tests():
            if pkg_dir not in target_dirs or not local_target_incompat_ext:
                return
            if gn_info and gn_info.get("has_sentinel"):
                return
            for bt in bazel_targets:
                if bt["rule"] not in ("fx_unittest_package", "fx_test") or bt.get("skipped"):
                    continue
                tname = bt["name"]
                bad_repos = local_target_incompat_ext.get(tname, [])
                if not bad_repos:
                    continue
                joined_repos = ", ".join(f"`{r}`" for r in bad_repos)
                msg = (
                    f"Bazel `{bt['rule']}(name = \"{tname}\")` in '{bazel_rel}' "
                    f"transitively depends on external repository {joined_repos}, whose "
                    "upstream `BUILD.bazel` cannot compile under `--config=fuchsia_platform` "
                    "(missing Fuchsia FIDL/SDK deps in upstream third-party sources), "
                    "causing `fx build` to fail on CQ builders when exported via a "
                    f"`test_suite` / `bazel_test_suite`, and '{gn_rel}' has no "
                    "`## BAZEL2GN SENTINEL`, so no generated GN copy of the test runs in CQ."
                )
                rem = (
                    _pcl_ext.external_repo_blocked_test_remediation(
                        pkg_dir, f"`{bt['rule']}(name = \"{tname}\")`", bad_repos
                    )
                    if _pcl_ext is not None
                    else (
                        f"Keep '{gn_rel}' dual-built with `{tname}` synced below "
                        f"`## BAZEL2GN SENTINEL` (`fx bazel2gn -d {pkg_dir}`) and remove "
                        f"`\"//{pkg_dir}:{tname}\"` from any ancestor `test_suite`."
                    )
                )
                _emit(
                    findings,
                    "unbuildable_external_dep_device_test",
                    "error",
                    bazel_rel,
                    bt["line"],
                    msg,
                    rem,
                )

        _check_unbuildable_ext_dep_device_tests()

    gn_kept_pkg_test_deps_by_dir[pkg_dir] = set(gn_kept_pkg_test_deps)

    # Check that non-test fuchsia_package / fuchsia_package_with_single_component targets
    # above the sentinel whose components are already migrated to fx_component in
    # BUILD.bazel (and that only serve assembly or have an assembly bazel_inputs bridge)
    # are migrated to # @bazel2gn:skip fx_package in BUILD.bazel and removed from BUILD.gn.
    def check_unmigrated_gn_fuchsia_packages(btargets, brel, grel, pdir, g_info):
        if not g_info:
            return
        fx_pkg_names = {
            t["name"] for t in btargets if t["rule"] in ("fx_package", "fuchsia_package")
        } | {
            t["package_name"]
            for t in btargets
            if t["rule"] in ("fx_package", "fuchsia_package")
        }
        bz_comp_names = {
            t["name"]
            for t in btargets
            if t["rule"] in ("fx_component", "resource", "fuchsia_package_resource_group")
        }
        bz_bin_names = {
            t["name"]
            for t in btargets
            if t["rule"]
            in ("rustc_binary", "rust_binary", "fx_cc_binary", "cc_binary", "go_binary", "fx_packaged_binary")
        }
        bz_pkg_dep_names = bz_comp_names | bz_bin_names
        bridge_gn_abs = os.path.join(
            workdir, "bundles", "assembly", "bazel_inputs", pdir, "BUILD.gn"
        )
        has_bridge = os.path.isfile(bridge_gn_abs)
        bridge_txt = ""
        if has_bridge:
            try:
                with open(bridge_gn_abs, encoding="utf-8") as bf:
                    bridge_txt = bf.read()
            except OSError:
                bridge_txt = ""
        for gname, (gtmpl, gline, gbody) in sorted(g_info["pre_targets"].items()):
            if gtmpl not in ("fuchsia_package", "fuchsia_package_with_single_component"):
                continue
            cleaned = re.sub(r"#[^\n]*", "", gbody)
            is_testonly_pkg = bool(re.search(r"\btestonly\s*=\s*true\b", cleaned))
            is_bridged_in_assembly = bool(
                bridge_txt
                and re.search(
                    r'"//' + re.escape(pdir) + r"(?::" + re.escape(gname) + r')?"',
                    bridge_txt,
                )
            )
            if (
                (is_testonly_pkg and not is_bridged_in_assembly)
                or "$" in cleaned
                or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
                or GN_ONLY_ATTR_RE.search(cleaned)
            ):
                continue
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"]+)"', cleaned)
            gn_pkg_name = pn_m.group(1) if pn_m else gname
            if gname in fx_pkg_names or gn_pkg_name in fx_pkg_names:
                continue
            # Keep fuchsia_package in BUILD.gn when another retained pre-sentinel GN
            # target in the same BUILD.gn (such as sdk_fuchsia_package via package_label)
            # references it directly.
            if any(
                other_n != gname
                and re.search(
                    r'"(?::|' + re.escape(f"//{pdir}:") + r")" + re.escape(gname) + r'(?:"|\()',
                    re.sub(r"#[^\n]*", "", other_body),
                )
                for other_n, (_ot, _ol, other_body) in g_info["pre_targets"].items()
            ):
                continue
            dep_lbls = []
            for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", cleaned):
                dep_lbls.extend(re.findall(r'"([^"]+)"', dm.group(1)))
            if not dep_lbls:
                continue
            dep_locals = [
                d[1:]
                if d.startswith(":")
                else (d.split(":", 1)[1] if pdir and d.startswith(f"//{pdir}:") else "")
                for d in dep_lbls
            ]
            if not all(d and d in bz_pkg_dep_names for d in dep_locals):
                continue
            vis_lbls = []
            for vm in re.finditer(r"\bvisibility\s*\+?=\s*\[([^\]]*)\]", cleaned):
                vis_lbls.extend(re.findall(r'"([^"]+)"', vm.group(1)))
            assembly_only_vis = bool(vis_lbls) and all(
                v.startswith("//bundles/assembly") or v in (":*", f"//{pdir}:*")
                for v in vis_lbls
            )
            if not (assembly_only_vis or has_bridge or is_bridged_in_assembly):
                continue
            # Keep fuchsia_package in BUILD.gn when a packaged component binary depends
            # on a rustc_dylib target and collect_debug_symbols_manifest_aspect in
            # //build/bazel/debug_symbols/aspects.bzl does not yet collect rust_dylib_library.
            dylib_dep = find_uncollected_rust_dylib_dep(pdir, dep_locals)
            if dylib_dep:
                info_msg = (
                    f'GN `{gtmpl}("{gname}")` in \'{grel}\' stays above '
                    "`## BAZEL2GN SENTINEL` because its packaged component binary "
                    f"depends on dual-built `rustc_dylib` `{dylib_dep}`, which would "
                    "duplicate the shared dylib in BlobFS across GN and Bazel packages "
                    "in assembled images."
                )
                info_rem = (
                    f'Keep `{gtmpl}("{gname}")` in \'{grel}\' bridged via '
                    f"`//bundles/assembly/bazel_inputs/{pdir}` while `{dylib_dep}` is "
                    "dual-built in GN and shared by GN-assembled packages."
                )
                _emit(findings, "gn_fuchsia_package_kept_for_rust_dylib_debug_symbols", "INFO", grel, gline, info_msg, info_rem)
                continue
            pkg_attr = (
                f'package_name = "{gn_pkg_name}", ' if gn_pkg_name != gname else ""
            )
            testonly_attr = "testonly = True, " if is_testonly_pkg else ""
            bz_rule_by_name = {t["name"]: t["rule"] for t in btargets}
            comp_deps = [d for d in dep_locals if bz_rule_by_name.get(d) == "fx_component"]
            res_deps = [
                d
                for d in dep_locals
                if bz_rule_by_name.get(d) in ("resource", "fuchsia_package_resource_group")
            ]
            bin_deps = [d for d in dep_locals if d in bz_bin_names]
            comp_list = ", ".join(f'":{d}"' for d in dep_locals)
            fx_pkg_kwargs = []
            if comp_deps:
                fx_pkg_kwargs.append("components = [" + ", ".join(f'":{d}"' for d in comp_deps) + "]")
            res_and_bin_lbls = [f'":{d}"' for d in res_deps]
            if bin_deps:
                for d in bin_deps:
                    if bz_rule_by_name.get(d) == "fx_packaged_binary":
                        res_and_bin_lbls.append(f'":{d}"')
                    elif bz_rule_by_name.get(f"{d}_packaged_bin") == "fx_packaged_binary":
                        res_and_bin_lbls.append(f'":{d}_packaged_bin"')
                    elif bz_rule_by_name.get(f"{d}_packaged") == "fx_packaged_binary":
                        res_and_bin_lbls.append(f'":{d}_packaged"')
                    else:
                        res_and_bin_lbls.append(f'":{d}_packaged_bin"')
            if res_and_bin_lbls:
                fx_pkg_kwargs.append("resources = [" + ", ".join(res_and_bin_lbls) + "]")
            kwargs_str = (", ".join(fx_pkg_kwargs) + ", ") if fx_pkg_kwargs else ""
            bin_note = (
                " (wrapping each raw binary in `# @bazel2gn:skip` `fx_packaged_binary(name = \"<bin>_packaged_bin\", binary = \":<bin>\")`)"
                if any(bz_rule_by_name.get(d) != "fx_packaged_binary" for d in bin_deps)
                else ""
            )
            msg = (
                f'GN `{gtmpl}("{gname}")` in \'{grel}\' packages target(s) '
                f"({comp_list}) that are already migrated to `BUILD.bazel`, and "
                "shared library / `rustc_dylib` runfiles deduplication is supported "
                "in `fx_package`, but the package target remains in `'BUILD.gn'`."
            )
            rem = (
                f'Add `# @bazel2gn:skip` `fx_package(name = "{gname}", {pkg_attr}{testonly_attr}'
                f'{kwargs_str}visibility = ["//bundles/assembly:__subpackages__"])`{bin_note} '
                f"in '{brel}', delete `{gtmpl}(\"{gname}\")` from '{grel}', and "
                f"replace any `//bundles/assembly/bazel_inputs/{pdir}` bridge with "
                f"`//{pdir}:{gname}` in `bundles/assembly/BUILD.bazel`."
            )
            # Remove any optimistic gn_package_kept_above_sentinel INFO finding for this package
            findings[:] = [
                f
                for f in findings
                if not (
                    f.get("category") == "gn_package_kept_above_sentinel"
                    and f.get("file") == grel
                    and f.get("line") == gline
                )
            ]
            _emit(findings, "duplicate_gn_fuchsia_package", "error", grel, gline, msg, rem)

    # Flag retained targets above ## BAZEL2GN SENTINEL whose relative order in BUILD.gn
    # changed compared to change_base, which produces unnecessary movement diffs.
    def check_reordered_gn_targets_above_sentinel(grel, g_info):
        if not g_info or not g_info.get("pre_targets"):
            return
        try:
            base_gn_raw = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{grel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            return
        base_parsed = parse_gn_text(base_gn_raw)
        base_all = {}
        base_all.update(base_parsed.get("pre_targets", {}))
        base_all.update(base_parsed.get("post_targets", {}))
        cur_pre = g_info["pre_targets"]
        common = [
            name
            for name, (_t, ln, _b) in sorted(cur_pre.items(), key=lambda kv: kv[1][1])
            if name in base_all
        ]
        base_order = [
            name
            for name, (_t, ln, _b) in sorted(base_all.items(), key=lambda kv: kv[1][1])
            if name in cur_pre
        ]
        if len(common) >= 2 and common != base_order:
            first_diff = next(c for c, b in zip(common, base_order) if c != b)
            diff_ln = cur_pre[first_diff][1]
            _emit(findings, "reordered_gn_targets_above_sentinel", "error", grel, diff_ln, (
                    f"Targets retained above `## BAZEL2GN SENTINEL` in '{grel}' were reordered "
                    f"({common} vs original order {base_order}), causing spurious diffs in `BUILD.gn`."
                ), (
                    f"Preserve the original relative order of targets kept above `## BAZEL2GN SENTINEL` "
                    f"in '{grel}': {base_order}."
                ))

    check_unmigrated_gn_fuchsia_packages(
        bazel_targets, bazel_rel, gn_rel, pkg_dir, gn_info
    )
    check_reordered_gn_targets_above_sentinel(gn_rel, gn_info)
    if gn_info and gn_info["has_sentinel"]:
        # A package-private testonly helper that only skipped (Bazel-only) tests use would
        # otherwise be generated into BUILD.gn with no GN consumer.
        for t in bazel_targets:
            if (
                t["rule"] in CONVERTIBLE_BAZEL_RULES
                and t["skip_line"] is None
                and t["name"] not in auto_skipped_manifests
                and is_bazel_only_private_test_dep(
                    t, bazel_targets, gn_info, pkg_dir, packaged_binaries, gn_kept_pkg_test_deps
                )
            ):
                _emit(findings, "orphaned_test_helper", "error", bazel_rel, t["line"], (
                        f"Package-private test-only target `{t['rule']}(name = \"{t['name']}\")` in '{bazel_rel}' "
                        "is only consumed by Bazel tests in the same package that have `# @bazel2gn:skip`, but has no "
                        f"`# @bazel2gn:skip` itself, so bazel2gn generates an orphaned GN target in '{gn_rel}'."
                    ), (
                        f"Put `# @bazel2gn:skip` directly above `{t['rule']}(name = \"{t['name']}\")` in "
                        f"'{bazel_rel}', preceded by a one-line reason comment (e.g. `# Bazel-only: used "
                        "only by skipped Bazel tests.`; cc_header_deps_and_annotations requires it, and it "
                        "keeps the skip valid once visibility_audit has you drop a redundant private "
                        f"`visibility`), and re-run `fx bazel2gn -d {pkg_dir}`."
                    ))

    if fx_pkgs:
        fx_pkg_name = fx_pkgs[0]["name"]
        # Check whether an assembled fx_package packages a component whose binary depends on a
        # dual-built rustc_dylib target (or when aspects.bzl omits rust_dylib_library).
        def check_fx_package_rust_dylib_debug_symbols(f_pkgs, brel, grel, pdir):
            found_dylib = False
            bridge_rel = f"bundles/assembly/bazel_inputs/{pdir}/BUILD.gn"
            has_pkg_bridge = os.path.isfile(os.path.join(workdir, bridge_rel)) or bool(
                git_lines(["show", f"{change_base}:{bridge_rel}"])
            )
            for fpkg in f_pkgs:
                vis = fpkg.get("vis_items") or []
                is_assembled = has_pkg_bridge or any(
                    v.startswith("//bundles/assembly") for v in vis
                )
                if not is_assembled:
                    asm_bz = os.path.join(workdir, "bundles", "assembly", "BUILD.bazel")
                    if os.path.isfile(asm_bz):
                        try:
                            asm_txt = open(asm_bz, encoding="utf-8").read()
                            is_assembled = f'"//{pdir}' in asm_txt
                        except OSError:
                            pass
                if not is_assembled:
                    continue
                seeds = [fpkg["name"]] + sorted(fpkg.get("dep_refs", ()))
                dylib_dep = find_uncollected_rust_dylib_dep(pdir, seeds)
                if not dylib_dep:
                    continue
                # Reject non-test assembled fx_package targets whose binary transitively
                # depends on a dual-built rustc_dylib (intra-Bazel runfiles dedup does not
                # dedupe across the GN/Bazel BlobFS boundary).
                found_dylib = True
                pname = fpkg["name"]
                msg = (
                    f'Target `fx_package(name = "{pname}")` in \'{brel}\' packages a binary '
                    f"that depends on dual-built `rustc_dylib` `{dylib_dep}`. Switching "
                    "assembled packages that link a dual-built `rustc_dylib` from a GN "
                    "`fuchsia_package` to a Bazel `fx_package` ships a second copy of the "
                    "shared dylib in BlobFS alongside the GN-built copy shared by remaining "
                    "GN packages (exceeding release binary size creep budgets on size-checked "
                    "builders). Intra-package `fx_package` runfiles deduplication and "
                    "`aspects.bzl` debug-symbol support do NOT deduplicate across the GN/Bazel "
                    "boundary, so a passing local `fx bazel build` is not a ground to dispute "
                    "this finding."
                )
                rem = (
                    f'Do NOT dispute this finding in `disputed_findings`. Remove '
                    f'`fx_package(name = "{pname}")` (and any unused `fx_package` load) from '
                    f"'{brel}', keep `fuchsia_package(\"{pname}\")` above `## BAZEL2GN SENTINEL` "
                    f"in '{grel}' (while keeping `fx_component`, `fx_component_manifest`, and "
                    f"`fx_packaged_binary` in '{brel}'), restore `testonly = True` on any "
                    "component or manifest targets whose `testonly` attribute was dropped only "
                    "for the `fx_package` cutover (and re-run `fx bazel2gn`), restore any "
                    f"deleted parent `BUILD.gn` group references to `{pname}`, keep the "
                    f"`//bundles/assembly/bazel_inputs/{pdir}` bridge in "
                    "`bundles/assembly/BUILD.bazel` and `bundles/assembly/bazel_inputs/BUILD.gn`, "
                    f"remove `Bazel-Migration-Target: //{pdir}:{pname}` and any commit-body "
                    "claims about migrating the package, and never include "
                    "`fx_package_missing_rust_dylib_debug_symbols` in `disputed_findings`."
                )
                _emit(
                    findings,
                    "fx_package_missing_rust_dylib_debug_symbols",
                    "error",
                    brel,
                    fpkg["line"],
                    msg,
                    rem,
                )
            return found_dylib

        has_uncollected_dylib_fx_pkg = check_fx_package_rust_dylib_debug_symbols(
            fx_pkgs, bazel_rel, gn_rel, pkg_dir
        )
        bazel_text_raw = open(bazel_abs, encoding="utf-8").read()
        migrated_pkg_names = {t["name"] for t in bazel_targets} | {t["package_name"] for t in fx_pkgs}
        migrated_manifests = set(re.findall(r'\bmanifest\s*=\s*"([^"]+)"', bazel_text_raw))

        # Exclude GN package/component targets that still have active GN reverse dependencies
        # and cannot be generated below the sentinel by bazel2gn.
        def filter_kept_gn_package_targets(pdir, pre_targets, btargets, has_sentinel, cand_dups):
            if not cand_dups:
                return set()
            default_t = os.path.basename(pdir)
            ref_files = set(
                git_lines(
                    [
                        "grep",
                        "-l",
                        "--fixed-strings",
                        f"//{pdir}",
                        "--",
                        "*BUILD.gn",
                        "*.gni",
                    ]
                )
            )
            parts = pdir.split("/")
            for i in range(1, len(parts)):
                anc_rel = "/".join(parts[:i]) + "/BUILD.gn"
                if os.path.isfile(os.path.join(workdir, anc_rel)):
                    ref_files.add(anc_rel)
            abs_re = re.compile(
                r'"//' + re.escape(pdir) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
            )
            ext_rdeps = set()
            for rel in sorted(ref_files):
                if (
                    rel == f"{pdir}/BUILD.gn"
                    or rel.startswith("bundles/assembly/bazel_inputs/")
                    or os.path.basename(rel) == "visibility.gni"
                ):
                    continue
                try:
                    raw = open(os.path.join(workdir, rel), encoding="utf-8").read()
                except Exception:
                    continue
                cleaned = re.sub(
                    r"\bvisibility\s*\+?=\s*\[[^\]]*\]",
                    "",
                    re.sub(r"#[^\n]*", "", raw),
                    flags=re.DOTALL,
                )
                for m in abs_re.finditer(cleaned):
                    ext_rdeps.add(m.group(1) or default_t)
                caller_pkg = os.path.dirname(rel).strip("/")
                if caller_pkg and pdir.startswith(caller_pkg + "/"):
                    rel_sub = pdir[len(caller_pkg) + 1 :]
                    rel_re = re.compile(
                        r'"' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
                    )
                    for m in rel_re.finditer(cleaned):
                        ext_rdeps.add(m.group(1) or default_t)
            b2gn_conv = {
                n
                for n in cand_dups
                if (
                    has_sentinel
                    and pre_targets[n][0] == "fuchsia_component"
                    and not is_gn_component_target_blocked(
                        n, pre_targets[n][0], pre_targets[n][2], pre_targets, btargets, pdir
                    )
                )
                or pre_targets[n][0] in ("fuchsia_package", "fuchsia_package_with_single_component")
            }
            kept = {
                n
                for n in pre_targets
                if n not in cand_dups or (n in ext_rdeps and n not in b2gn_conv)
            }
            changed = True
            while changed:
                changed = False
                for k in list(kept):
                    kbody = pre_targets[k][2]
                    for c in list(cand_dups - kept - b2gn_conv):
                        if re.search(
                            r'"(?::|//' + re.escape(pdir) + r':)' + re.escape(c) + r'(?:"|\()',
                            kbody,
                        ):
                            kept.add(c)
                            changed = True
            return cand_dups - kept

        if gn_info:
            def _gn_pkg_has_external_gn_callers(pkg_d, target_n):
                try:
                    hit_files = subprocess.check_output(
                        ["git", "-C", workdir, "grep", "-Fl", f"//{pkg_d}", "--", "*.gn", "*.gni"],
                        stderr=subprocess.DEVNULL,
                        text=True,
                    ).splitlines()
                except Exception:
                    hit_files = []
                label_pats = [re.compile(r'"//' + re.escape(f"{pkg_d}:{target_n}") + r'(?:"|\()')]
                if target_n == os.path.basename(pkg_d):
                    label_pats.append(re.compile(r'"//' + re.escape(pkg_d) + r'(?:"|\()'))
                for rel_f in hit_files:
                    rel_f = rel_f.strip()
                    if not rel_f or rel_f == gn_rel or rel_f.startswith("bundles/assembly/bazel_inputs/"):
                        continue
                    try:
                        with open(os.path.join(workdir, rel_f), encoding="utf-8", errors="replace") as fh:
                            content = "\n".join(line.split("#", 1)[0] for line in fh.read().splitlines())
                    except OSError:
                        continue
                    if any(p.search(content) for p in label_pats):
                        return True
                return False

            try:
                with open(os.path.join(workdir, "build/BUILD.gn"), encoding="utf-8", errors="replace") as _bf:
                    _build_gn_text = _bf.read()
            except OSError:
                _build_gn_text = ""

            gn_pkg_TMPLS = ("fuchsia_package", "fuchsia_package_with_single_component", "fuchsia_component")
            cand_dup_gn_pkg_names = set()
            for gname, (gtmpl, gline, gbody) in gn_info["pre_targets"].items():
                if gtmpl not in gn_pkg_TMPLS:
                    continue
                gn_p_m = re.search(r'\b(?:package_name|component_name)\s*=\s*"([^"]+)"', gbody)
                gn_m_m = re.search(r'\bmanifest\s*=\s*"([^"]+)"', gbody)
                if (
                    gname in migrated_pkg_names
                    or (gn_p_m and gn_p_m.group(1) in migrated_pkg_names)
                    or (gn_m_m and gn_m_m.group(1) in migrated_manifests)
                ):
                    cand_dup_gn_pkg_names.add(gname)
            dup_gn_pkg_names = filter_kept_gn_package_targets(
                pkg_dir,
                gn_info["pre_targets"],
                bazel_targets,
                gn_info["has_sentinel"],
                cand_dup_gn_pkg_names,
            )

            remaining_non_pkg = [
                (gn_n, gn_t)
                for gn_n, (gn_t, _, gn_b) in gn_info["pre_targets"].items()
                if gn_n not in dup_gn_pkg_names
                and gn_t != "resource"
                and not (
                    gn_t in ("executable", "rustc_binary")
                    and not re.search(r"\btestonly\s*=\s*true\b", gn_b)
                    and (gn_n in packaged_binaries or any(t["name"] == gn_n for t in bazel_targets))
                )
            ]
            for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
                is_dup_pkg = gname in dup_gn_pkg_names
                is_dup_bin = (
                    not gn_info["has_sentinel"]
                    and gtmpl in ("executable", "rustc_binary", "resource")
                    and not re.search(r"\btestonly\s*=\s*true\b", gbody)
                    and (
                        gname in packaged_binaries
                        or any(t["name"] == gname for t in bazel_targets)
                        or any(
                            dup_n in gn_info["pre_targets"]
                            and re.search(r'":' + re.escape(gname) + r'(?:"|\()', gn_info["pre_targets"][dup_n][2])
                            for dup_n in dup_gn_pkg_names
                        )
                    )
                    and not any(
                        other_n != gname
                        and other_n not in dup_gn_pkg_names
                        and re.search(r'":' + re.escape(gname) + r'(?:"|\()', other_b)
                        for other_n, (other_t, _, other_b) in gn_info["pre_targets"].items()
                    )
                )
                if (is_dup_pkg or is_dup_bin) and not has_uncollected_dylib_fx_pkg:
                    if gtmpl in ("fuchsia_package", "fuchsia_package_with_single_component") and _gn_pkg_has_external_gn_callers(pkg_dir, gname):
                        rem = (
                            f"Replace `{gtmpl}(\"{gname}\")` in '{gn_rel}' with "
                            f"`bazel_fuchsia_package(\"{gname}\")` (`import(\"//build/bazel/bazel_fuchsia_package.gni\")`, "
                            "setting `bazel_config = \"fuchsia_platform\"` and preserving `package_name` and `testonly`). "
                            "Do not edit `group(\"non_hermetic_deps\")` in 'build/BUILD.gn'."
                        )
                    elif not remaining_non_pkg and not gn_info["has_sentinel"]:
                        rem = (
                            f"Delete '{gn_rel}' (`git rm {gn_rel}`) now that all targets in "
                            f"'//{pkg_dir}' are migrated to '{bazel_rel}'."
                        )
                    else:
                        rem = (
                            f"Delete `{gtmpl}(\"{gname}\")` (and any dedicated package component/binary/resource targets "
                            f"not used by remaining GN test targets) from '{gn_rel}', keeping only the GN-only test or "
                            f"library targets in '{gn_rel}'."
                        )
                    _emit(findings, "duplicate_gn_fuchsia_package", "error", gn_rel, gline, (
                            f"GN target `{gtmpl}(\"{gname}\")` remains in '{gn_rel}' after migrating the package "
                            f"to `fx_package(name = \"{fx_pkg_name}\")` in '{bazel_rel}'."
                        ), rem)

        bridge_dir_rel = f"bundles/assembly/bazel_inputs/{pkg_dir}"
        bridge_gn_rel = f"{bridge_dir_rel}/BUILD.gn"
        bridge_bazel_rel = f"{bridge_dir_rel}/BUILD.bazel"
        assembly_bazel_rel = "bundles/assembly/BUILD.bazel"
        assembly_inputs_gn_rel = "bundles/assembly/bazel_inputs/BUILD.gn"
        has_bridge_files = os.path.isfile(os.path.join(workdir, bridge_gn_rel)) or os.path.isfile(
            os.path.join(workdir, bridge_bazel_rel)
        )
        # Match references to the package's own bridge directory (`//<bridge_dir_rel>`,
        # `//<bridge_dir_rel>:<target>`, or `//<bridge_dir_rel>(...)`), excluding
        # unmigrated child subpackage bridges (`//<bridge_dir_rel>/<sub>...`).
        bridge_label_re = re.compile(
            r'"//' + re.escape(bridge_dir_rel) + r'(?:[:(][^"]*)?"'
        )
        in_assembly_bazel = False
        if os.path.isfile(os.path.join(workdir, assembly_bazel_rel)):
            try:
                asm_bz_clean = re.sub(
                    r"#[^\n]*",
                    "",
                    open(
                        os.path.join(workdir, assembly_bazel_rel), encoding="utf-8"
                    ).read(),
                )
                in_assembly_bazel = bridge_label_re.search(asm_bz_clean) is not None
            except Exception:
                pass
        in_assembly_inputs_gn = False
        if os.path.isfile(os.path.join(workdir, assembly_inputs_gn_rel)):
            try:
                asm_in_clean = re.sub(
                    r"#[^\n]*",
                    "",
                    open(
                        os.path.join(workdir, assembly_inputs_gn_rel), encoding="utf-8"
                    ).read(),
                )
                in_assembly_inputs_gn = bridge_label_re.search(asm_in_clean) is not None
            except Exception:
                pass
        # Verify whether an assembly bazel_inputs bridge points to a GN package target
        # that remains unmigrated in BUILD.gn (e.g. a componentless shell-tool package).
        def is_bridged_gn_package_kept(b_gn_rel, p_dir, g_info, dup_names):
            if not g_info or not os.path.isfile(os.path.join(workdir, b_gn_rel)):
                return False
            try:
                b_txt = open(os.path.join(workdir, b_gn_rel), encoding="utf-8").read()
            except Exception:
                return False
            bm = re.search(
                r'\bpackage\s*=\s*"//' + re.escape(p_dir) + r'(?::([^"]+))?"', b_txt
            )
            if not bm:
                return False
            bridged_t = bm.group(1) or os.path.basename(p_dir)
            return bridged_t in g_info["pre_targets"] and bridged_t not in dup_names

        bridge_kept = gn_info and is_bridged_gn_package_kept(
            bridge_gn_rel, pkg_dir, gn_info, dup_gn_pkg_names
        )
        if (
            (has_bridge_files or in_assembly_bazel or in_assembly_inputs_gn)
            and not bridge_kept
            and not has_uncollected_dylib_fx_pkg
        ):
            _emit(findings, "stale_assembly_bazel_inputs_bridge", "error", bridge_gn_rel if has_bridge_files else (assembly_bazel_rel if in_assembly_bazel else assembly_inputs_gn_rel), 1, (
                    f"Package '//{pkg_dir}:{fx_pkg_name}' is defined as `fx_package` in '{bazel_rel}', "
                    f"but the legacy GN-to-Bazel assembly bridge under '//{bridge_dir_rel}' is still present or referenced."
                ), (
                    f"Complete the assembly switchover for '//{pkg_dir}:{fx_pkg_name}': "
                    f"(1) in `{assembly_bazel_rel}`, replace `\"//{bridge_dir_rel}...\"` with `\"//{pkg_dir}:{fx_pkg_name}\"`; "
                    f"(2) in `{assembly_inputs_gn_rel}`, remove the `\"//{bridge_dir_rel}...\"` entry; "
                    f"(3) delete `{bridge_gn_rel}` and `{bridge_bazel_rel}` (`git rm -f {bridge_dir_rel}/BUILD.*`); "
                    f"and (4) ensure `visibility = [\"//bundles/assembly:__subpackages__\"]` is set on `{fx_pkg_name}` in `{bazel_rel}`."
                ))

        parent_dir = os.path.dirname(pkg_dir).strip("/")
        child_name = os.path.basename(pkg_dir)
        parent_gn_rel = f"{parent_dir}/BUILD.gn" if parent_dir else ""
        parent_gn_abs = os.path.join(workdir, parent_gn_rel) if parent_gn_rel else ""
        if parent_dir and parent_dir not in ("src", "sdk", "tools") and os.path.isfile(parent_gn_abs):
            parent_info = parse_gn_file(parent_gn_abs)
            gn_now_targets = set(gn_info["pre_targets"].keys()) if gn_info else set()
            if gn_info and gn_info["post_text"]:
                for pm in re.finditer(r'^\s*[a-zA-Z0-9_]+\(\s*"([^"]+)"\s*\)\s*\{', gn_info["post_text"], re.M):
                    gn_now_targets.add(pm.group(1))
            base_gn_targets = set()
            try:
                base_gn_text = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                for bm in re.finditer(r'^\s*[a-zA-Z0-9_]+\(\s*"([^"]+)"\s*\)\s*\{', base_gn_text, re.M):
                    base_gn_targets.add(bm.group(1))
            except Exception:
                pass
            if parent_info and base_gn_targets:
                ref_re = re.compile(
                    r'"((?://' + re.escape(pkg_dir) + r'|' + re.escape(child_name) + r')(?::([A-Za-z0-9_.-]+))?)"'
                )
                for pname, (ptmpl, pline, pbody) in sorted(parent_info["pre_targets"].items()):
                    for rm in ref_re.finditer(pbody):
                        raw_ref = rm.group(1)
                        sub_t = rm.group(2) or child_name
                        if sub_t in base_gn_targets and sub_t not in gn_now_targets:
                            _emit(findings, "dangling_parent_gn_package_dep", "error", parent_gn_rel, pline, (
                                    f"Target `{ptmpl}(\"{pname}\")` in '{parent_gn_rel}' still references `\"{raw_ref}\"`, "
                                    f"which points to deleted GN target `//{pkg_dir}:{sub_t}` (migrated to `fx_package` in '{bazel_rel}')."
                                ), (
                                    f"Remove `\"{raw_ref}\"` from `{ptmpl}(\"{pname}\")` in '{parent_gn_rel}'."
                                ))

    # Check for directories where zero targets are synced by bazel2gn: reject an empty
    # `## BAZEL2GN SENTINEL` section in BUILD.gn and flag redundant `# @bazel2gn:skip`
    # directives in BUILD.bazel when bazel2gn does not manage the directory.
    def check_unsynced_bazel2gn_directory(p_dir, b_rel, g_rel, g_info, b_targets):
        syncable_rules = (
            CONVERTIBLE_BAZEL_RULES
            | (BAZEL_TEST_RULES - UNSUPPORTED_TEST_BINARY_RULES)
            | {"fuchsia_fidl_library", "fidl_library"}
        )
        has_unskipped_syncable = any(
            t["skip_line"] is None
            and (
                (
                    t["rule"] in syncable_rules
                    and t["name"] not in packaged_binaries
                    and t["name"] not in auto_skipped_manifests
                )
                or (
                    t["rule"] == "fx_package"
                    and t.get("has_test_components")
                    and not t.get("has_resources")
                )
            )
            for t in b_targets
        )
        if has_unskipped_syncable:
            return
        sync_blocking_cats = {
            "skipped_convertible_target",
            "unjustified_test_skip",
            "sdk_library_test_skip",
            "duplicate_target_above_sentinel",
            "duplicate_test_above_sentinel",
            "gn_test_not_migrated",
            "gn_test_package_not_migrated",
            "sentinel_drift",
        }
        if any(
            f.get("file") in (g_rel, b_rel) and f.get("category") in sync_blocking_cats
            for f in findings
        ):
            return
        if g_info and g_info["has_sentinel"] and not g_info["post_targets"]:
            sentinel_ln = g_info.get("sentinel_line", 1)
            msg = (
                f"'{g_rel}' has `## BAZEL2GN SENTINEL`, but none of the targets in "
                f"'{b_rel}' are synced to GN below the sentinel."
            )
            rem = (
                f"Remove `## BAZEL2GN SENTINEL` and "
                f"`import(\"//build/tools/bazel2gn/bazel_migration.gni\")` from '{g_rel}', "
                f"leaving only the hand-written GN/bridge targets in '{g_rel}' (or delete "
                f"'{g_rel}' if no GN targets remain), and remove any `# @bazel2gn:skip` "
                f"annotations from '{b_rel}'."
            )
            _emit(
                findings,
                "unnecessary_bazel2gn_on_package_only_bazel",
                "error",
                g_rel,
                sentinel_ln,
                msg,
                rem,
            )
        elif p_dir in target_dirs and not (g_info and g_info["has_sentinel"]):
            pre_gn_tmpls = {v[0] for v in g_info["pre_targets"].values()} if g_info else set()
            has_unbridged_conv = any(
                t["rule"] in CONVERTIBLE_BAZEL_RULES
                and t["name"] not in packaged_binaries
                and t["name"] not in auto_skipped_manifests
                and not (
                    g_info
                    and t["name"] in g_info["pre_targets"]
                    and g_info["pre_targets"][t["name"]][0]
                    in ("bazel_host_tool", "bazel_ffx_tool", "bazel_fuchsia_package")
                )
                for t in b_targets
            )
            if not has_unbridged_conv and not (pre_gn_tmpls & CONVERTIBLE_GN_TEMPLATES):
                base_skips = set()
                for rev_c in dict.fromkeys((change_base, "HEAD~1", "origin/main")):
                    if not rev_c:
                        continue
                    try:
                        b_txt = subprocess.check_output(
                            ["git", "-C", workdir, "show", f"{rev_c}:{b_rel}"],
                            stderr=subprocess.DEVNULL,
                            text=True,
                        )
                    except Exception:
                        continue
                    if b_txt:
                        base_skips = {
                            bt["name"]
                            for bt in parse_bazel_targets(b_rel, p_dir, text=b_txt)
                            if bt.get("skip_line") is not None
                        }
                        break
                for t in b_targets:
                    if t["skip_line"] is None or t["name"] in base_skips:
                        continue
                    t_name = t["name"]
                    r_name = t["rule"]
                    msg = (
                        f"`# @bazel2gn:skip` on `{r_name}(name = \"{t_name}\")` in "
                        f"'{b_rel}' is redundant because '{g_rel}' has no "
                        "`## BAZEL2GN SENTINEL` (`bazel2gn` does not manage this directory)."
                    )
                    rem = (
                        f"Remove `# @bazel2gn:skip` (and any comment whose only purpose was "
                        f"explaining the skip) above `{r_name}(name = \"{t_name}\")` in "
                        f"'{b_rel}'."
                    )
                    _emit(
                        findings,
                        "redundant_bazel2gn_skip",
                        "error",
                        b_rel,
                        t["skip_line"],
                        msg,
                        rem,
                    )

    check_unsynced_bazel2gn_directory(pkg_dir, bazel_rel, gn_rel, gn_info, bazel_targets)

ALLOWED_DEFS_BZL_LABELS = {
    "//build/bazel/rules/host:defs.bzl",
    "//build/bazel/rules/host/defs.bzl",
}
_defs_bzl_reexport_cache = {}


def _resolve_defs_bzl_label(mod_label):
    """Resolve a //pkg:defs.bzl or @repo//pkg:defs.bzl label to (abs_path, canonical_pkg, repo_name, pkg_path)."""
    if mod_label in ALLOWED_DEFS_BZL_LABELS:
        return None
    if mod_label.endswith(":defs.bzl"):
        prefix = mod_label[: -len(":defs.bzl")]
    elif ":" not in mod_label and mod_label.endswith("/defs.bzl"):
        prefix = mod_label[: -len("/defs.bzl")]
    else:
        return None
    if "//" not in prefix:
        return None
    repo_part, pkg_path = prefix.split("//", 1)
    pkg_path = pkg_path.strip("/")
    repo_name = repo_part.lstrip("@")
    if repo_name:
        defs_abs = os.path.join(
            workdir, "third_party", "bazel_vendor", "fuchsia_in_tree", f"{repo_name}+", pkg_path, "defs.bzl"
        )
        canonical_pkg = f"@{repo_name}//{pkg_path}" if pkg_path else f"@{repo_name}//"
    else:
        defs_abs = os.path.join(workdir, pkg_path, "defs.bzl")
        canonical_pkg = f"//{pkg_path}"
    return defs_abs, canonical_pkg, repo_name, pkg_path


def _same_dir_bzl_target(inner_label, canonical_pkg, repo_name, pkg_path):
    """Return canonical replacement label if inner_label is a .bzl in the same directory."""
    if inner_label.startswith(":"):
        rel = inner_label[1:]
        if "/" not in rel and rel.endswith(".bzl") and rel != "defs.bzl":
            sep = "" if canonical_pkg.endswith("//") else ":"
            return f"{canonical_pkg}{sep}{rel}"
        return None
    if "//" not in inner_label:
        return None
    in_repo, in_rest = inner_label.split("//", 1)
    in_repo_name = in_repo.lstrip("@")
    if in_repo_name not in ("", repo_name):
        return None
    if ":" in in_rest:
        in_pkg, in_file = in_rest.split(":", 1)
    else:
        in_pkg, _, in_file = in_rest.rpartition("/")
    in_pkg = in_pkg.strip("/")
    if in_pkg == pkg_path and "/" not in in_file and in_file.endswith(".bzl") and in_file != "defs.bzl":
        sep = "" if canonical_pkg.endswith("//") else ":"
        return f"{canonical_pkg}{sep}{in_file}"
    return None


def get_defs_bzl_reexports(mod_label):
    """Return (canonical_pkg, {exported_symbol: (target_bzl_label, orig_symbol)}) for a *:defs.bzl label."""
    resolved = _resolve_defs_bzl_label(mod_label)
    if resolved is None:
        return None
    defs_abs, canonical_pkg, repo_name, pkg_path = resolved
    cache_key = (defs_abs, canonical_pkg, repo_name, pkg_path)
    if cache_key in _defs_bzl_reexport_cache:
        return _defs_bzl_reexport_cache[cache_key]
    if not os.path.isfile(defs_abs):
        _defs_bzl_reexport_cache[cache_key] = None
        return None
    try:
        dtext = open(defs_abs, encoding="utf-8").read()
        dtree = ast.parse(dtext, filename=defs_abs)
    except Exception:
        _defs_bzl_reexport_cache[cache_key] = None
        return None

    loaded_same_dir = {}
    reexports = {}
    for stmt in dtree.body:
        if (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "load"
            and stmt.value.args
            and isinstance(stmt.value.args[0], ast.Constant)
            and isinstance(stmt.value.args[0].value, str)
        ):
            target_bzl = _same_dir_bzl_target(
                stmt.value.args[0].value, canonical_pkg, repo_name, pkg_path
            )
            if target_bzl:
                for arg in stmt.value.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        sym = arg.value
                        loaded_same_dir[sym] = (target_bzl, sym)
                        if not sym.startswith("_"):
                            reexports[sym] = (target_bzl, sym)
                for kw in stmt.value.keywords:
                    if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        loaded_same_dir[kw.arg] = (target_bzl, kw.value.value)
        elif isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            lhs = stmt.targets[0].id
            if isinstance(stmt.value, ast.Name) and stmt.value.id in loaded_same_dir:
                reexports[lhs] = loaded_same_dir[stmt.value.id]
            else:
                reexports.pop(lhs, None)
        elif isinstance(stmt, ast.FunctionDef):
            reexports.pop(stmt.name, None)

    result = (canonical_pkg, reexports)
    _defs_bzl_reexport_cache[cache_key] = result
    return result


# 4. rust_unit_test_layout & unrolled_unittest_package: a Rust unit test that isn't
#    host-only (and isn't packaged into an fx_component test fixture)
#    must be packaged for the device (fx_unittest_package, or
#    fx_unittest_component + fx_package + fx_test when packaging extra resources;
#    unrolled fx_packaged_binary + fx_component_manifest + fx_test_component is only
#    allowed when the test requires a custom .cml manifest).
HAS_FX_UNITTEST_PACKAGE = os.path.isfile(
    os.path.join(workdir, "build/bazel/rules/testing/fx_unittest_package.bzl")
)
STANDARD_UNITTEST_CML_SHARDS = {
    "//src/sys/test_runners/rust/default.shard.cml",
    "//src/sys/test_runners/gtest/default.shard.cml",
    "//src/sys/test_runners/gtest/zxtest.shard.cml",
    "//sdk/lib/sys/testing/elf_test_runner.shard.cml",
    "sys/testing/elf_test_runner.shard.cml",
    "syslog/use.shard.cml",
    "//sdk/lib/syslog/use.shard.cml",
    "syslog/client.shard.cml",
    "//sdk/lib/syslog/client.shard.cml",
    "syslog/offer.shard.cml",
    "//sdk/lib/syslog/offer.shard.cml",
}


# Resolve a //.shard.cml include path to its owning expect_includes() target label
# (e.g. "//src/sys/test_runners/tmp_storage.shard.cml" -> "//src/sys/test_runners:tmp_storage")
# when fx_unittest_package / fx_unittest_component can auto-include it via aspect_hints.
def resolve_expect_includes_shard(shard_path):
    if not isinstance(shard_path, str) or not shard_path.startswith("//"):
        return None
    rel_path = shard_path[2:].strip("/")
    parts = rel_path.split("/")
    for i in range(len(parts) - 1, 0, -1):
        s_pkg = "/".join(parts[:i])
        s_file = "/".join(parts[i:])
        gn_file = os.path.join(workdir, s_pkg, "BUILD.gn")
        bz_file = os.path.join(workdir, s_pkg, "BUILD.bazel")
        if os.path.isfile(gn_file):
            g_info = parse_gn_file(gn_file)
            if g_info:
                for g_name, (g_tmpl, _, g_body) in g_info["pre_targets"].items():
                    if g_tmpl == "expect_includes" and f'"{s_file}"' in g_body:
                        return f"//{s_pkg}:{g_name}"
        if os.path.isfile(bz_file):
            try:
                bz_tree = ast.parse(open(bz_file, encoding="utf-8").read(), filename=bz_file)
                for st in bz_tree.body:
                    if not (
                        isinstance(st, ast.Expr)
                        and isinstance(st.value, ast.Call)
                        and isinstance(st.value.func, ast.Name)
                        and st.value.func.id == "expect_includes"
                    ):
                        continue
                    st_kws = {kw.arg: kw.value for kw in st.value.keywords if kw.arg}
                    n_node = st_kws.get("name")
                    inc_node = st_kws.get("includes")
                    if (
                        isinstance(n_node, ast.Constant)
                        and isinstance(n_node.value, str)
                        and isinstance(inc_node, ast.List)
                    ):
                        inc_vals = {
                            e.value.lstrip(":")
                            for e in inc_node.elts
                            if isinstance(e, ast.Constant) and isinstance(e.value, str)
                        }
                        if s_file in inc_vals:
                            return f"//{s_pkg}:{n_node.value}"
            except Exception:
                pass
        if os.path.isfile(gn_file) or os.path.isfile(bz_file):
            break
    return None


def inspect_standard_unittest_cml(cml_abs):
    try:
        raw = open(cml_abs, encoding="utf-8").read()
    except Exception:
        return False, []
    # Strip /* ... */ and // ... comments outside string literals
    cleaned = re.sub(
        r'("(?:\\.|[^"\\])*")|/\*.*?\*/|//[^\n]*',
        lambda m: m.group(1) if m.group(1) is not None else "",
        raw,
        flags=re.S,
    )
    cleaned = re.sub(r"([{,]\s*)([A-Za-z_][A-Za-z0-9_-]*)\s*:", r'\1"\2":', cleaned)
    cleaned = re.sub(r",\s*([\]}])", r"\1", cleaned)
    try:
        data = json.loads(cleaned)
    except Exception:
        return False, []
    if not isinstance(data, dict) or not (set(data.keys()) <= {"include", "program"}):
        return False, []
    prog = data.get("program", {})
    if not isinstance(prog, dict) or not (set(prog.keys()) <= {"binary"}):
        return False, []
    inc = data.get("include", [])
    if not isinstance(inc, list):
        return False, []
    expect_labels = []
    for s in inc:
        if not isinstance(s, str):
            return False, []
        if s in STANDARD_UNITTEST_CML_SHARDS:
            continue
        exp_lbl = resolve_expect_includes_shard(s)
        if not exp_lbl:
            return False, []
        if exp_lbl not in expect_labels:
            expect_labels.append(exp_lbl)
    return True, expect_labels
for pkg_dir in sorted(candidate_dirs):
    bazel_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_abs = os.path.join(workdir, pkg_dir, "BUILD.gn")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")
    if not os.path.isfile(bazel_abs):
        continue
    try:
        btext = open(bazel_abs, encoding="utf-8").read()
        btree = ast.parse(btext, filename=bazel_abs)
    except Exception:
        continue
    # Skip pre-existing re-export load() statements in ancillary or collateral-edited
    # upstream packages that already existed at change_base or HEAD~1. (Out-of-target
    # packages skip the defs.bzl re-export check entirely via is_ancillary_pkg below.)
    is_out_of_target_pkg = bool(target_dirs and pkg_dir not in target_dirs)
    is_ancillary_candidate = is_out_of_target_pkg
    base_btext_flat = ""
    base_refs = ["HEAD~1", change_base] if change_base == "HEAD" else [change_base, "HEAD~1"]
    for _ref in dict.fromkeys(base_refs):
        _lines = git_lines(["show", f"{_ref}:{bazel_rel}"])
        if _lines:
            base_btext_flat = re.sub(r"\s+", "", "\n".join(_lines))
            if not is_ancillary_candidate and not target_dirs:
                _diff = git_lines(["diff", "--unified=0", _ref, "--", bazel_rel])
                _chg = [
                    ln[1:].strip()
                    for ln in _diff
                    if ln[:1] in ("+", "-")
                    and not ln.startswith(("+++", "---"))
                    and ln[1:].strip()
                    and not ln[1:].strip().startswith("#")
                ]
                _col_pat = re.compile(
                    r'^(?:load\(\s*"(?:@platforms//host:constraints\.bzl|'
                    r"//build/bazel/platforms:constraints\.bzl|"
                    r'//build/bazel/rules/[\w/]+:(?:defs|\w+)\.bzl)"|'
                    r"target_compatible_with\s*=\s*HOST_(?:OS_)?CONSTRAINTS\b|"
                    r"visibility\s*=\s*|"
                    r'"(?:(?://|:)[^"]+|\w+)"\s*,?$|'
                    r"\],?$|\),?$)"
                )
                if _chg and all(_col_pat.search(ln) for ln in _chg):
                    is_ancillary_candidate = True
            break
    if FIDL_RUST_NEXT_UNIMPLEMENTED:
        next_m = FIDL_RUST_NEXT_DEP_RE.search(btext)
        if next_m:
            next_line = btext.count("\n", 0, next_m.start()) + 1
            _emit(findings, "unimplemented_fidl_rust_next_dep", "error", bazel_rel, next_line, (
                    f"'{bazel_rel}' depends on `:{next_m.group(1)}`, but `fidl_library()` in "
                    "`//build/bazel/rules/fidl/fidl_library.bzl` leaves `enable_rust_next` as an unimplemented "
                    "stub and does not generate `*_rust_next` or `*_rust_fdomain_next` Bazel targets yet."
                ), (
                    f"Do not leave an unbuildable target or partially migrate '{pkg_dir}': when any target in "
                    f"'{gn_rel}' is blocked on `*_rust_next` / `*_rust_fdomain_next` bindings, delete '{bazel_rel}' "
                    f"and restore '{gn_rel}' (`rm -f {bazel_rel} && git checkout -- {gn_rel}`), run `run_checks.sh` "
                    "on the clean tree so it exits 0, and report the blocker in `summary` with `modified_files: []` "
                    "and `disputed_findings: []`."
                ))
    # Extract pre-existing top-level load() symbol bindings so pre-existing
    # load() lines at change_base are exempt from defs_bzl_reexport_load, and
    # flag unnecessary load() rewrites in out-of-scope ancillary packages.
    def _extract_top_level_loads(tree_node):
        lmap = {}
        if not tree_node:
            return lmap
        for stmt in tree_node.body:
            if (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
                and stmt.value.func.id == "load"
                and stmt.value.args
                and isinstance(stmt.value.args[0], ast.Constant)
                and isinstance(stmt.value.args[0].value, str)
            ):
                m_lbl = stmt.value.args[0].value
                for a in stmt.value.args[1:]:
                    if isinstance(a, ast.Constant) and isinstance(a.value, str):
                        lmap[a.value] = m_lbl
                for kw in stmt.value.keywords:
                    if (
                        kw.arg
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        lmap[kw.value.value] = m_lbl
        return lmap

    base_btext_for_loads = "\n".join(
        git_lines(["show", f"{change_base or 'HEAD'}:{bazel_rel}"])
    )
    try:
        base_btree_for_loads = (
            ast.parse(base_btext_for_loads, filename=bazel_rel)
            if base_btext_for_loads
            else None
        )
    except Exception:
        base_btree_for_loads = None
    base_load_map = _extract_top_level_loads(base_btree_for_loads)
    # Detect surgical upstream repairs where no new top-level targets were added
    def _extract_named_rules(tree_node):
        rmap = {}
        if not tree_node:
            return rmap
        for stmt in tree_node.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                continue
            fn = stmt.value.func.id if isinstance(stmt.value.func, ast.Name) else ""
            if fn in ("load", "package", "exports_files", "licenses", "visibility"):
                continue
            for kw in stmt.value.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    rmap[kw.value.value] = fn
        return rmap

    base_rules = _extract_named_rules(base_btree_for_loads)
    cur_rules = _extract_named_rules(btree)
    is_surgical_ancillary = bool(base_rules and base_rules == cur_rules)
    is_ancillary_pkg = bool(is_out_of_target_pkg or is_surgical_ancillary)

    rules_used = set()
    rust_tests = []
    added_lines_in_bazel = get_file_added_lines(bazel_rel)
    for node in btree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            if added_lines_in_bazel is not None:
                stmt_lines = range(node.lineno, getattr(node, "end_lineno", node.lineno) + 1)
                if not any(ln in added_lines_in_bazel for ln in stmt_lines):
                    continue
            mod_label = node.value.args[0].value
            # Collateral-edited packages (no target_dirs, only visibility/constraint/load
            # edits): skip load() statements that existed verbatim at the base revision.
            # Ancillary packages (is_ancillary_pkg) are handled below instead, so their
            # rewritten pre-existing loads are still flagged.
            load_seg = ast.get_source_segment(btext, node) or ""
            if (
                is_ancillary_candidate
                and not is_ancillary_pkg
                and base_btext_flat
                and load_seg
                and re.sub(r"\s+", "", load_seg) in base_btext_flat
            ):
                continue
            loaded_items = [
                (None, a.value)
                for a in node.value.args[1:]
                if isinstance(a, ast.Constant) and isinstance(a.value, str)
            ] + [
                (kw.arg, kw.value.value)
                for kw in node.value.keywords
                if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)
            ]
            # Skip defs.bzl re-export load checks in ancillary out-of-target packages
            # so pre-existing loads are preserved without triggering load-rewrite loops.
            reexport_info = get_defs_bzl_reexports(mod_label)
            if reexport_info is not None and not is_ancillary_pkg:
                canonical_pkg, reexports = reexport_info
                bad_syms = []
                repl_loads = []
                for alias, sym in loaded_items:
                    if base_load_map.get(sym) == mod_label:
                        continue
                    if sym in reexports:
                        target_bzl, orig_sym = reexports[sym]
                        bad_syms.append(sym)
                        if alias and alias != orig_sym:
                            repl_loads.append(f'`load("{target_bzl}", {alias} = "{orig_sym}")`')
                        else:
                            repl_loads.append(f'`load("{target_bzl}", "{orig_sym}")`')
                if bad_syms:
                    repls = ", ".join(repl_loads)
                    _emit(findings, "defs_bzl_reexport_load", "error", bazel_rel, node.lineno, (
                            f"'{bazel_rel}' loads {', '.join(f'`{s}`' for s in bad_syms)} from "
                            f"`{mod_label}`, which only re-exports symbols from individual `.bzl` "
                            "files in the same directory."
                        ), (
                            f"Replace `load(\"{mod_label}\", ...)` in '{bazel_rel}' with direct "
                            f"loads from the defining files in `{canonical_pkg}`: {repls}."
                        ))
            elif is_ancillary_pkg and mod_label.startswith(
                ("@rules_cc//cc:", "@rules_rust//rust:")
            ):
                for _alias, sym in loaded_items:
                    orig_mod = base_load_map.get(sym)
                    if orig_mod and orig_mod != mod_label:
                        _emit(
                            findings,
                            "ancillary_preexisting_load_rewritten",
                            "error",
                            bazel_rel,
                            node.lineno,
                            (
                                f"Ancillary package '{bazel_rel}' (outside the assigned target "
                                f"directories) rewrites pre-existing `load(\"{orig_mod}\", \"{sym}\")` "
                                f"to `load(\"{mod_label}\", \"{sym}\")`, introducing an unwrapped "
                                "external rule load hunk on pre-existing targets."
                            ),
                            (
                                f"Restore the original pre-existing `load(\"{orig_mod}\", \"{sym}\")` "
                                f"line verbatim at the top of '{bazel_rel}' (removing any added blank "
                                "line above it) so pre-existing `load()` and rule statements stay "
                                "unchanged relative to the base revision while keeping only the "
                                "minimal surgical `hdrs` or `visibility` update."
                            ),
                        )
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)):
            continue
        call = node.value
        rule = call.func.id
        rules_used.add(rule)
        kws = {kw.arg: kw for kw in call.keywords if kw.arg}
        name_kw = kws.get("name")
        tname = name_kw.value.value if name_kw and isinstance(name_kw.value, ast.Constant) else "?"
        if rule == "rustc_test":
            seg = ast.get_source_segment(btext, node) or ""
            rust_tests.append(
                (
                    tname,
                    node.lineno,
                    "HOST_OS_CONSTRAINTS" in seg or "HOST_CONSTRAINTS" in seg,
                )
            )

    # Check whether a cross-package BUILD.bazel packages //clean_dir:bin_name.
    def _bazel_file_packages_test_bin(pkg_rel, clean_dir, bin_name):
        pkg_clean = pkg_rel.strip("/")
        if not pkg_clean or pkg_clean == clean_dir:
            return False
        tmap = _get_pkg_bazel_targets_map(pkg_clean)
        if not tmap:
            return False
        full_lbls = {f"//{clean_dir}:{bin_name}"}
        if os.path.basename(clean_dir) == bin_name:
            full_lbls.add(f"//{clean_dir}")
        consumer_rules = {
            "fx_unittest_package",
            "fx_packaged_binary",
            "fx_test_component",
            "fx_unittest_component",
            "fx_component",
            "fx_test",
        }
        return any(
            t_rule in consumer_rules and (set(t_refs) & full_lbls)
            for t_rule, t_refs in tmap.values()
        )

    # Check whether an ancestor or cross-package BUILD.bazel packages //cur_pkg_dir:bin_name
    # (a hand-written GN test package does not count: complete-directory policy).
    def _is_packaged_by_other_bazel_pkg(cur_pkg_dir, bin_name):
        clean_dir = cur_pkg_dir.strip("/")
        anc = os.path.dirname(clean_dir)
        while anc:
            if _bazel_file_packages_test_bin(anc, clean_dir, bin_name):
                return True
            next_anc = os.path.dirname(anc)
            if next_anc == anc:
                break
            anc = next_anc
        for cross_dir in _collect_cross_pkg_dirs(clean_dir, include_children=True):
            if _bazel_file_packages_test_bin(cross_dir, clean_dir, bin_name):
                return True
        return False

    # Test target hygiene: package-private plumbing, no "manual" tag (unless a
    # Fuchsia package wrapper wraps the target, it is a binary, or the package is
    # Bazel-only), crate deps
    # reused, <crate>_host_test naming, and no unrolled unit-test manifests when
    # fx_unittest_package / fx_unittest_component suffices.
    calls = _collect_named_top_level_calls(btree)
    top_var_nodes = {
        node.targets[0].id: node.value
        for node in btree.body
        if isinstance(node, ast.Assign)
        and len(node.targets) == 1
        and isinstance(node.targets[0], ast.Name)
    }

    def str_list(v):
        if isinstance(v, ast.BinOp):
            return str_list(v.left) + str_list(v.right)
        if isinstance(v, ast.List):
            return [
                e.value
                for e in v.elts
                if isinstance(e, ast.Constant) and isinstance(e.value, str)
            ]
        return []

    def str_val(v):
        return v.value if isinstance(v, ast.Constant) and isinstance(v.value, str) else None

    def all_str_vals(v, _seen=()):
        if v is None:
            return []
        out = []
        for n in ast.walk(v):
            if isinstance(n, ast.Constant) and isinstance(n.value, str):
                out.append(n.value)
            elif (
                isinstance(n, ast.Name)
                and n.id in top_var_nodes
                and n.id not in _seen
            ):
                out.extend(all_str_vals(top_var_nodes[n.id], _seen + (n.id,)))
        return out

    # Check whether a device rustc_test binary is packaged into a standalone component
    # (e.g. a test-data helper component synced to fuchsia_component via bazel2gn and packaged
    # by another directory's test package).
    def _load_pkg_calls_for_dir(cur_dir):
        if cur_dir == pkg_dir:
            return calls
        bz_p = os.path.join(workdir, cur_dir.strip("/"), "BUILD.bazel")
        if not os.path.isfile(bz_p):
            return {}
        try:
            sub_tree = ast.parse(open(bz_p, encoding="utf-8").read(), filename=bz_p)
        except Exception:
            return {}
        return _collect_named_top_level_calls(sub_tree)

    def is_component_packaged_rust_test(test_bin_name, cur_pkg_dir=pkg_dir):
        clean_pkg = cur_pkg_dir.strip("/")
        bin_labels = {test_bin_name, f":{test_bin_name}", f"//{clean_pkg}:{test_bin_name}"}
        dir_calls = _load_pkg_calls_for_dir(cur_pkg_dir)
        pkg_bin_labels = set(bin_labels)
        for cname, (crule, ckws, _) in dir_calls.items():
            if crule == "fx_packaged_binary" and (
                (str_val(ckws.get("binary")) or "") in bin_labels
                or (set(all_str_vals(ckws.get("binary"))) & bin_labels)
            ):
                pkg_bin_labels.update({cname, f":{cname}", f"//{clean_pkg}:{cname}"})
        for cname, (crule, ckws, _) in dir_calls.items():
            if crule in ("fx_component", "fx_test_component") and (
                (set(str_list(ckws.get("deps"))) | set(all_str_vals(ckws.get("deps"))))
                & pkg_bin_labels
            ):
                return True
        return False

    # Check that non-host-only rustc_test targets are packaged for the device in BUILD.bazel
    # (fx_unittest_package / fx_test, a standalone helper fx_component, or another Bazel package).
    def check_rust_unit_test_layout(cur_pkg_dir, brel, r_tests, r_used):
        if {"fx_test", "fx_unittest_package"} & r_used:
            return
        grel = os.path.join(cur_pkg_dir, "BUILD.gn")
        # With fx_test available, cq_reachability unpackaged_rust_device_test reports the
        # unpackaged device Rust test binaries of the target dirs, while gn_kept records
        # test binaries packaged by kept pre-sentinel GN test packages (where
        # premature_gn_test_package_component forbids unreferenced fx_component wrappers).
        defer_to_cq = cur_pkg_dir in target_dirs and os.path.isfile(
            os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl")
        )
        if defer_to_cq and _pcl_ext is not None:
            # cq_reachability skips dual-built external-repo-blocked packages; the Bazel test
            # still needs Bazel packaging there (it stays dual-built and unexported).
            try:
                cur_gn_txt = open(os.path.join(workdir, grel), encoding="utf-8").read()
                cur_bz_txt = open(os.path.join(workdir, brel), encoding="utf-8").read()
            except OSError:
                cur_gn_txt = cur_bz_txt = ""
            if "BAZEL2GN SENTINEL" in cur_gn_txt and _pcl_ext.external_repo_test_blockers(
                workdir, cur_pkg_dir, cur_bz_txt
            ):
                defer_to_cq = False
        gn_kept = gn_kept_pkg_test_deps_by_dir.get(cur_pkg_dir, set())
        device_tests = [
            t
            for t in r_tests
            if not t[2]
            and not defer_to_cq
            and t[0] not in gn_kept
            and base_rules.get(t[0]) != "rustc_test"
            and not is_component_packaged_rust_test(t[0], cur_pkg_dir)
            and not _is_packaged_by_other_bazel_pkg(cur_pkg_dir, t[0])
        ]
        if device_tests:
            tname, tline, _ = device_tests[0]
            msg = (
                f"`rustc_test(name = \"{tname}\")` in '{brel}' is not "
                "host-only but the package declares no `fx_unittest_package`, "
                "`fx_test`, or standalone `fx_component` packaging it, so the "
                "Fuchsia (target-side) test is missing."
            )
            rem = (
                f"Add `fx_unittest_package(name = \"<test-pkg>\", unit_tests = "
                f"[\":{tname}\"])` (use `fx_unittest_component` + `fx_package` + "
                "`fx_test` if packaging extra resources, unroll into "
                "`fx_packaged_binary` + `fx_component_manifest` + "
                "`fx_test_component` if a custom `.cml` test root is needed, or "
                "use `fx_packaged_binary` + `fx_component_manifest` + "
                "`fx_component(testonly = True)` for a standalone test-fixture "
                "`fuchsia_component`), or restrict the test with "
                "`target_compatible_with = HOST_OS_CONSTRAINTS` if GN only built "
                "it for the host. Do not keep the test package in GN due to an "
                f"out-of-scope external dependency (revert `{brel}` and `{grel}` to leave "
                f"'{cur_pkg_dir}' unmigrated with `modified_files: []`), or report "
                "unavoidable local exceptions via `disputed_findings` in `CoderReport`."
            )
            _emit(findings, "rust_unit_test_layout", "error", brel, tline, msg, rem)

    check_rust_unit_test_layout(pkg_dir, bazel_rel, rust_tests, rules_used)

    if HAS_FX_UNITTEST_PACKAGE:
        for tname, (rule, kws, line) in calls.items():
            if rule != "fx_test_component":
                continue
            cm_ref = (
                str_val(kws.get("compiled_manifest") or kws.get("component_manifest") or kws.get("manifest")) or ""
            ).lstrip(":")
            if not cm_ref or cm_ref not in calls or calls[cm_ref][0] != "fx_component_manifest":
                continue
            cm_kws = calls[cm_ref][1]
            manifest_rel = str_val(cm_kws.get("manifest"))
            if not manifest_rel:
                continue
            cml_rel = os.path.normpath(os.path.join(pkg_dir, manifest_rel))
            cml_abs = os.path.join(workdir, cml_rel)
            if not os.path.isfile(cml_abs):
                continue
            # Pre-existing .cml manifests in git at change_base are intentional custom manifests.
            if (
                subprocess.call(
                    ["git", "-C", workdir, "cat-file", "-e", f"{change_base}:{cml_rel}"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                == 0
            ):
                continue
            is_std_cml, expect_labels = inspect_standard_unittest_cml(cml_abs)
            if not is_std_cml:
                continue
            comp_deps = [d.lstrip(":") for d in str_list(kws.get("deps"))]
            pkg_bins = [d for d in comp_deps if d in calls and calls[d][0] == "fx_packaged_binary"]
            extra_comp_deps = [d for d in comp_deps if d not in pkg_bins]
            raw_bin = (
                (str_val(calls[pkg_bins[0]][1].get("binary")) or f":{pkg_bins[0]}")
                if pkg_bins
                else ":<test_bin>"
            )
            enclosing_pkg = None
            enclosing_pkg_kws = {}
            for pname, (prule, pkws, _) in calls.items():
                if prule == "fx_package" and (f":{tname}" in str_list(pkws.get("test_components")) or tname in str_list(pkws.get("test_components"))):
                    enclosing_pkg = pname
                    enclosing_pkg_kws = pkws
                    break
            enclosing_test = None
            if enclosing_pkg:
                for xname, (xrule, xkws, _) in calls.items():
                    if xrule == "fx_test" and (str_val(xkws.get("package")) or "").lstrip(":") == enclosing_pkg:
                        enclosing_test = xname
                        break
            has_extra_pkg_items = bool(
                extra_comp_deps
                or str_list(enclosing_pkg_kws.get("resources"))
                or str_list(enclosing_pkg_kws.get("components"))
                or str_list(enclosing_pkg_kws.get("subpackages"))
                or str_list(enclosing_pkg_kws.get("tools"))
            )
            target_test_name = enclosing_test or enclosing_pkg or tname
            hint_clause = ""
            if expect_labels:
                fmt_lbls = ", ".join(f'"{l}"' for l in expect_labels)
                hint_clause = (
                    f" Pass `aspect_hints = [{fmt_lbls}]` on `{raw_bin}` (defining "
                    "`expect_includes(...)` in the shard package's `BUILD.bazel` if not "
                    "yet present, instead of `exports_files`) so `fx_unittest_package` "
                    "auto-includes the shard(s) and `bazel2gn` emits the GN `deps`."
                )
            if has_extra_pkg_items:
                rem = (
                    f"Delete the newly added standard unit-test manifest '{cml_rel}' (`git rm -f {cml_rel}`) and "
                    f"replace `fx_packaged_binary`, `fx_component_manifest(\"{cm_ref}\")`, and "
                    f"`fx_test_component(\"{tname}\")` in '{bazel_rel}' with "
                    f"`fx_unittest_component(name = \"{tname}\", binary = \"{raw_bin}\")` "
                    "(load from `//build/bazel/rules/testing:fx_unittest_component.bzl`, moving any `resource()` deps into "
                    "`fx_package(resources = [...])`), or attach `resource()` targets via `data = [...]` on "
                    f"`{raw_bin}` and replace the entire package/test chain with "
                    f"`fx_unittest_package(name = \"{target_test_name}\", unit_tests = [\"{raw_bin}\"])`."
                    + hint_clause
                )
            else:
                rem = (
                    f"Delete the newly added standard unit-test manifest '{cml_rel}' (`git rm -f {cml_rel}`) and "
                    f"replace the unrolled `fx_packaged_binary`, `fx_component_manifest(\"{cm_ref}\")`, "
                    f"`fx_test_component(\"{tname}\")`, `fx_package`, and `fx_test` in '{bazel_rel}' with a single "
                    f"`fx_unittest_package(name = \"{target_test_name}\", unit_tests = [\"{raw_bin}\"])` "
                    "(load from `//build/bazel/rules/testing:fx_unittest_package.bzl`). "
                    "Also revert any `exports_files` edits in `src/sys/test_runners/` or "
                    "`sdk/lib/` that were only added for the deleted `.cml`."
                    + hint_clause
                )
            _emit(findings, "unrolled_unittest_package", "error", bazel_rel, line, (
                    f"`fx_test_component(name = \"{tname}\")` in '{bazel_rel}' unrolls `fx_packaged_binary` + "
                    f"`fx_component_manifest` with a newly added manifest '{cml_rel}' that only uses standard "
                    "unit-test runner/syslog shards and `program.binary`. Unit tests must default to "
                    "`fx_unittest_package` (or `fx_unittest_component`) and only use the unrolled rules when a "
                    "custom `.cml` manifest is required."
                ), rem)

    def is_true(v):
        return isinstance(v, ast.Constant) and v.value is True

    def hygiene(line, message, remediation):
        _emit(findings, "rust_test_target_hygiene", "error", bazel_rel, line, message, remediation)

    base_manual_targets = set()
    try:
        base_bz_txt = subprocess.check_output(
            ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
        for bnode in ast.parse(base_bz_txt, filename=bazel_rel).body:
            if (
                isinstance(bnode, ast.Expr)
                and isinstance(bnode.value, ast.Call)
                and isinstance(bnode.value.func, ast.Name)
            ):
                bkws = {kw.arg: kw.value for kw in bnode.value.keywords if kw.arg}
                bn = bkws.get("name")
                if (
                    isinstance(bn, ast.Constant)
                    and isinstance(bn.value, str)
                    and "manual" in all_str_vals(bkws.get("tags"))
                ):
                    base_manual_targets.add(bn.value)
    except Exception:
        pass

    # rustc_test (or other binary) targets that a package wrapper in this file builds into a
    # Fuchsia test or package (fx_unittest_package unit_tests, fx_unittest_component or
    # fx_packaged_binary binary).
    # These carry `tags = ["manual"]` on purpose so `tests = []` / `:all` runs the packaged
    # test and not the bare host-incompatible binary.
    def local_target_name(lbl):
        lbl = (lbl or "").strip()
        if lbl.startswith(":"):
            return lbl[1:]
        if lbl.startswith(f"//{pkg_dir}:"):
            return lbl.split(":", 1)[1]
        return None

    packaged_rust_tests = set()
    for _cname, (crule, ckws, _cline) in calls.items():
        if crule == "fx_unittest_package":
            for lbl in str_list(ckws.get("unit_tests")):
                if local_target_name(lbl):
                    packaged_rust_tests.add(local_target_name(lbl))
        elif crule in ("fx_unittest_component", "fx_packaged_binary"):
            if local_target_name(str_val(ckws.get("binary"))):
                packaged_rust_tests.add(local_target_name(str_val(ckws.get("binary"))))

    # Check that rustc_test targets do not duplicate crate deps.
    crate_of_test = {}
    for tname, (rule, kws, line) in calls.items():
        if rule != "rustc_test":
            continue
        crate = kws.get("crate")
        crate_name = (
            crate.value.lstrip(":")
            if isinstance(crate, ast.Constant) and isinstance(crate.value, str)
            else None
        )
        crate_of_test[tname] = crate_name

        if crate_name and crate_name in calls:
            lib_deps = set(str_list(calls[crate_name][1].get("deps")))
            dup = sorted(lib_deps & set(str_list(kws.get("deps"))))
            if dup:
                hygiene(line,
                        f"`rustc_test(name = \"{tname}\")` repeats deps of its crate `:{crate_name}`: {', '.join(dup)}.",
                        "List only the GN test-only deps (test_deps) in the rustc_test. If it does not inherit "
                        "the crate deps, hoist them into a module-level list (e.g. `_DEPS = [...]`) used as "
                        "`deps = _DEPS` in the library and `deps = _DEPS + [<test-only deps>]` in the test.")

    def vis_of(kws):
        return str_list(kws.get("visibility")) if "visibility" in kws else None

    # Check whether a test plumbing target has external callers outside pkg_dir
    # (for example, an fx_test_component or rustc_test packaged by another
    # directory's integration test package), so declaring non-public visibility
    # for those external callers is allowed rather than flagged as internal plumbing.
    def has_external_test_plumbing_rdeps(tname):
        default_t = os.path.basename(pkg_dir)
        needle = f":{tname}" if tname != default_t else f"//{pkg_dir}"
        try:
            out = subprocess.check_output(
                [
                    "git",
                    "-C",
                    workdir,
                    "grep",
                    "--untracked",
                    "-l",
                    "--fixed-strings",
                    "-e",
                    needle,
                    "--",
                    "*BUILD.gn",
                    "*BUILD.bazel",
                    "*.gni",
                    "*.bzl",
                ],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            cand_files = [
                os.path.join(workdir, l.strip()) for l in out.splitlines() if l.strip()
            ]
        except Exception:
            cand_files = []
        pkg_parts = pkg_dir.split("/") if pkg_dir else []
        for i in range(1, len(pkg_parts)):
            anc_gn = os.path.join(workdir, "/".join(pkg_parts[:i]), "BUILD.gn")
            if os.path.isfile(anc_gn) and anc_gn not in cand_files:
                cand_files.append(anc_gn)
        abs_re = re.compile(
            r'"//' + re.escape(pkg_dir) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
        )
        tname_norm = tname.replace("-", "_")
        for fp in cand_files:
            rel = os.path.relpath(fp, workdir)
            caller_pkg = os.path.dirname(rel).strip("/")
            if caller_pkg == pkg_dir or os.path.basename(rel) == "visibility.gni":
                continue
            try:
                raw = open(fp, encoding="utf-8", errors="ignore").read()
            except Exception:
                continue
            stripped = re.sub(
                r"\b(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]",
                "",
                re.sub(r"#[^\n]*", "", raw),
                flags=re.DOTALL,
            )
            for m in abs_re.finditer(stripped):
                sub = m.group(1) or default_t
                if sub == tname or sub.replace("-", "_") == tname_norm:
                    return True
            if rel.endswith((".gn", ".gni")) and caller_pkg and pkg_dir:
                try:
                    cand_rel = os.path.relpath(pkg_dir, caller_pkg)
                    if (
                        cand_rel
                        and cand_rel != "."
                        and not cand_rel.startswith("../../..")
                    ):
                        rel_re = re.compile(
                            r'"(?:\./)?'
                            + re.escape(cand_rel)
                            + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
                        )
                        for m in rel_re.finditer(stripped):
                            sub = m.group(1) or default_t
                            if sub == tname or sub.replace("-", "_") == tname_norm:
                                return True
                except ValueError:
                    pass
        return False

    gn_info = parse_gn_file(os.path.join(workdir, pkg_dir, "BUILD.gn"))
    btext_lines = btext.splitlines()

    # 24496: binaries and every target of a Bazel-only package (no `bazel2gn` sentinel in
    # `BUILD.gn`) may set `tags = ["manual"]` so wildcard builds only build them under the
    # Fuchsia package transition. Targets wrapped by `fx_unittest_package` /
    # `fx_unittest_component` / `fx_packaged_binary` are covered by `packaged_rust_tests`.
    manual_tag_binary_rules = ("rustc_binary", "rust_binary", "fx_cc_binary", "cc_binary")

    def _is_exempt_manual_tag_target(tname, rule):
        if tname in base_manual_targets or tname in packaged_rust_tests:
            return True
        if not (gn_info and gn_info["has_sentinel"]):
            return True
        return rule in manual_tag_binary_rules

    for tname, (rule, kws, line) in calls.items():
        if "manual" in all_str_vals(kws.get("tags")) and not _is_exempt_manual_tag_target(
            tname, rule
        ):
            tags_line = getattr(kws.get("tags"), "lineno", line)
            _emit(
                findings,
                "unnecessary_manual_tag",
                "error",
                bazel_rel,
                tags_line,
                f'`{rule}(name = "{tname}")` in \'{bazel_rel}\' sets `tags = [..., "manual", ...]`.',
                (
                    f'Remove `"manual"` from `tags` (or omit `tags` if empty) on '
                    f'`{rule}(name = "{tname}")` in \'{bazel_rel}\' and re-run '
                    f'`fx bazel2gn -d {pkg_dir}` if dual-building.'
                ),
            )
        plumbing = (
            rule in ("rustc_test", "fx_test_component", "fx_unittest_component")
            or (
                rule in ("fx_packaged_binary", "fx_component_manifest")
                and is_true(kws.get("testonly"))
            )
            or (rule == "fx_package" and "test_components" in kws)
        )
        vis_vals = vis_of(kws)
        if plumbing and vis_vals not in (None, [":__pkg__"], ["//visibility:private"]):
            has_ext_rdeps = has_external_test_plumbing_rdeps(tname)
            if "//visibility:public" in vis_vals or not has_ext_rdeps:
                hygiene(
                    line,
                    f'Test target `{rule}(name = "{tname}")` is not restricted to its package.',
                    (
                        # Callers outside the package (e.g. an ancestor test_suite) need a
                        # narrow visibility: omitting it would trip visibility_audit
                        # missing_target_visibility and cq_reachability's suite export.
                        "Replace `//visibility:public` with the narrow visibility of its "
                        "external callers (e.g. `[\"//<ancestor>:__pkg__\"]` for an ancestor "
                        "`test_suite`), as visibility_audit recommends."
                        if has_ext_rdeps
                        else "Omit `visibility` (defaulting to package-private) on internal "
                        "test plumbing targets."
                    ),
                )
        if rule.startswith("wrap_host_"):
            bn = kws.get("binary_name")
            test = bn.value if isinstance(bn, ast.Constant) and isinstance(bn.value, str) else None
            crate_name = crate_of_test.get(test)
            # `<base>-host-test` is the name bazel_minimality wrap_host_rust_test_naming
            # requires in rustc_library_with_features packages (base without -instrumented).
            if crate_name and tname not in (
                f"{crate_name}_host_test",
                f"{crate_name}-host_test",
                f"{re.sub(r'-instrumented$', '', crate_name)}-host-test",
            ):
                hygiene(line, f"Host test wrapper `{tname}` does not follow the `<crate>_host_test` naming.",
                        f"Rename it to `{crate_name}_host_test` and update the BUILD.gn host_tests reference.")

    GENERIC_RUST_CRATE_NAMES = {
        "a", "bin", "bin_with_fake_time", "client", "common", "core", "driver",
        "fidl", "framework", "lib", "main", "parser", "rust", "server", "test",
        "test_lib", "test_util", "testing", "tests", "util", "utils",
        "validator",
    }

    def is_generic_rust_test_output_name(name):
        base = name.replace("-", "_")
        for suffix in ("_lib_test", "_bin_test", "_dylib_test", "_host_test", "_test"):
            if base.endswith(suffix):
                base = base[: -len(suffix)]
                break
        return base in GENERIC_RUST_CRATE_NAMES

    for tname, (rule, kws, line) in calls.items():
        if rule not in ("rustc_test", "host_rustc_test") or not (gn_info and gn_info["has_sentinel"]):
            continue
        idx = line - 2
        while idx >= 0 and btext_lines[idx].strip().startswith("#"):
            idx -= 1
        if any("@bazel2gn:skip" in l for l in btext_lines[idx + 1 : line - 1]):
            continue
        raw_output_name = None
        for l in btext_lines[idx + 1 : line - 1]:
            m_out = re.search(r'@bazel2gn:raw_attribute:output_name\s*=\s*"([^"]+)"', l)
            if m_out:
                raw_output_name = m_out.group(1)
        out_name = raw_output_name or (str_val(kws.get("binary_name")) if rule == "rustc_test" else None) or str_val(kws.get("crate_name")) or tname
        effective_out = out_name.replace("-", "_")
        if is_generic_rust_test_output_name(effective_out):
            _emit(findings, "generic_test_output_name", "error", bazel_rel, line, (
                    f"`{rule}(name = \"{tname}\")` in '{bazel_rel}' resolves to generic test binary name "
                    f"`{effective_out}` in `${{root_out_dir}}`, which collides across packages."
                ), (
                    f"Give `{rule}(name = \"{tname}\")` a package-specific `name` (such as "
                    f"`\"<crate_name>_lib_test\"` or `\"<pkg>_test\"`) in '{bazel_rel}', update "
                    f"any referencing `wrap_host_rust_test(binary_name = ...)` / "
                    f"`fx_packaged_binary(binary = ...)` / `{pkg_dir}/BUILD.gn` test references, "
                    f"and re-run `fx bazel2gn -d {pkg_dir}` (do not use "
                    f"`# @bazel2gn:raw_attribute:output_name` or `crate_name` on `{rule}`)."
                ))


# Verify that dual-build packages have no redundant `# @bazel2gn:raw_attribute:source_root`
# annotations on `rustc_test(crate = ":...")` targets and pass `bazel2gn --check_only`.
def find_up_to_date_bazel2gn_bin():
    try:
        build_dir = open(os.path.join(workdir, ".fx-build-dir"), encoding="utf-8").read().strip()
    except OSError:
        return None, None, None
    if not build_dir:
        return None, None, None
    b2gn_src_dir = os.path.join(workdir, "build", "tools", "bazel2gn")
    max_src_mtime = 0.0
    if os.path.isdir(b2gn_src_dir):
        for root, _, files in os.walk(b2gn_src_dir):
            for fn in files:
                if fn.endswith((".go", ".json", ".gni", ".bzl", ".bazel")):
                    try:
                        max_src_mtime = max(
                            max_src_mtime, os.path.getmtime(os.path.join(root, fn))
                        )
                    except OSError:
                        pass
    candidates = [
        os.path.join(
            workdir,
            build_dir,
            "gen",
            "build",
            "bazel",
            "workspace",
            "bazel-bin",
            "build",
            "tools",
            "bazel2gn",
            "bazel2gn_",
            "bazel2gn",
        ),
        os.path.join(workdir, build_dir, "host_x64", "bazel2gn"),
        os.path.join(workdir, build_dir, "host_arm64", "bazel2gn"),
    ]
    best_bin = None
    best_mtime = -1.0
    for c in candidates:
        try:
            if os.path.isfile(c) and os.access(c, os.X_OK):
                mt = os.path.getmtime(c)
                if mt >= max_src_mtime and mt > best_mtime:
                    best_bin = c
                    best_mtime = mt
        except OSError:
            pass
    if not best_bin:
        return None, None, None
    for plat in ("linux-x64", "linux-arm64", "mac-x64", "mac-arm64"):
        gn_bin = os.path.join(workdir, "prebuilt", "third_party", "gn", plat, "gn")
        bf_bin = os.path.join(
            workdir, "prebuilt", "third_party", "buildifier", plat, "buildifier"
        )
        if os.path.isfile(gn_bin) and os.path.isfile(bf_bin):
            return best_bin, gn_bin, bf_bin
    return None, None, None


def _git_show_base_file(rel_path, fallback_refs=()):
    """Reads `rel_path` from the effective git change base (then any `fallback_refs`), or ''."""
    refs = [change_base] + [r for r in fallback_refs if r != change_base]
    for ref in refs:
        try:
            out = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            continue
        if out:
            return out
    return ""


# Read the base BUILD.bazel content for a package at change_base (or HEAD~1).
def _read_base_bazel_for_b2gn(pkg_dir):
    rel = f"{pkg_dir}/BUILD.bazel" if pkg_dir else "BUILD.bazel"
    return _git_show_base_file(rel, fallback_refs=("HEAD~1",))


# Extract normalized top-level BUILD.bazel statements that bazel2gn converts to GN.
def _extract_b2gn_synced_statements(bz_txt):
    if not bz_txt:
        return None
    try:
        tree = ast.parse(bz_txt)
    except SyntaxError:
        return None
    lines = bz_txt.splitlines()
    non_b2gn_calls = NO_GN_EQUIV_BAZEL_RULES | {"load", "licenses", "exports_files"}
    synced = []
    for stmt in tree.body:
        start_ln = max(1, getattr(stmt, "lineno", 1))
        end_ln = max(start_ln, getattr(stmt, "end_lineno", start_ln))
        idx = start_ln - 2
        has_skip = False
        b2gn_anns = []
        while idx >= 0:
            s_ln = lines[idx].strip()
            if not s_ln:
                idx -= 1
                continue
            if s_ln.startswith("#"):
                if re.search(r"^#\s*@bazel2gn:skip\b", s_ln):
                    has_skip = True
                elif "@bazel2gn:" in s_ln:
                    b2gn_anns.append(s_ln)
                idx -= 1
                continue
            break
        if has_skip:
            continue
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            fn = (
                call.func.id
                if isinstance(call.func, ast.Name)
                else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
            )
            if fn in non_b2gn_calls:
                continue
            if fn == "package":
                kw_names = {kw.arg for kw in call.keywords if kw.arg}
                if "default_visibility" not in kw_names:
                    continue
        stmt_lines = []
        for raw_ln in lines[start_ln - 1 : end_ln]:
            s_in = raw_ln.strip()
            if s_in.startswith("#"):
                if "@bazel2gn:" in s_in:
                    stmt_lines.append(s_in)
            else:
                stmt_lines.append(re.sub(r"#(?!\s*@bazel2gn:).*$", "", raw_ln).strip())
        norm_stmt = " ".join(
            part for part in (list(reversed(b2gn_anns)) + stmt_lines) if part
        )
        synced.append(re.sub(r"\s+", " ", norm_stmt).strip())
    return synced


# Check whether an ancillary package outside target_dirs only modified non-bazel2gn statements.
def _has_only_non_synced_ancillary_bazel_edits(pkg_dir, cur_bz_txt):
    if not target_dirs or pkg_dir in target_dirs:
        return False
    if f"{pkg_dir}/BUILD.gn" in changed:
        return False
    base_bz_txt = _read_base_bazel_for_b2gn(pkg_dir)
    if not base_bz_txt:
        return False
    cur_synced = _extract_b2gn_synced_statements(cur_bz_txt)
    base_synced = _extract_b2gn_synced_statements(base_bz_txt)
    return cur_synced is not None and cur_synced == base_synced


# Check whether a bazel2gn conversion error in an ancillary package outside target_dirs
# comes from an untouched rule already present in the base BUILD.bazel.
def _is_preexisting_ancillary_b2gn_diag(diag):
    if not isinstance(diag, dict) or diag.get("out_of_sync"):
        return False
    d_dir = (diag.get("directory") or "").strip("/")
    if not d_dir or not target_dirs or d_dir in target_dirs:
        return False
    base_txt = _read_base_bazel_for_b2gn(d_dir)
    if not base_txt:
        return False
    d_msg = (diag.get("message") or "").strip()
    m_unk = re.match(r"^([A-Za-z0-9_]+)\s+is not a known Bazel rule\b", d_msg)
    if m_unk:
        unk_rule = m_unk.group(1)
        if re.search(rf"(?m)^\s*{re.escape(unk_rule)}\s*\(", base_txt):
            return True
    return False


# Execute bazel2gn --check_only --json on a list of directories and return parsed diagnostics.
def _run_bazel2gn_check_only(b2gn_bin, gn_bin, bf_bin, check_dirs):
    import tempfile

    if not check_dirs:
        return 0, []
    fd, list_path = tempfile.mkstemp(prefix="planter-b2gn-dirs.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            for d in check_dirs:
                f.write(d + "\n")
        res = subprocess.run(
            [
                b2gn_bin,
                "--check_only",
                "--json",
                "--fuchsia_dir",
                workdir,
                "--gn_bin",
                gn_bin,
                "--buildifier_bin",
                bf_bin,
                "--directory_list",
                list_path,
            ],
            cwd=workdir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
        )
    except Exception:
        return 0, []
    finally:
        try:
            os.unlink(list_path)
        except OSError:
            pass
    if res.returncode == 0:
        return 0, []
    try:
        diags = json.loads(res.stdout) if res.stdout.strip().startswith("[") else []
    except Exception:
        diags = []
    return res.returncode, diags


# Check dual-build packages for redundant `raw_attribute:source_root` on `crate = ":..."`
# tests and verify BUILD.gn/BUILD.bazel sync via `bazel2gn --check_only`.
def check_bazel2gn_sync(dirs_to_check):
    sentinel = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##"
    raw_src_root_re = re.compile(
        r"^\s*#\s*@bazel2gn:raw_attribute:\s*source_root\s*=\s*"
    )
    dual_dirs = []
    for d in sorted(dirs_to_check):
        b_abs = os.path.join(workdir, d, "BUILD.bazel")
        g_abs = os.path.join(workdir, d, "BUILD.gn")
        if not (os.path.isfile(b_abs) and os.path.isfile(g_abs)):
            continue
        try:
            g_txt = open(g_abs, encoding="utf-8").read()
            b_txt = open(b_abs, encoding="utf-8").read()
        except OSError:
            continue
        if sentinel not in g_txt:
            continue
        if _has_only_non_synced_ancillary_bazel_edits(d, b_txt):
            continue
        dual_dirs.append(d)
        b_lines = b_txt.splitlines()
        try:
            b_tree = ast.parse(b_txt, filename=b_abs)
        except Exception:
            continue
        for stmt in b_tree.body:
            if not (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
            ):
                continue
            call = stmt.value
            if call.func.id not in ("rustc_test", "rust_test", "host_rustc_test"):
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            if "crate" not in kws:
                continue
            t_name = (
                kws["name"].value
                if "name" in kws
                and isinstance(kws["name"], ast.Constant)
                and isinstance(kws["name"].value, str)
                else call.func.id
            )
            idx = stmt.lineno - 2
            while idx >= 0 and b_lines[idx].strip().startswith("#"):
                if raw_src_root_re.match(b_lines[idx]):
                    msg = (
                        f"`{call.func.id}(name = \"{t_name}\")` in '{d}/BUILD.bazel' "
                        "sets `crate` and also carries "
                        f"`{b_lines[idx].strip()}`. `bazel2gn` automatically inherits "
                        "`crate_root` from `crate` (including `source_root = \"src/main.rs\"` "
                        "for binary crates), so the raw attribute emits a duplicate "
                        "`source_root` assignment in BUILD.gn and fails `verify_bazel2gn`."
                    )
                    rem = (
                        f"Remove `{b_lines[idx].strip()}` above "
                        f"`{call.func.id}(name = \"{t_name}\")` in '{d}/BUILD.bazel' "
                        f"and re-run `fx bazel2gn -d {d}`."
                    )
                    _emit(findings, "bazel2gn_out_of_sync", "error", f"{d}/BUILD.bazel", idx + 1, msg, rem)
                idx -= 1

    if not dual_dirs:
        return
    b2gn_bin, gn_bin, bf_bin = find_up_to_date_bazel2gn_bin()
    if not b2gn_bin:
        return
    rc, diags = _run_bazel2gn_check_only(b2gn_bin, gn_bin, bf_bin, dual_dirs)
    if rc == 0:
        return
    preexisting_anc_dirs = {
        (diag.get("directory") or "").strip("/")
        for diag in diags
        if _is_preexisting_ancillary_b2gn_diag(diag)
    }
    if preexisting_anc_dirs:
        remaining_dirs = [d for d in dual_dirs if d not in preexisting_anc_dirs]
        if remaining_dirs and len(remaining_dirs) < len(dual_dirs):
            _, diags = _run_bazel2gn_check_only(
                b2gn_bin, gn_bin, bf_bin, remaining_dirs
            )
        else:
            diags = [
                d for d in diags if not _is_preexisting_ancillary_b2gn_diag(d)
            ]
    for diag in diags:
        if not isinstance(diag, dict) or _is_preexisting_ancillary_b2gn_diag(diag):
            continue
        d_dir = diag.get("directory") or dual_dirs[0]
        f_rel = diag.get("file") or f"{d_dir}/BUILD.gn"
        ln = int(diag.get("line") or 1)
        d_msg = (diag.get("message") or "").strip()
        if diag.get("out_of_sync"):
            msg = (
                f"'{f_rel}' is out of sync with '{d_dir}/BUILD.bazel' "
                f"(`verify_bazel2gn`):\n{d_msg}"
            )
            rem = (
                f"Update '{d_dir}/BUILD.bazel' rather than editing below "
                "`## BAZEL2GN SENTINEL`, then re-sync with "
                f"`fx bazel2gn -d {d_dir}`."
            )
        else:
            msg = f"`bazel2gn` failed on '{f_rel}': {d_msg}"
            rem = (
                f"Fix the `bazel2gn` conversion error in '{d_dir}/BUILD.bazel' "
                f"and re-run `fx bazel2gn -d {d_dir}`."
            )
        _emit(findings, "bazel2gn_out_of_sync", "error", f_rel, ln, msg, rem)


check_bazel2gn_sync(candidate_dirs)


# GN-only constructs in the bazel2gn-generated section of a dual-build BUILD.gn must come from
# BUILD.bazel. bazel2gn emits them only from a sanctioned source on the Bazel target (or, for
# top-level statements, anywhere in BUILD.bazel):
#   - a `# @bazel2gn:raw_attribute:`, `raw_overwrite:`, `value_overwrite:` or `path_overwrite:`
#     annotation whose text contains the construct;
#   - `disable_instrumentation = True` (`exclude_toolchain_tags = [ "instrumented" ]`) or
#     `disable_build_flags` (`configs -=`);
#   - the dedicated tags/attributes of newer bazel2gn versions: `exclude_toolchain_tags=<tag>`,
#     `assert_no_deps_<toolchain>=<label>` (e.g. `assert_no_deps_current_toolchain=`), a large
#     RBE worker request (`exec_properties` naming the large worker, or a large-worker tag) for
#     `concurrent_jobs.rust_highmem`, `concurrent_jobs=<pool>`, and `$(execpath <label>)` /
#     `$(location <label>)` or a local `compile_data` label for toolchain-qualified references to
#     that label.
# A construct with none of these sources is a hand edit below the sentinel: the next `fx bazel2gn`
# run drops it and `verify_bazel2gn` fails in CQ (unsanctioned_gn_construct_below_sentinel, error).
# This check needs no bazel2gn binary, so it also catches hand edits while `check_bazel2gn_sync`
# cannot run. Only constructs that the generated target did not already have at the change base
# are reported.
GN_EXEC_REF_RE = re.compile(
    r"\$\((?:execpaths?|rootpaths?|locations?|rlocationpaths?)\s+([^)\s]+)\s*\)"
)
LARGE_WORKER_RE = re.compile(r"(?i)large[_-]?(?:rbe[_-]?)?worker|rust[_-]?highmem")
GN_TOOLCHAIN_LABEL_RE = re.compile(r'"([^"\s]*\(\$[A-Za-z0-9_]*toolchain\))"')
BAZEL2GN_ANNOTATION_RE = re.compile(
    r"#\s*@bazel2gn:(?:raw_attribute|raw_overwrite|value_overwrite|path_overwrite):(.*)$"
)


def _nows(s):
    return re.sub(r"\s+", "", s)


_bazel2gn_features = None


def bazel2gn_features():
    """Which dedicated tag/attribute translations the checkout's bazel2gn implements.

    Annotations (`raw_attribute`, `raw_overwrite`, ...) and `disable_instrumentation` /
    `disable_build_flags` are always available. The newer forms only sanction a generated
    construct once the checkout's bazel2gn sources (non-test .go files) implement them.
    """
    global _bazel2gn_features
    if _bazel2gn_features is not None:
        return _bazel2gn_features
    text = BAZEL2GN_GO_SOURCES
    _bazel2gn_features = {
        "exclude_toolchain_tags_tag": "exclude_toolchain_tags=" in text,
        "qualified_assert_no_deps_tag": bool(re.search(r"assert_no_deps_[A-Za-z0-9_]*toolchain", text)),
        "large_worker": "concurrent_jobs" in text,
        "exec_refs": bool(re.search(r"execpath|\$\(location", text)),
    }
    return _bazel2gn_features


def canonical_gn_or_bazel_label(pkg_dir, label):
    lbl = label.strip().strip('"')
    if lbl.startswith("@//"):
        lbl = lbl[1:]
    if lbl.startswith(":"):
        lbl = f"//{pkg_dir}:{lbl[1:]}"
    lbl = lbl.replace("//third_party/rust_crates/vendor:", "//third_party/rust_crates:")
    if lbl.startswith("//") and ":" in lbl:
        pkg_part, tgt = lbl.split(":", 1)
        if tgt == pkg_part.rsplit("/", 1)[-1]:
            lbl = pkg_part
    return lbl


def gn_only_constructs(body):
    """(key, display, offset) for every GN-only construct in a GN body (comments stripped)."""
    code = re.sub(r"#[^\n]*", lambda m: " " * len(m.group(0)), body)
    out = []
    for m in re.finditer(r"(?<![\w.])exclude_toolchain_tags\s*\+?=\s*\[([^\]]*)\]", code):
        for v in re.findall(r'"([^"]+)"', m.group(1)):
            out.append((("exclude_toolchain_tags", v), f'exclude_toolchain_tags "{v}"', m.start()))
    for m in re.finditer(r"(?<![\w.])configs\s*-=\s*(\[[^\]]*\]|[A-Za-z_][\w.]*)", code):
        out.append((("configs -=", _nows(m.group(1))), f"configs -= {m.group(1).strip()}", m.start()))
    for m in re.finditer(r"(?<![\w.])disable_syslog_backend\s*=", code):
        out.append((("disable_syslog_backend",), "disable_syslog_backend", m.start()))
    for m in re.finditer(r"(?<![\w.])forward_variables_from\s*\(\s*([^,)]+)", code):
        arg = _nows(m.group(1))
        out.append((("forward_variables_from", arg), f"forward_variables_from({arg}, ...)", m.start()))
    for m in re.finditer(r"(?<![\w.])concurrent_jobs\.([A-Za-z_]\w*)", code):
        out.append((("concurrent_jobs", m.group(1)), f"concurrent_jobs.{m.group(1)}", m.start()))
    for m in GN_TOOLCHAIN_LABEL_RE.finditer(code):
        attr_m = None
        for attr_m in re.finditer(r"(?<![\w.])([A-Za-z_]\w*)\s*[+-]?=(?!=)", code[: m.start()]):
            pass
        attr = attr_m.group(1) if attr_m else ""
        out.append((("toolchain_label", m.group(1), attr), f'"{m.group(1)}"', m.start()))
    return out


def bazel2gn_sources(bazel_text, pkg_dir):
    """Per-target and file-level BUILD.bazel sources of GN-only constructs."""
    lines = bazel_text.splitlines()
    file_ann = [_nows(m.group(1)) for ln in lines for m in [BAZEL2GN_ANNOTATION_RE.search(ln)] if m]
    per_target = {}
    try:
        tree = ast.parse(bazel_text)
    except Exception:
        return per_target, file_ann
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        name_node = kws.get("name")
        if not (isinstance(name_node, ast.Constant) and isinstance(name_node.value, str)):
            continue
        start = node.lineno
        end = getattr(call, "end_lineno", start) or start
        idx = start - 2
        while idx >= 0 and (not lines[idx].strip() or lines[idx].strip().startswith("#")):
            idx -= 1
        span = lines[idx + 1 : end]
        ann = [_nows(m.group(1)) for ln in span for m in [BAZEL2GN_ANNOTATION_RE.search(ln)] if m]
        tags = []
        tags_node = kws.get("tags")
        if tags_node is not None:
            for sub in ast.walk(tags_node):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    tags.append(sub.value)
        disable_instr = kws.get("disable_instrumentation")
        exec_props_seg = (
            ast.get_source_segment(bazel_text, kws["exec_properties"]) or ""
            if "exec_properties" in kws
            else ""
        )
        exec_labels = set()
        for attr in ("rustc_env", "compile_data", "data", "env"):
            node_v = kws.get(attr)
            if node_v is None:
                continue
            for sub in ast.walk(node_v):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    for em in GN_EXEC_REF_RE.finditer(sub.value):
                        exec_labels.add(canonical_gn_or_bazel_label(pkg_dir, em.group(1)))
                    if attr == "compile_data" and sub.value.startswith(":"):
                        exec_labels.add(canonical_gn_or_bazel_label(pkg_dir, sub.value))
        per_target[name_node.value] = {
            "ann": ann,
            "tags": tags,
            "disable_instrumentation": isinstance(disable_instr, ast.Constant)
            and disable_instr.value is True,
            "disable_build_flags": "disable_build_flags" in kws,
            "large_worker": bool(LARGE_WORKER_RE.search(exec_props_seg))
            or any(LARGE_WORKER_RE.search(t) for t in tags),
            "exec_labels": exec_labels,
        }
    return per_target, file_ann


def gn_construct_sanctioned(key, src, ann, pkg_dir):
    kind = key[0]
    feats = bazel2gn_features()
    tags = src.get("tags", []) if src else []
    if kind == "exclude_toolchain_tags":
        v = key[1]
        if any("exclude_toolchain_tags" in a and f'"{v}"' in a for a in ann):
            return True
        if feats["exclude_toolchain_tags_tag"] and any(
            t.startswith("exclude_toolchain_tags=") and v in t.split("=", 1)[1].split(",")
            for t in tags
        ):
            return True
        return bool(src) and v == "instrumented" and src["disable_instrumentation"]
    if kind == "configs -=":
        return any("configs-=" in a for a in ann) or (bool(src) and src["disable_build_flags"])
    if kind == "disable_syslog_backend":
        return any("disable_syslog_backend" in a for a in ann)
    if kind == "forward_variables_from":
        if any("forward_variables_from(" + key[1] in a for a in ann):
            return True
        return (
            "concurrent_jobs" in key[1]
            and feats["large_worker"]
            and bool(src)
            and (src["large_worker"] or any(t.startswith("concurrent_jobs=") for t in tags))
        )
    if kind == "concurrent_jobs":
        pool = key[1]
        if any("concurrent_jobs." + pool in a for a in ann):
            return True
        if not feats["large_worker"]:
            return False
        if any(t == f"concurrent_jobs={pool}" for t in tags):
            return True
        return pool == "rust_highmem" and bool(src) and src["large_worker"]
    if kind == "toolchain_label":
        label = key[1]
        if any(_nows(label) in a for a in ann):
            return True
        base, _, qual = label.partition("($")
        toolchain = qual.rstrip(")")
        base_canon = canonical_gn_or_bazel_label(pkg_dir, base)
        if feats["qualified_assert_no_deps_tag"]:
            for t in tags:
                tm = re.match(r"assert_no_deps_([A-Za-z0-9_]*toolchain)=(.+)$", t)
                if (
                    tm
                    and tm.group(1) == toolchain
                    and canonical_gn_or_bazel_label(pkg_dir, tm.group(2)) == base_canon
                ):
                    return True
        return feats["exec_refs"] and bool(src) and base_canon in src["exec_labels"]
    return False


def gn_unsupported_form_note(key, src):
    """Names a dedicated BUILD.bazel form the target uses that the checkout's bazel2gn lacks."""
    if not src:
        return ""
    feats = bazel2gn_features()
    kind = key[0]
    form = ""
    if kind == "exclude_toolchain_tags" and not feats["exclude_toolchain_tags_tag"]:
        if any(t.startswith("exclude_toolchain_tags=") for t in src["tags"]):
            form = "the `exclude_toolchain_tags=<tag>` tag"
    elif kind in ("concurrent_jobs", "forward_variables_from") and not feats["large_worker"]:
        if src["large_worker"]:
            form = "the large RBE worker request"
    elif kind == "toolchain_label" and key[2] == "assert_no_deps":
        if not feats["qualified_assert_no_deps_tag"] and any(
            re.match(r"assert_no_deps_[A-Za-z0-9_]*toolchain=", t) for t in src["tags"]
        ):
            form = "the `assert_no_deps_<toolchain>=<label>` tag"
    elif kind == "toolchain_label":
        if not feats["exec_refs"] and src["exec_labels"]:
            form = "`$(execpath ...)`/`compile_data` label translation"
    if not form:
        return ""
    return (
        f" The target uses {form}, but this checkout's build/tools/bazel2gn does not implement "
        "it yet, so bazel2gn cannot have generated this line"
    )


def gn_construct_recipe(key):
    kind = key[0]
    if kind == "exclude_toolchain_tags":
        return (
            f'`tags = ["exclude_toolchain_tags={key[1]}"]`'
            + (" (or `disable_instrumentation = True` on `fx_cc_binary`)" if key[1] == "instrumented" else "")
            + "; never `# @bazel2gn:raw_attribute:exclude_toolchain_tags`, which "
            "`raw_attribute_for_supported_bazel2gn_tag` rejects"
        )
    if kind == "configs -=":
        return (
            "`disable_build_flags = [...]` on `fx_cc_*` targets, otherwise "
            "`# @bazel2gn:raw_attribute:configs -= [ ... ]`"
        )
    if kind in ("concurrent_jobs", "forward_variables_from") and (
        kind == "concurrent_jobs" or "concurrent_jobs" in key[1]
    ):
        return (
            "the large RBE worker request where the checkout's bazel2gn maps it, otherwise three "
            "stacked raw attributes (GN cannot chain `concurrent_jobs.rust_highmem.configs`): "
            "`# @bazel2gn:raw_attribute:_concurrent_jobs = concurrent_jobs.rust_highmem`, "
            "`# @bazel2gn:raw_attribute:configs += _concurrent_jobs.configs`, "
            '`# @bazel2gn:raw_attribute:forward_variables_from(_concurrent_jobs.vars, "*")`, with '
            '`import("//build/toolchain/concurrent_jobs.gni")` above the sentinel'
        )
    if kind == "toolchain_label" and key[2] == "assert_no_deps":
        return (
            f'`# @bazel2gn:raw_attribute:assert_no_deps += [ "{key[1]}" ]` above the target (keep the '
            "unqualified entries as `assert_no_deps=<label>` tags), or the "
            "`assert_no_deps_current_toolchain=<label>` tag where the checkout's bazel2gn supports it"
        )
    if kind == "toolchain_label" and key[2] in ("deps", "public_deps", "data_deps", "non_rust_deps"):
        return (
            f'`# @bazel2gn:raw_attribute:{key[2]} += [ "{key[1]}" ]` above the target, '
            "`# @bazel2gn:path_overwrite:` on the list entry, or (for a generated file embedded "
            "through `rustc_env`/`compile_data`) the `$(execpath <label>)` translation where the "
            "checkout's bazel2gn supports it"
        )
    if kind == "toolchain_label":
        return (
            "the `$(execpath <label>)` translation of `rustc_env`/`compile_data` where the "
            "checkout's bazel2gn supports it, otherwise file-level GN variables above the sentinel "
            "(computed with `get_label_info(...)`) referenced through `# @bazel2gn:value_overwrite:` "
            "on the `rustc_env` entry and `# @bazel2gn:raw_overwrite:[ <var> ]` on the closing `],` of "
            "`compile_data`, plus `# @bazel2gn:raw_attribute:deps += [ ... ]` for the producing target"
        )
    return f"`# @bazel2gn:raw_attribute:` naming `{key[-1]}` above the target"


def check_gn_only_constructs_below_sentinel(dirs_to_check):
    target_re = re.compile(r'^[ \t]*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for d in sorted(dirs_to_check):
        b_abs = os.path.join(workdir, d, "BUILD.bazel")
        g_abs = os.path.join(workdir, d, "BUILD.gn")
        if not (os.path.isfile(b_abs) and os.path.isfile(g_abs)):
            continue
        try:
            g_txt = open(g_abs, encoding="utf-8").read()
            b_txt = open(b_abs, encoding="utf-8").read()
        except OSError:
            continue
        gn_rel = f"{d}/BUILD.gn"
        bazel_rel = f"{d}/BUILD.bazel"
        # Cross-compiled arm64 bridged host tools (bazel_host_tool / bazel_ffx_tool) that
        # reach HOST_CONSTRAINTS are reported once, by bazel_minimality
        # host_tool_transitive_host_constraints (transitive, proc-macro aware).
        sm = GN_SENTINEL_RE.search(g_txt)
        if not sm:
            continue
        post_off = sm.end()
        post = g_txt[post_off:]

        def segments(text):
            segs = []
            covered = []
            for tm in target_re.finditer(text):
                brace = tm.end() - 1
                end = find_closing_brace(text, brace)
                segs.append((tm.group(2), tm.group(1), brace + 1, text[brace + 1 : end]))
                covered.append((tm.start(), end + 1))
            rest = list(text)
            for s, e in covered:
                for i in range(s, min(e, len(rest))):
                    if rest[i] != "\n":
                        rest[i] = " "
            segs.append((None, None, 0, "".join(rest)))
            return segs

        base_keys = {}
        try:
            base_txt = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_txt = ""
        bsm = GN_SENTINEL_RE.search(base_txt) if base_txt else None
        if bsm:
            for name, _, _, body in segments(base_txt[bsm.end() :]):
                base_keys.setdefault(name, set()).update(k for k, _, _ in gn_only_constructs(body))

        per_target, file_ann = bazel2gn_sources(b_txt, d)
        reported = set()
        for name, tmpl, body_off, body in segments(post):
            if tmpl == "ffx_tool" and name:
                line = g_txt.count("\n", 0, post_off + body_off) + 1
                _emit(findings, "unsanctioned_gn_construct_below_sentinel", "error", gn_rel, line, (
                        f'`ffx_tool("{name}")` in the bazel2gn-generated section below '
                        f"`## BAZEL2GN SENTINEL` of '{gn_rel}' is stale: `bazel2gn` now maps "
                        "`ffx_tool` to `noGNEquivalent` and "
                        "`//build/tools/bazel2gn/bazel_migration.gni` no longer imports "
                        "`//src/developer/ffx/build/ffx_tool.gni`, causing `gn gen` to fail "
                        "with `Unknown function`."
                    ), (
                        f"Remove the stale `ffx_tool(\"{name}\")` block below "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' by re-running "
                        f"`fx bazel2gn -d {d}` (if no other targets remain in '{gn_rel}', "
                        f"delete '{gn_rel}'). If GN targets still depend on `//{d}:{name}` "
                        f"(or `:{name}_versioned` / `:{name}_host_tool`), add "
                        "`import(\"//src/developer/ffx/build/ffx_tool.gni\")` and "
                        f"`bazel_ffx_tool(\"{name}\") {{ ... }}` above `## BAZEL2GN SENTINEL`."
                    ))
                continue
            src = per_target.get(name) if name else None
            ann = (src["ann"] if src else []) + ([] if src else file_ann)
            for key, display, off in gn_only_constructs(body):
                if key in base_keys.get(name, set()) or (name, key) in reported:
                    continue
                if gn_construct_sanctioned(key, src, ann, d):
                    continue
                reported.add((name, key))
                line = g_txt.count("\n", 0, post_off + body_off + off) + 1
                where = (
                    f'bazel2gn-generated `{tmpl}("{name}")`' if name else "the bazel2gn-generated section"
                )
                _emit(findings, "unsanctioned_gn_construct_below_sentinel", "error", gn_rel, line, (
                        f"`{display}` in {where} below `## BAZEL2GN SENTINEL` of '{gn_rel}' has no "
                        f"source in '{bazel_rel}'"
                        + (f' (`{name}` there has no annotation, tag or attribute that emits it)' if src else "")
                        + "."
                        + gn_unsupported_form_note(key, src)
                        + (". " if gn_unsupported_form_note(key, src) else " ")
                        + "It is a hand edit below the sentinel: the next `fx bazel2gn` run drops it "
                        "and `verify_bazel2gn` fails in CQ."
                    ), (
                        f"Express it in '{bazel_rel}'"
                        + (f' on `{name}`' if name else "")
                        + f" with {gn_construct_recipe(key)}, then re-run `fx bazel2gn -d {d}`. "
                        "If bazel2gn cannot express it at all, keep the whole target hand-written "
                        "above the sentinel and mark the Bazel target `# @bazel2gn:skip` with a comment "
                        "naming why bazel2gn cannot generate it (e.g. `# bazel2gn cannot express "
                        "<construct>; hand-written above the GN sentinel.`, not \"kept/remains in GN\", "
                        "which stale_gn_retention_comment_in_bazel rejects)."
                    ))


check_gn_only_constructs_below_sentinel(candidate_dirs)


# Parse a BUILD.bazel AST into exported files, targets, and cml_shard sources.
def _extract_bazel_exports_and_targets(tree):
    var_lists = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            var_lists[node.targets[0].id] = [
                sub.value
                for sub in ast.walk(node.value)
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
            ]

    def _eval_str_list(val):
        if val is None:
            return []
        if isinstance(val, ast.BinOp):
            return _eval_str_list(val.left) + _eval_str_list(val.right)
        if isinstance(val, ast.List):
            out = []
            for elt in val.elts:
                if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                    out.append(elt.value)
            return out
        if isinstance(val, ast.Name) and val.id in var_lists:
            return list(var_lists[val.id])
        return []

    pkg_default_vis = []
    exports = {}
    export_calls = []
    targets = {}
    cml_shards = set()
    pkg_lineno = 1

    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        if rule == "package":
            pkg_lineno = node.lineno
            if "default_visibility" in kws:
                pkg_default_vis = _eval_str_list(kws["default_visibility"])
            continue
        if rule == "exports_files":
            srcs_node = call.args[0] if call.args else kws.get("srcs")
            vis_node = call.args[1] if len(call.args) >= 2 else kws.get("visibility")
            srcs_list = _eval_str_list(srcs_node)
            vis_list = (
                _eval_str_list(vis_node)
                if vis_node is not None
                else (list(pkg_default_vis) if pkg_default_vis else ["//visibility:public"])
            )
            if srcs_list:
                export_calls.append((srcs_list, vis_list))
                for f in srcs_list:
                    exports.setdefault(f, set()).update(vis_list)
            continue
        name_node = kws.get("name")
        if not (isinstance(name_node, ast.Constant) and isinstance(name_node.value, str)):
            continue
        tname = name_node.value
        vis_list = (
            _eval_str_list(kws["visibility"])
            if "visibility" in kws
            else list(pkg_default_vis)
        )
        targets[tname] = (rule, kws, vis_list, node.lineno)
        if rule == "cml_shard":
            shard_node = kws.get("shard")
            if isinstance(shard_node, ast.Constant) and isinstance(shard_node.value, str):
                cml_shards.add(shard_node.value)
    return exports, export_calls, targets, cml_shards, pkg_lineno


# Check whether current visibility entries cover a required visibility spec from change_base.
def _vis_covers_entry(cur_vis_set, req_v, pkg_dir):
    if "//visibility:public" in cur_vis_set or "//:__subpackages__" in cur_vis_set:
        return True
    if req_v in cur_vis_set or req_v in (":__pkg__", "//visibility:private", f"//{pkg_dir}:__pkg__"):
        return True
    if not (req_v.startswith("//") and ":" in req_v):
        return False
    req_pkg = req_v[2:].split(":", 1)[0].strip("/")
    for cv in cur_vis_set:
        if cv.startswith("//") and cv.endswith(":__subpackages__"):
            cv_prefix = cv[2 : -len(":__subpackages__")].strip("/")
            if not cv_prefix or req_pkg == cv_prefix or req_pkg.startswith(cv_prefix + "/"):
                return True
    return False


# Find BUILD.bazel or .bzl files outside bazel_rel that reference a given label string.
def _find_external_bazel_label_callers(label_str, bazel_rel):
    try:
        out = subprocess.check_output(
            [
                "git",
                "-C",
                workdir,
                "grep",
                "-l",
                "-F",
                label_str,
                "--",
                "*BUILD.bazel",
                "*.bzl",
            ],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return []
    return sorted(
        f.strip() for f in out.splitlines() if f.strip() and f.strip() != bazel_rel
    )


# Find the deepest Bazel subpackage under pkg_dir that now owns a nested relative file path.
def _find_owning_bazel_subpackage(pkg_dir, rel_file):
    if "/" not in rel_file:
        return None
    cur_sub = os.path.dirname(rel_file).strip("/")
    while cur_sub:
        sub_pkg = f"{pkg_dir}/{cur_sub}" if pkg_dir else cur_sub
        for bname in ("BUILD.bazel", "BUILD"):
            sub_build = os.path.join(workdir, sub_pkg, bname)
            if os.path.isfile(sub_build):
                sub_rel = os.path.relpath(rel_file, cur_sub)
                return sub_pkg, sub_rel, f"{sub_pkg}/{bname}", sub_build
        cur_sub = os.path.dirname(cur_sub).strip("/")
    return None


# Check whether a newly created Bazel subpackage exports a file relocated from an ancestor.
def _subpackage_exports_relocated_file(sub_build_abs, sub_bazel_rel, sub_rel):
    try:
        sub_txt = open(sub_build_abs, encoding="utf-8").read()
        sub_tree = ast.parse(sub_txt, filename=sub_bazel_rel)
        s_exports, _, s_targets, s_shards, s_lineno = (
            _extract_bazel_exports_and_targets(sub_tree)
        )
    except Exception:
        return False, 1
    is_exported = (
        sub_rel in s_exports or sub_rel in s_targets or sub_rel in s_shards
    )
    return is_exported, s_lineno


# Check that pre-existing exports_files(...) entries and targets in BUILD.bazel at
# PLANTER_CHANGE_BASE are preserved when migrating additional targets into the package.
def check_preexisting_bazel_exports_and_targets(dirs_to_check):
    test_plumbing_rules = {
        "fx_packaged_binary",
        "fx_component_manifest",
        "fx_test_component",
        "fx_unittest_component",
        "fx_package",
    }
    for d in sorted(dirs_to_check):
        bazel_rel = f"{d}/BUILD.bazel"
        b_abs = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(b_abs):
            continue
        try:
            base_txt = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            cur_txt = open(b_abs, encoding="utf-8").read()
            base_tree = ast.parse(base_txt, filename=bazel_rel)
            cur_tree = ast.parse(cur_txt, filename=bazel_rel)
        except Exception:
            continue

        base_exports, base_export_calls, base_targets, _, _ = (
            _extract_bazel_exports_and_targets(base_tree)
        )
        cur_exports, _, cur_targets, cur_cml_shards, pkg_lineno = (
            _extract_bazel_exports_and_targets(cur_tree)
        )

        for srcs_list, base_vis in base_export_calls:
            dropped_files = []
            lost_vis_files = []
            callers = set()
            for f in srcs_list:
                if not os.path.exists(os.path.join(workdir, d, f)):
                    continue
                lbl = f"//{d}:{f}"
                sub_owner = _find_owning_bazel_subpackage(d, f)
                if sub_owner is not None:
                    sub_pkg, sub_rel, sub_bazel_rel, sub_build_abs = sub_owner
                    is_exp, sub_lineno = _subpackage_exports_relocated_file(
                        sub_build_abs, sub_bazel_rel, sub_rel
                    )
                    if is_exp:
                        continue
                    sub_lbl = f"//{sub_pkg}:{sub_rel}"
                    sub_callers = set(
                        _find_external_bazel_label_callers(lbl, sub_bazel_rel)
                    ) | set(
                        _find_external_bazel_label_callers(sub_lbl, sub_bazel_rel)
                    )
                    if not sub_callers:
                        continue
                    caller_note = (
                        f" (referenced by "
                        f"{', '.join(f'`{c}`' for c in sorted(sub_callers)[:3])})"
                    )
                    msg = (
                        f"'{bazel_rel}' previously exported `{f}`, which now lives inside "
                        f"Bazel subpackage `//{sub_pkg}`, but '{sub_bazel_rel}' does not "
                        f"export `{sub_rel}`{caller_note}."
                    )
                    rem = (
                        f"Add `exports_files([\"{sub_rel}\"], visibility = [...])` in "
                        f"'{sub_bazel_rel}' (never in '{bazel_rel}', since `//{sub_pkg}` "
                        f"is now a Bazel subpackage) and update any callers of `{lbl}` "
                        f"to `{sub_lbl}`."
                    )
                    _emit(findings, "dropped_preexisting_bazel_export", "error", sub_bazel_rel, sub_lineno, msg, rem)
                    continue
                if f in cur_exports or f in cur_targets:
                    cur_vis = (
                        cur_exports.get(f, set())
                        if f in cur_exports
                        else (set(cur_targets[f][2]) or {f"//{d}:__pkg__"})
                    )
                    missing_v = [
                        v
                        for v in base_vis
                        if v != "//visibility:public"
                        and not _vis_covers_entry(cur_vis, v, d)
                    ]
                    if missing_v:
                        lost_vis_files.append((f, missing_v))
                        callers.update(_find_external_bazel_label_callers(lbl, bazel_rel))
                    continue
                ext_callers = _find_external_bazel_label_callers(lbl, bazel_rel)
                if f in cur_cml_shards and not ext_callers:
                    continue
                dropped_files.append(f)
                callers.update(ext_callers)

            if dropped_files:
                files_fmt = ", ".join(f"`{f}`" for f in dropped_files)
                vis_fmt = json.dumps(base_vis)
                srcs_fmt = json.dumps(dropped_files)
                caller_note = (
                    f" (referenced by {', '.join(f'`{c}`' for c in sorted(callers)[:3])})"
                    if callers
                    else ""
                )
                msg = (
                    f"'{bazel_rel}' dropped pre-existing `exports_files(...)` "
                    f"entries {files_fmt} (visibility `{vis_fmt}`) that existed in "
                    f"'{bazel_rel}' before this change{caller_note}."
                )
                rem = (
                    f"Restore `exports_files({srcs_fmt}, visibility = {vis_fmt})` in "
                    f"'{bazel_rel}'. When migrating a package that already has a "
                    "`BUILD.bazel` file, preserve all pre-existing `exports_files(...)` "
                    "declarations and their `visibility` scopes alongside the newly "
                    "migrated targets."
                )
                _emit(findings, "dropped_preexisting_bazel_export", "error", bazel_rel, pkg_lineno, msg, rem)

            for f, missing_v in lost_vis_files:
                mv_fmt = ", ".join(f"`{v}`" for v in missing_v)
                msg = (
                    f"'{bazel_rel}' narrowed or removed pre-existing visibility "
                    f"{mv_fmt} on exported file `{f}`."
                )
                rem = (
                    f"Keep {mv_fmt} in the `visibility` list of `exports_files(...)` "
                    f"(or target `{f}`) in '{bazel_rel}' so existing Bazel callers "
                    f"of `//{d}:{f}` continue to build."
                )
                _emit(findings, "dropped_preexisting_bazel_export", "error", bazel_rel, pkg_lineno, msg, rem)

        has_cur_test_pkg = any(
            r in ("fx_unittest_package", "fx_test") for r, _, _, _ in cur_targets.values()
        )
        for tname, (b_rule, b_kws, b_vis, _) in sorted(base_targets.items()):
            if tname in cur_targets or tname in cur_exports:
                if tname in cur_targets and b_vis and b_rule not in test_plumbing_rules:
                    cur_t_vis = set(cur_targets[tname][2]) or {f"//{d}:__pkg__"}
                    missing_tv = [
                        v
                        for v in b_vis
                        if v != "//visibility:public"
                        and not _vis_covers_entry(cur_t_vis, v, d)
                    ]
                    if missing_tv and not cur_targets[tname][2]:
                        mv_fmt = ", ".join(f"`{v}`" for v in missing_tv)
                        msg = (
                            f"'{bazel_rel}' dropped pre-existing `visibility` "
                            f"{mv_fmt} on `{b_rule}(name = \"{tname}\")`."
                        )
                        rem = (
                            f"Restore `visibility = {json.dumps(b_vis)}` on "
                            f"`{b_rule}(name = \"{tname}\")` in '{bazel_rel}'."
                        )
                        _emit(findings, "dropped_preexisting_bazel_target", "error", bazel_rel, cur_targets[tname][3], msg, rem)
                continue
            if b_rule == "filegroup" and "srcs" not in b_kws:
                continue
            if b_rule in test_plumbing_rules and has_cur_test_pkg:
                continue
            ext_callers = _find_external_bazel_label_callers(f"//{d}:{tname}", bazel_rel)
            caller_note = (
                f" (referenced by {', '.join(f'`{c}`' for c in ext_callers[:3])})"
                if ext_callers
                else ""
            )
            msg = (
                f"'{bazel_rel}' dropped pre-existing `{b_rule}(name = \"{tname}\")` "
                f"target that existed in '{bazel_rel}' before this change{caller_note}."
            )
            rem = (
                f"Restore `{b_rule}(name = \"{tname}\", ...)` (including its "
                f"pre-existing `visibility` {json.dumps(b_vis)}) in '{bazel_rel}' "
                f"(compare against `git show {change_base}:{bazel_rel}`)."
            )
            _emit(findings, "dropped_preexisting_bazel_target", "error", bazel_rel, pkg_lineno, msg, rem)


check_preexisting_bazel_exports_and_targets(candidate_dirs)


# Extract same-package target names referenced in a GN target body.
def _extract_same_pkg_gn_deps(body, pkg):
    targets = []
    for quoted in re.findall(r'"([^"]+)"', body):
        label = quoted.split("(", 1)[0].strip()
        if label.startswith(":"):
            targets.append(label[1:])
        elif label.startswith(f"//{pkg}:"):
            targets.append(label[len(f"//{pkg}:") :])
    return targets


# Check a dual-build package for install_host_tools() or bazel_host_tool() kept above
# the BAZEL2GN SENTINEL while bazel2gn still generates the host binary below it.
def check_dual_built_host_tools(dirs):
    host_bin_rules = ("rustc_binary", "go_binary", "executable", "cc_binary")
    sentinel = "## BAZEL2GN SENTINEL"
    for d in sorted(dirs):
        gn_rel = os.path.join(d, "BUILD.gn")
        bz_rel = os.path.join(d, "BUILD.bazel")
        gn_abs = os.path.join(workdir, gn_rel)
        bz_abs = os.path.join(workdir, bz_rel)
        if not (os.path.isfile(gn_abs) and os.path.isfile(bz_abs)):
            continue
        gn_info = parse_gn_file(gn_abs)
        if not gn_info or not gn_info["has_sentinel"]:
            continue
        generated_bins = {
            bname: (btmpl, bline, bbody)
            for bname, (btmpl, bline, bbody) in gn_info["post_targets"].items()
            if btmpl in host_bin_rules
        }
        if not generated_bins:
            continue

        reported_bins = set()
        # Check install_host_tools() above the sentinel referencing a generated binary.
        for iname, (itmpl, iline, ibody) in gn_info["pre_targets"].items():
            if itmpl != "install_host_tools":
                continue
            for dep_name in _extract_same_pkg_gn_deps(ibody, d):
                if dep_name not in generated_bins:
                    continue
                gen_rule, gen_line, gen_body = generated_bins[dep_name]
                reported_bins.add(dep_name)
                m_out = re.search(r'\boutput_name\s*=\s*"([^"]+)"', gen_body)
                m_inst_out = re.search(
                    r'\boutputs\s*=\s*\[\s*"([^"]+)"', ibody, re.DOTALL
                )
                out_name = (
                    (m_out.group(1) if m_out else None)
                    or (m_inst_out.group(1) if m_inst_out else None)
                    or dep_name
                )
                if gen_rule == "go_binary":
                    out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}_/{dep_name}"
                else:
                    out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}"
                out_attr = (
                    f'output_name = "{out_name}" ' if out_name != dep_name else ""
                )
                msg = (
                    f"`{gn_rel}` keeps `install_host_tools(\"{iname}\")` above "
                    f"`{sentinel}` while `bazel2gn` generates "
                    f"`{gen_rule}(\"{dep_name}\")` below the sentinel (line "
                    f"{gen_line}). Host tools must be built in Bazel only and "
                    "brought into GN via `bazel_host_tool` instead of syncing "
                    "the binary with `bazel2gn`."
                )
                rem = (
                    f"In `{gn_rel}`, replace `install_host_tools(\"{iname}\")` "
                    "with `import(\"//build/bazel/bazel_host_tool.gni\")` and "
                    f"`if (is_host) {{ bazel_host_tool(\"{dep_name}\") {{ "
                    f'bazel_target = ":{dep_name}" '
                    f'bazel_output_path = "{out_path}" '
                    f"{out_attr}"
                    f"install_host_tool = true }} }}` above `{sentinel}`, and "
                    f'update any same-package group depending on `:{iname}` to '
                    f'depend on `:{dep_name}($host_toolchain)` (or keep a '
                    f'forwarding `group("{iname}")` if external callers '
                    f"reference `:{iname}`). In `{bz_rel}`, place "
                    "`# @bazel2gn:skip` on the line immediately above "
                    f"`{dep_name}`, remove any inner `# @bazel2gn:*` attribute "
                    f"comments inside `{dep_name}`, and re-run "
                    f"`fx bazel2gn -d {d}` so `{dep_name}` is no longer "
                    "generated below the sentinel."
                )
                _emit(findings, "dual_built_host_tool", "error", gn_rel, iline, msg, rem)

        # Check bazel_host_tool() above the sentinel duplicating a generated binary.
        for hname, (htmpl, hline, hbody) in gn_info["pre_targets"].items():
            if htmpl != "bazel_host_tool":
                continue
            candidates = [hname]
            m_bt = re.search(r'\bbazel_target\s*=\s*"([^"]+)"', hbody)
            if m_bt:
                bt_lbl = m_bt.group(1).strip()
                if bt_lbl.startswith(":"):
                    candidates.append(bt_lbl[1:])
                elif bt_lbl.startswith(f"//{d}:"):
                    candidates.append(bt_lbl[len(f"//{d}:") :])
            for dep_name in candidates:
                if dep_name in generated_bins and dep_name not in reported_bins:
                    gen_rule, gen_line, _ = generated_bins[dep_name]
                    reported_bins.add(dep_name)
                    msg = (
                        f"`{gn_rel}` defines `bazel_host_tool(\"{hname}\")` "
                        f"above `{sentinel}` (line {hline}) while "
                        f"`{gen_rule}(\"{dep_name}\")` is still generated "
                        f"below `{sentinel}` (line {gen_line})."
                    )
                    rem = (
                        f"In `{bz_rel}`, place `# @bazel2gn:skip` on the line "
                        f"immediately above `{dep_name}`, remove any inner "
                        f"`# @bazel2gn:*` attribute comments inside `{dep_name}`, "
                        f"and re-run `fx bazel2gn -d {d}` so `{dep_name}` is not "
                        f"generated below `{sentinel}` in `{gn_rel}`."
                    )
                    _emit(findings, "dual_built_host_tool", "error", gn_rel, gen_line, msg, rem)


check_dual_built_host_tools(candidate_dirs)


def check_helper_components_and_dead_selects(dirs):
    """Flags component and select() patterns reviewers keep rejecting in migrations.

    - test_component_not_test_root (error): a new `fx_test_component` that no `fx_package`
      lists in `test_components`. `fx_test_component` marks the component `is_test`, which
      `fx_package` only accepts for test roots, and bazel2gn never translates it, so a helper
      component (mock, fake, test server) declared with it has no GN copy and no Bazel user.
    - skipped_test_helper_component (error): an `fx_component` with `# @bazel2gn:skip` whose
      hand-written GN copy above the sentinel is a `fuchsia_test_component` without
      `test_type`. That GN template is only a `testonly` `fuchsia_component`, which bazel2gn
      emits from `fx_component(testonly = True)`, so the skip and the GN copy are unnecessary.
    - gcc_compiler_select (warning): a new `@rules_cc//cc/compiler:gcc` select() branch. Fuchsia
      has no GCC toolchain in Bazel, so the branch is dead code.
    """
    target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for pkg_dir in sorted(dirs):
        bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
        bazel_abs = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(bazel_abs):
            continue
        try:
            text = open(bazel_abs, encoding="utf-8").read()
            tree = ast.parse(text, filename=bazel_rel)
        except Exception:
            continue
        lines = text.splitlines()
        try:
            base_text = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_text = ""
        base_names = set(re.findall(r'^\s*name\s*=\s*"([^"]+)"', base_text, re.M))

        gn_rel = os.path.join(pkg_dir, "BUILD.gn")
        gn_pre = ""
        try:
            gn_pre = gn_hand_written_text(open(os.path.join(workdir, gn_rel), encoding="utf-8").read())
        except Exception:
            pass
        gn_pre_targets = {}
        for m in target_re.finditer(gn_pre):
            body_end = find_closing_brace(gn_pre, m.end() - 1)
            gn_pre_targets[m.group(2)] = (m.group(1), gn_pre[m.end() : body_end])

        test_component_refs = set()
        calls = []
        top_level_vars = {}
        for node in tree.body:
            if isinstance(node, ast.Assign):
                for tgt in node.targets:
                    if isinstance(tgt, ast.Name):
                        top_level_vars[tgt.id] = node.value

        def collect_label_names(expr, seen=()):
            # Follows top-level list variables (`test_components = _TESTS + [...]`).
            for sub in ast.walk(expr):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    test_component_refs.add(sub.value.rsplit(":", 1)[-1])
                elif isinstance(sub, ast.Name) and sub.id in top_level_vars and sub.id not in seen:
                    collect_label_names(top_level_vars[sub.id], seen + (sub.id,))

        for node in tree.body:
            if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
                continue
            call = node.value
            if not isinstance(call.func, ast.Name):
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            if "test_components" in kws:
                collect_label_names(kws["test_components"])
            name = kws.get("name")
            if isinstance(name, ast.Constant) and isinstance(name.value, str):
                calls.append((call.func.id, name.value, node.lineno, kws))

        for rule, name, lineno, kws in calls:
            ext_ref_args = ["-e", f"//{pkg_dir}:{name}"]
            if name == os.path.basename(pkg_dir):
                ext_ref_args += ["-e", f'"//{pkg_dir}"']
            if (
                rule == "fx_test_component"
                and name not in base_names
                and name not in test_component_refs
                and not git_lines(
                    ["grep", "-l", "--fixed-strings"] + ext_ref_args + ["--", "*BUILD.bazel"]
                )
            ):
                _emit(findings, "test_component_not_test_root", "error", bazel_rel, lineno, (
                        f"`fx_test_component(name = \"{name}\")` is not listed in the `test_components` of "
                        "any `fx_package`. `fx_test_component` marks a test root (`is_test`, only allowed "
                        "in `fx_package(test_components)`) and bazel2gn never translates it, so a helper "
                        "component packaged next to a test (mock service, fake, test server) gets no GN "
                        "copy and cannot be used from Bazel either."
                    ), (
                        (
                            f"'{name}' sets `test_type`, which `fx_component` cannot carry and "
                            "bazel2gn only emits for an `fx_test_component` listed in the "
                            "`test_components` of a test `fx_package` in the same BUILD.bazel. "
                            "If a Bazel test package uses it, list it there. Otherwise (its only "
                            "users are GN test packages) delete the Bazel target and its "
                            "Bazel-only manifest/binary helpers, and keep the GN "
                            f"`fuchsia_test_component` hand-written above `## BAZEL2GN SENTINEL` in "
                            f"'{gn_rel}' (no `# @bazel2gn:skip` twin); name the reason in the summary."
                        )
                        if "test_type" in kws
                        or "test_type" in gn_pre_targets.get(name, ("", ""))[1]
                        else (
                            f"Declare '{name}' with `fx_component(testonly = True, ...)` and "
                            "`visibility` from find_rdeps, delete its hand-written GN copy above "
                            f"`## BAZEL2GN SENTINEL` in '{gn_rel}', and run "
                            f"`fx bazel2gn -d {pkg_dir}`. A GN `fuchsia_test_component` without "
                            "`test_type` is just a `testonly` `fuchsia_component`, which is what "
                            "bazel2gn emits."
                        )
                    ))
            # Flag non-test fx_component targets referenced in fx_package(test_components = [...]).
            if rule == "fx_component" and name in test_component_refs:
                _emit(
                    findings,
                    "non_test_component_in_test_components",
                    "error",
                    bazel_rel,
                    lineno,
                    (
                        f"`fx_component(name = \"{name}\")` is listed in `test_components` "
                        "of an `fx_package`, which requires `fx_test_component` or "
                        "`fx_unittest_component`. Using `fx_component` fails both `fx bazel "
                        "build` (`Please use components for non-test components`) and `fx bazel2gn`."
                    ),
                    (
                        f"Change `fx_component(name = \"{name}\", ...)` to "
                        f"`fx_test_component(name = \"{name}\", ...)` in '{bazel_rel}', "
                        "add `\"fx_test_component\"` to "
                        "`load(\"//build/bazel/rules/components:fx_component.bzl\", ...)`, "
                        "omit redundant `testonly = True` on `fx_test_component`, do NOT add "
                        "`# @bazel2gn:skip` on the `fx_package`, and run "
                        f"`fx bazel2gn -d {pkg_dir}`."
                    ),
                )
            if rule == "fx_component" and has_skip_comment(lines, lineno):
                gn_copy = gn_pre_targets.get(name)
                if (
                    gn_copy
                    and gn_copy[0] == "fuchsia_test_component"
                    and not re.search(r"\btest_type\s*=", gn_copy[1])
                ):
                    _emit(findings, "skipped_test_helper_component", "error", bazel_rel, lineno, (
                            f"`fx_component(name = \"{name}\")` is skipped with `# @bazel2gn:skip` and kept as "
                            f"a hand-written `fuchsia_test_component(\"{name}\")` above the sentinel in "
                            f"'{gn_rel}'. Without `test_type`, `fuchsia_test_component` is a `testonly` "
                            "`fuchsia_component`, which bazel2gn emits from `fx_component(testonly = True)`."
                        ), (
                            f"Remove `# @bazel2gn:skip` (and its reason comment) from '{name}', make sure it "
                            "sets `testonly = True` and `visibility` from find_rdeps, delete "
                            f"`fuchsia_test_component(\"{name}\")` above the sentinel in '{gn_rel}', and run "
                            f"`fx bazel2gn -d {pkg_dir}`."
                        ))

        if "@rules_cc//cc/compiler:gcc" not in base_text:
            for idx, line in enumerate(lines, 1):
                if "@rules_cc//cc/compiler:gcc" in line and not line.lstrip().startswith("#"):
                    _emit(findings, "gcc_compiler_select", "warning", bazel_rel, idx, (
                            "`@rules_cc//cc/compiler:gcc` never matches in the Fuchsia Bazel build: there is "
                            "no GCC toolchain in Bazel (the toolchain's compiler is `__bazel_compiler__`), so "
                            "this select() branch is dead code. GN's GCC variants keep using the GN configs "
                            "above the sentinel."
                        ), (
                            "Drop the `@rules_cc//cc/compiler:gcc` branch and keep only the Clang flags. If "
                            "GN needs GCC-specific configs, keep them above the sentinel or name them with "
                            "`# @bazel2gn:raw_overwrite:` on the closing `],`."
                        ))
                    break


check_helper_components_and_dead_selects(candidate_dirs)


# Collect local target references and external shard package dependencies in a BUILD.bazel AST.
def _collect_bazel_component_graph(tree, pkg_dir):
    top_vars = {}
    calls = []
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for tgt in node.targets:
                if isinstance(tgt, ast.Name):
                    top_vars[tgt.id] = node.value
        elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            call = node.value
            if not isinstance(call.func, ast.Name):
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            name_node = kws.get("name")
            if isinstance(name_node, ast.Constant) and isinstance(name_node.value, str):
                calls.append((call.func.id, name_node.value, node.lineno, kws))

    def _expr_strings(expr, seen=()):
        vals = []
        if expr is None:
            return vals
        for sub in ast.walk(expr):
            if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                vals.append(sub.value)
            elif isinstance(sub, ast.Name) and sub.id in top_vars and sub.id not in seen:
                vals.extend(_expr_strings(top_vars[sub.id], seen + (sub.id,)))
        return vals

    def _local_target(raw):
        lbl = raw.strip()
        if lbl.startswith(":"):
            return lbl[1:] or None
        if pkg_dir and lbl.startswith(f"//{pkg_dir}:"):
            return lbl.split(":", 1)[1] or None
        if pkg_dir and lbl == f"//{pkg_dir}":
            return os.path.basename(pkg_dir) or None
        return None

    referrers_of = {}
    direct_deps_of = {}
    includes_pkgs_of = {}
    for _, tname, _, kws in calls:
        local_refs = set()
        for arg_name, expr in kws.items():
            if arg_name == "name":
                continue
            for val in _expr_strings(expr):
                loc = _local_target(val)
                if loc and loc != tname:
                    local_refs.add(loc)
                    referrers_of.setdefault(loc, set()).add(tname)
                if arg_name == "includes" and val.startswith("//"):
                    inc_pkg = val[2:].split(":", 1)[0].strip("/")
                    if inc_pkg and inc_pkg != pkg_dir:
                        includes_pkgs_of.setdefault(tname, set()).add(inc_pkg)
        direct_deps_of[tname] = local_refs
    return calls, referrers_of, direct_deps_of, includes_pkgs_of


# Trace local targets consumed by pre-sentinel GN test packages vs non-test packages.
def _collect_pre_sentinel_pkg_consumers(gn_pre_targets, pkg_dir):
    test_pkg_tmpls = GN_ALL_TEST_PACKAGE_TEMPLATES
    non_test_pkg_tmpls = {
        "fuchsia_package",
        "fuchsia_package_with_single_component",
    }

    edges = {
        name: _gn_body_local_refs(body, pkg_dir)
        for name, (_, body) in gn_pre_targets.items()
    }
    # Pre-sentinel `fuchsia_package` / `fuchsia_package_with_single_component` targets
    # are non-test packages (even with `testonly = true`, e.g. assembly-included test
    # realm packages wrapping `bazel2gn`-generated `fuchsia_component` targets below
    # the sentinel) unless they are subpackaged by a same-file test package or wrap a
    # pre-sentinel `fuchsia_test_component` / `fuchsia_unittest_component`.
    test_comp_tmpls = {"fuchsia_test_component", "fuchsia_unittest_component"}
    subpackaged_by_test_pkg = set()
    for pname, (ptmpl, _) in gn_pre_targets.items():
        if ptmpl in test_pkg_tmpls:
            subpackaged_by_test_pkg.update(edges.get(pname, ()))
    test_pkg_by_ref = {}
    non_test_refs = set()
    for pname, (ptmpl, pbody) in sorted(gn_pre_targets.items()):
        pclean = re.sub(r"#[^\n]*", "", pbody)
        wraps_test_comp = any(
            gn_pre_targets.get(ref, ("", ""))[0] in test_comp_tmpls
            for ref in edges.get(pname, ())
        )
        is_test_pkg = ptmpl in test_pkg_tmpls or (
            ptmpl in non_test_pkg_tmpls
            and bool(re.search(r"\btestonly\s*=\s*true\b", pclean))
            and (pname in subpackaged_by_test_pkg or wraps_test_comp)
        )
        is_non_test_pkg = ptmpl in non_test_pkg_tmpls and not is_test_pkg
        if not (is_test_pkg or is_non_test_pkg):
            continue
        queue = list(edges.get(pname, ()))
        visited = {pname}
        while queue:
            cur = queue.pop(0)
            if not cur or cur in visited:
                continue
            visited.add(cur)
            if is_test_pkg:
                if pname not in test_pkg_by_ref.setdefault(cur, []):
                    test_pkg_by_ref[cur].append(pname)
            else:
                non_test_refs.add(cur)
            if cur in edges:
                queue.extend(edges[cur])
    return test_pkg_by_ref, non_test_refs


# Check whether any build file outside `pkg_dir` (tracked or in `all_dirs`) references `tname`.
def _has_external_component_callers(pkg_dir, tname, all_dirs):
    if not pkg_dir:
        return False
    needles = [
        f'"//{pkg_dir}:{tname}"',
        f'"//{pkg_dir}:{tname}(',
        f"'//{pkg_dir}:{tname}'",
    ]
    if tname == os.path.basename(pkg_dir):
        needles.extend([f'"//{pkg_dir}"', f'"//{pkg_dir}(', f"'//{pkg_dir}'"])
    ext_args = []
    for needle in needles:
        ext_args.extend(["-e", needle])
    hits = git_lines(
        ["grep", "-l", "--fixed-strings"]
        + ext_args
        + ["--", "*BUILD.gn", "*.gni", "*BUILD.bazel", "*BUILD"]
    )
    if any(os.path.dirname(path) != pkg_dir for path in hits):
        return True
    for other_dir in all_dirs:
        if not other_dir or other_dir == pkg_dir:
            continue
        for fname in ("BUILD.bazel", "BUILD", "BUILD.gn"):
            cand = os.path.join(workdir, other_dir, fname)
            if not os.path.isfile(cand):
                continue
            try:
                content = open(cand, encoding="utf-8").read()
            except Exception:
                continue
            if any(needle in content for needle in needles):
                return True
    return False


# Identify modified out-of-directory BUILD.bazel files touched for `.shard.cml` visibility.
def _modified_external_shard_files(shard_pkgs):
    if not shard_pkgs:
        return []
    changed = set(git_lines(["diff", "--name-only", change_base]))
    changed.update(git_lines(["diff", "--name-only", "--cached"]))
    return sorted(
        f"{sp}/BUILD.bazel" for sp in shard_pkgs if f"{sp}/BUILD.bazel" in changed
    )


# Reject migrating package-private fx_component wrappers whose sole consumer is a
# GN-blocked test package kept above ## BAZEL2GN SENTINEL in the same BUILD.gn.
def check_premature_gn_test_package_components(dirs):
    target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for pkg_dir in sorted(dirs):
        bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
        gn_rel = os.path.join(pkg_dir, "BUILD.gn")
        bazel_abs = os.path.join(workdir, bazel_rel)
        gn_abs = os.path.join(workdir, gn_rel)
        if not (os.path.isfile(bazel_abs) and os.path.isfile(gn_abs)):
            continue
        # Skip if the GN test package itself has no blockers and is already flagged for migration.
        if any(
            f.get("category") == "gn_test_package_not_migrated" and f.get("file") == gn_rel
            for f in findings
        ):
            continue
        try:
            bazel_text = open(bazel_abs, encoding="utf-8").read()
            gn_full = open(gn_abs, encoding="utf-8").read()
            tree = ast.parse(bazel_text, filename=bazel_rel)
        except Exception:
            continue
        if "## BAZEL2GN SENTINEL" not in gn_full:
            continue
        lines = bazel_text.splitlines()
        try:
            base_bz = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_bz = ""
        base_names = set(re.findall(r'^\s*name\s*=\s*"([^"]+)"', base_bz, re.M))
        try:
            base_gn = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_gn = ""
        base_gn_tmpls = {m.group(2): m.group(1) for m in target_re.finditer(base_gn)}

        gn_pre = gn_hand_written_text(gn_full)
        gn_pre_targets = {}
        for match in target_re.finditer(gn_pre):
            body_end = find_closing_brace(gn_pre, match.end() - 1)
            gn_pre_targets[match.group(2)] = (match.group(1), gn_pre[match.end() : body_end])
        if not gn_pre_targets:
            continue

        test_pkg_by_ref, non_test_refs = _collect_pre_sentinel_pkg_consumers(
            gn_pre_targets, pkg_dir
        )
        if not test_pkg_by_ref:
            continue

        calls, referrers_of, direct_deps_of, includes_pkgs_of = _collect_bazel_component_graph(
            tree, pkg_dir
        )
        rules_by_name = {tname: rule for rule, tname, _, _ in calls}
        covered = set()

        for rule, name, lineno, _ in calls:
            if (
                rule != "fx_component"
                or name in base_names
                or has_skip_comment(lines, lineno)
                or referrers_of.get(name)
                or name in non_test_refs
                or name not in test_pkg_by_ref
                or _has_external_component_callers(pkg_dir, name, dirs)
            ):
                continue
            companions = sorted(
                dep
                for dep in direct_deps_of.get(name, set())
                if referrers_of.get(dep) == {name}
                and rules_by_name.get(dep) in ("fx_component_manifest", "fx_packaged_binary")
            )
            covered.add(name)
            covered.update(companions)
            shard_pkgs = set(includes_pkgs_of.get(name, set()))
            for comp_dep in companions:
                shard_pkgs.update(includes_pkgs_of.get(comp_dep, set()))
            ext_shard_files = _modified_external_shard_files(shard_pkgs)
            gn_pkgs = ", ".join(f"`{p}`" for p in test_pkg_by_ref[name])
            orig_tmpl = base_gn_tmpls.get(name, "fuchsia_test_component")
            drop_list = ", ".join(f"`:{t}`" for t in [name] + companions)
            shard_note = (
                " Revert out-of-directory shard visibility edits ("
                + ", ".join(f"'{p}'" for p in ext_shard_files)
                + ") if nothing else in this change needs them, keeping the CL self-contained."
                if ext_shard_files
                else ""
            )
            msg = (
                f"Package-private `fx_component(name = \"{name}\")` in '{bazel_rel}' has no "
                "Bazel consumers and no external callers; its sole consumer is the GN-only test "
                f"package {gn_pkgs} kept above `## BAZEL2GN SENTINEL` in '{gn_rel}'. Migrating "
                "unused Bazel component/manifest wrappers for a GN-blocked test package adds "
                "unreferenced Bazel targets and unnecessary cross-package `.shard.cml` "
                "visibility dependencies."
            )
            rem = (
                f"Keep `{orig_tmpl}(\"{name}\")` hand-written above `## BAZEL2GN SENTINEL` in "
                f"'{gn_rel}' alongside {gn_pkgs} (depending on the `bazel2gn`-generated test "
                f"binary below the sentinel), remove {drop_list} from '{bazel_rel}' while "
                "keeping convertible binary/library/test-binary targets (such as `rustc_test`) "
                f"in '{bazel_rel}', and re-run `fx bazel2gn -d {pkg_dir}`."
                + shard_note
            )
            _emit(findings, "premature_gn_test_package_component", "error", bazel_rel, lineno, msg, rem)

        for rule, name, lineno, _ in calls:
            if (
                rule not in ("fx_component_manifest", "fx_packaged_binary")
                or name in covered
                or name in base_names
                or has_skip_comment(lines, lineno)
                or referrers_of.get(name)
                or name in non_test_refs
                or _has_external_component_callers(pkg_dir, name, dirs)
            ):
                continue
            ext_shard_files = _modified_external_shard_files(includes_pkgs_of.get(name, set()))
            shard_note = (
                " Revert out-of-directory shard visibility edits ("
                + ", ".join(f"'{p}'" for p in ext_shard_files)
                + ") if nothing else in this change needs them."
                if ext_shard_files
                else ""
            )
            msg = (
                f"Unreferenced `{rule}(name = \"{name}\")` in '{bazel_rel}' has no Bazel "
                f"consumers and no external callers while '{gn_rel}' keeps its test package "
                "hand-written above `## BAZEL2GN SENTINEL`."
            )
            rem = (
                f"Remove `{rule}(name = \"{name}\")` from '{bazel_rel}' (keep the component "
                f"manifest on the pre-sentinel GN test component in '{gn_rel}') and re-run "
                f"`fx bazel2gn -d {pkg_dir}`."
                + shard_note
            )
            _emit(findings, "premature_gn_test_package_component", "error", bazel_rel, lineno, msg, rem)


check_premature_gn_test_package_components(candidate_dirs)


# Blank GN `#` comments outside double-quoted strings while preserving offsets and newlines.
def _blank_gn_comments(text):
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        ch = text[i]
        if in_str:
            if ch == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            in_str = ch != '"'
        elif ch == '"':
            in_str = True
        elif ch == "#":
            nl = text.find("\n", i)
            nl = n if nl < 0 else nl
            out.append(" " * (nl - i))
            i = nl
            continue
        out.append(ch)
        i += 1
    return "".join(out)


# Locate the matching `}` for `{` at `brace_idx` in comment-blanked GN text.
def _find_gn_closing_brace(code, brace_idx):
    depth, i, n, in_str = 0, brace_idx, len(code), False
    while i < n:
        ch = code[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            in_str = ch != '"'
        elif ch == '"':
            in_str = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return n


# Blank `template("...") { ... }` definitions while keeping line numbers intact.
def _blank_gn_templates(code):
    chars = list(code)
    for tm in re.finditer(r'\btemplate\(\s*"[^"]*"\s*\)\s*\{', code):
        end_idx = _find_gn_closing_brace(code, tm.end() - 1)
        for k in range(tm.start(), min(end_idx + 1, len(chars))):
            if chars[k] != "\n":
                chars[k] = " "
    return "".join(chars)


# Resolve a literal GN label in `pkg_dir/BUILD.gn` to its same-package target name.
def _resolve_same_pkg_gn_label(raw_lbl, pkg_dir):
    lbl = raw_lbl.strip()
    if not lbl or "$" in lbl or "(" in lbl:
        return None
    if lbl.startswith(":"):
        return lbl[1:] or None
    if pkg_dir and lbl.startswith(f"//{pkg_dir}:"):
        return lbl.split(":", 1)[1] or None
    if pkg_dir and lbl == f"//{pkg_dir}":
        return os.path.basename(pkg_dir) or None
    return None


# Extract same-package `get_target_outputs()` evaluations in a comment/template-blanked BUILD.gn.
def _collect_gn_get_target_outputs_refs(code, target_blocks, pkg_dir):
    refs = []
    seen = set()

    def _record(call_line, caller_name, caller_desc, raw_lbl):
        ref_name = _resolve_same_pkg_gn_label(raw_lbl, pkg_dir)
        if not ref_name or (call_line, ref_name) in seen:
            return
        seen.add((call_line, ref_name))
        refs.append((call_line, caller_name, caller_desc, ref_name))

    for gm in re.finditer(r'\bget_target_outputs\(\s*"([^"$]+)"\s*\)', code):
        ln = code.count("\n", 0, gm.start()) + 1
        _record(ln, "", "`get_target_outputs()`", gm.group(1))

    for tmpl, tname, _, bstart, bend in target_blocks:
        body = code[bstart:bend]
        if (
            tmpl in ("fuchsia_component", "fuchsia_test_component")
            or tmpl.startswith("fuchsia_structured_config_")
        ):
            for cm in re.finditer(r'\bcm_label\s*=\s*"([^"$]+)"', body):
                ln = code.count("\n", 0, bstart + cm.start()) + 1
                _record(ln, tname, f'`{tmpl}("{tname}")` (`cm_label`)', cm.group(1))
        elif tmpl == "fuchsia_test_package":
            for tcm in re.finditer(r"\btest_components\s*\+?=\s*\[([^\]]*)\]", body):
                for lm in re.finditer(r'"([^"$]+)"', tcm.group(1)):
                    ln = code.count("\n", 0, bstart + tcm.start(1) + lm.start()) + 1
                    _record(
                        ln,
                        tname,
                        f'`{tmpl}("{tname}")` (`test_components`)',
                        lm.group(1),
                    )
        elif tmpl == "fuchsia_test":
            for cpm in re.finditer(r'(?<![\w.])component\s*=\s*"([^"$]+)"', body):
                ln = code.count("\n", 0, bstart + cpm.start()) + 1
                _record(ln, tname, f'`{tmpl}("{tname}")` (`component`)', cpm.group(1))
    return refs


# Verify GN lexical ordering for `get_target_outputs()` callers (`cm_label`, `test_components`,
# `component`, and direct `get_target_outputs()`), catching forward references across
# `## BAZEL2GN SENTINEL` even when the package is outside the local `args.gn` build graph.
def check_gn_get_target_outputs_order(dirs):
    target_def_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    non_target_kw = {"template", "declare_args", "foreach", "forward_variables_from", "if"}
    base_ref = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

    for pkg_dir in sorted(dirs):
        gn_rel = os.path.join(pkg_dir, "BUILD.gn")
        gn_abs = os.path.join(workdir, gn_rel)
        if not os.path.isfile(gn_abs):
            continue
        try:
            raw_gn = open(gn_abs, encoding="utf-8").read()
        except OSError:
            continue

        sentinel_line = None
        for idx, raw_ln in enumerate(raw_gn.splitlines(), 1):
            if "BAZEL2GN SENTINEL" in raw_ln:
                sentinel_line = idx
                break

        code = _blank_gn_templates(_blank_gn_comments(raw_gn))
        has_foreach = bool(re.search(r"\bforeach\s*\(", code))
        first_def_line = {}
        target_blocks = []
        for dm in target_def_re.finditer(code):
            tmpl, tname = dm.group(1), dm.group(2)
            if tmpl in non_target_kw:
                continue
            start_line = code.count("\n", 0, dm.start()) + 1
            brace_idx = dm.end() - 1
            end_idx = _find_gn_closing_brace(code, brace_idx)
            first_def_line.setdefault(tname, start_line)
            target_blocks.append((tmpl, tname, start_line, brace_idx + 1, end_idx))

        refs = _collect_gn_get_target_outputs_refs(code, target_blocks, pkg_dir)
        if not refs:
            continue

        bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
        known_candidates = set()
        try:
            bz_txt = open(os.path.join(workdir, bazel_rel), encoding="utf-8").read()
            known_candidates.update(re.findall(r'^\s*name\s*=\s*"([^"]+)"', bz_txt, re.M))
        except OSError:
            pass
        try:
            base_gn = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{base_ref}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            base_code = _blank_gn_templates(_blank_gn_comments(base_gn))
            for bm in target_def_re.finditer(base_code):
                if bm.group(1) not in non_target_kw:
                    known_candidates.add(bm.group(2))
        except Exception:
            pass

        for call_line, caller_name, caller_desc, ref_name in refs:
            if ref_name in first_def_line and first_def_line[ref_name] > call_line:
                def_line = first_def_line[ref_name]
                across_sentinel = (
                    sentinel_line is not None and call_line < sentinel_line < def_line
                )
                consumer_lbl = caller_name or "the consumer target"
                if across_sentinel:
                    msg = (
                        f"In '{gn_rel}', {caller_desc} at line {call_line} (above "
                        f"`## BAZEL2GN SENTINEL`) references `:{ref_name}`, which is "
                        f"defined below `## BAZEL2GN SENTINEL` at line {def_line}. "
                        "GN's `get_target_outputs()` can only be used for targets "
                        "previously defined earlier in the current `BUILD.gn` file, so "
                        f"`gn gen` fails whenever '{gn_rel}' is loaded (including on CQ "
                        "builders whose graph includes this package even if local "
                        "`fx build` does not)."
                    )
                    rem = (
                        f"Migrate `{consumer_lbl}` to '{bazel_rel}' without "
                        "`# @bazel2gn:skip` (granting Bazel `visibility` on any "
                        "cross-package `fx_packaged_binary` or resource dependency if "
                        "needed), delete its hand-written definition above "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}', and re-run "
                        f"`fx bazel2gn -d {pkg_dir}` so `bazel2gn` emits `:{ref_name}` "
                        f"before `{consumer_lbl}` below the sentinel. Only if "
                        f"`{consumer_lbl}` uses GN-only features or unmigrated deps that "
                        "`bazel2gn` cannot translate, keep `:{ref_name}` hand-written "
                        f"above `{consumer_lbl}` (above `## BAZEL2GN SENTINEL`) in "
                        f"'{gn_rel}' and annotate `{ref_name}` in '{bazel_rel}' with "
                        "`# @bazel2gn:skip` and an explanatory comment."
                    )
                else:
                    msg = (
                        f"In '{gn_rel}', {caller_desc} at line {call_line} references "
                        f"`:{ref_name}` before its definition at line {def_line}. "
                        "GN's `get_target_outputs()` can only be used for targets "
                        "previously defined earlier in the current `BUILD.gn` file."
                    )
                    rem = (
                        f"Move the definition of `{ref_name}` above line {call_line} in "
                        f"'{gn_rel}' (or re-run `fx bazel2gn -d {pkg_dir}` if both targets "
                        "are generated below `## BAZEL2GN SENTINEL`)."
                    )
                _emit(findings, "gn_forward_get_target_outputs", "error", gn_rel, call_line, msg, rem)
            elif (
                ref_name not in first_def_line
                and not has_foreach
                and ref_name in known_candidates
            ):
                consumer_lbl = caller_name or "its consumer"
                msg = (
                    f"In '{gn_rel}', {caller_desc} at line {call_line} references "
                    f"`:{ref_name}`, which has no definition in '{gn_rel}'. GN's "
                    "`get_target_outputs()` requires `:{ref_name}` to be defined "
                    "earlier in the same `BUILD.gn` file."
                )
                rem = (
                    f"Either migrate both `{ref_name}` and `{consumer_lbl}` to "
                    f"'{bazel_rel}' without `# @bazel2gn:skip` and re-run "
                    f"`fx bazel2gn -d {pkg_dir}`, or define `{ref_name}` above line "
                    f"{call_line} in '{gn_rel}'."
                )
                _emit(findings, "gn_forward_get_target_outputs", "error", gn_rel, call_line, msg, rem)


check_gn_get_target_outputs_order(candidate_dirs)


# Waive verified review_diff false positives on skipped bridge and pre-existing targets
def _load_review_diff_findings():
    """Loads review_diff findings from PLANTER_REVIEW_DIFF_JSON or planter diff fallback."""
    if os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return []
    raw_path = os.environ.get("PLANTER_REVIEW_DIFF_JSON", "").strip()
    if raw_path and os.path.isfile(raw_path):
        try:
            with open(raw_path, encoding="utf-8") as rf:
                data = json.load(rf)
            return data if isinstance(data, list) else []
        except Exception:
            return []
    if "PLANTER_REVIEW_DIFF_JSON" in os.environ:
        return []
    # Resolve the checks helper directory from either PLANTER_PROJECT_DIR or SCRIPT_DIR
    cand_dirs = [
        os.path.join(os.environ.get("PLANTER_PROJECT_DIR", ""), "checks"),
        checks_script_dir,
    ]
    for cdir in cand_dirs:
        if cdir and os.path.isdir(cdir) and cdir not in sys.path:
            sys.path.insert(0, cdir)
    try:
        import rdeps_lib as _rl

        planter_bin = _rl.find_planter_bin()
    except Exception:
        planter_bin = os.environ.get("PLANTER_BIN", "").strip()
    if not planter_bin:
        return []
    sub_env = dict(
        os.environ,
        PLANTER_BIN=planter_bin,
        PLANTER_CHANGE_BASE=change_base,
        PLANTER_RESOLVED_CHANGE_BASE=change_base,
        PLANTER_SKIP_DIFF_COVERAGE="1",
    )
    for flag in ("--format=findings-json", "--check-findings"):
        try:
            out = subprocess.run(
                [planter_bin, "diff", "--local", flag, f"--workdir={workdir}"],
                cwd=workdir,
                env=sub_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=60,
            ).stdout
            start, end = out.find("["), out.rfind("]")
            if start >= 0 and end > start:
                parsed = json.loads(out[start : end + 1])
                if isinstance(parsed, list):
                    return parsed
        except Exception:
            continue
    return []


# Extract string literals from list expressions, shared list variables, concatenations, and select().
def _extract_bazel_str_expr(node, top_vars):
    if node is None:
        return []
    if isinstance(node, (ast.List, ast.Tuple)):
        return [
            e.value
            for e in node.elts
            if isinstance(e, ast.Constant) and isinstance(e.value, str)
        ]
    if isinstance(node, ast.Name) and node.id in top_vars:
        return list(top_vars[node.id])
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _extract_bazel_str_expr(node.left, top_vars) + _extract_bazel_str_expr(
            node.right, top_vars
        )
    if isinstance(node, ast.IfExp):
        return _extract_bazel_str_expr(node.body, top_vars) + _extract_bazel_str_expr(
            node.orelse, top_vars
        )
    if (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "select"
        and node.args
        and isinstance(node.args[0], ast.Dict)
    ):
        vals = []
        for v in node.args[0].values:
            vals.extend(_extract_bazel_str_expr(v, top_vars))
        return vals
    return []


def _parse_bazel_call_map(src_text):
    """Maps top-level BUILD.bazel target names to parsed rule metadata dicts."""
    if not src_text:
        return {}
    try:
        tree = ast.parse(src_text)
    except SyntaxError:
        return {}
    lines = src_text.splitlines()
    top_vars = {}
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1:
            tgt = stmt.targets[0]
            if isinstance(tgt, ast.Name):
                top_vars[tgt.id] = _extract_bazel_str_expr(stmt.value, top_vars)
    out = {}
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        fn = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
        )
        tname = None
        kw_names = set()
        kw_nodes = {}
        for kw in call.keywords:
            if kw.arg:
                kw_names.add(kw.arg)
                kw_nodes[kw.arg] = kw.value
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                tname = kw.value.value
        if not tname:
            continue
        idx = getattr(call, "lineno", 1) - 2
        has_skip = False
        raw_attr_lines = []
        while idx >= 0:
            s_ln = lines[idx].strip()
            if not s_ln:
                idx -= 1
                continue
            if s_ln.startswith("#"):
                if re.search(r"^#\s*@bazel2gn:skip\b", s_ln):
                    has_skip = True
                if re.search(r"#\s*@bazel2gn:raw_(?:attribute|overwrite):", s_ln):
                    raw_attr_lines.append(s_ln)
                idx -= 1
                continue
            break
        start_ln = max(1, getattr(call, "lineno", 1))
        end_ln = max(start_ln, getattr(call, "end_lineno", start_ln))
        call_src = "\n".join(lines[start_ln - 1 : end_ln])
        for inner_ln in lines[start_ln - 1 : end_ln]:
            s_in = inner_ln.strip()
            if re.search(r"#\s*@bazel2gn:raw_(?:attribute|overwrite):", s_in):
                raw_attr_lines.append(s_in)
        norm_slice = re.sub(r"\s+", " ", re.sub(r"#[^\n]*", "", call_src)).strip()

        def _str_list(node):
            return _extract_bazel_str_expr(node, top_vars)

        def _str_val(node):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                return node.value
            return ""

        out[tname] = {
            "rule": fn,
            "has_skip": has_skip,
            "raw_slice": call_src,
            "norm_slice": norm_slice,
            "kw_names": kw_names,
            "deps": _str_list(kw_nodes.get("deps"))
            + _str_list(kw_nodes.get("implementation_deps"))
            + _str_list(kw_nodes.get("subpackages")),
            "test_components": _str_list(kw_nodes.get("test_components")),
            "binary": _str_val(kw_nodes.get("binary")),
            "compiled_manifest": _str_val(kw_nodes.get("compiled_manifest")),
            "crate_features": _str_list(kw_nodes.get("crate_features"))
            + _str_list(kw_nodes.get("features")),
            "crate_name": _str_val(kw_nodes.get("crate_name")),
            "package": _str_val(kw_nodes.get("package")),
            "raw_attr_text": "\n".join(raw_attr_lines),
        }
    return out


_NON_RULE_BAZEL_STMTS = {"load", "package", "licenses", "visibility", "exports_files"}


# Check whether BUILD.bazel only declares non-convertible skip bridge stubs while BUILD.gn is untouched
def _is_bridge_stub_bazel_pkg(pkg_dir):
    """True when BUILD.gn is untouched and BUILD.bazel only defines non-compiled skip stubs."""
    gn_rel = f"{pkg_dir}/BUILD.gn" if pkg_dir else "BUILD.gn"
    bz_rel = f"{pkg_dir}/BUILD.bazel" if pkg_dir else "BUILD.bazel"
    bz_full = os.path.join(workdir, bz_rel)
    if gn_rel in changed or not os.path.isfile(bz_full):
        return False
    is_new_bz = not bool(_git_show_base_file(bz_rel))
    try:
        bz_src = open(bz_full, encoding="utf-8").read()
    except OSError:
        return False
    calls = _parse_bazel_call_map(bz_src)
    rule_entries = [
        (info["rule"], info["has_skip"], "srcs" in info["kw_names"])
        for info in calls.values()
        if info["rule"] not in _NON_RULE_BAZEL_STMTS
    ]
    if not rule_entries:
        return False
    if is_new_bz:
        # New header-only C/C++ stubs (no `srcs`) match the bridge stub that
        # cq_reachability prescribes and _check_unsynced_ancillary_and_hdrs exempts.
        return all(
            has_skip
            and (
                rule not in CONVERTIBLE_BAZEL_RULES
                or (
                    rule in ("cc_library", "fx_cc_library", "fx_cc_library_headers")
                    and not has_srcs
                )
            )
            for rule, has_skip, has_srcs in rule_entries
        )
    return all(
        has_skip and not (has_srcs and rule in CONVERTIBLE_BAZEL_RULES)
        for rule, has_skip, has_srcs in rule_entries
    )


def _has_only_non_bazel2gn_targets(pkg_dir):
    """True when BUILD.gn has no sentinel and BUILD.bazel only defines non-bazel2gn targets."""
    gn_full = os.path.join(workdir, pkg_dir, "BUILD.gn")
    bz_full = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    if not (os.path.isfile(gn_full) and os.path.isfile(bz_full)):
        return False
    try:
        gn_src = open(gn_full, encoding="utf-8").read()
        bz_src = open(bz_full, encoding="utf-8").read()
    except OSError:
        return False
    if "## BAZEL2GN SENTINEL" in gn_src:
        return False
    calls = _parse_bazel_call_map(bz_src)
    for info in calls.values():
        fn = info["rule"]
        has_skip = info["has_skip"]
        if fn in _NON_RULE_BAZEL_STMTS or fn == "test_suite":
            continue
        if (
            has_skip
            or fn in NO_GN_EQUIV_BAZEL_RULES
            or fn in UNSUPPORTED_TEST_BINARY_RULES
            or fn not in CONVERTIBLE_BAZEL_RULES
        ):
            continue
        return False
    return True


# Normalize a GN or Bazel label within `pkg_dir` to `(canonical_pkg, target_name)`
def _norm_pkg_target(raw_label, pkg_dir):
    lab = re.sub(r"\([^()]*\)$", "", (raw_label or "").strip().strip('"')).strip()
    if lab.startswith("@//"):
        lab = lab[1:]
    if not lab or lab.startswith("@"):
        return None
    if lab.startswith(":"):
        return (pkg_dir, lab[1:])
    if lab.startswith("//"):
        body = lab[2:]
        p, sep, t = body.partition(":")
        p = p.strip("/")
        t = t if sep else (p.rsplit("/", 1)[-1] if p else "")
        return (p, t)
    path_part, sep, t = lab.partition(":")
    joined = (
        os.path.normpath(os.path.join(pkg_dir, path_part))
        .replace(os.sep, "/")
        .strip("/")
    )
    if joined == ".":
        joined = ""
    t = t if sep else (joined.rsplit("/", 1)[-1] if joined else "")
    return (joined, t)


# Check whether a GN //third_party label maps to an external repository label
def _third_party_matches_ext_repo(gn_label, bz_label):
    if not gn_label.startswith("//third_party/") or not bz_label.startswith("@"):
        return False
    tp_body = gn_label[len("//third_party/") :]
    tp_path, tp_sep, tp_tgt = tp_body.partition(":")
    tp_last = tp_path.strip("/").rsplit("/", 1)[-1].lower().replace("-", "_")
    tp_tgt_norm = (tp_tgt if tp_sep else tp_last).lower().replace("-", "_")
    ext_body = bz_label[1:]
    ext_repo, ext_has_slashes, ext_rest = ext_body.partition("//")
    ext_repo_norm = ext_repo.lower().replace("-", "_")
    if ext_has_slashes:
        ext_pkg, ext_sep, ext_tgt = ext_rest.partition(":")
        fallback_tgt = (
            ext_pkg.strip("/").rsplit("/", 1)[-1]
            if ext_pkg.strip("/")
            else ext_repo
        )
        ext_tgt_norm = (ext_tgt if ext_sep else fallback_tgt).lower().replace("-", "_")
    else:
        ext_tgt_norm = ext_repo_norm
    if not tp_last or tp_last not in ext_repo_norm:
        return False
    return tp_tgt_norm == ext_tgt_norm or (not tp_sep and not ext_has_slashes)


# Return the cached `_parse_bazel_call_map` of `<clean_pkg>/BUILD.bazel` ({} when absent).
def _get_cached_bazel_call_map(clean_pkg, ext_pkg_cache):
    if clean_pkg not in ext_pkg_cache:
        bz_full = os.path.join(workdir, clean_pkg, "BUILD.bazel")
        try:
            bz_txt = open(bz_full, encoding="utf-8").read() if os.path.isfile(bz_full) else ""
        except OSError:
            bz_txt = ""
        ext_pkg_cache[clean_pkg] = _parse_bazel_call_map(bz_txt)
    return ext_pkg_cache[clean_pkg]


# Check whether two labels in the same package resolve to the same primary library target
def _same_pkg_target_equivalent(dep_pkg, gn_t, bz_t, ext_pkg_cache):
    clean_pkg = (dep_pkg or "").strip("/")
    if not clean_pkg or not gn_t or not bz_t:
        return False
    dep_map = _get_cached_bazel_call_map(clean_pkg, ext_pkg_cache)
    binfo = dep_map.get(bz_t)
    if not binfo:
        return False
    pkg_base = clean_pkg.rsplit("/", 1)[-1]
    gn_norm = gn_t.lower().replace("-", "_")
    base_norm = pkg_base.lower().replace("-", "_")
    crate_norm = (binfo.get("crate_name") or bz_t).lower().replace("-", "_")
    if gn_norm in (crate_norm, base_norm, "lib") and crate_norm in (gn_norm, base_norm):
        return True
    lib_targets = [
        name
        for name, info in dep_map.items()
        if info["rule"] in ("rustc_library", "cc_library", "fx_cc_library", "source_set")
    ]
    return len(lib_targets) == 1 and lib_targets[0] == bz_t and gn_norm in (base_norm, "lib")


# Collect normalized (pkg, target) tuples declared on a target and its companion fx_package.
def _collect_companion_dep_norms(tinfo, tname, pkg_dir, cur_map):
    norms = set()
    cand_infos = [tinfo]
    comp_pkg = (tinfo.get("package") or "").lstrip(":")
    if comp_pkg and comp_pkg in cur_map and cur_map[comp_pkg] not in cand_infos:
        cand_infos.append(cur_map[comp_pkg])
    default_pkg = f"{tname}_package"
    if default_pkg in cur_map and cur_map[default_pkg] not in cand_infos:
        cand_infos.append(cur_map[default_pkg])
    for info in cand_infos:
        for dep in info.get("deps", ()):
            dn = _norm_pkg_target(dep, pkg_dir)
            if dn:
                norms.add(dn)
            if dep.startswith(":"):
                pb_info = cur_map.get(dep[1:])
                if (
                    pb_info
                    and pb_info["rule"] == "fx_packaged_binary"
                    and pb_info["binary"]
                ):
                    bn = _norm_pkg_target(pb_info["binary"], pkg_dir)
                    if bn:
                        norms.add(bn)
    return norms


# Check whether a package's BUILD.gn is synced via BAZEL2GN SENTINEL for Go strict-dep parity
def _is_sentinel_synced_go_target(pkg_dir, tinfo):
    if tinfo.get("rule") not in ("go_library", "go_binary", "go_test", "host_go_test"):
        return False
    if tinfo.get("has_skip"):
        return False
    gn_full = (
        os.path.join(workdir, pkg_dir, "BUILD.gn")
        if pkg_dir
        else os.path.join(workdir, "BUILD.gn")
    )
    if not os.path.isfile(gn_full):
        return False
    try:
        gn_txt = open(gn_full, encoding="utf-8").read()
    except OSError:
        return False
    return "## BAZEL2GN SENTINEL" in gn_txt


NON_RUST_BAZEL_RULES_FOR_DEP_WAIVER = {
    "cc_binary",
    "cc_library",
    "cc_test",
    "fx_cc_binary",
    "fx_cc_library",
    "fx_cc_library_headers",
    "fx_cc_test",
    "fx_cc_test_binary",
    "fx_component",
    "fx_package",
    "fx_test",
    "fx_test_component",
    "fx_unittest_component",
    "fx_unittest_package",
}


# Check whether a GN dependency label resolves to a Rust library/proc-macro target
# that non-Rust C/C++ or component/package rules in Bazel cannot depend on.
def _is_cross_lang_rust_crate_dep_for_non_rust_target(tinfo, g_norm, ext_pkg_cache):
    if not g_norm or tinfo.get("rule") not in NON_RUST_BAZEL_RULES_FOR_DEP_WAIVER:
        return False
    dep_pkg, dep_t = g_norm
    if not dep_pkg or not dep_t:
        return False
    dep_map = _get_cached_bazel_call_map(dep_pkg, ext_pkg_cache)
    for cand_t in (dep_t, dep_pkg.rsplit("/", 1)[-1]):
        binfo = dep_map.get(cand_t)
        if binfo and binfo.get("rule") in (
            "rustc_library",
            "rust_library",
            "rustc_proc_macro",
            "rust_proc_macro",
        ):
            return True
    gn_full = os.path.join(workdir, dep_pkg, "BUILD.gn")
    if os.path.isfile(gn_full):
        try:
            gn_txt = open(gn_full, encoding="utf-8").read()
        except OSError:
            gn_txt = ""
        if re.search(
            r'\b(?:rustc_library|rustc_proc_macro)\(\s*"'
            + re.escape(dep_t)
            + r'"\s*\)',
            gn_txt,
        ):
            return True
    return False


# Verify canonical GN-to-Bazel dependency label equivalences for dep_alignment
def _verify_dep_alignment_waiver(tname, pkg_dir, msg, cur_map, ext_pkg_cache):
    tinfo = cur_map.get(tname)
    if not tinfo:
        return ""
    gn_m = re.search(r"unmatched in GN:\s*\[([^\]]*)\]", msg)
    bz_m = re.search(r"unmatched in Bazel:\s*\[([^\]]*)\]", msg)
    unmatched_gn = (
        [x.strip() for x in gn_m.group(1).split(",") if x.strip()] if gn_m else []
    )
    unmatched_bz = (
        [x.strip() for x in bz_m.group(1).split(",") if x.strip()] if bz_m else []
    )
    if not unmatched_gn and not unmatched_bz:
        return ""

    companion_dep_norms = _collect_companion_dep_norms(tinfo, tname, pkg_dir, cur_map)
    is_synced_go = _is_sentinel_synced_go_target(pkg_dir, tinfo)

    raw_attr = tinfo["raw_attr_text"]
    matched_bz = set()
    for g in unmatched_gn:
        g_norm = _norm_pkg_target(g, pkg_dir)
        paired = False
        for idx, b in enumerate(unmatched_bz):
            b_norm = _norm_pkg_target(b, pkg_dir)
            same_pkg_equiv = (
                g_norm
                and b_norm
                and g_norm[0] == b_norm[0]
                and _same_pkg_target_equivalent(
                    g_norm[0], g_norm[1], b_norm[1], ext_pkg_cache
                )
            )
            if (
                (g_norm and b_norm and g_norm == b_norm)
                or same_pkg_equiv
                or _third_party_matches_ext_repo(g, b)
            ):
                matched_bz.add(idx)
                paired = True
        if paired:
            continue
        if g in raw_attr:
            continue
        if g_norm and f"//{g_norm[0]}:{g_norm[1]}" in raw_attr:
            continue
        if g_norm and g_norm in companion_dep_norms:
            continue
        if is_synced_go:
            continue
        if _is_cross_lang_rust_crate_dep_for_non_rust_target(
            tinfo, g_norm, ext_pkg_cache
        ):
            continue
        return ""

    if len(matched_bz) != len(unmatched_bz) and not is_synced_go:
        return ""

    return (
        f"Verified canonical GN-to-Bazel dependency mapping on target '{tname}': "
        "every unmatched label resolves identically across relative-to-canonical "
        "paths, same-package renamed library targets, third-party external repository "
        "labels, strict direct Go imports synced via `fx bazel2gn`, "
        "`# @bazel2gn:raw_attribute:` / `# @bazel2gn:raw_overwrite:` annotations, "
        "or `fx_packaged_binary` wrappers."
    )


# Check whether GN manifest attributes (restricted_features, component_name, manifest)
# are carried on the companion fx_component_manifest / fx_compiled_manifest target.
def _verify_manifest_companion_attr_waiver(tname, attr_name, cur_map):
    tinfo = cur_map.get(tname)
    if not tinfo:
        return ""
    cm_ref = tinfo.get("compiled_manifest", "")
    if cm_ref.startswith(":"):
        minfo = cur_map.get(cm_ref[1:])
        if minfo and (
            attr_name in minfo["kw_names"] or attr_name == "component_name"
        ):
            return (
                f"Verified `{attr_name}` parity for target '{tname}': "
                f"declared on companion manifest target `:{cm_ref[1:]}`."
            )
    for mname, minfo in cur_map.items():
        if minfo["rule"] in ("fx_component_manifest", "fx_compiled_manifest") and (
            attr_name in minfo["kw_names"] or attr_name == "component_name"
        ):
            return (
                f"Verified `{attr_name}` parity for target '{tname}': "
                f"declared on manifest target `:{mname}` in BUILD.bazel."
            )
    return ""


# Map GN target attribute names to their Bazel rule/macro keyword equivalents.
_GN_TO_BAZEL_ATTR_EQUIVS = {
    "cflags": ("copts",),
    "cflags_c": ("conlyopts",),
    "cflags_cc": ("cxxopts",),
    "configs": ("build_flags", "copts", "rustc_flags"),
    "crate_root": ("crate_root",),
    "data_deps": ("data", "test_data"),
    "defines": ("defines", "local_defines"),
    "features": ("crate_features", "features"),
    "inputs": (
        "additional_linker_inputs",
        "srcs",
        "compile_data",
        "data",
        "test_data",
        "tools",
    ),
    "ldflags": ("linkopts",),
    "libs": ("linkopts",),
    "output_name": ("gn_output_name", "crate_name", "binary_name", "package_name"),
    "outputs": ("outs", "out"),
    "public": ("hdrs",),
    "rustenv": ("rustc_env",),
    "rustflags": ("rustc_flags",),
    "script": ("cmd", "tools", "tool"),
    "source_root": ("crate_root", "main"),
    "sources": ("srcs", "hdrs"),
}


# Standard Fuchsia GN-to-Bazel rule mappings flagged as unusual by review_diff.
_VALID_GN_TO_BAZEL_RULE_MAPPINGS = {
    ("cc_test_executable", "fx_cc_test_binary"),
    ("cm", "fx_compiled_manifest"),
    ("cm", "fx_component_manifest"),
    ("executable", "fx_cc_test_binary"),
    ("fuchsia_component", "fx_test_component"),
    ("fuchsia_package_with_single_component", "fx_package"),
    ("fuchsia_test_component", "fx_component"),
    ("fuchsia_test_package", "fx_package"),
    ("rustc_test", "host_rustc_test"),
    ("test", "fx_cc_test_binary"),
    ("test", "host_rustc_test"),
    ("test", "rustc_test"),
}


# Verify fx_package max_severity/min_severity, unusual rule mappings, @bazel2gn raw
# attributes/overwrites, or companion manifest attrs.
def _verify_attr_alignment_waiver(tname, msg, cur_map):
    tinfo = cur_map.get(tname)
    if not tinfo:
        return ""
    m_unusual_rule = re.search(
        r"\(Rule & Directives\):\s*Unusual rule mapping from ([A-Za-z_]\w*) to ([A-Za-z_]\w*)",
        msg,
    )
    if m_unusual_rule:
        gn_rule, bz_rule = m_unusual_rule.group(1), m_unusual_rule.group(2)
        if (
            tinfo["rule"] == bz_rule
            and (gn_rule, bz_rule) in _VALID_GN_TO_BAZEL_RULE_MAPPINGS
        ):
            return (
                f"Verified standard Fuchsia GN-to-Bazel rule mapping on target "
                f"'{tname}': GN `{gn_rule}` maps to Bazel `{bz_rule}`."
            )
    m_raw_attr = re.search(
        r"\(Target Attributes\):\s*([A-Za-z_]\w*)\s+in GN not carried over to Bazel",
        msg,
    )
    if m_raw_attr:
        attr_nm = m_raw_attr.group(1)
        raw_txt = tinfo.get("raw_attr_text", "")
        if re.search(r"\b" + re.escape(attr_nm) + r"\s*(?:\+?=|-=)", raw_txt):
            return (
                f"Verified `{attr_nm}` parity on target '{tname}': preserved in "
                "`BUILD.gn` via `# @bazel2gn:raw_attribute:` / `# @bazel2gn:raw_overwrite:`."
            )
        equiv_kws = _GN_TO_BAZEL_ATTR_EQUIVS.get(attr_nm, (attr_nm,))
        matched_kw = next((k for k in equiv_kws if k in tinfo["kw_names"]), "")
        if matched_kw:
            return (
                f"Verified `{attr_nm}` parity on target '{tname}': mapped to "
                f"`{matched_kw}` in `BUILD.bazel`."
            )
        if attr_nm == "testonly" and tinfo["rule"] in (
            "fx_cc_test_binary",
            "fx_cc_test",
            "cc_test",
            "rustc_test",
            "host_rustc_test",
            "go_test",
            "host_go_test",
            "fx_test_component",
            "fx_unittest_component",
            "fx_unittest_package",
            "fx_test",
            "test_suite",
        ):
            return (
                f"Verified `testonly` parity on `{tinfo['rule']}(name = \"{tname}\")`: "
                f"`{tinfo['rule']}` is implicitly `testonly` in both Bazel and `bazel2gn`."
            )
    if "restricted_features" in msg:
        return _verify_manifest_companion_attr_waiver(
            tname, "restricted_features", cur_map
        )
    for m_attr in ("component_name", "manifest"):
        if m_attr in msg and tinfo["rule"] in ("fx_component", "fx_test_component"):
            return _verify_manifest_companion_attr_waiver(tname, m_attr, cur_map)
    if tinfo["rule"] != "fx_package":
        return ""
    severities = [s for s in ("max_severity", "min_severity") if s in msg]
    if not severities:
        return ""
    for info in cur_map.values():
        if info["rule"] != "fx_test" or info["package"] not in (f":{tname}", tname):
            continue
        bazel_kws = [s.replace("_severity", "_log_severity") for s in severities]
        if all(kw in info["kw_names"] for kw in bazel_kws):
            return (
                f"Verified `{'`/`'.join(severities)}` parity on "
                f"`fx_package(name = \"{tname}\")`: Bazel sets "
                f"`{'`/`'.join(bazel_kws)}` on the paired `fx_test` target."
            )
    return ""


# Verify that a dropped comment from a removed BUILD.gn target was an obsolete retention note
def _verify_dropped_comment_waiver(pkg_dir, msg, gn_blob_cache):
    clean_pkg = (pkg_dir or "").strip("/")
    gn_rel = f"{clean_pkg}/BUILD.gn" if clean_pkg else "BUILD.gn"
    bz_rel = f"{clean_pkg}/BUILD.bazel" if clean_pkg else "BUILD.bazel"
    # A surviving BUILD.gn (e.g. bazel2gn output below the sentinel) does not make a
    # GN-retention note valid again: waive only when the target moved into BUILD.bazel
    # and the note no longer appears anywhere in the current BUILD.gn.
    cur_gn_text = None
    if os.path.isfile(os.path.join(workdir, gn_rel)):
        try:
            cur_gn_text = open(os.path.join(workdir, gn_rel), encoding="utf-8").read()
            bz_text = open(os.path.join(workdir, bz_rel), encoding="utf-8").read()
        except OSError:
            return ""
    m = re.match(r'^Target "([^"]+)" dropped comment from BUILD\.gn:\s*(.+)$', msg)
    if not m:
        return ""
    tname, dropped_line = m.group(1), m.group(2).strip()
    if cur_gn_text is not None:
        if dropped_line in cur_gn_text:
            return ""
        if not re.search(r'\bname\s*=\s*"' + re.escape(tname) + r'"', bz_text):
            return ""
    if gn_rel not in gn_blob_cache:
        gn_blob_cache[gn_rel] = _git_show_base_file(gn_rel)
    base_gn = gn_blob_cache[gn_rel]
    if not base_gn:
        return ""
    blocks = []
    cur_block = []
    for raw_ln in base_gn.splitlines():
        s = raw_ln.strip()
        if s.startswith("#") and not s.startswith("## BAZEL2GN SENTINEL"):
            cur_block.append(s)
        else:
            if cur_block:
                blocks.append(cur_block)
                cur_block = []
    if cur_block:
        blocks.append(cur_block)
    obsolete_note_re = re.compile(
        r"\bretained\s+in\s+gn\b|\bgn-only\b|\bnot\s+(?:yet\s+)?in\s+bazel\b"
        r"|\bdual-build\b|\bbazel2gn\b",
        re.IGNORECASE,
    )
    for blk in blocks:
        if any(dropped_line == ln or dropped_line in ln for ln in blk):
            joined = " ".join(blk)
            if obsolete_note_re.search(joined):
                return (
                    f"Verified obsolete GN-retention comment on target '{tname}': "
                    + (
                        f"'{gn_rel}' has been deleted after completing Bazel migration."
                        if cur_gn_text is None
                        else f"the target now lives in '{bz_rel}' and the note is gone "
                        f"from '{gn_rel}'."
                    )
                )
    return ""


# Verify C++ test binary, test package, and test component template parity waivers
def _verify_template_parity_waiver(tname, pkg_dir, msg, cur_map):
    tinfo = cur_map.get(tname)
    if not tinfo:
        return ""
    if (
        tinfo["rule"] == "fx_cc_test_binary"
        and "cc_test_executable" in msg
        and ("was test(" in msg or "was executable(" in msg)
    ):
        return (
            f"Verified `fx_cc_test_binary(name = \"{tname}\")`: C++ test binaries "
            "migrate to `fx_cc_test_binary` in Bazel, which `bazel2gn` emits as "
            "`cc_test_executable` below `## BAZEL2GN SENTINEL`."
        )
    if (
        tinfo["rule"] == "fx_cc_binary"
        and "testonly" in tinfo["kw_names"]
        and "//build/config/lto:no-fat-lto-objects-linking"
        in tinfo.get("raw_slice", "")
        and "generated executable(" in msg
        and "was cc_test_executable(" in msg
    ):
        return (
            f"Verified `fx_cc_binary(name = \"{tname}\", testonly = True)`: "
            "preserves `//build/config/lto:no-fat-lto-objects-linking` and "
            "`bazel2gn` emits `executable` below `## BAZEL2GN SENTINEL`."
        )
    if (
        tinfo["rule"] in ("fx_package", "fx_unittest_package")
        and "generated fuchsia_test_package(" in msg
        and "was fuchsia_unittest_package(" in msg
    ):
        return (
            f"Verified `{tinfo['rule']}(name = \"{tname}\")`: `bazel2gn` emits "
            "`fuchsia_test_package` below `## BAZEL2GN SENTINEL`."
        )
    if "fuchsia_test_component" not in msg or (
        "fuchsia_component" not in msg
        and "fuchsia_unittest_component" not in msg
    ):
        return ""
    clean_pkg = (pkg_dir or "").strip("/")
    cand_refs = {tname, f":{tname}"}
    if clean_pkg:
        cand_refs.add(f"//{clean_pkg}:{tname}")
    in_test_components = any(
        info["rule"] == "fx_package" and (cand_refs & set(info["test_components"]))
        for info in cur_map.values()
    )
    if tinfo["rule"] == "fx_test_component" and (
        in_test_components or "was fuchsia_unittest_component(" in msg
    ):
        return (
            f"Verified `fx_test_component(name = \"{tname}\")`: test root "
            "components use `fx_test_component` in Bazel, which `bazel2gn` "
            "emits as `fuchsia_test_component`."
        )
    if (
        tinfo["rule"] == "fx_component"
        and "testonly" in tinfo["kw_names"]
        and not in_test_components
    ):
        return (
            f"Verified `fx_component(name = \"{tname}\", testonly = True)`: test "
            "helper components not listed in `fx_package.test_components` must use "
            "`fx_component(testonly = True)`, which `bazel2gn` emits as "
            "`fuchsia_component`."
        )
    return ""


# Verify unit_test_parity waiver when omitted GN test_deps are non-crate GN targets.
def _verify_non_crate_gn_test_deps_waiver(clean_pkg, f_rel, tname, msg):
    td_m = re.search(r"GN test_deps\s*\[([^\]]+)\]", msg)
    if not td_m:
        return ""
    raw_labels = [
        item.strip().strip("'\"")
        for item in td_m.group(1).split(",")
        if item.strip().strip("'\"")
    ]
    if not raw_labels:
        return ""
    for raw_lbl in raw_labels:
        norm = _norm_pkg_target(raw_lbl, clean_pkg)
        if not norm or not norm[0] or not norm[1]:
            return ""
        dep_pkg, dep_name = norm
        dep_gn_rel = f"{dep_pkg}/BUILD.gn"
        dep_gn_full = os.path.join(workdir, dep_gn_rel)
        dep_gn_txt = ""
        if os.path.isfile(dep_gn_full):
            try:
                dep_gn_txt = open(dep_gn_full, encoding="utf-8").read()
            except OSError:
                dep_gn_txt = ""
        if not dep_gn_txt:
            dep_gn_txt = _git_show_base_file(dep_gn_rel)
        pat = (
            r"\b(?:fuchsia_package|fuchsia_package_with_single_component|"
            r"fuchsia_component|fuchsia_test_component|fuchsia_component_manifest|"
            r"fuchsia_structured_config_values|resource|resource_group|"
            r"resource_tree|group)\s*\(\s*\""
            + re.escape(dep_name)
            + r"\"\s*\)"
        )
        if not re.search(pat, dep_gn_txt):
            return ""
    return (
        f"Verified unit test target '{tname}' in '{f_rel}': omitted GN "
        "`test_deps` entries are non-library GN package/component targets "
        "rather than library crates accepted by Bazel `deps`, and the unit "
        "test binary is packaged in `BUILD.bazel`."
    )


# Verify unit_test_parity waiver when a unit test binary is packaged in GN or cross-package Bazel.
def _verify_unit_test_parity_waiver(pkg_dir, f_rel, msg, cur_map):
    clean_pkg = pkg_dir.strip("/")
    gn_path = os.path.join(workdir, clean_pkg, "BUILD.gn")
    gn_txt = ""
    if os.path.isfile(gn_path):
        try:
            gn_txt = open(gn_path, encoding="utf-8").read()
        except OSError:
            gn_txt = ""
    has_sentinel = "## BAZEL2GN SENTINEL" in gn_txt
    quoted = re.findall(r'"([^"]+)"', msg)
    test_cands = [
        q
        for q in quoted
        if q in cur_map
        and cur_map[q]["rule"]
        in ("rustc_test", "host_rustc_test", "cc_test", "fx_cc_test")
        and not cur_map[q]["has_skip"]
    ]
    if not test_cands:
        return ""
    pkg_re = re.compile(
        r"\b(?:fuchsia_unittest_package|fuchsia_test_package|"
        r"fuchsia_test_component|fuchsia_unittest_component)\s*\("
    )
    bz_pkg_re = re.compile(
        r"\b(?:fx_unittest_package|fx_packaged_binary|"
        r"fx_test_component|fx_unittest_component)\s*\("
    )
    cross_dirs = _collect_cross_pkg_dirs(clean_pkg, include_ancestors=True)
    cf_m = re.search(r"does not enable crate_features\s*\[([^\]]+)\]", msg)
    for tname in test_cands:
        if cf_m:
            req_feats = [
                x.strip().strip("'\"")
                for x in cf_m.group(1).split(",")
                if x.strip().strip("'\"")
            ]
            t_feats = set(cur_map[tname].get("crate_features", ()))
            if req_feats and set(req_feats) <= t_feats:
                return (
                    f"Verified `crate_features` parity on unit test '{tname}' in '{f_rel}': "
                    f"target sets `crate_features` covering `{', '.join(req_feats)}`."
                )
        if has_sentinel:
            pre_txt = gn_txt.split("## BAZEL2GN SENTINEL", 1)[0]
            if pkg_re.search(pre_txt) and re.search(
                r'"(?::|//' + re.escape(clean_pkg) + r":)" + re.escape(tname) + r'"',
                pre_txt,
            ):
                return (
                    f"Verified dual-built unit test binary '{tname}' in '{f_rel}': target is "
                    f"synced below `## BAZEL2GN SENTINEL` in '{clean_pkg}/BUILD.gn' and "
                    "packaged by an active GN test package."
                )
        lbl_pat = re.compile(
            r'"//' + re.escape(clean_pkg) + r":" + re.escape(tname) + r'"'
        )
        for cdir in cross_dirs:
            c_bz = os.path.join(workdir, cdir, "BUILD.bazel")
            if os.path.isfile(c_bz):
                try:
                    c_bz_txt = open(c_bz, encoding="utf-8").read()
                except OSError:
                    c_bz_txt = ""
                if bz_pkg_re.search(c_bz_txt) and lbl_pat.search(c_bz_txt):
                    return (
                        f"Verified unit test binary '{tname}' in '{f_rel}': target is "
                        f"packaged across package boundaries by '{cdir}/BUILD.bazel'."
                    )
            if not has_sentinel:
                continue
            c_gn = os.path.join(workdir, cdir, "BUILD.gn")
            if not os.path.isfile(c_gn):
                continue
            try:
                c_txt = open(c_gn, encoding="utf-8").read()
            except OSError:
                continue
            if pkg_re.search(c_txt) and lbl_pat.search(c_txt):
                return (
                    f"Verified dual-built unit test binary '{tname}' in '{f_rel}': target "
                    f"is synced below `## BAZEL2GN SENTINEL` in '{clean_pkg}/BUILD.gn' and "
                    f"packaged across package boundaries by '{cdir}/BUILD.gn'."
                )
        non_crate_reason = _verify_non_crate_gn_test_deps_waiver(
            clean_pkg, f_rel, tname, msg
        )
        if non_crate_reason:
            return non_crate_reason
    return ""


def _waiver_reason_for_review_diff(df, bz_cache, ext_pkg_cache, gn_blob_cache):
    """Returns an explanatory waiver string if `df` is a verified review_diff false positive."""
    cat = df.get("category", "")
    f_rel = (df.get("file", "") or "").strip("/")
    msg = df.get("message", "") or ""
    pkg_dir = os.path.dirname(f_rel).strip("/")

    # Parse (and cache) the current and change-base call maps of BUILD.bazel `f_rel`.
    def _cur_and_base_maps():
        if f_rel not in bz_cache:
            cur_full = os.path.join(workdir, f_rel)
            try:
                cur_txt = open(cur_full, encoding="utf-8").read()
            except OSError:
                cur_txt = ""
            base_txt = _git_show_base_file(f_rel)
            bz_cache[f_rel] = (
                _parse_bazel_call_map(cur_txt),
                _parse_bazel_call_map(base_txt),
            )
        return bz_cache[f_rel]

    if cat == "dropped_comment" and os.path.basename(f_rel) == "BUILD.bazel":
        return _verify_dropped_comment_waiver(pkg_dir, msg, gn_blob_cache)

    if cat == "unit_test_parity" and os.path.basename(f_rel) == "BUILD.bazel":
        cur_map, _ = _cur_and_base_maps()
        return _verify_unit_test_parity_waiver(pkg_dir, f_rel, msg, cur_map)

    if cat == "gn_unmodified" and os.path.basename(f_rel) == "BUILD.gn":
        if _is_bridge_stub_bazel_pkg(pkg_dir):
            return (
                f"Verified bridge-stub package '{pkg_dir}': BUILD.bazel only defines "
                "`# @bazel2gn:skip` bridge stubs while BUILD.gn intentionally remains untouched."
            )
        if _has_only_non_bazel2gn_targets(pkg_dir):
            return (
                f"Verified non-bazel2gn package '{pkg_dir}': BUILD.gn retains GN-only "
                "targets and BUILD.bazel defines only non-bazel2gn rules, so omitting "
                "`## BAZEL2GN SENTINEL` is required."
            )
        return ""

    if cat == "sentinel_drift" and os.path.basename(f_rel) == "BUILD.gn":
        if _has_only_non_bazel2gn_targets(pkg_dir):
            return (
                f"Verified GN-only package '{pkg_dir}': BUILD.gn retains only GN-only targets "
                "and BUILD.bazel defines only non-bazel2gn targets, so omitting "
                "`## BAZEL2GN SENTINEL` is required."
            )
        return ""

    if (
        cat
        in (
            "missing_sources",
            "dep_alignment",
            "attribute_alignment",
            "template_parity",
        )
        and os.path.basename(f_rel) == "BUILD.bazel"
    ):
        tm = re.match(r'^Target "([^"]+)"', msg)
        if not tm:
            return ""
        tname = tm.group(1)
        cur_map, base_map = _cur_and_base_maps()
        cur_info = cur_map.get(tname)
        base_info = base_map.get(tname)
        if (
            cur_info
            and base_info
            and cur_info["norm_slice"] == base_info["norm_slice"]
        ):
            return (
                f"Verified untouched pre-existing target '{tname}' in '{f_rel}': target "
                "definition is unchanged relative to the pre-migration change base."
            )
        # Evaluate semantic category-specific waivers before generic skip-stub fallback
        if cat == "missing_sources":
            if cur_info and bool({"srcs", "hdrs"} & set(cur_info.get("kw_names", ()))):
                ms_m = re.match(
                    r'^Target "[^"]+" is missing source file\(s\) from BUILD\.gn:\s*(.+)$',
                    msg.strip(),
                )
                if ms_m:
                    req_srcs = [
                        s.strip() for s in ms_m.group(1).split(",") if s.strip()
                    ]
                    raw_sl = cur_info.get("raw_slice", "") or ""
                    overwrites = {
                        m.group(1).strip()
                        for m in re.finditer(
                            r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", raw_sl
                        )
                    }
                    if req_srcs and all(r in overwrites for r in req_srcs):
                        return (
                            f"Verified target '{tname}' in '{f_rel}': all GN source paths "
                            "are mapped in `srcs` via `# @bazel2gn:path_overwrite:` and "
                            "emitted below `## BAZEL2GN SENTINEL` in `BUILD.gn`."
                        )
        if cat == "dep_alignment":
            reason = _verify_dep_alignment_waiver(
                tname, pkg_dir, msg, cur_map, ext_pkg_cache
            )
            if reason:
                return reason
        if cat == "attribute_alignment":
            reason = _verify_attr_alignment_waiver(tname, msg, cur_map)
            if reason:
                return reason
            if (
                cur_info
                and cur_info["rule"] == "fx_package"
                and ("max_severity" in msg or "min_severity" in msg)
            ):
                return ""
        if cat == "template_parity":
            reason = _verify_template_parity_waiver(tname, pkg_dir, msg, cur_map)
            if reason:
                return reason
        if (
            cat in ("missing_sources", "attribute_alignment")
            and cur_info
            and cur_info["has_skip"]
            and "srcs" not in cur_info["kw_names"]
        ):
            return (
                f"Verified `# @bazel2gn:skip` bridge target '{tname}' in '{f_rel}': rule "
                "intentionally bridges GN build outputs into Bazel without a `srcs` attribute."
            )
    return ""


def waive_verified_review_diff_findings():
    """Emits explicit waivers for review_diff findings proven safe by AST inspection."""
    first_td = target_dirs[0] if target_dirs else ""
    if target_dir and first_td and target_dir != first_td:
        return
    diff_list = _load_review_diff_findings()
    if not diff_list:
        return
    bz_cache = {}
    ext_pkg_cache = {}
    gn_blob_cache = {}
    seen_keys = set()
    for df in diff_list:
        if not isinstance(df, dict):
            continue
        reason = _waiver_reason_for_review_diff(
            df, bz_cache, ext_pkg_cache, gn_blob_cache
        )
        if not reason:
            continue
        cat = df.get("category", "")
        f_rel = df.get("file", "")
        ln = int(df.get("line", 0) or 0)
        msg = df.get("message", "")
        key = (cat, f_rel, ln, msg)
        if key in seen_keys:
            continue
        seen_keys.add(key)
        entry = {
            "source": "migration_sanity",
            "category": cat,
            "severity": "info",
            "file": f_rel,
            "line": ln,
            "message": msg,
            "remediation": reason,
            "waiver": reason,
        }
        findings.append(entry)


# Check newly added compiled targets in build/bazel/toplevel.BUILD.bazel
def _check_toplevel_compiled_targets():
    tl_rel = "build/bazel/toplevel.BUILD.bazel"
    if tl_rel not in changed:
        return
    tl_full = os.path.join(workdir, tl_rel)
    if not os.path.isfile(tl_full):
        return
    try:
        cur_txt = open(tl_full, encoding="utf-8").read()
    except OSError:
        return
    base_map = _parse_bazel_call_map(_git_show_base_file(tl_rel))
    try:
        tree = ast.parse(cur_txt, filename=tl_full)
    except Exception:
        return
    code_exts = (".c", ".cc", ".cpp", ".cxx", ".S", ".s", ".rs", ".go")
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        tname = ""
        compiled_srcs = []
        for kw in call.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant):
                tname = str(kw.value.value)
            elif kw.arg == "srcs":
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        if sub.value.endswith(code_exts):
                            compiled_srcs.append(sub.value)
        if not tname or tname in base_map or not compiled_srcs:
            continue
        _emit(
            findings,
            "compiled_target_in_toplevel_build",
            "error",
            tl_rel,
            getattr(call, "lineno", 1),
            f"Target '{tname}' ({call.func.id}) in '{tl_rel}' compiles source files "
            f"({', '.join(compiled_srcs[:3])}) directly in the root workspace package. "
            "Do not sweep unmigrated third_party/ or multi-hop cross-area compiled "
            "libraries into `build/bazel/toplevel.BUILD.bazel`.",
            f"Revert `{tl_rel}` (`git checkout HEAD -- {tl_rel}`) instead of defining "
            "compiled library targets in the root package.",
        )


# Check unsynchronized out-of-target BUILD.bazel files and public headers placed in srcs
def _check_unsynced_ancillary_and_hdrs():
    for bz_rel in sorted(changed):
        if os.path.basename(bz_rel) != "BUILD.bazel":
            continue
        pkg = os.path.dirname(bz_rel).strip("/")
        if not pkg or pkg in EXCLUDED_GLOBAL_DIRS:
            continue
        bz_full = os.path.join(workdir, bz_rel)
        if not os.path.isfile(bz_full):
            continue
        try:
            bz_txt = open(bz_full, encoding="utf-8").read()
            tree = ast.parse(bz_txt, filename=bz_full)
        except Exception:
            continue
        gn_rel = f"{pkg}/BUILD.gn"
        gn_full = os.path.join(workdir, gn_rel)
        has_gn = os.path.isfile(gn_full)
        gn_txt = open(gn_full, encoding="utf-8").read() if has_gn else ""
        has_sent = "## BAZEL2GN SENTINEL" in gn_txt
        is_new_bz = not bool(_git_show_base_file(bz_rel))
        b_targets = parse_bazel_targets(bz_full, pkg_dir=pkg, text=bz_txt)

        # Check newly created ancillary BUILD.bazel files with unsynced skipped convertible rules
        if (
            is_new_bz
            and pkg not in target_dirs
            and has_gn
            and not has_sent
            and gn_rel not in changed
        ):
            # Header-only C/C++ bridge stubs (no compiled `srcs`) are the minimal
            # `# @bazel2gn:skip` stub that cq_reachability's missing_bazel_dependency_package
            # remediation prescribes (and bazel_minimality's header-stub scope exemption
            # accepts), so they are not unsynchronized cherry-picks.
            stub_code_exts = (
                ".c", ".cc", ".cpp", ".cxx", ".m", ".mm", ".s", ".S", ".rs", ".go", ".py",
            )
            header_only_stub_names = set()
            for st_node in tree.body:
                if not (
                    isinstance(st_node, ast.Expr)
                    and isinstance(st_node.value, ast.Call)
                    and isinstance(st_node.value.func, ast.Name)
                    and st_node.value.func.id
                    in ("cc_library", "fx_cc_library", "fx_cc_library_headers")
                ):
                    continue
                st_kws = {kw.arg: kw.value for kw in st_node.value.keywords if kw.arg}
                st_name = st_kws.get("name")
                if not (
                    isinstance(st_name, ast.Constant) and isinstance(st_name.value, str)
                ):
                    continue
                st_srcs = st_kws.get("srcs")
                if st_srcs is None or not any(
                    isinstance(sub, ast.Constant)
                    and isinstance(sub.value, str)
                    and sub.value.endswith(stub_code_exts)
                    for sub in ast.walk(st_srcs)
                ):
                    header_only_stub_names.add(st_name.value)
            skipped_conv = [
                t for t in b_targets
                if t.get("skip_line") is not None
                and t["rule"] in CONVERTIBLE_BAZEL_RULES
                and t.get("name") not in header_only_stub_names
            ]
            if skipped_conv:
                first_t = skipped_conv[0]
                _emit(
                    findings,
                    "unsynced_ancillary_bazel_pkg",
                    "error",
                    bz_rel,
                    first_t["skip_line"] or first_t["line"],
                    f"Newly created '{bz_rel}' defines `# @bazel2gn:skip` convertible target "
                    f"'{first_t['name']}' ({first_t['rule']}) while leaving '{gn_rel}' "
                    "untouched without `## BAZEL2GN SENTINEL`.",
                    f"Do not create unsynchronized `# @bazel2gn:skip` convertible targets in "
                    f"'{bz_rel}' while leaving '{gn_rel}' untouched. Either synchronize "
                    f"'{pkg}' properly with `## BAZEL2GN SENTINEL` and `fx bazel2gn -d {pkg}` "
                    "(without `# @bazel2gn:skip` or dead placeholder targets) when co-migrating "
                    "a small same-area library, or remove the cherry-picked out-of-scope "
                    f"'{bz_rel}' file.",
                )

        # Check public headers under `includes` directories placed in `srcs` instead of `hdrs`
        for node in tree.body:
            if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
                continue
            call = node.value
            if not isinstance(call.func, ast.Name):
                continue
            tname = ""
            inc_dirs = []
            src_hdrs = []
            for kw in call.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant):
                    tname = str(kw.value.value)
                elif kw.arg == "includes" and isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            d = elt.value.strip().strip("/")
                            if d:
                                inc_dirs.append(d + "/")
                elif kw.arg == "srcs" and isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            if elt.value.endswith((".h", ".hpp", ".hh")):
                                src_hdrs.append((elt.value, getattr(elt, "lineno", call.lineno)))
            for h_path, h_line in src_hdrs:
                if any(h_path.startswith(pfx) for pfx in inc_dirs):
                    _emit(
                        findings,
                        "public_include_header_in_srcs",
                        "error",
                        bz_rel,
                        h_line,
                        f"Target '{tname}' in '{bz_rel}' lists public header '{h_path}' "
                        "(under `includes`) in `srcs` instead of `hdrs`.",
                        f"Move '{h_path}' from `srcs` to `hdrs` in '{bz_rel}' (adding "
                        "`# @bazel2gn:skip` on that entry if the GN target omits it from `public`).",
                    )


_check_toplevel_compiled_targets()
_check_unsynced_ancillary_and_hdrs()
waive_verified_review_diff_findings()

print(json.dumps(findings, indent=2))
PYEOF
