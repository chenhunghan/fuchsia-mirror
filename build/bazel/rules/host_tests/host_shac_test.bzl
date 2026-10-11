# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining `host_test()` targets that run `shac test`."""

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load(":host_test.bzl", "host_test")

def _host_shac_test_impl(name, visibility, src, data):
    # `shac test` takes test file paths relative to the checkout root.
    if src.repo_name:
        fail("src must be in the main repository, got: %s" % src)
    src_path = src.package + "/" + src.name if src.package else src.name

    host_test(
        name = name,
        binary = "//tools/shac_test_runner:shac_test_runner",
        test_args = [src_path],
        data = [src] + data + [
            "//:prebuilt/tools/shac/shac",
            "//:shac.textproto",
        ],
        target_compatible_with = HOST_CONSTRAINTS,
        # shac runs unmocked subprocesses in its own nsjail sandbox, which
        # can't create nested namespaces inside Bazel's linux-sandbox.
        tags = ["no-sandbox"],
        visibility = visibility,
    )

host_shac_test = macro(
    doc = """Defines a Fuchsia host test that runs `shac test` on a `*_test.star` file.

    host_test() stages every file in `data` at its workspace-relative path
    under the test's runfiles directory, so `shac test` runs against a minimal
    checkout containing only the declared files. Everything the test loads or
    executes (other than mocked subprocesses) must therefore be listed in
    `src` or `data`.
    """,
    implementation = _host_shac_test_impl,
    attrs = {
        "src": attr.label(
            doc = "The `*_test.star` file to execute.",
            mandatory = True,
            allow_single_file = [".star"],
            # The path is computed at loading time to pass to `shac test`.
            configurable = False,
        ),
        "data": attr.label_list(
            doc = "Additional `.star` modules, configs, scripts, or prebuilt " +
                  "binaries loaded or executed by the test.",
            allow_files = True,
            default = [],
        ),
    },
)
