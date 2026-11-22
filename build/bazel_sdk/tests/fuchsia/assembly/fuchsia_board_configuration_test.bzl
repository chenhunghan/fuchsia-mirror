# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# buildifier: disable=module-docstring
load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_fuchsia//fuchsia/private/assembly:providers.bzl", "FuchsiaBoardConfigInfo")
load("//test_utils:json_validator.bzl", "CREATE_VALIDATION_SCRIPT_ATTRS", "create_validation_script_provider")

def _fuchsia_board_configuration_test_impl(ctx):
    golden_file = ctx.file.golden_file
    board_config = ctx.attr.board_config[FuchsiaBoardConfigInfo]
    if len(ctx.files.board_config) == 1:
        # This is a newly constructed board config with the only file being
        # the directory.
        config_dir = ctx.files.board_config[0]
    else:
        # This is a prebuilt board config.
        config_dir = board_config.directory

    relative_path = "board_configuration.json"
    return [create_validation_script_provider(
        ctx,
        config_dir,
        golden_file,
        relative_path,
        runfiles = ctx.runfiles(files = ctx.files.board_config),
        is_subset = True,
    )]

fuchsia_board_configuration_test = rule(
    doc = """Validate the generated board configuration file.""",
    test = True,
    implementation = _fuchsia_board_configuration_test_impl,
    attrs = {
        "board_config": attr.label(
            doc = "Built Board Config.",
            providers = [FuchsiaBoardConfigInfo],
            mandatory = True,
        ),
        "golden_file": attr.label(
            doc = "Golden file to match against",
            allow_single_file = True,
            mandatory = True,
        ),
    } | CREATE_VALIDATION_SCRIPT_ATTRS,
)

def _fuchsia_hybrid_board_configuration_test_impl(ctx):
    golden_file = ctx.file.golden_bib
    board_config = ctx.attr.hybrid_board_config[FuchsiaBoardConfigInfo]
    if len(ctx.files.hybrid_board_config) == 1:
        # This is a newly constructed board config with the only file being
        # the directory.
        config_dir = ctx.files.hybrid_board_config[0]
    else:
        # This is a prebuilt board config.
        config_dir = board_config.directory

    relative_path = ctx.attr.relative_path_of_board_input_bundle + "/board_input_bundle.json"
    return [create_validation_script_provider(
        ctx,
        config_dir,
        golden_file,
        relative_path,
        runfiles = ctx.runfiles(files = ctx.files.hybrid_board_config),
        is_subset = True,
    )]

fuchsia_hybrid_board_configuration_test = rule(
    doc = """Validate the hybrid board has replaced BIB. Note this test assumes there is only one BIB to verify.""",
    test = True,
    implementation = _fuchsia_hybrid_board_configuration_test_impl,
    attrs = {
        "hybrid_board_config": attr.label(
            doc = "Built hybrid board config target",
            providers = [FuchsiaBoardConfigInfo],
            mandatory = True,
        ),
        "golden_bib": attr.label(
            doc = "Golden BIB file to match against",
            allow_single_file = True,
            mandatory = True,
        ),
        "relative_path_of_board_input_bundle": attr.string(
            doc = "The path inside hyrid_board_config of the board input bundle to check",
            mandatory = True,
        ),
    } | CREATE_VALIDATION_SCRIPT_ATTRS,
)

# Tests for the `arch` argument of fuchsia_board_configuration() and
# fuchsia_prebuilt_board_configuration().
#
# Setting `arch` restricts a board to that CPU: on any other CPU the board is
# incompatible, and so is everything that depends on it, so Bazel skips those
# targets instead of building the board for the wrong architecture.
#
# Each test below builds its board for the board's own CPU, which is forced
# by the test rule (see the `fuchsia_board_arch_*_test` rules), so every test
# runs no matter which CPU the SDK test suite itself is configured for. The
# test then checks that:
#
#   * The board analyzes successfully for that CPU, including when `arch` is
#     combined with an explicit `target_compatible_with`.
#   * For fuchsia_board_configuration(), the generated board configuration
#     JSON contains the expected `arch` (rather than one derived from the
#     target platform).
#
# Limitation: Bazel treats a test that depends on an incompatible target as
# incompatible itself, and the test suite skips it rather than failing it.
# So if a board were wrongly restricted to a different CPU than its test
# forces, the test would show up as skipped, not failed. For the same reason,
# the opposite case (that a board is skipped on other CPUs) can't be tested
# here either.

def _board_arch_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    asserts.true(env, FuchsiaBoardConfigInfo in target)

    if ctx.attr.expected_arch:
        configs = [
            json.decode(action.content)
            for action in analysistest.target_actions(env)
            if action.mnemonic == "FileWrite" and
               action.outputs.to_list()[0].basename.endswith("_board_config.json")
        ]
        asserts.equals(env, 1, len(configs))
        if configs:
            asserts.equals(env, ctx.attr.expected_arch, configs[0]["arch"])

    return analysistest.end(env)

_BOARD_ARCH_TEST_ATTRS = {
    "expected_arch": attr.string(
        doc = "The `arch` expected in the generated board configuration " +
              "JSON. Leave empty for prebuilt boards, whose JSON is not " +
              "generated.",
    ),
}

# Each rule builds the board under test for the Fuchsia platform of one CPU.
# The platform is set through config_settings, which must be fixed when the
# rule is defined, hence one rule per CPU. Label() resolves the platform in
# this file's repository; a plain string would be resolved by skylib.
fuchsia_board_arch_x64_test = analysistest.make(
    _board_arch_test_impl,
    doc = "Checks a board with `arch = \"x64\"` when built for x64.",
    attrs = _BOARD_ARCH_TEST_ATTRS,
    config_settings = {
        "//command_line_option:platforms": str(Label("@rules_fuchsia//fuchsia/constraints/platforms:fuchsia_x64")),
    },
)

fuchsia_board_arch_arm64_test = analysistest.make(
    _board_arch_test_impl,
    doc = "Checks a board with `arch = \"arm64\"` when built for arm64.",
    attrs = _BOARD_ARCH_TEST_ATTRS,
    config_settings = {
        "//command_line_option:platforms": str(Label("@rules_fuchsia//fuchsia/constraints/platforms:fuchsia_arm64")),
    },
)

fuchsia_board_arch_riscv64_test = analysistest.make(
    _board_arch_test_impl,
    doc = "Checks a board with `arch = \"riscv64\"` when built for riscv64.",
    attrs = _BOARD_ARCH_TEST_ATTRS,
    config_settings = {
        "//command_line_option:platforms": str(Label("@rules_fuchsia//fuchsia/constraints/platforms:fuchsia_riscv64")),
    },
)
