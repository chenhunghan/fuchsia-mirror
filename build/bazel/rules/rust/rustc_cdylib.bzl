# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust cdylib shared library."""

load(
    "@fuchsia_rules_common//build_flags:rust.bzl",
    "BUILD_FLAGS_RUST_MACRO_ATTRS",
    "wrap_rust_macro_args_with_build_flags",
)
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust:defs.bzl", "rust_common", "rust_shared_library")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load(
    "//build/bazel/rules/rust:common.bzl",
    "with_fuchsia_rust_link_exec_properties",
    "with_fuchsia_rust_stdlib_link_deps",
    "with_fuchsia_rust_tags",
    "with_fuchsia_rustc_flags",
)

visibility(["//..."])

def _cdylib_with_hdrs_impl(ctx):
    cdylib = ctx.attr.shared_library
    providers = [
        cdylib[DefaultInfo],
        # Pass `headers` via `direct_cc_infos` so its public headers are
        # re-exported as direct headers on the merged `CcInfo`, and `cdylib`
        # via `cc_infos` to merge its linking context.
        cc_common.merge_cc_infos(
            direct_cc_infos = [ctx.attr.headers[CcInfo]],
            cc_infos = [cdylib[CcInfo]],
        ),
        cdylib[rust_common.test_crate_info],
        cdylib[rust_common.dep_info],
    ]
    if OutputGroupInfo in cdylib:
        providers.append(cdylib[OutputGroupInfo])
    return providers

_cdylib_with_hdrs = rule(
    doc = "Combines a `rust_shared_library` (`cdylib`) with public C/C++ headers.",
    implementation = _cdylib_with_hdrs_impl,
    attrs = {
        "shared_library": attr.label(
            mandatory = True,
            providers = [
                DefaultInfo,
                CcInfo,
                rust_common.test_crate_info,
                rust_common.dep_info,
            ],
        ),
        "headers": attr.label(
            mandatory = True,
            providers = [CcInfo],
        ),
        "implementation_deps": attr.label_list(
            doc = "Aspect traversal edges for `assert_no_deps` and `expect_includes`.",
            default = [],
        ),
    },
    provides = [CcInfo, rust_common.test_crate_info, rust_common.dep_info],
)

def _rustc_cdylib_impl(
        name,
        lint_config,
        disable_clippy,
        rustc_flags,
        build_flags,
        output_name,
        public_deps,
        hdrs,
        validations,
        visibility,
        **kwargs):
    # `@rules_rust` names `cdylib` outputs `lib<crate_name>.so` (without a hash).
    # Component manifests (`program.binary` in `.cml`), `dlopen()` callers, and
    # `bazel2gn` (which unwraps `fx_packaged_binary` to the `rustc_cdylib` target
    # in GN `fuchsia_component` deps) are shared between Bazel and GN and all
    # expect the same `lib<output_name>.so` filename in both build systems.
    if output_name:
        if "-" in output_name:
            fail("`output_name` in rustc_cdylib(%r) cannot contain '-' (got %r); @rules_rust names cdylib outputs `lib<crate_name>.so` and Rust crate names cannot contain '-'." % (name, output_name))
        if kwargs.get("crate_name") and kwargs["crate_name"] != output_name:
            fail("`output_name` (%r) and `crate_name` (%r) cannot differ in rustc_cdylib(%r) because @rules_rust always names cdylib outputs `lib<crate_name>.so`." % (output_name, kwargs["crate_name"], name))
        kwargs["crate_name"] = output_name
    elif "-" in name and not kwargs.get("crate_name"):
        fail("rustc_cdylib(%r) has '-' in `name`; set `output_name` (or `crate_name`) without '-' so Bazel and GN produce the same `lib<name>.so` filename." % name)
    elif hdrs and not kwargs.get("crate_name"):
        # When `hdrs` is set, the inner `rust_shared_library` target is named
        # `<name>_rustc`, so preserve the public `<name>` crate/library name.
        kwargs["crate_name"] = name.replace("-", "_")

    # rules_rust propagates link dependencies transitively, so `public_deps`
    # are added to `link_deps` on the cdylib (and as `deps` of the generated
    # headers library when `hdrs` is set).
    if public_deps:
        kwargs["link_deps"] = (kwargs.get("link_deps") or []) + public_deps

    if validations:
        compile_data = kwargs.get("compile_data")
        kwargs["compile_data"] = (compile_data if compile_data != None else []) + validations

    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_production"

    kwargs["tags"] = with_fuchsia_rust_tags(kwargs.get("tags"), disable_clippy)
    kwargs["rustc_flags"] = with_fuchsia_rustc_flags(rustc_flags)

    rust_target_name = (name + "_rustc") if hdrs else name

    library_kwargs = wrap_rust_macro_args_with_build_flags(
        kwargs = kwargs,
        name = rust_target_name,
        rust_rule_name = "rust_shared_library",
        build_flags = build_flags,
        target_type = "rust_shared_library",
    )

    rust_shared_library(
        name = rust_target_name,
        lint_config = lint_config,
        visibility = ["//visibility:private"] if hdrs else visibility,
        **(library_kwargs | {
            "exec_properties": with_fuchsia_rust_link_exec_properties(library_kwargs.get("exec_properties")),
            "link_deps": with_fuchsia_rust_stdlib_link_deps(library_kwargs.get("link_deps")),
        })
    )

    if hdrs:
        headers_target_name = name + "_cdylib_headers"
        fx_cc_library(
            name = headers_target_name,
            hdrs = hdrs,
            deps = public_deps,
            tags = ["manual"],
            target_compatible_with = kwargs.get("target_compatible_with"),
            testonly = kwargs.get("testonly"),
            visibility = ["//visibility:private"],
        )
        _cdylib_with_hdrs(
            name = name,
            aspect_hints = kwargs.get("aspect_hints"),
            shared_library = ":" + rust_target_name,
            headers = ":" + headers_target_name,
            implementation_deps = [
                ":" + rust_target_name,
                ":" + headers_target_name,
            ],
            tags = kwargs.get("tags"),
            target_compatible_with = kwargs.get("target_compatible_with"),
            testonly = kwargs.get("testonly"),
            visibility = visibility,
        )

rustc_cdylib = macro(
    doc = """`rust_shared_library` (`cdylib`) wrapper with Fuchsia-specific features.

Builds a C ABI Rust `cdylib` shared library (`lib<output_name>.so`), providing
`CcInfo` for native dependents and `TestCrateInfo` for unit tests (`rustc_test`
with `crate = ":<name>"`).

To include a `rustc_cdylib` target in a Fuchsia component or package, wrap it
with `fx_packaged_binary(binary = ":<name>", binary_name = "lib<output_name>.so", install_root = "lib/")`.

Also used to allow syncing between Bazel and GN targets via `bazel2gn`, where
this maps to the GN `rustc_cdylib()` template in `//build/rust/rustc_cdylib.gni`.
""",
    implementation = _rustc_cdylib_impl,
    inherit_attrs = rust_shared_library,
    attrs = {
        "disable_clippy": attr.bool(
            doc = "If true, disables clippy lints on this target.",
            default = False,
            configurable = False,
        ),
        "public_deps": attr.label_list(
            doc = """Native (e.g. C/C++) dependencies that are also exposed to dependents in GN.

In Bazel, these are added to `link_deps` (and to `deps` of the generated headers
library when `hdrs` is set). In the GN `rustc_cdylib()` template generated by
`bazel2gn`, these are forwarded as GN `public_deps`.""",
            default = [],
        ),
        "hdrs": attr.label_list(
            doc = "Public C/C++ header files exposed to native dependents (maps to `public` in GN `rustc_cdylib()`).",
            allow_files = True,
            default = [],
            configurable = False,
        ),
        "validations": attr.label_list(
            doc = "Targets whose validation actions run whenever this library is built (maps to `validations` in GN).",
            default = [],
            configurable = False,
        ),
        "output_name": attr.string(
            doc = """Base name of the shared library, i.e. `lib<output_name>.so`.

Defaults to `crate_name` if specified, or `name` otherwise.""",
            configurable = False,
        ),
    } | BUILD_FLAGS_RUST_MACRO_ATTRS,
)
