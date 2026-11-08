# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for preprocessing test expectation JSON5 files."""

load(
    "@fuchsia_rules_common//:utils.bzl",
    "make_resource_struct",
)
load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentManifestInfo",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)

visibility(["//src/lib/testing/expectation/..."])

# LINT.IfChange

FuchsiaExpectationsInfo = provider(
    doc = "Carries a root expectations `.json5` file and any transitive `.json5` files it includes for the Bazel action sandbox.",
    fields = {
        "root_file": "The root `.json5` expectations `File`.",
        "includes": "A `depset[File]` of `.json5` files transitively included by `root_file`.",
    },
)

FuchsiaExpectationsOfferShardInfo = provider(
    doc = "Marks the generated expectations directory offer shard produced by `fx_component_manifest_with_expectations()`.",
    fields = {},
)

_ExpectationsManifestVerifiedInfo = provider(
    doc = "Records whether a manifest target includes the expectations directory offer shard.",
    fields = {
        "verified": "True if the manifest target includes an `FuchsiaExpectationsOfferShardInfo` shard.",
    },
)

def _verify_expectations_manifest_aspect_impl(target, ctx):
    verified = FuchsiaExpectationsOfferShardInfo in target
    if not verified:
        manifest_dep = getattr(ctx.rule.attr, "manifest", None)
        if (
            manifest_dep and
            _ExpectationsManifestVerifiedInfo in manifest_dep and
            manifest_dep[_ExpectationsManifestVerifiedInfo].verified
        ):
            verified = True
    if not verified:
        for src_dep in getattr(ctx.rule.attr, "srcs", []):
            if (
                _ExpectationsManifestVerifiedInfo in src_dep and
                src_dep[_ExpectationsManifestVerifiedInfo].verified
            ):
                verified = True
                break
    return [_ExpectationsManifestVerifiedInfo(verified = verified)]

_verify_expectations_manifest_aspect = aspect(
    implementation = _verify_expectations_manifest_aspect_impl,
    attr_aspects = ["manifest", "srcs"],
)

def _preprocess_expectations_impl(ctx):
    if not ctx.attr.compiled_manifest[_ExpectationsManifestVerifiedInfo].verified:
        fail(
            "`compiled_manifest` target '{}' was not created by `fx_component_manifest_with_expectations()`. Use `fx_component_manifest_with_expectations()` instead of `fx_component_manifest()`.".format(
                ctx.attr.compiled_manifest.label,
            ),
        )

    if FuchsiaExpectationsInfo in ctx.attr.expectations:
        exp_info = ctx.attr.expectations[FuchsiaExpectationsInfo]
        root_file = exp_info.root_file
        transitive_includes = [exp_info.includes]
    else:
        root_file = ctx.file.expectations
        transitive_includes = []

    preprocessed_file = ctx.actions.declare_file(
        "{}-expectations-preprocessed.json5".format(ctx.label.name),
    )

    args = [
        "--root-expectations-file",
        root_file.path,
        "--preprocessed-expectations-file",
        preprocessed_file.path,
        "--fuchsia-dir",
        ".",
        "--cases-to-run",
        ctx.attr.cases_to_run,
    ]

    ctx.actions.run(
        executable = ctx.executable._preprocessor_tool,
        arguments = args,
        inputs = depset(
            [root_file],
            transitive = transitive_includes,
        ),
        outputs = [preprocessed_file],
        mnemonic = "PreprocessExpectations",
        progress_message = "Preprocessing expectations for %s" % ctx.label,
    )

    manifest_component_name = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].component_name
    dest = "data/tests/expectations/{}/expectations.json5".format(manifest_component_name)

    resource_struct = make_resource_struct(
        src = preprocessed_file,
        dest = dest,
    )

    return [
        FuchsiaPackageResourcesInfo(resources = [resource_struct]),
        DefaultInfo(
            files = depset([preprocessed_file]),
            runfiles = ctx.runfiles(
                root_symlinks = {resource_struct.dest: resource_struct.src},
            ),
        ),
    ]

preprocess_expectations = rule(
    doc = """Preprocesses an expectations `.json5` file or target (`fx_expectations()` or `fx_generated_expectations()`)
    to resolve `include`s and validate it, producing a package resource at
    `data/tests/expectations/<compiled_manifest.component_name>/expectations.json5`.""",
    implementation = _preprocess_expectations_impl,
    attrs = {
        "expectations": attr.label(
            doc = "A `.json5` expectations file, `fx_expectations()`, or `fx_generated_expectations()` target.",
            allow_single_file = [".json5"],
            mandatory = True,
        ),
        "compiled_manifest": attr.label(
            doc = """The `fx_component_manifest_with_expectations()` target whose `component_name`
            determines the package destination path
            `data/tests/expectations/<component_name>/expectations.json5`.""",
            providers = [FuchsiaComponentManifestInfo],
            aspects = [_verify_expectations_manifest_aspect],
            mandatory = True,
        ),
        "cases_to_run": attr.string(
            doc = """String specifier of which cases to run with respect to error logs:
            `"NoErrLogs"`, `"WithErrLogs"`, or `"All"` (default).""",
            default = "All",
            values = ["All", "NoErrLogs", "WithErrLogs"],
        ),
        "_preprocessor_tool": attr.label(
            default = "//src/lib/testing/expectation/preprocess:bin",
            executable = True,
            cfg = "exec",
        ),
    },
    provides = [FuchsiaPackageResourcesInfo],
)

# LINT.ThenChange(//src/lib/testing/expectation/preprocess/preprocess.gni)
