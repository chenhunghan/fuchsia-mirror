# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Aspect used to generate rust_project.json file from Rust targets."""

load("@rules_rust//rust:defs.bzl", "rust_analyzer_aspect")
load("@rules_rust//rust/private:providers.bzl", "RustAnalyzerGroupInfo", "RustAnalyzerInfo")

# Aspect to use for building/querying rust-analyzer related data from Rust Bazel targets.
RUST_ANALYZER_ASPECT = "@rules_rust//rust:defs.bzl%rust_analyzer_aspect"

# The list of Bazel output groups to request when using the aspect. This ensures that
# the files referenced from the manifest are properly generated.
# LINT.IfChange(rust_analyzer_output_groups)
RUST_ANALYZER_OUTPUT_GROUPS = [
    "rust_analyzer_crate_spec",
    "rust_generated_srcs",
    "rust_analyzer_proc_macro_dylib",
    "rust_analyzer_src",
]
# LINT.ThenChange(//build/bazel/scripts/bazel_rust_analyzer_utils.py:rust_analyzer_output_groups)

def _generate_rust_analyzer_manifest_impl(target, actx):
    if RustAnalyzerGroupInfo in target:
        crate_infos = target[RustAnalyzerGroupInfo].deps
    elif RustAnalyzerInfo in target:
        crate_infos = [target[RustAnalyzerInfo]]
    else:
        crate_infos = []

    crate_specs = depset(transitive = [info.crate_specs for info in crate_infos]).to_list()

    output = actx.actions.declare_file("%s.fuchsia_rust_analyzer_manifest.json" % target.label.name)

    # LINT.IfChange(rust_analyzer_manifest_schema)
    content_json = {
        "label": str(target.label),
        "crate_specs": [spec.path for spec in crate_specs],
    }
    # LINT.ThenChange(//build/bazel/scripts/bazel_rust_analyzer_utils.py:rust_analyzer_manifest_schema)

    actx.actions.write(output, json.encode_indent(content_json, indent = "  "))

    return [
        OutputGroupInfo(
            fuchsia_rust_analyzer_manifest = depset([output]),
        ),
    ]

generate_rust_analyzer_manifest = aspect(
    doc = """Generate a manifest file describing Rust analyzer outputs.""",
    implementation = _generate_rust_analyzer_manifest_impl,
    # This aspect does not traverse, so no attr_aspects definition here.
    # Ensure that rust_analyze_aspect is run first.
    requires = [rust_analyzer_aspect],
    # Ensure that the result of rust_analyzer_aspect is available.
    required_aspect_providers = [[RustAnalyzerInfo], [RustAnalyzerGroupInfo]],
    provides = [OutputGroupInfo],
)
