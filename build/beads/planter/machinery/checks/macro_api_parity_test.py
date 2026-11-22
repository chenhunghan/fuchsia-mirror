#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel/style/rules_macros.md enforcement in macro_api_parity.sh."""

import json
import os
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "macro_api_parity.sh"
)

HEADER = """\
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

visibility("private")

"""


def _git(repo: str, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *args], text=True
    ).strip()


def _init_repo(tmpdir: str, files: dict[str, str]) -> str:
    _git(tmpdir, "init", "-q")
    _git(tmpdir, "config", "user.email", "test")
    _git(tmpdir, "config", "user.name", "Test User")
    for rel_path, content in files.items():
        full_path = os.path.join(tmpdir, rel_path)
        os.makedirs(os.path.dirname(full_path), exist_ok=True)
        with open(full_path, "w", encoding="utf-8") as f:
            f.write(content)
    _git(tmpdir, "add", ".")
    _git(tmpdir, "commit", "-q", "-m", "base")
    return _git(tmpdir, "rev-parse", "HEAD")


def _write_files(repo: str, files: dict[str, str]) -> None:
    for rel_path, content in files.items():
        full_path = os.path.join(repo, rel_path)
        os.makedirs(os.path.dirname(full_path), exist_ok=True)
        with open(full_path, "w", encoding="utf-8") as f:
            f.write(content)


def _run_check(repo: str, base_sha: str) -> list[dict]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")}
    env.update(
        {
            "PLANTER_WORKDIR": repo,
            "PLANTER_CHANGE_BASE": base_sha,
            "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
        }
    )
    out = subprocess.run(
        ["bash", SCRIPT], env=env, capture_output=True, text=True, check=True
    )
    return json.loads(out.stdout)


class MacroApiParityTest(unittest.TestCase):
    def test_valid_symbolic_macro_passes(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            _write_files(
                tmpdir,
                {
                    "build/bazel/rules/fx_widget.bzl": HEADER
                    + '''
def _fx_widget_impl(
        name,
        visibility,
        deps,
        optional_config,
        enable_feature,
        extra_srcs,
        **kwargs):
    """Implements fx_widget."""
    if "args" in kwargs:
        fail("Use widget_args instead of args")
    helper_name = name + "_helper"
    native.filegroup(
        name = helper_name,
        srcs = extra_srcs,
    )
    all_deps = (deps if deps != None else []) + [":" + helper_name]
    if optional_config != None:
        config_alias = name + "_config"
        native.alias(
            name = config_alias,
            actual = optional_config,
        )
        all_deps = all_deps + [":" + config_alias]
    if enable_feature:
        all_deps = all_deps + ["//src/lib:feature"]
    native.filegroup(
        name = name,
        srcs = all_deps,
        visibility = visibility,
        **kwargs
    )

fx_widget = macro(
    doc = "Defines a Fuchsia widget.",
    implementation = _fx_widget_impl,
    inherit_attrs = native.filegroup,
    attrs = {
        "optional_config": attr.label(
            doc = "Optional configuration target.",
        ),
        "enable_feature": attr.bool(
            doc = "Whether to enable the extra feature.",
            default = False,
            configurable = False,
        ),
        "extra_srcs": attr.label_list(
            doc = "Additional sources.",
            default = [],
        ),
        "_tool": attr.label(
            default = "//tools:widget_gen",
            executable = True,
            cfg = "exec",
        ),
    },
)
'''
                },
            )
            self.assertEqual(_run_check(tmpdir, base), [])

    def test_configurable_attr_misuse_and_non_none_default_none_compare(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            _write_files(
                tmpdir,
                {
                    "src/foo/defs.bzl": HEADER
                    + """
def _fx_bad_impl(name, visibility, flag, items, prefix, opt_label):
    if flag:
        pass
    if items != None:
        pass
    resolved_prefix = prefix if prefix != None else "default_prefix"
    if opt_label:
        pass
    native.filegroup(
        name = name,
        srcs = [opt_label],
        deprecation = resolved_prefix,
        visibility = visibility,
    )

fx_bad = macro(
    doc = "Macro with configurable attribute violations.",
    implementation = _fx_bad_impl,
    attrs = {
        "flag": attr.bool(doc = "Flag."),
        "items": attr.label_list(doc = "Items."),
        "prefix": attr.string(doc = "Prefix."),
        "opt_label": attr.label(doc = "Optional label."),
    },
)
"""
                },
            )
            findings = _run_check(tmpdir, base)
            cats = [f["category"] for f in findings]
            self.assertIn("macro_configurable_attr_misuse", cats)
            self.assertEqual(
                cats.count("macro_non_none_default_none_compare"), 2
            )
            none_cmp_rems = [
                f["remediation"]
                for f in findings
                if f["category"] == "macro_non_none_default_none_compare"
            ]
            self.assertTrue(
                any("never use `if items:`" in r for r in none_cmp_rems)
            )

    def test_unchanged_preexisting_configurable_violation_downgrades_to_info(
        self,
    ):
        existing_bzl = (
            HEADER
            + """
ALLOWLIST = [
    "//src/a",
]

def _fx_legacy_impl(name, visibility, flag):
    if flag:
        pass
    native.filegroup(name = name, visibility = visibility)

fx_legacy = macro(
    doc = "Preexisting macro.",
    implementation = _fx_legacy_impl,
    attrs = {
        "flag": attr.bool(doc = "Flag."),
    },
)
"""
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"src/foo/defs.bzl": existing_bzl})
            _write_files(
                tmpdir,
                {
                    "src/foo/defs.bzl": existing_bzl.replace(
                        '    "//src/a",\n',
                        '    "//src/a",\n    "//src/b",\n',
                    )
                },
            )
            findings = _run_check(tmpdir, base)
            blocking = [f for f in findings if f["severity"] != "info"]
            self.assertEqual(blocking, [])
            self.assertTrue(
                any(
                    f["category"] == "macro_configurable_attr_misuse"
                    and f["severity"] == "info"
                    for f in findings
                )
            )

    def test_style_rules_impl_defaults_naming_docs_kwargs_and_prefix(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            _write_files(
                tmpdir,
                {
                    "src/foo/bad_style.bzl": HEADER
                    + """
def wrong_impl_name(name, visibility, deps = [], **kwargs):
    extra = kwargs.get("testonly", False)
    native.filegroup(
        name = name + "_inner",
        srcs = deps,
    )
    native.filegroup(
        name = name,
        srcs = [":" + name + "_inner"],
        testonly = extra,
        visibility = visibility,
    )

fuchsia_thing = macro(
    doc = "Part one. " + "Part two.",
    implementation = wrong_impl_name,
    attrs = {
        "deps": attr.label_list(),
    },
)

def legacy_helper(name, enable = False, visibility = None):
    native.filegroup(name = name + "_priv")
    native.filegroup(name = name)
"""
                },
            )
            findings = _run_check(tmpdir, base)
            cats = {f["category"] for f in findings if f["severity"] == "error"}
            self.assertIn("macro_impl_default_arg", cats)
            self.assertIn("rule_or_macro_impl_name", cats)
            self.assertIn("bzl_fuchsia_prefix_outside_sdk", cats)
            self.assertIn("bzl_docstring_concat", cats)
            self.assertIn("missing_bzl_documentation", cats)
            self.assertIn("macro_kwargs_attr_extraction", cats)
            self.assertIn("macro_inline_target_name_duplication", cats)
            self.assertIn("bzl_public_func_positional_defaults", cats)
            self.assertIn("legacy_macro_missing_target_visibility", cats)

    def test_bzl_antipatterns_and_gni_crate_name(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            _write_files(
                tmpdir,
                {
                    "src/foo/rule.bzl": HEADER
                    + """
def _fx_bad_rule_impl(ctx):
    out = ctx.actions.declare_file("bazel-bin/out.txt")
    ctx.actions.run(
        outputs = [out],
        executable = "python3",
    )
    return [DefaultInfo(files = depset([out]))]

fx_bad_rule = rule(
    doc = "Bad rule.",
    implementation = _fx_bad_rule_impl,
)
""",
                    "src/foo/tmpl.gni": """
template("my_rust_tmpl") {
  _crate = target_name
  if (defined(invoker.crate_name)) {
    _crate = invoker.crate_name
  }
  _target = "${target_name}_inner"
  rustc_library(_target) {
    crate_name = _target
  }
}
""",
                },
            )
            findings = _run_check(tmpdir, base)
            antipattern_msgs = [
                f["message"]
                for f in findings
                if f["category"] == "bzl_antipattern"
            ]
            self.assertEqual(len(antipattern_msgs), 2)
            self.assertTrue(any("bazel-bin" in m for m in antipattern_msgs))
            self.assertTrue(any("python3" in m for m in antipattern_msgs))
            self.assertTrue(
                any(
                    f["category"] == "gn_template_crate_name_not_forwarded"
                    for f in findings
                )
            )

    def test_loaded_starlark_func_calls_unexpected_and_missing_kwargs(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/bazel/rules/helper.bzl": HEADER
                    + """
def fx_helper(*, name, required_dep, optional_flag = False):
    \"\"\"Helper function without **kwargs.\"\"\"
    native.filegroup(
        name = name,
        srcs = [required_dep] if optional_flag else [],
        visibility = ["//visibility:private"],
    )
"""
                },
            )
            _write_files(
                tmpdir,
                {
                    "src/foo/BUILD.bazel": """
load("//build/bazel/rules:helper.bzl", "fx_helper")

fx_helper(
    name = "my_helper",
    bogus_kwarg = True,
)
"""
                },
            )
            findings = _run_check(tmpdir, base)
            cats = {f["category"] for f in findings}
            self.assertIn("starlark_call_unexpected_kwarg", cats)
            self.assertIn("starlark_call_missing_kwarg", cats)


if __name__ == "__main__":
    unittest.main()
