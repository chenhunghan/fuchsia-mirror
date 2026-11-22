# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines `driver_bind_bytecode_test()`, which tests a driver's bind rules."""

load("@bazel_skylib//lib:shell.bzl", "shell")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")
load(":providers.bzl", "DriverBindBytecodeInfo")

visibility([
    "//build/bazel/rules/...",
    "//examples/...",
    "//sdk/...",
    "//src/...",
    "//vendor/...",
    "//zircon/...",
])

def _driver_bind_bytecode_test_runner_impl(ctx):
    info = ctx.attr.bytecode[DriverBindBytecodeInfo]
    if not info.src:
        fail("{}: {} has no bind rules to test.".format(ctx.label, ctx.attr.bytecode.label))

    bindc = ctx.executable._bindc
    bind_library_sources = info.bind_library_sources.to_list()
    args = [
        "test",
        "--lint",
        info.src.short_path,
        "--test-spec",
        ctx.file.test_specification.short_path,
    ]
    if bind_library_sources:
        args += ["--include"] + [f.short_path for f in bind_library_sources]

    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        output = script,
        content = (
            "#!/bin/bash\n" +
            "set -euo pipefail\n" +
            'if [[ -n "${RUNFILES_DIR:-}" && -d "${RUNFILES_DIR}/_main" ]]; then\n' +
            '  cd "${RUNFILES_DIR}/_main"\n' +
            "fi\n" +
            "exec {} {}\n".format(
                shell.quote(bindc.short_path),
                " ".join([shell.quote(arg) for arg in args]),
            )
        ),
        is_executable = True,
    )

    # This rule is `executable = True` and passed directly to `host_test(binary = ...)`
    # rather than wrapped in `sh_binary()` because `info.src` and
    # `info.bind_library_sources` are only available at analysis time from
    # `DriverBindBytecodeInfo` for constructing runfiles.
    runfiles = ctx.runfiles(
        files = [bindc, info.src, ctx.file.test_specification] + bind_library_sources,
    )
    runfiles = runfiles.merge(ctx.attr._bindc[DefaultInfo].default_runfiles)
    return [DefaultInfo(executable = script, runfiles = runfiles)]

_driver_bind_bytecode_test_runner = rule(
    doc = "Generates an executable `bindc test` runner with runfiles from `DriverBindBytecodeInfo`.",
    implementation = _driver_bind_bytecode_test_runner_impl,
    executable = True,
    attrs = {
        "bytecode": attr.label(
            doc = "The `driver_bind_bytecode()` target whose bind rules to test.",
            mandatory = True,
            providers = [DriverBindBytecodeInfo],
        ),
        "test_specification": attr.label(
            doc = "The bind test specification JSON file.",
            mandatory = True,
            allow_single_file = [".json"],
        ),
        "_bindc": attr.label(
            default = "//tools/bindc:bin",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _driver_bind_bytecode_test_impl(
        name,
        visibility,
        bytecode,
        test_specification,
        tags,
        target_compatible_with,
        **kwargs):
    runner_name = name + "_runner"
    runner_tags = ["manual"] + (tags if tags != None else [])
    compat = HOST_OS_CONSTRAINTS if target_compatible_with == None else target_compatible_with

    _driver_bind_bytecode_test_runner(
        name = runner_name,
        testonly = True,
        bytecode = bytecode,
        test_specification = test_specification,
        tags = runner_tags,
        target_compatible_with = compat,
        visibility = ["//visibility:private"],
    )
    host_test(
        name = name,
        binary = ":" + runner_name,
        tags = tags,
        target_compatible_with = compat,
        visibility = visibility,
        **kwargs
    )

driver_bind_bytecode_test = macro(
    doc = """Defines a host test that runs `bindc test` on a `driver_bind_bytecode()` target.

    Wraps the generated `bindc test` runner in `host_test()` so it provides
    `FuchsiaHostTestInfo` for `fx test` and `tests.json` as well as running
    under `fx bazel test --config=host`. Add the test to the package's
    `host_tests` `test_suite()`.

    Not converted by bazel2gn; reaches CI through `bazel_test_suite()`.
    GN equivalent: `driver_bind_rules(tests = ...)` / `bind_test()` in
    //build/bind/bind.gni.
    """,
    implementation = _driver_bind_bytecode_test_impl,
    inherit_attrs = host_test,
    attrs = {
        "binary": None,
        "test_args": None,
        "unstripped_binary": None,
        "bytecode": attr.label(
            doc = "The `driver_bind_bytecode()` target whose bind rules to test.",
            mandatory = True,
        ),
        "test_specification": attr.label(
            doc = """The JSON bind test specification file.
            GN equivalent: `tests` on `driver_bind_rules()` / `bind_test()`
            """,
            mandatory = True,
            allow_single_file = [".json"],
        ),
    },
)
