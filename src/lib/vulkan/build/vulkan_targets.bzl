# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel macros for building Vulkan C/C++ executables and test binaries.

Counterparts to `vulkan_executable()` and `vulkan_test()` in
`//src/lib/vulkan/build/vulkan_targets.gni`.
"""

load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/cc:fx_cc_test_binary.bzl", "fx_cc_test_binary")

visibility(["//src/..."])

def _dedupe_vulkan_dep(deps):
    vulkan_label = Label("//src/lib/vulkan")
    if deps == None:
        return [vulkan_label]
    if type(deps) == "list":
        seen = {native.package_relative_label(d): True for d in deps}
        if vulkan_label in seen:
            return deps
        return deps + [vulkan_label]
    return deps + [vulkan_label]

def _apply_gn_compat_flags(copts, linkopts, include_dirs, libs, configs):
    extra_copts = []
    if include_dirs:
        for d in include_dirs:
            extra_copts.append("-I" + d.removeprefix("//"))
    if configs:
        for c in configs:
            if c == "//build/config:Wno-conversion":
                extra_copts.append("-Wno-conversion")
            else:
                fail("vulkan_executable()/vulkan_test() do not support config " + c)
    if extra_copts:
        copts = (copts or []) + extra_copts

    if libs:
        extra_linkopts = ["-l" + lib for lib in libs]
        linkopts = (linkopts or []) + extra_linkopts

    return copts, linkopts

def vulkan_executable(
        *,
        name,
        deps = None,
        copts = None,
        linkopts = None,
        include_dirs = None,
        libs = None,
        configs = None,
        **kwargs):
    """Defines a C/C++ executable with a Vulkan compile-time and run-time dependency.

    Mirrors `vulkan_executable()` in `//src/lib/vulkan/build/vulkan_targets.gni`
    and syncs to it via `bazel2gn`.
    """
    copts, linkopts = _apply_gn_compat_flags(copts, linkopts, include_dirs, libs, configs)
    fx_cc_binary(
        name = name,
        deps = _dedupe_vulkan_dep(deps),
        copts = copts,
        linkopts = linkopts,
        **kwargs
    )

def vulkan_test(
        *,
        name,
        deps = None,
        copts = None,
        linkopts = None,
        include_dirs = None,
        libs = None,
        configs = None,
        **kwargs):
    """Defines a C/C++ test binary with a Vulkan compile-time and run-time dependency.

    Mirrors `vulkan_test()` in `//src/lib/vulkan/build/vulkan_targets.gni`
    and syncs to it via `bazel2gn`.
    """
    copts, linkopts = _apply_gn_compat_flags(copts, linkopts, include_dirs, libs, configs)
    fx_cc_test_binary(
        name = name,
        deps = _dedupe_vulkan_dep(deps),
        copts = copts,
        linkopts = linkopts,
        **kwargs
    )
