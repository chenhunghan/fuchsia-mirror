# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel macros for building Fuchsia C++ and Rust DSO components (`dso_runner`).

Counterparts to `fuchsia_cc_dso()` and `fuchsia_rust_dso()` in
`//src/lib/dso/fuchsia_dso.gni`.
"""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/cc:fx_cc_shared_library.bzl", "fx_cc_shared_library")
load("//build/bazel/rules/rust:rustc_cdylib.bzl", "rustc_cdylib")

visibility([
    "//build/...",
    "//examples/...",
    "//src/...",
])

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
        name = name,
        aspect_hints = aspect_hints,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        disable_instrumentation = disable_instrumentation,
        shared_lib_name = "lib{}.so".format(out_name),
        tags = tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        user_link_flags = linkopts,
        visibility = visibility,
        deps = [":" + impl_name],
    )

def fuchsia_rust_dso(
        *,
        name,
        deps = None,
        target_compatible_with = None,
        **kwargs):
    """Defines a Fuchsia Rust DSO shared library (`lib<output_name>.so`) for `dso_runner`.

    Mirrors the GN `fuchsia_rust_dso()` template in `//src/lib/dso/fuchsia_dso.gni`,
    adding the implicit `//src/lib/fuchsia`, `//src/lib/dso/rust`, and
    `//sdk/rust/zx` dependencies and delegating to `rustc_cdylib()`.

    Args:
        name: Name of the target.
        deps: Rust dependencies; implicit DSO dependencies are appended
            automatically without duplicating existing labels.
        target_compatible_with: Platform constraints (defaults to Fuchsia if unset).
        **kwargs: Additional `rustc_cdylib` attributes (`srcs`, `crate_root`,
            `edition`, `output_name`, etc.).
    """
    implicit_deps = [
        Label("//src/lib/fuchsia"),
        Label("//src/lib/dso/rust"),
        Label("//sdk/rust/zx"),
    ]
    all_deps = _dedupe_implicit_deps(deps, implicit_deps)

    if target_compatible_with == None:
        target_compatible_with = ["@platforms//os:fuchsia"]

    rustc_cdylib(
        name = name,
        deps = all_deps,
        target_compatible_with = target_compatible_with,
        **kwargs
    )
