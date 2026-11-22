# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC checks for SEStarnix userspace test SELinux policy fragments.

Registered by `//src/starnix/shac.star`, so `ctx.scm.root` is `//src/starnix`
and all SCM paths are relative to `//src/starnix`.
"""

load("//scripts/shac/common.star", "cipd_platform_name", "os_exec")

_POLICIES_DIR = "tests/selinux/userspace/policies/"
_BASE_POLICY = _POLICIES_DIR + "test_policy.conf"

# Rules with pre-existing violations in legacy policies are enforced only on
# added/modified lines; all other rules (zero pre-existing debt) apply whole-file.
_CHANGED_LINES_ONLY_RULES = (
    "capability_target_self",
    "naming_prefix",
    "naming_suffix",
    "no_domain_a_on_test_domain",
)

def _is_userspace_policy(path):
    if not path.startswith(_POLICIES_DIR) or not path.endswith(".conf"):
        return False
    return path != _BASE_POLICY and "/" not in path[len(_POLICIES_DIR):]

def _selinux_policy_lint(ctx):
    """Lints SEStarnix userspace test SELinux policy fragments."""
    affected = ctx.scm.affected_files(glob = ["*.conf"])
    files = sorted([f for f in affected if _is_userspace_policy(f)])
    if not files:
        return

    python_bin = "%s/../../prebuilt/third_party/python3/%s/bin/python3" % (
        ctx.scm.root,
        cipd_platform_name(ctx),
    )
    linter_script = "%s/lib/selinux/scripts/lint_policy.py" % ctx.scm.root
    res = os_exec(
        ctx,
        [python_bin, linter_script, "--root", ctx.scm.root] + files,
    ).wait()

    changed_lines_by_file = {}
    for finding in json.decode(res.stdout):
        filepath = finding["filepath"]
        line = finding["line"]
        rule = finding["rule"]
        if rule in _CHANGED_LINES_ONLY_RULES:
            if filepath not in changed_lines_by_file:
                changed_lines_by_file[filepath] = set([
                    num
                    for num, _ in affected[filepath].affected_lines(changed_only = True)
                ])
            if line not in changed_lines_by_file[filepath]:
                continue
        ctx.emit.finding(
            level = "error",
            filepath = filepath,
            line = line,
            message = "[%s] %s" % (rule, finding["message"]),
        )

selinux_policy_lint = shac.check(_selinux_policy_lint, formatter = False)

def register_selinux_policy_checks():
    """Registers SEStarnix test SELinux policy SHAC checks."""
    shac.register_check(selinux_policy_lint)
