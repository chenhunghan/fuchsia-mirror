# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis failure tests for `fx_test()` and test environment validation."""

load("//build/bazel/rules/testing:defs.bzl", "analysis_failure_test")
load("//build/bazel/rules/testing:fx_test.bzl", "fx_test")
load(
    "//build/bazel/rules/testing:fx_test_environment.bzl",
    "fx_test_environment",
    "fx_test_environment_variant",
)

_AEMU_DIMENSIONS = {"device_type": "AEMU"}

# Each case is (name, `fx_test_environment()` kwargs, expected error
# substring).
_ENVIRONMENT_CASES = [
    (
        "empty_dimensions",
        {"dimensions": {}},
        "`dimensions` must not be empty.",
    ),
    (
        "tags_in_dimensions",
        {"dimensions": {"device_type": "AEMU", "tags": "emulated"}},
        "Use `env_tags` instead of a 'tags' dimension.",
    ),
    (
        "unknown_emulator_field",
        {"dimensions": _AEMU_DIMENSIONS, "emulator": {"name": "a", "uefi": "true"}},
        "Unknown `emulator` field 'uefi'",
    ),
    (
        "emulator_without_name",
        {"dimensions": _AEMU_DIMENSIONS, "emulator": {"device": "x64-emu-min"}},
        "`emulator` requires a unique 'name'.",
    ),
]

def _failure_test(name, target_under_test, expected_message):
    analysis_failure_test(
        name = name,
        target_under_test = target_under_test,
        expected_message = expected_message,
    )
    return ":" + name

def fx_test_validation_tests(name, package, **kwargs):
    """Defines an `analysis_failure_test()` for each invalid test or environment.

    Args:
        name: The name of the `test_suite()` grouping all the failure tests.
        package: An `fx_package()` target with test components, used by every
            `fx_test()` under test so that only the argument being tested is
            invalid.
        **kwargs: Common attributes for the `test_suite()` (e.g. `visibility`).
    """
    tests = []

    for case_name, env_kwargs, expected_message in _ENVIRONMENT_CASES:
        target_name = "{}_{}".format(name, case_name)
        fx_test_environment(
            name = target_name + "_env",
            tags = ["manual"],
            **env_kwargs
        )
        tests.append(_failure_test(
            name = target_name,
            target_under_test = ":{}_env".format(target_name),
            expected_message = expected_message,
        ))

    fx_test_environment(
        name = name + "_aemu_env",
        dimensions = _AEMU_DIMENSIONS,
        tags = ["manual"],
    )
    fx_test_environment_variant(
        name = name + "_tagged_aemu_env",
        base = ":{}_aemu_env".format(name),
        env_tags = ["emulated"],
        tags = ["manual"],
    )

    fx_test_environment_variant(
        name = name + "_variant_of_variant_env",
        base = ":{}_tagged_aemu_env".format(name),
        env_tags = ["other"],
        tags = ["manual"],
    )
    tests.append(_failure_test(
        name = name + "_variant_of_variant",
        target_under_test = ":{}_variant_of_variant_env".format(name),
        expected_message = "is itself a variant.",
    ))

    fx_test_environment_variant(
        name = name + "_unchanged_variant_env",
        base = ":{}_aemu_env".format(name),
        tags = ["manual"],
    )
    tests.append(_failure_test(
        name = name + "_unchanged_variant",
        target_under_test = ":{}_unchanged_variant_env".format(name),
        expected_message = "A variant must change something.",
    ))

    fx_test(
        name = name + "_build_only_with_environments_fx_test",
        package = package,
        build_only = True,
        environments = [":{}_aemu_env".format(name)],
        tags = ["manual"],
    )
    tests.append(_failure_test(
        name = name + "_build_only_with_environments",
        target_under_test = ":{}_build_only_with_environments_fx_test".format(name),
        expected_message = "build_only tests should not specify environments",
    ))

    # A `select()` that picks no environments mustn't silently fall back to
    # the default environments.
    fx_test(
        name = name + "_empty_environments_fx_test",
        package = package,
        environments = select({"//conditions:default": []}),
        tags = ["manual"],
    )
    tests.append(_failure_test(
        name = name + "_empty_environments",
        target_under_test = ":{}_empty_environments_fx_test".format(name),
        expected_message = "`environments` is empty.",
    ))

    fx_test(
        name = name + "_non_environment_fx_test",
        package = package,
        environments = [package],
        tags = ["manual"],
    )
    tests.append(_failure_test(
        name = name + "_non_environment",
        target_under_test = ":{}_non_environment_fx_test".format(name),
        expected_message = "is not an `fx_test_environment()`.",
    ))

    native.test_suite(
        name = name,
        tests = tests,
        **kwargs
    )
