# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for generating a qcow2 disk image resource for testing."""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")
load("@fuchsia_rules_common//:utils.bzl", "make_resource_struct")
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "package_resources_providers",
)
load(
    "//build/bazel/rules/python:py_toolchain.bzl",
    "PY_TOOLCHAIN_ATTRS",
    "generate_python_build_action",
)

visibility(["//src/virtualization/..."])

def _qcow_test_image_impl(ctx):
    filename = ctx.attr.filename if ctx.attr.filename else ctx.label.name + ".qcow2"
    raw_file = ctx.actions.declare_file(filename + ".raw")
    qcow_file = ctx.actions.declare_file(filename)

    raw_args = [
        "--file",
        raw_file.path,
        "--size",
        ctx.attr.disk_size,
    ]
    for write in ctx.attr.writes:
        raw_args.extend([
            "--write",
            write,
        ])

    generate_python_build_action(
        ctx = ctx,
        py_script = ctx.file._gen_sparse_image_script,
        inputs = [],
        outputs = [raw_file],
        arguments = raw_args,
        mnemonic = "GenSparseImage",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    ctx.actions.run(
        executable = ctx.file._qemu_img,
        arguments = [
            "convert",
            "-q",
            "-f",
            "raw",
            "-O",
            "qcow2",
            "-o",
            "cluster_size=" + ctx.attr.cluster_size,
            raw_file.path,
            qcow_file.path,
        ],
        inputs = [raw_file],
        outputs = [qcow_file],
        mnemonic = "QemuImgConvert",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return package_resources_providers(
        ctx,
        [make_resource_struct(src = qcow_file, dest = "data/" + filename)],
    )

qcow_test_image = rule(
    doc = """Generates a disk image in qcow2 format and exposes it as a package resource.""",
    implementation = _qcow_test_image_impl,
    attrs = {
        "disk_size": attr.string(
            doc = "The size of disk to generate (e.g. '10G', '64k').",
            mandatory = True,
        ),
        "filename": attr.string(
            doc = "The name of the generated disk image (defaults to '<name>.qcow2').",
            default = "",
        ),
        "cluster_size": attr.string(
            doc = "The qcow cluster size to use for the generated image.",
            default = "64k",
        ),
        "writes": attr.string_list(
            doc = "Byte patterns to write to ranges of the disk ('<offset>+<length>=<byte>').",
            default = [],
        ),
        "_gen_sparse_image_script": attr.label(
            default = "//src/virtualization/lib/qcow:scripts/gen-sparse-image.py",
            allow_single_file = True,
            cfg = "exec",
        ),
        "_qemu_img": attr.label(
            default = "//src/virtualization/lib/qcow:qemu_img",
            allow_single_file = True,
            cfg = "exec",
        ),
    } | PY_TOOLCHAIN_ATTRS,
)
