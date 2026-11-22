# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC checks for SKILL.md skills, dynamic workflows, and .agents configs."""

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def _run_skill_linter(ctx, extra_args, rel_files):
    exe_interp = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        get_fuchsia_dir(ctx),
        cipd_platform_name(ctx),
    )
    exe_script = "%s/scripts/skill_linter/skill_linter.py" % get_fuchsia_dir(ctx)
    file_paths = [ctx.scm.root + "/" + f for f in rel_files]
    return os_exec(
        ctx,
        [exe_interp, exe_script] + extra_args + file_paths,
        env = {
            "PYTHONPATH": get_fuchsia_dir(ctx) + "/third_party/pyyaml/src/lib",
        },
        ok_retcodes = [0, 1],
    ).wait()

def _skill_format(ctx):
    """Validates frontmatter and formats modified SKILL.md files."""
    skill_files = ctx.scm.affected_files(glob = ["**/SKILL.md"])

    if not skill_files:
        return

    res = _run_skill_linter(ctx, ["--suggest-fix-in-json"], skill_files)

    # Check for failure to produce JSON.
    if not res.stdout.strip().startswith("["):
        ctx.emit.finding(
            level = "error",
            message = "Skill linter error:\n%s" % res.stderr,
        )
        return

    findings = json.decode(res.stdout)
    for finding in findings:
        ctx.emit.finding(
            level = finding["level"],
            filepath = finding["filepath"],
            message = finding["message"],
            replacements = finding.get("replacements"),
        )

_MAX_SPECIFIED_FILES = 50

def _workflow_manifest_for_path(path):
    parts = path.split("/")
    for i in range(len(parts) - 2):
        if parts[i] in (".agents", "_agents") and parts[i + 1] == "workflows":
            return "/".join(parts[:i + 3] + ["workflow.json"])
    return None

def _agent_customizations_lint(ctx):
    """Lints skills, saved dynamic workflows, and .agents discovery configs."""
    skill_and_json_map = ctx.scm.affected_files(glob = [
        "**/SKILL.md",
        "**/.agents/agents.json",
        "**/.agents/plugins.json",
        "**/.agents/rules.json",
        "**/.agents/skills.json",
        "**/.agents/workflows.json",
        "**/_agents/agents.json",
        "**/_agents/plugins.json",
        "**/_agents/rules.json",
        "**/_agents/skills.json",
        "**/_agents/workflows.json",
    ])
    workflow_files_map = ctx.scm.affected_files(
        glob = [
            "**/.agents/workflows/**",
            "**/_agents/workflows/**",
        ],
        include_deleted = True,
    )
    target_files_map = dict(skill_and_json_map)
    target_files_map.update(workflow_files_map)

    if not target_files_map:
        return

    # SHAC's `gitCheckout` backend (`shac check` and `shac check --all`) sets a
    # non-empty `meta.action` on files modified or deleted in git and `""` on
    # untouched tracked files. Passing `include_deleted = True` keeps
    # deletion-only CLs git-backed. On a clean checkout (`HEAD == upstream`)
    # under `shac check --all`, every tracked file has `meta.action == ""`
    # (`git_backed == False`), which is distinguished from `specifiedFiles`
    # (`shac check <file>...`) by `len(affected_all) > _MAX_SPECIFIED_FILES`.
    affected_all = ctx.scm.affected_files(include_deleted = True)
    git_backed = any([m.action for m in affected_all.values()])
    modified_files = [f for f, meta in target_files_map.items() if meta.action]
    if git_backed:
        if not modified_files:
            return
        target_files = modified_files
    else:
        if len(affected_all) > _MAX_SPECIFIED_FILES:
            return
        target_files = list(target_files_map.keys())

    deleted_set = set([f for f, m in affected_all.items() if m.action == "D"])
    changed_set = (
        set([f for f, m in affected_all.items() if m.action and m.action != "D"]) if git_backed else set(affected_all.keys())
    )
    deleted_wf_manifests = set([
        m_path
        for f, meta in workflow_files_map.items()
        if meta.action == "D"
        for m_path in [_workflow_manifest_for_path(f)]
        if m_path
    ])
    tracked_set = set(ctx.scm.all_files())
    res = _run_skill_linter(
        ctx,
        [
            "--lint-json",
            "--repo-root",
            ctx.scm.root,
            "--fuchsia-root",
            get_fuchsia_dir(ctx),
        ],
        target_files,
    )

    if not res.stdout.strip().startswith("["):
        ctx.emit.finding(
            level = "error",
            message = "Skill and workflow linter error:\n%s" % res.stderr,
        )
        return

    findings = json.decode(res.stdout)
    for finding in findings:
        filepath = finding["filepath"]
        level = finding["level"]
        line = finding.get("line")
        message = finding["message"]
        if filepath in changed_set or (
            level == "error" and
            filepath in deleted_wf_manifests and
            filepath in tracked_set and
            filepath not in deleted_set
        ):
            ctx.emit.finding(
                level = level,
                filepath = filepath,
                line = line,
                message = message,
            )
        elif level == "error":
            if filepath in tracked_set and filepath not in deleted_set:
                # Untouched tracked sibling file in a workflow dir: downgrade
                # pre-existing errors to warning consistently across diff and
                # `--all` modes while omitting untouched sibling warnings.
                ctx.emit.finding(
                    level = "warning",
                    filepath = filepath,
                    line = line,
                    message = message,
                )
            else:
                # Sibling file in workflow dir is untracked in SCM; emit as a
                # repo-level error naming the untracked file and line.
                loc = "%s:%s" % (filepath, line) if line else filepath
                ctx.emit.finding(
                    level = level,
                    message = "[%s] %s" % (loc, message),
                )

def register_skills_checks():
    shac.register_check(shac.check(_skill_format, formatter = True))
    shac.register_check(_agent_customizations_lint)
