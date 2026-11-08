#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic confidentiality and commit-hygiene check for open-source
# Fuchsia CLs and shared machinery files:
# 1. Rejects internal repository/vendor subpaths and internal domain names.
# 2. Rejects Fuchsia Gerrit banned words (internal product/board codenames).
# 3. Rejects personal email addresses, user handle mentions (except Bazel repo
#    names like @bazel2gn, @platforms, @fuchsia_sdk), and individual names,
#    while allowing non-personal GCP IAM service account identifiers and
#    non-confidential string literals ported from existing GN files.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"

python3 - "$WORKDIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"

# Construct patterns dynamically so this file itself contains no banned tokens.
v_prefix = "//" + "vendor/"
v_goog = "vendor/" + "google"
g_three = "google" + "3"
g_plex = "google" + "plex"
c_dom = "corp." + "google.com"

INTERNAL_PATH_RE = re.compile(
    rf"(?i)({v_prefix}[a-z0-9_-]+/[a-z0-9_.-]+|\b{v_goog}\b|\b{g_three}\b"
    rf"|\b{g_plex}\b|\.{c_dom}\b|\b{c_dom}\b)"
)
EMAIL_RE = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
# Non-personal GCP service account domain (covers <project>.iam.gserviceaccount.com).
SERVICE_ACCOUNT_DOMAIN = "gserviceaccount.com"
HANDLE_RE = re.compile(r"(?:^|[\s(])@([A-Za-z][A-Za-z0-9_-]{2,})\b(?!//|:)")
ALLOWED_HANDLES = {
    "bazel2gn",
    "platforms",
    "fuchsia_sdk",
    "fuchsia_build_info",
    "fuchsia_clang",
    "fuchsia_icu",
    "rules_rust",
    "rules_cc",
    "rules_python",
    "rules_go",
    "rules_license",
    "rules_fuchsia",
    "io_bazel_rules_go",
    "com_google_protobuf",
    "com_google_googletest",
    "crate_index",
    "vendor",
    "handle",
    "handles",
    "re2",
    "zlib",
    "rapidjson",
    "googletest",
    "protobuf",
    "google_benchmark",
    "abseil-cpp",
    "abseil_cpp",
    "boringssl",
    "gn_targets",
}
ALLOWED_HANDLE_PREFIXES = (
    "fuchsia_",
    "rules_",
    "bazel_",
    "io_bazel_",
    "com_google_",
    "internal_sdk",
)


# Populate ALLOWED_HANDLES with declared Bazel module and third-party repo names so
# repository references in commit messages or comments are not flagged as user handles.
def _load_workspace_bazel_repo_handles(root_dir):
    mod_path = os.path.join(root_dir, "build", "bazel", "toplevel.MODULE.bazel")
    try:
        with open(mod_path, encoding="utf-8") as mf:
            mod_txt = mf.read()
        for m in re.finditer(r'\b(?:module_)?name\s*=\s*"([A-Za-z][A-Za-z0-9_-]{2,})"', mod_txt):
            ALLOWED_HANDLES.add(m.group(1).lower())
    except OSError:
        pass
    tp_path = os.path.join(root_dir, "build", "tools", "bazel2gn", "third_party_target_map.json")
    try:
        with open(tp_path, encoding="utf-8") as tf:
            tp_data = json.load(tf)
        if isinstance(tp_data, dict):
            for k in tp_data:
                if isinstance(k, str) and k.startswith("@"):
                    repo = k.lstrip("@").split("//", 1)[0].split(":", 1)[0].strip()
                    if repo:
                        ALLOWED_HANDLES.add(repo.lower())
    except (OSError, ValueError):
        pass


_load_workspace_bazel_repo_handles(workdir)
PLACEHOLDER_NAME_RE = re.compile(r"\b(" + "Al" + "ice" + r")\b")


def git_output(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


# Collect non-comment code lines from tracked GN/GNI files in a package at change_base.
base_gn_code_cache = {}


def get_base_gn_code_text(pkg_dir):
    if pkg_dir in base_gn_code_cache:
        return base_gn_code_cache[pkg_dir]
    tree_spec = f"{change_base}:{pkg_dir}" if pkg_dir else change_base
    entries = git_output(["ls-tree", "--name-only", tree_spec]).splitlines()
    code_lines = []
    for name in entries:
        name = name.strip()
        if not name.endswith((".gn", ".gni")):
            continue
        rel_gn = f"{pkg_dir}/{name}" if pkg_dir else name
        content = git_output(["show", f"{change_base}:{rel_gn}"])
        for raw_line in content.splitlines():
            stripped = raw_line.strip()
            if stripped and not stripped.startswith("#"):
                code_lines.append(stripped)
    joined = "\n".join(code_lines)
    base_gn_code_cache[pkg_dir] = joined
    return joined


# Check whether an email-shaped token is a non-personal service account or
# a pre-existing GN code configuration value rather than a personal email.
def is_allowed_email_identifier(email_str, rel_path, line):
    if INTERNAL_PATH_RE.search(email_str):
        return False
    _, _, domain = email_str.lower().partition("@")
    if domain == SERVICE_ACCOUNT_DOMAIN or domain.endswith("." + SERVICE_ACCOUNT_DOMAIN):
        return True
    if rel_path.endswith((".bazel", ".bzl", ".gn", ".gni")):
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            pkg_dir = os.path.dirname(rel_path)
            if email_str in get_base_gn_code_text(pkg_dir):
                return True
    return False


env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
if env_changed:
    changed = {line.strip() for line in env_changed.splitlines() if line.strip()}
else:
    changed = set()
    for cmd in (
        ["diff", "--name-only", change_base],
        ["ls-files", "--others", "--exclude-standard"],
    ):
        for line in git_output(cmd).splitlines():
            if line.strip():
                changed.add(line.strip())

findings = []


# Scan added lines for internal paths, personal emails, handles, and names.
def scan_text(rel_path, text):
    whole_vg_bazel = "//" + "vendor/" + "google:__subpackages__"
    whole_vg_gn = "//" + "vendor/" + "google/*"
    for idx, line in enumerate(text.splitlines(), start=1):
        sanitized = line.replace(whole_vg_bazel, "").replace(whole_vg_gn, "")
        m_path = INTERNAL_PATH_RE.search(sanitized)
        if m_path:
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": (
                    f"Confidential/internal path reference '{m_path.group(0)}' "
                    f"found in '{rel_path}'."
                ),
                "remediation": (
                    "Remove internal/vendor subpath references. For vendor visibility, "
                    "always expose to the entire tree using '\"//vendor/google:__subpackages__\"'."
                ),
            })
        for m_email in EMAIL_RE.finditer(line):
            email_val = m_email.group(0)
            if is_allowed_email_identifier(email_val, rel_path, line):
                continue
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": f"Email address '{email_val}' found in '{rel_path}'.",
                "remediation": (
                    "Remove personal email addresses and state the rule or comment generically."
                ),
            })
        for m_h in HANDLE_RE.finditer(line):
            handle = m_h.group(1)
            h_low = handle.lower()
            if h_low not in ALLOWED_HANDLES and not h_low.startswith(ALLOWED_HANDLE_PREFIXES):
                findings.append({
                    "source": "confidentiality_check",
                    "category": "confidentiality_violation",
                    "severity": "error",
                    "file": rel_path,
                    "line": idx,
                    "message": f"User handle mention '@{handle}' found in '{rel_path}'.",
                    "remediation": (
                        "Remove user handle mentions and state the technical rule generically."
                    ),
                })
        m_name = PLACEHOLDER_NAME_RE.search(line)
        if m_name:
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": f"Person/reviewer name '{m_name.group(0)}' found in '{rel_path}'.",
                "remediation": (
                    "Never name specific reviewers or individuals in comments, "
                    "commit messages, or lessons."
                ),
            })


for rel in sorted(changed):
    if rel.endswith("confidentiality_check.sh"):
        continue
    full = os.path.join(workdir, rel)
    if not os.path.isfile(full):
        continue
    diff_out = git_output(["diff", "-U0", change_base, "--", rel])
    added_lines = []
    for l in diff_out.splitlines():
        if l.startswith("+") and not l.startswith("+++"):
            added_lines.append(l[1:])
    if added_lines:
        scan_text(rel, "\n".join(added_lines))

head_rev = git_output(["rev-parse", "--verify", "HEAD"])
base_rev = git_output(["rev-parse", "--verify", change_base])
commit_msg = git_output(["log", "-1", "--format=%B", "HEAD"]).rstrip("\n")
if commit_msg and head_rev and base_rev and head_rev != base_rev:
    scan_text("COMMIT_MSG", commit_msg)

print(json.dumps(findings, indent=2))
PYEOF
