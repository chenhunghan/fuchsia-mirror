# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""cipd_prebuilt_file() and cipd_prebuilt_directory() rule definitions for on-demand prebuilts."""

load("@fuchsia_build_config//:defs.bzl", "build_config")
load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")

CipdPrebuiltInfo = provider(
    doc = (
        "Marks a target materialized from a CIPD prebuilt under FUCHSIA_DIR " +
        "(so fx_host_tool does not mark it .always_build) and records its " +
        "checkout-relative path."
    ),
    fields = {
        "source_rel_path": "Source-tree relative path under FUCHSIA_DIR for CIPD prebuilts.",
    },
)

def _cipd_prebuilt_file_impl(ctx):
    source_rel_path = ctx.attr.path.removeprefix("//").strip("/")
    rel_out = ctx.attr.out if ctx.attr.out else source_rel_path
    out = ctx.actions.declare_file(rel_out)

    ctx.actions.run(
        executable = ctx.file._python_bin,
        arguments = [
            ctx.file._fetch_script.path,
            "--path=%s" % ctx.attr.path,
            "--output-file=%s" % out.path,
        ],
        inputs = [ctx.file._fetch_script, ctx.file._jiri_snapshot, ctx.file._python_bin],
        outputs = [out],
        mnemonic = "CipdPrebuiltFetch",
        progress_message = "Fetching CIPD prebuilt %s" % ctx.attr.path,
        use_default_shell_env = True,
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return [
        DefaultInfo(
            files = depset([out]),
            runfiles = ctx.runfiles(files = [out]),
        ),
        CipdPrebuiltInfo(source_rel_path = source_rel_path),
    ]

def _cipd_prebuilt_directory_impl(ctx):
    source_rel_path = ctx.attr.path.removeprefix("//").strip("/")
    out = ctx.actions.declare_directory(source_rel_path)

    ctx.actions.run(
        executable = ctx.file._python_bin,
        arguments = [
            ctx.file._fetch_script.path,
            "--path=%s" % ctx.attr.path,
            "--output-dir=%s" % out.path,
        ],
        inputs = [ctx.file._fetch_script, ctx.file._jiri_snapshot, ctx.file._python_bin],
        outputs = [out],
        mnemonic = "CipdPrebuiltFetch",
        progress_message = "Fetching CIPD prebuilt %s" % ctx.attr.path,
        use_default_shell_env = True,
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return [
        DefaultInfo(
            files = depset([out]),
            runfiles = ctx.runfiles(files = [out]),
        ),
        CipdPrebuiltInfo(source_rel_path = source_rel_path),
    ]

cipd_prebuilt_file = rule(
    implementation = _cipd_prebuilt_file_impl,
    doc = "Fetches an on-demand prebuilt file via `jiri fetch-package` and materializes it in bazel-out/.",
    attrs = {
        "out": attr.string(
            default = "",
            doc = "Optional package-relative output file path when multiple targets in one package share a CIPD directory prefix.",
        ),
        "path": attr.string(
            mandatory = True,
            doc = "File path under prebuilt/ to fetch (e.g. 'prebuilt/third_party/...' or '//prebuilt/third_party/...').",
        ),
        "_jiri_snapshot": attr.label(
            doc = "The Jiri snapshot file used to invalidate cached actions when package versions change.",
            default = "//build/info:jiri_snapshot",
            allow_single_file = True,
        ),
        "_fetch_script": attr.label(
            doc = "Internal script used to fetch and materialize the CIPD prebuilt.",
            default = "//build/bazel/scripts:fetch_cipd_prebuilt.py",
            allow_single_file = True,
        ),
        "_python_bin": attr.label(
            doc = "The vendored Python interpreter used to run the fetch script.",
            default = "//:prebuilt/third_party/python3/%s/bin/python3" % build_config.host_tag,
            allow_single_file = True,
        ),
    },
)

cipd_prebuilt_directory = rule(
    implementation = _cipd_prebuilt_directory_impl,
    doc = "Fetches an on-demand prebuilt directory via `jiri fetch-package` and materializes it as a TreeArtifact in bazel-out/.",
    attrs = {
        "path": attr.string(
            mandatory = True,
            doc = "Directory path under prebuilt/ to fetch (e.g. 'prebuilt/third_party/...' or '//prebuilt/third_party/...').",
        ),
        "_jiri_snapshot": attr.label(
            doc = "The Jiri snapshot file used to invalidate cached actions when package versions change.",
            default = "//build/info:jiri_snapshot",
            allow_single_file = True,
        ),
        "_fetch_script": attr.label(
            doc = "Internal script used to fetch and materialize the CIPD prebuilt.",
            default = "//build/bazel/scripts:fetch_cipd_prebuilt.py",
            allow_single_file = True,
        ),
        "_python_bin": attr.label(
            doc = "The vendored Python interpreter used to run the fetch script.",
            default = "//:prebuilt/third_party/python3/%s/bin/python3" % build_config.host_tag,
            allow_single_file = True,
        ),
    },
)
