# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for rustc_dylib link-boundary provider filtering."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_cc//cc:cc_import.bzl", "cc_import")
load("@rules_cc//cc:cc_library.bzl", "cc_library")
load("@rules_cc//cc:cc_shared_library.bzl", "cc_shared_library")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust:defs.bzl", "rust_clippy_aspect")
load("@rules_rust//rust:rust_common.bzl", "rust_common")
load("//build/bazel/rules/rust:rustc_binary.bzl", "rustc_binary")
load("//build/bazel/rules/rust:rustc_dylib.bzl", "rustc_dylib")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

visibility("private")

def _collect_libs(linker_inputs):
    static_libs = []
    dynamic_libs = []
    for li in linker_inputs:
        for lib in li.libraries:
            if lib.static_library:
                static_libs.append(lib.static_library.basename)
            if lib.pic_static_library:
                static_libs.append(lib.pic_static_library.basename)
            if lib.dynamic_library:
                dynamic_libs.append(lib.dynamic_library.basename)
    return static_libs, dynamic_libs

def _dylib_boundary_providers_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    asserts.true(
        env,
        rust_common.crate_group_info in target,
        "Expected CrateGroupInfo on rustc_dylib boundary target.",
    )
    asserts.true(
        env,
        CcInfo in target,
        "Expected CcInfo on rustc_dylib boundary target.",
    )

    cc_static, cc_dynamic = _collect_libs(
        target[CcInfo].linking_context.linker_inputs.to_list(),
    )
    asserts.equals(
        env,
        [],
        cc_static,
        "Expected no static archives in rustc_dylib CcInfo, got {}.".format(cc_static),
    )
    asserts.true(
        env,
        any(["dylib_shared_cc_dep" in name for name in cc_dynamic]),
        "Expected shared library dylib_shared_cc_dep in CcInfo dynamic libs, got {}.".format(cc_dynamic),
    )

    dep_variants = target[rust_common.crate_group_info].dep_variant_infos.to_list()
    asserts.equals(env, 1, len(dep_variants), "Expected 1 DepVariantInfo.")
    variant = dep_variants[0]
    asserts.equals(env, "dylib", variant.crate_info.type, "Expected dylib CrateInfo.")

    noncrate_static, noncrate_dynamic = _collect_libs(
        variant.dep_info.transitive_noncrates.to_list(),
    )
    asserts.equals(
        env,
        [],
        noncrate_static,
        "Expected no static archives in DepInfo.transitive_noncrates, got {}.".format(noncrate_static),
    )
    asserts.true(
        env,
        any(["dylib_shared_cc_dep" in name for name in noncrate_dynamic]),
        "Expected shared library dylib_shared_cc_dep in DepInfo.transitive_noncrates, got {}.".format(noncrate_dynamic),
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_dylib_boundary_providers_test = analysistest.make(
    _dylib_boundary_providers_test_impl,
)

def _dylib_dependent_binary_link_args_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    rustc_actions = [a for a in target.actions if a.mnemonic == "Rustc"]
    asserts.equals(env, 1, len(rustc_actions), "Expected 1 Rustc action.")

    argv = rustc_actions[0].argv
    asserts.false(
        env,
        any(["dylib_static_cc_dep" in arg for arg in argv]),
        "Expected static C++ dep of rustc_dylib not to be relinked into dependent binary.",
    )
    asserts.false(
        env,
        any([arg.startswith("-lstatic=std-") for arg in argv]),
        "Expected static libstd not to be relinked into dependent binary.",
    )
    asserts.true(
        env,
        any(["boundary_dylib" in arg and arg.endswith(".so") for arg in argv]),
        "Expected dependent binary to link against boundary_dylib .so.",
    )
    asserts.true(
        env,
        any(["dylib_shared_cc_dep" in arg for arg in argv]),
        "Expected shared C++ dep of rustc_dylib to be propagated to dependent binary.",
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_dylib_dependent_binary_link_args_test = analysistest.make(
    _dylib_dependent_binary_link_args_test_impl,
)

def _dylib_boundary_clippy_forwarding_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    has_clippy_checks = OutputGroupInfo in target and hasattr(target[OutputGroupInfo], "clippy_checks")
    asserts.equals(
        env,
        ctx.attr.expect_clippy,
        has_clippy_checks,
        "Expected clippy_checks={} on rustc_dylib boundary target.".format(ctx.attr.expect_clippy),
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_dylib_boundary_clippy_forwarding_test = analysistest.make(
    _dylib_boundary_clippy_forwarding_test_impl,
    extra_target_under_test_aspects = [rust_clippy_aspect],
    attrs = {
        "expect_clippy": attr.bool(mandatory = True),
    },
)

def dylib_link_boundary_test_suite(name):
    """Defines fixtures and analysis tests for rustc_dylib link-boundary behavior.

    Args:
      name: Name of the generated test_suite target.
    """
    cc_library(
        name = "dylib_static_cc_dep",
        srcs = ["dep.cc"],
    )

    cc_library(
        name = "dylib_shared_cc_impl",
        srcs = ["dep.cc"],
    )

    cc_shared_library(
        name = "dylib_shared_cc_so",
        shared_lib_name = "libdylib_shared_cc_dep.so",
        deps = [":dylib_shared_cc_impl"],
    )

    cc_import(
        name = "dylib_shared_cc_dep",
        shared_library = ":dylib_shared_cc_so",
    )

    rustc_library(
        name = "dylib_rlib_dep",
        srcs = ["lib.rs"],
        edition = "2024",
    )

    rustc_dylib(
        name = "boundary_dylib",
        srcs = ["lib.rs"],
        edition = "2024",
        link_deps = [":dylib_static_cc_dep"],
        public_deps = [":dylib_shared_cc_dep"],
        rustc_flags = ["-Aunused-crate-dependencies"],
        deps = [":dylib_rlib_dep"],
    )

    rustc_binary(
        name = "bin_depending_on_boundary_dylib",
        srcs = ["main.rs"],
        edition = "2024",
        rustc_flags = ["-Aunused-crate-dependencies"],
        deps = [":boundary_dylib"],
    )

    _dylib_boundary_providers_test(
        name = "dylib_boundary_providers_test",
        target_under_test = ":boundary_dylib",
    )

    _dylib_dependent_binary_link_args_test(
        name = "dylib_dependent_binary_link_args_test",
        target_under_test = ":bin_depending_on_boundary_dylib",
    )

    _dylib_boundary_clippy_forwarding_test(
        name = "dylib_boundary_clippy_enabled_test",
        expect_clippy = True,
        target_under_test = ":dylib_default",
    )

    _dylib_boundary_clippy_forwarding_test(
        name = "dylib_boundary_clippy_disabled_test",
        expect_clippy = False,
        target_under_test = ":dylib_disabled",
    )

    native.test_suite(
        name = name,
        visibility = ["//visibility:private"],
        tests = [
            ":dylib_boundary_clippy_disabled_test",
            ":dylib_boundary_clippy_enabled_test",
            ":dylib_boundary_providers_test",
            ":dylib_dependent_binary_link_args_test",
        ],
    )
