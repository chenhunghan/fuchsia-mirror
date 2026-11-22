# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Linux host Python C extension module."""

load("@bazel_skylib//rules:copy_file.bzl", "copy_file")
load("@rules_python//python:defs.bzl", "py_library")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/cc:instrumentation.bzl", "DISABLE_INSTRUMENTATION_FEATURES")

visibility(["//..."])

_LINUX_HOST_CONSTRAINTS = [
    "@platforms//os:linux",
    "//build/bazel/platforms:linux_flavor_host",
]

def _c_extension_shlib_data_impl(ctx):
    shlib_info = ctx.attr.shlib[DefaultInfo]
    runtime_solibs = [
        f
        for f in shlib_info.default_runfiles.files.to_list()
        if f.extension == "so" or ".so." in f.basename
    ]
    runfiles = ctx.runfiles(
        files = runtime_solibs,
        transitive_files = shlib_info.files,
    )
    return [
        DefaultInfo(
            files = shlib_info.files,
            runfiles = runfiles,
        ),
    ]

# Forwards the linked extension .so and any runtime shared libraries (*.so /
# *.so.*) into py_library's data runfiles while filtering out non-library data /
# compile_data files (such as unbuilt Ninja-generated workspace symlinks leaked
# by transitive rust_library deps into cc_binary's default_runfiles, which
# otherwise break host_py_test's .runtime_dir tree artifact under standalone
# `fx bazel test`).
_c_extension_shlib_data = rule(
    doc = "Forwards the extension and transitive `.so` runfiles while filtering non-library data.",
    implementation = _c_extension_shlib_data_impl,
    attrs = {
        "shlib": attr.label(
            doc = "The linked extension shared library target.",
            mandatory = True,
        ),
    },
)

def python_c_extension(
        *,
        name,
        srcs,
        deps = [],
        defines = [],
        stubs_root = None,
        stubs = [],
        testonly = False,
        target_compatible_with = None,
        visibility = None):
    """Defines a Linux host Python C extension library matching GN `python_c_extension()`.

    Builds `lib<name>.so` linked against the Python C API symbol linker script,
    generates a `<name>/__init__.py` import hook under an isolated `_<name>_py_stage`
    directory (avoiding collisions with any checked-in `<name>/__init__.py` IDE stub),
    stages `lib<name>.so` and any `.pyi` stubs alongside `__init__.py`, and wraps
    them in `py_library(name = name)`.

    Args:
      name: Name of the Python module and `py_library` target.
      srcs: C/C++ source and header files for the extension shared library.
      deps: C/C++ or `rustc_staticlib` dependencies for the shared library.
      defines: Preprocessor defines applied when compiling `srcs`.
      stubs_root: Base directory relative to which `stubs` paths are resolved. Required if `stubs` is non-empty.
      stubs: List of `.pyi` stub paths relative to `stubs_root`.
      testonly: Standard Bazel `testonly` flag.
      target_compatible_with: Platform constraints; defaults to Linux host.
      visibility: Target visibility.
    """
    if stubs and not stubs_root:
        fail("In python_c_extension(%r): stubs_root must be provided when stubs is non-empty" % name)
    normalized_stubs_root = None
    if stubs_root:
        if stubs_root.startswith("/"):
            fail("In python_c_extension(%r): stubs_root %r must be a relative path within the package" % (name, stubs_root))
        root_segments = [s for s in stubs_root.split("/") if s and s != "."]
        if ".." in root_segments:
            fail("In python_c_extension(%r): stubs_root %r must be a relative path within the package" % (name, stubs_root))
        normalized_stubs_root = "/".join(root_segments) if root_segments else "."
    for stub in stubs:
        # An empty split segment ("") rejects empty strings as well as leading,
        # trailing, or repeated slashes.
        if any([seg in ("", ".", "..") for seg in stub.split("/")]):
            fail("In python_c_extension(%r): stub %r must be a relative path within stubs_root" % (name, stub))
    if target_compatible_with == None:
        target_compatible_with = _LINUX_HOST_CONSTRAINTS
    elif type(target_compatible_with) == "list":
        target_compatible_with = _LINUX_HOST_CONSTRAINTS + [
            c
            for c in target_compatible_with
            if c not in _LINUX_HOST_CONSTRAINTS
        ]
    else:
        target_compatible_with = _LINUX_HOST_CONSTRAINTS + target_compatible_with

    stage_root = "_{}_py_stage".format(name)
    stage_pkg_dir = "{}/{}".format(stage_root, name)
    shlib_base = "lib" + name
    shlib_so = "{}/{}.so".format(stage_pkg_dir, shlib_base)

    # Disable instrumentation across the extension shared library and its
    # transitive dependencies: host sanitizer runtimes are statically linked
    # into executables, not shared libraries, and prebuilt_python does not
    # provide symbols such as __asan_init when dlopen-ing extensions.
    fx_cc_binary(
        name = shlib_so,
        srcs = srcs,
        additional_linker_inputs = [
            "//build/config/python:generate_python_api_symbols",
        ],
        defines = defines,
        linkopts = [
            "-Wl,-T,$(location //build/config/python:generate_python_api_symbols)",
        ],
        linkshared = True,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        transitive_features = DISABLE_INSTRUMENTATION_FEATURES,
        visibility = ["//visibility:private"],
        deps = (deps or []) + [
            "@prebuilt_python//:headers",
        ],
    )

    wrapper_name = name + "_wrapper"
    wrapper_out = "{}/__init__.py".format(stage_pkg_dir)

    native.genrule(
        name = wrapper_name,
        outs = [wrapper_out],
        cmd = " ".join([
            "$(location //build/python:create_c_extension_import_hook)",
            "--target_name",
            name,
            "--shlib",
            shlib_base,
            "--gen_dir",
            "$(RULEDIR)/" + stage_root,
        ]),
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        tools = ["//build/python:create_c_extension_import_hook"],
        visibility = ["//visibility:private"],
    )

    staged_stubs = []
    for idx, stub in enumerate(stubs):
        stub_target = "{}_stub_{}".format(name, idx)
        stub_out = "{}/{}".format(stage_pkg_dir, stub)
        stub_src = stub if normalized_stubs_root == "." else "{}/{}".format(normalized_stubs_root, stub)

        # Copy rather than symlink: copy_file defaults to allow_symlink=True for
        # non-executable files, producing symlinks through
        # gen/build/bazel/workspace/<top_dir>/... that collide with the
        # workspace/<top_dir> directory symlink when LUCI CAS downloads
        # host_test runtime trees on Swarming bots.
        copy_file(
            name = stub_target,
            src = stub_src,
            out = stub_out,
            allow_symlink = False,
            target_compatible_with = target_compatible_with,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        staged_stubs.append(":" + stub_target)

    shlib_data_name = name + "_shlib_data"
    _c_extension_shlib_data(
        name = shlib_data_name,
        shlib = ":" + shlib_so,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    py_library(
        name = name,
        srcs = [":" + wrapper_name],
        data = [":" + shlib_data_name] + staged_stubs,
        imports = [stage_root],
        pyi_srcs = staged_stubs,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )
