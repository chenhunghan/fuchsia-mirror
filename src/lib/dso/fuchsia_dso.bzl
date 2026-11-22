# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel macros for building Fuchsia C++ and Rust DSO components (`dso_runner`).

Counterparts to `fuchsia_cc_dso()` and `fuchsia_rust_dso()` in
`//src/lib/dso/fuchsia_dso.gni`.
"""

load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_cc//cc/common:cc_shared_library_info.bzl", "CcSharedLibraryInfo")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/cc:fx_cc_shared_library.bzl", "fx_cc_shared_library")
load("//build/bazel/rules/rust:rustc_cdylib.bzl", "rustc_cdylib")

visibility([
    "//build/...",
    "//examples/...",
    "//src/...",
])

def _cc_dso_impl(ctx):
    shlib = ctx.attr.shared_library
    cc_shlib_info = shlib[CcSharedLibraryInfo]
    providers = [
        shlib[DefaultInfo],
        cc_shlib_info,
        CcInfo(
            linking_context = cc_common.create_linking_context(
                linker_inputs = depset([cc_shlib_info.linker_input]),
            ),
        ),
    ]
    if OutputGroupInfo in shlib:
        providers.append(shlib[OutputGroupInfo])
    return providers

_cc_dso = rule(
    doc = "Exposes a `fuchsia_cc_dso` `fx_cc_shared_library` target with `CcInfo` so dependents such as `rustc_test(link_deps)` can link against it directly.",
    implementation = _cc_dso_impl,
    attrs = {
        "shared_library": attr.label(
            mandatory = True,
            providers = [DefaultInfo, CcSharedLibraryInfo],
        ),
    },
    provides = [CcInfo, CcSharedLibraryInfo],
)

def _dedupe_implicit_deps(deps, implicit_labels):
    """Appends `implicit_labels` to `deps`, avoiding duplicate labels when `deps` is a list."""
    if deps == None:
        return list(implicit_labels)
    if type(deps) == "list":
        seen = {native.package_relative_label(d): True for d in deps}
        extra = [lbl for lbl in implicit_labels if lbl not in seen]
        return deps + extra
    return deps + list(implicit_labels)

def fuchsia_cc_dso(
        *,
        name,
        is_async,
        output_name = None,
        srcs = None,
        deps = None,
        linkopts = None,
        aspect_hints = None,
        build_flags = None,
        disable_build_flags = None,
        disable_instrumentation = False,
        tags = None,
        target_compatible_with = None,
        testonly = None,
        visibility = None,
        **kwargs):
    """Defines a Fuchsia C++ DSO shared library (`lib<output_name>.so`) for `dso_runner`.

    Mirrors the GN `fuchsia_cc_dso()` template in `//src/lib/dso/fuchsia_dso.gni`.
    To package into a Fuchsia component, wrap this target with `fx_packaged_binary`:

    ```python
    fx_packaged_binary(
        name = "my_dso_packaged",
        binary = ":my_dso",
        binary_name = "libmy_dso.so",
        install_root = "lib/",
    )
    ```

    Args:
        name: Name of the target.
        is_async: Required boolean (`True` for `//src/lib/dso/cpp:async` /
            `_dso_start_async`, `False` for `//src/lib/dso/cpp:sync` / `_dso_start`).
            Named `is_async` because `async` is a reserved keyword in Starlark;
            `bazel2gn` translates `is_async` to GN's `async`.
        output_name: Optional base name of the output `.so` (`lib<output_name>.so`).
            Defaults to `name`.
        srcs: C/C++ source files (and private headers) compiled into the DSO.
        deps: Dependencies linked into the DSO.
        linkopts: Additional linker flags (maps to `ldflags` in GN).
        aspect_hints: Aspect hints attached to the DSO shared library target.
        build_flags: `build_flags()` targets applied to the DSO.
        disable_build_flags: `build_flags()` targets disabled on the DSO.
        disable_instrumentation: If true, disables instrumentation on the DSO.
        tags: Standard Bazel tags.
        target_compatible_with: Platform constraints (defaults to Fuchsia if unset).
        testonly: Standard Bazel meaning.
        visibility: Standard Bazel visibility.
        **kwargs: Additional `fx_cc_library` attributes (`copts`, `defines`, etc.).
    """
    if type(is_async) != "bool":
        fail("`is_async` in fuchsia_cc_dso(%r) must be a boolean (True or False), got %r" % (name, is_async))

    out_name = output_name or name
    implicit_dep = Label("//src/lib/dso/cpp:async") if is_async else Label("//src/lib/dso/cpp:sync")
    all_deps = _dedupe_implicit_deps(deps, [implicit_dep])

    if target_compatible_with == None:
        target_compatible_with = ["@platforms//os:fuchsia"]

    impl_name = name + "_impl"
    shlib_name = name + "_shlib"
    inner_tags = list(tags) if tags else []
    if "manual" not in inner_tags:
        inner_tags.append("manual")

    fx_cc_library(
        name = impl_name,
        srcs = srcs,
        deps = all_deps,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        tags = inner_tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = ["//visibility:private"],
        alwayslink = True,
        **kwargs
    )

    fx_cc_shared_library(
        name = shlib_name,
        aspect_hints = aspect_hints,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        disable_instrumentation = disable_instrumentation,
        shared_lib_name = "lib{}.so".format(out_name),
        tags = inner_tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        user_link_flags = linkopts,
        visibility = ["//visibility:private"],
        deps = [":" + impl_name],
    )

    _cc_dso(
        name = name,
        aspect_hints = aspect_hints,
        shared_library = ":" + shlib_name,
        tags = tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

def fuchsia_rust_dso(
        *,
        name,
        output_name = None,
        deps = None,
        rustc_flags = None,
        target_compatible_with = None,
        **kwargs):
    """Defines a Fuchsia Rust DSO shared library (`lib<output_name>.so`) for `dso_runner`.

    Mirrors the GN `fuchsia_rust_dso()` template in `//src/lib/dso/fuchsia_dso.gni`,
    adding the implicit `//src/lib/fuchsia`, `//src/lib/dso/rust`, and
    `//sdk/rust/zx` dependencies and delegating to `rustc_cdylib()`.

    Args:
        name: Name of the target.
        output_name: Optional base name of the output `.so` (`lib<output_name>.so`).
            Defaults to `crate_name` (if set) or `name`.
        deps: Rust dependencies; implicit DSO dependencies are appended
            automatically without duplicating existing labels.
        rustc_flags: Optional extra flags passed to `rustc_cdylib()`.
        target_compatible_with: Platform constraints (defaults to Fuchsia if unset).
        **kwargs: Additional `rustc_cdylib` attributes (`srcs`, `crate_root`,
            `edition`, etc.).
    """
    implicit_deps = [
        Label("//src/lib/fuchsia"),
        Label("//src/lib/dso/rust"),
        Label("//sdk/rust/zx"),
    ]
    all_deps = _dedupe_implicit_deps(deps, implicit_deps)

    if target_compatible_with == None:
        target_compatible_with = ["@platforms//os:fuchsia"]

    # Match GN's clang_toolchain `solink` behavior and `fx_cc_shared_library()`
    # by setting `DT_SONAME` to `lib<out_name>.so` so `dlopen_vmo_vmar()` in
    # `dso_runner` deduplicates pre-linked Rust DSOs (e.g. in `dso_runner_tests`)
    # instead of loading a second instance.
    out_name = output_name or kwargs.get("crate_name") or name.replace("-", "_")
    soname_flag = ["-Clink-arg=-Wl,-soname=lib{}.so".format(out_name)]
    all_rustc_flags = soname_flag + (rustc_flags or [])

    rustc_cdylib(
        name = name,
        output_name = output_name,
        deps = all_deps,
        rustc_flags = all_rustc_flags,
        target_compatible_with = target_compatible_with,
        **kwargs
    )
