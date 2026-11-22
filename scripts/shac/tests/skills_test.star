# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/skills.star."""

load("//scripts/shac/skills.star", "_agent_customizations_lint", "_skill_format")

def test_skill_format_no_skills():
    """Verifies non-skill files produce zero findings."""
    res = testing.run(_skill_format, files = {
        "README.md": "",
        "src/main.cc": "",
        "XYZSKILL.md": "",
        "src/subsystem/XYZSKILL.md": "",
        "docs/index.md": "",
    })
    asserts.eq(res.findings, ())

def test_skill_format_multiple_subdirectories():
    """Verifies formatting findings across multiple SKILL.md paths."""
    findings = [
        {
            "filepath": "src/connectivity/wlan/SKILL.md",
            "level": "warning",
            "message": "Warning: Markdown body contains lines exceeding 80 characters.",
            "replacements": ["---\nname: wlan\n---\n\nFormatted.\n"],
        },
        {
            "filepath": "vendor/google/ui/SKILL.md",
            "level": "warning",
            "message": "Warning: Markdown body contains missing empty line before list.",
            "replacements": ["---\nname: ui\n---\n\n* Item 1\n"],
        },
    ]

    res = testing.run(
        _skill_format,
        files = {
            "src/connectivity/wlan/SKILL.md": "unformatted wlan skill\n",
            "vendor/google/ui/SKILL.md": "unformatted ui skill\n",
            "README.md": "",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--suggest-fix-in-json",
                    testing.root + "/src/connectivity/wlan/SKILL.md",
                    testing.root + "/vendor/google/ui/SKILL.md",
                ],
                stdout = json.encode(findings),
            ),
        ],
    )

    asserts.eq(len(res.findings), 2)
    asserts.eq(res.findings[0], testing.finding(
        level = "warning",
        filepath = "src/connectivity/wlan/SKILL.md",
        message = "Warning: Markdown body contains lines exceeding 80 characters.",
        replacements = ["---\nname: wlan\n---\n\nFormatted.\n"],
    ))
    asserts.eq(res.findings[1], testing.finding(
        level = "warning",
        filepath = "vendor/google/ui/SKILL.md",
        message = "Warning: Markdown body contains missing empty line before list.",
        replacements = ["---\nname: ui\n---\n\n* Item 1\n"],
    ))

def test_skill_format_linter_failure():
    """Verifies fatal linter crashes emit a repo-level error finding."""
    res = testing.run(
        _skill_format,
        files = {
            "my_feature/skills/SKILL.md": "broken skill\n",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--suggest-fix-in-json",
                    testing.root + "/my_feature/skills/SKILL.md",
                ],
                retcode = 1,
                stdout = "Traceback (most recent call last):\nFatal error\n",
                stderr = "Exception occurred in parser\n",
            ),
        ],
    )

    asserts.eq(len(res.findings), 1)
    asserts.eq(res.findings[0].level, "error")
    asserts.true("Skill linter error" in res.findings[0].message)

def test_agent_customizations_lint_ignores_unrelated_files():
    """Verifies non-customization files do not trigger _agent_customizations_lint."""
    res = testing.run(_agent_customizations_lint, files = {
        "src/main.rs": "",
        "src/testing/perfcompare/local_workflow.py": "",
        "docs/config.json": "{}",
    })
    asserts.eq(res.findings, ())

def test_agent_customizations_lint_skills_workflows_and_json():
    """Verifies workflow and discovery JSON findings and sibling error handling."""
    findings = [
        {
            "filepath": ".agents/workflows/demo/workflow.py",
            "line": 12,
            "level": "error",
            "message": 'call_tool("run_workflow", ...) is disallowed.',
        },
        {
            "filepath": ".agents/workflows/demo/README.md",
            "line": 5,
            "level": "warning",
            "message": "Untouched sibling warning should be filtered out.",
        },
        {
            "filepath": ".agents/skills.json",
            "line": 1,
            "level": "error",
            "message": 'Configured entries path "missing/skill" does not exist.',
        },
    ]

    res = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/workflows/demo/workflow.json": "{}\n",
            ".agents/skills.json": "{}\n",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/skills.json",
                    testing.root + "/.agents/workflows/demo/workflow.json",
                ],
                stdout = json.encode(findings),
            ),
        ],
    )

    # Error on untracked sibling workflow.py is surfaced without crashing SHAC;
    # warning on untouched README.md is omitted.
    asserts.eq(len(res.findings), 2)
    asserts.eq(res.findings[0], testing.finding(
        level = "error",
        message = '[.agents/workflows/demo/workflow.py:12] call_tool("run_workflow", ...) is disallowed.',
    ))
    asserts.eq(res.findings[1], testing.finding(
        level = "error",
        filepath = ".agents/skills.json",
        line = 1,
        message = 'Configured entries path "missing/skill" does not exist.',
    ))

def test_agent_customizations_lint_tracked_sibling_and_crash():
    """Verifies symmetric untouched sibling handling across diff/--all and crashes."""
    sibling_findings = [
        {
            "filepath": ".agents/workflows/demo/workflow.py",
            "line": 7,
            "level": "error",
            "message": 'call_tool("run_command", ...) is disallowed.',
        },
        {
            "filepath": ".agents/workflows/demo/README.md",
            "line": 3,
            "level": "warning",
            "message": "Untouched sibling warning should be omitted.",
        },
    ]

    # 1. Default diff mode (`affected = False` on untouched tracked siblings).
    res_diff = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/workflows/demo/workflow.json": testing.file("{}\n", action = "M"),
            ".agents/workflows/demo/workflow.py": testing.file("pass\n", affected = False),
            ".agents/workflows/demo/README.md": testing.file("# Doc\n", affected = False),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/workflows/demo/workflow.json",
                ],
                stdout = json.encode(sibling_findings),
            ),
        ],
    )

    # 2. `shac check --all` mode (`action = ""` on untouched tracked siblings).
    res_all = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/workflows/demo/workflow.json": testing.file("{}\n", action = "M"),
            ".agents/workflows/demo/workflow.py": testing.file("pass\n", action = ""),
            ".agents/workflows/demo/README.md": testing.file("# Doc\n", action = ""),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/workflows/demo/workflow.json",
                ],
                stdout = json.encode(sibling_findings),
            ),
        ],
    )

    expected_sibling = (
        testing.finding(
            level = "warning",
            filepath = ".agents/workflows/demo/workflow.py",
            line = 7,
            message = 'call_tool("run_command", ...) is disallowed.',
        ),
    )
    asserts.eq(res_diff.findings, expected_sibling)
    asserts.eq(res_all.findings, expected_sibling)

    crash_res = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/skills/demo/SKILL.md": "---\nname: demo\n---\n",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/skills/demo/SKILL.md",
                ],
                retcode = 1,
                stdout = "Traceback (most recent call last):\nCrash\n",
                stderr = "Unexpected error in semantic linter\n",
            ),
        ],
    )

    asserts.eq(len(crash_res.findings), 1)
    asserts.eq(crash_res.findings[0].level, "error")
    asserts.true("Skill and workflow linter error" in crash_res.findings[0].message)

def test_agent_customizations_lint_all_mode_narrowing_and_specified_files():
    """Verifies `--all` skips unrelated CLs, narrows mixed CLs, and runs explicit files."""

    # 1. `--all` on a CL that only modifies an unrelated non-skill file: returns
    # early without invoking skill_linter or emitting warnings on untouched skills.
    res_unrelated_cl = testing.run(
        _agent_customizations_lint,
        files = {
            "README.md": testing.file("Modified unrelated doc.\n", action = "M"),
            ".agents/skills/untouched/SKILL.md": testing.file("---\nname: untouched\n---\n", action = ""),
        },
    )
    asserts.eq(res_unrelated_cl.findings, ())

    # 2. `--all` on a CL that modifies one skill while leaving others untouched:
    # only the modified skill is passed to skill_linter.
    modified_findings = [
        {
            "filepath": ".agents/skills/changed/SKILL.md",
            "line": 4,
            "level": "error",
            "message": "Broken local link.",
        },
    ]
    res_narrowed = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/skills/changed/SKILL.md": testing.file("---\nname: changed\n---\n", action = "M"),
            ".agents/skills/untouched/SKILL.md": testing.file("---\nname: untouched\n---\n", action = ""),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/skills/changed/SKILL.md",
                ],
                stdout = json.encode(modified_findings),
            ),
        ],
    )
    asserts.eq(len(res_narrowed.findings), 1)
    asserts.eq(res_narrowed.findings[0].level, "error")

    # 3. `specifiedFiles` mode (`shac check <file>`), where all passed files have
    # `action == ""`: still runs and reports errors on the explicitly passed file.
    explicit_findings = [
        {
            "filepath": ".agents/skills/explicit/SKILL.md",
            "line": 4,
            "level": "error",
            "message": "Broken local link.",
        },
    ]
    res_explicit = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/skills/explicit/SKILL.md": testing.file("---\nname: explicit\n---\n", action = ""),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/skills/explicit/SKILL.md",
                ],
                stdout = json.encode(explicit_findings),
            ),
        ],
    )
    asserts.eq(len(res_explicit.findings), 1)
    asserts.eq(res_explicit.findings[0], testing.finding(
        level = "error",
        filepath = ".agents/skills/explicit/SKILL.md",
        line = 4,
        message = "Broken local link.",
    ))

def test_agent_customizations_lint_clean_all_and_deleted_files():
    """Verifies clean-tree `--all`, deletion-only CLs, and deleted workflow scripts."""

    # 1. Clean-tree `shac check --all` (`HEAD == upstream`, > 50 tracked files
    # all with `action == ""`): returns early with zero findings or exec calls.
    clean_repo_files = {
        ".agents/skills/untouched/SKILL.md": testing.file("---\nname: untouched\n---\n", action = ""),
    }
    for i in range(55):
        clean_repo_files["src/file_%d.rs" % i] = testing.file("// code\n", action = "")

    res_clean_all = testing.run(
        _agent_customizations_lint,
        files = clean_repo_files,
    )
    asserts.eq(res_clean_all.findings, ())

    # 2. Deletion-only unrelated CL under `shac check --all`: `include_deleted = True`
    # keeps `git_backed == True` so untouched skills are skipped.
    res_del_unrelated = testing.run(
        _agent_customizations_lint,
        files = {
            "src/obsolete.cc": testing.file(action = "D"),
            ".agents/skills/untouched/SKILL.md": testing.file("---\nname: untouched\n---\n", action = ""),
        },
    )
    asserts.eq(res_del_unrelated.findings, ())

    # 3. Deleting a workflow's `workflow.py` without updating `workflow.json`
    # triggers `_agent_customizations_lint` and emits a blocking `error` on
    # `workflow.json`.
    missing_entry_findings = [
        {
            "filepath": ".agents/workflows/demo/workflow.json",
            "line": 1,
            "level": "error",
            "message": 'Workflow entry script "workflow.py" does not exist.',
        },
    ]
    res_del_wf_script = testing.run(
        _agent_customizations_lint,
        files = {
            ".agents/workflows/demo/workflow.json": testing.file('{"name": "demo"}\n', affected = False),
            ".agents/workflows/demo/workflow.py": testing.file(action = "D"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--lint-json",
                    "--repo-root",
                    testing.root,
                    "--fuchsia-root",
                    testing.root,
                    testing.root + "/.agents/workflows/demo/workflow.py",
                ],
                stdout = json.encode(missing_entry_findings),
            ),
        ],
    )
    asserts.eq(res_del_wf_script.findings, (
        testing.finding(
            level = "error",
            filepath = ".agents/workflows/demo/workflow.json",
            line = 1,
            message = 'Workflow entry script "workflow.py" does not exist.',
        ),
    ))
