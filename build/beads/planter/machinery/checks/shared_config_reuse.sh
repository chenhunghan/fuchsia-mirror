#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check that shared build configs (Rust lint configs in particular)
# are referenced and reused, not wrapped or copied, by GN-to-Bazel migrations.
#
# 1. alias_obscures_config (error): an `alias()` added by the change (absent from
#    the file at $PLANTER_CHANGE_BASE) whose `actual` is a `rust_lint_config` (or
#    whose name/actual is lint-named). Point `lint_config` at the defining
#    rust_lint_config instead. Warning for new aliases of other config-like rules.
# 2. duplicated_default_lints (error): a dict literal in a changed BUILD.bazel/.bzl
#    that repeats most entries of a default lint dict owned by
#    //build/config/rust/lints (its exported .bzl constants, or the private dicts
#    of its BUILD.bazel while nothing is exported yet). Area configs must load the
#    exported constants and spell out only their own additions.
# 3. lint_config_gn_parity (error): a `lint_config` label in a changed dual-build
#    BUILD.bazel (sibling BUILD.gn has the BAZEL2GN sentinel) whose package has no
#    GN `config("<name>")`. bazel2gn copies lint_config verbatim, so the GN
#    counterpart must sit next to the Bazel rust_lint_config under the same name.
# 4. broken_lint_then_change (error): a `LINT.ThenChange(...)` in or next to a
#    changed file that points at a missing file, or at a file without the matching
#    `LINT.IfChange(<id>)` (e.g. after moving lint dicts into a .bzl).
#
# All findings are change-wide, so with several target directories only the run
# for the first one reports.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" <<'PYEOF'
import ast
import json
import math
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

LINTS_PKG = "build/config/rust/lints"
BAZEL_BUILD_FILES = ("BUILD.bazel", "BUILD")
CONFIG_RULES = {
    "config_setting", "label_flag", "bool_flag", "string_flag", "int_flag",
    "string_list_flag", "constraint_value", "platform",
}
LINT_NAME_RE = re.compile(r"(^|[_:/-])(lints?|clippy)([_:/-]|$)", re.I)
SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL", re.M)
BASELINE_PAIRS = {("all", "allow")}
MIN_OVERLAP = 3
RATIO = 0.6


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return None


def git_lines(args):
    return [l.strip() for l in (git(args) or "").splitlines() if l.strip()]


def read(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8") as f:
            return f.read()
    except Exception:
        return None


def parse(text, rel):
    if text is None:
        return None
    try:
        return ast.parse(text, filename=rel)
    except Exception:
        return None


touched = set(git_lines(["diff", "--name-only", change_base]))
touched.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
changed = {p for p in touched if os.path.isfile(os.path.join(workdir, p))}


def is_starlark(rel):
    base = os.path.basename(rel)
    return base in BAZEL_BUILD_FILES or base.endswith(".bzl")


def str_kw(call, key):
    for kw in call.keywords:
        if kw.arg == key and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
            return kw.value.value
    return None


def targets(tree):
    """name -> (rule, call) for the top-level rule calls of a BUILD file."""
    out = {}
    for node in (tree.body if tree else []):
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name):
            name = str_kw(node.value, "name")
            if name:
                out[name] = (node.value.func.id, node.value)
    return out


def resolve(label, pkg):
    """(package, name) for an in-repo label, else None."""
    if not label or label.startswith("@"):
        return None
    if label.startswith(":"):
        return pkg, label[1:]
    if label.startswith("//"):
        body = label[2:]
        if ":" in body:
            p, n = body.split(":", 1)
        else:
            p, n = body, body.rsplit("/", 1)[-1]
        return p.strip("/"), n
    return None


_pkg_cache = {}


def pkg_targets(pkg):
    if pkg not in _pkg_cache:
        res = {}
        for base in BAZEL_BUILD_FILES:
            rel = os.path.join(pkg, base) if pkg else base
            text = read(rel)
            if text is not None:
                res = targets(parse(text, rel))
                break
        _pkg_cache[pkg] = res
    return _pkg_cache[pkg]


def rule_of(pkg, name, depth=0):
    t = pkg_targets(pkg).get(name)
    if not t:
        return None
    rule, call = t
    if rule == "alias" and depth < 5:
        r = resolve(str_kw(call, "actual"), pkg)
        if r:
            return rule_of(r[0], r[1], depth + 1) or "alias"
    return rule


def literal_pairs(node):
    pairs = set()
    for k, v in zip(node.keys, node.values):
        if (isinstance(k, ast.Constant) and isinstance(k.value, str)
                and isinstance(v, ast.Constant) and isinstance(v.value, str)):
            pairs.add((k.value, v.value))
    return pairs - BASELINE_PAIRS


def eval_dict(node, env):
    if isinstance(node, ast.Dict):
        return {k: v for k, v in literal_pairs(node)}
    if isinstance(node, ast.Name):
        return env.get(node.id)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        left, right = eval_dict(node.left, env), eval_dict(node.right, env)
        if left is None and right is None:
            return None
        return {**(left or {}), **(right or {})}
    return None


def dict_names(tree):
    """id(dict node) -> the variable or rule attribute it belongs to."""
    names = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            for sub in ast.walk(node.value):
                if isinstance(sub, ast.Dict):
                    names.setdefault(id(sub), node.targets[0].id)
        elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name):
            tname = str_kw(node.value, "name") or "?"
            for kw in node.value.keywords:
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Dict):
                        names.setdefault(id(sub), f"{node.value.func.id}({tname}).{kw.arg}")
    return names


def public_dicts(tree):
    env, public = {}, []
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            val = eval_dict(node.value, env)
            if val is not None:
                env[node.targets[0].id] = val
                if val and not node.targets[0].id.startswith("_"):
                    public.append(node.targets[0].id)
    return public


def add_chunks(tree, rel, chunks):
    names = dict_names(tree)
    for node in ast.walk(tree):
        if isinstance(node, ast.Dict):
            pairs = literal_pairs(node)
            if len(pairs) >= MIN_OVERLAP:
                chunks.append((names.get(id(node), "dict"), rel, pairs))


findings = []


def emit(category, severity, rel, line, message, remediation):
    findings.append({
        "source": "shared_config_reuse",
        "category": category,
        "severity": severity,
        "file": rel,
        "line": line,
        "message": message,
        "remediation": remediation,
    })


# 1. New alias() wrappers around lint configs.
for rel in sorted(changed):
    if os.path.basename(rel) not in BAZEL_BUILD_FILES:
        continue
    pkg = os.path.dirname(rel)
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    base_tree = parse(git(["show", f"{change_base}:{rel}"]), rel)
    base_aliases = {n for n, (r, _) in targets(base_tree).items() if r == "alias"}
    for name, (rule, call) in sorted(targets(tree).items()):
        if rule != "alias" or name in base_aliases:
            continue
        actual = str_kw(call, "actual") or ""
        r = resolve(actual, pkg)
        actual_rule = rule_of(*r) if r else None
        label = f"//{pkg}:{name}"
        if actual_rule == "rust_lint_config" or LINT_NAME_RE.search(name) or LINT_NAME_RE.search(actual):
            emit(
                "alias_obscures_config", "error", rel, call.lineno,
                f"New alias `{label}` only re-exports the lint config `{actual}`"
                + (f" ({actual_rule})" if actual_rule else "")
                + ". Wrapper labels hide which lints a target gets.",
                f"Delete `{label}` (and any GN `config(\"{name}\")` that only forwards to the same "
                f"lints) and set `lint_config = \"{actual}\"` on the targets that use it. bazel2gn copies "
                "lint_config verbatim, so the same-named GN `config()` must live in the BUILD.gn of the "
                "package that defines the Bazel rust_lint_config. Keep pre-existing wrapper labels "
                "unchanged; never add new ones.",
            )
        elif actual_rule in CONFIG_RULES:
            emit(
                "alias_obscures_config", "warning", rel, call.lineno,
                f"New alias `{label}` re-exports the {actual_rule} `{actual}` under another name.",
                f"Reference `{actual}` directly unless the alias is required for label compatibility; "
                "explain it in a comment if it is.",
            )

# 2. Copied default lint dicts.
canonical_files, exported, chunks = [], [], []
lints_dir = os.path.join(workdir, LINTS_PKG)
if os.path.isdir(lints_dir):
    for f in sorted(os.listdir(lints_dir)):
        if not f.endswith(".bzl"):
            continue
        rel = f"{LINTS_PKG}/{f}"
        tree = parse(read(rel), rel)
        names = public_dicts(tree) if tree else []
        if names:
            canonical_files.append(rel)
            exported += [(rel, n) for n in names]
            add_chunks(tree, rel, chunks)
    if not canonical_files:
        for base in BAZEL_BUILD_FILES:
            rel = f"{LINTS_PKG}/{base}"
            tree = parse(read(rel), rel)
            if tree is not None:
                canonical_files.append(rel)
                add_chunks(tree, rel, chunks)
                break

if exported:
    loads = "; ".join(
        f"load(\"//{os.path.dirname(r)}:{os.path.basename(r)}\", "
        + ", ".join(f"\"{n}\"" for rr, n in exported if rr == r) + ")"
        for r in sorted({r for r, _ in exported})
    )
    dup_fix = (
        f"{loads} and compose the config from those constants, e.g. "
        "`clippy = <production defaults> | _AREA_CLIPPY` (test variant: `<test defaults> | _AREA_CLIPPY`) "
        "and `rustc = <rustc defaults>`; keep only the area-specific entries as literals."
    )
else:
    dup_fix = (
        f"The defaults are private dicts in //{LINTS_PKG}/BUILD.bazel. Export them in this change: move "
        f"them verbatim (with their LINT.IfChange/ThenChange markers) into a .bzl in //{LINTS_PKG} under "
        "public names, load() them in that BUILD.bazel keeping every rust_lint_config target with the same "
        f"name and contents, point the LINT.ThenChange lines of //{LINTS_PKG}/BUILD.gn at the .bzl, then "
        "load the constants here and keep only the area-specific entries as literals."
    )

for rel in sorted(changed):
    if not is_starlark(rel) or rel in canonical_files:
        continue
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    names = dict_names(tree)
    for node in sorted((n for n in ast.walk(tree) if isinstance(n, ast.Dict)), key=lambda n: n.lineno):
        pairs = literal_pairs(node)
        if len(pairs) < MIN_OVERLAP:
            continue
        best = None
        for cname, crel, cpairs in chunks:
            overlap = len(pairs & cpairs)
            if overlap >= MIN_OVERLAP and overlap >= math.ceil(RATIO * len(cpairs)):
                if best is None or overlap > best[0]:
                    best = (overlap, cname, crel, len(cpairs))
        if best:
            overlap, cname, crel, total = best
            where = names.get(id(node), "dict literal")
            emit(
                "duplicated_default_lints", "error", rel, node.lineno,
                f"`{where}` restates {overlap}/{total} entries of the default lint dict `{cname}` "
                f"owned by '{crel}'. Copies drift from the defaults and are not covered by its "
                "LINT.IfChange blocks.",
                dup_fix,
            )

# 3. lint_config labels that bazel2gn will copy into BUILD.gn.
def label_values(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        yield node.value
    elif isinstance(node, ast.Dict):
        for v in node.values:
            yield from label_values(v)
    elif isinstance(node, ast.Call):
        for a in node.args:
            yield from label_values(a)
    elif isinstance(node, (ast.List, ast.Tuple)):
        for e in node.elts:
            yield from label_values(e)
    elif isinstance(node, ast.BinOp):
        yield from label_values(node.left)
        yield from label_values(node.right)


for rel in sorted(changed):
    if os.path.basename(rel) not in BAZEL_BUILD_FILES:
        continue
    pkg = os.path.dirname(rel)
    gn_text = read(os.path.join(pkg, "BUILD.gn"))
    if not gn_text or not SENTINEL_RE.search(gn_text):
        continue
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    for node in ast.walk(tree):
        if not isinstance(node, ast.keyword) or node.arg != "lint_config":
            continue
        for label in label_values(node.value):
            if label == "//conditions:default":
                continue
            r = resolve(label, pkg)
            if not r:
                continue
            lpkg, lname = r
            lgn = read(os.path.join(lpkg, "BUILD.gn"))
            if lgn and re.search(r'^\s*config\(\s*"' + re.escape(lname) + r'"\s*\)', lgn, re.M):
                continue
            emit(
                "lint_config_gn_parity", "error", rel, getattr(node.value, "lineno", 0),
                f"`lint_config = \"{label}\"` is copied verbatim into the generated BUILD.gn by bazel2gn, "
                f"but '{os.path.join(lpkg, 'BUILD.gn')}' defines no `config(\"{lname}\")`.",
                f"Add `config(\"{lname}\")` to '{os.path.join(lpkg, 'BUILD.gn')}', next to the Bazel "
                "rust_lint_config of the same name, holding only the area-specific rustflags (GN appends "
                "the defaults and drops production lints on testonly targets). Do not add an alias or a "
                "forwarding config in another package instead.",
            )

# 4. LINT.ThenChange references broken by moving lint dicts.
scan = set(changed)
for rel in touched:
    d = os.path.dirname(rel)
    try:
        entries = os.listdir(os.path.join(workdir, d))
    except OSError:
        continue
    for f in entries:
        if f in ("BUILD.gn",) + BAZEL_BUILD_FILES or f.endswith((".bzl", ".gni")):
            scan.add(os.path.join(d, f) if d else f)

THEN_RE = re.compile(r"LINT\.ThenChange\(")


def then_change_items(text):
    """Yields (line, item) for every LINT.ThenChange(...) target in text."""
    for m in THEN_RE.finditer(text or ""):
        end = text.find(")", m.end())
        if end < 0:
            continue
        # Continuation lines start with a comment marker; the first line does not.
        arg = re.sub(r"\n\s*(#|//)[ \t]*", "\n", text[m.end():end])
        line = text.count("\n", 0, m.start()) + 1
        for item in re.split(r"[,\s]+", arg):
            item = item.strip().strip("\"'")
            if item:
                yield line, item


def split_item(item, src):
    path, label = item, None
    if ":" in item.rsplit("/", 1)[-1]:
        path, label = item.rsplit(":", 1)
    if path.startswith("/"):
        return path.lstrip("/"), label
    return os.path.normpath(os.path.join(os.path.dirname(src), path)), label


def problem_with(trel, label, text):
    if text is None:
        return f"points at '{trel}', which does not exist"
    if label and not re.search(r"LINT\.IfChange\(\s*" + re.escape(label) + r"\s*\)", text):
        return f"points at '{trel}', which has no `LINT.IfChange({label})`"
    return None


def current_text(trel):
    if os.path.isdir(os.path.join(workdir, trel)):
        return ""
    return read(trel)


for rel in sorted(scan):
    text = read(rel)
    if not text or "LINT.ThenChange(" not in text:
        continue
    base_items = None
    for line, item in then_change_items(text):
        trel, label = split_item(item, rel)
        if rel not in touched and trel not in touched:
            continue
        problem = problem_with(trel, label, current_text(trel))
        if not problem:
            continue
        # Only report references this change broke, not pre-existing stale ones.
        if base_items is None:
            base_items = {i for _, i in then_change_items(git(["show", f"{change_base}:{rel}"]))}
        if item in base_items and problem_with(trel, label, git(["show", f"{change_base}:{trel}"])):
            continue
        emit(
            "broken_lint_then_change", "error", rel, line,
            f"`LINT.ThenChange({item})` {problem}.",
            "Update the LINT.ThenChange path/label to the file that now holds the matching "
            "`LINT.IfChange(<id>)` block (e.g. the .bzl the lint dicts were moved to), keeping "
            "both sides of every IfChange/ThenChange pair.",
        )

unique, seen = [], set()
for f in findings:
    key = (f["category"], f["file"], f["line"], f["message"])
    if key not in seen:
        seen.add(key)
        unique.append(f)
print(json.dumps(unique, indent=2))
PYEOF
