# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC check validating test-parity footers and gating test drops on Test-Drop-Verified."""

load("./common.star", "get_fuchsia_dir")

_PARITY_FOOTER_RE = r"(?im)^(Deleted-Tests|Skipped-Dual-Tests|Renamed-Tests)\s*:"
_TAG_RE = r"(?i)^TAG\s*[:=]\s*(.+)$"
_BUG_RE = r"(?i)^(?:Bug|Fixed)\s*:\s*(.+)$"
_BUG_ID_RE = r"\b(\d{4,})\b"
_DELETED_RE = r"(?i)^Deleted-Tests\s*:\s*(.*)$"
_SKIPPED_RE = r"(?i)^Skipped-Dual-Tests\s*:\s*(.*)$"
_RENAMED_RE = r"(?i)^Renamed-Tests\s*:\s*(.*)$"
_LABEL_ENTRY_RE = r"^(//[\w./\-]*:[\w.\-]+)(?:\s*\([^()]+\))?$"
_RENAMED_CLAUSE_RE = r"^(//[\w./\-]*:[\w.\-]+)\s*->\s*(//[\w./\-]*:[\w.\-]+)$"

_TEST_DROP_VERIFIED = {"fail_if_missing_label": "Test-Drop-Verified"}

def _is_submitted_commit(message):
    """Returns True if a commit has already been submitted by Gerrit or a roller."""
    lines = message.rstrip().splitlines()
    last_para_start = 0
    for i in range(len(lines) - 1, -1, -1):
        if not lines[i].strip():
            last_para_start = i + 1
            break
    return any([
        line.startswith("Reviewed-on:") or line.startswith("Roller-URL:")
        for line in lines[last_para_start:]
    ])

def _split_top_level_commas(raw_val):
    """Splits a footer value on commas outside parentheses."""
    items = []
    cur = []
    depth = 0
    for ch in raw_val.elems():
        if ch == "(":
            depth += 1
        elif ch == ")" and depth > 0:
            depth -= 1
        elif ch == "," and depth == 0:
            part = "".join(cur).strip()
            if part:
                items.append(part)
            cur = []
            continue
        cur.append(ch)
    tail = "".join(cur).strip()
    if tail:
        items.append(tail)
    return items

def _dedupe(items):
    """Returns unique items preserving first-seen order."""
    return list({item: True for item in items}.keys())

def _parse_commit_message(ctx, message):
    """Extracts test-parity footers and metadata from a commit message."""
    has_tag_planter = False
    bug_ids = []
    deleted_raw = []
    skipped_raw = []
    renamed_raw = []

    for idx, line in enumerate(message.splitlines()):
        lineno = idx + 1
        if line != line.lstrip():
            continue
        stripped = line.rstrip()
        if not stripped or stripped.startswith(">") or stripped.startswith("#"):
            continue

        m_tag = ctx.re.match(_TAG_RE, stripped)
        if m_tag and "planter" in m_tag.groups[1].strip().lower():
            has_tag_planter = True

        m_bug = ctx.re.match(_BUG_RE, stripped)
        if m_bug:
            for m_id in ctx.re.allmatches(_BUG_ID_RE, m_bug.groups[1]):
                bug_ids.append(m_id.groups[1])

        m_del = ctx.re.match(_DELETED_RE, stripped)
        if m_del:
            deleted_raw.append((lineno, m_del.groups[1].strip()))

        m_skip = ctx.re.match(_SKIPPED_RE, stripped)
        if m_skip:
            skipped_raw.append((lineno, m_skip.groups[1].strip()))

        m_ren = ctx.re.match(_RENAMED_RE, stripped)
        if m_ren:
            renamed_raw.append((lineno, m_ren.groups[1].strip()))

    return struct(
        has_tag_planter = has_tag_planter,
        bug_ids = bug_ids,
        deleted_tests_raw = deleted_raw,
        skipped_dual_tests_raw = skipped_raw,
        renamed_tests_raw = renamed_raw,
    )

def _check_deleted_tests(ctx, commit, meta):
    """Validates `Deleted-Tests:` footers and gates on `Test-Drop-Verified`."""
    if not meta.deleted_tests_raw:
        return

    first_line = meta.deleted_tests_raw[0][0]
    if meta.has_tag_planter:
        ctx.emit.commit_message_finding(
            level = "error",
            commit = commit,
            line = first_line,
            message = (
                "🚨 [TEST PARITY ALERT \u2014 FORBIDDEN AGENT Deleted-Tests: " +
                "FOOTER] Automated `TAG: planter` migration commits are " +
                "strictly forbidden from deleting tests or emitting " +
                "`Deleted-Tests:`.\n" +
                "Remediation: Restore and migrate the test target(s), keep " +
                "unmigrated GN-blocked tests above `## BAZEL2GN SENTINEL`, " +
                "or halt and escalate to a human owner."
            ),
        )
        return

    labels = []
    problems = []
    for lineno, raw_val in meta.deleted_tests_raw:
        if not raw_val:
            problems.append("empty `Deleted-Tests:` footer on line %d" % lineno)
            continue
        if "*" in raw_val:
            problems.append(
                "wildcards (`*`) are forbidden in `Deleted-Tests:` (line %d)" % lineno,
            )
            continue
        for item in _split_top_level_commas(raw_val):
            m = ctx.re.match(_LABEL_ENTRY_RE, item)
            if not m:
                problems.append(
                    "entry `%s` on line %d is not an exact `//<pkg>:<target>` label" % (
                        item,
                        lineno,
                    ),
                )
            else:
                labels.append(m.groups[1])

    if not meta.bug_ids:
        problems.append("missing numeric `Bug: <id>` footer")

    if problems:
        ctx.emit.commit_message_finding(
            level = "error",
            commit = commit,
            line = first_line,
            message = (
                "🚨 [TEST PARITY ALERT \u2014 INVALID Deleted-Tests: " +
                "FOOTER] `Deleted-Tests:` attestation is invalid " +
                "(%s).\n" % "; ".join(problems) +
                "Remediation: Specify exact `//<pkg>:<target>` labels " +
                "without wildcards alongside a numeric `Bug: <id>` footer, " +
                "and obtain a human `Test-Drop-Verified +1` vote on Gerrit."
            ),
        )
        return

    unique_labels = _dedupe(labels)
    ctx.emit.commit_message_finding(
        level = "warning",
        commit = commit,
        line = first_line,
        properties = _TEST_DROP_VERIFIED,
        message = (
            "🚨 [TEST PARITY ALERT \u2014 ATTESTED ESCAPE-HATCH TEST " +
            "DELETION (REQUIRES Test-Drop-Verified +1)] This CL declares " +
            "deleted test target(s): %s (Bug: %s).\n" % (
                ", ".join(unique_labels),
                meta.bug_ids[0],
            ) +
            "Remediation: A human committer / reviewer must audit every " +
            "deleted test listed above and vote `Test-Drop-Verified +1` on " +
            "Gerrit to allow submission."
        ),
    )

def _check_skipped_dual_tests(ctx, commit, meta):
    """Validates `Skipped-Dual-Tests:` footers and gates on `Test-Drop-Verified`."""
    if not meta.skipped_dual_tests_raw:
        return

    first_line = meta.skipped_dual_tests_raw[0][0]
    labels = []
    problems = []
    for lineno, raw_val in meta.skipped_dual_tests_raw:
        if not raw_val:
            problems.append(
                "empty `Skipped-Dual-Tests:` footer on line %d" % lineno,
            )
            continue
        if "*" in raw_val:
            problems.append(
                "wildcards (`*`) are forbidden in `Skipped-Dual-Tests:` (line %d)" % lineno,
            )
            continue
        for item in _split_top_level_commas(raw_val):
            m = ctx.re.match(_LABEL_ENTRY_RE, item)
            if not m:
                problems.append(
                    "entry `%s` on line %d is not an exact `//<pkg>:<target>` label" % (
                        item,
                        lineno,
                    ),
                )
            else:
                labels.append(m.groups[1])

    if not meta.bug_ids:
        problems.append("missing numeric `Bug: <id>` footer")

    if problems:
        ctx.emit.commit_message_finding(
            level = "error",
            commit = commit,
            line = first_line,
            message = (
                "🚨 [TEST PARITY ALERT \u2014 INVALID Skipped-Dual-Tests: " +
                "FOOTER] `Skipped-Dual-Tests:` attestation is invalid " +
                "(%s).\n" % "; ".join(problems) +
                "Remediation: List exact canonical `//<pkg>:<target>` " +
                "labels alongside a numeric `Bug: <id>` footer, e.g. " +
                "`Skipped-Dual-Tests: //src/foo:foo_tests`."
            ),
        )
        return

    unique_labels = _dedupe(labels)
    ctx.emit.commit_message_finding(
        level = "notice",
        commit = commit,
        line = first_line,
        properties = _TEST_DROP_VERIFIED,
        message = (
            "🚨 [TEST PARITY ALERT \u2014 DUAL-BUILD TEST SKIPPED " +
            "(REQUIRES Test-Drop-Verified +1)] This CL skips dual-building " +
            "test target(s): %s (Bug: %s).\n" % (
                ", ".join(unique_labels),
                meta.bug_ids[0],
            ) +
            "Remediation: A human reviewer must verify the skip " +
            "justification and vote `Test-Drop-Verified +1` on Gerrit to " +
            "clear this gate."
        ),
    )

def _check_renamed_tests(ctx, commit, meta):
    """Validates `Renamed-Tests: <old> -> <new>` footers (one pair per line, 1:1 bijection)."""
    if not meta.renamed_tests_raw:
        return

    forward_targets = {}
    reverse_owners = {}
    first_line_for_old = {}
    first_line_for_new = {}

    for lineno, raw_val in meta.renamed_tests_raw:
        if not raw_val or "*" in raw_val:
            ctx.emit.commit_message_finding(
                level = "error",
                commit = commit,
                line = lineno,
                message = (
                    "🚨 [TEST PARITY ALERT \u2014 INVALID Renamed-Tests: " +
                    "FOOTER] Commit message line %d has invalid " % lineno +
                    "`Renamed-Tests: %s` (wildcards `*` and empty " % raw_val +
                    "entries are forbidden).\n" +
                    "Remediation: Write `Renamed-Tests: " +
                    "//old/pkg:old_test -> //new/pkg:new_test`."
                ),
            )
            continue

        m = ctx.re.match(_RENAMED_CLAUSE_RE, raw_val)
        if not m:
            ctx.emit.commit_message_finding(
                level = "error",
                commit = commit,
                line = lineno,
                message = (
                    "🚨 [TEST PARITY ALERT \u2014 INVALID Renamed-Tests: " +
                    "SYNTAX] Entry `%s` on commit message line %d " % (
                        raw_val,
                        lineno,
                    ) +
                    "does not match `//<old_pkg>:<old_name> " +
                    "-> //<new_pkg>:<new_name>` (expected one `->` pair " +
                    "per `Renamed-Tests:` footer line).\n" +
                    "Remediation: Specify one exact `//pkg:target -> " +
                    "//pkg:target` pair per `Renamed-Tests:` line."
                ),
            )
            continue

        old_lbl = m.groups[1]
        new_lbl = m.groups[2]
        forward_targets.setdefault(old_lbl, []).append(new_lbl)
        reverse_owners.setdefault(new_lbl, []).append(old_lbl)
        first_line_for_old.setdefault(old_lbl, lineno)
        first_line_for_new.setdefault(new_lbl, lineno)

    for old_lbl in sorted(forward_targets.keys()):
        unique_news = _dedupe(forward_targets[old_lbl])
        if len(unique_news) > 1:
            ctx.emit.commit_message_finding(
                level = "error",
                commit = commit,
                line = first_line_for_old[old_lbl],
                message = (
                    "🚨 [TEST PARITY ALERT \u2014 NON-BIJECTIVE " +
                    "Renamed-Tests: SPLIT] Removed test `%s` maps " % old_lbl +
                    "to multiple targets (%s) in " % ", ".join(unique_news) +
                    "`Renamed-Tests:`, violating strict 1:1 bijection.\n" +
                    "Remediation: Map each renamed test target 1:1 to a " +
                    "single target label."
                ),
            )

    for new_lbl in sorted(reverse_owners.keys()):
        unique_olds = _dedupe(reverse_owners[new_lbl])
        if len(unique_olds) > 1:
            ctx.emit.commit_message_finding(
                level = "error",
                commit = commit,
                line = first_line_for_new[new_lbl],
                message = (
                    "🚨 [TEST PARITY ALERT \u2014 NON-BIJECTIVE " +
                    "Renamed-Tests: COLLAPSE] Multiple removed tests " +
                    "(%s) map to the same Bazel " % ", ".join(unique_olds) +
                    "target `%s` in `Renamed-Tests:`, which " % new_lbl +
                    "collapses distinct test targets!\n" +
                    "Remediation: Map each removed test target 1:1 to its " +
                    "own wired Bazel test target."
                ),
            )

def _bazel_migration_test_parity(ctx):
    """Validates Deleted-Tests/Skipped-Dual-Tests/Renamed-Tests footers and gates test drops."""
    fuchsia_dir = get_fuchsia_dir(ctx)

    # Exclude repositories other than `fuchsia.git` (such as `vendor/*`).
    if ctx.scm.root != fuchsia_dir:
        return

    for commit in ctx.scm.commits():
        if _is_submitted_commit(commit.message):
            continue
        if not ctx.re.match(_PARITY_FOOTER_RE, commit.message):
            continue
        meta = _parse_commit_message(ctx, commit.message)
        _check_deleted_tests(ctx, commit, meta)
        _check_skipped_dual_tests(ctx, commit, meta)
        _check_renamed_tests(ctx, commit, meta)

def register_bazel_migration_test_parity_checks():
    """Registers the Bazel migration test parity footer shac check."""
    shac.register_check(
        shac.check(
            _bazel_migration_test_parity,
            name = "bazel_migration_test_parity",
            formatter = False,
        ),
    )
