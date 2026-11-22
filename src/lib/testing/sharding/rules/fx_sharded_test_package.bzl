# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Symbolic macro for defining a sharded Fuchsia test package."""

load(
    "//build/bazel/rules/components:cmc_merge.bzl",
    "cmc_merge",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
    "fx_test_component",
)
load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")
load(
    "//src/lib/testing/expectation/rules:fx_test_component_with_expectations.bzl",
    "RUN_ONLY_CASES_WITH_ERROR_LOGS",
    "SKIP_CASES_WITH_ERROR_LOGS",
    "fx_component_manifest_with_expectations",
    "fx_test_component_with_expectations",
)
load(
    ":shard_config.bzl",
    "testshard_config",
    "testshard_offer_shard",
)

visibility([
    "//src/...",
    "//third_party/gvisor_syscall_tests/...",
    "//vendor/...",
])

# LINT.IfChange

def _fx_sharded_test_package_impl(
        name,
        package_name,
        component_name,
        manifest,
        includes,
        num_shards,
        shard_part_regex,
        expectations,
        treatment_of_cases_with_error_logs,
        deps,
        components,
        subpackages,
        testonly,
        visibility,
        tags,
        **kwargs):
    if testonly != None and not testonly:
        fail("`fx_sharded_test_package()` targets are always testonly.")
    if num_shards < 2:
        fail("`num_shards` must be at least 2, got {}.".format(num_shards))
    if expectations == None and treatment_of_cases_with_error_logs:
        fail("`treatment_of_cases_with_error_logs` can only be set when `expectations` is specified.")

    tags = tags or []
    subtarget_tags = tags if "manual" in tags else tags + ["manual"]
    effective_package_name = package_name or name
    effective_component_name = component_name or effective_package_name
    has_expectations = expectations != None

    sharder = Label("//src/lib/testing/sharding:sharder")
    expected_includes = (
        Label("//src/lib/testing/sharding:expected_includes_client_with_expectations") if has_expectations else Label("//src/lib/testing/sharding:expected_includes_client")
    )

    shard_components = []
    for shard_index in range(num_shards):
        shard_component_name = "{}_shard_{}_of_{}".format(
            effective_component_name,
            shard_index,
            num_shards,
        )
        shard_target_prefix = "{}_shard_{}_of_{}".format(
            name,
            shard_index,
            num_shards,
        )

        config_target_name = shard_target_prefix + "_testshard_config"
        testshard_config(
            name = config_target_name,
            testonly = True,
            shard_component_name = shard_component_name,
            num_shards = num_shards,
            shard_index = shard_index,
            shard_part_regex = shard_part_regex,
            tags = subtarget_tags,
            **kwargs
        )

        offer_shard_target_name = shard_target_prefix + "_testshard_offer_shard"
        testshard_offer_shard(
            name = offer_shard_target_name,
            testonly = True,
            shard_component_name = shard_component_name,
            tags = subtarget_tags,
            **kwargs
        )

        merged_cml_target_name = shard_target_prefix + "_testshard_cml"
        cmc_merge(
            name = merged_cml_target_name,
            testonly = True,
            srcs = [
                ":" + offer_shard_target_name,
                manifest,
            ],
            tags = subtarget_tags,
            **kwargs
        )

        manifest_target_name = shard_target_prefix + "_manifest"
        shard_deps = deps + [
            ":" + config_target_name,
            expected_includes,
        ]
        if has_expectations:
            fx_component_manifest_with_expectations(
                name = manifest_target_name,
                component_name = shard_component_name,
                manifest = ":" + merged_cml_target_name,
                includes = includes,
                tags = subtarget_tags,
                **kwargs
            )
            fx_test_component_with_expectations(
                name = shard_target_prefix,
                compiled_manifest = ":" + manifest_target_name,
                expectations = expectations,
                treatment_of_cases_with_error_logs = treatment_of_cases_with_error_logs,
                deps = shard_deps,
                tags = subtarget_tags,
                **kwargs
            )
        else:
            fx_component_manifest(
                name = manifest_target_name,
                component_name = shard_component_name,
                manifest = ":" + merged_cml_target_name,
                includes = includes,
                testonly = True,
                tags = subtarget_tags,
                **kwargs
            )
            fx_test_component(
                name = shard_target_prefix,
                compiled_manifest = ":" + manifest_target_name,
                deps = shard_deps,
                tags = subtarget_tags,
                **kwargs
            )

        shard_components.append(":" + shard_target_prefix)

    fx_package(
        name = name,
        package_name = package_name or None,
        components = components + [sharder],
        test_components = shard_components,
        subpackages = subpackages,
        testonly = True,
        visibility = visibility,
        tags = tags,
        **kwargs
    )

fx_sharded_test_package = macro(
    doc = """Defines a Fuchsia test package (`fx_package()`) for a sharded test component.

    Shards `manifest` across `num_shards` test components named
    `<component_name>_shard_<i>_of_<num_shards>.cm` (`0 <= i < num_shards`),
    delegating to `fx_component_manifest_with_expectations()` and
    `fx_test_component_with_expectations()` when `expectations` is set (or
    `fx_component_manifest()` and `fx_test_component()` otherwise), and packages
    them with `//src/lib/testing/sharding:sharder` via `fx_package()`. Pass this
    target as `package` to `fx_test()`.
    """,
    implementation = _fx_sharded_test_package_impl,
    inherit_attrs = "common",
    attrs = {
        "package_name": attr.string(
            doc = """Optional name of the Fuchsia package. Defaults to `name` to match
            `fx_package()`.""",
            configurable = False,
        ),
        "component_name": attr.string(
            doc = """Base name for the generated shard components. Defaults to
            `package_name` (`name`), producing
            `<component_name>_shard_<i>_of_<num_shards>.cm`.""",
            configurable = False,
        ),
        "manifest": attr.label(
            doc = "The component manifest file (`.cml`) to shard and compile.",
            allow_single_file = [".cml"],
            mandatory = True,
            configurable = False,
        ),
        "includes": attr.label_list(
            doc = """Manifest shard targets (`cml_shard()`) or `.cml` files included by
            `manifest`, needed for sandboxed `cmc` compilation.""",
            allow_files = [".cml"],
        ),
        "num_shards": attr.int(
            doc = "The number of shards (`>= 2`) into which the component's test cases are divided.",
            mandatory = True,
            configurable = False,
        ),
        "shard_part_regex": attr.string(
            doc = """Optional regex with a single capture group selecting the portion of each
            test case name to hash when assigning shards (use raw strings such as
            `r"section_(\\d+)::case_.*"` in Starlark).""",
        ),
        "expectations": attr.label(
            doc = """Optional `.json5` expectations file, `fx_expectations()`, or
            `fx_generated_expectations()` target. When set, each shard is emitted via
            `fx_component_manifest_with_expectations()` and
            `fx_test_component_with_expectations()`.""",
            allow_single_file = [".json5"],
        ),
        "treatment_of_cases_with_error_logs": attr.string(
            doc = """Identifies how test cases expected to generate error logs should be run
            (`"SKIP_CASES_WITH_ERROR_LOGS"` or `"RUN_ONLY_CASES_WITH_ERROR_LOGS"`).
            Only valid when `expectations` is set. By default, all test cases are run.""",
            values = [
                "",
                SKIP_CASES_WITH_ERROR_LOGS,
                RUN_ONLY_CASES_WITH_ERROR_LOGS,
            ],
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "Dependencies (`fx_packaged_binary()`, `resource()`, `fx_component()`, etc.) of the test component.",
        ),
        "components": attr.label_list(
            doc = "Additional non-test `fx_component()` targets to include in the package.",
        ),
        "subpackages": attr.label_list(
            doc = "Subpackage targets (`fx_package()`) to include in the package.",
        ),
    },
)

# LINT.ThenChange(//src/lib/testing/sharding/fuchsia_sharded_test_package.gni)
