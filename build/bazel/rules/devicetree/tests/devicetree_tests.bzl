# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis and build verification rules for devicetree Bazel rules."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@fuchsia_rules_common//devicetree:providers.bzl", "FuchsiaDeviceTreeSegmentInfo")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")
load(
    "//build/bazel/debug_symbols:aspects.bzl",
    "DebugSymbolManifestInfo",
    "collect_debug_symbols_manifest_aspect",
)

visibility("private")

def _dtb_actions_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    files = target[DefaultInfo].files.to_list()
    asserts.equals(env, 1, len(files))
    asserts.equals(env, ctx.attr.expected_output_short_path, files[0].short_path)

    actions = analysistest.target_actions(env)
    preprocess_actions = [a for a in actions if a.mnemonic == "DtsPreprocess"]
    compile_actions = [a for a in actions if a.mnemonic == "DtbCompile"]

    asserts.equals(env, 1 if ctx.attr.expect_preprocess else 0, len(preprocess_actions))
    asserts.equals(env, 1, len(compile_actions))

    if ctx.attr.expect_preprocess:
        pp_argv = preprocess_actions[0].argv
        asserts.true(env, "-undef" in pp_argv, "Expected -undef in DtsPreprocess argv: %s" % pp_argv)
        asserts.true(env, "-P" in pp_argv, "Expected -P in DtsPreprocess argv: %s" % pp_argv)
        for flag in ctx.attr.expected_preprocess_flags:
            tokens = flag.split(" ")
            found = any([
                pp_argv[i:i + len(tokens)] == tokens
                for i in range(len(pp_argv) - len(tokens) + 1)
            ])
            asserts.true(
                env,
                found,
                "Expected flag %r in DtsPreprocess argv: %s" % (flag, pp_argv),
            )

    compile_argv = compile_actions[0].argv
    asserts.equals(env, "compile", compile_argv[1])
    for flag in ctx.attr.expected_dtcflags:
        asserts.true(
            env,
            flag in compile_argv,
            "Expected dtcflag %r in DtbCompile argv: %s" % (flag, compile_argv),
        )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

dtb_actions_test = analysistest.make(
    _dtb_actions_test_impl,
    doc = "Verifies actions and outputs of a `dtb()` target.",
    attrs = {
        "expected_output_short_path": attr.string(
            doc = "Expected short_path of the compiled `.dtb` output.",
            mandatory = True,
        ),
        "expect_preprocess": attr.bool(
            doc = "Whether a `DtsPreprocess` action is expected.",
            default = False,
        ),
        "expected_preprocess_flags": attr.string_list(
            doc = "Flags expected to appear in `DtsPreprocess` argv.",
            default = [],
        ),
        "expected_dtcflags": attr.string_list(
            doc = "Flags expected to appear in `DtbCompile` argv.",
            default = [],
        ),
    },
)

def _visitor_link_action_test_impl(ctx):
    env = analysistest.begin(ctx)

    link_actions = [
        a
        for a in analysistest.target_actions(env)
        if a.mnemonic == "CppLink"
    ]
    asserts.equals(env, 1, len(link_actions))
    argv = link_actions[0].argv
    asserts.true(
        env,
        any(["--version-script," in arg and arg.endswith("visitor.ld") for arg in argv]),
        "Expected --version-script,...visitor.ld in CppLink argv: %s" % argv,
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

visitor_link_action_test = analysistest.make(
    _visitor_link_action_test_impl,
    doc = "Verifies that a devicetree visitor's `.so` target links with `visitor.ld`.",
)

def _verify_devicetree_package_impl(ctx):
    pkg_info = ctx.attr.package[FuchsiaPackageInfo]
    actual_dests = sorted([r.dest for r in pkg_info.package_resources])
    for expected_dest in ctx.attr.expected_dests:
        if expected_dest not in actual_dests:
            fail("Expected %r in package_resources of %s, got %s" % (
                expected_dest,
                ctx.attr.package.label,
                actual_dests,
            ))

    debug_info = ctx.attr.package[DebugSymbolManifestInfo]
    if not debug_info.unstripped_binaries.to_list():
        fail("Expected unstripped binaries in DebugSymbolManifestInfo for %s" % ctx.attr.package.label)

    stamp = ctx.actions.declare_file(ctx.label.name + ".stamp")
    ctx.actions.write(output = stamp, content = "ok\n")
    return [DefaultInfo(files = depset([stamp]))]

verify_devicetree_package = rule(
    doc = "Verifies that `fx_package` contains the expected devicetree visitor `.so` and `.dtb` resources.",
    implementation = _verify_devicetree_package_impl,
    attrs = {
        "package": attr.label(
            doc = "The `fx_package()` target to inspect.",
            providers = [FuchsiaPackageInfo],
            aspects = [collect_debug_symbols_manifest_aspect],
            mandatory = True,
        ),
        "expected_dests": attr.string_list(
            doc = "Resource destinations expected in `FuchsiaPackageInfo.package_resources`.",
            mandatory = True,
        ),
    },
)

def _stub_devicetree_fragment_impl(ctx):
    return [
        DefaultInfo(files = depset([ctx.file.src])),
        FuchsiaDeviceTreeSegmentInfo(
            includes = depset([ctx.file.src.dirname or "."]),
            files = depset([ctx.file.src]),
        ),
    ]

stub_devicetree_fragment = rule(
    doc = "Minimal fragment rule providing `FuchsiaDeviceTreeSegmentInfo` for testing `dtb(deps = ...)`.",
    implementation = _stub_devicetree_fragment_impl,
    attrs = {
        "src": attr.label(
            doc = "The `.dtsi` fragment file.",
            allow_single_file = [".dtsi"],
            mandatory = True,
        ),
    },
)
