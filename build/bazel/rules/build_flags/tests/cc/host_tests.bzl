# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper macros and verification rules for C++ `build_flags()` host tests."""

load(
    "@fuchsia_rules_common//build_flags:aspects.bzl",
    "BuildFlagsLabelsInfo",
    "collect_build_flags_labels_aspect",
)
load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "wrap_cc_macro_args_with_build_flags",
)
load("@fuchsia_rules_common//build_flags:providers.bzl", "BuildFlagsInfo", "BuildFlagsListInfo")
load("@rules_cc//cc:cc_library.bzl", "cc_library")
load("@rules_cc//cc:find_cc_toolchain.bzl", "CC_TOOLCHAIN_ATTRS", "find_cc_toolchain", "use_cc_toolchain")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")

visibility(["//build/bazel/rules/build_flags/tests/cc/..."])

def build_flags_cc_binary_host_test(
        name,
        build_flags = [],
        disable_build_flags = [],
        transitive_features = [],
        **kwargs):
    """Define a new C++ or C host test for build_flags() support.

    Args:
       name: (string) Name of the host_test() target, the binary will be named "{name}_bin".
       build_flags: (list[label]) optional list of labels to build_flags() targets.
       disable_build_flags: (list[label]) optional list of labels to build_flags() targets.
       transitive_features: (list[string]) optional list of feature names to apply transitively.
       **kwargs: All other arguments are passed to fx_cc_binary().
    """
    binary_name = "{}_bin".format(name)

    fx_cc_binary(
        name = binary_name,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        transitive_features = transitive_features,
        **kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
    )

def build_flags_cc_library(
        name,
        build_flags = [],
        disable_build_flags = [],
        **kwargs):
    """Define a new C++ library for build_flags() support.

    Args:
       name: (string) Name of the library target.
       build_flags: (list[label]) optional list of labels to build_flags() targets.
       disable_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to cc_library().
    """
    new_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs,
        name = name,
        cc_rule_name = "cc_library",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = "cxx_common",
    )

    cc_library(
        name = name,
        **new_kwargs
    )

def _verify_build_flags_labels_bin_impl(ctx):
    actual_labels = ctx.attr.target_under_test[BuildFlagsLabelsInfo].labels
    failures = []

    # Verify every expected build_flags() label is present and
    # appears strictly after the preceding expected label, confirming flag
    # precedence order (e.g., no-fat-lto-objects-linking after caller flags).
    prev_index = -1
    prev_label = None
    for expected_target in ctx.attr.expected_ordered_labels:
        expected_label = expected_target[BuildFlagsInfo].label
        if expected_label not in actual_labels:
            failures.append("Expected build_flags label {} not found in {}".format(
                expected_label,
                actual_labels,
            ))
            continue
        idx = actual_labels.index(expected_label)
        if idx <= prev_index:
            failures.append(
                "Expected build_flags label {} (index {}) to appear after {} (index {}) in {}".format(
                    expected_label,
                    idx,
                    prev_label,
                    prev_index,
                    actual_labels,
                ),
            )
        prev_index = idx
        prev_label = expected_label

    for unexpected_target in ctx.attr.unexpected_labels:
        unexpected_label = unexpected_target[BuildFlagsInfo].label
        if unexpected_label in actual_labels:
            failures.append("Unexpected build_flags label {} found in {}".format(
                unexpected_label,
                actual_labels,
            ))

    if failures:
        fail("Final build_flags mismatch on {}:\n  {}".format(
            ctx.attr.target_under_test.label,
            "\n  ".join(failures),
        ))

    output = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        output = output,
        content = "#!/bin/sh\nexit 0\n",
        is_executable = True,
    )
    return [
        DefaultInfo(
            files = depset([output]),
            executable = output,
            runfiles = ctx.runfiles(files = [output]),
        ),
    ]

_verify_build_flags_labels_bin = rule(
    doc = "Asserts ordered presence and absence of build_flags() labels on a target.",
    implementation = _verify_build_flags_labels_bin_impl,
    executable = True,
    attrs = {
        "expected_ordered_labels": attr.label_list(
            doc = "build_flags() targets that must be applied to the target, in this relative order.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
        "target_under_test": attr.label(
            doc = "The target under test, wrapped with build_flags() support.",
            mandatory = True,
            aspects = [collect_build_flags_labels_aspect],
        ),
        "unexpected_labels": attr.label_list(
            doc = "build_flags() targets that must not be applied to the target.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
    },
)

def verify_build_flags_labels_test(
        name,
        target_under_test,
        expected_ordered_labels = [],
        unexpected_labels = []):
    """Defines a host test asserting the build_flags() applied to `target_under_test`.

    Args:
        name: (string) Name of the host_test() target.
        target_under_test: (label) Label of a target wrapped with build_flags() support (e.g. `fx_cc_test_binary`).
        expected_ordered_labels: (list[label]) `build_flags` labels expected in order.
        unexpected_labels: (list[label]) `build_flags` labels expected to be absent.
    """
    bin_name = name + "_bin"
    _verify_build_flags_labels_bin(
        name = bin_name,
        testonly = True,
        target_under_test = target_under_test,
        expected_ordered_labels = expected_ordered_labels,
        unexpected_labels = unexpected_labels,
    )
    host_test(
        name = name,
        binary = ":" + bin_name,
    )

def _custom_linker_input_cc_lib_impl(ctx):
    cc_toolchain = find_cc_toolchain(ctx)
    feature_configuration = cc_common.configure_features(
        ctx = ctx,
        cc_toolchain = cc_toolchain,
        requested_features = ctx.features + ["pic"],
        unsupported_features = ctx.disabled_features,
    )
    expanded_flags = list(ctx.attr.user_link_flags)
    if ctx.files.additional_inputs:
        first_input_path = ctx.files.additional_inputs[0].path
        expanded_flags = [
            flag.replace("{additional_input}", first_input_path)
            for flag in expanded_flags
        ]

    mode = ctx.attr.mode
    if mode == "user_link_flags_and_inputs":
        compilation_context, compilation_outputs = cc_common.compile(
            name = ctx.label.name,
            actions = ctx.actions,
            feature_configuration = feature_configuration,
            cc_toolchain = cc_toolchain,
            srcs = ctx.files.srcs,
            local_defines = ctx.attr.local_defines,
            disallow_nopic_outputs = True,
        )
        linking_context, _ = cc_common.create_linking_context_from_compilation_outputs(
            actions = ctx.actions,
            name = ctx.label.name,
            feature_configuration = feature_configuration,
            cc_toolchain = cc_toolchain,
            compilation_outputs = compilation_outputs,
            disallow_dynamic_library = True,
            user_link_flags = expanded_flags,
            additional_inputs = ctx.files.additional_inputs,
        )
    elif mode == "non_pic_static_only":
        compilation_context = cc_common.create_compilation_context()
        dummy_a = ctx.actions.declare_file("lib{}.a".format(ctx.label.name))
        ctx.actions.write(output = dummy_a, content = "")
        lib_to_link = cc_common.create_library_to_link(
            actions = ctx.actions,
            feature_configuration = feature_configuration,
            cc_toolchain = cc_toolchain,
            static_library = dummy_a,
        )
        linker_input = cc_common.create_linker_input(
            owner = ctx.label,
            libraries = depset([lib_to_link]),
            user_link_flags = expanded_flags,
            additional_inputs = depset(ctx.files.additional_inputs),
        )
        linking_context = cc_common.create_linking_context(
            linker_inputs = depset([linker_input]),
        )
    else:
        fail("Unsupported mode {}".format(mode))

    return [
        CcInfo(
            compilation_context = compilation_context,
            linking_context = linking_context,
        ),
    ]

custom_linker_input_cc_lib = rule(
    doc = "Test-only CcInfo producer for user_link_flags/additional_inputs and non-PIC static LibraryToLink shapes.",
    implementation = _custom_linker_input_cc_lib_impl,
    attrs = {
        "additional_inputs": attr.label_list(
            doc = "Files added to the linker input's additional_inputs.",
            allow_files = True,
            default = [],
        ),
        "local_defines": attr.string_list(
            doc = "Preprocessor defines for srcs (user_link_flags_and_inputs mode only).",
            default = [],
        ),
        "mode": attr.string(
            doc = "Which LibraryToLink shape to produce.",
            mandatory = True,
            values = ["user_link_flags_and_inputs", "non_pic_static_only"],
        ),
        "srcs": attr.label_list(
            doc = "Sources compiled as PIC (user_link_flags_and_inputs mode only).",
            allow_files = True,
            default = [],
        ),
        "user_link_flags": attr.string_list(
            doc = "Linker flags for the linker input; {additional_input} expands to the path of the first additional_inputs file.",
            default = [],
        ),
    } | CC_TOOLCHAIN_ATTRS,
    fragments = ["cpp"],
    provides = [CcInfo],
    toolchains = use_cc_toolchain(),
)

_ShlibExtraLinkFlagsInfo = provider(
    doc = "Captures BuildFlagsListInfo.extra_cpp_link_flags from a cc_shared_library target.",
    fields = ["flags"],
)

def _shlib_extra_link_flags_aspect_impl(target, ctx):
    for dep in getattr(ctx.rule.attr, "additional_linker_inputs", []):
        if BuildFlagsListInfo in dep:
            return [
                _ShlibExtraLinkFlagsInfo(
                    flags = list(dep[BuildFlagsListInfo].extra_cpp_link_flags),
                ),
            ]
    fail("Expected an additional_linker_inputs target providing BuildFlagsListInfo on {}".format(target.label))

_shlib_extra_link_flags_aspect = aspect(
    implementation = _shlib_extra_link_flags_aspect_impl,
    provides = [_ShlibExtraLinkFlagsInfo],
)

def _verify_shlib_extra_link_flags_bin_impl(ctx):
    actual_flags = ctx.attr.target_under_test[_ShlibExtraLinkFlagsInfo].flags
    failures = []

    if ctx.attr.expect_empty and actual_flags:
        failures.append(
            "Expected empty extra_cpp_link_flags, got {}".format(actual_flags),
        )

    prev_idx = -1
    prev_pattern = None
    for pattern in ctx.attr.expected_ordered_substrings:
        matched_idx = -1
        for i in range(prev_idx + 1, len(actual_flags)):
            if pattern in actual_flags[i]:
                matched_idx = i
                break
        if matched_idx < 0:
            failures.append(
                "Expected '{}' after '{}' (index {}) in extra_cpp_link_flags {}".format(
                    pattern,
                    prev_pattern,
                    prev_idx,
                    actual_flags,
                ),
            )
        else:
            prev_idx = matched_idx
            prev_pattern = pattern

    for unexpected in ctx.attr.unexpected_substrings:
        for flag in actual_flags:
            if unexpected in flag:
                failures.append(
                    "Did not expect '{}' in extra_cpp_link_flags element '{}' (full list: {})".format(
                        unexpected,
                        flag,
                        actual_flags,
                    ),
                )

    if failures:
        fail("extra_cpp_link_flags mismatch on {}:\n  {}".format(
            ctx.attr.target_under_test.label,
            "\n  ".join(failures),
        ))

    output = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        output = output,
        content = "#!/bin/sh\nexit 0\n",
        is_executable = True,
    )
    return [
        DefaultInfo(
            files = depset([output]),
            executable = output,
            runfiles = ctx.runfiles(files = [output]),
        ),
    ]

_verify_shlib_extra_link_flags_bin = rule(
    doc = "Asserts contents and ordering of BuildFlagsListInfo.extra_cpp_link_flags on a cc_shared_library target.",
    implementation = _verify_shlib_extra_link_flags_bin_impl,
    executable = True,
    attrs = {
        "expect_empty": attr.bool(
            doc = "Whether extra_cpp_link_flags must be empty.",
            default = False,
        ),
        "expected_ordered_substrings": attr.string_list(
            doc = "Substrings that must appear in order across extra_cpp_link_flags.",
            default = [],
        ),
        "target_under_test": attr.label(
            doc = "The cc_shared_library target to inspect.",
            mandatory = True,
            aspects = [_shlib_extra_link_flags_aspect],
        ),
        "unexpected_substrings": attr.string_list(
            doc = "Substrings that must not appear in any extra_cpp_link_flags element.",
            default = [],
        ),
    },
)

def verify_shlib_extra_link_flags_test(
        name,
        target_under_test,
        *,
        expect_empty = False,
        expected_ordered_substrings = [],
        unexpected_substrings = []):
    """Defines a host test asserting extra_cpp_link_flags on `target_under_test`."""
    bin_name = name + "_bin"
    _verify_shlib_extra_link_flags_bin(
        name = bin_name,
        testonly = True,
        target_under_test = target_under_test,
        expect_empty = expect_empty,
        expected_ordered_substrings = expected_ordered_substrings,
        unexpected_substrings = unexpected_substrings,
    )
    host_test(
        name = name,
        binary = ":" + bin_name,
    )
