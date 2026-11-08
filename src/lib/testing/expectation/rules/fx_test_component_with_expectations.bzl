# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Symbolic macros for defining a Fuchsia test component and manifest with expectations."""

load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentManifestInfo",
)
load(
    "//build/bazel/rules/components:cmc_merge.bzl",
    "cmc_merge",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
    "fx_test_component",
)
load(
    ":preprocess.bzl",
    "FuchsiaExpectationsOfferShardInfo",
    "preprocess_expectations",
)

visibility([
    "//src/...",
    "//third_party/gvisor_syscall_tests/...",
    "//vendor/...",
])

# LINT.IfChange

SKIP_CASES_WITH_ERROR_LOGS = "SKIP_CASES_WITH_ERROR_LOGS"
RUN_ONLY_CASES_WITH_ERROR_LOGS = "RUN_ONLY_CASES_WITH_ERROR_LOGS"

def _resolve_cases_to_run(treatment_of_cases_with_error_logs):
    if not treatment_of_cases_with_error_logs:
        return "All"
    if treatment_of_cases_with_error_logs == SKIP_CASES_WITH_ERROR_LOGS:
        return "NoErrLogs"
    if treatment_of_cases_with_error_logs == RUN_ONLY_CASES_WITH_ERROR_LOGS:
        return "WithErrLogs"
    fail(
        "Invalid `treatment_of_cases_with_error_logs` {}. Valid values are: {}, {}".format(
            repr(treatment_of_cases_with_error_logs),
            repr(SKIP_CASES_WITH_ERROR_LOGS),
            repr(RUN_ONLY_CASES_WITH_ERROR_LOGS),
        ),
    )

def _expectations_offer_shard_impl(ctx):
    offer_shard = ctx.actions.declare_file(
        "{}.shard.cml".format(ctx.label.name),
    )
    offer_contents = json.encode_indent(
        {
            "offer": [
                {
                    "directory": "pkg",
                    "from": "framework",
                    "as": "expectations",
                    "to": "#expectation-comparer",
                    "subdir": "data/tests/expectations/{}".format(ctx.attr.component_name),
                },
            ],
        },
        indent = "  ",
    ) + "\n"
    ctx.actions.write(
        output = offer_shard,
        content = offer_contents,
    )

    return [
        DefaultInfo(files = depset([offer_shard])),
        FuchsiaExpectationsOfferShardInfo(),
    ]

_expectations_offer_shard = rule(
    doc = "Generates the CML shard offering the expectations package subdirectory to `#expectation-comparer`.",
    implementation = _expectations_offer_shard_impl,
    provides = [FuchsiaExpectationsOfferShardInfo],
    attrs = {
        "component_name": attr.string(
            doc = "The component name used to scope the expectations package subdirectory.",
            mandatory = True,
        ),
    },
)

def _fx_component_manifest_with_expectations_impl(
        name,
        component_name,
        manifest,
        testonly,
        visibility,
        tags,
        **kwargs):
    if testonly != None and not testonly:
        fail("`fx_component_manifest_with_expectations()` targets are always testonly.")

    tags = tags or []
    effective_component_name = component_name or name

    offer_shard_target_name = name + "_expectations_offer_shard"
    _expectations_offer_shard(
        name = offer_shard_target_name,
        testonly = True,
        component_name = effective_component_name,
        tags = tags + ["manual"],
    )

    merged_cml_target_name = name + "_expectations_cml"
    cmc_merge(
        name = merged_cml_target_name,
        testonly = True,
        srcs = [
            ":" + offer_shard_target_name,
            manifest,
        ],
        tags = tags + ["manual"],
    )

    fx_component_manifest(
        name = name,
        component_name = effective_component_name,
        manifest = ":" + merged_cml_target_name,
        testonly = True,
        visibility = visibility,
        tags = tags,
        **kwargs
    )

fx_component_manifest_with_expectations = macro(
    doc = """Compiles a Fuchsia test component manifest (`.cml`) with the expectations offer shard.

    Merges a generated CML shard offering `data/tests/expectations/<component_name>`
    from the package directory as `expectations` to `#expectation-comparer` into
    `manifest` via `cmc_merge()`, and compiles the merged manifest into a `.cm` via
    `fx_component_manifest()`. Pass this target as `compiled_manifest` to
    `fx_test_component_with_expectations()`.
    """,
    implementation = _fx_component_manifest_with_expectations_impl,
    inherit_attrs = fx_component_manifest,
    attrs = {
        "manifest": attr.label(
            doc = "The component manifest file (`.cml`) to compile.",
            allow_single_file = [".cml"],
            mandatory = True,
            configurable = False,
        ),
        "component_name": attr.string(
            doc = "The name of the component. Defaults to `name`.",
            configurable = False,
        ),
    },
)

def _fx_test_component_with_expectations_impl(
        name,
        compiled_manifest,
        expectations,
        treatment_of_cases_with_error_logs,
        deps,
        testonly,
        visibility,
        tags,
        **kwargs):
    if testonly != None and not testonly:
        fail("`fx_test_component_with_expectations()` targets are always testonly.")

    tags = tags or []
    cases_to_run = _resolve_cases_to_run(treatment_of_cases_with_error_logs)

    expectations_target_name = name + "_preprocessed_expectations"
    preprocess_expectations(
        name = expectations_target_name,
        testonly = True,
        compiled_manifest = compiled_manifest,
        expectations = expectations,
        cases_to_run = cases_to_run,
        tags = tags + ["manual"],
    )

    expectation_comparer = Label("//src/lib/testing/expectation:expectation_comparer")
    expected_includes_common = Label("//src/lib/testing/expectation:expected_includes_common")
    component_deps = (deps or []) + [
        ":" + expectations_target_name,
        expectation_comparer,
        expected_includes_common,
    ]

    fx_test_component(
        name = name,
        compiled_manifest = compiled_manifest,
        deps = component_deps,
        testonly = True,
        visibility = visibility,
        tags = tags,
        **kwargs
    )

fx_test_component_with_expectations = macro(
    doc = """Defines a Fuchsia test component with test expectations.

    Preprocesses the provided `expectations` (`.json5` file, `fx_expectations()`, or
    `fx_generated_expectations()`) into a package resource at
    `data/tests/expectations/<compiled_manifest.component_name>/expectations.json5`
    matching the `subdir` offered by `compiled_manifest`
    (`fx_component_manifest_with_expectations()`), verifies via `cmc check-includes`
    that the manifest includes `//src/lib/testing/expectation/meta/common.shard.cml`,
    and produces an `fx_test_component()` target with `expectation_comparer` and the
    expectations resource packaged in its dependencies.
    """,
    implementation = _fx_test_component_with_expectations_impl,
    inherit_attrs = fx_test_component,
    attrs = {
        "compiled_manifest": attr.label(
            doc = "The `fx_component_manifest_with_expectations()` target for the test component.",
            providers = [FuchsiaComponentManifestInfo],
            mandatory = True,
            configurable = False,
        ),
        "expectations": attr.label(
            doc = "A `.json5` expectations file, `fx_expectations()`, or `fx_generated_expectations()` target.",
            allow_single_file = [".json5"],
            mandatory = True,
        ),
        "treatment_of_cases_with_error_logs": attr.string(
            doc = """Identifies how test cases that are expected to generate error logs
            should be run (`"SKIP_CASES_WITH_ERROR_LOGS"` or
            `"RUN_ONLY_CASES_WITH_ERROR_LOGS"`). By default, all test cases are run.""",
            # `attr.string()` defaults to `""` when omitted, so `""` must be
            # included in `values`.
            values = [
                "",
                SKIP_CASES_WITH_ERROR_LOGS,
                RUN_ONLY_CASES_WITH_ERROR_LOGS,
            ],
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "Dependencies (`fx_packaged_binary()`, `resource()`, `fx_component()`, etc.) of the component.",
        ),
        # Set internally by the macro.
        "check_includes": None,
    },
)

# LINT.ThenChange(//src/lib/testing/expectation/fuchsia_test_component_with_expectations.gni)
