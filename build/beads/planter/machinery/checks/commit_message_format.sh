#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks the migration commit message:
#   subject: [bazel_migration][<subsystem>] //<migrated/dir>
#            or [bazel_migration] //<migrated/dir> when the directory's git log
#            shows no established subsystem tag (there is no parent subsystem).
#            A target dir build/secondary/<dir> (GN-only overlay) migrates to
#            //<dir> (gn_secondary_tree). A change that modifies no file versus
#            its parent migrates nothing: INFO instead of subject findings.
#   Test: footers: `fx bazel` commands need --config=..., Bazel labels start
#            with @//, no redundant //a/b:b labels, shac/bazel2gn verification is
#            the single line `Test: fx lint`, listed last.
# Validates HEAD when it is the task's commit, and flags uncommitted target migrations
# without Bazel-Migration-Target: footers during full check runs.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$BASE" <<'PYEOF'
import collections, json, os, re, subprocess, sys, tempfile

workdir, target_dir, base = sys.argv[1], sys.argv[2], sys.argv[3]

def git(*args):
    try:
        r = subprocess.run(["git", "-C", workdir, *args], capture_output=True, text=True, timeout=30)
        return r.stdout.strip() if r.returncode == 0 else ""
    except Exception:
        return ""

# Resolve a git revision or jj revset (such as @ or @- in a jj colocated checkout) to a git commit SHA.
def resolve_rev(rev):
    if not rev.startswith("@"):
        sha = git("rev-parse", "--verify", f"{rev}^{{commit}}")
        if sha:
            return sha
    try:
        r = subprocess.run(
            ["jj", "--no-pager", "--ignore-working-copy", "-R", workdir, "log", "--no-graph", "-r", rev, "-T", "commit_id"],
            capture_output=True, text=True, timeout=30,
        )
        if r.returncode == 0 and r.stdout.strip():
            return git("rev-parse", "--verify", f"{r.stdout.strip()}^{{commit}}")
    except Exception:
        pass
    sha = git("rev-parse", "--verify", f"{rev}^{{commit}}")
    if sha:
        return sha
    if rev == "@-":
        return git("rev-parse", "--verify", "HEAD^{commit}")
    return ""

findings = []

def add(category, line, message, remediation, severity="warning"):
    findings.append({
        "source": "commit_message_format",
        "category": category,
        "severity": severity,
        "file": "/COMMIT_MSG",
        "line": line,
        "message": message,
        "remediation": remediation,
    })

raw_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", target_dir)
    if d.strip().strip("/")
]
# build/secondary/<dir> is GN's overlay for //<dir> and must not get Bazel files
# (gn_secondary_tree): its code migrates to //<dir> (e.g. //third_party/<name>).
dirs = [d[len("build/secondary/"):] if d.startswith("build/secondary/") else d for d in raw_dirs]
check_dirs = list(dict.fromkeys(raw_dirs + dirs))

# Match a package path against the //dir or //.../<suffix> path in a commit subject,
# including parent/child subdirectories belonging to the same multi-directory task.
def matches_subject_dir(pkg, sdir):
    if not pkg or not sdir:
        return False
    if sdir.startswith(".../"):
        suf = sdir[4:]
        return bool(suf) and (
            pkg == suf or pkg.endswith("/" + suf) or f"/{suf}/" in f"/{pkg}/"
        )
    return pkg == sdir or pkg.startswith(sdir + "/") or sdir.startswith(pkg + "/")


# Verify that a migration HEAD commit belongs to this task's target
# directories rather than an upstream commit or stacked parent migration commit.
def is_task_migration_head(head_sha, parent_sha, subj):
    if not head_sha or not parent_sha:
        return False
    head_msg = git("log", "-1", "--format=%B", head_sha)
    has_mig_footer = bool(
        re.search(
            r"^(?:TAG\s*[:=]\s*.*\bplanter\b|Bazel-Migration-Target\s*:)",
            head_msg,
            re.M | re.I,
        )
    )
    has_mig_subj = subj.startswith("[bazel_migration]") or bool(
        re.search(r"^\[bazel\b|\bmigrat(?:e|ing|ion)\b.*\bbazel\b", subj, re.I)
    )
    if not (has_mig_subj or has_mig_footer):
        return False
    if not check_dirs:
        return subj.startswith("[bazel_migration]") or has_mig_footer
    head_files = [f for f in git("diff", "--name-only", parent_sha, head_sha).splitlines() if f]
    head_pkgs = {
        os.path.dirname(f)
        for f in head_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn", "BUILD")
    }
    head_bazel_pkgs = {
        os.path.dirname(f)
        for f in head_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    }
    untracked_bazel_pkgs = {
        os.path.dirname(f)
        for f in git("ls-files", "--others", "--exclude-standard", "--", *check_dirs).splitlines()
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    }
    if untracked_bazel_pkgs and not (head_bazel_pkgs & set(check_dirs)):
        return False
    sdir = (
        subj.split(" //", 1)[1].split()[0].split(":", 1)[0].strip().rstrip(".,;:").strip("/")
        if " //" in subj
        else ""
    )
    subj_matches = bool(sdir) and any(matches_subject_dir(td, sdir) for td in check_dirs)
    sdir_in_head = bool(sdir) and any(matches_subject_dir(hp, sdir) for hp in head_pkgs)
    if sdir_in_head and not subj_matches and not (head_bazel_pkgs & set(check_dirs)):
        return False
    if head_pkgs:
        return bool(head_pkgs & set(check_dirs)) or subj_matches
    return (
        any(f == td or f.startswith(td + "/") for td in check_dirs for f in head_files)
        or subj_matches
    )


head = resolve_rev("HEAD")
base_rev = resolve_rev(base)
uploaded_rev = resolve_rev(os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip())
# In a jj colocated checkout with base="@-" (or resolved to @-'s commit SHA),
# git HEAD stays at @- while the working-copy commit @ holds the change and
# commit message once described. Never fall back to a stale jj @ snapshot when
# git HEAD is at PLANTER_CHANGE_BASE with uncommitted working-tree edits or
# when @ is the previous uploaded commit.
if (
    head
    and base_rev
    and head == base_rev
    and os.path.isdir(os.path.join(workdir, ".jj"))
    and (base == "@-" or base_rev == resolve_rev("@-"))
):
    at_rev = resolve_rev("@")
    if (
        at_rev
        and at_rev != base_rev
        and at_rev != uploaded_rev
        and not git("diff", "--name-only", at_rev)
        and git("log", "-1", "--format=%B", at_rev).strip()
    ):
        head = at_rev

parent_rev = git("rev-parse", "--verify", f"{head}~1^{{commit}}") if head else ""
head_subj = git("log", "-1", "--format=%s", head) if head else ""
if is_task_migration_head(head, parent_rev, head_subj) and (not base_rev or base_rev == head):
    base_rev = parent_rev

# Advance a pre-rebase or pre-amend base revision to HEAD~1 (or the origin/main
# merge-base) so upstream commits pulled in by `git rebase origin/main` or prior
# patchsets before a rebase/amend are not diffed as part of this migration commit.
def advance_rebased_base(cur_head, cur_base):
    if not (cur_head and cur_base and cur_head != cur_base):
        return cur_base
    is_mig = bool(
        parent_rev
        and (
            is_task_migration_head(cur_head, parent_rev, head_subj)
            or head_subj.startswith("[bazel_migration]")
        )
    )
    if is_mig and cur_base != parent_rev:
        base_subj = git("log", "-1", "--format=%s", cur_base)
        if base_subj.startswith("[bazel_migration]"):
            return parent_rev
        try:
            anc_parent = (
                subprocess.run(
                    ["git", "-C", workdir, "merge-base", "--is-ancestor", cur_base, parent_rev],
                    capture_output=True,
                    timeout=15,
                ).returncode
                == 0
            )
            if anc_parent:
                return parent_rev
        except Exception:
            pass
    try:
        is_anc = (
            subprocess.run(
                ["git", "-C", workdir, "merge-base", "--is-ancestor", cur_base, cur_head],
                capture_output=True,
                timeout=15,
            ).returncode
            == 0
        )
    except Exception:
        is_anc = True
    mb = git("merge-base", cur_head, "origin/main")
    if not is_anc:
        if is_mig:
            return parent_rev
        if mb and mb != cur_head:
            return mb
        if parent_rev:
            return parent_rev
    if not (mb and mb != cur_base and mb != cur_head):
        return cur_base
    try:
        r = subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", cur_base, mb],
            capture_output=True,
            timeout=15,
        )
        if r.returncode == 0:
            if is_mig:
                return parent_rev
            return mb
    except Exception:
        pass
    return cur_base

base_rev = advance_rebased_base(head, base_rev)


# Return (build_dir, labels) for the configured `<build_dir>/tests.json`; labels is None
# when `.fx-build-dir` or tests.json is missing, unreadable, or empty.
def load_tests_json_labels():
    bdir = ""
    try:
        with open(f"{workdir}/.fx-build-dir") as fh:
            bdir = fh.read().strip()
        if bdir:
            with open(f"{workdir}/{bdir}/tests.json") as fh:
                tj_data = json.load(fh)
            if isinstance(tj_data, list) and tj_data:
                labels = set()
                for entry in tj_data:
                    t_obj = entry.get("test", {}) if isinstance(entry, dict) else {}
                    for k in ("label", "source_label", "name", "package_url"):
                        val = t_obj.get(k)
                        if isinstance(val, str) and val.strip():
                            labels.add(re.sub(r"^@@?//", "//", val.strip()).split("(", 1)[0])
                return bdir, labels
    except (OSError, ValueError):
        pass
    return bdir, None


# Warn when a target directory defines a test package absent from <build_dir>/tests.json
# during uncommitted working-tree rounds so the coder does not emit an `fx test` footer
# that fails `test_verifier` (`missing_fx_test_target`).
def check_unindexed_working_tree_test_packages():
    if not dirs or (head and base_rev and head != base_rev):
        return
    bdir, tj_lbls = load_tests_json_labels()
    if not tj_lbls:
        return
    pkg_rule_re = re.compile(
        r'\b(?:fuchsia_test_package|fuchsia_unittest_package)\(\s*"([^"]+)"\s*\)'
    )
    for d in dirs:
        gn_p = os.path.join(workdir, d, "BUILD.gn")
        bz_p = os.path.join(workdir, d, "BUILD.bazel")
        if not (os.path.isfile(gn_p) and os.path.isfile(bz_p)):
            continue
        try:
            gn_txt = open(gn_p, encoding="utf-8").read()
            bz_txt = open(bz_p, encoding="utf-8").read()
        except OSError:
            continue
        bz_names = re.findall(r'^\s*name\s*=\s*"([^"]+)"', bz_txt, re.M)
        if not bz_names:
            continue
        for m_pkg in pkg_rule_re.finditer(gn_txt):
            t_pkg = m_pkg.group(1)
            lbl = f"//{d}:{t_pkg}"
            if lbl in tj_lbls or any(t.startswith(f"//{d}:") for t in tj_lbls):
                continue
            ln_no = gn_txt[: m_pkg.start()].count("\n") + 1
            findings.append({
                "source": "commit_message_format",
                "category": "unindexed_fx_test_package_in_tests_json",
                "severity": "info",
                "file": f"{d}/BUILD.gn",
                "line": ln_no,
                "message": (
                    f"Test package `{lbl}` is not present in `{bdir}/tests.json`, so "
                    f"listing `fx test {lbl}` in `CoderReport.test_footers` or `Test:` "
                    "footers will fail `test_verifier` with `missing_fx_test_target`."
                ),
                "remediation": (
                    f"Do NOT include `fx test {lbl}` in `CoderReport.test_footers` or "
                    f"`Test:` footers; verify migrated Bazel target(s) in `//{d}` with "
                    f"`fx bazel build --config=fuchsia_platform @//{d}:{bz_names[0]}` "
                    "instead."
                ),
            })


check_unindexed_working_tree_test_packages()
if head and base_rev and head != base_rev:
    msg = git("log", "-1", "--format=%B", head)
    lines = msg.splitlines()
    subject = lines[0] if lines else ""
    env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
    env_base_rev = resolve_rev(
        os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip() or base
    )
    changed = (
        [f.strip() for f in env_changed.splitlines() if f.strip()]
        if (env_changed and env_base_rev and env_base_rev == base_rev)
        else [f for f in git("diff", "--name-only", base_rev, head).splitlines() if f]
    )
    changed_dirs = [
        d for d in dirs if any(f == d or f.startswith(d + "/") for f in changed)
    ]
    first = (changed_dirs[0] if changed_dirs else dirs[0]) if dirs else "<dir>"

    # Subsystem tags used by earlier commits touching the directory.
    # Ignore cross-cutting language, migration, sanitizer, and chore sweep tags
    # unless the tag is itself an informative path segment of the migrated directory.
    always_ignored_tags = {
        "bazel_migration", "bazel", "bazel2gn", "build", "gn", "roll", "release",
    }
    sweep_tags = always_ignored_tags | {
        "rust", "rs", "cpp", "c++", "cxx", "go", "py", "python", "fidl",
        "clippy", "rustfmt", "clang", "lint", "format", "cleanup", "refactor",
        "flake", "fix", "trivial",
        "lsc", "cmxtinction", "cmx", "v2", "fxbug.dev", "fxbug", "fxb",
        "doc", "docs", "readme", "owners", "license", "copyright",
        "spelling", "typo", "typos", "style", "various", "misc", "treewide",
        "revert", "reland", "deps", "visibility", "allowlist",
        "src", "lib", "libs", "bin", "bins", "test", "tests", "testing",
        "unittest", "unittests", "util", "utils", "common",
        "asan", "lsan", "ubsan", "tsan", "hwasan", "msan", "kasan", "sancov",
        "lto", "fatlto", "thinlto", "wconversion", "werror", "warnings",
        "warning", "toolchain", "llvm", "rustc",
    }
    generic_dir_segments = {
        "src", "lib", "libs", "bin", "bins", "test", "tests", "testing",
        "util", "utils", "common", "internal", "build", "secondary", "third_party",
    }
    dir_segments = {
        seg.lower()
        for d in dirs
        for seg in d.split("/")
        if seg and seg.lower() not in generic_dir_segments
    }
    leaves = [d.rsplit("/", 1)[-1] for d in dirs]

    # Read commit subjects touching path_args, preserving newest-first output if git log times out.
    def read_directory_git_log_subjects(rev, path_args):
        cmd = ["git", "-C", workdir, "log", "-n", "200", "--format=%s", rev, "--", *path_args]
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
            return res.stdout.strip() if res.returncode == 0 else ""
        except subprocess.TimeoutExpired as exc:
            raw = exc.stdout or ""
            if isinstance(raw, bytes):
                raw = raw.decode("utf-8", errors="replace")
            # Drop the trailing line in case timeout cut off mid-subject.
            complete_lines = raw.splitlines()[:-1] if "\n" in raw else []
            return "\n".join(complete_lines).strip()
        except Exception:
            return ""

    # Collect net tag counts from directory git history, discounting reverted commits
    # so a commit + revert (+ reland) cycle does not inflate one-off sweep tags.
    def collect_directory_subsystem_tags():
        tag_counts = collections.Counter()
        non_revert_count = 0
        if not dirs:
            return tag_counts, non_revert_count
        log_out = read_directory_git_log_subjects(
            base_rev, list(dict.fromkeys(raw_dirs + dirs))
        )
        for subj_line in log_out.splitlines():
            is_revert = bool(re.match(r"^(?:\[Revert\]\s*)?Revert\b", subj_line, re.I))
            if not is_revert:
                non_revert_count += 1
            for tok in re.findall(r"\[([A-Za-z0-9_.\-]+)\]", subj_line):
                tl = tok.lower()
                if tl in always_ignored_tags:
                    continue
                if tl in sweep_tags and tl not in dir_segments:
                    continue
                if is_revert:
                    tag_counts[tok] -= 1
                else:
                    tag_counts[tok] += 1
        return tag_counts, non_revert_count

    tags, non_revert_commits = collect_directory_subsystem_tags()
    max_leaf_count = max((tags[lf] for lf in leaves), default=0)

    # Filter out incidental non-path tags (e.g., two-commit foreign dependency
    # sweeps) that neither match a directory path segment nor account for a
    # substantial share of the directory's commit history.
    def is_established_subsystem_tag(tag_name, count):
        if count < 2:
            return False
        if tag_name.lower() in dir_segments:
            return True
        min_non_path = max(3, int(0.15 * non_revert_commits))
        if max_leaf_count > 0:
            min_non_path = max(min_non_path, max_leaf_count // 2)
        return count >= min_non_path

    raw_common = [
        t for t, n in tags.most_common() if is_established_subsystem_tag(t, n)
    ]

    # Prefer a component-level path tag over an area-wide one. A tag naming a shallow
    # area segment (e.g. the `<area>` in src/<area>/<component>/tests/test_data/<leaf>) is
    # too broad when a more specific non-leaf component segment sits more than one level
    # below it; exclude area-wide tags from `common`/`preferred_tags` without fabricating
    # an unestablished component tag that never appeared in `git log`.
    fixture_dir_segments = {
        "test_data", "testdata", "data", "fixtures", "examples", "example", "integration",
    }

    # Return (index, name) of the deepest informative non-leaf segment of rel_dir.
    def narrowest_component_segment(rel_dir):
        segs = rel_dir.split("/")[:-1]
        for idx in range(len(segs) - 1, -1, -1):
            low = segs[idx].lower()
            if low in generic_dir_segments or low in fixture_dir_segments:
                continue
            return idx, segs[idx]
        return -1, ""

    # Return the narrower component tag when tag_name is an area-wide or unestablished grouping segment.
    def narrower_tag_for(tag_name):
        segs_low = [s.lower() for s in first.split("/")]
        if not tag_name or tag_name.lower() not in segs_low:
            return ""
        comp_idx, comp_seg = narrowest_component_segment(first)
        if comp_idx < 0:
            return ""
        tag_idx = segs_low.index(tag_name.lower())
        if tag_idx >= comp_idx:
            return ""
        if tag_idx == comp_idx - 1 and tag_name in raw_common:
            return ""
        return comp_seg

    common = [t for t in raw_common if not narrower_tag_for(t)]
    shown = " ".join(dict.fromkeys(raw_dirs[:1] + dirs[:1])) or "<dir>"
    hint = (f"Tags used for this directory in `git log --format=%s -- {shown}`: "
            + ", ".join(f"[{t}]" for t in common[:5]) + ".") if common else (
            f"`git log --format=%s -- {shown}` shows no recurring subsystem tag.")
    moved = [f"//{r} -> //{d}" for r, d in zip(raw_dirs, dirs) if r != d]
    if moved:
        hint += (" GN overlay directories migrate to the directory they overlay (gn_secondary_tree): "
                 + ", ".join(moved) + ".")

    non_leaf_common = [t for t in common if t not in leaves]
    preferred_tags = non_leaf_common or common

    # Build a single-line commit subject <= 65 chars by abbreviating leading path segments when needed.
    def abbreviate_subject_path(prefix, rel_dir, limit=65):
        full = f"{prefix} //{rel_dir}"
        if len(full) <= limit:
            return full
        segs = [s for s in rel_dir.split("/") if s]
        for i in range(1, len(segs)):
            cand = f"{prefix} //.../{'/'.join(segs[i:])}"
            if len(cand) <= limit:
                return cand
        return f"{prefix} //.../{segs[-1]}" if segs else full

    if preferred_tags:
        tag0 = preferred_tags[0]
        tagged_full = f"[bazel_migration][{tag0}] //{first}"
        untagged_full = f"[bazel_migration] //{first}"
        if len(tagged_full) <= 65:
            pref_subj = tagged_full
            alt_subj = ""
        elif len(untagged_full) <= 65:
            pref_subj = untagged_full
            alt_subj = abbreviate_subject_path(f"[bazel_migration][{tag0}]", first)
        else:
            pref_subj = abbreviate_subject_path(f"[bazel_migration][{tag0}]", first)
            alt_subj = abbreviate_subject_path("[bazel_migration]", first)
    else:
        pref_subj = abbreviate_subject_path("[bazel_migration]", first)
        alt_subj = ""
    alt_note = f" (or '{alt_subj}')" if alt_subj and alt_subj != pref_subj else ""
    subj_path = (
        subject.split(" //", 1)[1].split(":", 1)[0].split(" targets", 1)[0].strip().rstrip(".").strip("/")
        if " //" in subject
        else ""
    )
    def _bz_dir_migrated(rel_dir):
        if f"{rel_dir}/BUILD.gn" in changed:
            return True
        base_btxt = git("show", f"{base_rev}:{rel_dir}/BUILD.bazel")
        try:
            with open(os.path.join(workdir, rel_dir, "BUILD.bazel")) as fh:
                cur_btxt = fh.read()
        except OSError:
            cur_btxt = ""
        base_names = set(re.findall(r'^\s*name\s*=\s*"([^"]+)"', base_btxt, re.M))
        cur_names = set(re.findall(r'^\s*name\s*=\s*"([^"]+)"', cur_btxt, re.M))
        if cur_names - base_names:
            return True
        return not base_btxt and rel_dir in dirs

    is_multi_dir_change = len(dirs) > 1 or len([
        f
        for f in changed
        if os.path.basename(f) == "BUILD.bazel" and _bz_dir_migrated(os.path.dirname(f))
    ]) > 1
    # Per //build/beads/references/migration/commit_message/guidelines.md Section 2:
    # - 2.A / 2.B: `//<dir>` or `//<dir>:<target>` (no comma-separated `//dir1, //dir2`)
    # - 2.C: `//<dir> targets`
    # - 2.D (multi-directory): `//<root_dir>`, `<family_or_dirs> libraries`, or
    #   `//<shared_parent>: <dir1>, <dir2>`
    single_dir_ref_pat = r"//[^\s,]+(?:\s+targets)?(?<![.,;:])"
    _sub_item = r"[A-Za-z0-9_.\-/]+"
    _sub_list = (
        _sub_item
        + r"(?:\s+and\s+"
        + _sub_item
        + r"|(?:,\s+(?:and\s+)?"
        + _sub_item
        + r")+|,\s+\.\.\.)"
    )
    _family_list = (
        _sub_item
        + r"(?:\s+and\s+"
        + _sub_item
        + r"|(?:,\s+(?:and\s+)?"
        + _sub_item
        + r")+|\s+"
        + _sub_item
        + r")?"
    )
    multi_dir_ref_pat = (
        r"(?:"
        + single_dir_ref_pat
        + r"|//[^\s,:]+:\s+"
        + _sub_list
        + r"(?:(?<![.,;:])|(?<=\.\.\.))"
        + r"|(?!//)"
        + _family_list
        + r"\s+(?:libraries|packages|targets|tools|drivers|tests|modules|components|binaries)(?<![.,;:])"
        + r")"
    )
    ref_pat = multi_dir_ref_pat if is_multi_dir_change else single_dir_ref_pat
    m_tag = re.match(r"^\[bazel_migration\]\[([A-Za-z0-9_.\-]+)\] " + ref_pat + r"$", subject)
    m_none = re.match(r"^\[bazel_migration\] " + ref_pat + r"$", subject)
    m_infra = (
        re.match(r"^(?:\[[A-Za-z0-9_.\-]+\])+ \S.*(?<![.,;:])$", subject)
        if not dirs and not subject.startswith("[bazel_migration]")
        else None
    )
    # Emit an advisory note for leaf-directory tags absent from directory git history.
    def emit_unestablished_leaf_tag_info(tag_name):
        if tags[tag_name] >= 1:
            return
        untagged = abbreviate_subject_path("[bazel_migration]", first)
        msg_text = (
            f"Commit subject tag [{tag_name}] is just the leaf directory name, "
            "which may be too narrow to be a subsystem tag."
        )
        rem_text = (
            "Keep it only if the history or reviewer guidance shows it is an established "
            f"subsystem; otherwise, with no parent subsystem, use '{untagged}'. {hint}"
        )
        add("commit_subject_leaf_tag", 1, msg_text, rem_text, severity="info")

    if not changed and not git("status", "--porcelain", "--ignore-submodules=all"):
        add("commit_subject_empty_change", 1,
            "The change modifies no file versus its parent, so nothing is migrated and the "
            "'[bazel_migration] //<migrated dir>' subject is not required.",
            "Do not announce a migration that did not happen: keep a subject that describes the commit and "
            "report the blocker and the steps it needs first in the summary. Once the change migrates "
            f"targets, use '{pref_subj}'{alt_note}. {hint}", severity="info")
    # Detect a follow-up that only edits already-migrated BUILD.bazel files (no BUILD.gn
    # change, no new BUILD.bazel, no new Bazel target names): it migrates nothing, so a
    # '[bazel_migration] //<dir>' subject overstates it and a plain descriptive subject is fine.
    elif (
        changed
        and all(os.path.basename(f) == "BUILD.bazel" for f in changed)
        and not any(_bz_dir_migrated(os.path.dirname(f)) for f in changed)
    ):
        if subject.startswith("[bazel_migration]"):
            add("commit_subject_overstates_migration", 1,
                f"Commit subject {subject!r} announces a migration, but the change only edits "
                "pre-existing BUILD.bazel files (no BUILD.gn change, no new BUILD.bazel file "
                "and no new Bazel target), so nothing is migrated.",
                "Use a plain '[<subsystem>] <imperative summary>' subject (<= 65 chars) that "
                "says what the follow-up does, e.g. '[<subsystem>] Run Bazel tests in :all', "
                "and keep '[bazel_migration] //<dir>' for commits that move targets from GN "
                "to Bazel.", severity="warning")
        elif not re.match(r"^(?:\[[A-Za-z0-9_.\-]+\])+ \S.*(?<![.,;:])$", subject) \
                or len(subject) > 65:
            add("commit_subject_format", 1,
                f"Commit subject {subject!r} of this non-migration follow-up does not follow "
                "'[<subsystem>] <imperative summary>' (<= 65 chars, no trailing punctuation).",
                "Describe what the follow-up does, e.g. '[<subsystem>] Run Bazel tests in :all'.",
                severity="error")
    elif not (m_tag or m_none or m_infra):
        add("commit_subject_format", 1,
            f"Commit subject {subject!r} does not follow '[bazel_migration][<subsystem>] //<migrated dir>' "
            "(or '[bazel_migration] //<migrated dir>' when there is no established subsystem, "
            "without trailing punctuation).",
            f"Use '{pref_subj}'{alt_note} on a single unwrapped line <= 65 characters (without a trailing period). "
            "When adding `[<subsystem>]` would push the full `//<dir>` path over 65 characters, omit `[<subsystem>]` "
            "or abbreviate leading path segments as `//.../<suffix>` rather than wrapping `//<dir>` onto the next line. "
            f"{hint}", severity="error")
    elif len(subject) > 65:
        subj_tag = m_tag.group(1) if m_tag else (preferred_tags[0] if preferred_tags else "")
        over_untagged = f"[bazel_migration] //{first}"
        if len(over_untagged) <= 65:
            over_pref = over_untagged
            over_alt = (
                abbreviate_subject_path(f"[bazel_migration][{subj_tag}]", first)
                if subj_tag
                else ""
            )
        elif subj_tag:
            over_pref = abbreviate_subject_path(f"[bazel_migration][{subj_tag}]", first)
            over_alt = abbreviate_subject_path("[bazel_migration]", first)
        else:
            over_pref = abbreviate_subject_path("[bazel_migration]", first)
            over_alt = ""
        over_alt_note = f" or `{over_alt}`" if over_alt and over_alt != over_pref else ""
        add(
            "commit_subject_format", 1,
            f"Commit subject `{subject}` is {len(subject)} characters, exceeding the "
            "65-character limit (which causes subject line wrapping/truncation).",
            f"Use `{over_pref}`{over_alt_note} on a single unwrapped line <= 65 characters "
            "(omit `[<subsystem>]` or abbreviate leading path segments as `//.../<suffix>` "
            "instead of wrapping `//<dir>` onto the next line).",
            severity="error",
        )
    elif (
        len(dirs) == 1
        and not is_multi_dir_change
        and not re.search(r":\s+[A-Za-z0-9_.\-]+", subject)
        and subj_path
        and "..." not in subj_path
        and first.startswith(subj_path + "/")
        and f"{subj_path}/BUILD.bazel" not in changed
    ):
        add(
            "commit_subject_format",
            1,
            f"Commit subject `{subject}` names the parent directory `//{subj_path}` instead of "
            f"the migrated directory `//{first}`.",
            f"Use `{pref_subj}`{alt_note} (omit `[<subsystem>]` or abbreviate with `//.../` "
            f"when adding `[<subsystem>]` would push the full `//{first}` subject over "
            "65 characters).",
            severity="error",
        )
    # Reject cross-cutting sweep/chore tags and unestablished non-subsystem tags
    # on `[bazel_migration][<tag>] //<dir>` commit subjects.
    elif (
        m_tag
        and m_tag.group(1).lower() in sweep_tags
        and m_tag.group(1).lower() not in dir_segments
    ):
        rec_subj = (
            abbreviate_subject_path(f"[bazel_migration][{preferred_tags[0]}]", first)
            if preferred_tags
            else abbreviate_subject_path("[bazel_migration]", first)
        )
        msg_text = (
            f"Commit subject tag [{m_tag.group(1)}] is a cross-cutting "
            "migration/language/chore tag from tree-wide sweeps rather than an "
            f"established subsystem tag for `//{first}`."
        )
        rem_text = f"Remove `[{m_tag.group(1)}]` and use '{rec_subj}' instead. {hint}"
        add(
            "commit_subject_sweep_tag",
            1,
            msg_text,
            rem_text,
            severity="error",
        )
    elif (
        m_tag
        and not non_leaf_common
        and m_tag.group(1) not in leaves
        and m_tag.group(1).lower() not in dir_segments
        and tags[m_tag.group(1)] < 3
    ):
        rec_subj = (
            abbreviate_subject_path(f"[bazel_migration][{preferred_tags[0]}]", first)
            if preferred_tags
            else abbreviate_subject_path("[bazel_migration]", first)
        )
        msg_text = (
            f"Commit subject tag [{m_tag.group(1)}] is not an established "
            f"subsystem tag for `//{first}`."
        )
        rem_text = f"Remove `[{m_tag.group(1)}]` and use '{rec_subj}' instead. {hint}"
        add(
            "commit_subject_sweep_tag",
            1,
            msg_text,
            rem_text,
            severity="error",
        )
    # Check leaf-directory subject tags when no broader parent subsystem tag exists in history.
    elif (
        m_tag
        and m_tag.group(1) in leaves
        and m_tag.group(1) not in common
        and not non_leaf_common
    ):
        emit_unestablished_leaf_tag_info(m_tag.group(1))
    # Check area-wide path tags when a narrower component segment exists.
    elif m_tag and narrower_tag_for(m_tag.group(1)):
        narrow = narrower_tag_for(m_tag.group(1))
        untagged_subj = abbreviate_subject_path("[bazel_migration]", first)
        narrow_subj = abbreviate_subject_path(f"[bazel_migration][{narrow}]", first)
        if narrow in common:
            rem_msg = f"Use the narrower component tag: '{narrow_subj}'. {hint}"
        else:
            rem_msg = (
                f"Omit `[{m_tag.group(1)}]` and use '{untagged_subj}' "
                f"(or '{narrow_subj}'). {hint}"
            )
        add(
            "commit_subject_broad_tag",
            1,
            f"Commit subject tag [{m_tag.group(1)}] names a broad area of the tree rather "
            f"than the component containing `//{first}`.",
            rem_msg,
            severity="warning",
        )
    # Emit an advisory note for path-segment tags absent from directory git history that
    # force abbreviating that segment out of the `//.../` subject path, without blocking
    # when reviewer guidance or task notes explicitly request that component tag.
    elif (
        m_tag
        and not preferred_tags
        and m_tag.group(1) not in common
        and f"/{m_tag.group(1).lower()}/"
        in abbreviate_subject_path("[bazel_migration]", first).lower()
        and f"/{m_tag.group(1).lower()}/" not in subject.lower()
    ):
        untagged_subj = abbreviate_subject_path("[bazel_migration]", first)
        msg_text = (
            f"Commit subject tag [{m_tag.group(1)}] is not an established subsystem "
            f"tag in `git log` for `//{first}` and forces abbreviating "
            f"`{m_tag.group(1)}/` out of the subject path."
        )
        rem_text = (
            f"Keep `[{m_tag.group(1)}]` only if reviewer guidance or task notes "
            f"requested it; otherwise omit `[{m_tag.group(1)}]` and use "
            f"'{untagged_subj}' so `{m_tag.group(1)}/` remains in the subject path. "
            f"{hint}"
        )
        add(
            "commit_subject_unestablished_tag",
            1,
            msg_text,
            rem_text,
            severity="info",
        )
    elif (
        m_none
        and preferred_tags
        and len(f"[bazel_migration][{preferred_tags[0]}] //{first}") <= 65
        and len(subject) + len(f"[{preferred_tags[0]}]") <= 65
    ):
        add("commit_subject_missing_tag", 1,
            "Commit subject has no subsystem tag although the directory's git log uses one.",
            f"Use '{pref_subj}'{alt_note}. {hint}")

    # Check whether the `test` host tool required by `fx test --dry` exists in the build directory.
    def find_missing_fx_test_tool(wdir, build_dir):
        if not build_dir:
            return None
        try:
            with open(os.path.join(wdir, build_dir, "tool_paths.json")) as fh:
                tp_data = json.load(fh)
        except (OSError, ValueError):
            return None
        if not isinstance(tp_data, list):
            return None
        for entry in tp_data:
            if not isinstance(entry, dict) or entry.get("name") != "test":
                continue
            rel_path = (entry.get("path") or "").strip()
            lbl = (entry.get("label") or "//scripts/fxtest/python:install").strip()
            if rel_path and not os.path.isfile(os.path.join(wdir, build_dir, rel_path)):
                return f"{build_dir}/{rel_path}", lbl
        return None

    bdir, tj_targets = load_tests_json_labels()
    missing_fx_test_tool = find_missing_fx_test_tool(workdir, bdir)

    test_lines = [(i + 1, l) for i, l in enumerate(lines) if l.startswith("Test:")]
    for ln, l in test_lines:
        cmd = l[len("Test:"):].strip()
        if re.search(r"\bfx bazel (build|test|run|query|cquery)\b", cmd) and "--config=" not in cmd:
            add("test_footer_bazel_no_config", ln,
                f"`Test:` footer `{cmd}` runs `fx bazel` without `--config=...`, which is invalid.",
                "Pass `--config=fuchsia_platform` (device build) or `--config=host` (host test), "
                "e.g. `fx bazel test --config=host @//<dir>:<test>` or "
                "`fx bazel build --config=fuchsia_platform @//<dir>:<test>`; record only commands "
                "that exited 0.")
        if re.search(r"\bfx bazel\b", cmd) and re.search(r"(?<![@\w])//", cmd):
            add("test_footer_label_without_at", ln,
                f"`Test:` footer `{cmd}` passes Bazel labels without a leading `@`.",
                "Write Bazel labels as `@//<dir>:<target>` in `fx bazel ...` commands.")
        if re.search(r"\bfx bazel\b", cmd):
            for pkg, name in re.findall(r"@?//([\w./\-]+):([\w.\-]+)", cmd):
                if pkg.rsplit("/", 1)[-1] == name:
                    add("test_footer_redundant_label", ln,
                        f"`Test:` footer label `//{pkg}:{name}` repeats the package name.",
                        f"Write `//{pkg}` instead of `//{pkg}:{name}`.")
        if (
            re.search(r"\bfx bazel2gn\b", cmd)
            or re.search(r"verify_bazel2gn", cmd)
            or re.search(r"\bshac\s+check\b", cmd)
            or (
                re.search(r"\bfx\s+lint\b", cmd)
                and cmd
                not in (
                    "fx lint",
                    "fx lint --fail-level warning",
                    "fx lint --fail-level=warning",
                    "fx lint -- --fail-level warning",
                    "fx lint -- --fail-level=warning",
                )
            )
        ):
            add("test_footer_bazel2gn_form", ln,
                f"`Test:` footer `{cmd}` verifies bazel2gn/shac in a narrower or indirect form.",
                "Use the single line `Test: fx lint` (runs `verify_bazel2gn` and all other shac checks).")
        # Reject `:all` in `fx bazel build` footers: `:all` silently skips incompatible
        # targets (such as host-only `ffx_plugin`/`ffx_tool`/`HOST_OS_CONSTRAINTS` targets
        # under `--config=fuchsia_platform`) instead of verifying explicit migrated targets.
        if re.search(r"\bfx bazel build\b.*:all\b", cmd):
            add(
                "test_footer_build_all_not_test",
                ln,
                f"`Test:` footer `{cmd}` builds `:all` (which silently skips "
                "platform-incompatible targets) instead of naming the migrated "
                "target(s) explicitly.",
                "Name each migrated target explicitly: use "
                "`fx bazel build --config=host @//<dir>:<target>` for host-only targets "
                "(`ffx_plugin`, `ffx_tool`, `idk_ffx_tool`, `*_host_tool`, "
                "`HOST_OS_CONSTRAINTS`), `fx bazel build --config=fuchsia_platform "
                "@//<dir>:<target>` for Fuchsia library/binary/component/`fx_test`/"
                "`fx_unittest_package` targets, and `fx bazel test --config=host "
                "@//<dir>:<test>` for runnable Bazel host tests.",
            )
        # Check that `fx test` footers do not pass a GN `group("tests")` label (`//<dir>:tests`),
        # which is not a leaf test entry in `tests.json` and fails `fx test --dry`.
        if re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b", cmd):
            if missing_fx_test_tool is not None:
                tool_rel, tool_lbl = missing_fx_test_tool
                msg_text = (
                    f"`Test:` footer `{cmd}` runs `fx test`, but the `test` host tool "
                    f"(`{tool_rel}`) has not been built, so Planter's `test_verifier` "
                    "(`fx test --dry`) fails with `missing_fx_test_target` "
                    "(`Tool \"test\" is required by this command and has not been built`)."
                )
                rem_text = (
                    "Replace `Test: fx test ...` in the commit message and "
                    "`CoderReport.test_footers` with "
                    "`Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` "
                    "(for device tests) or `Test: fx bazel test --config=host "
                    "@//<dir>:<test>` (for host tests), or build the `fx test` host "
                    f"tool first with `fx build '{tool_lbl}'`."
                )
                add(
                    "test_footer_fx_test_tool_not_built",
                    ln,
                    msg_text,
                    rem_text,
                    severity="error",
                )
            if tj_targets:
                fx_lbls = [
                    re.sub(r"^@@?//", "//", lbl).split("(", 1)[0]
                    for lbl in re.findall(r"@?//[^\s,;\"'`]+", cmd)
                ]
                unconf = [
                    lbl for lbl in fx_lbls
                    if not (
                        lbl in tj_targets
                        or any(
                            t == lbl or (":" not in lbl[2:] and t.startswith(lbl.rstrip("/") + ":"))
                            for t in tj_targets
                        )
                    )
                ]
                if unconf:
                    add(
                        "test_footer_fx_test_not_in_tests_json",
                        ln,
                        f"`Test:` footer `{cmd}` passes target(s) to `fx test` that are not in "
                        f"the configured `<build dir>/tests.json`: {' '.join(unconf)}. "
                        "Planter's `test_verifier` runs `fx test --dry` and fails with "
                        "`missing_fx_test_target` when `<build dir>/tests.json` is non-empty and "
                        "does not contain the target.",
                        "Replace `Test: fx test ...` in the commit message and `CoderReport.test_footers` "
                        "with `Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` (for device "
                        "tests) or `Test: fx bazel test --config=host @//<dir>:<test>` (for host tests).",
                        severity="error",
                    )
            for pkg_d, t_name in re.findall(r"(?<![@\w])//([\w./\-]+):([\w.\-]+)", cmd):
                pkg_d = pkg_d.strip("/")
                try:
                    with open(f"{workdir}/{pkg_d}/BUILD.gn") as fh:
                        gn_txt = fh.read()
                except OSError:
                    continue
                if t_name == "tests" and re.search(r'\bgroup\(\s*"tests"\s*\)', gn_txt):
                    add(
                        "test_footer_gn_tests_group_label",
                        ln,
                        f"`Test:` footer `{cmd}` passes GN `group(\"tests\")` label "
                        f"`//{pkg_d}:tests` to `fx test`, which fails `fx test --dry` "
                        "because `<build dir>/tests.json` only indexes leaf test targets, "
                        "not GN groups.",
                        f"Use `Test: fx test //{pkg_d}:<test_target>` or "
                        f"`Test: fx test //{pkg_d}` (without `:tests`), which selects "
                        "both the Bazel `bazel_test_suite` test and the GN test.",
                        severity="error",
                    )
                    continue
                if pkg_d not in dirs:
                    continue
                try:
                    with open(f"{workdir}/{pkg_d}/BUILD.bazel") as fh:
                        bz_txt = fh.read()
                except OSError:
                    bz_txt = ""
                in_bazel = bool(
                    re.search(r'\bname\s*=\s*"' + re.escape(t_name) + r'"', bz_txt)
                    or re.search(r'"//' + re.escape(f"{pkg_d}:{t_name}") + r'"', gn_txt)
                    or (
                        t_name.endswith("_test")
                        and re.search(r'\bname\s*=\s*"' + re.escape(t_name[:-5]) + r'"', bz_txt)
                    )
                )
                if (
                    not in_bazel
                    and re.search(r'\bgroup\(\s*"tests"\s*\)', gn_txt)
                    and re.search(r"\bbazel_test_suite\(", gn_txt)
                    and re.search(r'\(\s*"' + re.escape(t_name) + r'"\s*\)', gn_txt)
                ):
                    add(
                        "test_footer_gn_only_test_label",
                        ln,
                        f"`Test:` footer `{cmd}` passes the GN-only test target "
                        f"`//{pkg_d}:{t_name}` rather than `//{pkg_d}`, "
                        "which only selects the GN test in `BUILD.gn`.",
                        f"Use `Test: fx test //{pkg_d}` (without `:tests`) so `fx test` "
                        "selects all tests under the package in `tests.json` (both "
                        "`bazel_test_suite` and GN tests).",
                    )
    # Blank out GN `#` comments outside string literals while preserving offsets.
    def strip_gn_comments(gn_text):
        return re.sub(
            r'("(?:\\.|[^"\\])*")|#[^\n]*',
            lambda m: m.group(1) or " " * len(m.group(0)),
            gn_text,
        )

    # Check that `fx test` footers do not reference build-time GN action targets
    # (which belong in `fx build [--host] //<pkg>:<target>`) or non-existent targets.
    def collect_build_target_catalog(rel_paths):
        gn_actions = {}
        all_known = set()
        gn_action_rules = {"action", "compiled_action", "python_action"}
        for rel_p in sorted(rel_paths):
            abs_p = os.path.join(workdir, rel_p)
            if not os.path.isfile(abs_p):
                continue
            pkg_p = os.path.dirname(rel_p).strip("/")
            bname = os.path.basename(rel_p)
            try:
                with open(abs_p, encoding="utf-8", errors="replace") as fh:
                    txt = fh.read()
            except OSError:
                continue
            if bname == "BUILD.gn":
                clean_gn = strip_gn_comments(txt)
                for m_t in re.finditer(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', clean_gn, re.M):
                    r_name, t_name = m_t.group(1), m_t.group(2)
                    all_known.add(t_name)
                    if r_name in gn_action_rules:
                        gn_actions[t_name] = (pkg_p, r_name)
            elif bname in ("BUILD.bazel", "BUILD"):
                for t_name in re.findall(r'\bname\s*=\s*"([^"]+)"', txt):
                    all_known.add(t_name)
                    if re.search(r"with_(?:host_)?unit_tests", txt):
                        all_known.add(f"{t_name}_test")
        return gn_actions, all_known

    def extract_fx_test_tokens(cmd_str):
        m_fx = re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b(.*)$", cmd_str)
        if not m_fx:
            return []
        rest = m_fx.group(1).strip()
        value_flags = {"-t", "-c", "--count", "--test-filter", "--parallel-cases", "--min-severity-logs"}
        toks = rest.split()
        out = []
        skip_next = False
        for tok in toks:
            if skip_next:
                skip_next = False
                continue
            if tok in value_flags:
                skip_next = True
                continue
            if tok.startswith("-"):
                continue
            if tok in ("&&", "||", ";", "|"):
                break
            out.append(tok.rstrip(".,;"))
        return out

    def check_fx_test_footer_targets(test_footer_lines, changed_files):
        fx_cmds = [
            (ln_no, raw_l[len("Test:") :].strip())
            for ln_no, raw_l in test_footer_lines
            if re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b", raw_l[len("Test:") :])
        ]
        if not fx_cmds:
            return
        build_files = {
            f
            for f in changed_files
            if os.path.basename(f) in ("BUILD.gn", "BUILD.bazel", "BUILD")
        }
        # Check whether a Bazel cutover test target was unwired in base BUILD.gn and
        # remains unexported to any ancestor test_suite / bazel_test_suite (so fx test --dry
        # cannot find it in tests.json).
        def is_unwired_cutover_fx_test(pkg_p, t_name):
            if os.path.isfile(os.path.join(workdir, pkg_p, "BUILD.gn")):
                return False
            base_gn = git("show", f"{base_rev}:{pkg_p}/BUILD.gn")
            if not base_gn:
                return False
            decl_pat = (
                r'\b(?:fuchsia_unittest_package|fuchsia_test_package)\(\s*"'
                + re.escape(t_name)
                + r'"\s*\)'
            )
            if not re.search(decl_pat, base_gn):
                return False
            if not re.search(r'\bgroup\(\s*"(?:tests|hermetic_tests)"\s*\)', base_gn):
                return False
            if re.search(r'":' + re.escape(t_name) + r'(?:\(|")', base_gn):
                return False
            parts = [p for p in pkg_p.split("/") if p]
            for i in range(len(parts), 0, -1):
                anc = "/".join(parts[:i])
                for fn in ("BUILD.bazel", "BUILD.gn"):
                    anc_p = os.path.join(workdir, anc, fn)
                    if not os.path.isfile(anc_p):
                        continue
                    try:
                        anc_txt = open(anc_p, encoding="utf-8").read()
                    except OSError:
                        continue
                    if f"//{pkg_p}:{t_name}" in anc_txt:
                        return False
                    if t_name == os.path.basename(pkg_p) and f'"//{pkg_p}"' in anc_txt:
                        return False
            return True

        gn_actions, _ = collect_build_target_catalog(build_files)
        for ln_no, cmd_str in fx_cmds:
            for tok in extract_fx_test_tokens(cmd_str):
                m_lbl = re.match(r"^@?//([\w./\-]+):([\w.\-]+)$", tok)
                if m_lbl:
                    pkg_p, t_name = m_lbl.group(1).strip("/"), m_lbl.group(2)
                    pkg_files = [
                        f"{pkg_p}/{fn}"
                        for fn in ("BUILD.gn", "BUILD.bazel", "BUILD")
                        if os.path.isfile(os.path.join(workdir, pkg_p, fn))
                    ]
                    if not pkg_files:
                        continue
                    pkg_actions, pkg_known = collect_build_target_catalog(pkg_files)
                    if t_name in pkg_actions and t_name not in (pkg_known - set(pkg_actions)):
                        _, r_name = pkg_actions[t_name]
                        msg_t = (
                            f"`Test:` footer `{cmd_str}` passes GN `{r_name}()` target "
                            f"`//{pkg_p}:{t_name}` to `fx test`, but build-time GN actions "
                            "are not runnable entries in `tests.json`."
                        )
                        rem_t = (
                            f"Verify build-time actions with `fx build //{pkg_p}:{t_name}` (add "
                            "`--host` only when the action is configured in the host toolchain; "
                            "build_verification rejects `fx build` labels missing from the configured "
                            "toolchain) rather than listing them in an `fx test` footer."
                        )
                        add("test_footer_non_test_target", ln_no, msg_t, rem_t, severity="error")
                    elif t_name not in pkg_known and t_name != os.path.basename(pkg_p):
                        msg_t = (
                            f"`Test:` footer `{cmd_str}` references `//{pkg_p}:{t_name}`, "
                            f"which is not declared in `{pkg_p}/BUILD.bazel` or `{pkg_p}/BUILD.gn`."
                        )
                        rem_t = (
                            f"Use the exact test target name declared in `{pkg_p}/BUILD.bazel` or "
                            f"`{pkg_p}/BUILD.gn` (and ensure Bazel host/device tests are wired into "
                            "`tests.json` before invoking `fx test`, or run `fx bazel test`)."
                        )
                        add("test_footer_unknown_target", ln_no, msg_t, rem_t, severity="error")
                    elif is_unwired_cutover_fx_test(pkg_p, t_name):
                        msg_t = (
                            f"`Test:` footer `{cmd_str}` passes `//{pkg_p}:{t_name}` to "
                            f"`fx test`, but `//{pkg_p}:{t_name}` was unwired in base "
                            f"`{pkg_p}/BUILD.gn` and is not exported by any `bazel_test_suite`, "
                            "so `fx test --dry` cannot find it in `tests.json`."
                        )
                        rem_t = (
                            f"Remove `//{pkg_p}:{t_name}` from `Test: fx test` (verify unwired "
                            "cutover test packages with `fx bazel build --config=fuchsia "
                            f"//{pkg_p}:{t_name}` instead), or wire `//{pkg_p}:{t_name}` into "
                            "the ancestor `test_suite` / `bazel_test_suite`."
                        )
                        add("test_footer_unwired_fx_test_target", ln_no, msg_t, rem_t, severity="error")
                elif re.match(r"^[A-Za-z0-9_.\-]+$", tok):
                    if tok in gn_actions:
                        pkg_p, r_name = gn_actions[tok]
                        msg_t = (
                            f"`Test:` footer `{cmd_str}` passes build-time GN `{r_name}()` target "
                            f"`{tok}` (`//{pkg_p}:{tok}`) to `fx test`, which only runs entries "
                            "registered in `tests.json`."
                        )
                        rem_t = (
                            f"Remove `{tok}` from `fx test` and verify build-time actions with "
                            f"`fx build //{pkg_p}:{tok}` (add `--host` only when the action is "
                            "configured in the host toolchain), or run migrated Bazel tests via "
                            "`fx bazel test --config=host @//<pkg>:<test>`."
                        )
                        add("test_footer_non_test_target", ln_no, msg_t, rem_t, severity="error")

    check_fx_test_footer_targets(test_lines, changed)
    # Test footers: prefer an ancestor test_suite() that includes the migrated tests.
    suites, has_tests = [], False
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                own = fh.read()
            has_tests |= bool(re.search(r'with_host_unit_tests\s*=\s*True|with_unit_tests\s*=\s*"(host|both|fuchsia)"'
                                        r'|^\s*(host_\w*test|rustc_test|fx_test|fx_unittest_package|test_suite)\(', own, re.M))
        except OSError:
            pass
        parts = d.split("/")
        for i in range(len(parts) - 1, 0, -1):
            anc = "/".join(parts[:i])
            try:
                with open(f"{workdir}/{anc}/BUILD.bazel") as fh:
                    text = fh.read()
            except OSError:
                continue
            for blk in re.findall(r"^test_suite\((.*?)^\)", text, re.M | re.S):
                n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
                if n and re.search(r'"@?//' + re.escape(d) + r'[:"]', blk):
                    suites.append((anc, n.group(1)))
    def names_suite(l, a, n):
        return f"//{a}:{n}" in l or (n == a.rsplit("/", 1)[-1] and re.search(r"//" + re.escape(a) + r"(?![\w/:.-])", l))
    if suites and test_lines and not any(names_suite(l, a, n) for _, l in test_lines for a, n in suites):
        a, n = suites[-1]
        add("test_footer_not_ancestor_suite", test_lines[0][0],
            f"`Test:` footers run the migrated tests directly; the ancestor `test_suite()` //{a}:{n} also includes them.",
            f"When all tests in `@{a}:{n}` pass, you may use `Test: fx bazel test --config=host @//{a}:{n}` (or keep `@//<dir>:<test>` when an unrelated test in `//{a}:{n}` fails on RBE).",
            severity="info")
    if dirs and has_tests and not suites:
        add("migrated_tests_not_in_ancestor_suite", 1,
            "No ancestor BUILD.bazel `test_suite()` includes the migrated tests.",
            "When an ancestor directory already defines a wired `test_suite()` / `bazel_test_suite`, add the migrated "
            "tests there. When no ancestor `bazel_test_suite` exists yet above the package, place `bazel_test_suite` "
            "at the nearest `group(\"tests\")` boundary (do not invent a new top-level area `BUILD.bazel` suite unless "
            "requested).", severity="info")
    # Body: changes not specific to the migrated targets must be described.
    planter_banner = (
        "[DO NOT SUBMIT/REVIEW] waiting on uploader acknowledgement of planter findings"
    )
    no_ifttt_re = re.compile(r"^NO_IFTTT\s*[:=]", re.I)
    body = "\n".join(
        l
        for l in lines[1:]
        if l.strip() != planter_banner
        and not re.match(
            r"^(Test|Bug|Fixed|Bazel-Migration-Target|Build-Only|Deleted-Tests|"
            r"Renamed-Tests|Skipped-Dual-Tests|Skip-Readme-Check|TAG|CONV|"
            r"Change-Id|Multiply|Cq-[\w-]+):",
            l,
        )
        and not no_ifttt_re.match(l.strip())
    )
    # Check that the commit body does not restate the [bazel_migration] //<dir> subject line,
    # including when wrapped across multiple lines or when the subject abbreviates with //.../
    footer_line_re = re.compile(
        r"^(Test|Bug|Fixed|Bazel-Migration-Target|Build-Only|Deleted-Tests|Renamed-Tests|"
        r"Skipped-Dual-Tests|Skip-Readme-Check|TAG|CONV|Change-Id|"
        r"Multiply|Cq-[\w-]+|Reviewed-on|Reviewed-by|Commit-Queue|Fuchsia-Auto-Submit|"
        r"Presubmit-Verified|API-Review):"
    )
    redundant_body_para_re = re.compile(
        r"^(?:\[[\w.\-]+\]\s*)+(?:migrates?\b.*|@?//[\w./\-]+\.?)$"
        r"|^@?//[\w./\-]+\.?$"
        r"|^(?:this\s+(?:cl|change|commit|patch)\s+)?migrates?\s+(?:the\s+)?(?:@?//)?[\w./\-]+"
        r"(?:\s+(?:and\s+(?:its\s+)?(?:unit\s+)?tests?|targets?|package|library|binary))?"
        r"(?:\s+from\s+GN)?(?:\s+to\s+Bazel)?"
        r"(?:,?\s+and\s+(?:generates?|regenerates?|updates?|removes?|deletes?)\s+.*"
        r"(?:BUILD\.gn|GN\s+targets?|bazel2gn))?"
        r"(?:\s+(?:with|using)\s+(?:dual[- ]build(?:\s+support)?|bazel2gn)"
        r"(?:\s+(?:via|using|with)\s+bazel2gn)?)?\.?$",
        re.I,
    )

    def body_paragraphs(msg_lines):
        paragraphs = []
        cur_start, cur_end, cur_parts = None, None, []
        for idx, raw in enumerate(msg_lines[1:], start=2):
            if footer_line_re.match(raw):
                break
            stripped = raw.strip()
            if not stripped or no_ifttt_re.match(stripped):
                if cur_parts:
                    paragraphs.append((cur_start, cur_end, " ".join(cur_parts)))
                    cur_start, cur_end, cur_parts = None, None, []
                continue
            if cur_start is None:
                cur_start = idx
            cur_end = idx
            cur_parts.append(stripped)
        if cur_parts:
            paragraphs.append((cur_start, cur_end, " ".join(cur_parts)))
        return paragraphs

    def check_redundant_body_title(msg_lines):
        for start_ln, end_ln, para in body_paragraphs(msg_lines):
            if redundant_body_para_re.match(para):
                span = f"lines {start_ln}-{end_ln}" if end_ln > start_ln else f"line {start_ln}"
                msg_text = (
                    f"Commit body ({span}: `{para}`) merely restates the "
                    "`[bazel_migration] //<dir>` subject line without adding information "
                    "(even when the subject abbreviates the path with `//.../`)."
                )
                rem_text = (
                    "Remove the entire redundant `Migrate //<dir>...` paragraph from the "
                    "commit body (leave the body empty when no non-obvious changes need "
                    "explanation; the full path is already recorded in "
                    "`Bazel-Migration-Target:`)."
                )
                add("commit_body_redundant_title", end_ln, msg_text, rem_text)
                break

    # Inspect whether a package directory still has unmigrated GN targets in BUILD.gn
    # or only adds exports_files() without migrating the entire directory.
    gn_meta_constructs = {
        "group",
        "config",
        "bazel_test_suite",
        "bazel_host_tool",
        "bazel_ffx_tool",
        "bazel_fuchsia_package",
        "host_test_data",
        "sdk_atom",
        "declare_args",
        "import",
        "set_defaults",
        "template",
        "foreach",
        "if",
        "assert",
    }
    gn_test_only_rules = {
        "fuchsia_test_package",
        "fuchsia_unittest_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "fuchsia_test",
        "rustc_test",
        "test",
        "go_test",
        "host_test",
        "host_test_data",
        "python_host_test",
        "bootfs_test",
    }
    gn_test_data_helper_rules = {
        "copy",
        "host_test_data",
        "resource",
        "fuchsia_package_resource_group",
        "generated_file",
    }
    bz_test_or_data_re = re.compile(
        r"^\s*(?:host_test_data_\w+|wrap_host_rust_test|host_\w*test|"
        r"rustc_test|cc_test|go_test|fx_unittest_package|fx_test)\s*\(",
        re.M,
    )

    # Yield `(rule, name, body)` for GN target declarations `rule("name") { ... }`,
    # ignoring GN built-in function calls such as `get_target_outputs(":foo")` that
    # have no trailing `{ ... }` block.
    def iter_gn_targets(gn_text):
        clean = strip_gn_comments(gn_text)
        for m in re.finditer(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{', clean, re.M):
            rule, gname = m.group(1), m.group(2)
            i = m.end()
            depth = 1
            in_str = False
            while i < len(clean) and depth > 0:
                ch = clean[i]
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
                i += 1
            yield rule, gname, clean[m.end() : max(m.end(), i - 1)]

    # Check whether a pre-sentinel GN target only supplies GN test/runtime data for
    # dual-built or GN-kept tests when BUILD.bazel already migrates tests/test data.
    def is_gn_dual_build_test_helper(rule, gname, gbody, gtxt_clean, btxt_str, has_sentinel):
        if rule not in gn_test_data_helper_rules:
            return False
        if not bz_test_or_data_re.search(btxt_str):
            return False
        if re.search(r"\btestonly\s*=\s*true\b", gbody):
            return True
        return bool(
            has_sentinel
            and not re.search(r"\bvisibility\s*=", gbody)
            and re.search(r"\b(?:test_runtime_deps|data_deps)\b", gtxt_clean)
            and f":{gname}" in gtxt_clean
        )

    # Check whether a target block in BUILD.bazel is preceded by `# @bazel2gn:skip`.
    def has_bazel2gn_skip(btxt_str, start_pos):
        prefix_lines = btxt_str[:start_pos].splitlines()
        idx = len(prefix_lines) - 1
        while idx >= 0:
            s = prefix_lines[idx].strip()
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    return True
                idx -= 1
                continue
            break
        return False

    # Read post-change file contents from disk, falling back to HEAD when a concurrent
    # check (such as tests_json_parity) has temporarily reverted the working tree.
    def read_committed_or_wt(rel_path, base_content=""):
        disk_path = os.path.join(workdir, rel_path)
        disk_txt = ""
        if os.path.isfile(disk_path):
            try:
                with open(disk_path) as fh:
                    disk_txt = fh.read()
            except OSError:
                disk_txt = ""
        if head and (not disk_txt or (rel_path in changed and disk_txt == base_content)):
            head_txt = git("show", f"{head}:{rel_path}")
            if head_txt:
                return head_txt
        return disk_txt

    def inspect_dir_migration(rel_dir):
        base_gtxt = git("show", f"{base_rev}:{rel_dir}/BUILD.gn")
        base_btxt = git("show", f"{base_rev}:{rel_dir}/BUILD.bazel")
        gtxt = read_committed_or_wt(f"{rel_dir}/BUILD.gn", base_gtxt)
        btxt = read_committed_or_wt(f"{rel_dir}/BUILD.bazel", base_btxt)
        pre_gtxt = gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if gtxt else ""
        suite_names = {
            gname
            for grule, gname, _ in iter_gn_targets(gtxt)
            if grule
            in ("bazel_test_suite", "bazel_host_tool", "bazel_ffx_tool", "bazel_fuchsia_package")
        }
        kept_pre_gn = {}
        for grule, gname, _ in iter_gn_targets(pre_gtxt):
            if grule not in gn_meta_constructs and gname not in suite_names:
                kept_pre_gn[gname] = grule
        bz_names = set()
        new_bz_labels = []
        base_bz_names = set()
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", base_btxt, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(0), re.M)
            if n:
                base_bz_names.add(n.group(1))
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", btxt, re.M | re.S):
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if not n:
                continue
            tname = n.group(1)
            if (
                tname in kept_pre_gn
                and has_bazel2gn_skip(btxt, m.start())
                and rule not in ("fx_test_component", "rustc_test", "go_test")
            ):
                continue
            bz_names.add(tname)
            if rule not in ("package", "exports_files", "fx_packaged_binary", "fx_unittest_component"):
                if tname not in base_bz_names:
                    new_bz_labels.append(f"//{rel_dir}:{tname}")
        if not gtxt:
            return [], [], new_bz_labels
        unmigrated_all = []
        unmigrated_non_test = []
        has_sentinel = "## BAZEL2GN SENTINEL" in gtxt
        gtxt_clean = strip_gn_comments(gtxt)
        for rule, gname, gbody in iter_gn_targets(gtxt):
            if rule in gn_meta_constructs or gname in bz_names or gname in suite_names:
                continue
            if is_gn_dual_build_test_helper(
                rule, gname, gbody, gtxt_clean, btxt, has_sentinel
            ):
                continue
            item = f'{rule}("{gname}")'
            if item not in unmigrated_all:
                unmigrated_all.append(item)
            if (
                rule not in gn_test_only_rules
                and not (
                    has_sentinel
                    and rule in ("fuchsia_package", "fuchsia_package_with_single_component")
                )
                and item not in unmigrated_non_test
            ):
                unmigrated_non_test.append(item)
        return unmigrated_all, unmigrated_non_test, new_bz_labels

    # Resolve a subject directory that may use a leading `.../` abbreviation, and determine
    # which package directories actually migrate targets in this commit against base_rev.
    all_cand_dirs = list(
        dict.fromkeys(
            dirs
            + [
                os.path.dirname(f)
                for f in changed
                if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn") and os.path.dirname(f)
            ]
        )
    )

    def resolve_subject_dir(spec):
        s = spec.split(":", 1)[0].strip("/")
        if s.startswith(".../"):
            suf = "/" + s[4:].strip("/")
            for cand in all_cand_dirs:
                if ("/" + cand).endswith(suf):
                    return cand
        return s

    def is_dir_migrated_in_commit(rel_dir):
        bz_rel = f"{rel_dir}/BUILD.bazel"
        if not (
            os.path.isfile(os.path.join(workdir, bz_rel))
            or (head and git("show", f"{head}:{bz_rel}"))
        ):
            return False
        _, _, new_bz_lbls = inspect_dir_migration(rel_dir)
        if new_bz_lbls:
            return True
        gn_rel = f"{rel_dir}/BUILD.gn"
        if gn_rel not in changed:
            return False
        base_gtxt = git("show", f"{base_rev}:{gn_rel}")
        if not base_gtxt:
            return False
        cur_gtxt = read_committed_or_wt(gn_rel, base_gtxt)
        if not cur_gtxt:
            return True
        if "## BAZEL2GN SENTINEL" in cur_gtxt and "## BAZEL2GN SENTINEL" not in base_gtxt:
            return True
        base_pre = base_gtxt.split("## BAZEL2GN SENTINEL", 1)[0]
        cur_pre = cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[0]
        base_t = {
            n
            for r, n, _ in iter_gn_targets(base_pre)
            if r not in gn_meta_constructs
        }
        cur_t = {
            n
            for r, n, _ in iter_gn_targets(cur_pre)
            if r not in gn_meta_constructs
        }
        return bool(base_t - cur_t)

    active_mig_dirs = [d for d in all_cand_dirs if is_dir_migrated_in_commit(d)]
    if active_mig_dirs:
        new_pkg_dirs = [
            d
            for d in active_mig_dirs
            if d.count("/") >= 2 and not git("show", f"{base_rev}:{d}/BUILD.bazel")
        ]
        first = (
            new_pkg_dirs[0]
            if (not dirs and new_pkg_dirs)
            else active_mig_dirs[0]
        )
    resolved_subj_dir = resolve_subject_dir(subj_path) if subj_path else ""

    # Check that commit message paths are relative to the Fuchsia checkout root
    # (never prefixed with .jiri_root/worktrees/<name>/ or host filesystem paths)
    # and that partially migrated or already-migrated directories are caught.
    non_checkout_path_re = re.compile(
        r"(?:@?//)?\.jiri_root/(?:worktrees/[^/\s]+/)?([\w./\-]*)"
        r"|(?<![\w/:.])/(?:usr|home|tmp|var|opt)/[\w./\-]+"
    )

    migrated_list_headers = {
        "Also migrated in this change:",
        "Migrated directories:",
        "Migrated packages:",
        "Migrated targets:",
    }

    def get_pkg_area(pkg: str) -> str:
        parts = [p for p in pkg.strip("/").split("/") if p]
        if not parts:
            return ""
        if parts[0] in ("src", "sdk", "tools", "zircon", "third_party") and len(parts) >= 2:
            return f"{parts[0]}/{parts[1]}"
        return parts[0]

    def third_party_project(pkg: str) -> str:
        parts = [p for p in pkg.strip("/").split("/") if p]
        if "third_party" not in parts:
            return ""
        idx = parts.index("third_party")
        return "/".join(parts[: idx + 2])

    # Check whether a package directory listed under `Also migrated in this change:`
    # belongs to the same subsystem area or coupled dependency graph as the change's
    # reference directories (`first`, `resolved_subj_dir`, and `dirs`), without
    # crossing `third_party/` project boundaries.
    def is_also_migrated_in_scope(b_dir: str) -> bool:
        ref_dirs = [
            d for d in dict.fromkeys([first, resolved_subj_dir] + dirs) if d and d != "<dir>"
        ]
        if not ref_dirs:
            return True
        b_tp = third_party_project(b_dir)
        for ref_d in ref_dirs:
            if third_party_project(ref_d) != b_tp:
                continue
            if get_pkg_area(b_dir) == get_pkg_area(ref_d):
                return True
        if b_tp:
            return False
        coupled_dirs = set(ref_dirs) | {
            d for d in active_mig_dirs if not third_party_project(d)
        }
        for other_d in sorted(coupled_dirs):
            if other_d == b_dir:
                continue
            for src_d, dst_d in ((b_dir, other_d), (other_d, b_dir)):
                try:
                    with open(os.path.join(workdir, src_d, "BUILD.bazel")) as fh:
                        btxt = fh.read()
                except OSError:
                    continue
                if re.search(r'"@?//' + re.escape(dst_d) + r'(?=[:"])', btxt):
                    return True
        return False

    def inspect_new_exports_files(rel_dir):
        bz_path = os.path.join(workdir, rel_dir, "BUILD.bazel")
        try:
            with open(bz_path) as fh:
                btxt = fh.read()
        except OSError:
            return []
        base_btxt = git("show", f"{base_rev}:{rel_dir}/BUILD.bazel")
        out = []
        for m in re.finditer(r"^exports_files\(\s*\[(.*?)\]", btxt, re.M | re.S):
            for exp in re.findall(r'"([^"]+)"', m.group(1)):
                if f'"{exp}"' not in base_btxt:
                    out.append(f"//{rel_dir}:{exp}")
        return out

    # Flag an `Also migrated in this change:` header that is not followed by any bullet.
    def emit_empty_also_migrated_header(header_idx, header_text):
        if header_idx is None:
            return
        add(
            "commit_body_redundant_also_migrated",
            header_idx,
            f"Commit body contains an empty `{header_text}` header with no bullets.",
            f"Remove the empty `{header_text}` line from the commit message body.",
            severity="error",
        )

    def check_checkout_relative_paths(msg_lines):
        in_also_migrated = False
        header_idx = None
        header_text = ""
        header_has_bullets = False
        for idx, raw in enumerate(msg_lines, start=1):
            stripped = raw.strip()
            if stripped in migrated_list_headers:
                if in_also_migrated and not header_has_bullets:
                    emit_empty_also_migrated_header(header_idx, header_text)
                in_also_migrated = True
                header_idx = idx
                header_text = stripped
                header_has_bullets = False
                continue
            if in_also_migrated and not stripped.startswith("- "):
                if not header_has_bullets:
                    emit_empty_also_migrated_header(header_idx, header_text)
                in_also_migrated = False
            m_bad = non_checkout_path_re.search(raw)
            if m_bad:
                bad_tok = m_bad.group(0)
                rel_sub = (m_bad.group(1) or "<dir>").strip("/") or "<dir>"
                msg_text = (
                    f"Commit message line {idx} contains non-checkout-relative path "
                    f"`{bad_tok}` (includes `.jiri_root`/worktree or host filesystem prefix)."
                )
                rem_text = (
                    f"Use a path relative to the Fuchsia checkout root (`//{rel_sub}`), "
                    "or remove the `Also migrated in this change:` block if no directories "
                    f"other than `//{first}` were migrated. Also ensure `modified_files` in "
                    "CoderReport uses checkout-relative paths (`<dir>/BUILD.bazel`, never "
                    "`.jiri_root/worktrees/<name>/...`)."
                )
                add("commit_msg_non_checkout_path", idx, msg_text, rem_text, severity="error")
                continue
            if in_also_migrated and stripped.startswith("- "):
                header_has_bullets = True
                bullet = stripped[2:].strip()
                b_spec = bullet[2:].strip("/") if bullet.startswith("//") else ""
                b_dir = b_spec.split(":", 1)[0].strip("/")
                if not b_spec or b_spec.startswith("."):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` is not a valid "
                        "Fuchsia checkout-relative `//<dir>` or `//<dir>:<target>` label."
                    )
                    rem_text = (
                        "Write each bullet as `- //<dir>` (when the entire directory is "
                        "migrated) or `- //<dir>:<target>` (when only specific targets in "
                        "`//<dir>` are migrated), or remove the block if no extra targets "
                        "were migrated."
                    )
                    add("commit_msg_non_checkout_path", idx, msg_text, rem_text, severity="error")
                elif b_dir in (first, resolved_subj_dir) and (
                    b_dir == resolved_subj_dir or len(active_mig_dirs) <= 1
                ):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` duplicates the "
                        f"primary migrated directory `//{b_dir}`."
                    )
                    rem_text = (
                        "Remove the redundant bullet (or the entire `Also migrated in this "
                        f"change:` block when only `//{first}` is migrated in this change), "
                        "and pass checkout-relative paths in `CoderReport.modified_files`."
                    )
                    add("commit_body_redundant_also_migrated", idx, msg_text, rem_text)
                elif not is_dir_migrated_in_commit(b_dir):
                    unmig_all, _, _ = inspect_dir_migration(b_dir)
                    if unmig_all:
                        sample = ", ".join(f"`{t}`" for t in unmig_all[:3])
                        msg_text = (
                            f"`Also migrated in this change:` bullet `{bullet}` names "
                            f"`//{b_dir}`, while `{b_dir}/BUILD.gn` still defines unmigrated "
                            f"targets ({sample}) and no targets in `//{b_dir}` are migrated in "
                            "this change (only `exports_files()` / `visibility` wiring was added)."
                        )
                        rem_text = (
                            f"Drop `- {bullet}` from `Also migrated in this change:` (and "
                            "remove the `Also migrated in this change:` section if only "
                            f"`//{first}` is migrated in this change); exporting files via "
                            "`exports_files()` or updating `visibility` does not migrate "
                            f"`//{b_dir}`."
                        )
                        add(
                            "also_migrated_partial_directory",
                            idx,
                            msg_text,
                            rem_text,
                            severity="error",
                        )
                    else:
                        msg_text = (
                            f"`Also migrated in this change:` bullet `{bullet}` names "
                            f"`//{b_dir}`, which was already migrated before this commit (no "
                            f"targets in `//{b_dir}` are newly migrated against base)."
                        )
                        rem_text = (
                            f"Remove `- {bullet}` from `Also migrated in this change:` (and "
                            "remove the entire `Also migrated in this change:` block if only "
                            f"`//{first}` is migrated in this change); `exports_files()` / "
                            f"`visibility` updates in `//{b_dir}` are wiring edits rather than "
                            f"newly migrated targets (revert `{b_dir}/` edits only if they are "
                            "unrelated rebase leftovers)."
                        )
                        add(
                            "also_migrated_already_migrated",
                            idx,
                            msg_text,
                            rem_text,
                            severity="error",
                        )
                elif first and not is_also_migrated_in_scope(b_dir):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` names "
                        f"`//{b_dir}`, which is outside the subsystem area or third-party "
                        f"scope of `//{first}` (`//build/beads/references/migration/scoping_changelists.md`)."
                    )
                    rem_text = (
                        "Per `//build/beads/references/migration/scoping_changelists.md`, "
                        "never combine directories from different areas or across "
                        f"`third_party/` boundaries in one CL. Revert the out-of-scope "
                        f"changes in `{b_dir}/` and remove `- {bullet}` from `Also migrated "
                        "in this change:`."
                    )
                    add(
                        "also_migrated_out_of_scope",
                        idx,
                        msg_text,
                        rem_text,
                        severity="error",
                    )
                elif ":" not in b_spec:
                    unmig_all, _, new_bz_lbls = inspect_dir_migration(b_dir)
                    if unmig_all:
                        sample = ", ".join(f"`{t}`" for t in unmig_all[:3])
                        examples = (
                            ", ".join(f"`- {lbl}`" for lbl in new_bz_lbls[:4])
                            if new_bz_lbls
                            else f"`- //{b_dir}:<target>`"
                        )
                        msg_text = (
                            f"`Also migrated in this change:` bullet `{bullet}` names the "
                            f"entire directory `//{b_dir}`, but `{b_dir}/BUILD.gn` still "
                            f"defines unmigrated targets ({sample})."
                        )
                        rem_text = (
                            f"Do not imply the entire `//{b_dir}` directory is migrated: "
                            "be specific about which targets are migrated by writing "
                            f"`- //{b_dir}:<target>` (e.g. {examples}), or remove the bullet "
                            "if no targets in that directory were migrated."
                        )
                        add(
                            "also_migrated_partial_directory",
                            idx,
                            msg_text,
                            rem_text,
                            severity="error",
                        )
        if in_also_migrated and not header_has_bullets:
            emit_empty_also_migrated_header(header_idx, header_text)

    # Check that a bare directory subject does not name a directory that was already migrated
    # before this commit or imply an entire directory is migrated when BUILD.gn still retains
    # unmigrated non-test targets.
    def check_subject_partial_directory(subj_str):
        m_subj = re.match(
            r"^\[bazel_migration\](?:\[[A-Za-z0-9_.\-]+\])? //([^\s,:]+(?::[^\s,]+)?)(\s+targets)?$",
            subj_str,
        )
        if not m_subj:
            return
        raw_spec = m_subj.group(1).strip("/")
        has_targets_suffix = bool(m_subj.group(2))
        subj_dir = resolve_subject_dir(raw_spec)
        if active_mig_dirs and subj_dir and not is_dir_migrated_in_commit(subj_dir):
            msg_text = (
                f"Commit subject `{subj_str}` names `//{subj_dir}`, which was already "
                f"migrated before this commit (no targets in `//{subj_dir}` are newly "
                f"migrated against base), whereas this change migrates `//{first}`."
            )
            rem_text = (
                f"Update the commit subject to name `//{first}` (the directory actually "
                f"migrated in this change), remove already-migrated directories from "
                "`Also migrated in this change:`, and revert any leftover rebase edits in "
                f"`{subj_dir}/`."
            )
            add(
                "commit_subject_already_migrated_directory",
                1,
                msg_text,
                rem_text,
                severity="error",
            )
            return
        if ":" in raw_spec or has_targets_suffix:
            return
        unmig_all, unmig_non_test, new_bz_lbls = inspect_dir_migration(subj_dir)
        cand_lbls = new_bz_lbls or inspect_new_exports_files(subj_dir)
        if unmig_non_test and cand_lbls:
            sample = ", ".join(f"`{t}`" for t in unmig_non_test[:3])
            ex_lbl = f"//{subj_dir} targets" if len(cand_lbls) > 1 else cand_lbls[0]
            if len(f"[bazel_migration] {ex_lbl}") <= 65:
                msg_text = (
                    f"Commit subject `{subj_str}` names the bare directory `//{subj_dir}`, "
                    f"but `{subj_dir}/BUILD.gn` still defines unmigrated targets ({sample})."
                )
                rem_text = (
                    f"Be specific about what is migrated (e.g. `{ex_lbl}`) instead "
                    f"of implying that the entire `//{subj_dir}` directory is migrated."
                )
                add("commit_subject_partial_directory", 1, msg_text, rem_text, severity="error")

    # Check that any file-local GN template("<name>") unrolled during migration is noted in the commit body.
    def check_unrolled_gn_template_explanation():
        for d in active_mig_dirs:
            gn_rel = f"{d}/BUILD.gn"
            base_gtxt = git("show", f"{base_rev}:{gn_rel}")
            if not base_gtxt:
                continue
            cur_gtxt = ""
            gn_path = os.path.join(workdir, gn_rel)
            if os.path.isfile(gn_path):
                try:
                    with open(gn_path) as fh:
                        cur_gtxt = fh.read()
                except OSError:
                    pass
            base_tmpls = set(re.findall(r'^\s*template\(\s*"([^"]+)"\s*\)', base_gtxt, re.M))
            cur_tmpls = set(re.findall(r'^\s*template\(\s*"([^"]+)"\s*\)', cur_gtxt, re.M))
            for tmpl_name in sorted(base_tmpls - cur_tmpls):
                if tmpl_name not in body:
                    msg_text = (
                        f"`{gn_rel}` defined GN `template(\"{tmpl_name}\")` which was unrolled into "
                        f"explicit targets in `{d}/BUILD.bazel`, but the commit body does not mention "
                        f"`{tmpl_name}`."
                    )
                    rem_text = (
                        f"Mention in the commit message body that GN `template(\"{tmpl_name}\")` was "
                        f"unrolled into explicit targets in `{d}/BUILD.bazel` (and note any unreferenced "
                        "template unit test targets if applicable)."
                    )
                    add("commit_body_unmentioned_unrolled_template", 1, msg_text, rem_text)

    # Check that any substantive source file fix (pre-existing bitrot repair or
    # test-only host_test_data runtime_dir path adaptation) is explained in the body.
    source_suffixes = (".rs", ".cc", ".cpp", ".c", ".h", ".hpp")
    ifttt_only_re = re.compile(r"^\s*(?://|#)\s*LINT\.(?:IfChange|ThenChange)\b")
    source_edit_reason_re = re.compile(
        r"\b(bitrot|unbuilt|not\s+built|disconnected|pre-existing|compile|build\s+error"
        r"|type\s+error|syntax|E0\d{3}|runtime_dir|host_test_data|test_data|testdata"
        r"|working\s+directory|cwd|clippy|include)\b",
        re.I,
    )

    def _is_ifttt_only_source_diff(rel_path):
        diff_out = git("diff", "-U0", base_rev, "--", rel_path) or ""
        mod = [
            l[1:].strip()
            for l in diff_out.splitlines()
            if (l.startswith("+") and not l.startswith("+++"))
            or (l.startswith("-") and not l.startswith("---"))
        ]
        mod = [l for l in mod if l]
        return bool(mod) and all(ifttt_only_re.match(l) for l in mod)

    def check_source_bitrot_explanation():
        for f in changed:
            if not f.endswith(source_suffixes) or _is_ifttt_only_source_diff(f):
                continue
            base_name = f.rsplit("/", 1)[-1]
            rel_names = [f, base_name]
            for d in dirs:
                if f.startswith(d + "/"):
                    rel_names.append(f[len(d) + 1:])
            mentioned = any(n in body for n in rel_names)
            has_reason = bool(source_edit_reason_re.search(body))
            if not (mentioned and has_reason):
                msg_text = (
                    f"The change modifies source file `{f}` (allowed by `build_only_scope` "
                    "only for narrow exceptions such as pre-existing bitrot in an unbuilt GN "
                    "package or `#[cfg(test)]` `host_test_data` `runtime_dir` path lookup), "
                    "but the commit body does not mention the file and explain why the source "
                    "edit was required."
                )
                rem_text = (
                    f"Explain the `{f}` edit in the commit body (naming `{base_name}` and "
                    "stating why Bazel required it, e.g. pre-existing compile bitrot in an "
                    "unbuilt GN package or `#[cfg(test)]` `runtime_dir` path fallback for "
                    "`host_test_data_files`), keeping the source diff strictly minimal."
                )
                add("commit_body_unmentioned_source_fix", 1, msg_text, rem_text)

    check_redundant_body_title(lines)
    check_checkout_relative_paths(lines)
    check_subject_partial_directory(subject)
    check_unrolled_gn_template_explanation()
    check_source_bitrot_explanation()

    # Check that planter's automatic [DO NOT SUBMIT/REVIEW] banner is never wrapped
    # across multiple lines, altered, or duplicated in the commit body.
    planter_banner_re = re.compile(
        r"^\[DO NOT SUBMIT/REVIEW\]\s+waiting\s+on\s+uploader\s+acknowledgement\s+"
        r"of\s+planter\s+findings\.?$",
        re.I,
    )

    def check_planter_do_not_submit_banner(msg_lines):
        seen_exact = False
        for start_ln, end_ln, para in body_paragraphs(msg_lines):
            is_banner_para = bool(planter_banner_re.match(para)) or (
                para.startswith("[DO NOT SUBMIT/REVIEW]")
                and "planter" in para.lower()
            )
            if not is_banner_para:
                continue
            if end_ln > start_ln:
                msg_text = (
                    f"Commit message lines {start_ln}-{end_ln} contain a wrapped "
                    f"multi-line copy of the `{planter_banner}` banner. Planter "
                    "adds and strips that banner itself only when it is a single "
                    "unwrapped line; a wrapped copy will not be stripped and would "
                    "ship in the ready-for-review CL."
                )
                rem_text = (
                    f"Delete the wrapped multi-line copy of `{planter_banner}` "
                    "(and the blank line after it). Do not wrap or otherwise edit "
                    "planter's single-line banner."
                )
                add(
                    "commit_body_wrapped_planter_banner",
                    start_ln,
                    msg_text,
                    rem_text,
                    severity="error",
                )
            elif msg_lines[start_ln - 1] != planter_banner:
                msg_text = (
                    f"Commit message line {start_ln} alters planter's "
                    f"`{planter_banner}` banner (`{msg_lines[start_ln - 1]}`), "
                    "which prevents planter from stripping it automatically."
                )
                rem_text = (
                    f"Leave `{planter_banner}` on a single unmodified line with no "
                    "leading/trailing spaces or punctuation, or delete the altered "
                    "copy if the exact single-line banner is already present."
                )
                add(
                    "commit_body_wrapped_planter_banner",
                    start_ln,
                    msg_text,
                    rem_text,
                    severity="error",
                )
            elif seen_exact:
                msg_text = (
                    f"Commit message line {start_ln} duplicates the "
                    f"`{planter_banner}` banner."
                )
                rem_text = (
                    f"Delete the duplicate `{planter_banner}` line (and the blank "
                    "line after it) so the banner appears at most once."
                )
                add(
                    "commit_body_wrapped_planter_banner",
                    start_ln,
                    msg_text,
                    rem_text,
                    severity="error",
                )
            else:
                seen_exact = True

    check_planter_do_not_submit_banner(lines)

    # Check subject and body line lengths (mirroring scripts/shac/commit_msg_checker.py).
    exempt_subject_re = re.compile(r"^(?:Revert(?:\^\d+)?|Reland|fixup!|squash!|amend!) ")
    exempt_footer_re = re.compile(
        r"^(Bug|Fixed|Test|Bazel-Migration-Target|Build-Only|Deleted-Tests|Renamed-Tests|"
        r"Skipped-Dual-Tests|Skip-Readme-Check|Change-Id|"
        r"Cq-Include-Trybots|Fuchsia-Auto-Submit|Multiply|Depends-on|Run-All-Tests|"
        r"Exempt|No-Tree-Checks|No-Presubmit|No-Try|NO_IFTTT|TAG|CONV|Reviewed-on|Reviewed-by|"
        r"Commit-Queue|Presubmit-Verified|API-Review)[:=]",
        re.I,
    )

    def check_line_lengths(msg_lines):
        if msg_lines:
            subj = msg_lines[0]
            if len(subj) > 65 and not exempt_subject_re.match(subj):
                msg_text = (
                    f"Commit subject line exceeds 65 characters ({len(subj)} chars): "
                    f"{subj!r} (triggers shac `commit_msg` warning)."
                )
                rem_text = (
                    "Shorten the commit subject line to at most 65 characters "
                    "(recommended <= 50; abbreviate long middle path segments with "
                    "`//.../<leaf>` when needed while keeping the full target label "
                    "in `Bazel-Migration-Target:`)."
                )
                add("commit_subject_too_long", 1, msg_text, rem_text, severity="error")
        nonempty = [i for i, l in enumerate(msg_lines) if l.strip()]
        trailer_start = len(msg_lines)
        if nonempty:
            s = nonempty[-1]
            while s > 0 and msg_lines[s - 1].strip():
                s -= 1
            if any(
                exempt_footer_re.match(msg_lines[i].strip())
                or footer_line_re.match(msg_lines[i].strip())
                for i in range(s, nonempty[-1] + 1)
            ):
                trailer_start = s
        for idx, raw in enumerate(msg_lines[2:], start=3):
            if len(raw) <= 72:
                continue
            if raw.strip() == planter_banner:
                continue
            if re.search(r"https?://[^\s]+", raw):
                continue
            stripped = raw.strip()
            if no_ifttt_re.match(stripped):
                continue
            if idx - 1 >= trailer_start and (
                exempt_footer_re.match(stripped)
                or footer_line_re.match(stripped)
                or re.match(r"^[A-Za-z0-9\-]+\s*:\s*\S", stripped)
            ):
                continue
            if raw.startswith(("    ", "\t", ">", "#", "```")):
                continue
            msg_text = (
                f"Commit message line {idx} exceeds 72 characters ({len(raw)} chars): "
                f"`{raw}` (triggers shac `commit_msg` warning: "
                f"`Line {idx} exceeds 72 characters ({len(raw)} chars). "
                "Wrap body text at 72 characters.`)."
            )
            rem_text = (
                "Wrap commit message body lines at 72 characters or fewer "
                "(only URL lines, indented code blocks, planter's single-line "
                "`[DO NOT SUBMIT/REVIEW]` banner, preserved `NO_IFTTT=` "
                "lines, and single-line footers in the final footer block such "
                "as `Test:` and `Bazel-Migration-Target:` may exceed 72 characters)."
            )
            add("commit_body_line_too_long", idx, msg_text, rem_text, severity="error")

    check_line_lengths(lines)
    in_dirs = lambda f: any(f == d or f.startswith(d + "/") for d in dirs)
    for f in changed:
        if in_dirs(f) or not (f.endswith(".bzl") or f.rsplit("/", 1)[-1] == "MODULE.bazel"):
            continue
        if f not in body and f.rsplit("/", 1)[-1] not in body:
            add("commit_body_unmentioned_shared_change", 1,
                f"The change edits `{f}`, which is not specific to the migrated targets, but the commit body "
                "does not mention it.",
                f"Describe the `{f}` change and why it is needed in the body (one bullet per topic). Wiring edits "
                "(parent BUILD files, `*.gni` lists) need no mention. Keep the body in "
                "sync as changes are added or removed during review. First confirm the shared edit is needed at "
                "all (e.g. test-only files belong in `test_data`, not in a rule edit forwarding `data`).")
    # Body: name only the rules/macros actually called in the migrated BUILD.bazel files.
    called = set()
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                called |= set(re.findall(r"^\s*([A-Za-z_]\w*)\(", fh.read(), re.M))
        except OSError:
            pass
    if dirs and called:
        for rule in sorted(set(re.findall(r"(?<![\w.])(rust_(?:library|binary|test|proc_macro)|cc_(?:library|binary|test))(?![\w.])", body))):
            if rule not in called:
                add("commit_body_uncalled_rule", 1,
                    f"The commit body names `{rule}`, which no migrated BUILD.bazel calls directly.",
                    "Name only the rules/macros that define targets in BUILD.bazel, written as `rustc_library()`; "
                    "say which attribute generates implicit targets (e.g. `with_host_unit_tests = True` generates "
                    "\"lib_test\"). Refer to targets by name and give the crate name separately when different "
                    "(target \"lib\" (crate name \"foo\")); put identifiers in backticks or quotes.")
    # Body: a crate_name that differs from its target name must not be presented as a target name.
    crates = {}
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                text = fh.read()
        except OSError:
            continue
        for blk in re.findall(r"^[A-Za-z_]\w*\((.*?)^\)", text, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            c = re.search(r'^\s*crate_name\s*=\s*"([^"]+)"', blk, re.M)
            if n and c and n.group(1) != c.group(1):
                crates[c.group(1)] = n.group(1)
    for crate, tgt in sorted(crates.items()):
        for m in re.finditer(r"(?<![\w/.:-])" + re.escape(crate) + r"(_test)?(?![\w-])", body):
            if not re.search(r"\bcrate\b", body[max(0, m.start() - 40):m.end() + 20], re.I):
                add("commit_body_crate_name_as_target", 1,
                    f"The commit body names `{m.group(0)}` like a target, but `{crate}` is the crate_name of target "
                    f"\"{tgt}\" (its unit test target is \"{tgt}_test\").",
                    f"Refer to the target by name and give the crate name separately, e.g. target \"{tgt}\" "
                    f"(crate name \"{crate}\"), and say which attribute generates \"{tgt}_test\".")
                break
    b2g = [ln for ln, l in test_lines if re.search(r"\bfx bazel2gn\b|verify_bazel2gn|\bshac\s+check\b|\bfx\s+lint\b", l)]
    if b2g and test_lines and b2g[-1] != test_lines[-1][0]:
        add("test_footer_bazel2gn_not_last", b2g[-1],
            "The shac/bazel2gn verification `Test:` line is not last.",
            "Group build/test commands together and put `Test: fx lint` last.")
    # Bazel-Migration-Target: footers (required for every non-empty migration commit).
    # Scan both primary target_dir packages and any other BUILD.bazel packages modified
    # in the commit so co-migrated targets are recognized when target_dir only updates
    # exports_files().
    is_empty = not changed and not git("status", "--porcelain", "--ignore-submodules=all")
    bmt_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Bazel-Migration-Target\s*:", l)]
    migrated_targets = []
    required_bmt_targets = []
    fallback_dir_targets = []
    excluded_bmt = {}
    helper_rules = {
        "package",
        "exports_files",
        "alias",
        "config_setting",
        "fx_packaged_binary",
        "fx_unittest_component",
    }
    synth_component_rules = {"fx_component_manifest", "fx_test_component", "cml_shard"}
    # Bazel-only test-data helpers synthesized for migrated tests are not migrated GN
    # targets, so they need no `Bazel-Migration-Target:` footer.
    test_data_rules = {
        "host_test_data_files",
        "host_test_data_dirs",
        "host_test_data_map",
        "host_test_data",
        "test_data",
        "resource",
        "fuchsia_package_resource_group",
    }
    ut_re = r'^\s*(with_unit_tests\s*=\s*"(host|both|fuchsia)"|with_unit_tests\s*=\s*True|with_host_unit_tests\s*=\s*True)'
    bmt_dirs = list(
        dict.fromkeys(
            dirs
            + [
                os.path.dirname(f)
                for f in changed
                if f.endswith("/BUILD.bazel") and os.path.dirname(f)
            ]
        )
    )
    kept_gn_by_dir = {}
    for d in bmt_dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                btxt = fh.read()
        except OSError:
            continue
        try:
            with open(f"{workdir}/{d}/BUILD.gn") as fh:
                cur_gtxt = fh.read()
        except OSError:
            cur_gtxt = ""
        pre_gtxt = cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if cur_gtxt else ""
        cur_suite_names = {
            gname
            for grule, gname, _ in iter_gn_targets(cur_gtxt)
            if grule
            in ("bazel_test_suite", "bazel_host_tool", "bazel_ffx_tool", "bazel_fuchsia_package")
        }
        kept_gn_targets = {}
        for grule, gname, _ in iter_gn_targets(pre_gtxt):
            if grule not in gn_meta_constructs and gname not in cur_suite_names:
                kept_gn_targets[gname] = grule
        kept_gn_by_dir[d] = kept_gn_targets
        base_btxt = git("show", f"{base_rev}:{d}/BUILD.bazel")
        base_gtxt = git("show", f"{base_rev}:{d}/BUILD.gn")
        base_bazel_names = set()
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", base_btxt, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(0), re.M)
            if n:
                base_bazel_names.add(n.group(1))
                if re.search(ut_re, m.group(0), re.M):
                    base_bazel_names.add(f"{n.group(1)}_test")
        post_gtxt = (
            cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[1]
            if "## BAZEL2GN SENTINEL" in cur_gtxt
            else ""
        )
        base_ut_tmpls = {
            tname
            for grule, tname, gbody in iter_gn_targets(base_gtxt)
            if grule == "template"
            and re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", gbody)
        }
        base_gn_names = set()
        # Names GN defined before this commit. Unlike base_gn_names, this leaves out the
        # bazel2gn output below the sentinel, which also holds helper targets that exist
        # only because of the migration (e.g. `<stem>_manifest` or `.shard`).
        orig_gn_names = set()
        for grule, gname, gbody in iter_gn_targets(base_gtxt):
            if grule not in gn_meta_constructs:
                orig_gn_names.add(gname)
                if (
                    grule in base_ut_tmpls
                    or re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", gbody)
                ):
                    orig_gn_names.add(f"{gname}_test")
        for grule, gname, gbody in iter_gn_targets(base_gtxt + "\n" + post_gtxt):
            if grule not in gn_meta_constructs:
                base_gn_names.add(gname)
                if (
                    grule in base_ut_tmpls
                    or re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", gbody)
                ):
                    base_gn_names.add(f"{gname}_test")
        blocks = list(re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", btxt, re.M | re.S))
        explicit_names = set()
        test_wrapped_pkgs = set()
        wrapped_bins = set()
        wrapped_test_bins = set()
        for m in blocks:
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if n:
                explicit_names.add(n.group(1))
            if rule == "fx_test":
                p = re.search(r'^\s*package\s*=\s*":([^"]+)"', blk, re.M)
                if p:
                    test_wrapped_pkgs.add(p.group(1))
            if rule in ("host_test", "fx_packaged_binary", "fx_unittest_component"):
                b = re.search(r'^\s*binary\s*=\s*":([^"]+)"', blk, re.M)
                if b:
                    if b.group(1) not in base_gn_names:
                        wrapped_bins.add(b.group(1))
                    if rule in ("host_test", "fx_unittest_component") or re.search(
                        r"^\s*testonly\s*=\s*True\b", blk, re.M
                    ):
                        wrapped_test_bins.add(b.group(1))
            if rule == "wrap_host_rust_test":
                bn = re.search(r'^\s*binary_name\s*=\s*"([^"]+)"', blk, re.M)
                if bn:
                    wrapped_test_bins.add(bn.group(1))
            if rule == "fx_unittest_package":
                u = re.search(r'unit_tests\s*=\s*\[(.*?)\]', blk, re.S)
                if u:
                    for b in re.findall(r'":([^"]+)"', u.group(1)):
                        wrapped_test_bins.add(b)
                        if b not in base_gn_names:
                            wrapped_bins.add(b)
        all_dir_targets, new_dir_targets, dir_required_bmt = [], [], []
        new_dir_rules = {}
        for m in blocks:
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if not n:
                continue
            tname = n.group(1)
            lbl = f"//{d}:{tname}"
            is_testonly_helper = (
                rule in ("genrule", "filegroup", "copy_file")
                and bool(re.search(r"^\s*testonly\s*=\s*True\b", blk, re.M))
            )
            if (
                tname in kept_gn_targets
                and has_bazel2gn_skip(btxt, m.start())
                and rule not in ("fx_test_component", "rustc_test", "go_test")
            ):
                grule = kept_gn_targets[tname]
                excluded_bmt[lbl] = (
                    f'remains hand-written as `{grule}("{tname}")` above the BAZEL2GN SENTINEL '
                    f'in `{d}/BUILD.gn` (only copied/skipped in `BUILD.bazel`, not migrated from GN)'
                )
                continue
            is_synth_comp = rule in synth_component_rules and tname not in orig_gn_names
            if (
                rule in helper_rules
                or is_synth_comp
                or (rule == "fx_package" and tname in test_wrapped_pkgs)
                or tname in wrapped_bins
            ):
                excluded_bmt[lbl] = (
                    "is an internal Bazel packaging/test-wrapper helper target, "
                    "not a migrated GN target"
                )
                continue
            all_dir_targets.append(lbl)
            is_synth_test_data = (
                (rule in test_data_rules or is_testonly_helper)
                and tname not in base_gn_names
            )
            if tname not in base_bazel_names and not is_synth_test_data:
                new_dir_targets.append(lbl)
                new_dir_rules[lbl] = rule
                if (
                    tname in base_gn_names
                    and tname not in wrapped_test_bins
                    and rule != "test_suite"
                ):
                    dir_required_bmt.append(lbl)
            if re.search(ut_re, blk, re.M) and f"{tname}_test" not in explicit_names:
                tlbl = f"//{d}:{tname}_test"
                all_dir_targets.append(tlbl)
                if f"{tname}_test" not in base_bazel_names:
                    new_dir_targets.append(tlbl)
        for tname, grule in kept_gn_targets.items():
            lbl = f"//{d}:{tname}"
            if lbl not in excluded_bmt and tname not in explicit_names:
                excluded_bmt[lbl] = (
                    f'remains unmigrated as `{grule}("{tname}")` in `{d}/BUILD.gn`'
                )
        if new_dir_targets:
            for lbl in all_dir_targets:
                t_short = lbl.split(":", 1)[-1]
                if lbl not in new_dir_targets and t_short in base_bazel_names:
                    excluded_bmt[lbl] = "already existed in `BUILD.bazel` before this commit"
            migrated_targets.extend(new_dir_targets)
            if dir_required_bmt:
                required_bmt_targets.extend(dir_required_bmt)
            elif not base_gn_names:
                # A new package with one migrated target needs its footer, but an ancestor
                # `test_suite` (e.g. a new `target_tests` aggregator) is not a migrated target.
                non_suite = [l for l in new_dir_targets if new_dir_rules.get(l) != "test_suite"]
                if len(non_suite) == 1:
                    required_bmt_targets.extend(non_suite)
        elif d in dirs and is_dir_migrated_in_commit(d):
            fallback_dir_targets.extend(all_dir_targets)
        else:
            for lbl in all_dir_targets:
                excluded_bmt[lbl] = "already existed in `BUILD.bazel` before this commit"
    if not migrated_targets:
        migrated_targets.extend(fallback_dir_targets)

    # Detect a blocked change that migrates nothing: no Bazel build file is added or edited
    # (e.g. only a BUILD.gn comment naming the blocker), or an existing BUILD.bazel is only
    # cleaned up (e.g. comments/load paths) without migrating any new target from GN.
    # Neither a Bazel-Migration-Target footer nor a Test: footer building a newly migrated
    # Bazel target can exist, so don't demand them.
    bazel_file_re = re.compile(r"(?:^|/)(?:BUILD\.bazel|BUILD|MODULE\.bazel|[^/]+\.bzl)$")
    pending = [
        l[3:].strip()
        for l in git("status", "--porcelain", "--ignore-submodules=all").splitlines()
        if len(l) > 3
    ]
    has_new_bazel_file = any(
        os.path.basename(f) in ("BUILD.bazel", "BUILD")
        and not git("show", f"{base_rev}:{f}")
        for f in list(changed) + pending
    )
    nothing_migrated = (
        not is_empty
        and not migrated_targets
        and (
            not any(bazel_file_re.search(f) for f in list(changed) + pending)
            or (not active_mig_dirs and not has_new_bazel_file)
        )
    )
    if nothing_migrated:
        add(
            "commit_nothing_migrated",
            1,
            "The change migrates no new Bazel target, so "
            "`Bazel-Migration-Target:` and `fx bazel build` `Test:` footers are not required.",
            "Describe the blocker in the body and summary, drop `Bazel-Migration-Target:` footers, "
            "and keep only `Test:` footers for what was actually verified "
            "(e.g. `Test: fx lint`).",
            severity="info",
        )
    if not is_empty:
        migrated_targets = list(dict.fromkeys(migrated_targets))
        required_bmt_targets = list(dict.fromkeys(required_bmt_targets))
        footer_ln = test_lines[-1][0] if test_lines else max(1, len(lines))
        example_bmt = ", ".join(
            f"`Bazel-Migration-Target: {t}`"
            for t in (migrated_targets or [f"//{first}:<target>"])
        )
        requires_bmt_footer = bool(
            dirs or migrated_targets or required_bmt_targets or m_tag or m_none
        )
        if not bmt_lines and requires_bmt_footer and not nothing_migrated:
            add(
                "missing_bazel_migration_target_footer",
                footer_ln,
                "Commit message is missing `Bazel-Migration-Target:` footer(s) "
                "recording the migrated target label(s).",
                "Add one `Bazel-Migration-Target: //<dir>:<target>` (or "
                "`//<dir>`) footer line per migrated target in the single "
                "contiguous footer block at the bottom of the commit message "
                "(after `Test:` and before `TAG:`/`Change-Id:`, with no blank "
                "lines between footers and no line wrapping; "
                "`Bazel-Migration-Target:` lines may exceed 72 characters), "
                f"e.g. {example_bmt}.",
                severity="error",
            )
        build_file_bmt_re = re.compile(
            r"(?:^|/|:)(?:BUILD(?:\.gn|\.bazel)?|[\w.\-]+\.(?:gn|gni|bazel|bzl))$"
        )
        present_bmt = set()
        for ln, l in bmt_lines:
            val = l.split(":", 1)[1].strip()
            canon_val = val if ":" in val else f"{val}:{val.rsplit('/', 1)[-1]}"
            present_bmt.add(val)
            present_bmt.add(canon_val)
            wrapped = (
                ln < len(lines)
                and bool(lines[ln].strip())
                and not re.match(r"^[A-Za-z0-9\-]+\s*:", lines[ln])
            )
            leaf_tok = val.split(":", 1)[-1].rsplit("/", 1)[-1]
            is_build_filename = leaf_tok in ("BUILD.gn", "BUILD.bazel", "BUILD")
            if build_file_bmt_re.search(val) or is_build_filename:
                add(
                    "bazel_migration_target_footer_format",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names a build "
                    f"file (`{val}`) rather than a migrated target label.",
                    "Replace the file path with one "
                    "`Bazel-Migration-Target: //<dir>:<target>` line per "
                    f"migrated target, e.g. {example_bmt}.",
                )
            elif not re.match(r"^//[\w./\-]+(:[\w.\-]+)?$", val) or wrapped:
                add(
                    "bazel_migration_target_footer_format",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` must specify a "
                    "single canonical `//<dir>:<target>` (or `//<dir>`) target label "
                    "on one unwrapped line (without a leading `@` or `/BUILD.gn` suffix).",
                    "Write one unwrapped line per migrated target (may exceed "
                    f"72 characters), e.g. {example_bmt}.",
                )
            elif nothing_migrated:
                # Verify no footer claims a migration in a change without Bazel build files
                add(
                    "bazel_migration_target_not_migrated",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names `{val}`, but the change "
                    "adds or edits no Bazel build file, so nothing was migrated.",
                    "Remove the `Bazel-Migration-Target:` footer(s) and the `fx bazel build` "
                    "`Test:` footers for targets that do not exist in Bazel.",
                    severity="error",
                )
            elif canon_val in excluded_bmt:
                add(
                    "bazel_migration_target_not_migrated",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names `{val}`, "
                    f"which {excluded_bmt[canon_val]}.",
                    "List only targets migrated from GN in this commit, "
                    f"e.g. {example_bmt}.",
                    severity="error",
                )
        # Check that no migrated GN target is omitted when Bazel-Migration-Target footers exist.
        if bmt_lines and required_bmt_targets:
            missing_bmt = [
                lbl for lbl in required_bmt_targets if lbl not in present_bmt
            ]
            if missing_bmt:
                miss_list = ", ".join(f"`{lbl}`" for lbl in missing_bmt)
                miss_footers = ", ".join(
                    f"`Bazel-Migration-Target: {lbl}`" for lbl in missing_bmt
                )
                add(
                    "missing_bazel_migration_target_footer",
                    bmt_lines[-1][0],
                    "Commit message is missing `Bazel-Migration-Target:` "
                    f"footer(s) for migrated target(s): {miss_list}.",
                    f"Add {miss_footers} to the `Bazel-Migration-Target:` "
                    "footer block so every target migrated from GN in this "
                    "commit is recorded.",
                    severity="error",
                )

        all_changed = set(changed) | {f for f in git("diff", "--name-only", base_rev).splitlines() if f}

        # Check that a `Migrate ...` sentence in the commit body does not claim to migrate
        # targets that still remain hand-written in BUILD.gn or claim to migrate/sync BUILD.gn
        # when the package's BUILD.gn file was not touched by the commit.
        def check_body_unmigrated_targets(msg_lines, kept_by_dir, mig_lbls, changed_paths):
            for start_ln, end_ln, para in body_paragraphs(msg_lines):
                for sent in re.split(r"(?<=\.)\s+", para):
                    is_mig_sent = bool(
                        re.match(r"^(?:this\s+(?:cl|change|commit|patch)\s+)?migrates?\b", sent, re.I)
                    )
                    claims_gn_sync = bool(
                        re.search(r"\b(?:sync|regenerate)\s+BUILD\.gn\b|\bvia\s+fx\s+bazel2gn\b", sent, re.I)
                    )
                    if not (is_mig_sent or claims_gn_sync):
                        continue
                    clause = re.split(
                        r"\b(?:while\s+keeping|keeping|and\s+keep)\b", sent, maxsplit=1, flags=re.I
                    )[0]
                    for d, kmap in kept_by_dir.items():
                        gn_rel = f"{d}/BUILD.gn"
                        gn_untouched = (
                            os.path.isfile(os.path.join(workdir, gn_rel))
                            and gn_rel not in changed_paths
                        )
                        if is_mig_sent:
                            for kname, krule in kmap.items():
                                pat = (
                                    r'(?:[A-Za-z_]\w*\(\s*"'
                                    + re.escape(kname)
                                    + r'"\s*\)|`?'
                                    + re.escape(f":{kname}")
                                    + r'`?)'
                                )
                                if re.search(pat, clause):
                                    gn_note = (
                                        f" and `{gn_rel}` was not modified in this commit"
                                        if gn_untouched
                                        else ""
                                    )
                                    msg_text = (
                                        f'Commit body claims to migrate `{kname}`, which remains '
                                        f'hand-written as `{krule}("{kname}")` in `{gn_rel}`'
                                        f"{gn_note} (copied with `# @bazel2gn:skip` in "
                                        "`BUILD.bazel`, not migrated from GN)."
                                    )
                                    rem_text = (
                                        "Describe `# @bazel2gn:skip` targets whose GN definitions "
                                        f"remain in `{gn_rel}` as copied to `BUILD.bazel` while "
                                        f"keeping `{gn_rel}` unchanged (do not claim `Migrate ...` "
                                        f"or `sync BUILD.gn via fx bazel2gn` when `{gn_rel}` is "
                                        "untouched), and remove any unreferenced `# @bazel2gn:skip` "
                                        "duplicate targets from `BUILD.bazel`."
                                    )
                                    add(
                                        "commit_body_claims_unmigrated_target",
                                        start_ln,
                                        msg_text,
                                        rem_text,
                                        severity="error",
                                    )
                                    return
                        if claims_gn_sync and gn_untouched and (d in sent or len(kept_by_dir) == 1):
                            msg_text = (
                                f"Commit body claims to sync `{gn_rel}` via `fx bazel2gn`, "
                                f"but `{gn_rel}` was not modified in this commit."
                            )
                            rem_text = (
                                f"Describe the `# @bazel2gn:skip` targets in `{d}/BUILD.bazel` "
                                f"as copied to `BUILD.bazel` while keeping `{gn_rel}` unchanged, "
                                "and remove the false `sync BUILD.gn via fx bazel2gn` claim."
                            )
                            add(
                                "commit_body_claims_unmigrated_target",
                                start_ln,
                                msg_text,
                                rem_text,
                                severity="error",
                            )
                            return

        check_body_unmigrated_targets(lines, kept_gn_by_dir, migrated_targets, all_changed)
        skip_readme_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Skip-Readme-Check\s*:", l)]
        has_valid_skip_readme = False
        for ln, l in skip_readme_lines:
            if len(l) > 72:
                add("skip_readme_check_line_too_long", ln,
                    f"`Skip-Readme-Check:` footer `{l}` is {len(l)} characters, exceeding the 72-character upload limit.",
                    "Shorten it to `Skip-Readme-Check: No version change, Bazel build files only.` (<= 72 characters).",
                    severity="error")
            elif l.split(":", 1)[1].strip():
                has_valid_skip_readme = True
            else:
                add("skip_readme_check_empty_reason", ln,
                    "The `Skip-Readme-Check:` footer must be followed by a non-empty reason.",
                    "Write `Skip-Readme-Check: No version change, Bazel build files only.` in the contiguous footer block at the bottom of the commit message.",
                    severity="error")
        missing_readmes = set()
        for f in sorted(all_changed):
            if not f.startswith("third_party/") or f.startswith("third_party/rust_crates/"):
                continue
            parts = f.split("/")
            for depth in range(len(parts) - 1, 1, -1):
                cand = "/".join(parts[:depth])
                readme = f"{cand}/README.fuchsia"
                if os.path.isfile(f"{workdir}/{readme}"):
                    if f != readme and readme not in all_changed:
                        missing_readmes.add((cand, readme))
                    break
        if missing_readmes and not has_valid_skip_readme:
            cand, readme = sorted(missing_readmes)[0]
            add("missing_skip_readme_check_footer", footer_ln,
                f"Files under `{cand}` were modified without modifying `{readme}`, which fails the CQ `third_party_readme_update` shac check unless waived by a `Skip-Readme-Check:` footer.",
                "Because GN-to-Bazel migrations must not modify `README.fuchsia` (`build_only_scope`), add `Skip-Readme-Check: No version change, Bazel build files only.` to the contiguous footer block at the bottom of the commit message (before `TAG:`/`Change-Id:`).",
                severity="error")
        # Check for unwarranted Cq-Include-Trybots: footers when migrated build files
        # have no architecture-specific conditionals matching the requested trybot.
        def check_cq_include_trybots(msg_lines):
            cq_lines = [
                (i + 1, l)
                for i, l in enumerate(msg_lines)
                if re.match(r"^Cq-Include-Trybots\s*:", l, re.I)
            ]
            if not cq_lines:
                return
            build_blobs = []
            for d in dirs:
                for fn in ("BUILD.bazel", "BUILD.gn"):
                    p = os.path.join(workdir, d, fn)
                    if os.path.isfile(p):
                        try:
                            build_blobs.append(open(p, encoding="utf-8").read())
                        except OSError:
                            pass
            combined_builds = "\n".join(build_blobs)
            for ln, l in cq_lines:
                arch_tokens = re.findall(r"\b(riscv64|arm64|x64)\b", l)
                if not arch_tokens or not all(
                    re.search(r"\b" + re.escape(tok) + r"\b", combined_builds)
                    for tok in arch_tokens
                ):
                    msg_text = (
                        f"Commit message includes `{l}`, which specifies extra trybot(s) not "
                        "motivated by architecture-specific conditions in the migrated build files."
                    )
                    rem_text = (
                        "Remove the `Cq-Include-Trybots:` footer line from the commit message; "
                        "default CQ presubmit builders already cover standard GN-to-Bazel "
                        "migrations."
                    )
                    add(
                        "unnecessary_cq_include_trybots_footer",
                        ln,
                        msg_text,
                        rem_text,
                        severity="error",
                    )

        check_cq_include_trybots(lines)
        if check_dirs and not any(d.startswith("src/connectivity/wlan") for d in check_dirs):
            for idx_b, raw_b in enumerate(lines, start=1):
                if "543568916" in raw_b:
                    add(
                        "wrong_subsystem_bug_footer",
                        idx_b,
                        f"Commit message line `{raw_b.strip()}` cites WLAN-only tracking bug `543568916` on a non-WLAN migration (`{', '.join('//' + d for d in check_dirs)}`).",
                        "Remove `Bug: 543568916` (or replace `543568916` with `570694755` or the verified open bug for the migrated subsystem).",
                        severity="error",
                    )

        deleted_tests_lines = [
            (i + 1, l)
            for i, l in enumerate(lines)
            if re.match(r"^Deleted-Tests\s*:", l, re.I)
        ]
        skipped_dual_lines = [
            (i + 1, l)
            for i, l in enumerate(lines)
            if re.match(r"^Skipped-Dual-Tests\s*:", l, re.I)
        ]
        renamed_tests_lines = [
            (i + 1, l)
            for i, l in enumerate(lines)
            if re.match(r"^Renamed-Tests\s*:", l, re.I)
        ]
        build_only_lines = [
            (i + 1, l)
            for i, l in enumerate(lines)
            if re.match(r"^Build-Only\s*:", l, re.I)
        ]

        def _gn_test_footprint(text):
            if not text:
                return []
            clean_gn = re.sub(
                r'("(?:\\.|[^"\\])*")|#[^\n]*',
                lambda m: m.group(1) or " " * len(m.group(0)),
                text,
            )
            test_tmpls = {
                "test",
                "cc_test",
                "rustc_test",
                "go_test",
                "python_host_test",
                "host_test",
                "fuchsia_unittest_package",
                "fuchsia_test_package",
                "fuchsia_unittest_component",
                "fuchsia_test_component",
                "bazel_test_suite",
                "standardized_test_groups",
            }
            test_groups = {
                "tests",
                "hermetic_tests",
                "non_hermetic_tests",
                "e2e_tests",
                "host_tests",
                "tests_no_e2e",
            }
            sigs = []
            for m_t in re.finditer(
                r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{', clean_gn
            ):
                tmpl_name, tgt_name = m_t.group(1), m_t.group(2)
                depth = 0
                in_s = False
                pos = m_t.end() - 1
                while pos < len(clean_gn):
                    ch = clean_gn[pos]
                    if in_s:
                        if ch == "\\":
                            pos += 2
                            continue
                        in_s = ch != '"'
                    elif ch == '"':
                        in_s = True
                    elif ch == "{":
                        depth += 1
                    elif ch == "}":
                        depth -= 1
                        if depth == 0:
                            break
                    pos += 1
                blk_body = clean_gn[m_t.end() : pos]
                norm_blk = re.sub(r"\s+", " ", blk_body).strip()
                if tmpl_name in test_tmpls or (
                    tmpl_name == "group" and tgt_name in test_groups
                ):
                    sigs.append((tmpl_name, tgt_name, norm_blk))
                elif re.search(
                    r"\bwith_(?:host_)?unit_tests\s*=", blk_body
                ):
                    ut_flags = re.findall(
                        r"\bwith_(?:host_)?unit_tests\s*=\s*\w+", blk_body
                    )
                    sigs.append(
                        (tmpl_name, tgt_name, ";".join(sorted(ut_flags)))
                    )
            return sorted(sigs)

        def _bazel_test_footprint(text):
            if not text:
                return []
            import ast

            bz_test_rules = {
                "fx_unittest_package",
                "fx_unittest_component",
                "fuchsia_unittest_package",
                "fuchsia_unittest_component",
                "fuchsia_test_package",
                "fuchsia_test_component",
                "fx_test",
                "fx_test_component",
                "rustc_test",
                "rust_test",
                "cc_test",
                "fx_cc_test",
                "go_test",
                "py_test",
                "host_test",
                "host_cc_test",
                "host_rustc_test",
                "host_go_test",
                "host_py_test",
                "sdk_host_test",
                "python_host_test",
                "wrap_host_rust_test",
                "test_suite",
            }
            try:
                tree = ast.parse(text)
            except SyntaxError:
                return [("SYNTAX_ERROR", re.sub(r"\s+", " ", text))]
            sigs = []
            for stmt in tree.body:
                if not isinstance(stmt, ast.Expr) or not isinstance(
                    stmt.value, ast.Call
                ):
                    continue
                call = stmt.value
                if isinstance(call.func, ast.Name):
                    rname = call.func.id
                elif isinstance(call.func, ast.Attribute):
                    rname = call.func.attr
                else:
                    continue
                kw_names = {kw.arg for kw in call.keywords if kw.arg}
                if rname in bz_test_rules:
                    sigs.append((rname, ast.dump(call)))
                elif kw_names & {"with_unit_tests", "with_host_unit_tests"}:
                    ut_kws = sorted(
                        (kw.arg, ast.dump(kw.value))
                        for kw in call.keywords
                        if kw.arg
                        in ("name", "with_unit_tests", "with_host_unit_tests")
                    )
                    sigs.append((rname, repr(ut_kws)))
            return sorted(sigs)

        def _read_after_file(rel_path):
            disk_path = os.path.join(workdir, rel_path)
            if os.path.isfile(disk_path):
                try:
                    with open(disk_path, encoding="utf-8", errors="replace") as fh:
                        return fh.read()
                except OSError:
                    pass
            return git("show", f"{head}:{rel_path}")

        def check_test_parity_footers(msg_lines, changed_files):
            has_planter_tag = any(
                re.match(r"^TAG\s*[:=]\s*.*\bplanter\b", raw, re.I)
                for raw in msg_lines
            )
            has_numeric_bug = any(
                re.match(r"^(?:Bug|Fixed)\s*:\s*(?:b/)?\d+\b", raw, re.I)
                for raw in msg_lines
            )

            for ln, l in deleted_tests_lines:
                rhs = l.split(":", 1)[1].strip()
                if has_planter_tag:
                    add(
                        "forbidden_deleted_tests_footer",
                        ln,
                        f"Agent-authored (`TAG: planter`) migration commit declares `{l}`; coding agents are strictly forbidden from deleting tests or emitting `Deleted-Tests:`.",
                        "Restore and migrate the test target(s), keep unmigrated GN-blocked tests above `## BAZEL2GN SENTINEL`, or halt and escalate to a human owner.",
                        severity="error",
                    )
                elif not rhs or "*" in rhs or not has_numeric_bug:
                    add(
                        "invalid_deleted_tests_footer",
                        ln,
                        f"`{l}` must name exact test target label(s) without wildcards (`*`) and be accompanied by a numeric `Bug: <id>` footer (plus human `Test-Drop-Verified +1` on Gerrit).",
                        "Specify exact target label(s) in `Deleted-Tests:` alongside `Bug: <id>`, or keep the test instead of deleting it.",
                        severity="error",
                    )

            for ln, l in skipped_dual_lines:
                rhs = l.split(":", 1)[1].strip()
                if not rhs or "*" in rhs or not has_numeric_bug:
                    add(
                        "invalid_skipped_dual_tests_footer",
                        ln,
                        f"`{l}` must name exact Bazel test label(s) (`//<dir>:<target>`) without wildcards and include a numeric `Bug: <id>` footer.",
                        "Specify `Skipped-Dual-Tests: //<dir>:<target>` and `Bug: <id>` alongside `# Needs reviewer attention (bazel2gn skip): <reason>`.",
                        severity="error",
                    )

            for ln, l in renamed_tests_lines:
                rhs = l.split(":", 1)[1].strip()
                parts = [p.strip() for p in rhs.split("->")]
                if len(parts) != 2 or not parts[0] or not parts[1] or "*" in rhs:
                    add(
                        "invalid_renamed_tests_footer",
                        ln,
                        f"`{l}` must have the exact 1:1 format `Renamed-Tests: <old> -> <new>` without wildcards.",
                        "Write one `Renamed-Tests: <old> -> <new>` footer line per renamed test target.",
                        severity="error",
                    )

            build_only_true = [
                (ln, l)
                for ln, l in build_only_lines
                if l.split(":", 1)[1].strip().lower() == "true"
            ]
            if build_only_true:
                ln0, l0 = build_only_true[0]
                reasons = []
                if deleted_tests_lines or skipped_dual_lines:
                    reasons.append(
                        "commit declares `Deleted-Tests:` or `Skipped-Dual-Tests:`"
                    )
                build_pat = re.compile(
                    r"(?:^|/)(?:BUILD\.gn|BUILD\.bazel|BUILD|MODULE\.bazel|[^/]+\.(?:gni|bzl))$"
                )
                non_build = [
                    f for f in sorted(changed_files) if not build_pat.search(f)
                ]
                if non_build:
                    reasons.append(
                        f"non-build file(s) modified ({', '.join(non_build[:3])})"
                    )
                touched_test_files = []
                for f in sorted(changed_files):
                    base_b = os.path.basename(f)
                    if base_b == "BUILD.gn":
                        before_txt = git("show", f"{base_rev}:{f}")
                        after_txt = _read_after_file(f)
                        if _gn_test_footprint(before_txt) != _gn_test_footprint(
                            after_txt
                        ):
                            touched_test_files.append(f)
                    elif base_b in ("BUILD.bazel", "BUILD"):
                        before_txt = git("show", f"{base_rev}:{f}")
                        after_txt = _read_after_file(f)
                        if _bazel_test_footprint(
                            before_txt
                        ) != _bazel_test_footprint(after_txt):
                            touched_test_files.append(f)
                if touched_test_files:
                    reasons.append(
                        "test rules, `with_unit_tests`, `bazel_test_suite`, `test_suite`, or `:tests` group deps touched in "
                        + ", ".join(touched_test_files[:4])
                    )
                if reasons:
                    add(
                        "illegal_build_only_footer",
                        ln0,
                        f"`{l0}` is forbidden on test-touching or source-modifying changes ({'; '.join(reasons)}).",
                        "Remove `Build-Only: true` from the commit message so full CQ test builders execute; `Build-Only: true` is permitted ONLY on pure non-test library `BUILD.gn`/`BUILD.bazel` migrations that touch zero test rules or `:tests` group edges.",
                        severity="error",
                    )

        check_test_parity_footers(lines, all_changed)

        nonempty_idxs = [i for i, l in enumerate(lines) if l.strip()]
        if nonempty_idxs:
            final_start = nonempty_idxs[-1]
            while final_start > 0 and lines[final_start - 1].strip():
                final_start -= 1
            for ln, l in (
                bmt_lines
                + test_lines
                + skip_readme_lines
                + deleted_tests_lines
                + skipped_dual_lines
                + renamed_tests_lines
                + build_only_lines
            ):
                if ln - 1 < final_start:
                    add("footer_block_not_contiguous", ln,
                        f"Footer `{l}` is separated from the final footer block by a blank line, so Gerrit will not "
                        "index it as a commit footer (breaking `hasfooter:Bazel-Migration-Target` queries).",
                        "Group all footers (`Bug:`/`Fixed:`, `Skip-Readme-Check:`, `Renamed-Tests:`, `Skipped-Dual-Tests:`, `Test:`, `Bazel-Migration-Target:`, `Build-Only:`, `TAG:`, "
                        "`Change-Id:`) into a single contiguous block at the very bottom of the commit message with "
                        "no blank lines between them.")
                    break
            # Check NO_IFTTT lines: Planter and AI agents must never add NO_IFTTT lines;
            # human-added NO_IFTTT=<reason> lines from an existing uploaded commit must be
            # preserved verbatim on their own unwrapped line above a blank line before the
            # contiguous footer block.
            no_ifttt_lines = [
                (i + 1, l.strip())
                for i, l in enumerate(lines[1:], start=1)
                if no_ifttt_re.match(l.strip())
            ]
            uploaded_no_ifttt = (
                [
                    l.strip()
                    for l in git("log", "-1", "--format=%B", uploaded_rev).splitlines()[1:]
                    if no_ifttt_re.match(l.strip())
                ]
                if uploaded_rev
                else []
            )
            is_planter_run = bool(
                os.environ.get("PLANTER_AGENT_ROLE", "").strip()
                or os.environ.get("PLANTER_CHECK_NAME", "").strip()
            )
            has_final_trailers = any(
                footer_line_re.match(lines[i].strip())
                for i in range(final_start, nonempty_idxs[-1] + 1)
            )
            for ln, raw_no_ifttt in no_ifttt_lines:
                if (uploaded_rev and raw_no_ifttt not in uploaded_no_ifttt) or (
                    not uploaded_rev and is_planter_run
                ):
                    add(
                        "agent_added_no_ifttt",
                        ln,
                        f"Commit message includes `{raw_no_ifttt}`, which was added by the "
                        "agent rather than a human reviewer.",
                        f"Remove `{raw_no_ifttt}` from the commit message (and "
                        "`CoderReport.summary`). Planter and AI agents must never add "
                        "`NO_IFTTT=...` lines; humans must review any "
                        "`LINT.IfChange`/`LINT.ThenChange` (IFTTT) warnings or errors, "
                        "evaluate them, and add their own `NO_IFTTT=...` message as "
                        "appropriate.",
                        severity="error",
                    )
                elif not re.match(r"^NO_IFTTT=\S", raw_no_ifttt):
                    add(
                        "no_ifttt_format",
                        ln,
                        f"`NO_IFTTT` line `{raw_no_ifttt}` must use `NO_IFTTT=<reason>` "
                        "syntax with `=` and a non-empty reason.",
                        "Preserve the human-added `NO_IFTTT=<reason>` line on a single "
                        "unwrapped line in its own paragraph above a blank line before "
                        "the footer block.",
                        severity="error",
                    )
                elif (has_final_trailers and ln - 1 >= final_start) or (
                    (ln - 2 >= 1 and lines[ln - 2].strip() and not no_ifttt_re.match(lines[ln - 2].strip()))
                    or (ln < len(lines) and lines[ln].strip() and not no_ifttt_re.match(lines[ln].strip()))
                ):
                    add(
                        "no_ifttt_in_footer_block",
                        ln,
                        f"`NO_IFTTT` line `{raw_no_ifttt}` is not in its own paragraph "
                        "separated by a blank line above the contiguous footer block "
                        "(placing `NO_IFTTT=...` inside the footer block causes Gerrit "
                        "to stop indexing footers above it).",
                        f"Keep the human-added `{raw_no_ifttt}` line verbatim on a single "
                        "unwrapped line in its own paragraph at the end of the commit body, "
                        "separated from the contiguous footer block (`Bug:`, `Test:`, "
                        "`Bazel-Migration-Target:`, `TAG:`, `Change-Id:`) by a blank line.",
                        severity="error",
                    )
            current_no_ifttt = {l for _, l in no_ifttt_lines}
            for req_no_ifttt in uploaded_no_ifttt:
                if req_no_ifttt not in current_no_ifttt:
                    add(
                        "dropped_human_no_ifttt",
                        footer_ln,
                        f"Commit message dropped or modified the human-added `{req_no_ifttt}` "
                        "line from the previous patchset.",
                        f"Restore `{req_no_ifttt}` verbatim on a single unwrapped line in its "
                        "own paragraph separated by a blank line above the contiguous footer "
                        "block.",
                        severity="error",
                    )
elif head:
    skip_b = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
    dry_r = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
    if not (skip_b or dry_r):
        wt_changed = [f for f in git("diff", "--name-only", "HEAD").splitlines() if f]
        wt_untracked = [
            f for f in git("ls-files", "--others", "--exclude-standard").splitlines() if f
        ]
        wt_all = list(dict.fromkeys(wt_changed + wt_untracked))
        wt_set = set(wt_all)
        cand_wt_dirs = list(
            dict.fromkeys(
                dirs
                + [
                    os.path.dirname(f)
                    for f in wt_all
                    if os.path.basename(f) in ("BUILD.bazel", "BUILD") and os.path.dirname(f)
                ]
            )
        )
        wt_gn_meta = {
            "group",
            "config",
            "bazel_test_suite",
            "bazel_host_tool",
            "bazel_ffx_tool",
            "bazel_fuchsia_package",
            "host_test_data",
            "sdk_atom",
            "declare_args",
            "import",
            "set_defaults",
            "template",
            "foreach",
            "if",
            "assert",
        }
        wt_helper_rules = {
            "package",
            "exports_files",
            "alias",
            "config_setting",
            "test_suite",
            "fx_packaged_binary",
            "fx_unittest_component",
        }
        wt_synth_comp_rules = {"fx_component_manifest", "fx_test_component", "cml_shard"}
        wt_test_data_rules = {
            "host_test_data_files",
            "host_test_data_dirs",
            "host_test_data_map",
            "host_test_data",
            "test_data",
            "resource",
            "fuchsia_package_resource_group",
        }
        wt_ut_re = (
            r'^\s*(with_unit_tests\s*=\s*"(host|both|fuchsia)"'
            r"|with_unit_tests\s*=\s*True|with_host_unit_tests\s*=\s*True)"
        )

        def _wt_iter_gn_targets(gn_text):
            clean = re.sub(
                r'("(?:\\.|[^"\\])*")|#[^\n]*',
                lambda m: m.group(1) or " " * len(m.group(0)),
                gn_text,
            )
            for m in re.finditer(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{', clean, re.M):
                rule, gname = m.group(1), m.group(2)
                i, depth, in_str = m.end(), 1, False
                while i < len(clean) and depth > 0:
                    ch = clean[i]
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
                    i += 1
                yield rule, gname, clean[m.end() : max(m.end(), i - 1)]

        def _wt_has_skip(btxt_str, start_pos):
            prefix_lines = btxt_str[:start_pos].splitlines()
            idx = len(prefix_lines) - 1
            while idx >= 0:
                s = prefix_lines[idx].strip()
                if not s:
                    idx -= 1
                    continue
                if s.startswith("#"):
                    if "@bazel2gn:skip" in s:
                        return True
                    idx -= 1
                    continue
                break
            return False

        wt_migrated = []
        wt_mig_dirs = []
        for d in cand_wt_dirs:
            if not (
                f"{d}/BUILD.bazel" in wt_set
                or f"{d}/BUILD" in wt_set
                or f"{d}/BUILD.gn" in wt_set
                or f"build/secondary/{d}/BUILD.gn" in wt_set
            ):
                continue
            bz_path = os.path.join(workdir, d, "BUILD.bazel")
            if not os.path.isfile(bz_path):
                continue
            try:
                with open(bz_path, encoding="utf-8") as fh:
                    btxt = fh.read()
            except OSError:
                continue
            cur_gtxt = ""
            gn_path = os.path.join(workdir, d, "BUILD.gn")
            if os.path.isfile(gn_path):
                try:
                    with open(gn_path, encoding="utf-8") as fh:
                        cur_gtxt = fh.read()
                except OSError:
                    cur_gtxt = ""
            pre_cur_gtxt = cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if cur_gtxt else ""
            cur_suites = {
                gn
                for gr, gn, _ in _wt_iter_gn_targets(cur_gtxt)
                if gr
                in (
                    "bazel_test_suite",
                    "bazel_host_tool",
                    "bazel_ffx_tool",
                    "bazel_fuchsia_package",
                )
            }
            kept_gn = {
                gn
                for gr, gn, _ in _wt_iter_gn_targets(pre_cur_gtxt)
                if gr not in wt_gn_meta and gn not in cur_suites
            }
            base_btxt = git("show", f"{head}:{d}/BUILD.bazel")
            base_gtxt = git("show", f"{head}:{d}/BUILD.gn") or git(
                "show", f"{head}:build/secondary/{d}/BUILD.gn"
            )
            base_bz = set()
            for m in re.finditer(
                r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", base_btxt, re.M | re.S
            ):
                n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(0), re.M)
                if n:
                    base_bz.add(n.group(1))
                    if re.search(wt_ut_re, m.group(0), re.M):
                        base_bz.add(f"{n.group(1)}_test")
            pre_base_gtxt = (
                base_gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if base_gtxt else ""
            )
            ut_tmpls = {
                tn
                for gr, tn, gb in _wt_iter_gn_targets(pre_base_gtxt)
                if gr == "template"
                and re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", gb)
            }
            orig_gn = set()
            for gr, gn, gb in _wt_iter_gn_targets(pre_base_gtxt):
                if gr not in wt_gn_meta:
                    orig_gn.add(gn)
                    if gr in ut_tmpls or re.search(
                        r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", gb
                    ):
                        orig_gn.add(f"{gn}_test")
            blocks = list(
                re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", btxt, re.M | re.S)
            )
            test_wrapped_pkgs = set()
            wrapped_bins = set()
            wrapped_test_bins = set()
            for m in blocks:
                rule, blk = m.group(1), m.group(0)
                if rule == "fx_test":
                    p = re.search(r'^\s*package\s*=\s*":([^"]+)"', blk, re.M)
                    if p:
                        test_wrapped_pkgs.add(p.group(1))
                if rule in ("host_test", "fx_packaged_binary", "fx_unittest_component"):
                    b = re.search(r'^\s*binary\s*=\s*":([^"]+)"', blk, re.M)
                    if b:
                        if b.group(1) not in orig_gn:
                            wrapped_bins.add(b.group(1))
                        if rule in ("host_test", "fx_unittest_component") or re.search(
                            r"^\s*testonly\s*=\s*True\b", blk, re.M
                        ):
                            wrapped_test_bins.add(b.group(1))
                if rule == "wrap_host_rust_test":
                    bn = re.search(r'^\s*binary_name\s*=\s*"([^"]+)"', blk, re.M)
                    if bn:
                        wrapped_test_bins.add(bn.group(1))
                if rule == "fx_unittest_package":
                    u = re.search(r"unit_tests\s*=\s*\[(.*?)\]", blk, re.S)
                    if u:
                        for b in re.findall(r'":([^"]+)"', u.group(1)):
                            wrapped_test_bins.add(b)
                            if b not in orig_gn:
                                wrapped_bins.add(b)
            dir_new = []
            dir_req = []
            for m in blocks:
                rule, blk = m.group(1), m.group(0)
                n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
                if not n:
                    continue
                tname = n.group(1)
                if (
                    tname in kept_gn
                    and _wt_has_skip(btxt, m.start())
                    and rule not in ("fx_test_component", "rustc_test", "go_test")
                ):
                    continue
                is_testonly_helper = rule in ("genrule", "filegroup", "copy_file") and bool(
                    re.search(r"^\s*testonly\s*=\s*True\b", blk, re.M)
                )
                is_synth_comp = rule in wt_synth_comp_rules and tname not in orig_gn
                is_synth_td = (
                    rule in wt_test_data_rules or is_testonly_helper
                ) and tname not in orig_gn
                if (
                    rule in wt_helper_rules
                    or is_synth_comp
                    or is_synth_td
                    or (rule == "fx_package" and tname in test_wrapped_pkgs)
                    or tname in wrapped_bins
                ):
                    continue
                if tname not in base_bz:
                    lbl = f"//{d}:{tname}"
                    dir_new.append(lbl)
                    if tname in orig_gn and tname not in wrapped_test_bins:
                        dir_req.append(lbl)
            chosen = dir_req if dir_req else dir_new
            if chosen:
                wt_migrated.extend(chosen)
                wt_mig_dirs.append(d)
        wt_migrated = list(dict.fromkeys(wt_migrated))
        if wt_migrated:
            use_jj = os.path.isdir(os.path.join(workdir, ".jj")) and (
                base == "@-" or base_rev == resolve_rev("@-")
            )
            example_bmt = ", ".join(
                f"`Bazel-Migration-Target: {t}`" for t in wt_migrated
            )
            t_list = ", ".join(f"`{t}`" for t in wt_migrated)
            first_wt = wt_mig_dirs[0] if wt_mig_dirs else (dirs[0] if dirs else "<dir>")
            vcs_cmd = (
                "`jj describe -m ...`"
                if use_jj
                else "`git add` and `git commit` (omitting `TAG:` so Planter adopts the commit on upload)"
            )
            add(
                "missing_bazel_migration_target_footer",
                1,
                "Uncommitted Bazel migration changes have no local task commit recording "
                f"`Bazel-Migration-Target:` footer(s) for migrated target(s): {t_list}.",
                f"Create the task commit locally via {vcs_cmd} with a "
                f"`[bazel_migration]... //{first_wt}` subject, concise rationale body, "
                "`Test:` footers, and one `Bazel-Migration-Target: //<dir>:<target>` line "
                f"per migrated GN target in the final contiguous footer block (e.g. {example_bmt}).",
                severity="error",
            )


# Probe whether the repository .git directory accepts non-empty file writes during full check runs.
def git_dir_write_probe_error():
    skip_b = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
    dry_r = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
    if skip_b or dry_r:
        return None
    gdir = git("rev-parse", "--absolute-git-dir")
    if not gdir or not os.path.isdir(gdir):
        return None
    probe_bytes = b"planter-write-probe\n"
    fd = None
    probe_path = None
    try:
        fd, probe_path = tempfile.mkstemp(dir=gdir, prefix=".planter-write-probe.")
        os.write(fd, probe_bytes)
        os.close(fd)
        fd = None
        with open(probe_path, "rb") as rf:
            if rf.read() != probe_bytes:
                raise OSError("short write during writability probe")
        os.unlink(probe_path)
    except OSError as err:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
        if probe_path and os.path.lexists(probe_path):
            try:
                os.unlink(probe_path)
            except OSError:
                pass
        return gdir, err
    return None


# Suppress unamendable commit-message findings when .git rejects file writes.
def suppress_unamendable_findings_on_write_error():
    blocking = [
        f for f in findings if f.get("severity", "").lower() in ("warning", "error")
    ]
    if not blocking:
        return
    probe_err = git_dir_write_probe_error()
    if probe_err is None:
        return
    gdir, err = probe_err
    findings[:] = [
        f for f in findings if f.get("severity", "").lower() not in ("warning", "error")
    ]
    msg = (
        f"Skipped {len(blocking)} commit message finding(s) because `{gdir}` rejected "
        f"file writes ({err}); `git commit --amend` cannot update HEAD while the "
        "workspace mount is unwritable."
    )
    rem = (
        "Stop retrying `git commit --amend` or build commands while the mount rejects "
        "writes; set `test_footers` to `[\"fx lint\"]` and "
        "`disputed_findings` to `[]` in `CoderReport` and report "
        "`workspace_write_error` in `summary`."
    )
    add("commit_msg_unwritable_git_dir", 1, msg, rem, severity="info")


suppress_unamendable_findings_on_write_error()
print(json.dumps(findings, indent=2))
PYEOF
