# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for patched `@rules_rust` compiler, clippy, and test flag behavior."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_rust//rust:defs.bzl", "rust_clippy_aspect", "rust_library", "rust_lint_config", "rust_proc_macro", "rust_test")
load("@rules_rust//rust:rust_common.bzl", "rust_common")

visibility("private")

def _write_text_file_impl(ctx):
    output = ctx.actions.declare_file(ctx.attr.filename)
    ctx.actions.write(
        output = output,
        content = ctx.attr.content,
    )
    providers = [DefaultInfo(files = depset([output]))]
    if ctx.attr.cfgs:
        providers.append(
            platform_common.TemplateVariableInfo({"RUSTC_FLAGS_FILE_CFGS": "\n".join(ctx.attr.cfgs)}),
        )
    return providers

write_text_file = rule(
    doc = "Writes a deterministic test response or environment file.",
    implementation = _write_text_file_impl,
    attrs = {
        "cfgs": attr.string_list(default = [], doc = "Cfg names exposed via TemplateVariableInfo."),
        "content": attr.string(default = "", doc = "Text written to the output file."),
        "filename": attr.string(mandatory = True, doc = "Output filename declared in the package."),
    },
)

_ProcMacroActionsInfo = provider(
    doc = "Captures Rustc action argv lists from exec-configured proc_macro_deps.",
    fields = ["proc_macro_argvs"],
)

def _proc_macro_actions_aspect_impl(target, ctx):
    if ctx.rule.kind == "rust_proc_macro":
        return [
            _ProcMacroActionsInfo(
                proc_macro_argvs = [a.argv for a in target.actions if a.mnemonic == "Rustc"],
            ),
        ]
    collected = []
    for dep in getattr(ctx.rule.attr, "proc_macro_deps", []):
        if _ProcMacroActionsInfo in dep:
            collected.extend(dep[_ProcMacroActionsInfo].proc_macro_argvs)
    return [_ProcMacroActionsInfo(proc_macro_argvs = collected)]

_proc_macro_actions_aspect = aspect(
    implementation = _proc_macro_actions_aspect_impl,
    attr_aspects = ["proc_macro_deps"],
    provides = [_ProcMacroActionsInfo],
)

_MOCK_TOOLCHAIN_CONFIG_SETTINGS = {
    "//command_line_option:extra_toolchains": [
        str(Label("//build/bazel/rules/rust/tests/rules_rust_flags:mock_rust_toolchain")),
    ],
}

def _exec_proc_macro_crate_type_flags_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    proc_macro_argvs = target[_ProcMacroActionsInfo].proc_macro_argvs
    asserts.equals(env, 1, len(proc_macro_argvs), "Expected 1 exec proc-macro Rustc action.")
    if proc_macro_argvs:
        pm_argv = proc_macro_argvs[0]
        asserts.true(
            env,
            "--cfg=exec_proc_macro_flag" in pm_argv,
            "Expected '--cfg=exec_proc_macro_flag' in exec proc-macro argv: {}".format(pm_argv),
        )
        asserts.true(
            env,
            "--cfg=from_extra_exec_rustc_flags" in pm_argv,
            "Expected '--cfg=from_extra_exec_rustc_flags' in exec proc-macro argv: {}".format(pm_argv),
        )
        asserts.false(
            env,
            "--cfg=non_exec_proc_macro_flag" in pm_argv,
            "Did not expect non-exec proc-macro flag in exec proc-macro argv: {}".format(pm_argv),
        )

    # When built directly for the target/host configuration, the consumer rlib
    # must receive non-exec crate-type and extra flags. Under `build_for_host`,
    # the entire test suite runs in `-exec`.
    if "-exec" not in ctx.genfiles_dir.path:
        rustc_actions = [a for a in target.actions if a.mnemonic == "Rustc"]
        asserts.equals(env, 1, len(rustc_actions), "Expected 1 consumer Rustc action.")
        if rustc_actions:
            lib_argv = rustc_actions[0].argv
            asserts.true(
                env,
                "--cfg=non_exec_rlib_flag" in lib_argv,
                "Expected '--cfg=non_exec_rlib_flag' in rlib argv: {}".format(lib_argv),
            )
            asserts.true(
                env,
                "--cfg=from_extra_rustc_flags" in lib_argv,
                "Expected '--cfg=from_extra_rustc_flags' in rlib argv: {}".format(lib_argv),
            )
            asserts.false(
                env,
                "--cfg=exec_proc_macro_flag" in lib_argv,
                "Did not expect exec proc-macro flag in rlib argv: {}".format(lib_argv),
            )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_exec_proc_macro_crate_type_flags_test = analysistest.make(
    _exec_proc_macro_crate_type_flags_test_impl,
    config_settings = _MOCK_TOOLCHAIN_CONFIG_SETTINGS,
    extra_target_under_test_aspects = [_proc_macro_actions_aspect],
)

def _assert_arg_contains(env, items, substring, label):
    matched = any([substring in item for item in items])
    asserts.true(
        env,
        matched,
        "Expected '{}' in {}: {}".format(substring, label, items),
    )

def _assert_arg_not_contains(env, items, substring, label):
    matched = any([substring in item for item in items])
    asserts.false(
        env,
        matched,
        "Did not expect '{}' in {}: {}".format(substring, label, items),
    )

def _wrapped_rust_test_crate_files_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    rustc_actions = [a for a in target.actions if a.mnemonic == "Rustc"]
    asserts.equals(env, 1, len(rustc_actions), "Expected 1 Rustc action.")
    if rustc_actions:
        argv = rustc_actions[0].argv
        inputs = [f.basename for f in rustc_actions[0].inputs.to_list()]
        for s in ctx.attr.expected_argv_substrings:
            _assert_arg_contains(env, argv, s, "Rustc argv")
        for s in ctx.attr.unexpected_argv_substrings:
            _assert_arg_not_contains(env, argv, s, "Rustc argv")
        for name in ctx.attr.expected_input_basenames:
            asserts.true(
                env,
                name in inputs,
                "Expected input '{}' in Rustc inputs: {}".format(name, inputs),
            )
        for name in ctx.attr.unexpected_input_basenames:
            asserts.false(
                env,
                name in inputs,
                "Did not expect input '{}' in Rustc inputs: {}".format(name, inputs),
            )

    cfgs = target[rust_common.crate_info].cfgs
    for cfg in ctx.attr.expected_cfgs:
        asserts.true(env, cfg in cfgs, "Expected cfg '{}' in CrateInfo.cfgs: {}".format(cfg, cfgs))
    for cfg in ctx.attr.unexpected_cfgs:
        asserts.false(env, cfg in cfgs, "Did not expect cfg '{}' in CrateInfo.cfgs: {}".format(cfg, cfgs))

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_wrapped_rust_test_crate_files_test = analysistest.make(
    _wrapped_rust_test_crate_files_test_impl,
    config_settings = _MOCK_TOOLCHAIN_CONFIG_SETTINGS | {
        str(Label("@rules_rust//rust/settings:collect_cfgs")): True,
    },
    attrs = {
        "expected_argv_substrings": attr.string_list(default = []),
        "expected_cfgs": attr.string_list(default = []),
        "expected_input_basenames": attr.string_list(default = []),
        "unexpected_argv_substrings": attr.string_list(default = []),
        "unexpected_cfgs": attr.string_list(default = []),
        "unexpected_input_basenames": attr.string_list(default = []),
    },
)

def _clippy_flag_order_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    clippy_actions = [a for a in target.actions if a.mnemonic == "Clippy"]
    asserts.equals(env, 1, len(clippy_actions), "Expected 1 Clippy action.")
    if clippy_actions:
        argv = clippy_actions[0].argv

        # `build_for_host` runs the suite under `-exec`, where `collect_extra_rustc_flags`
        # emits `extra_exec_rustc_flags` instead of `extra_rustc_flags`.
        is_exec = "-exec" in ctx.genfiles_dir.path
        expected = (
            ctx.attr.expected_exec_ordered_substrings if (is_exec and ctx.attr.expected_exec_ordered_substrings) else ctx.attr.expected_ordered_substrings
        )
        unexpected = (
            ctx.attr.unexpected_exec_substrings if is_exec else ctx.attr.unexpected_substrings
        )
        prev_idx = -1
        prev_pattern = None
        for pattern in expected:
            matched_idx = -1
            for i in range(prev_idx + 1, len(argv)):
                if pattern in argv[i]:
                    matched_idx = i
                    break
            asserts.true(
                env,
                matched_idx >= 0,
                "Expected '{}' after '{}' (index {}) in Clippy argv: {}".format(
                    pattern,
                    prev_pattern,
                    prev_idx,
                    argv,
                ),
            )
            if matched_idx >= 0:
                prev_idx = matched_idx
                prev_pattern = pattern
        for pattern in unexpected:
            _assert_arg_not_contains(env, argv, pattern, "Clippy argv")

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_clippy_flag_order_test = analysistest.make(
    _clippy_flag_order_test_impl,
    config_settings = _MOCK_TOOLCHAIN_CONFIG_SETTINGS | {
        str(Label("@rules_rust//rust/settings:clippy_flag")): [],
    },
    extra_target_under_test_aspects = [rust_clippy_aspect],
    attrs = {
        "expected_exec_ordered_substrings": attr.string_list(default = []),
        "expected_ordered_substrings": attr.string_list(mandatory = True),
        "unexpected_exec_substrings": attr.string_list(default = []),
        "unexpected_substrings": attr.string_list(default = []),
    },
)

def rules_rust_flags_analysis_test_suite(name, *, visibility = None):
    """Defines fixtures and analysis tests for `@rules_rust` flag ordering and opt-out.

    Args:
        name: Name of the generated `test_suite` target.
        visibility: Visibility of the generated `test_suite` target.
    """
    write_text_file(
        name = "exec_pm_allow_unused_rsp",
        content = "-Aunused_variables\n-Aclippy::redundant_clone\n",
        filename = "exec_pm_allow_unused.rsp",
        visibility = ["//visibility:private"],
    )

    rust_proc_macro(
        name = "exec_proc_macro_fixture",
        srcs = ["proc_macro_noop.rs"],
        edition = "2024",
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    rust_library(
        name = "exec_proc_macro_consumer_fixture",
        srcs = ["lib_clippy_override.rs"],
        edition = "2024",
        proc_macro_deps = [":exec_proc_macro_fixture"],
        rustc_flags_files = [":exec_pm_allow_unused_rsp"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    _exec_proc_macro_crate_type_flags_test(
        name = "exec_proc_macro_crate_type_flags_test",
        target_under_test = ":exec_proc_macro_consumer_fixture",
        visibility = ["//visibility:private"],
    )

    write_text_file(
        name = "mock_crate_default_rsp",
        cfgs = ["from_crate_default"],
        content = "--cfg=from_crate_default\n",
        filename = "mock_crate_default.rsp",
        visibility = ["//visibility:private"],
    )
    write_text_file(
        name = "mock_crate_target_rsp",
        cfgs = ["from_crate_target"],
        content = "--cfg=from_crate_target\n",
        filename = "mock_crate_target.rsp",
        visibility = ["//visibility:private"],
    )
    write_text_file(
        name = "mock_crate_env",
        content = "ENV_KEY=crate\n",
        filename = "mock_crate.env",
        visibility = ["//visibility:private"],
    )
    write_text_file(
        name = "mock_test_default_rsp",
        cfgs = ["from_test_default"],
        content = "--cfg=from_test_default\n",
        filename = "mock_test_default.rsp",
        visibility = ["//visibility:private"],
    )
    write_text_file(
        name = "mock_test_target_rsp",
        cfgs = ["from_test_target"],
        content = "--cfg=from_test_target\n",
        filename = "mock_test_target.rsp",
        visibility = ["//visibility:private"],
    )
    write_text_file(
        name = "mock_test_env",
        content = "ENV_KEY=test\n",
        filename = "mock_test.env",
        visibility = ["//visibility:private"],
    )

    rust_library(
        name = "mock_wrapped_lib_fixture",
        srcs = ["lib_wrapped_test.rs"],
        default_rustc_flags_files = [":mock_crate_default_rsp"],
        edition = "2024",
        rustc_env_files = [":mock_crate_env"],
        rustc_flags_files = [":mock_crate_target_rsp"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    rust_test(
        name = "mock_wrapped_keep_files_fixture",
        crate = ":mock_wrapped_lib_fixture",
        default_rustc_flags_files = [":mock_test_default_rsp"],
        features = ["-opt_out_extra_flags"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    _wrapped_rust_test_crate_files_test(
        name = "wrapped_test_preserves_crate_files_by_default_test",
        expected_argv_substrings = [
            "mock_test_default.rsp",
            "mock_crate_target.rsp",
        ],
        expected_cfgs = [
            "from_test_default",
            "from_crate_target",
        ],
        expected_input_basenames = [
            "mock_test_default.rsp",
            "mock_crate_target.rsp",
            "mock_crate.env",
        ],
        target_under_test = ":mock_wrapped_keep_files_fixture",
        unexpected_argv_substrings = [
            "mock_crate_default.rsp",
            "--cfg=from_extra_rustc_flags",
            "--cfg=from_extra_exec_rustc_flags",
        ],
        unexpected_cfgs = ["from_crate_default"],
        unexpected_input_basenames = ["mock_crate_default.rsp"],
        visibility = ["//visibility:private"],
    )

    rust_test(
        name = "mock_wrapped_replace_files_fixture",
        crate = ":mock_wrapped_lib_fixture",
        default_rustc_flags_files = [":mock_test_default_rsp"],
        features = ["-opt_out_extra_flags"],
        replace_crate_rustc_flags_files = True,
        rustc_env_files = [":mock_test_env"],
        rustc_flags_files = [":mock_test_target_rsp"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    _wrapped_rust_test_crate_files_test(
        name = "wrapped_test_replace_crate_files_opt_in_test",
        expected_argv_substrings = [
            "mock_test_default.rsp",
            "mock_test_target.rsp",
        ],
        expected_cfgs = [
            "from_test_default",
            "from_test_target",
        ],
        expected_input_basenames = [
            "mock_test_default.rsp",
            "mock_test_target.rsp",
            "mock_test.env",
        ],
        target_under_test = ":mock_wrapped_replace_files_fixture",
        unexpected_argv_substrings = [
            "mock_crate_default.rsp",
            "mock_crate_target.rsp",
            "--cfg=from_extra_rustc_flags",
            "--cfg=from_extra_exec_rustc_flags",
        ],
        unexpected_cfgs = [
            "from_crate_default",
            "from_crate_target",
        ],
        unexpected_input_basenames = [
            "mock_crate_default.rsp",
            "mock_crate_target.rsp",
            "mock_crate.env",
        ],
        visibility = ["//visibility:private"],
    )

    rust_lint_config(
        name = "clippy_order_lints",
        clippy = {
            "redundant_clone": "deny",
        },
        rustc = {
            "unused_variables": "deny",
        },
        visibility = ["//visibility:private"],
    )

    write_text_file(
        name = "clippy_default_allow_warnings_rsp",
        content = "-Awarnings\n",
        filename = "clippy_default_allow_warnings.rsp",
        visibility = ["//visibility:private"],
    )

    write_text_file(
        name = "clippy_target_allow_rsp",
        content = "-Aunused_variables\n",
        filename = "clippy_target_allow.rsp",
        visibility = ["//visibility:private"],
    )

    rust_library(
        name = "clippy_order_lint_config_fixture",
        srcs = ["lib_clippy_override.rs"],
        default_rustc_flags_files = [":clippy_default_allow_warnings_rsp"],
        edition = "2024",
        lint_config = ":clippy_order_lints",
        rustc_flags = ["-Aclippy::redundant_clone"],
        rustc_flags_files = [":clippy_target_allow_rsp"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    rust_library(
        name = "clippy_order_default_dwarnings_fixture",
        srcs = ["lib_clippy_override.rs"],
        default_rustc_flags_files = [":clippy_default_allow_warnings_rsp"],
        edition = "2024",
        rustc_flags = ["-Aclippy::redundant_clone"],
        rustc_flags_files = [":clippy_target_allow_rsp"],
        tags = ["manual"],
        visibility = ["//visibility:private"],
    )

    _clippy_flag_order_test(
        name = "clippy_lint_config_precedence_test",
        expected_exec_ordered_substrings = [
            "clippy_default_allow_warnings.rsp",
            "--cfg=from_extra_exec_rustc_flags",
            "--deny=unused_variables",
            "clippy_target_allow.rsp",
            "-Aclippy::redundant_clone",
        ],
        expected_ordered_substrings = [
            "clippy_default_allow_warnings.rsp",
            "--cfg=from_extra_rustc_flags",
            "--deny=unused_variables",
            "clippy_target_allow.rsp",
            "-Aclippy::redundant_clone",
        ],
        target_under_test = ":clippy_order_lint_config_fixture",
        unexpected_exec_substrings = ["--cfg=from_extra_rustc_flags"],
        unexpected_substrings = ["--cfg=from_extra_exec_rustc_flags"],
        visibility = ["//visibility:private"],
    )

    _clippy_flag_order_test(
        name = "clippy_default_dwarnings_precedence_test",
        expected_exec_ordered_substrings = [
            "clippy_default_allow_warnings.rsp",
            "--cfg=from_extra_exec_rustc_flags",
            "-Dwarnings",
            "clippy_target_allow.rsp",
            "-Aclippy::redundant_clone",
        ],
        expected_ordered_substrings = [
            "clippy_default_allow_warnings.rsp",
            "--cfg=from_extra_rustc_flags",
            "-Dwarnings",
            "clippy_target_allow.rsp",
            "-Aclippy::redundant_clone",
        ],
        target_under_test = ":clippy_order_default_dwarnings_fixture",
        unexpected_exec_substrings = ["--cfg=from_extra_rustc_flags"],
        unexpected_substrings = ["--cfg=from_extra_exec_rustc_flags"],
        visibility = ["//visibility:private"],
    )

    native.test_suite(
        name = name,
        tests = [
            ":clippy_default_dwarnings_precedence_test",
            ":clippy_lint_config_precedence_test",
            ":exec_proc_macro_crate_type_flags_test",
            ":wrapped_test_preserves_crate_files_by_default_test",
            ":wrapped_test_replace_crate_files_opt_in_test",
        ],
        visibility = visibility,
    )
