# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# buildifier: disable=module-docstring
load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_fuchsia//fuchsia:defs.bzl", "fuchsia_remote_product_bundle")
load("@rules_fuchsia//fuchsia/private:providers.bzl", "FuchsiaProductBundleInfo")

def _fuchsia_remote_product_bundle_test_impl(ctx):
    env = analysistest.begin(ctx)

    target_under_test = analysistest.target_under_test(env)
    pb_info = target_under_test[FuchsiaProductBundleInfo]

    asserts.equals(env, True, pb_info.is_remote)
    asserts.equals(env, ctx.attr.expected_transfer_url, pb_info.product_bundle)
    asserts.equals(env, ctx.attr.expected_product_bundle_name, pb_info.product_bundle_name)
    asserts.equals(env, ctx.attr.expected_product_version, pb_info.product_version)

    # Ensure all generated workflow tasks (download, emu, flash, ota) analyze
    # cleanly and produce executable DefaultInfo providers.
    for task in ctx.attr.workflow_tasks:
        asserts.true(env, task[DefaultInfo].files_to_run.executable != None)

    return analysistest.end(env)

fuchsia_remote_product_bundle_test = analysistest.make(
    _fuchsia_remote_product_bundle_test_impl,
    attrs = {
        "expected_transfer_url": attr.string(mandatory = True),
        "expected_product_bundle_name": attr.string(mandatory = True),
        "expected_product_version": attr.string(mandatory = True),
        "workflow_tasks": attr.label_list(mandatory = True),
    },
)

def _test_remote_product_bundle():
    transfer_url = "gs://fuchsia-artifacts/builds/1234/product_bundles/core.x64/transfer.json"

    fuchsia_remote_product_bundle(
        name = "remote_pb_with_version",
        transfer_url = transfer_url,
        product_bundle_name = "core.x64",
        product_version = "33.20260911.5.1",
        tags = ["manual"],
    )

    fuchsia_remote_product_bundle_test(
        name = "remote_pb_with_version_test",
        target_under_test = ":remote_pb_with_version",
        expected_transfer_url = transfer_url,
        expected_product_bundle_name = "core.x64",
        expected_product_version = "33.20260911.5.1",
        workflow_tasks = [
            ":remote_pb_with_version.download",
            ":remote_pb_with_version.emu",
            ":remote_pb_with_version.flash",
            ":remote_pb_with_version.ota",
        ],
    )

    fuchsia_remote_product_bundle(
        name = "remote_pb_default_version",
        transfer_url = transfer_url,
        tags = ["manual"],
    )

    fuchsia_remote_product_bundle_test(
        name = "remote_pb_default_version_test",
        target_under_test = ":remote_pb_default_version",
        expected_transfer_url = transfer_url,
        expected_product_bundle_name = "remote_pb_default_version",
        expected_product_version = "",
        workflow_tasks = [
            ":remote_pb_default_version.download",
            ":remote_pb_default_version.emu",
            ":remote_pb_default_version.flash",
            ":remote_pb_default_version.ota",
        ],
    )

    fuchsia_remote_product_bundle_test(
        name = "remote_pb_from_repository_test",
        target_under_test = "@test_fuchsia_products//:core.x64",
        expected_transfer_url = transfer_url,
        expected_product_bundle_name = "core.x64",
        expected_product_version = "33.20260911.5.1",
        workflow_tasks = [
            "@test_fuchsia_products//:core.x64.download",
            "@test_fuchsia_products//:core.x64.emu",
            "@test_fuchsia_products//:core.x64.flash",
            "@test_fuchsia_products//:core.x64.ota",
        ],
    )

# buildifier: disable=function-docstring
def fuchsia_remote_product_bundle_test_suite(name, **kwargs):
    _test_remote_product_bundle()

    native.test_suite(
        name = name,
        tests = [
            ":remote_pb_with_version_test",
            ":remote_pb_default_version_test",
            ":remote_pb_from_repository_test",
        ],
        **kwargs
    )
