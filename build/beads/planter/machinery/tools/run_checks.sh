#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Runs the deterministic checks of checks/manifest.json the way planter runs them
# after the coder reports: same scripts, arguments, environment, order, JSON
# parsing, severity normalization and de-duplication.
#
# Usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${PLANTER_PROJECT_DIR:-$(dirname "$SCRIPT_DIR")}"

python3 - "$PROJECT_DIR" "$@" <<'PYEOF'
import ast
import atexit
from concurrent.futures import ThreadPoolExecutor
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time

USAGE = """\
usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]

Runs the checks in checks/manifest.json the way planter runs them after the
coder reports. Exits 1 if any ERROR or WARNING finding would send the change
back, 0 otherwise.

  --skip-build  set PLANTER_SKIP_BUILD=1 so build_verification does not build
  --only NAMES  run only these checks (comma-separated, repeatable)
  --json        print all findings as one JSON array instead of a report
  --list        list the checks and exit
  DIR...        target directories (default: $PLANTER_TARGET_DIRS)"""

SERIAL_BUILD_CHECKS = ("build_verification", "tests_json_parity")
# Must match planter's upload gate (mergeReviewDiffFindings): only visibility_narrowed
# honors check waivers. Listing other review_diff categories here hides blocking
# diffengine findings during coder runs that planter's upload gate still rejects.
WAIVABLE_REVIEW_DIFF_CATEGORIES = {"visibility_narrowed"}

sys.stdout.reconfigure(errors="replace")
sys.stderr.reconfigure(errors="replace")


def log(msg):
    print(f"[run_checks] {msg}", file=sys.stderr, flush=True)


def die(msg, show_usage=False):
    suffix = f"\n\n{USAGE}" if show_usage else ""
    print(f"run_checks: {msg}{suffix}", file=sys.stderr)
    sys.exit(2)


project_dir = os.path.abspath(sys.argv[1])
alt_root = os.path.join(project_dir, "machinery")
mroot = (
    alt_root
    if os.path.isdir(alt_root)
    and not os.path.islink(alt_root)
    and os.path.exists(os.path.join(alt_root, "prompts", "coder.md"))
    and not os.path.exists(os.path.join(project_dir, "prompts", "coder.md"))
    else project_dir
)

sys.path.insert(0, os.path.join(mroot, "checks"))
import planter_check_lib as pcl
import rdeps_lib as rl

skip_build = as_json = list_only = False
only, dir_args, args, i = [], [], sys.argv[2:], 0
while i < len(args):
    a = args[i]
    if a in ("-h", "--help"):
        print(USAGE)
        sys.exit(0)
    elif a == "--skip-build":
        skip_build = True
    elif a == "--json":
        as_json = True
    elif a == "--list":
        list_only = True
    elif a == "--only" or a.startswith("--only="):
        if a == "--only":
            i += 1
            if i >= len(args):
                die("--only needs a comma-separated list of check names", True)
            value = args[i]
        else:
            value = a[len("--only="):]
        names = [n.strip() for n in value.split(",") if n.strip()]
        if not names:
            die("--only needs a comma-separated list of check names", True)
        only.extend(names)
    elif a == "--":
        dir_args.extend(args[i + 1:])
        break
    elif a.startswith("-"):
        die(f"unknown option {a}", True)
    else:
        dir_args.append(a)
    i += 1

manifest_path = os.path.join(mroot, "checks", "manifest.json")
try:
    specs = pcl.load_check_manifest(manifest_path)
except ValueError as e:
    die(f"cannot parse {manifest_path} (planter would fail the same way): {e}")
if specs is None:
    log(f"no readable {manifest_path}; planter runs no checks")
    specs = []

if list_only:
    for s in specs:
        print(f"{s['name']}{'' if s['enabled'] else ' [disabled]'}: {s['description']}")
    sys.exit(0)

enabled = [s for s in specs if s["enabled"]]
disabled = [s for s in specs if not s["enabled"]]
if only:
    known = {s["name"] for s in specs} | {"review_diff"}
    unknown = [n for n in only if n not in known]
    if unknown:
        die(f"unknown check(s): {', '.join(unknown)}; checks: {', '.join(s['name'] for s in specs)}, review_diff", True)
    wanted = set(only)
    enabled = [s for s in enabled if s["name"] in wanted]
    disabled = [s for s in disabled if s["name"] in wanted]

# Target directory arguments may be whitespace- or comma-delimited; order is preserved.
workdir = os.environ.get("PLANTER_WORKDIR", "") or os.getcwd()
if dir_args:
    target_dirs = pcl.parse_target_dirs(" ".join(dir_args), workdir)
else:
    target_dirs = pcl.parse_target_dirs(os.environ.get("PLANTER_TARGET_DIRS", ""), workdir)
    if not target_dirs and os.environ.get("PLANTER_TARGET_DIR", "").strip():
        target_dirs = pcl.parse_target_dirs(os.environ["PLANTER_TARGET_DIR"], workdir)

# Fetch origin/main at most once up front, then export PLANTER_SKIP_FETCH=1 to all checks.
if os.environ.get("PLANTER_SKIP_FETCH", "").strip() in ("", "0"):
    for repo in rl.discover_git_repos(workdir):
        rl.maybe_refresh_repo_origin_main(repo)
os.environ["PLANTER_SKIP_FETCH"] = "1"

# Resolve PLANTER_RESOLVED_CHANGE_BASE and PLANTER_CHANGED_FILES once up front.
change_base = pcl.resolve_change_base(workdir, target_dirs=target_dirs)
os.environ["PLANTER_RESOLVED_CHANGE_BASE"] = change_base
if "PLANTER_CHANGED_FILES" not in os.environ:
    os.environ["PLANTER_CHANGED_FILES"] = "\n".join(
        pcl.changed_files(workdir, change_base, target_dirs=target_dirs)
    )

signal.signal(signal.SIGTERM, lambda signum, _frame: sys.exit(128 + signum))


def execute(cmd, cwd, env, label):
    return pcl.execute_subprocess(cmd, cwd, env, label=label, log_fn=log)


_precomputed_diff_findings = None
macro_dispute_items = []
verified_diff_fp_ids = set()

# Detect Starlark macro and GN label-normalization false positives in review_diff findings.
# Known GN -> Bazel Starlark macro attribute renames that bazel2gn maps back to GN.
MACRO_ATTR_EQUIVALENTS = {
    "api": ("api_file_path",),
    "args": ("test_args",),
    "args_source_root": ("args_crate_root",),
    "args_sources": ("args_srcs",),
    "extra_args": ("test_args", "args"),
    "features": ("crate_features",),
    "inputs": ("compile_data",),
    "libraries": ("deps",),
    "library_deps": ("deps",),
    "log_settings": ("max_log_severity",),
    "main_source": ("main", "srcs"),
    "max_severity": ("max_log_severity",),
    "output_name": (
        "output_name",
        "binary_name",
        "idk_name",
        "crate_name",
        "gn_output_name",
    ),
    "public": ("hdrs",),
    "rustenv": ("rustc_env",),
    "rustflags": ("rustc_flags",),
    "sdk_area": ("api_area",),
    "sdk_category": ("sdk_category", "category"),
    "sdk_name": ("idk_name",),
    "sdk_target_name": ("sdk_target_name", "idk_name"),
    "source_root": ("crate_root",),
    "sources": ("srcs",),
    "test_specs": ("environments", "max_log_severity"),
}

# Bazel test rules/macros that are implicitly `testonly = True` (excluding
# `host_py_test`, whose inner `py_binary` inherits `**kwargs` without setting
# `testonly = True`, and `host_test_data_files`, which is a plain rule).
IMPLICIT_TESTONLY_BAZEL_RULES = (
    pcl.BAZEL_TEST_RULES - {"host_py_test", "host_test_data_files"}
) | {"test_suite"}

# Host-only Starlark macros/rules that apply host constraints internally and do not
# take explicit `target_compatible_with = HOST_OS_CONSTRAINTS` in BUILD.bazel.
# Note: `host_py_test` (`legacy_host_py_test`) forwards `**kwargs` to its inner
# `py_binary` without setting `target_compatible_with` or `tags = ["manual"]`, so it
# requires `target_compatible_with = HOST_OS_CONSTRAINTS`.
IMPLICIT_HOST_ONLY_MACROS = {
    "cc_binary_host_tool",
    "ffx_plugin",
    "ffx_tool",
    "go_binary_host_tool",
    "host_cc_test",
    "host_go_test",
    "host_rustc_test",
    "host_shac_test",
    "host_test",
    "idk_cc_binary_host_tool",
    "idk_ffx_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "py_binary_host_tool",
    "rustc_binary_host_tool",
    "wrap_host_rust_test",
}

ATTR_ALIGN_MSG_RE = re.compile(
    r'^Target "([^"]+)" \(Target Attributes\): ([A-Za-z_]\w*) in GN not carried over to Bazel$'
)
IDK_NAME_RENAME_MSG_RE = re.compile(
    r'^Target "([^"]+)" \(Target Attributes\): idk_name=(\S+) differs from '
    r'GN\'s default sdk_name "[^"]+" \(the target name\); the IDK atom is renamed$'
)
ATTR_ALIGN_ESCAPED_QUOTE_RE = re.compile(
    r'^Target "([^"]+)" \(Target Attributes\): (rustflags|cflags\w*|defines)\s*\(\[.*\\.*\]\)\s*!=\s*(rustc_flags|copts|defines)\s*\(\['
)
RUST_CRATE_HYPHEN_MSG_RE = re.compile(
    r'^Target "([^"]+)" \(Target Attributes\): '
    r"(output_name|name|crate_name) \(\[([^\]]+)\]\) != "
    r"(crate_name|output_name|name) \(\[([^\]]+)\]\)$"
)
ATTR_ALIGN_UNUSUAL_RULE_RE = re.compile(
    r'^Target "([^"]+)" \(Rule & Directives\): '
    r"Unusual rule mapping from ([A-Za-z_]\w*) to ([A-Za-z_]\w*)$"
)

# Standard Fuchsia GN-to-Bazel rule mappings flagged as unusual by review_diff.
VALID_GN_TO_BAZEL_RULE_MAPPINGS = {
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
HOST_COMPAT_MSG_RE = re.compile(
    r'^Target "([^"]+)" .*missing `target_compatible_with = HOST_(?:OS_)?CONSTRAINTS` in BUILD\.bazel'
)
DEP_ALIGN_UNMATCHED_RE = re.compile(
    r'^Target "([^"]+)" dependencies differ between GN and Bazel:.*?'
    r"unmatched in GN: \[([^\]]+)\](?:; unmatched in Bazel: \[([^\]]+)\])?\s*$"
)
DEP_ALIGN_BAZEL_ONLY_RE = re.compile(
    r'^Target "([^"]+)" dependencies differ between GN and Bazel:'
    r"(?!.*unmatched in GN:).*?unmatched in Bazel: \[([^\]]+)\]\s*$"
)
UNIT_TEST_PARITY_MSG_RE = re.compile(
    r'^Unit tests of "([^"]+)": GN test_deps \[([^\]]+)\] '
    r'are not dependencies of "([^"]+)"\.?\s*$'
)
UNIT_TEST_CRATE_FEATURES_MSG_RE = re.compile(
    r'^Unit tests of "([^"]+)": "([^"]+)" does not enable crate_features \[([^\]]+)\]'
)
MISSING_SOURCES_MSG_RE = re.compile(
    r'^Target "([^"]+)" is missing source file\(s\) from BUILD\.gn:\s*(.+?)\s*$'
)
TEST_DATA_PATH_RE = re.compile(
    r"(^|[/_.-])(test_?data|testdata|tests?|fixtures?|golden)([/_.-]|$)", re.I
)

KNOWN_GN_TARGET_ATTRS = {
    "all_dependent_configs",
    "api",
    "args",
    "args_source_root",
    "args_sources",
    "args_test_deps",
    "args_with_unit_tests",
    "asmflags",
    "assert_no_deps",
    "category",
    "cflags",
    "cflags_c",
    "cflags_cc",
    "cflags_objc",
    "cflags_objcc",
    "check_cfgs",
    "cm_label",
    "complete_static_lib",
    "component_name",
    "config_data",
    "configs",
    "crate_name",
    "crate_root",
    "data",
    "data_deps",
    "defines",
    "depfile",
    "deps",
    "disable_clippy",
    "disable_rbe",
    "disable_syslog_backend",
    "edition",
    "exclude_toolchain_tags",
    "features",
    "include_dirs",
    "inputs",
    "ldflags",
    "lib_dirs",
    "library_name",
    "libs",
    "link_deps",
    "lint_config",
    "main_source",
    "manifest",
    "mnemonic",
    "name",
    "non_rust_deps",
    "non_test_deps",
    "output_dir",
    "output_extension",
    "output_name",
    "outputs",
    "package_name",
    "plugin_deps",
    "pool",
    "proc_macro_deps",
    "public",
    "public_configs",
    "public_deps",
    "resources",
    "rustenv",
    "rustflags",
    "script",
    "sdk_area",
    "sdk_category",
    "sdk_headers_for_internal_use",
    "sdk_name",
    "sdk_publishable",
    "source_root",
    "sources",
    "test_deps",
    "testonly",
    "version",
    "visibility",
    "with_unit_tests",
}


_tp_target_map_cache = None


# Load build/tools/bazel2gn/third_party_target_map.json mapping @repo//... labels to GN.
def _load_third_party_target_map(wdir=""):
    global _tp_target_map_cache
    if _tp_target_map_cache is not None:
        return _tp_target_map_cache
    root = wdir or workdir
    tp_path = os.path.join(
        root, "build", "tools", "bazel2gn", "third_party_target_map.json"
    )
    try:
        with open(tp_path, encoding="utf-8") as tf:
            data = json.load(tf)
        _tp_target_map_cache = data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        _tp_target_map_cache = {}
    return _tp_target_map_cache


# Normalize GN/Bazel dependency labels (collapsing duplicate `/` inside package paths).
def _canon_dep_label(raw, pkg_dir=""):
    lbl = raw.strip().strip('"').strip("'")
    if not lbl:
        return None
    lbl = re.sub(r"\([^()]*\)$", "", lbl).strip()
    if not lbl or "$" in lbl:
        return None
    if lbl.startswith("@@"):
        lbl = lbl[1:]
    if lbl.startswith("@//"):
        lbl = lbl[1:]
    if lbl.startswith("@"):
        mapped = _load_third_party_target_map().get(lbl)
        if not mapped:
            return None
        lbl = mapped
    if lbl.startswith("//"):
        body = re.sub(r"/+", "/", lbl[2:])
        path, sep, name = body.partition(":")
    elif lbl.startswith(":"):
        if not pkg_dir:
            return None
        path, sep, name = pkg_dir, ":", lbl[1:]
    elif pkg_dir:
        rel, sep, name = re.sub(r"/+", "/", lbl).partition(":")
        path = os.path.normpath(os.path.join(pkg_dir, rel)).replace("\\", "/")
        if path.startswith(".."):
            return None
    else:
        return None
    path = path.strip("/")
    if not path:
        return None
    name = name if sep else path.rsplit("/", 1)[-1]
    return f"//{path}:{name}" if name else None


# Read a package's BUILD.gn at base_rev (before migration edits or deletion).
def _read_base_pkg_gn(wdir, pkg_rel, base_rev):
    if not wdir or not pkg_rel or not base_rev:
        return ""
    try:
        return subprocess.check_output(
            ["git", "-C", wdir, "show", f"{base_rev}:{pkg_rel}/BUILD.gn"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return ""


# Check whether gn_attr was a local GN helper variable in base BUILD.gn whose
# interpolated target attribute(s) are already carried on the Bazel target.
def _is_base_gn_local_helper_var(wdir, pkg_rel, base_rev, gn_attr, bz_attrs):
    if gn_attr in KNOWN_GN_TARGET_ATTRS or not bz_attrs:
        return False
    base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
    if not base_gn:
        return False
    clean_base = pcl.blank_comments(base_gn)
    if not re.search(rf"(?m)^\s*{re.escape(gn_attr)}\s*=", clean_base):
        return False
    var_ref_re = re.compile(
        rf"(?:\$\{{?{re.escape(gn_attr)}\}}?|(?<![\w.$]){re.escape(gn_attr)}(?![\w]))"
    )
    consumer_attrs = set()
    for am in re.finditer(
        r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=\s*(\[[^\]]*\]|[^\n]+)", clean_base
    ):
        other_attr, rhs = am.group(1), am.group(2)
        if other_attr != gn_attr and var_ref_re.search(rhs):
            consumer_attrs.add(other_attr)
    if not consumer_attrs:
        return False
    return all(
        any(eq in bz_attrs for eq in MACRO_ATTR_EQUIVALENTS.get(ca, (ca,)))
        for ca in consumer_attrs
    )


# Check whether gn_attr is a unit-test generation or test-deps attribute decomposed
# into standalone Rust test target(s) in BUILD.bazel (e.g. ffx_plugin args_with_unit_tests).
def _is_decomposed_unit_test_attr_false_positive(
    wdir, rel_bz, pkg_rel, base_rev, tname, gn_attr, bz_text, bz_targets
):
    if gn_attr not in (
        "args_with_unit_tests",
        "with_unit_tests",
        "args_test_deps",
        "test_deps",
    ):
        return False
    owner_targets = [bt for bt in bz_targets if bt["name"] == tname]
    if not owner_targets:
        return False
    is_args = gn_attr in ("args_with_unit_tests", "args_test_deps")
    crate_target = f"{tname}_args" if is_args else tname
    crate_forms = {f":{crate_target}", crate_target}
    if pkg_rel:
        crate_forms.add(f"//{pkg_rel}:{crate_target}")
    var_table = _parse_bazel_var_table(bz_text, filename=rel_bz)
    matched_tests = []
    for bt in bz_targets:
        if bt["rule"] not in ("host_rustc_test", "rustc_test", "rust_test"):
            continue
        crate_vals = pcl._extract_ast_strings(
            bt.get("keywords", {}).get("crate"), var_table
        )
        if any(cv.lstrip("@") in crate_forms for cv in crate_vals):
            matched_tests.append(bt["name"])
    if gn_attr in ("args_with_unit_tests", "with_unit_tests"):
        if matched_tests:
            return True
        base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
        if base_gn:
            for gt in pcl.extract_gn_targets(base_gn):
                if gt["name"] == tname and re.search(
                    rf"(?<![\w.]){re.escape(gn_attr)}\s*=\s*false\b",
                    gt["clean_body"],
                ):
                    return True
        src_attr = "args_srcs" if is_args else "srcs"
        src_rels = pcl._extract_ast_strings(
            owner_targets[0].get("keywords", {}).get(src_attr), var_table
        )
        if src_rels:
            has_tests = False
            for rs_rel in src_rels:
                rs_abs = os.path.join(wdir, pkg_rel, rs_rel.lstrip(":/"))
                if not os.path.isfile(rs_abs):
                    has_tests = True
                    break
                try:
                    with open(rs_abs, encoding="utf-8", errors="ignore") as rf:
                        rs_txt = rf.read()
                except OSError:
                    has_tests = True
                    break
                if re.search(
                    r"#\s*\[\s*(?:cfg\s*\(\s*test\b|(?:\w+::)*test\b)", rs_txt
                ) or re.search(r"(?m)^\s*(?:pub\s+)?mod\s+\w+\s*;", rs_txt):
                    has_tests = True
                    break
            if not has_tests:
                return True
        return False
    base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
    if not base_gn:
        return False
    gn_items = []
    found_owner_gn = False
    for gt in pcl.extract_gn_targets(base_gn):
        if gt["name"] == tname:
            found_owner_gn = True
            gn_items.extend(_extract_gn_target_list_strings(gt["clean_body"], gn_attr))
    if found_owner_gn and not gn_items:
        return True
    if not gn_items or not matched_tests:
        return False
    gn_canon = [_canon_dep_label(s, pkg_rel) for s in gn_items]
    if None in gn_canon:
        return False
    for ut_name in matched_tests:
        ctx = _collect_unit_test_bazel_context(
            wdir, rel_bz, tname, ut_name, pkg_rel
        )
        if ctx is None:
            continue
        code_deps, data_deps, overwrites, misplaced_data = ctx
        if any(_norm_vendor_crate_label(g) in misplaced_data for g in gn_canon):
            continue
        stop_set = code_deps | data_deps
        all_ok = True
        for g_lbl in gn_canon:
            g_norm = _norm_vendor_crate_label(g_lbl)
            if g_norm in stop_set or (
                g_norm in overwrites and (overwrites[g_norm] & stop_set)
            ):
                continue
            exp = _expand_pure_gn_forwarding_group(g_lbl, wdir, base_rev, stop_set)
            if not exp or not (exp <= stop_set):
                all_ok = False
                break
        if all_ok:
            return True
    return False


# Check whether an unusual rule mapping attribute_alignment finding uses a valid Fuchsia rule pair.
def _match_verified_unusual_rule_mapping(msg, wdir, rel_bz):
    m_rule = ATTR_ALIGN_UNUSUAL_RULE_RE.match(msg)
    if not m_rule:
        return None
    tname, gn_rule, bz_rule = m_rule.group(1), m_rule.group(2), m_rule.group(3)
    if (gn_rule, bz_rule) not in VALID_GN_TO_BAZEL_RULE_MAPPINGS:
        return None
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return None
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return None
    for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz):
        if bt["name"] == tname and bt["rule"] == bz_rule:
            return (tname, f"{gn_rule} -> {bz_rule}", "rule_mapping")
    return None


# Return the 0-based [first, end) line slice of Bazel target `bt`, including the
# contiguous `#` comment lines (e.g. `# @bazel2gn:` directives) directly above it.
def _bazel_target_lines_with_leading_comments(lines, bt):
    start_ln = max(1, bt.get("line", 1))
    end_ln = min(len(lines), bt.get("end_line", start_ln))
    idx = start_ln - 2
    while idx >= 0 and lines[idx].strip().startswith("#"):
        idx -= 1
    return idx + 1, end_ln


# Check whether attribute_alignment is a Starlark macro rename, IDK name rename, companion
# manifest attribute, valid GN-to-Bazel rule mapping, escaped-quote flag, decomposed unit-test
# attribute, or local GN helper var false positive.
def is_macro_synced_attr_false_positive(finding, wdir, base_rev=""):
    """Returns (target_name, gn_attr, kind) when BUILD.bazel / bazel2gn already carries gn_attr."""
    if finding.get("category") != "attribute_alignment":
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = finding.get("file", "")
    if not rel_bz.endswith("BUILD.bazel") or not wdir:
        return None
    msg = finding.get("message", "")
    pkg_rel = os.path.dirname(rel_bz).strip("/")
    pkg_dir = os.path.dirname(os.path.join(wdir, rel_bz))
    m_idk = IDK_NAME_RENAME_MSG_RE.match(msg)
    if m_idk:
        tname, idk_val = m_idk.group(1), m_idk.group(2)
        val_re = re.compile(
            rf'(?<![\w.])(?:output_name|idk_name|sdk_name)\s*=\s*"{re.escape(idk_val)}"'
        )
        gn_path = os.path.join(pkg_dir, "BUILD.gn")
        gn_cur = ""
        if os.path.isfile(gn_path):
            try:
                with open(gn_path, encoding="utf-8") as gf:
                    gn_cur = gf.read()
            except OSError:
                gn_cur = ""
        gn_base = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
        if val_re.search(pcl.blank_comments(gn_cur)) or val_re.search(
            pcl.blank_comments(gn_base)
        ):
            return (tname, "idk_name", "macro_attr")
        return None
    m_esc = ATTR_ALIGN_ESCAPED_QUOTE_RE.match(msg)
    if m_esc:
        return (m_esc.group(1), m_esc.group(2), "escaped_quote_flag")
    m_rust_hyphen = RUST_CRATE_HYPHEN_MSG_RE.match(msg)
    if m_rust_hyphen:
        tname, _, gn_val, bz_attr, bz_val = m_rust_hyphen.groups()
        if gn_val.strip().replace("-", "_") == bz_val.strip().replace("-", "_"):
            return (tname, bz_attr, "rust_crate_hyphen")
        return None
    matched_rule = _match_verified_unusual_rule_mapping(msg, wdir, rel_bz)
    if matched_rule:
        return matched_rule
    m = ATTR_ALIGN_MSG_RE.match(msg)
    if not m:
        return None
    tname, gn_attr = m.group(1), m.group(2)
    var_ref_re = re.compile(
        rf"(?:\$\{{?{re.escape(gn_attr)}\}}?|(?<![\w.$]){re.escape(gn_attr)}(?![\w]))"
    )

    # Check 1: Dual-build BUILD.gn has gn_attr synced or referenced via pre-sentinel var on tname.
    gn_path = os.path.join(pkg_dir, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            with open(gn_path, encoding="utf-8") as gf:
                gn_text = gf.read()
        except OSError:
            gn_text = ""
        if pcl.has_sentinel(gn_text):
            above, below = pcl.split_sentinel(gn_text)
            attr_re = re.compile(rf"(?<![\w.]){re.escape(gn_attr)}\s*\+?=")
            above_has_var = (
                gn_attr not in KNOWN_GN_TARGET_ATTRS
                and bool(
                    re.search(
                        rf"(?m)^\s*{re.escape(gn_attr)}\s*=", pcl.blank_comments(above)
                    )
                )
            )
            for gt in pcl.extract_gn_targets(below):
                if gt["name"] != tname:
                    continue
                if attr_re.search(gt["clean_body"]):
                    return (tname, gn_attr, "macro_attr")
                if above_has_var and var_ref_re.search(gt["clean_body"]):
                    return (tname, gn_attr, "local_gn_var")

    # Check 2: BUILD.bazel sets gn_attr counterpart, companion manifest attr, @bazel2gn var overwrite, or local helper attr.
    bz_path = os.path.join(wdir, rel_bz)
    if os.path.isfile(bz_path):
        try:
            with open(bz_path, encoding="utf-8") as bf:
                bz_text = bf.read()
        except OSError:
            bz_text = ""
        lines = bz_text.splitlines()
        ow_var_re = re.compile(
            r"#\s*@bazel2gn:(?:value_overwrite|raw_overwrite|raw_attribute):(.+)$"
        )
        raw_assign_re = re.compile(rf"\b{re.escape(gn_attr)}\s*(?:\+?=|-=)")
        equivs = MACRO_ATTR_EQUIVALENTS.get(gn_attr, (gn_attr,))
        bz_targets = pcl.extract_bazel_targets(bz_text, filename=rel_bz)
        by_name = {bt["name"]: bt for bt in bz_targets}
        for bt in bz_targets:
            if bt["name"] != tname:
                continue
            if any(a in bt["attrs"] for a in equivs):
                return (tname, gn_attr, "macro_attr")
            if gn_attr == "testonly" and bt["rule"] in IMPLICIT_TESTONLY_BAZEL_RULES:
                return (tname, gn_attr, "macro_attr")
            # fx_component / fx_test_component delegates component_name and manifest to
            # its referenced compiled_manifest (fx_component_manifest) target.
            cm_node = bt.get("keywords", {}).get("compiled_manifest")
            if isinstance(cm_node, ast.Constant) and isinstance(cm_node.value, str):
                cm_lbl = _canon_dep_label(cm_node.value, pkg_rel)
                cm_name = (
                    cm_lbl.split(":", 1)[1]
                    if cm_lbl and cm_lbl.startswith(f"//{pkg_rel}:")
                    else cm_node.value.lstrip(":")
                )
                cm_bt = by_name.get(cm_name)
                if cm_bt and any(a in cm_bt["attrs"] for a in equivs):
                    return (tname, gn_attr, "companion_manifest")
            if (
                gn_attr in ("component_name", "manifest", "restricted_features")
                and bt.get("rule")
                in (
                    "fx_component",
                    "fx_test_component",
                    "fx_package",
                    "fx_test",
                    "fx_unittest_package",
                )
                and any(
                    m_bt.get("rule") in ("fx_component_manifest", "fx_compiled_manifest")
                    and any(a in m_bt["attrs"] for a in equivs)
                    for m_bt in bz_targets
                )
            ):
                return (tname, gn_attr, "companion_manifest")
            # Unrolled fx_package test packages delegate environments / max_log_severity
            # to their paired fx_test(package = ":<tname>") target.
            if (
                gn_attr in ("environments", "log_settings", "max_severity", "test_specs")
                and bt.get("rule") == "fx_package"
            ):
                pkg_refs = {f":{tname}", tname}
                if pkg_rel:
                    pkg_refs.add(f"//{pkg_rel}:{tname}")
                for t_bt in bz_targets:
                    if t_bt.get("rule") != "fx_test":
                        continue
                    pkg_node = t_bt.get("keywords", {}).get("package")
                    if (
                        isinstance(pkg_node, ast.Constant)
                        and isinstance(pkg_node.value, str)
                        and pkg_node.value in pkg_refs
                        and any(a in t_bt["attrs"] for a in equivs)
                    ):
                        return (tname, gn_attr, "macro_attr")
            span_first, span_end = _bazel_target_lines_with_leading_comments(lines, bt)
            for raw_ln in lines[span_first:span_end]:
                om = ow_var_re.search(raw_ln)
                if om and raw_assign_re.search(om.group(1)):
                    return (tname, gn_attr, "macro_attr")
            if _is_decomposed_unit_test_attr_false_positive(
                wdir, rel_bz, pkg_rel, base_rev, tname, gn_attr, bz_text, bz_targets
            ):
                return (tname, gn_attr, "decomposed_unit_test")
            if gn_attr not in KNOWN_GN_TARGET_ATTRS:
                for ln in lines[span_first:span_end]:
                    om = ow_var_re.search(ln)
                    if om and var_ref_re.search(om.group(1)):
                        return (tname, gn_attr, "local_gn_var")
                if _is_base_gn_local_helper_var(
                    wdir, pkg_rel, base_rev, gn_attr, bt["attrs"]
                ):
                    return (tname, gn_attr, "local_gn_var")
    return None


# Check whether host_compatibility is a false positive on an implicitly host-only macro.
def is_host_only_macro_compat_false_positive(finding, wdir):
    """Returns (target_name, rule_name) when target uses an implicitly host-only macro."""
    if finding.get("category") != "host_compatibility":
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = finding.get("file", "")
    if not rel_bz.endswith("BUILD.bazel") or not wdir:
        return None
    m = HOST_COMPAT_MSG_RE.match(finding.get("message", ""))
    if not m:
        return None
    tname = m.group(1)
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return None
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return None
    for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz):
        if bt["name"] == tname and bt["rule"] in IMPLICIT_HOST_ONLY_MACROS:
            return (tname, bt["rule"])
    return None


PURE_GN_GROUP_ATTRS = {"public_deps", "deps", "visibility", "testonly"}
BAZEL_CODE_DEP_ATTRS = (
    "deps",
    "args_deps",
    "plugin_deps",
    "proc_macro_deps",
    "link_deps",
    "implementation_deps",
    "public_deps",
    "embed",
)
BAZEL_DATA_DEP_ATTRS = (
    "data",
    "test_data",
    "compile_data",
)
BAZEL_DEP_LIST_ATTRS = BAZEL_CODE_DEP_ATTRS + BAZEL_DATA_DEP_ATTRS
RUST_CRATE_DEP_RULES = {
    "rustc_library",
    "rust_library",
    "rustc_proc_macro",
    "rust_proc_macro",
}


# Normalize vendored Rust crate Bazel labels to GN crate labels for parity comparison.
def _norm_vendor_crate_label(canon_lbl):
    if canon_lbl and canon_lbl.startswith("//third_party/rust_crates/vendor:"):
        crate = canon_lbl.split(":", 1)[1]
        return f"//third_party/rust_crates:{crate}"
    return canon_lbl


# Read a dependency package's BUILD.gn from disk, falling back to base_rev when deleted.
def _read_dep_pkg_gn(wdir, dep_pkg, base_rev):
    gn_path = os.path.join(wdir, dep_pkg, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            with open(gn_path, encoding="utf-8") as gf:
                return gf.read()
        except OSError:
            return ""
    if not base_rev:
        return ""
    try:
        return subprocess.check_output(
            ["git", "-C", wdir, "show", f"{base_rev}:{dep_pkg}/BUILD.gn"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return ""


# Expand a pure GN forwarding `group("<name>")` (no configs or link settings) to its targets.
def _expand_pure_gn_forwarding_group(canon_lbl, wdir, base_rev, stop_set, seen=()):
    norm_lbl = _norm_vendor_crate_label(canon_lbl)
    if norm_lbl in stop_set:
        return {norm_lbl}
    if not canon_lbl or not wdir or canon_lbl in seen or len(seen) >= 6:
        return None
    dep_pkg, _, dep_name = canon_lbl[2:].partition(":")
    if not dep_pkg or not dep_name:
        return None
    gn_text = _read_dep_pkg_gn(wdir, dep_pkg, base_rev)
    if not gn_text:
        return None
    blocks = [gt for gt in pcl.extract_gn_targets(gn_text) if gt["name"] == dep_name]
    if len(blocks) != 1 or blocks[0]["tmpl"] != "group":
        return None
    clean_body = blocks[0]["clean_body"]
    attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", clean_body))
    if not attrs or not (attrs <= PURE_GN_GROUP_ATTRS):
        return None
    if re.search(r"(?m)^\s*[A-Za-z_]\w*\s*-=", clean_body) or re.search(
        r"\bif\s*\(", clean_body
    ):
        return None
    raw_deps = []
    for lm in re.finditer(r"(?<![\w.])(?:public_deps|deps)\s*\+?=\s*\[", clean_body):
        open_idx = lm.end() - 1
        close_idx = pcl.find_closing_delim(clean_body, open_idx, "[", "]")
        inner = clean_body[open_idx + 1 : close_idx]
        stripped_inner = re.sub(r'"(?:[^"\\]|\\.)*"', "", inner)
        if re.sub(r"[\s,]+", "", stripped_inner):
            return None
        raw_deps.extend(re.findall(r'"((?:[^"\\]|\\.)*)"', inner))
    if not raw_deps:
        return None
    expanded = set()
    next_seen = seen + (canon_lbl,)
    for raw in raw_deps:
        sub_canon = _canon_dep_label(raw, dep_pkg)
        if not sub_canon:
            return None
        sub_norm = _norm_vendor_crate_label(sub_canon)
        if sub_norm in stop_set:
            expanded.add(sub_norm)
            continue
        sub_exp = _expand_pure_gn_forwarding_group(
            sub_canon, wdir, base_rev, stop_set, next_seen
        )
        if sub_exp:
            expanded.update(sub_exp)
        else:
            expanded.add(sub_norm)
    return expanded if expanded else None


# Collect canonical Bazel deps, @bazel2gn:path_overwrite mappings, and @bazel2gn:raw_attribute
# dependency labels on target `tname`.
def _collect_bazel_target_dep_context(wdir, rel_bz, tname, pkg_rel):
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return set(), {}, set()
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return set(), {}, set()
    existing = set()
    overwrites = {}
    raw_attr_deps = set()
    lines = bz_text.splitlines()
    ow_re = re.compile(r'"([^"]+)"\s*,?\s*#\s*@bazel2gn:path_overwrite:(\S+)')
    raw_attr_re = re.compile(r"#\s*@bazel2gn:raw_(?:attribute|overwrite):(.+)$")
    bz_targets = pcl.extract_bazel_targets(bz_text, filename=rel_bz)
    # Map same-package fx_packaged_binary targets to their wrapped binary labels.
    pkg_bin_by_name = {}
    for bt in bz_targets:
        if bt.get("rule") == "fx_packaged_binary":
            b_node = bt.get("keywords", {}).get("binary")
            if isinstance(b_node, ast.Constant) and isinstance(b_node.value, str):
                pkg_bin_by_name[bt["name"]] = b_node.value
    for bt in bz_targets:
        if bt["name"] != tname:
            continue
        for d in bt.get("deps", ()):
            c = _canon_dep_label(d, pkg_rel)
            if c:
                norm_c = _norm_vendor_crate_label(c)
                existing.add(norm_c)
                d_name = d[1:] if d.startswith(":") else ""
                if d_name and d_name in pkg_bin_by_name:
                    inner_c = _canon_dep_label(pkg_bin_by_name[d_name], pkg_rel)
                    if inner_c:
                        norm_inner = _norm_vendor_crate_label(inner_c)
                        existing.add(norm_inner)
                        overwrites.setdefault(norm_inner, set()).add(norm_c)
        start_ln = max(1, bt.get("line", 1))
        span_first, end_ln = _bazel_target_lines_with_leading_comments(lines, bt)
        for raw_ln in lines[span_first:end_ln]:
            rm = raw_attr_re.search(raw_ln)
            if rm:
                for raw in re.findall(r'"([^"]+)"', rm.group(1)):
                    c = _canon_dep_label(raw, pkg_rel)
                    if c:
                        raw_attr_deps.add(_norm_vendor_crate_label(c))
        block_text = "\n".join(lines[start_ln - 1 : end_ln])
        for attr in BAZEL_DEP_LIST_ATTRS:
            for am in re.finditer(
                rf"(?m)^\s*{re.escape(attr)}\s*=\s*\[([^\]]*)\]", block_text
            ):
                for raw in re.findall(r'"([^"]+)"', am.group(1)):
                    c = _canon_dep_label(raw, pkg_rel)
                    if c:
                        existing.add(_norm_vendor_crate_label(c))
        for ln in lines[start_ln - 1 : end_ln]:
            om = ow_re.search(ln)
            if om:
                bz_c = _canon_dep_label(om.group(1), pkg_rel)
                gn_c = _canon_dep_label(om.group(2), pkg_rel)
                if bz_c and gn_c:
                    overwrites.setdefault(_norm_vendor_crate_label(gn_c), set()).add(
                        _norm_vendor_crate_label(bz_c)
                    )
    return existing, overwrites, raw_attr_deps


# Verify whether Bazel-only unmatched deps come from a shared `select(...)` list
# that was already present in conditional GN `deps` at base_rev and synced via bazel2gn.
def _is_conditional_select_dep_false_positive(wdir, pkg_rel, base_rev, bz_canon):
    if not wdir or not pkg_rel or not bz_canon:
        return False
    base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
    if not base_gn:
        return False
    base_norms = {
        _norm_vendor_crate_label(c)
        for raw in re.findall(r'"((?:[^"\\]|\\.)*)"', pcl.blank_comments(base_gn))
        for c in (_canon_dep_label(raw, pkg_rel),)
        if c
    }
    bz_norms = {_norm_vendor_crate_label(c) for c in bz_canon}
    if not (bz_norms <= base_norms):
        return False
    gn_path = os.path.join(wdir, pkg_rel, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            with open(gn_path, encoding="utf-8") as gf:
                cur_gn = gf.read()
        except OSError:
            cur_gn = ""
        if pcl.has_sentinel(cur_gn):
            _, below = pcl.split_sentinel(cur_gn)
            below_norms = {
                _norm_vendor_crate_label(c)
                for raw in re.findall(r'"((?:[^"\\]|\\.)*)"', pcl.blank_comments(below))
                for c in (_canon_dep_label(raw, pkg_rel),)
                if c
            }
            return bz_norms <= below_norms
    return True


# Verify that `tname` is a Go rule in BUILD.bazel whose same-named GN target lives
# below the BAZEL2GN SENTINEL of the package BUILD.gn (i.e. generated by bazel2gn),
# so GN/Bazel dep list differences are only label spelling or strict Go imports.
def _is_sentinel_synced_go_target_in_wdir(wdir, rel_bz, pkg_rel, tname):
    if not wdir or not tname:
        return False
    try:
        with open(os.path.join(wdir, rel_bz), encoding="utf-8") as bf:
            bz_text = bf.read()
        with open(os.path.join(wdir, pkg_rel, "BUILD.gn"), encoding="utf-8") as gf:
            gn_text = gf.read()
    except OSError:
        return False
    is_go = any(
        bt.get("name") == tname and str(bt.get("rule", "")).startswith("go_")
        for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz)
    )
    if not is_go or not pcl.has_sentinel(gn_text):
        return False
    _, below = pcl.split_sentinel(gn_text)
    return any(
        gt.get("name") == tname and str(gt.get("type", "")).startswith("go_")
        for gt in pcl.extract_gn_targets(below)
    )


NON_RUST_BAZEL_RULES_OMITTING_CRATE_DEPS = {
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


# Check whether an unmatched GN dependency on a non-Rust Bazel target resolves to a Rust
# crate (used in GN only for transitive expect_includes metadata) that Bazel rules reject.
def _is_cross_lang_rust_dep_on_non_rust_target(wdir, rel_bz, tname, gn_canon, base_rev=""):
    if not wdir or not gn_canon:
        return False
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return False
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return False
    t_rule = None
    for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz):
        if bt["name"] == tname:
            t_rule = bt["rule"]
            break
    if t_rule not in NON_RUST_BAZEL_RULES_OMITTING_CRATE_DEPS:
        return False
    for g_lbl in gn_canon:
        if _is_bazel_rust_crate_target(wdir, g_lbl):
            continue
        dep_pkg, _, dep_name = g_lbl[2:].partition(":")
        gn_text = _read_dep_pkg_gn(wdir, dep_pkg, base_rev)
        if any(
            gt["name"] == dep_name
            and gt["tmpl"] in ("rustc_library", "rustc_proc_macro")
            for gt in pcl.extract_gn_targets(gn_text)
        ):
            continue
        return False
    return True


# Check whether dep_alignment WARNING is a verified label-normalization, pure GN
# Check whether dep_alignment WARNING is a verified label-normalization, pure GN
# forwarding-group expansion, conditional select(), Python library_deps/libraries rename,
# @bazel2gn:path_overwrite, or @bazel2gn:raw_attribute false positive.
def is_verified_dep_alignment_false_positive(finding, wdir="", base_rev=""):
    """Returns (target_name, kind) when unmatched GN/Bazel dep lists are equivalent."""
    if finding.get("category") != "dep_alignment":
        return None
    if finding.get("severity") != "WARNING":
        return None
    rel_bz = finding.get("file", "")
    if not rel_bz.endswith("BUILD.bazel"):
        return None
    pkg_rel = os.path.dirname(rel_bz).strip("/")
    m_bz_only = DEP_ALIGN_BAZEL_ONLY_RE.match(finding.get("message", ""))
    if m_bz_only:
        tname = m_bz_only.group(1)
        bz_items = [s.strip() for s in m_bz_only.group(2).split(",") if s.strip()]
        bz_canon = [_canon_dep_label(s, pkg_rel) for s in bz_items]
        if (
            bz_canon
            and None not in bz_canon
            and _is_conditional_select_dep_false_positive(
                wdir, pkg_rel, base_rev, bz_canon
            )
        ):
            return (tname, "conditional_select")
        if _is_sentinel_synced_go_target_in_wdir(wdir, rel_bz, pkg_rel, tname):
            return (tname, "go_strict_deps")
        if bz_canon and None not in bz_canon and wdir:
            base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
            py_gn_items = []
            for gt in pcl.extract_gn_targets(base_gn):
                if gt["name"] != tname:
                    continue
                for dm in re.finditer(
                    r"(?<![\w.])(?:library_deps|libraries|deps|public_deps)\s*\+?=\s*\[([^\]]*)\]",
                    gt["clean_body"],
                ):
                    py_gn_items.extend(re.findall(r'"([^"]+)"', dm.group(1)))
            py_gn_canon = [_canon_dep_label(s, pkg_rel) for s in py_gn_items]
            if py_gn_canon and None not in py_gn_canon and set(py_gn_canon) == set(bz_canon):
                return (tname, "slash_or_shorthand")
        return None
    m = DEP_ALIGN_UNMATCHED_RE.match(finding.get("message", ""))
    if not m:
        return None
    tname = m.group(1)
    gn_items = [s.strip() for s in m.group(2).split(",") if s.strip()]
    bz_items = [s.strip() for s in (m.group(3) or "").split(",") if s.strip()]
    gn_canon = [_canon_dep_label(s, pkg_rel) for s in gn_items]
    bz_canon = [_canon_dep_label(s, pkg_rel) for s in bz_items]
    if None in gn_canon or None in bz_canon:
        return None
    if gn_canon and bz_canon and set(gn_canon) == set(bz_canon):
        if any(s.startswith("@") and not s.startswith("@//") for s in bz_items):
            return (tname, "third_party_ext_repo")
        return (tname, "slash_or_shorthand")
    if not wdir:
        return None
    existing_bz, overwrites, raw_attr_deps = _collect_bazel_target_dep_context(
        wdir, rel_bz, tname, pkg_rel
    )
    gn_norm_set = {_norm_vendor_crate_label(c) for c in gn_canon}
    if not bz_canon and gn_canon and raw_attr_deps and gn_norm_set <= (
        raw_attr_deps | existing_bz
    ):
        return (tname, "raw_attribute")
    if _is_sentinel_synced_go_target_in_wdir(wdir, rel_bz, pkg_rel, tname):
        return (tname, "go_strict_deps")
    if not bz_canon and _is_cross_lang_rust_dep_on_non_rust_target(
        wdir, rel_bz, tname, gn_canon, base_rev
    ):
        return (tname, "cross_lang_rust_dep")
    if not gn_canon or not bz_canon:
        return None
    bz_norm_set = {_norm_vendor_crate_label(c) for c in bz_canon}
    stop_set = bz_norm_set | existing_bz | raw_attr_deps
    expanded_gn = set()
    for g_lbl in gn_canon:
        g_norm = _norm_vendor_crate_label(g_lbl)
        if g_norm in stop_set:
            expanded_gn.add(g_norm)
        elif g_norm in overwrites and (overwrites[g_norm] & stop_set):
            expanded_gn.update(overwrites[g_norm] & stop_set)
        else:
            exp = _expand_pure_gn_forwarding_group(g_lbl, wdir, base_rev, stop_set)
            if not exp:
                return None
            expanded_gn.update(exp)
    if expanded_gn <= stop_set and bz_norm_set <= expanded_gn:
        return (tname, "forwarding_group")
    return None


# Extract top-level Starlark variable assignments for AST string resolution.
def _parse_bazel_var_table(bz_text, filename="<BUILD.bazel>"):
    try:
        tree = ast.parse(bz_text, filename=filename)
    except SyntaxError:
        return {}
    var_table = {}
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    var_table[tgt.id] = stmt.value
    return var_table


# Check whether canon_lbl resolves to a Rust crate/proc-macro target requiring `deps`.
def _is_bazel_rust_crate_target(wdir, canon_lbl):
    if not canon_lbl or not canon_lbl.startswith("//"):
        return False
    if canon_lbl.startswith(
        (
            "//third_party/rust_crates:",
            "//third_party/rust_crates/vendor:",
            "//third_party/rust_crates/forks/",
        )
    ):
        return True
    dep_pkg, _, dep_name = canon_lbl[2:].partition(":")
    if not dep_pkg or not dep_name:
        return False
    if dep_name.endswith(
        ("_rust", "_rust_flex", "_rust_fdomain", "_rust_fdomain_flex", "_rust_next")
    ):
        return True
    bz_path = os.path.join(wdir, dep_pkg, "BUILD.bazel")
    if not os.path.isfile(bz_path):
        return False
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return False
    for bt in pcl.extract_bazel_targets(bz_text, filename=bz_path):
        if bt["name"] == dep_name and (
            bt["rule"] in RUST_CRATE_DEP_RULES or bt["rule"] == "fidl_library"
        ):
            return True
    return False


# Collect code_deps, data_deps, and @bazel2gn:path_overwrite mappings for unit test target.
def _collect_unit_test_bazel_context(wdir, rel_bz, lib_name, test_name, pkg_rel):
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return None
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return None
    var_table = _parse_bazel_var_table(bz_text, filename=rel_bz)
    bz_targets = pcl.extract_bazel_targets(bz_text, filename=rel_bz)
    by_name = {bt["name"]: bt for bt in bz_targets}
    matched_names = set()
    if test_name in by_name:
        matched_names.add(test_name)
        bin_vals = pcl._extract_ast_strings(
            by_name[test_name]["keywords"].get("binary_name"), var_table
        )
        for bv in bin_vals:
            bname = bv.lstrip(":")
            if bname in by_name:
                matched_names.add(bname)
    for bt in bz_targets:
        if bt["rule"] == "wrap_host_rust_test":
            bin_vals = pcl._extract_ast_strings(
                bt["keywords"].get("binary_name"), var_table
            )
            if any(bv.lstrip(":") == test_name for bv in bin_vals):
                matched_names.add(bt["name"])
    if not matched_names:
        return None

    code_deps = set()
    data_deps = set()
    # Runtime test data placed in `data` of a host_rustc_test (forwarded to the
    # inner rustc_test binary) instead of `test_data` (forwarded to the
    # host_test() wrapper). These are real findings, not false positives.
    host_test_misplaced_data = set()
    overwrites = {}
    lines = bz_text.splitlines()
    ow_re = re.compile(r'"([^"]+)"\s*,?\s*#\s*@bazel2gn:path_overwrite:(\S+)')
    for t_nm in matched_names:
        bt = by_name[t_nm]
        kws = bt.get("keywords", {})
        for attr in BAZEL_CODE_DEP_ATTRS:
            for raw in pcl._extract_ast_strings(kws.get(attr), var_table):
                c = _canon_dep_label(raw, pkg_rel)
                if c:
                    code_deps.add(_norm_vendor_crate_label(c))
        for attr in BAZEL_DATA_DEP_ATTRS:
            for raw in pcl._extract_ast_strings(kws.get(attr), var_table):
                c = _canon_dep_label(raw, pkg_rel)
                if not c:
                    continue
                if attr == "data" and bt["rule"] == "host_rustc_test":
                    host_test_misplaced_data.add(_norm_vendor_crate_label(c))
                else:
                    data_deps.add(_norm_vendor_crate_label(c))
        start_ln = max(1, bt.get("line", 1))
        end_ln = min(len(lines), bt.get("end_line", start_ln))
        for ln in lines[start_ln - 1 : end_ln]:
            om = ow_re.search(ln)
            if om:
                bz_c = _canon_dep_label(om.group(1), pkg_rel)
                gn_c = _canon_dep_label(om.group(2), pkg_rel)
                if bz_c and gn_c:
                    overwrites.setdefault(_norm_vendor_crate_label(gn_c), set()).add(
                        _norm_vendor_crate_label(bz_c)
                    )
    if lib_name in by_name:
        lib_kws = by_name[lib_name].get("keywords", {})
        for attr in ("test_data", "test_compile_data"):
            for raw in pcl._extract_ast_strings(lib_kws.get(attr), var_table):
                c = _canon_dep_label(raw, pkg_rel)
                if c:
                    data_deps.add(_norm_vendor_crate_label(c))

    # Expand same-package host_test_data_files / host_test_data_map targets in data_deps.
    local_prefix = f"//{pkg_rel}:" if pkg_rel else "//:"
    for d_lbl in list(data_deps):
        if not d_lbl.startswith(local_prefix):
            continue
        htd = by_name.get(d_lbl[len(local_prefix) :])
        if not htd or htd["rule"] not in ("host_test_data_files", "host_test_data_map"):
            continue
        hkws = htd.get("keywords", {})
        for raw in pcl._extract_ast_strings(hkws.get("srcs"), var_table):
            c = _canon_dep_label(raw, pkg_rel)
            if c:
                data_deps.add(_norm_vendor_crate_label(c))
        fmap = hkws.get("files_map")
        if isinstance(fmap, ast.Dict):
            for vnode in fmap.values:
                for raw in pcl._extract_ast_strings(vnode, var_table):
                    c = _canon_dep_label(raw, pkg_rel)
                    if c:
                        data_deps.add(_norm_vendor_crate_label(c))
    return code_deps, data_deps, overwrites, host_test_misplaced_data - data_deps


_GENERIC_TEST_DATA_SEGMENTS = {
    "test_data",
    "testdata",
    "data",
    "host_test_data",
    "keys",
    "repository",
    "blobs",
    "meta",
    "bin",
    "lib",
}
_pkg_source_blob_cache = {}
_htd_tokens_cache = {}


# Add non-generic staged directory path tokens for host_test_data consumption checks.
def _add_test_data_dir_tokens(tokens, raw_dir):
    dd = (raw_dir or "").strip().strip("/")
    if not dd:
        return
    tokens.add(dd)
    parent = os.path.dirname(dd)
    if parent and parent not in _GENERIC_TEST_DATA_SEGMENTS:
        tokens.add(parent)
    for seg in dd.split("/"):
        if seg and seg not in _GENERIC_TEST_DATA_SEGMENTS:
            tokens.add(seg)


# Read all non-BUILD source files in package `pkg_rel` to verify test_data consumption.
def _package_non_build_source_blob(wdir, pkg_rel):
    key = (wdir, pkg_rel)
    if key not in _pkg_source_blob_cache:
        chunks = []
        root = os.path.join(wdir, pkg_rel) if pkg_rel else wdir
        if os.path.isdir(root):
            for dirpath, dirnames, filenames in os.walk(root):
                dirnames[:] = [
                    dn
                    for dn in dirnames
                    if not dn.startswith(".")
                    and not os.path.isfile(os.path.join(dirpath, dn, "BUILD.bazel"))
                ]
                for fn in filenames:
                    if fn in ("BUILD.bazel", "BUILD.gn", "BUILD"):
                        continue
                    try:
                        with open(
                            os.path.join(dirpath, fn),
                            encoding="utf-8",
                            errors="ignore",
                        ) as sf:
                            chunks.append(sf.read())
                    except OSError:
                        pass
        _pkg_source_blob_cache[key] = "\n".join(chunks)
    return _pkg_source_blob_cache[key]


# Collect staged path tokens for a host_test_data target in BUILD.bazel or BUILD.gn.
def _host_test_data_target_tokens(wdir, base_rev, canon_lbl):
    if not canon_lbl or not canon_lbl.startswith("//") or not wdir:
        return None
    key = (wdir, base_rev, canon_lbl)
    if key in _htd_tokens_cache:
        return _htd_tokens_cache[key]
    dep_pkg, _, dep_name = canon_lbl[2:].partition(":")
    if not dep_pkg or not dep_name:
        _htd_tokens_cache[key] = None
        return None
    tokens = set()
    is_htd = False
    bz_path = os.path.join(wdir, dep_pkg, "BUILD.bazel")
    if os.path.isfile(bz_path):
        try:
            with open(bz_path, encoding="utf-8") as bf:
                bz_text = bf.read()
        except OSError:
            bz_text = ""
        if bz_text:
            var_table = _parse_bazel_var_table(bz_text, filename=bz_path)
            for bt in pcl.extract_bazel_targets(bz_text, filename=bz_path):
                if bt["name"] == dep_name and bt["rule"] in (
                    "host_test_data_files",
                    "host_test_data_map",
                ):
                    is_htd = True
                    kws = bt.get("keywords", {})
                    for dd in pcl._extract_ast_strings(
                        kws.get("dest_dir"), var_table
                    ):
                        _add_test_data_dir_tokens(tokens, dd)
                    for src in pcl._extract_ast_strings(kws.get("srcs"), var_table):
                        base = os.path.basename(src.split(":", 1)[-1]).strip()
                        if base and not tokens:
                            tokens.add(base)
    if not is_htd:
        gn_text = _read_dep_pkg_gn(wdir, dep_pkg, base_rev)
        for gt in pcl.extract_gn_targets(gn_text):
            if gt["name"] == dep_name and gt["tmpl"] == "host_test_data":
                is_htd = True
                body = gt["clean_body"]
                for om in re.finditer(r"(?<![\w.])outputs\s*\+?=\s*\[", body):
                    close_idx = pcl.find_closing_delim(body, om.end() - 1, "[", "]")
                    for raw_out in re.findall(
                        r'"((?:[^"\\]|\\.)*)"', body[om.end() : close_idx]
                    ):
                        cleaned = re.sub(
                            r"^\$\{?root_(?:out|build)_dir\}?/+", "", raw_out.strip()
                        )
                        cleaned = re.sub(r"/+\{\{[^}]+\}\}.*$", "", cleaned)
                        _add_test_data_dir_tokens(tokens, cleaned)
    res = tokens if (is_htd and tokens) else None
    _htd_tokens_cache[key] = res
    return res


# Check whether canon_lbl is a host_test_data target whose staged paths are unused in pkg_rel.
def _is_unused_host_test_data_in_pkg(wdir, base_rev, pkg_rel, canon_lbl):
    tokens = _host_test_data_target_tokens(wdir, base_rev, canon_lbl)
    if not tokens:
        return False
    src_blob = _package_non_build_source_blob(wdir, pkg_rel)
    return not any(tok in src_blob for tok in tokens)


# Check whether test_name was an explicit standalone `rustc_test("<test_name>")` in base
# BUILD.gn that omitted unused GN template `test_deps` from `<lib_name>`.
def _is_explicit_gn_rustc_test_omitting_template_deps(
    wdir, base_rev, pkg_rel, lib_name, test_name, gn_canon
):
    if not wdir or not pkg_rel or test_name == f"{lib_name}_test":
        return False
    base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
    if not base_gn:
        return False
    blocks = [
        gt
        for gt in pcl.extract_gn_targets(base_gn)
        if gt["name"] == test_name and gt["tmpl"] == "rustc_test"
    ]
    if len(blocks) != 1:
        return False
    test_body_norms = {
        _norm_vendor_crate_label(c)
        for raw in re.findall(r'"((?:[^"\\]|\\.)*)"', blocks[0]["clean_body"])
        for c in (_canon_dep_label(raw, pkg_rel),)
        if c
    }
    return all(_norm_vendor_crate_label(g) not in test_body_norms for g in gn_canon)


# Check whether canon_lbl is a non-crate GN package/component/resource target that
# cannot be added to Bazel rustc_test(deps = [...]).
def _is_non_crate_gn_pkg_or_component_target(wdir, base_rev, canon_lbl):
    if not canon_lbl or not canon_lbl.startswith("//") or ":" not in canon_lbl:
        return False
    dep_pkg, dep_name = canon_lbl[2:].split(":", 1)
    if not dep_pkg or not dep_name or _is_bazel_rust_crate_target(wdir, canon_lbl):
        return False
    gn_text = _read_dep_pkg_gn(wdir, dep_pkg, base_rev)
    for gt in pcl.extract_gn_targets(gn_text):
        if gt["name"] == dep_name and gt["tmpl"] in (
            "fuchsia_package",
            "fuchsia_package_with_single_component",
            "fuchsia_component",
            "fuchsia_test_component",
            "fuchsia_component_manifest",
            "fuchsia_structured_config_values",
            "resource",
            "resource_group",
            "resource_tree",
        ):
            return True
    return False


# Check whether unit_test_parity ERROR/WARNING is a false positive on non-Rust
# runtime/compile test data (`data`/`test_data`/`compile_data`), mapped test deps,
# explicit standalone GN `rustc_test` targets, non-crate GN package/component test_deps,
# or unused GN `host_test_data` `test_deps`.
def is_verified_unit_test_parity_false_positive(finding, wdir="", base_rev=""):
    """Returns (test_name, kind) when reported GN test_deps are carried on test_name."""
    if finding.get("category") != "unit_test_parity":
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = finding.get("file", "")
    if not rel_bz.endswith("BUILD.bazel") or not wdir:
        return None
    msg = finding.get("message", "")
    m_cf = UNIT_TEST_CRATE_FEATURES_MSG_RE.match(msg)
    if m_cf:
        _, test_name, raw_feats = m_cf.group(1), m_cf.group(2), m_cf.group(3)
        req_feats = [s.strip().strip("'\"") for s in raw_feats.split(",") if s.strip().strip("'\"")]
        bz_path = os.path.join(wdir, rel_bz)
        if not req_feats or not os.path.isfile(bz_path):
            return None
        try:
            bz_text = open(bz_path, encoding="utf-8").read()
        except OSError:
            return None
        for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz):
            if bt["name"] == test_name and "crate_features" in bt.get("attrs", ()):
                cf_node = bt.get("keywords", {}).get("crate_features")
                found_feats = {
                    sub.value
                    for sub in ast.walk(cf_node)
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
                } if cf_node is not None else set()
                if set(req_feats) <= found_feats:
                    return (test_name, "crate_features_expr")
        return None
    m = UNIT_TEST_PARITY_MSG_RE.match(msg)
    if not m:
        return None
    lib_name, raw_list, test_name = m.group(1), m.group(2), m.group(3)
    pkg_rel = os.path.dirname(rel_bz).strip("/")
    gn_items = [s.strip() for s in raw_list.split(",") if s.strip()]
    gn_canon = [_canon_dep_label(s, pkg_rel) for s in gn_items]
    if not gn_canon or None in gn_canon:
        return None
    ctx = _collect_unit_test_bazel_context(wdir, rel_bz, lib_name, test_name, pkg_rel)
    if ctx is None:
        return None
    code_deps, data_deps, overwrites, misplaced_data = ctx
    if any(_norm_vendor_crate_label(g) in misplaced_data for g in gn_canon):
        return (test_name, "data_not_test_data")
    if _is_explicit_gn_rustc_test_omitting_template_deps(
        wdir, base_rev, pkg_rel, lib_name, test_name, gn_canon
    ):
        return (test_name, "explicit_gn_rustc_test")
    stop_set = code_deps | data_deps
    used_runtime_data = False
    dropped_unused_test_data = False
    dropped_non_crate_gn_target = False
    for g_lbl in gn_canon:
        g_norm = _norm_vendor_crate_label(g_lbl)
        if g_norm in code_deps:
            continue
        if g_norm in data_deps and not _is_bazel_rust_crate_target(wdir, g_lbl):
            used_runtime_data = True
            continue
        if g_norm in overwrites:
            ow_targets = overwrites[g_norm]
            if ow_targets & code_deps:
                continue
            ow_data = ow_targets & data_deps
            if ow_data and any(not _is_bazel_rust_crate_target(wdir, b) for b in ow_data):
                used_runtime_data = True
                continue
        if _is_unused_host_test_data_in_pkg(wdir, base_rev, pkg_rel, g_lbl):
            dropped_unused_test_data = True
            continue
        if _is_non_crate_gn_pkg_or_component_target(wdir, base_rev, g_lbl):
            dropped_non_crate_gn_target = True
            continue
        exp = _expand_pure_gn_forwarding_group(g_lbl, wdir, base_rev, stop_set)
        if not exp:
            return None
        for e_lbl in exp:
            if e_lbl in code_deps:
                continue
            if e_lbl in data_deps and not _is_bazel_rust_crate_target(wdir, e_lbl):
                used_runtime_data = True
                continue
            if _is_unused_host_test_data_in_pkg(wdir, base_rev, pkg_rel, e_lbl):
                dropped_unused_test_data = True
                continue
            if _is_non_crate_gn_pkg_or_component_target(wdir, base_rev, e_lbl):
                dropped_non_crate_gn_target = True
                continue
            return None
    if dropped_non_crate_gn_target:
        return (test_name, "non_crate_gn_target")
    if dropped_unused_test_data:
        return (test_name, "unused_host_test_data")
    return (test_name, "runtime_data" if used_runtime_data else "code_or_forwarding")


# Check whether sentinel_drift WARNING is a false positive on a BUILD.gn that
# retains only GN-only bridge/group targets with no GN consumers of BUILD.bazel
# convertible targets.
GN_ONLY_SENTINEL_FREE_TEMPLATES = {
    "bazel_ffx_tool",
    "bazel_fuchsia_package",
    "bazel_host_tool",
    "bazel_test_package",
    "bazel_test_suite",
    "group",
    "host_test_data",
    "install_host_tools",
    "sdk_atom",
    "sdk_host_tool",
}
GN_BAZEL_BRIDGE_TEMPLATES = {
    "bazel_ffx_tool",
    "bazel_fuchsia_package",
    "bazel_host_tool",
    "bazel_test_package",
    "bazel_test_suite",
}


# Read a repo-relative file at `base_rev` from git, returning "" when absent.
def _read_git_file_at_base(wdir, rel_path, base_rev=""):
    if not wdir or not rel_path:
        return ""
    ref = (base_rev or "HEAD").strip()
    try:
        return subprocess.check_output(
            ["git", "-C", wdir, "show", f"{ref}:{rel_path.strip('/')}"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return ""


# Check whether `pkg_rel` is a pre-existing Bazel package outside `target_dirs`
# whose `BUILD.bazel` added no new non-`test_suite` targets in this change.
def _is_collateral_preexisting_bazel_pkg(pkg_rel, cur_bz_text, wdir, base_rev=""):
    pkg_clean = (pkg_rel or "").strip("/")
    if pkg_clean in target_dirs:
        return False, ""
    rel_bz = f"{pkg_clean}/BUILD.bazel" if pkg_clean else "BUILD.bazel"
    base_bz = _read_git_file_at_base(wdir, rel_bz, base_rev)
    if not base_bz.strip():
        return False, ""
    base_pairs = {
        (bt["rule"], bt["name"])
        for bt in pcl.extract_bazel_targets(base_bz, filename=rel_bz)
    }
    cur_non_suite = {
        (bt["rule"], bt["name"])
        for bt in pcl.extract_bazel_targets(cur_bz_text, filename=rel_bz)
        if bt["rule"] not in ("test_suite", "exports_files")
    }
    if not (cur_non_suite <= base_pairs):
        return False, ""
    return True, base_bz


# Canonicalize a named top-level Starlark target call, ignoring collateral
# `visibility` and `HOST_CONSTRAINTS` <-> `HOST_OS_CONSTRAINTS` edits.
def _canon_collateral_bazel_target(bz_text, tname, filename="<BUILD.bazel>"):
    try:
        tree = ast.parse(bz_text, filename=filename)
    except SyntaxError:
        return None
    matches = []
    for stmt in tree.body:
        if not (
            isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)
        ):
            continue
        call = stmt.value
        fn = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (
                call.func.attr if isinstance(call.func, ast.Attribute) else ""
            )
        )
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        nm = kws.get("name")
        if not (
            isinstance(nm, ast.Constant)
            and isinstance(nm.value, str)
            and nm.value == tname
        ):
            continue
        norm_kws = []
        for k, v in sorted(kws.items()):
            if k == "visibility":
                continue
            if (
                k == "target_compatible_with"
                and isinstance(v, ast.Name)
                and v.id in ("HOST_CONSTRAINTS", "HOST_OS_CONSTRAINTS")
            ):
                norm_kws.append((k, "HOST_COMPAT"))
            else:
                norm_kws.append((k, ast.dump(v)))
        matches.append((fn, [ast.dump(a) for a in call.args], norm_kws))
    return matches if matches else None


COLLATERAL_TARGET_FP_CATEGORIES = {
    "attribute_alignment",
    "dep_alignment",
    "host_compatibility",
    "missing_sources",
    "unit_test_parity",
}
TARGET_HEAD_MSG_RE = re.compile(r'^(?:Target|Unit tests of) "([^"]+)"')


# Check whether a target-level review_diff finding flags an untouched pre-existing
# target in a collateral-modified non-target BUILD.bazel package.
def is_preexisting_collateral_target_false_positive(
    finding, wdir="", base_rev=""
):
    if finding.get("category") not in COLLATERAL_TARGET_FP_CATEGORIES:
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = finding.get("file", "")
    if (
        not (rel_bz == "BUILD" or rel_bz.endswith(("BUILD.bazel", "/BUILD")))
        or not wdir
    ):
        return None
    m = TARGET_HEAD_MSG_RE.match(finding.get("message", ""))
    if not m:
        return None
    tname = m.group(1)
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return None
    try:
        with open(bz_path, encoding="utf-8") as bf:
            cur_bz = bf.read()
    except OSError:
        return None
    pkg_rel = os.path.dirname(rel_bz).strip("/")
    is_col, base_bz = _is_collateral_preexisting_bazel_pkg(
        pkg_rel, cur_bz, wdir, base_rev
    )
    if not is_col:
        return None
    base_canon = _canon_collateral_bazel_target(
        base_bz, tname, filename=rel_bz
    )
    cur_canon = _canon_collateral_bazel_target(cur_bz, tname, filename=rel_bz)
    if not base_canon or base_canon != cur_canon:
        return None
    return tname


def is_verified_sentinel_drift_false_positive(finding, wdir="", base_rev=""):
    """Returns reason kind when BUILD.gn intentionally omits BAZEL2GN SENTINEL."""
    if finding.get("category") != "sentinel_drift":
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_gn = finding.get("file", "")
    if not rel_gn.endswith("BUILD.gn") or not wdir:
        return None
    gn_path = os.path.join(wdir, rel_gn)
    pkg_dir = os.path.dirname(gn_path)
    bz_path = os.path.join(pkg_dir, "BUILD.bazel")
    if not (os.path.isfile(gn_path) and os.path.isfile(bz_path)):
        return None
    try:
        with open(gn_path, encoding="utf-8") as gf:
            gn_text = gf.read()
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return None
    if pcl.has_sentinel(gn_text):
        return None
    pkg_rel = os.path.dirname(rel_gn).strip("/")
    gt_list = pcl.extract_gn_targets(gn_text)
    if gt_list and all(
        gt["tmpl"] in GN_ONLY_SENTINEL_FREE_TEMPLATES for gt in gt_list
    ):
        bridge_names = {
            gt["name"]
            for gt in gt_list
            if gt["tmpl"] in GN_BAZEL_BRIDGE_TEMPLATES
        }
        if bridge_names:
            bridge_derived = set(bridge_names)
            for bname in bridge_names:
                bridge_derived.add(f"{bname}_host_tool")
                bridge_derived.add(f"{bname}_versioned")
            bz_targets = pcl.extract_bazel_targets(bz_text, filename=bz_path)
            non_bridge_bz_names = {
                bt["name"]
                for bt in bz_targets
                if bt["name"] not in bridge_derived
            }
            has_local_ref = False
            for gt in gt_list:
                if gt["tmpl"] in GN_BAZEL_BRIDGE_TEMPLATES:
                    continue
                for bname in non_bridge_bz_names:
                    pat = (
                        r'"(?::|//'
                        + re.escape(pkg_rel)
                        + r":)"
                        + re.escape(bname)
                        + r'(?:"|\()'
                    )
                    if re.search(pat, gt["clean_body"]):
                        has_local_ref = True
                        break
                if has_local_ref:
                    break
            if not has_local_ref:
                return "gn_only_bridge"
    is_col, _ = _is_collateral_preexisting_bazel_pkg(
        pkg_rel, bz_text, wdir, base_rev
    )
    if is_col:
        base_gn = _read_git_file_at_base(wdir, rel_gn, base_rev)
        if base_gn and not pcl.has_sentinel(base_gn):
            base_gn_names = {
                gt["name"] for gt in pcl.extract_gn_targets(base_gn)
            }
            new_gn_tmpls = {
                gt["tmpl"]
                for gt in gt_list
                if gt["name"] not in base_gn_names
            }
            if new_gn_tmpls <= {"bazel_test_suite", "group"}:
                return "collateral_non_sentinel_pkg"
    return None


# Normalize a GN or Bazel file path or label to a repo-relative path without `//`.
def _norm_repo_file_path(raw, pkg_rel=""):
    s = (raw or "").strip().strip('"').strip("'")
    if not s or "$" in s:
        return ""
    if s.startswith("@//"):
        s = s[1:]
    if s.startswith("//"):
        body = s[2:].replace(":", "/")
        return os.path.normpath(body).replace("\\", "/").lstrip("/")
    if s.startswith(":"):
        s = s[1:]
    joined = os.path.join(pkg_rel, s) if pkg_rel else s
    return os.path.normpath(joined).replace("\\", "/").lstrip("/")


# Extract string literal list items assigned to `attr_name` inside a GN target body.
def _extract_gn_target_list_strings(clean_body, attr_name):
    items = []
    for m in re.finditer(rf"(?<![\w.]){re.escape(attr_name)}\s*\+?=\s*\[", clean_body):
        close_idx = pcl.find_closing_delim(clean_body, m.end() - 1, "[", "]")
        items.extend(
            re.findall(r'"((?:[^"\\]|\\.)*)"', clean_body[m.end() : close_idx])
        )
    return items


# Collect repo-relative paths and basenames staged in test_data or host_test_data_* in BUILD.bazel.
def _collect_bazel_test_data_files(bz_text, pkg_rel, filename="<BUILD.bazel>"):
    var_table = _parse_bazel_var_table(bz_text, filename=filename)
    paths = set()
    basenames = set()
    for bt in pcl.extract_bazel_targets(bz_text, filename=filename):
        kws = bt.get("keywords", {})
        attrs = (
            ("srcs",)
            if bt["rule"] in ("host_test_data_files", "host_test_data_map")
            else ("test_data",)
        )
        for attr in attrs:
            for raw in pcl._extract_ast_strings(kws.get(attr), var_table):
                np = _norm_repo_file_path(raw, pkg_rel)
                if np:
                    paths.add(np)
                    basenames.add(os.path.basename(np))
    return paths, basenames


# Check whether missing_sources ERROR/WARNING only reports base GN `inputs` entries
# that are runtime test fixtures omitted from `compile_data`.
def is_verified_missing_sources_false_positive(finding, wdir="", base_rev=""):
    """Returns target_name when missing GN files are runtime test fixtures from `inputs`."""
    if finding.get("category") != "missing_sources":
        return None
    if finding.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = finding.get("file", "")
    if not rel_bz.endswith("BUILD.bazel") or not wdir:
        return None
    m = MISSING_SOURCES_MSG_RE.match(finding.get("message", ""))
    if not m:
        return None
    tname, raw_list = m.group(1), m.group(2)
    missing_items = [s.strip() for s in raw_list.split(",") if s.strip()]
    if not missing_items:
        return None
    pkg_rel = os.path.dirname(rel_bz).strip("/")
    base_gn = _read_base_pkg_gn(wdir, pkg_rel, base_rev)
    if not base_gn:
        return None
    gt_blocks = [gt for gt in pcl.extract_gn_targets(base_gn) if gt["name"] == tname]
    if not gt_blocks:
        return None
    gn_inputs = set()
    gn_real_srcs = set()
    for gt in gt_blocks:
        for s in _extract_gn_target_list_strings(gt["clean_body"], "inputs"):
            np = _norm_repo_file_path(s, pkg_rel)
            if np:
                gn_inputs.add(np)
        for src_attr in ("sources", "args_sources", "public"):
            for s in _extract_gn_target_list_strings(gt["clean_body"], src_attr):
                np = _norm_repo_file_path(s, pkg_rel)
                if np:
                    gn_real_srcs.add(np)
    if not gn_inputs:
        return None
    bz_path = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_path):
        return None
    try:
        with open(bz_path, encoding="utf-8") as bf:
            bz_text = bf.read()
    except OSError:
        return None
    bz_targets = [
        bt
        for bt in pcl.extract_bazel_targets(bz_text, filename=rel_bz)
        if bt["name"] == tname
    ]
    if not bz_targets or any("rustc_env" in bt["attrs"] for bt in bz_targets):
        return None
    td_paths, td_basenames = _collect_bazel_test_data_files(
        bz_text, pkg_rel, filename=rel_bz
    )
    src_blob = _package_non_build_source_blob(wdir, pkg_rel)
    for item in missing_items:
        np = _norm_repo_file_path(item, pkg_rel)
        base = os.path.basename(np)
        if not np or not base or np in gn_real_srcs or np not in gn_inputs:
            return None
        if not (
            TEST_DATA_PATH_RE.search(item) or np in td_paths or base in td_basenames
        ):
            return None
        inc_pat = re.compile(
            r"include(?:_(?:str|bytes))?!\s*\([^)]*" + re.escape(base), re.S
        )
        if inc_pat.search(src_blob) or (
            "concat!" in src_blob
            and re.search(r"include(?:_(?:str|bytes))?!", src_blob)
            and base in src_blob
        ):
            return None
    return tname
# Check whether a review_diff/missing_sources finding only names GN source paths that are
# explicitly mapped via `# @bazel2gn:path_overwrite:<gn_path>` in `BUILD.bazel`.
def is_path_overwrite_missing_sources_false_positive(f, wdir):
    if f.get("category") != "missing_sources":
        return None
    if f.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = (f.get("file") or "").strip("/")
    msg = (f.get("message") or "").strip()
    if os.path.basename(rel_bz) != "BUILD.bazel":
        return None
    m = MISSING_SOURCES_MSG_RE.match(msg)
    if not m:
        return None
    tname = m.group(1)
    req_srcs = [s.strip() for s in m.group(2).split(",") if s.strip()]
    if not req_srcs:
        return None
    bz_full = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_full):
        return None
    try:
        src_text = open(bz_full, encoding="utf-8").read()
        src_lines = src_text.splitlines()
        tree = ast.parse(src_text, filename=bz_full)
    except Exception:
        return None
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        cname = ""
        has_srcs = False
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                cname = kw.value.value
            elif kw.arg == "srcs":
                has_srcs = True
        if cname != tname or not has_srcs:
            continue
        end_ln = getattr(stmt, "end_lineno", stmt.lineno) or stmt.lineno
        chunk = "\n".join(src_lines[max(0, stmt.lineno - 1) : end_ln])
        overwrites = {
            ow.group(1).strip()
            for ow in re.finditer(r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", chunk)
        }
        if all(r in overwrites for r in req_srcs):
            return tname
    return None


# Check whether a review_diff/template_parity finding is a verified C++ test binary,
# test package, or test-component rule mapping.
def is_verified_template_parity_false_positive(f, wdir):
    """Returns (target_name, kind) when a template_parity finding matches a valid Bazel rule mapping."""
    if f.get("category") != "template_parity":
        return None
    if f.get("severity") not in ("ERROR", "WARNING"):
        return None
    rel_bz = (f.get("file") or "").strip("/")
    msg = f.get("message") or ""
    if os.path.basename(rel_bz) != "BUILD.bazel":
        return None
    m = re.match(r'^Target "([^"]+)"', msg)
    if not m:
        return None
    tname = m.group(1)
    bz_full = os.path.join(wdir, rel_bz)
    if not os.path.isfile(bz_full):
        return None
    try:
        tree = ast.parse(open(bz_full, encoding="utf-8").read(), filename=bz_full)
    except Exception:
        return None
    pkg_dir = os.path.dirname(rel_bz).strip("/")
    cand_refs = {tname, f":{tname}"}
    if pkg_dir:
        cand_refs.add(f"//{pkg_dir}:{tname}")
    rule_of = {}
    has_testonly = set()
    has_no_fat_lto = set()
    test_comp_refs = set()
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        fn = call.func.id if isinstance(call.func, ast.Name) else ""
        cname = ""
        is_to = False
        is_no_fat_lto = False
        tcomps = set()
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                cname = kw.value.value
            elif (
                kw.arg == "testonly"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value is True
            ):
                is_to = True
            elif kw.arg == "build_flags":
                for sub in ast.walk(kw.value):
                    if (
                        isinstance(sub, ast.Constant)
                        and sub.value == "//build/config/lto:no-fat-lto-objects-linking"
                    ):
                        is_no_fat_lto = True
            elif kw.arg == "test_components":
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        tcomps.add(sub.value)
        if cname:
            rule_of[cname] = fn
            if is_to:
                has_testonly.add(cname)
            if is_no_fat_lto:
                has_no_fat_lto.add(cname)
        if fn == "fx_package":
            test_comp_refs.update(tcomps)
    t_rule = rule_of.get(tname)
    if (
        t_rule == "fx_cc_test_binary"
        and "cc_test_executable" in msg
        and ("was test(" in msg or "was executable(" in msg)
    ):
        return (tname, "cc_test_executable")
    if (
        t_rule == "fx_cc_binary"
        and tname in has_testonly
        and tname in has_no_fat_lto
        and "generated executable(" in msg
        and "was cc_test_executable(" in msg
    ):
        return (tname, "cc_test_binary_lto")
    if (
        t_rule in ("fx_package", "fx_unittest_package")
        and "generated fuchsia_test_package(" in msg
        and "was fuchsia_unittest_package(" in msg
    ):
        return (tname, "unittest_package")
    if "fuchsia_test_component" not in msg or (
        "fuchsia_component" not in msg
        and "fuchsia_unittest_component" not in msg
    ):
        return None
    in_test_comps = bool(cand_refs & test_comp_refs)
    if t_rule == "fx_test_component" and (
        in_test_comps or "was fuchsia_unittest_component(" in msg
    ):
        return (tname, "test_root")
    if (
        t_rule == "fx_component"
        and tname in has_testonly
        and not in_test_comps
    ):
        return (tname, "test_helper")
    return None


GN_RETENTION_DROPPED_COMMENT_RE = re.compile(
    r"\b(?:(?:retained|kept|stay(?:s|ing)?|remain(?:s|ing)?)\s+in\s+(?:gn|build\.gn)"
    r"|keep\b[^\n]*\bin\s+(?:gn|build\.gn))\b",
    re.I,
)


# Collect (file, target_name) pairs whose review_diff/dropped_comment findings
# come from an obsolete "Retained in GN / stay in GN" comment block on a migrated target.
def _collect_gn_retention_dropped_comment_targets(diff_findings, wdir):
    candidates = set()
    for f in diff_findings:
        if f.get("category") != "dropped_comment":
            continue
        if f.get("severity") not in ("ERROR", "WARNING"):
            continue
        msg = f.get("message") or ""
        m = re.match(r'^Target "([^"]+)"', msg)
        if m and GN_RETENTION_DROPPED_COMMENT_RE.search(msg):
            candidates.add((f.get("file", ""), m.group(1)))
    verified = set()
    for rel_bz, tname in candidates:
        if not rel_bz.endswith("BUILD.bazel") or not wdir:
            continue
        bz_full = os.path.join(wdir, rel_bz)
        gn_full = os.path.join(wdir, os.path.dirname(rel_bz), "BUILD.gn")
        if not os.path.isfile(gn_full):
            verified.add((rel_bz, tname))
            continue
        if os.path.isfile(bz_full):
            try:
                bz_txt = open(bz_full, encoding="utf-8").read()
            except OSError:
                bz_txt = ""
            if re.search(r'name\s*=\s*"' + re.escape(tname) + r'"', bz_txt):
                verified.add((rel_bz, tname))
    return verified


# Annotate verified review_diff false positives with accurate dispute instructions.
def annotate_review_diff_macro_false_positives(diff_findings, wdir, base_rev=""):
    """Updates remediation on verified false positives; returns (items, finding_ids)."""
    dispute_items = []
    seen_items = set()
    fp_ids = set()
    gn_retention_dropped = _collect_gn_retention_dropped_comment_targets(
        diff_findings, wdir
    )
    for f in diff_findings:
        f_path = f.get("file", "")
        f_cat = f.get("category", "")
        matched_dropped_comment = None
        if f_cat == "dropped_comment" and f.get("severity") in ("ERROR", "WARNING"):
            m_dc = re.match(r'^Target "([^"]+)"', f.get("message") or "")
            if m_dc and (f_path, m_dc.group(1)) in gn_retention_dropped:
                matched_dropped_comment = m_dc.group(1)
        # Evaluate verifiers in priority order; each runs only if no earlier one matched.
        prior = matched_dropped_comment
        matched_attr = (
            None if prior else is_macro_synced_attr_false_positive(f, wdir, base_rev)
        )
        prior = prior or matched_attr
        matched_host = (
            None if prior else is_host_only_macro_compat_false_positive(f, wdir)
        )
        prior = prior or matched_host
        matched_dep = (
            None if prior else is_verified_dep_alignment_false_positive(f, wdir, base_rev)
        )
        prior = prior or matched_dep
        matched_unit_test = (
            None
            if prior
            else is_verified_unit_test_parity_false_positive(f, wdir, base_rev)
        )
        prior = prior or matched_unit_test
        matched_sentinel = (
            None
            if prior
            else is_verified_sentinel_drift_false_positive(f, wdir, base_rev)
        )
        prior = prior or matched_sentinel
        matched_tmpl = (
            None if prior else is_verified_template_parity_false_positive(f, wdir)
        )
        prior = prior or matched_tmpl
        matched_missing_src = (
            None
            if prior
            else is_verified_missing_sources_false_positive(f, wdir, base_rev)
        )
        prior = prior or matched_missing_src
        matched_path_ow_srcs = (
            None
            if prior
            else is_path_overwrite_missing_sources_false_positive(f, wdir)
        )
        prior = prior or matched_path_ow_srcs
        matched_preexisting = (
            None
            if prior
            else is_preexisting_collateral_target_false_positive(f, wdir, base_rev)
        )
        if matched_dropped_comment:
            tname = matched_dropped_comment
            f["remediation"] = (
                f'Target "{tname}" in `{f_path}` is now migrated to `BUILD.bazel`, so '
                "its transitional GN-retention comment from `BUILD.gn` is obsolete "
                "(carrying it into `BUILD.bazel` is rejected by "
                "`cc_header_deps_and_annotations/stale_gn_retention_comment_in_bazel`). "
                "Once all deterministic check scripts pass, dispute this "
                "`review_diff/dropped_comment` false positive in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "dropped_comment"`, `file: "{f_path}"`) using a short, '
                "factual justification addressed to the human reviewer."
            )
        elif matched_path_ow_srcs:
            tname = matched_path_ow_srcs
            f["remediation"] = (
                f'Target "{tname}" in `{f_path}` maps every listed GN source path via '
                "`# @bazel2gn:path_overwrite:` in `srcs` and generates matching GN "
                "`sources` below `## BAZEL2GN SENTINEL`. Once all deterministic check "
                "scripts pass, dispute this `review_diff/missing_sources` false positive "
                'in `CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "missing_sources"`, `file: "{f_path}"`) using a short, '
                "factual justification addressed to the human reviewer."
            )
        elif matched_attr:
            tname, gn_attr, attr_kind = matched_attr
            if attr_kind == "companion_manifest":
                f["remediation"] = (
                    f'`fx_component` / `fx_test_component` does not accept `{gn_attr}` '
                    f'in Bazel; `{gn_attr}` is set on the companion '
                    f'`fx_component_manifest` target in `{f_path}`. Once all '
                    "deterministic check scripts pass, dispute this `review_diff` false "
                    'positive in `CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`) using a '
                    "short, factual justification addressed to the human reviewer "
                    '(e.g., `"fx_component does not accept component_name in Bazel; it '
                    'is set on the companion fx_component_manifest target"`). Never put '
                    "instructions to other agents in `reason`."
                )
            elif attr_kind == "escaped_quote_flag":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` carries the exact `{gn_attr}` value '
                    "(the `review_diff` string parser truncated at the escaped quote "
                    '`\\"` in GN). Once all deterministic check scripts pass, dispute it '
                    'in `CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`) with a '
                    "short, factual justification addressed to the human reviewer. Never "
                    "put instructions to other agents in `reason`."
                )
            elif attr_kind == "rule_mapping":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` uses the standard Fuchsia Bazel '
                    f'rule mapping (`{gn_attr}`), which `gn_target_type_parity` '
                    "verifies and check scripts cannot waive in `review_diff`. Once "
                    "all deterministic check scripts pass, dispute this "
                    "`review_diff/attribute_alignment` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`) with a '
                    "short, factual justification addressed to the human reviewer. "
                    "Never put instructions to other agents in `reason`."
                )
            elif attr_kind == "local_gn_var":
                f["remediation"] = (
                    f'In `{f_path}`, `{gn_attr}` was a local GN helper variable '
                    "(not a standalone target parameter) whose value is already "
                    f'carried on "{tname}" (and preserved via `@bazel2gn` overwrite '
                    "when dual-built), so this `review_diff/attribute_alignment` "
                    "finding is a parser false positive that check scripts cannot "
                    "waive. Once all deterministic check scripts pass, dispute it in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`) with a '
                    "short, factual justification addressed to the human reviewer. Never "
                    "put instructions to other agents in `reason`."
                )
            elif attr_kind == "decomposed_unit_test":
                f["remediation"] = (
                    f'In `{f_path}`, GN `{gn_attr}` on "{tname}" is decomposed '
                    "into standalone `host_rustc_test`/`rustc_test` target(s) "
                    "(since the Bazel macro/rule does not accept inline unit-test "
                    "generation attributes), so this `review_diff/attribute_alignment` "
                    "WARNING is a parser false positive that check scripts cannot "
                    "waive. Once all deterministic check scripts pass, dispute it in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`).'
                )
            elif attr_kind == "rust_crate_hyphen":
                f["remediation"] = (
                    f'In `{f_path}`, GN `rustc_*` templates normalize hyphens to '
                    "underscores in crate and `rustc_test` binary names while Bazel "
                    f'`{gn_attr}` requires a valid Rust identifier without hyphens '
                    f'(which `bazel2gn` syncs back to GN `output_name`/`crate_name` '
                    f'on "{tname}"), so this `review_diff/attribute_alignment` WARNING '
                    "is a hyphen-normalization false positive that check scripts "
                    "cannot waive. Once all deterministic check scripts pass, dispute "
                    'it in `CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`).'
                )
            else:
                f["remediation"] = (
                    f'Target "{tname}" already carries `{gn_attr}` in `{f_path}` '
                    "(and below `## BAZEL2GN SENTINEL` when dual-built), so this "
                    "`review_diff/attribute_alignment` WARNING is a Starlark macro "
                    "parser false positive that check scripts cannot waive. Once all "
                    "deterministic check scripts pass, dispute it in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "attribute_alignment"`, `file: "{f_path}"`) with a '
                    "short, factual justification addressed to the human reviewer. Never "
                    "put instructions to other agents in `reason`."
                )
        elif matched_host:
            tname, rule_name = matched_host
            f["remediation"] = (
                f'Target "{tname}" uses host-only macro `{rule_name}`, which '
                "applies host platform constraints internally and does not take "
                "`target_compatible_with` (adding it breaks Bazel loading and "
                "`fx bazel2gn`). Do NOT add `target_compatible_with`; once all "
                "deterministic check scripts pass, dispute this `review_diff` "
                "false positive in `CoderReport.disputed_findings` "
                f'(`source: "review_diff"`, `category: "host_compatibility"`, '
                f'`file: "{f_path}"`).'
            )
        elif matched_dep:
            tname, dep_kind = matched_dep
            if dep_kind == "third_party_ext_repo":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` maps GN `//third_party/...` '
                    "dependency target(s) to their Bazel external repository "
                    "counterpart(s) from "
                    "`build/tools/bazel2gn/third_party_target_map.json` (which "
                    "`bazel2gn` translates back to GN). Once all deterministic "
                    "check scripts pass, dispute this `review_diff/dep_alignment` "
                    'false positive in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "dep_alignment"`, `file: "{f_path}"`).'
                )
            elif dep_kind == "slash_or_shorthand":
                f["remediation"] = (
                    f'Target "{tname}" has identical dependencies in GN and Bazel '
                    "after normalizing duplicate slashes (`//` inside package paths, "
                    "which Bazel rejects) and shorthand target labels. Once all "
                    "deterministic check scripts pass, dispute this "
                    "`review_diff/dep_alignment` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "dep_alignment"`, `file: "{f_path}"`).'
                )
            elif dep_kind == "raw_attribute":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` preserves the GN-only '
                    "toolchain dependency in `BUILD.gn` below `## BAZEL2GN SENTINEL` "
                    "via `# @bazel2gn:raw_attribute:`. Once all deterministic check "
                    "scripts pass, dispute this `review_diff/dep_alignment` false "
                    'positive in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "dep_alignment"`, `file: "{f_path}"`).'
                )
            elif dep_kind == "go_strict_deps":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` is a sentinel-synced Go target '
                    "whose `BUILD.bazel` `deps` use canonical package labels and list "
                    "strict direct Go package imports synced to `BUILD.gn` below "
                    "`## BAZEL2GN SENTINEL` via `fx bazel2gn`. Once all deterministic "
                    "check scripts pass, dispute this `review_diff/dep_alignment` "
                    "false positive in `CoderReport.disputed_findings` (`source: "
                    f'"review_diff"`, `category: "dep_alignment"`, `file: "{f_path}"`).'
                )
            elif dep_kind == "cross_lang_rust_dep":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` is a non-Rust C/C++ or '
                    "component/package target that omits a GN Rust crate dependency "
                    "(used in GN only for transitive `expect_includes` manifest "
                    "metadata, as Bazel non-Rust rules cannot depend on `CrateInfo` "
                    "targets). Once all deterministic check scripts pass, dispute this "
                    "`review_diff/dep_alignment` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "dep_alignment"`, `file: "{f_path}"`).'
                )
            else:
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` expands pure GN forwarding '
                    "group dependency target(s) directly to their underlying Bazel "
                    "target(s) (or maps them via `# @bazel2gn:path_overwrite:`), "
                    "which `gn_dep_parity` verifies and check scripts cannot waive "
                    "in `review_diff`. Once all deterministic check scripts pass, "
                    "dispute this `review_diff/dep_alignment` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "dep_alignment"`, `file: "{f_path}"`).'
                )
        elif matched_unit_test and matched_unit_test[1] == "data_not_test_data":
            # Not a false positive: keep the finding blocking and point at `test_data`.
            f["remediation"] = (
                f'Target "{matched_unit_test[0]}" in `{f_path}` lists these runtime test data '
                "targets (e.g. `host_test_data_files`/`host_test_data_map`) in `data`, which "
                "only reaches the inner `rustc_test` binary. Move them to `test_data` of the "
                "`host_rustc_test` (forwarded to its `host_test()` wrapper). `bazel2gn` cannot "
                "translate `test_data` on `host_rustc_test`, so mark the test "
                "`# @bazel2gn:skip` (migration_sanity T3) with a comment, run it from Bazel via "
                "the area's aggregation `test_suite()` (`host_tests`), and remove the GN "
                "`host_test_data` targets above the sentinel if no GN consumers remain. Do NOT "
                "dispute this finding."
            )
            continue
        elif matched_unit_test:
            test_name, ut_kind = matched_unit_test
            if ut_kind == "non_crate_gn_target":
                f["remediation"] = (
                    f'Target "{test_name}" in `{f_path}` intentionally omits '
                    "non-crate GN package/component/resource `test_deps` entry/entries "
                    "because Bazel `rustc_test(deps)` requires `CrateInfo` (and any "
                    "packaged test helper components belong on the test package target "
                    "rather than `rustc_test.deps`), which check scripts cannot waive "
                    "in `review_diff`. Once all deterministic check scripts pass, "
                    "dispute this `review_diff/unit_test_parity` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "unit_test_parity"`, `file: "{f_path}"`) using a '
                    "short, factual justification addressed to the human reviewer "
                    "(dropping any stale dispute entries for check scripts that now "
                    "pass)."
                )
            elif ut_kind == "unused_host_test_data":
                f["remediation"] = (
                    f'Target "{test_name}" in `{f_path}` intentionally omits unused '
                    "GN `host_test_data` `test_deps` entry/entries whose staged test "
                    "data paths are not referenced by any source or test file in the "
                    "package (adding them to `test_data` is rejected by "
                    "`test_data_attribute/unused_test_data`), which check scripts "
                    "cannot waive in `review_diff`. Once all deterministic check "
                    "scripts pass, dispute this `review_diff/unit_test_parity` false "
                    'positive in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "unit_test_parity"`, `file: '
                    f'"{f_path}"`) using a short, factual justification addressed to '
                    "the human reviewer."
                )
            elif ut_kind == "runtime_data":
                f["remediation"] = (
                    f'Target "{test_name}" in `{f_path}` already carries these '
                    "non-Rust test dependencies in `test_data`/`compile_data` "
                    "(mapped to `non_rust_deps`/`inputs` by `bazel2gn`; Rust `deps` "
                    "requires `CrateInfo` and rejects runtime data targets). Do NOT "
                    "move them to `deps`; once all deterministic check scripts pass, "
                    "dispute this `review_diff/unit_test_parity` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "unit_test_parity"`, `file: "{f_path}"`) using a '
                    "short, factual justification addressed to the human reviewer."
                )
            else:
                f["remediation"] = (
                    f'Target "{test_name}" in `{f_path}` already carries these GN '
                    "`test_deps` entries (via normalized Bazel labels, `link_deps`, "
                    "pure GN forwarding group expansion, or "
                    "`# @bazel2gn:path_overwrite:`), which check scripts cannot "
                    "waive in `review_diff`. Once all deterministic check scripts "
                    "pass, dispute this `review_diff/unit_test_parity` false "
                    'positive in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "unit_test_parity"`, `file: '
                    f'"{f_path}"`) using a short, factual justification addressed to '
                    "the human reviewer."
                )
        elif matched_tmpl:
            tname, tmpl_kind = matched_tmpl
            if tmpl_kind == "cc_test_executable":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` uses `fx_cc_test_binary` '
                    "in Bazel, which is the canonical rule for C++ test "
                    "binaries and causes `bazel2gn` to emit "
                    "`cc_test_executable()` below `## BAZEL2GN SENTINEL` "
                    "(verified by `gn_target_type_parity`). Once all "
                    "deterministic check scripts pass, dispute this "
                    "`review_diff/template_parity` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "template_parity"`, `file: "{f_path}"`).'
                )
            elif tmpl_kind == "cc_test_binary_lto":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` uses `fx_cc_binary` with '
                    "`testonly = True` and "
                    "`//build/config/lto:no-fat-lto-objects-linking`, which "
                    "causes `bazel2gn` to emit `executable()` below "
                    "`## BAZEL2GN SENTINEL` (verified by "
                    "`gn_target_type_parity`). Once all deterministic check "
                    "scripts pass, dispute this `review_diff/template_parity` "
                    'false positive in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "template_parity"`, `file: '
                    f'"{f_path}"`).'
                )
            elif tmpl_kind == "unittest_package":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` causes `bazel2gn` to emit '
                    "`fuchsia_test_package` below `## BAZEL2GN SENTINEL` "
                    "(verified by `gn_target_type_parity`). Once all "
                    "deterministic check scripts pass, dispute this "
                    "`review_diff/template_parity` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "template_parity"`, `file: "{f_path}"`).'
                )
            elif tmpl_kind == "test_root":
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` is a test root component '
                    "that requires `fx_test_component` in Bazel and causes "
                    "`bazel2gn` to emit `fuchsia_test_component` below the "
                    "sentinel. Once all deterministic check scripts pass, "
                    "dispute this `review_diff/template_parity` false positive "
                    'in `CoderReport.disputed_findings` (`source: '
                    f'"review_diff"`, `category: "template_parity"`, `file: '
                    f'"{f_path}"`).'
                )
            else:
                f["remediation"] = (
                    f'Target "{tname}" in `{f_path}` is a test helper component not '
                    "listed in `fx_package.test_components`, which requires "
                    "`fx_component(testonly = True)` in Bazel and causes `bazel2gn` "
                    "to emit `fuchsia_component` below the sentinel. Once all "
                    "deterministic check scripts pass, dispute this "
                    "`review_diff/template_parity` false positive in "
                    '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                    f'`category: "template_parity"`, `file: "{f_path}"`).'
                )
        elif matched_sentinel == "collateral_non_sentinel_pkg":
            f["remediation"] = (
                f"`{f_path}` is a pre-existing non-`bazel2gn` package outside "
                "the migrated target directories, modified only for collateral "
                "test or dependency wiring, so omitting `## BAZEL2GN SENTINEL` "
                "is expected. Do NOT run `fx bazel2gn` or add `# LINT.IfChange` "
                "to it; once all deterministic check scripts pass, dispute this "
                "`review_diff/sentinel_drift` false positive in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "sentinel_drift"`, `file: "{f_path}"`).'
            )
        elif matched_sentinel:
            f["remediation"] = (
                f"`{f_path}` retains only GN-only bridge/group targets (such as "
                "`bazel_ffx_tool`, `bazel_host_tool`, `bazel_fuchsia_package`, "
                "`bazel_test_suite`, `sdk_atom`, `host_test_data`, or `group`) with "
                "no GN consumers of convertible `BUILD.bazel` targets, so omitting "
                "`## BAZEL2GN SENTINEL` is required. Do NOT add `## BAZEL2GN SENTINEL` "
                "or `# LINT.IfChange`; once all deterministic check scripts pass, "
                "dispute this `review_diff/sentinel_drift` false positive in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "sentinel_drift"`, `file: "{f_path}"`).'
            )
        elif matched_missing_src:
            f["remediation"] = (
                f'Target "{matched_missing_src}" in `{f_path}` intentionally omits '
                "runtime test fixture(s) that were listed in GN `inputs` at the "
                "change base (adding runtime test fixtures to `compile_data` is "
                "rejected by `test_data_attribute/runtime_test_data_in_compile_data`; "
                "they belong on the test target via `test_data` / "
                "`host_test_data_*`), which check scripts cannot waive in "
                "`review_diff`. Once all deterministic check scripts pass, dispute "
                "this `review_diff/missing_sources` false positive in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "missing_sources"`, `file: "{f_path}"`).'
            )
        elif matched_preexisting:
            f["remediation"] = (
                f'Target "{matched_preexisting}" in `{f_path}` is a pre-existing '
                "target in a non-target package modified only for collateral test "
                "suite, visibility, or host constraint wiring, and is outside "
                "this migration's scope. Do NOT modify this pre-existing target; "
                "once all deterministic check scripts pass, dispute this "
                f"`review_diff/{f_cat}` false positive in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "{f_cat}"`, `file: "{f_path}"`).'
            )
        else:
            continue
        fp_ids.add(id(f))
        item_key = (f_cat, f_path)
        if f_cat and f_path and item_key not in seen_items:
            seen_items.add(item_key)
            dispute_items.append(item_key)
    # Remind coder to combine multiple verified false positives sharing (category, file).
    key_counts = {}
    for f in diff_findings:
        if id(f) in fp_ids:
            k = (f.get("category", ""), f.get("file", ""))
            key_counts[k] = key_counts.get(k, 0) + 1
    for f in diff_findings:
        if id(f) in fp_ids:
            k = (f.get("category", ""), f.get("file", ""))
            if key_counts.get(k, 0) >= 2:
                f["remediation"] = (
                    f"{f.get('remediation', '')} Multiple `review_diff/{k[0]}` "
                    f"false positives exist in `{k[1]}`; emit one combined "
                    "`CoderReport.disputed_findings` entry for this `(source, "
                    "category, file)` whose `reason` covers all of them."
                ).strip()
    return dispute_items, fp_ids


def precompute_review_diff():
    global _precomputed_diff_findings
    planter_bin = rl.find_planter_bin()
    if not planter_bin or not workdir:
        return
    env = dict(os.environ, PLANTER_BIN=planter_bin, PLANTER_WORKDIR=workdir,
               PLANTER_TARGET_DIRS=" ".join(target_dirs), PLANTER_CHANGE_BASE=change_base,
               PLANTER_RESOLVED_CHANGE_BASE=change_base, PLANTER_SKIP_FETCH="1",
               PLANTER_SKIP_DIFF_COVERAGE="1")
    for flag in ("--format=findings-json", "--check-findings"):
        out, _, err_text = execute([planter_bin, "diff", "--local", flag, f"--workdir={workdir}"], workdir, env, "review_diff")
        findings, parsed_ok = pcl.parse_check_findings(out.strip())
        if parsed_ok or (err_text is None and out.strip().startswith("[")):
            _precomputed_diff_findings = findings
            break
    if _precomputed_diff_findings is None:
        _precomputed_diff_findings = []
    try:
        tf = tempfile.NamedTemporaryFile("w", prefix="planter-review-diff-", suffix=".json", delete=False)
        json.dump(_precomputed_diff_findings, tf)
        tf.close()
        os.environ["PLANTER_REVIEW_DIFF_JSON"] = tf.name
        atexit.register(lambda p=tf.name: os.path.exists(p) and os.unlink(p))
    except OSError:
        pass


def run_once(spec, target_dir):
    script = spec["script_file"]
    if not os.path.isabs(script):
        for cand in (
            os.path.normpath(os.path.join(mroot, "checks", script)),
            os.path.normpath(os.path.join(project_dir, "checks", script)),
            os.path.normpath(os.path.join(mroot, script)),
            os.path.normpath(os.path.join(project_dir, script)),
        ):
            if os.path.exists(cand):
                script = cand
                break
        else:
            return [pcl.new_finding(
                source=spec["name"], category="check_script_missing_in_pin", severity="INFO",
                message=f"check script {json.dumps(spec['script_file'])} is not present in this pinned machinery snapshot; skipping.",
            )]
    path = os.environ.get("PATH", "")
    env = dict(os.environ, SMELT_PROJECT_DIR=project_dir, PLANTER_PROJECT_DIR=project_dir,
               PLANTER_WORKDIR=workdir, PLANTER_CHECK_NAME=spec["name"],
               PLANTER_TARGET_DIR=target_dir, PLANTER_TARGET_DIRS=" ".join(target_dirs),
               PLANTER_CHANGE_BASE=change_base, PLANTER_RESOLVED_CHANGE_BASE=change_base,
               PLANTER_SKIP_FETCH="1",
               PATH=os.path.join(project_dir, "tools") + (os.pathsep + path if path else ""))
    if skip_build:
        env["PLANTER_SKIP_BUILD"] = "1"
    label = spec["name"] + (f" ({target_dir})" if len(target_dirs) > 1 else "")
    out, err, err_text = execute(["bash", script] + spec["args"], workdir or project_dir, env, label)
    raw = out.strip()
    findings, parsed_ok = pcl.parse_check_findings(raw)
    for f in findings:
        f["source"] = f["source"] or spec["name"]
        f["severity"] = pcl.normalize_severity(f["severity"])
    if err_text is not None and (not parsed_ok or not any(pcl.is_blocking_finding(f) for f in findings)):
        findings.append(pcl.new_finding(
            source=spec["name"], category="check_script_failure", severity="ERROR",
            message=f"check tool {json.dumps(spec['name'], ensure_ascii=False)} exited with error ({err_text}): {err.strip() or raw or err_text}",
        ))
    return findings


def _exec_check(spec):
    started, seen, results = time.monotonic(), set(), []
    for d in target_dirs or [""]:
        for f in run_once(spec, d):
            key = (f["source"], f["category"], f["file"], f["line"], f["message"])
            if key not in seen:
                seen.add(key)
                if len(target_dirs) > 1 and not f["file"] and d:
                    f["message"] = f"[{d}] {f['message']}"
                results.append(f)
    return spec, results, time.monotonic() - started


builds_skipped = skip_build or os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
log(f"{len(enabled)} check(s) in {workdir}; target dirs: {' '.join(target_dirs) or '(none)'}; change base: {change_base}" + ("; builds skipped" if builds_skipped else ""))
max_workers = 1 if os.environ.get("PLANTER_SERIAL_CHECKS", "").strip() in ("1", "true") else min(8, os.cpu_count() or 4)

# Static checks run in parallel; build_verification and tests_json_parity NEVER run concurrently
# with static checks or with each other so tests_json_parity's tree/args.gn swap never races fx build.
static_specs = [s for s in enabled if s["name"] not in SERIAL_BUILD_CHECKS]
serial_specs = [s for s in enabled if s["name"] in SERIAL_BUILD_CHECKS]

try:
    diff_start = time.monotonic()
    if not only or "review_diff" in only or any(s["name"] in ("visibility_audit", "migration_sanity", "gn_attr_parity") for s in enabled):
        precompute_review_diff()
    diff_duration = time.monotonic() - diff_start

    total, idx_map, results_by_idx, done_count = len(enabled), {id(s): i for i, s in enumerate(enabled)}, {}, 0
    if static_specs:
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            for fut in [executor.submit(_exec_check, spec) for spec in static_specs]:
                spec, findings, duration = fut.result()
                done_count += 1
                log(f"({done_count}/{total}) {spec['name']} done ({pcl.fmt_secs(duration)})")
                results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)
    for spec in serial_specs:
        spec, findings, duration = _exec_check(spec)
        done_count += 1
        log(f"({done_count}/{total}) {spec['name']} done ({pcl.fmt_secs(duration)})")
        results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)

    rows = [results_by_idx[i] for i in range(total)]
    all_findings = [f for _, flist, _ in rows for f in flist]

    if (not only or "review_diff" in only) and _precomputed_diff_findings is not None:
        # Same rule as planter's upload gate (mergeReviewDiffFindings): a review_diff finding is
        # skipped only when a check reported the same key at a severity that blocks at least as
        # much, or explicitly waived it in a waivable category. A non-blocking check finding never
        # hides a blocking review_diff finding.
        seen_blocking = {}
        waived = set()
        check_waiver_reasons = {}
        for f in all_findings:
            key = (f["category"], f["file"], f["line"], f["message"])
            seen_blocking[key] = seen_blocking.get(key, False) or pcl.is_blocking_finding(f)
            if f.get("waiver"):
                check_waiver_reasons[key] = f["waiver"]
                if f["category"] in WAIVABLE_REVIEW_DIFF_CATEGORIES:
                    waived.add(key)
        diff_findings = []
        for raw_f in _precomputed_diff_findings:
            f = dict(raw_f, source="review_diff", severity=pcl.normalize_severity(raw_f.get("severity", "")))
            key = (f["category"], f["file"], f["line"], f["message"])
            blocks = pcl.is_blocking_finding(f)
            if key in waived or (key in seen_blocking and (seen_blocking[key] or not blocks)):
                continue
            seen_blocking[key] = blocks
            diff_findings.append(f)
        macro_dispute_items, verified_diff_fp_ids = (
            annotate_review_diff_macro_false_positives(
                diff_findings, workdir, change_base
            )
        )
        # Annotate review_diff findings verified safe by check waivers outside
        # WAIVABLE_REVIEW_DIFF_CATEGORIES with human-facing dispute guidance.
        seen_dispute_items = set(macro_dispute_items)
        for f in diff_findings:
            key = (f["category"], f["file"], f["line"], f["message"])
            w_reason = check_waiver_reasons.get(key)
            if not w_reason or id(f) in verified_diff_fp_ids:
                continue
            f_cat, f_path = f.get("category", ""), f.get("file", "")
            f["remediation"] = (
                f"{w_reason} Because planter's upload gate requires an explicit "
                f"dispute for `review_diff/{f_cat}`, list this finding in "
                '`CoderReport.disputed_findings` (`source: "review_diff"`, '
                f'`category: "{f_cat}"`, `file: "{f_path}"`) using a short, '
                "factual justification of the build structure addressed to the "
                "human reviewer (do not reference internal check names in `reason`)."
            )
            verified_diff_fp_ids.add(id(f))
            item_key = (f_cat, f_path)
            if f_cat and f_path and item_key not in seen_dispute_items:
                seen_dispute_items.add(item_key)
                macro_dispute_items.append(item_key)
        rows.append(({"name": "review_diff", "enabled": True}, diff_findings, diff_duration))
        all_findings.extend(diff_findings)
except KeyboardInterrupt:
    log("interrupted")
    sys.exit(130)

# Downgrade unamendable commit-message findings when a workspace write error occurred,
# or when build_verification built the `test` host tool during the same run.
if any(f["category"] == "workspace_write_error" for f in all_findings):
    for f in all_findings:
        if f["source"] == "commit_message_format" and pcl.is_blocking_finding(f):
            f["severity"] = "INFO"
if not builds_skipped and not any(f["category"] == "unbuilt_fx_test_tool" for f in all_findings):
    for f in all_findings:
        if (
            f["source"] == "commit_message_format"
            and f["category"] == "test_footer_fx_test_tool_not_built"
        ):
            f["severity"] = "INFO"

blocking_findings = [f for f in all_findings if pcl.is_blocking_finding(f)]
exit_code = 1 if blocking_findings else 0

if as_json:
    out = [
        {k: f[k] for k in ("source", "category", "severity", "file", "line", "message", "remediation", "waiver") if k in ("source", "category", "severity", "message") or f.get(k)}
        for f in all_findings
    ]
    print(json.dumps(out, indent=2))
    sys.exit(exit_code)


def counts(findings):
    return ", ".join(f"{n} {sev}" for sev in ("ERROR", "WARNING", "INFO") for n in [sum(1 for f in findings if f["severity"] == sev)] if n)


def print_finding(prefix, f):
    where = f["file"] + (f":{f['line']}" if f["file"] and f["line"] else "")
    print(f"{prefix}[{f['severity']}] {f['source']}/{f['category'] or '-'}" + (f"  {where}" if where else ""))
    for line in f["message"].splitlines() or [""]:
        print("    " + line)
    if f.get("waiver"):
        print("    Waiver: " + f["waiver"])
    elif f["remediation"]:
        rem = f["remediation"].splitlines()
        print("    Remediation: " + rem[0])
        for line in rem[1:]:
            print("    " + line)


width = max([len(s["name"]) for s, _, _ in rows] + [len(s["name"]) for s in disabled] + [4])
print("Checks (planter runs these, in this order, after you report):")
for spec, findings, secs in rows:
    status = "FAIL" if any(pcl.is_blocking_finding(f) for f in findings) else "PASS"
    detail = counts(findings)
    print(f"  {status}  {spec['name']:<{width}}  {pcl.fmt_secs(secs):>7}" + (f"  {detail}" if detail else ""))
for spec in disabled:
    print(f"  ----  {spec['name']:<{width}}  disabled in checks/manifest.json (planter skips it)")
if not rows:
    print("  (no enabled checks)")

if blocking_findings:
    print(f"\nBlocking findings ({len(blocking_findings)}); planter sends the change back for any of these:\n")
    for n, f in enumerate(blocking_findings, 1):
        print_finding(f"{n}. ", f)
        print()
others = [f for f in all_findings if not pcl.is_blocking_finding(f)]
if others:
    print(f"\nNon-blocking informational/waived notes ({len(others)}) — already satisfied, do NOT dispute:\n")
    for f in others:
        print_finding("- ", f)
        print()

print()
def _missing_agg_disputable_dirs(findings_list):
    return {
        os.path.dirname(f["file"]).strip("/")
        for f in findings_list
        if f.get("source") == "cq_reachability"
        and f.get("category") == "unexported_bazel_test"
        and "`CoderReport.disputed_findings`" in (f.get("remediation") or "")
        and f.get("file")
    }


def _dir_in_any_subtree(sub_dir, parent_dirs):
    return any(
        pd == "" or sub_dir == pd or sub_dir.startswith(pd + "/")
        for pd in parent_dirs
    )


def _is_missing_agg_disputable_finding(f, findings_list):
    rem = f.get("remediation") or ""
    if (
        f.get("source") == "cq_reachability"
        and f.get("category") == "unexported_bazel_test"
        and "`CoderReport.disputed_findings`" in rem
    ):
        return True
    if (
        f.get("source") == "test_suite_layout"
        and f.get("category") == "expanded_empty_test_suite"
    ):
        return True
    missing_dirs = _missing_agg_disputable_dirs(findings_list)
    if (
        missing_dirs
        and f.get("source") == "tests_json_parity"
        and f.get("category") == "test_dropped_from_tests_json"
    ):
        f_path = f.get("file") or ""
        if f_path:
            return _dir_in_any_subtree(
                os.path.dirname(f_path).strip("/"), missing_dirs
            )
        other_dropped_dirs = {
            os.path.dirname(g["file"]).strip("/")
            for g in findings_list
            if g.get("source") == "tests_json_parity"
            and g.get("category") == "test_dropped_from_tests_json"
            and g.get("file")
            and not _dir_in_any_subtree(
                os.path.dirname(g["file"]).strip("/"), missing_dirs
            )
        }
        return not other_dropped_dirs
    return False


# Format deterministic check script blocking items for the run_checks.sh summary footer.
def _format_script_blocking_summary(findings_list):
    script_blocking = [
        f
        for f in findings_list
        if f.get("source")
        and f.get("source") != "review_diff"
        and not _is_missing_agg_disputable_finding(f, findings_list)
    ]
    if not script_blocking:
        return None
    item_labels = sorted(
        {
            f"`{f['source']}/{f['category']}` (`{f['file']}`)"
            if f.get("file")
            else f"`{f['source']}/{f['category']}`"
            for f in script_blocking
        }
    )
    extra = ""
    if any(_is_missing_agg_disputable_finding(f, findings_list) for f in findings_list):
        extra = (
            " Once those fixable findings pass, report any remaining "
            "human-verification (`expanded_empty_test_suite`) or "
            "missing-aggregation-suite finding(s) in "
            "`CoderReport.disputed_findings` and `summary`."
        )
    return (
        f"ACTION REQUIRED: fix all {len(script_blocking)} deterministic check "
        f"finding(s) ({', '.join(item_labels)}) in the workspace and re-run "
        "`run_checks.sh` BEFORE submitting `CoderReport` (including on rebased or "
        "follow-up patchsets). Do NOT stop early or submit `disputed_findings` "
        f"while any fixable deterministic check script reports ERROR/WARNING.{extra}"
    )


if blocking_findings:
    sources = sorted({f["source"] for f in blocking_findings})
    print(f"RESULT: FAIL: {len(blocking_findings)} blocking finding(s) ({counts(blocking_findings)}) from {', '.join(sources)}.")
    script_action_note = _format_script_blocking_summary(blocking_findings)
    if script_action_note:
        print(script_action_note)
    missing_agg_findings = [
        f for f in blocking_findings if _is_missing_agg_disputable_finding(f, blocking_findings)
    ]
    # Guide CoderReport.disputed_findings when all fixable check scripts passed
    # and only disputable check findings (missing-aggregation test findings or
    # expanded_empty_test_suite) and/or review_diff WARNINGs or verified
    # review_diff false positives remain.
    if not script_action_note and (
        missing_agg_findings
        or all(
            f["source"] == "review_diff"
            and (f["severity"] == "WARNING" or id(f) in verified_diff_fp_ids)
            for f in blocking_findings
        )
    ):
        dispute_parts = []
        if missing_agg_findings:
            agg_labels = ", ".join(
                sorted(
                    {
                        f"`{f['source']}/{f['category']}` (`{f['file']}`)"
                        if f.get("file")
                        else f"`{f['source']}/{f['category']}`"
                        for f in missing_agg_findings
                    }
                )
            )
            dispute_parts.append(
                f"human-review / missing-aggregation-suite test finding(s) ({agg_labels})"
            )
        if macro_dispute_items:
            items_str = ", ".join(
                f"`review_diff/{cat}` (`{p}`)" for cat, p in macro_dispute_items
            )
            dispute_parts.append(
                f"verified macro/forwarding-group/label false positive(s) ({items_str})"
            )
        elif any(f["source"] == "review_diff" for f in blocking_findings):
            dispute_parts.append("`review_diff` WARNING / verified false positive(s)")
        print(
            "NOTE: all fixable deterministic check scripts passed; only "
            + " and ".join(dispute_parts)
            + " remain. Ensure the package `test_suite()` is defined with explicit "
            "`tests = [...]` (omitting `visibility` until the aggregation package is "
            "known when unexported; never add a per-package `bazel_test_suite` or "
            "guess an aggregation directory) and list each remaining item in "
            "`CoderReport.disputed_findings` (plus `summary`: for "
            "`expanded_empty_test_suite`, request that a human reviewer ensure that "
            "all relevant tests in the package have been included and state that "
            "agents must not resolve that comment; for `unexported_bazel_test`, ask "
            "the reviewer which aggregation `test_suite()` -- or where a new one, "
            "with a matching `bazel_test_suite` in `BUILD.gn`, should be added if "
            "none exists -- the package's `test_suite()`(s) should be added to, "
            "suggesting asking the Build team if they do not know). Drop stale "
            "`disputed_findings` entries for checks that now pass (never drop "
            "`test_suite_layout/expanded_empty_test_suite` while the `test_suite()` "
            "expansion remains in the change, and keep any post-turn check finding "
            "absent from `run_checks.sh` repeating across consecutive rounds due to "
            "host/container check-runner skew). Each `reason` in "
            "`disputed_findings` is posted directly to the human reviewer as a "
            "Gerrit review thread, so write a short, factual message addressed to "
            "a human."
        )
else:
    # Distinguish one-turn mid-turn pin lag from persistent host/container check skew.
    print(
        "RESULT: PASS: no blocking findings in live `$PLANTER_PROJECT_DIR` checks. "
        "Set `disputed_findings` to `[]` in `CoderReport` (including once if a mid-turn "
        "machinery update missed the prior round's start-of-turn check snapshot). Only if "
        "the exact same post-turn check finding absent from `run_checks.sh` repeats across "
        "consecutive clean rounds with no mid-turn update, report that persistent check-"
        "runner skew in `disputed_findings` so the stale pin or runner is refreshed."
    )
if any(f["category"] == "workspace_write_error" for f in blocking_findings):
    print(
        "NOTE: workspace filesystem/mount write failure detected (`workspace_write_error`). "
        "Stop retrying build or `git commit` write commands, set `test_footers` to "
        "`[\"fx lint\"]` and `disputed_findings` to `[]` in `CoderReport` "
        "(empty `test_footers` fails `test_verifier` with `missing_test_footer`, while "
        "target-bearing `Test:` commands trigger spurious query errors on an unwritable "
        "mount), and report the mount write failure in `summary`."
    )
if only:
    print("NOTE: only some checks ran (--only). Run all of them before reporting.")
if any(f["category"] == "build_skipped" for f in all_findings):
    print("NOTE: the builds were skipped. Run `run_checks.sh` without --skip-build before reporting.")
sys.exit(exit_code)
PYEOF
