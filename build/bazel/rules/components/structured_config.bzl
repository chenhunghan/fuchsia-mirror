# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Structured configuration client libraries for in-tree components.

These are translations of the templates defined in
//build/components/fuchsia_structured_config.gni and
//tools/configc/build/config.gni. bazel2gn converts
fuchsia_structured_config_rust_lib() and
fuchsia_structured_config_cpp_elf_lib(); the *_config_client_lib_source rules
are Bazel-only.
"""

load("@fuchsia_rules_common//components:providers.bzl", "FuchsiaComponentManifestInfo")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

_CONFIGC = "//tools/configc"

_CONFIGC_ATTRS = {
    "compiled_manifest": attr.label(
        doc = "The fx_component_manifest() target whose `config` schema is used.",
        mandatory = True,
        providers = [FuchsiaComponentManifestInfo],
    ),
    "fidl_library_name": attr.string(
        doc = "Name of the internal FIDL library that holds the configuration.",
        mandatory = True,
    ),
    "_configc": attr.label(
        default = _CONFIGC,
        executable = True,
        cfg = "exec",
    ),
}

def _fidl_config_client_lib_source_impl(ctx):
    compiled_manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest
    fidl_format = ctx.executable._fidl_format
    output = ctx.actions.declare_file(ctx.label.name + ".fidl")
    ctx.actions.run(
        executable = ctx.executable._configc,
        arguments = [
            "fidl",
            "--cm",
            compiled_manifest.path,
            "--output",
            output.path,
            "--library-name",
            ctx.attr.fidl_library_name,
            "--fidl-format",
            fidl_format.path,
        ],
        inputs = [compiled_manifest],
        tools = [fidl_format],
        outputs = [output],
        mnemonic = "ConfigcFidl",
        progress_message = "Generating structured config FIDL library %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

fidl_config_client_lib_source = rule(
    doc = "Generates the FIDL source of a structured configuration client library.",
    implementation = _fidl_config_client_lib_source_impl,
    attrs = _CONFIGC_ATTRS | {
        "_fidl_format": attr.label(
            default = "//tools/fidl/fidlc:fidl-format",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _rust_config_client_lib_source_impl(ctx):
    compiled_manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest
    rust_toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    rustfmt = rust_toolchain.rustfmt
    rustfmt_config = ctx.file._rustfmt_config
    output = ctx.actions.declare_file(ctx.label.name + ".rs")

    # The rustfmt config must be passed explicitly so that the output does not
    # depend on where the action runs. See `_fidlgen_rust_impl()` in
    # //build/bazel/rules/fidl/fidl_rust_library.bzl.
    ctx.actions.run(
        executable = ctx.executable._configc,
        arguments = [
            "rust",
            "--cm",
            compiled_manifest.path,
            "--output",
            output.path,
            "--fidl-library-name",
            ctx.attr.fidl_library_name,
            "--rustfmt",
            rustfmt.path,
            "--rustfmt-config",
            rustfmt_config.path,
        ],
        inputs = [compiled_manifest, rustfmt, rustfmt_config],
        tools = rust_toolchain.all_files,
        outputs = [output],
        mnemonic = "ConfigcRust",
        progress_message = "Generating structured config Rust library %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

rust_config_client_lib_source = rule(
    doc = "Generates the Rust source of a structured configuration client library.",
    implementation = _rust_config_client_lib_source_impl,
    toolchains = ["@rules_rust//rust:toolchain_type"],
    attrs = _CONFIGC_ATTRS | {
        "_rustfmt_config": attr.label(
            default = "@//:rustfmt.toml",
            allow_single_file = True,
        ),
    },
)

def _fuchsia_structured_config_rust_lib_impl(
        name,
        cm_label,
        crate_name,
        testonly,
        visibility):
    # LINT.IfChange(rust_lib_names)
    rust_library_name = (crate_name or name).replace(".", "_").replace("-", "_")

    # The FIDL library name is only used internally by configc when generating
    # the FIDL and Rust sources. It is not visible to users.
    fidl_library_name = "cf.sc.internal." + rust_library_name.replace("_", "")

    fidl_source_target = name + "_fidl_config_lib_source"
    rust_source_target = name + "_rust_config_lib_source"
    fidl_library_target = name + "_fidl_internal"

    # LINT.ThenChange(//build/components/fuchsia_structured_config.gni:rust_lib_names)

    fidl_config_client_lib_source(
        name = fidl_source_target,
        compiled_manifest = cm_label,
        fidl_library_name = fidl_library_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    rust_config_client_lib_source(
        name = rust_source_target,
        compiled_manifest = cm_label,
        fidl_library_name = fidl_library_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fidl_library(
        name = fidl_library_target,
        srcs = [":" + fidl_source_target],
        library_name = fidl_library_name,
        excluded_checks = [
            "invalid-copyright-for-platform-source-library",
            "wrong-prefix-for-platform-source-library",
        ],
        # Only the Rust bindings are used.
        enable_cpp = False,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    rustc_library(
        name = name,
        crate_name = rust_library_name,
        srcs = [":" + rust_source_target],
        edition = "2024",
        build_flags = ["//build/config/rust/lints:deny_unused_results"],
        deps = [
            ":" + fidl_library_target + "_rust",
            "//sdk/rust/zx",
            "//src/lib/diagnostics/inspect/rust:fuchsia-inspect",
            "//src/lib/fidl/rust/fidl",
            "//src/lib/fuchsia-component/config",
        ],
        testonly = testonly,
        visibility = visibility,
    )

fuchsia_structured_config_rust_lib = macro(
    doc = """Defines a Rust structured configuration client library for a component.

    The library is generated from the `config` schema of a compiled component
    manifest, and exposes the configuration of the component, e.g.:

    ```
    fx_component_manifest(
        name = "manifest",
        component_name = "my_component",
        manifest = "meta/my_component.cml",
    )

    fuchsia_structured_config_rust_lib(
        name = "my_component_config",
        cm_label = ":manifest",
    )

    rustc_binary(
        name = "bin",
        ...
        deps = [":my_component_config", ...],
    )
    ```

    And in Rust:

    ```
    use my_component_config::Config;

    let config = Config::take_from_startup_handle();
    ```
    """,
    implementation = _fuchsia_structured_config_rust_lib_impl,
    attrs = {
        "cm_label": attr.label(
            doc = "The fx_component_manifest() target of the component.",
            mandatory = True,
            configurable = False,
            providers = [FuchsiaComponentManifestInfo],
        ),
        "crate_name": attr.string(
            doc = "Name of the generated Rust crate. Defaults to `name`. `.` and `-` are replaced by `_`.",
            configurable = False,
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)

def _cpp_config_client_lib_source_impl(ctx):
    compiled_manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest
    clang_format = ctx.executable._clang_format
    namespace = (ctx.attr.namespace or ctx.label.name).replace(".", "_").replace("-", "_").lower()
    prefix = ctx.label.name + ("/" + ctx.label.package if ctx.label.package else "")
    h_output = ctx.actions.declare_file(prefix + "/" + namespace + ".h")
    cc_output = ctx.actions.declare_file(prefix + "/" + namespace + ".cc")
    ctx.actions.run(
        executable = ctx.executable._configc,
        arguments = [
            "cpp",
            "--cm",
            compiled_manifest.path,
            "--h-output",
            h_output.path,
            "--cc-output",
            cc_output.path,
            "--namespace",
            namespace,
            "--fidl-library-name",
            ctx.attr.fidl_library_name,
            "--clang-format",
            clang_format.path,
        ],
        inputs = [compiled_manifest],
        tools = [clang_format],
        outputs = [h_output, cc_output],
        mnemonic = "ConfigcCpp",
        progress_message = "Generating structured config C++ library %{label}",
    )
    return [
        DefaultInfo(files = depset([h_output, cc_output])),
        OutputGroupInfo(
            h = depset([h_output]),
            cc = depset([cc_output]),
        ),
    ]

cpp_config_client_lib_source = rule(
    doc = "Generates the C++ header and source of a structured configuration client library.",
    implementation = _cpp_config_client_lib_source_impl,
    attrs = _CONFIGC_ATTRS | {
        "namespace": attr.string(
            doc = "Namespace used by the C++ library. Defaults to `name`. `.` and `-` are replaced by `_` and characters are lowercased.",
        ),
        "_clang_format": attr.label(
            default = "@fuchsia_clang//:bin/clang-format",
            executable = True,
            allow_single_file = True,
            cfg = "exec",
        ),
    },
)

def _fuchsia_structured_config_cpp_elf_lib_impl(
        name,
        cm_label,
        namespace,
        fidl_library_name,
        testonly,
        visibility):
    cpp_namespace = (namespace or name).replace(".", "_").replace("-", "_").lower()
    internal_fidl_name = fidl_library_name or (
        "cf.sc.internal." + cpp_namespace.replace("_", "")
    )

    fidl_source_target = name + "_fidl_config_lib_source"
    cpp_elf_source_target = name + "_cpp_elf_config_lib_source"
    cpp_elf_h_target = name + "_cpp_elf_config_lib_h"
    cpp_elf_cc_target = name + "_cpp_elf_config_lib_cc"
    cpp_elf_headers_target = name + "_cpp_elf_config_lib_headers"
    fidl_library_target = name + "_fidl_internal"

    fidl_config_client_lib_source(
        name = fidl_source_target,
        compiled_manifest = cm_label,
        fidl_library_name = internal_fidl_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    cpp_config_client_lib_source(
        name = cpp_elf_source_target,
        compiled_manifest = cm_label,
        namespace = cpp_namespace,
        fidl_library_name = internal_fidl_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    native.filegroup(
        name = cpp_elf_h_target,
        srcs = [":" + cpp_elf_source_target],
        output_group = "h",
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    native.filegroup(
        name = cpp_elf_cc_target,
        srcs = [":" + cpp_elf_source_target],
        output_group = "cc",
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fidl_library(
        name = fidl_library_target,
        srcs = [":" + fidl_source_target],
        library_name = internal_fidl_name,
        excluded_checks = [
            "invalid-copyright-for-platform-source-library",
            "wrong-prefix-for-platform-source-library",
        ],
        # Only the C++ bindings are used.
        enable_rust = False,
        enable_rust_next = False,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fx_cc_library(
        name = cpp_elf_headers_target,
        hdrs = [":" + cpp_elf_h_target],
        strip_include_prefix = cpp_elf_source_target,
        deps = [
            "//zircon/system/ulib/inspect",
        ],
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fx_cc_library(
        name = name,
        srcs = [
            ":" + cpp_elf_cc_target,
            ":" + cpp_elf_h_target,
        ],
        deps = [
            ":" + cpp_elf_headers_target,
        ],
        implementation_deps = [
            ":" + fidl_library_target + "_cpp",
            "//sdk/lib/fidl",
        ],
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = visibility,
    )

fuchsia_structured_config_cpp_elf_lib = macro(
    doc = """Defines a C++ structured configuration client library for an ELF component.

    The library is generated from the `config` schema of a compiled component
    manifest, and exposes the configuration of the component, e.g.:

    ```
    fx_component_manifest(
        name = "manifest",
        component_name = "my_component",
        manifest = "meta/my_component.cml",
    )

    fuchsia_structured_config_cpp_elf_lib(
        name = "my_component_config",
        cm_label = ":manifest",
    )

    fx_cc_binary(
        name = "bin",
        ...
        deps = [":my_component_config", ...],
    )
    ```

    And in C++:

    ```
    #include "path/to/pkg/my_component_config.h"

    auto config = my_component_config::Config::TakeFromStartupHandle();
    ```
    """,
    implementation = _fuchsia_structured_config_cpp_elf_lib_impl,
    attrs = {
        "cm_label": attr.label(
            doc = "The fx_component_manifest() target of the component.",
            mandatory = True,
            configurable = False,
            providers = [FuchsiaComponentManifestInfo],
        ),
        "namespace": attr.string(
            doc = "C++ namespace and header basename used by the generated library. Defaults to `name`. `.` and `-` are replaced by `_` and characters are lowercased.",
            configurable = False,
        ),
        "fidl_library_name": attr.string(
            doc = "Name for the generated internal FIDL library.",
            configurable = False,
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)
