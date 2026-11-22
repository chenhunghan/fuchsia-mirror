# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_rules_common//assembly:json_utils.bzl", "extract_labels")
load(
    "@fuchsia_rules_common//assembly:product_configuration.bzl",
    "PRODUCT_CONFIGURATION_COMMON_ATTRS",
    "product_configuration_common_impl",
)
load("@fuchsia_rules_common//assembly:providers.bzl", "FuchsiaProductConfigInfo")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")

def _product_configuration_impl(ctx):
    assembly_config_binary = ctx.executable._assembly_config

    return product_configuration_common_impl(
        ctx,
        assembly_config_binary,
        bootfs_files_package = ctx.attr.bootfs_files_package,
    )

_product_configuration = rule(
    doc = """Generates a product configuration file for Fuchsia platform testing (in-tree) only.""",
    implementation = _product_configuration_impl,
    provides = [FuchsiaProductConfigInfo],
    # Use the common attributes for product assembly, plus the in-tree-only
    # attributes and the Bazel-built `assembly_config` host tool.
    attrs = PRODUCT_CONFIGURATION_COMMON_ATTRS | {
        "bootfs_files_package": attr.label(
            doc = "A package of files to include in the bootfs of the zbi.",
            providers = [FuchsiaPackageInfo],
            default = None,
        ),
        "enable_example_aib": attr.bool(
            doc = "Optionally enable example AIBs for testing.",
            default = False,
        ),
        "_assembly_config": attr.label(
            default = "//build/assembly/tools/assembly_config",
            executable = True,
            cfg = "exec",
        ),
    },
)

def product_configuration(
        *,
        name,
        product_config_json = None,
        bootfs_packages = None,
        bootfs_files_package = None,
        base_packages = None,
        cache_packages = None,
        base_driver_packages = None,
        ota_configuration = None,
        starnix_containers = [],
        enable_example_aib = False,
        **kwargs):
    """Generates a product configuration file for Fuchsia platform testing (in-tree) only."""
    json_config = product_config_json if product_config_json else {}
    _product_configuration(
        name = name,
        product_config = json.encode_indent(json_config, indent = "    "),
        product_config_labels = extract_labels(json_config),
        bootfs_packages = bootfs_packages,
        bootfs_files_package = bootfs_files_package,
        base_packages = base_packages,
        cache_packages = cache_packages,
        base_driver_packages = base_driver_packages,
        ota_configuration = ota_configuration,
        starnix_containers = starnix_containers,
        enable_example_aib = enable_example_aib,
        **kwargs
    )
