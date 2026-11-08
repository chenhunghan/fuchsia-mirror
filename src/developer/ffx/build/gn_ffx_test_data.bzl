# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bridges GN-built ffx and subtool `host_test_data` targets into Bazel."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host_tests:host_test_data.bzl", "FuchsiaHostTestDataInfo")

visibility(["//src/developer/ffx/..."])

def _gn_ffx_test_data_impl(ctx):
    src_by_basename = {f.basename: f for f in ctx.files.src}
    copied_outs = []
    dest_map = {}

    for filename in ctx.attr.expected_files:
        if filename not in src_by_basename:
            fail(
                "Expected file '{}' not found in {} (available: {})".format(
                    filename,
                    ctx.attr.src.label,
                    sorted(src_by_basename.keys()),
                ),
            )
        src_file = src_by_basename[filename]
        dest_rel = "host-tools/" + filename
        out_file = ctx.actions.declare_file(dest_rel)

        # Materialize the gn_targets file into bazel-out/ so that runtime
        # directory symlinks in host_test() remain valid even if a subsequent
        # Bazel invocation replants external/ without gn_targets.
        ctx.actions.run_shell(
            inputs = [src_file],
            outputs = [out_file],
            command = 'cp -Lf "$1" "$2" && if [ -x "$1" ]; then chmod +x "$2"; fi',
            arguments = [src_file.path, out_file.path],
            mnemonic = "CopyGnFfxTestData",
            progress_message = "Copying GN ffx test data %s" % dest_rel,
            execution_requirements = {
                "local": "1",
                "no-remote": "1",
            },
        )
        copied_outs.append(out_file)
        dest_map[dest_rel] = out_file

    runfiles = ctx.runfiles(
        files = copied_outs,
        symlinks = dest_map,
    )

    return [
        DefaultInfo(
            files = depset(copied_outs),
            runfiles = runfiles,
        ),
        FuchsiaHostTestDataInfo(
            label = ctx.label,
            files = dest_map,
        ),
    ]

_gn_ffx_test_data_rule = rule(
    doc = "Copies GN-built ffx host_test_data files from gn_targets into host-tools/.",
    implementation = _gn_ffx_test_data_impl,
    provides = [DefaultInfo, FuchsiaHostTestDataInfo],
    attrs = {
        "src": attr.label(
            doc = "The gn_targets filegroup exposing the GN-built ffx binary and metadata.",
            mandatory = True,
            allow_files = True,
        ),
        "expected_files": attr.string_list(
            doc = "Basenames expected in `src` to expose under `host-tools/<basename>`.",
            mandatory = True,
            allow_empty = False,
        ),
    },
)

def gn_ffx_test_data(
        name,
        expected_files,
        gn_target = None,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = None,
        testonly = True):
    """Exposes a GN-built ffx or subtool `host_test_data` target to Bazel host tests.

    Places each file in `expected_files` at `host-tools/<filename>` in both
    `FuchsiaHostTestDataInfo` and `DefaultInfo` runfiles symlinks.
    """
    if gn_target == None:
        gn_target = "@gn_targets//toolchain_host_x64/{}:{}".format(
            native.package_name(),
            name,
        )

    _gn_ffx_test_data_rule(
        name = name,
        src = gn_target,
        expected_files = expected_files,
        target_compatible_with = target_compatible_with,
        visibility = visibility,
        testonly = testonly,
    )
