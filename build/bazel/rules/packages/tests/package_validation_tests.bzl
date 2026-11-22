# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the build-time validations of `fx_package()`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaDebugSymbolInfo",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageInfo",
)
load(
    "//build/bazel/rules/packages:package_validations.bzl",
    "FuchsiaPackageValidationInfo",
    "VALIDATE_STRUCTURED_CONFIG_MNEMONIC",
    "VERIFY_ELF_BINARIES_MNEMONIC",
    "validate_package_elf_binaries",
    "validate_package_structured_config",
)

visibility("private")

def _expect_package_validation_failure_impl(ctx):
    package = ctx.attr.package
    if ctx.attr.validation == "elf_binaries":
        stamp = validate_package_elf_binaries(
            ctx,
            validate_package_tool = ctx.executable._validate_package_tool,
            package_info = package[FuchsiaPackageInfo],
            debug_symbol_info = package[FuchsiaDebugSymbolInfo],
            prebuilt_unstripped_files = package[FuchsiaPackageValidationInfo].prebuilt_unstripped_files,
            expect_failure = ctx.attr.expected_message,
        )
    else:
        stamp = validate_package_structured_config(
            ctx,
            validate_package_tool = ctx.executable._validate_package_tool,
            configc = ctx.executable._configc,
            package_info = package[FuchsiaPackageInfo],
            expect_failure = ctx.attr.expected_message,
        )
    return [DefaultInfo(files = depset([stamp]))]

expect_package_validation_failure = rule(
    doc = """Verifies that a package validation rejects `package`.

    Runs the same validation action as `fx_package()`, but succeeds only if
    the validation fails with an error containing `expected_message`. Package
    validations fail at execution time, so `analysis_failure_test()` can't
    check them. `package` must opt out of the validation under test, or
    building it would fail.""",
    implementation = _expect_package_validation_failure_impl,
    attrs = {
        "package": attr.label(
            mandatory = True,
            providers = [
                FuchsiaPackageInfo,
                FuchsiaDebugSymbolInfo,
                FuchsiaPackageValidationInfo,
            ],
        ),
        "validation": attr.string(
            mandatory = True,
            values = ["elf_binaries", "structured_config"],
        ),
        "expected_message": attr.string(mandatory = True),
        "_configc": attr.label(
            default = "//tools/configc",
            executable = True,
            cfg = "exec",
        ),
        "_validate_package_tool": attr.label(
            default = "//build/bazel/rules/packages/tools:validate_package",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _package_validations_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    validation_outputs = target[OutputGroupInfo]._validation.to_list() if hasattr(target[OutputGroupInfo], "_validation") else []
    actual = sorted([
        action.mnemonic
        for action in analysistest.target_actions(env)
        if [f for f in action.outputs.to_list() if f in validation_outputs]
    ])
    expected = sorted(
        ([VERIFY_ELF_BINARIES_MNEMONIC] if ctx.attr.expect_elf_binaries else []) +
        ([VALIDATE_STRUCTURED_CONFIG_MNEMONIC] if ctx.attr.expect_structured_config else []),
    )
    asserts.equals(env, expected, actual, "Unexpected validation actions in the `_validation` output group.")

    # analysistest normally reports failures only when the test is run, but
    # these tests are built rather than run (see `build_only_tests`), so fail
    # analysis directly instead.
    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

package_validations_test = analysistest.make(
    _package_validations_test_impl,
    doc = "Verifies which validation actions are in the `_validation` output group of an `fx_package()`.",
    attrs = {
        "expect_elf_binaries": attr.bool(mandatory = True),
        "expect_structured_config": attr.bool(mandatory = True),
    },
)

def _expect_elf_failure_inputs_test_impl(ctx):
    env = analysistest.begin(ctx)
    prebuilt = ctx.attr.package[FuchsiaPackageValidationInfo].prebuilt_unstripped_files
    asserts.true(
        env,
        bool(prebuilt),
        "Expected `package` to carry non-empty `prebuilt_unstripped_files` (e.g. sysroot ld.so.1).",
    )

    elf_actions = [
        action
        for action in analysistest.target_actions(env)
        if action.mnemonic == VERIFY_ELF_BINARIES_MNEMONIC
    ]
    asserts.equals(env, 1, len(elf_actions), "Expected one ELF verification action.")
    if elf_actions:
        inputs = elf_actions[0].inputs.to_list()
        argv = elf_actions[0].argv
        for debug_file in prebuilt:
            asserts.true(
                env,
                debug_file in inputs,
                "Expected {} in ELF verification inputs.".format(debug_file.short_path),
            )
            asserts.true(
                env,
                debug_file.path in argv,
                "Expected {} passed via --debug-file.".format(debug_file.short_path),
            )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

expect_elf_failure_inputs_test = analysistest.make(
    _expect_elf_failure_inputs_test_impl,
    doc = "Verifies that `expect_package_validation_failure()` forwards `prebuilt_unstripped_files` to ELF verification.",
    attrs = {
        "package": attr.label(
            mandatory = True,
            providers = [FuchsiaPackageValidationInfo],
        ),
    },
)
