# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Starlark macros for defining Fuchsia system test suites (upgrade, reboot, recovery).

This is the Bazel port of //src/sys/pkg/tests/system-tests/system-tests.gni.
"""

load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")
load(
    "//build/bazel/rules/testing:fx_test_environment.bzl",
    "FxTestEnvironmentInfo",
)

visibility("private")

# The default service account that allows system tests to download build artifacts.
DEFAULT_SERVICE_ACCOUNT = (
    "artifact-readers" + "@" + "fuchsia-infra.iam.gserviceaccount.com"
)

# Environment tag used in GN's disabled_env to prevent infra from scheduling raw binaries.
DISABLED_ENV_TAG = "system-tests-disabled"

# Default runtime test_data dependencies shared across all system test suites.
# Note: `//src/sys/pkg/tests/system-tests:test_data` currently stages AVB keys
# and `test_private_key.pem`; `artifacts`, `lkg`, `mock-omaha-server`, and `zbi`
# from GN `test_tools` (in //src/sys/pkg/tests/system-tests/BUILD.gn and
# //src/testing/host-target-testing/omaha_tool/BUILD.gn) are pending Bazel
# migration/export prerequisites (see `SYSTEM_TESTS_TOOLS_DATA`).
DEFAULT_SYSTEM_TESTS_DATA = [
    "//src/sys/pkg/tests/system-tests:non_go_deps",
]

# Host tool binaries staged into `test_data/system-tests/` by GN `test_tools`
# (in //src/sys/pkg/tests/system-tests/BUILD.gn and
# //src/testing/host-target-testing/omaha_tool/BUILD.gn).
# Merge `SYSTEM_TESTS_TOOLS_DATA` into `//src/sys/pkg/tests/system-tests:test_data`
# `files_map` once these targets (including `cipd_prebuilt_file` targets for
# `//prebuilt/tools/artifacts/artifacts` and `//prebuilt/tools/lkg/lkg`) are
# migrated/exported in Bazel.
SYSTEM_TESTS_TOOLS_DATA = {
    "test_data/system-tests/artifacts": "//src/sys/pkg/tests/system-tests:artifacts",
    "test_data/system-tests/lkg": "//src/sys/pkg/tests/system-tests:lkg",
    "test_data/system-tests/mock-omaha-server": "//src/sys/pkg/testing/mock-omaha-server:bin",
    "test_data/system-tests/zbi": "//zircon/tools/zbi",
}

# ffx host_test_data targets required at runtime by system-tests (ported in parallel).
# Add `FFX_SYSTEM_TESTS_DATA` to `//src/sys/pkg/tests/system-tests:non_go_deps` (or pass via
# `extra_test_data = FFX_SYSTEM_TESTS_DATA`) once those targets are available in Bazel.
FFX_SYSTEM_TESTS_DATA = [
    "//src/developer/ffx:test_data",
    "//src/developer/ffx/plugins/component:ffx_component_tool_test_data",
    "//src/developer/ffx/plugins/emulator:ffx_emulator_tool_test_data",
    "//src/developer/ffx/plugins/inspect:ffx_inspect_tool_test_data",
    "//src/developer/ffx/plugins/log:ffx_log_tool_test_data",
    "//src/developer/ffx/plugins/package:ffx_package_tool_test_data",
    "//src/developer/ffx/plugins/product:ffx_product_tool_test_data",
    "//src/developer/ffx/plugins/product_bundle:ffx_product_bundle_tool_test_data",
    "//src/developer/ffx/plugins/repository:ffx_repository_tool_test_data",
]

SystemTestSuiteMetadataInfo = provider(
    doc = "Infra scheduling and invocation metadata for a system_test_*_suite() target.",
    fields = {
        "suite_name": "Name of the system test suite.",
        "test_binary": "Name of the underlying Go system test binary.",
        "test_args": "CLI flags passed to the underlying Go system test binary.",
        "timeout": "Go test timeout duration string.",
        "service_account": "Service account injected into all test environments.",
        "environments": "List of normalized environment dicts (dimensions, tags, service_account).",
        "metadata_json": "Generated JSON File recording the resolved environments and invocation spec.",
    },
)

def _system_test_metadata_impl(ctx):
    inline_envs = json.decode(ctx.attr.inline_environments)
    resolved_envs = []
    seen = {}

    for raw_env in inline_envs:
        env = dict(raw_env)
        env["service_account"] = ctx.attr.service_account
        key = json.encode(env)
        if key not in seen:
            seen[key] = True
            resolved_envs.append(env)

    for target in ctx.attr.environment_targets:
        env = dict(target[FxTestEnvironmentInfo].environment)
        env["service_account"] = ctx.attr.service_account
        key = json.encode(env)
        if key not in seen:
            seen[key] = True
            resolved_envs.append(env)

    if not resolved_envs:
        fail("environments must not be empty for {}".format(ctx.attr.suite_name))

    out = ctx.actions.declare_file(ctx.attr.name + ".json")
    payload = {
        "environments": resolved_envs,
        "service_account": ctx.attr.service_account,
        "suite_name": ctx.attr.suite_name,
        "test_args": ctx.attr.test_args,
        "test_binary": ctx.attr.test_binary,
        "timeout": ctx.attr.test_timeout,
    }
    ctx.actions.write(
        output = out,
        content = json.encode_indent(payload, indent = "  ") + "\n",
    )

    return [
        DefaultInfo(
            files = depset([out]),
            runfiles = ctx.runfiles(files = [out]),
        ),
        SystemTestSuiteMetadataInfo(
            suite_name = ctx.attr.suite_name,
            test_binary = ctx.attr.test_binary,
            test_args = ctx.attr.test_args,
            timeout = ctx.attr.test_timeout,
            service_account = ctx.attr.service_account,
            environments = resolved_envs,
            metadata_json = out,
        ),
    ]

_system_test_metadata = rule(
    implementation = _system_test_metadata_impl,
    doc = "Records infra environment dimensions, tags, timeout, and service_account metadata for a system test suite.",
    provides = [SystemTestSuiteMetadataInfo],
    attrs = {
        "suite_name": attr.string(mandatory = True),
        "test_binary": attr.string(mandatory = True),
        "test_args": attr.string_list(default = []),
        "test_timeout": attr.string(mandatory = True),
        "service_account": attr.string(mandatory = True),
        "inline_environments": attr.string(default = "[]"),
        "environment_targets": attr.label_list(
            providers = [FxTestEnvironmentInfo],
            default = [],
        ),
    },
)

def _require_non_empty_str(val, field_name, target_name, allow_int = False):
    if allow_int and type(val) == "int":
        return str(val)
    if type(val) != "string" or val == "":
        fail("'{}' cannot have an empty {}".format(target_name, field_name))
    return val

def _normalize_environments(name, environments, service_account):
    if environments == None or len(environments) == 0:
        fail("environments must be defined and non-empty for {}".format(name))

    inline_envs = []
    env_targets = []
    derived_tags = ["service_account:" + service_account]

    for env in environments:
        if type(env) == "dict":
            dimensions = env.get("dimensions")
            if type(dimensions) != "dict" or not dimensions:
                fail("environment dict in '{}' must define a non-empty 'dimensions' dict".format(name))
            normalized = {
                "dimensions": dict(dimensions),
                "service_account": service_account,
            }
            env_tags = env.get("tags", env.get("env_tags"))
            if env_tags:
                normalized["tags"] = list(env_tags)
                for tag in env_tags:
                    tag_entry = "env_tag:" + tag
                    if tag_entry not in derived_tags:
                        derived_tags.append(tag_entry)
            if "device_type" in dimensions:
                dt_entry = "device_type:" + dimensions["device_type"]
                if dt_entry not in derived_tags:
                    derived_tags.append(dt_entry)
            if env.get("emulator"):
                normalized["emulator"] = dict(env["emulator"])
            if env.get("netboot"):
                normalized["netboot"] = True
            inline_envs.append(normalized)
        elif type(env) == "string":
            env_targets.append(env)
        else:
            fail("environments entry in '{}' must be an environment dict or fx_test_environment label string, got {}".format(
                name,
                type(env),
            ))

    return inline_envs, env_targets, derived_tags

def _dedupe_labels(labels):
    result = []
    seen = {}
    for label in labels:
        if label not in seen:
            seen[label] = True
            result.append(label)
    return result

def _define_system_test_suite(
        *,
        name,
        test_binary_name,
        test_binary_data,
        args,
        environments,
        service_account,
        timeout,
        test_data,
        extra_test_data,
        tags,
        visibility):
    host_test_target_name = "{}_{}".format(name, test_binary_name)
    metadata_target_name = "{}_metadata".format(host_test_target_name)

    resolved_service_account = DEFAULT_SERVICE_ACCOUNT
    if service_account != None:
        resolved_service_account = _require_non_empty_str(
            service_account,
            "service_account",
            host_test_target_name,
        )

    resolved_timeout = "5m"
    if timeout != None:
        resolved_timeout = _require_non_empty_str(
            timeout,
            "timeout",
            host_test_target_name,
        )

    inline_envs, env_targets, derived_tags = _normalize_environments(
        name = name,
        environments = environments,
        service_account = resolved_service_account,
    )

    base_test_data = DEFAULT_SYSTEM_TESTS_DATA if test_data == None else list(test_data)
    all_data = _dedupe_labels(
        [
            test_binary_data,
            ":" + metadata_target_name,
        ] + base_test_data + list(extra_test_data),
    )

    tags = list(tags)
    if "manual" not in tags:
        tags.append("manual")

    host_test_tags = list(tags)
    for t in derived_tags:
        if t not in host_test_tags:
            host_test_tags.append(t)

    # System tests are Linux-only, matching `assert(is_linux, ...)` in system-tests.gni.
    linux_only = ["@platforms//os:linux"]

    _system_test_metadata(
        name = metadata_target_name,
        testonly = True,
        suite_name = name,
        test_binary = test_binary_name,
        test_args = args,
        test_timeout = resolved_timeout,
        service_account = resolved_service_account,
        inline_environments = json.encode(inline_envs),
        environment_targets = env_targets,
        target_compatible_with = linux_only,
        visibility = visibility,
    )

    # Wrap the Go system test binary with //tools/go_test_parser, matching host_go_test.bzl
    # and //build/go/go_test.gni.
    wrapped_test_args = [
        "./" + test_binary_name,
        "-test.timeout",
        resolved_timeout,
        "-test.v",
    ] + args

    host_test(
        name = host_test_target_name,
        binary = "//tools/go_test_parser:go_test_parser",
        test_args = wrapped_test_args,
        data = all_data,
        tags = host_test_tags,
        target_compatible_with = linux_only,
        visibility = visibility,
    )

    native.test_suite(
        name = name,
        tests = [":" + host_test_target_name],
        tags = list(tags),
        visibility = visibility,
    )

def system_test_upgrade_suite(
        name,
        environments,
        builds = None,
        downgrade_release_builder = None,
        downgrade_release_build_id = None,
        downgrade_ota_attempts = None,
        upgrade_release_builder = None,
        upgrade_release_build_id = None,
        installer = None,
        service_account = None,
        timeout = None,
        pave_timeout = None,
        cycle_timeout = None,
        cycle_count = None,
        device_connect_timeout = None,
        workaround_broken_time_skip = False,
        workaround_downgrade_ota_no_rewrite_rules = False,
        require_cup = False,
        use_flash = False,
        build_expect_unknown_firmware = False,
        max_system_image_size = None,
        max_update_images_size = None,
        max_update_package_size = None,
        bootfs_compression = None,
        check_abr = True,
        resolver_mode = None,
        extra_args = [],
        test_data = None,
        extra_test_data = [],
        tags = [],
        visibility = None):
    """Configures a System Test Upgrade Suite (Bazel port of system_test_upgrade_suite in system-tests.gni).

    Generates:
      - `<name>_system_tests_upgrade_metadata`: JSON metadata & SystemTestSuiteMetadataInfo provider
        recording environments, dimensions, tags, timeout, and service_account.
      - `<name>_system_tests_upgrade`: `host_test` wrapping `system_tests_upgrade` via `go_test_parser`.
      - `<name>`: `test_suite` grouping the generated host test target.

    Args:
      name: Name of the system test suite target.
      environments: List of environment dicts or fx_test_environment labels.
      builds: Optional list of build dicts with release_builder or release_build_id.
      downgrade_release_builder: Optional downgrade release builder name.
      downgrade_release_build_id: Optional downgrade release build ID.
      downgrade_ota_attempts: Optional number of downgrade OTA attempts.
      upgrade_release_builder: Optional upgrade release builder name.
      upgrade_release_build_id: Optional upgrade release build ID.
      installer: Optional OTA installer name.
      service_account: Optional service account for artifact downloads.
      timeout: Optional test timeout duration string.
      pave_timeout: Optional pave timeout duration string.
      cycle_timeout: Optional cycle timeout duration string.
      cycle_count: Optional number of test cycles.
      device_connect_timeout: Optional device connect timeout duration string.
      workaround_broken_time_skip: Whether to enable broken time skip workaround.
      workaround_downgrade_ota_no_rewrite_rules: Whether to skip rewrite rules on downgrade OTA.
      require_cup: Whether mock-omaha-server requires CUP.
      use_flash: Whether to use ffx flash instead of paving.
      build_expect_unknown_firmware: Whether to expect unknown firmware during build.
      max_system_image_size: Optional maximum system image size.
      max_update_images_size: Optional maximum update images size.
      max_update_package_size: Optional maximum update package size.
      bootfs_compression: Optional bootfs compression setting.
      check_abr: Whether to check ABR slot after boot.
      resolver_mode: Optional resolver mode string.
      extra_args: Additional CLI flags passed to the test binary.
      test_data: Optional replacement list of base runtime test_data labels.
      extra_test_data: Additional runtime test_data labels.
      tags: Additional Bazel tags applied to generated targets.
      visibility: Visibility applied to generated targets.
    """
    if downgrade_release_builder != None and downgrade_release_build_id != None:
        fail("downgrade_release_builder and downgrade_release_build_id are mutually exclusive for {}".format(name))
    if upgrade_release_builder != None and upgrade_release_build_id != None:
        fail("upgrade_release_builder and upgrade_release_build_id are mutually exclusive for {}".format(name))
    if builds != None and downgrade_release_builder != None:
        fail("builds is mutually exclusive with downgrade_release_builder for {}".format(name))
    if builds != None and downgrade_release_build_id != None:
        fail("builds is mutually exclusive with downgrade_release_build_id for {}".format(name))
    if builds != None and upgrade_release_builder != None:
        fail("builds is mutually exclusive with upgrade_release_builder for {}".format(name))
    if builds != None and upgrade_release_build_id != None:
        fail("builds is mutually exclusive with upgrade_release_build_id for {}".format(name))

    resolved_builds = []
    if builds != None:
        for build in builds:
            if type(build) != "dict":
                fail("each entry in builds must be a dict for {}".format(name))
            has_builder = build.get("release_builder") != None
            has_build_id = build.get("release_build_id") != None
            if not has_builder and not has_build_id:
                fail("release_builder or release_build_id must be defined for {}".format(name))
            if has_builder and has_build_id:
                fail("release_builder and release_build_id are mutually exclusive for {}".format(name))
            resolved_builds.append(build)
    else:
        if downgrade_release_builder != None:
            resolved_builds.append({"release_builder": downgrade_release_builder})
        elif downgrade_release_build_id != None:
            resolved_builds.append({"release_build_id": downgrade_release_build_id})

        if upgrade_release_builder != None:
            resolved_builds.append({"release_builder": upgrade_release_builder})
        elif upgrade_release_build_id != None:
            resolved_builds.append({"release_build_id": upgrade_release_build_id})

    if not resolved_builds:
        fail("upgrade builds must be specified for {}".format(name))

    host_test_target_name = "{}_system_tests_upgrade".format(name)
    args = []

    for build in resolved_builds:
        if build.get("release_build_id") != None:
            args.extend([
                "--build-id",
                _require_non_empty_str(build["release_build_id"], "release_build_id", host_test_target_name),
            ])
        if build.get("release_builder") != None:
            args.extend([
                "--builder-name",
                _require_non_empty_str(build["release_builder"], "release_builder", host_test_target_name),
            ])

    if downgrade_ota_attempts != None:
        args.extend([
            "-downgrade-ota-attempts",
            _require_non_empty_str(
                downgrade_ota_attempts,
                "downgrade_ota_attempts",
                host_test_target_name,
                allow_int = True,
            ),
        ])

    if pave_timeout != None:
        args.extend([
            "-pave-timeout",
            _require_non_empty_str(pave_timeout, "pave timeout", host_test_target_name),
        ])

    if cycle_timeout != None:
        args.extend([
            "-cycle-timeout",
            _require_non_empty_str(cycle_timeout, "cycle timeout", host_test_target_name),
        ])

    if cycle_count != None:
        args.extend([
            "-cycle-count",
            _require_non_empty_str(cycle_count, "cycle count", host_test_target_name, allow_int = True),
        ])

    if installer != None:
        args.extend([
            "-installer",
            _require_non_empty_str(installer, "installer", host_test_target_name),
        ])

    if device_connect_timeout != None:
        args.extend([
            "-device-connect-timeout",
            _require_non_empty_str(device_connect_timeout, "device connect timeout", host_test_target_name),
        ])

    if workaround_broken_time_skip:
        args.append("-workaround-broken-time-skip")

    if workaround_downgrade_ota_no_rewrite_rules:
        args.append("-workaround-downgrade-ota-no-rewrite-rules")

    if require_cup:
        args.append("-require-cup")

    if use_flash:
        args.append("-use-flash")

    if build_expect_unknown_firmware:
        args.append("-build-expect-unknown-firmware")

    if max_system_image_size != None:
        args.extend([
            "-max-system-image-size",
            _require_non_empty_str(
                max_system_image_size,
                "max_system_image_size",
                host_test_target_name,
                allow_int = True,
            ),
        ])

    if max_update_images_size != None:
        args.extend([
            "-max-update-images-size",
            _require_non_empty_str(
                max_update_images_size,
                "max_update_images_size",
                host_test_target_name,
                allow_int = True,
            ),
        ])

    if max_update_package_size != None:
        args.extend([
            "-max-update-package-size",
            _require_non_empty_str(
                max_update_package_size,
                "max_update_package_size",
                host_test_target_name,
                allow_int = True,
            ),
        ])

    if bootfs_compression != None:
        args.extend([
            "-bootfs-compression",
            _require_non_empty_str(bootfs_compression, "bootfs_compression", host_test_target_name),
        ])

    if check_abr:
        args.append("--check-abr=true")
    else:
        args.append("--check-abr=false")

    if resolver_mode != None:
        args.append("--resolver-mode=" + _require_non_empty_str(resolver_mode, "resolver_mode", host_test_target_name))
    else:
        args.append("--resolver-mode=constant")

    args.extend(extra_args)

    _define_system_test_suite(
        name = name,
        test_binary_name = "system_tests_upgrade",
        test_binary_data = "//src/sys/pkg/tests/system-tests/upgrade_test:system_tests_upgrade.test_data",
        args = args,
        environments = environments,
        service_account = service_account,
        timeout = timeout,
        test_data = test_data,
        extra_test_data = extra_test_data,
        tags = tags,
        visibility = visibility,
    )

def system_test_reboot_suite(
        name,
        environments,
        release_builder = None,
        release_build_id = None,
        service_account = None,
        timeout = None,
        pave_timeout = None,
        cycle_timeout = None,
        cycle_count = None,
        device_connect_timeout = None,
        sleep_after_reboot = None,
        use_flash = False,
        check_abr = True,
        resolver_mode = None,
        extra_args = [],
        test_data = None,
        extra_test_data = [],
        tags = [],
        visibility = None):
    """Configures a System Test Reboot Suite (Bazel port of system_test_reboot_suite in system-tests.gni).

    Generates:
      - `<name>_system_tests_reboot_metadata`: JSON metadata & SystemTestSuiteMetadataInfo provider.
      - `<name>_system_tests_reboot`: `host_test` wrapping `system_tests_reboot` via `go_test_parser`.
      - `<name>`: `test_suite` grouping the generated host test target.

    Args:
      name: Name of the system test suite target.
      environments: List of environment dicts or fx_test_environment labels.
      release_builder: Optional release builder name.
      release_build_id: Optional release build ID.
      service_account: Optional service account for artifact downloads.
      timeout: Optional test timeout duration string.
      pave_timeout: Optional pave timeout duration string.
      cycle_timeout: Optional cycle timeout duration string.
      cycle_count: Optional number of test cycles.
      device_connect_timeout: Optional device connect timeout duration string.
      sleep_after_reboot: Optional duration to sleep after reboot.
      use_flash: Whether to use ffx flash instead of paving.
      check_abr: Whether to check ABR slot after boot.
      resolver_mode: Optional resolver mode string.
      extra_args: Additional CLI flags passed to the test binary.
      test_data: Optional replacement list of base runtime test_data labels.
      extra_test_data: Additional runtime test_data labels.
      tags: Additional Bazel tags applied to generated targets.
      visibility: Visibility applied to generated targets.
    """
    if release_builder != None and release_build_id != None:
        fail("release_builder and release_build_id are mutually exclusive for {}".format(name))

    host_test_target_name = "{}_system_tests_reboot".format(name)
    args = []

    if release_builder != None:
        args.extend([
            "-builder-name",
            _require_non_empty_str(release_builder, "release_builder", host_test_target_name),
        ])

    if release_build_id != None:
        args.extend([
            "-build-id",
            _require_non_empty_str(release_build_id, "release_build_id", host_test_target_name),
        ])

    if pave_timeout != None:
        args.extend([
            "-pave-timeout",
            _require_non_empty_str(pave_timeout, "pave timeout", host_test_target_name),
        ])

    if cycle_timeout != None:
        args.extend([
            "-cycle-timeout",
            _require_non_empty_str(cycle_timeout, "cycle timeout", host_test_target_name),
        ])

    if cycle_count != None:
        args.extend([
            "-cycle-count",
            _require_non_empty_str(cycle_count, "cycle count", host_test_target_name, allow_int = True),
        ])

    if device_connect_timeout != None:
        args.extend([
            "-device-connect-timeout",
            _require_non_empty_str(device_connect_timeout, "device connect timeout", host_test_target_name),
        ])

    if sleep_after_reboot != None:
        args.extend([
            "-sleep-after-reboot",
            _require_non_empty_str(sleep_after_reboot, "sleep after reboot", host_test_target_name),
        ])

    if use_flash:
        args.append("-use-flash")

    if check_abr:
        args.append("--check-abr=true")
    else:
        args.append("--check-abr=false")

    if resolver_mode != None:
        args.append("--resolver-mode=" + _require_non_empty_str(resolver_mode, "resolver_mode", host_test_target_name))

    args.extend(extra_args)

    _define_system_test_suite(
        name = name,
        test_binary_name = "system_tests_reboot",
        test_binary_data = "//src/sys/pkg/tests/system-tests/reboot_test:system_tests_reboot.test_data",
        args = args,
        environments = environments,
        service_account = service_account,
        timeout = timeout,
        test_data = test_data,
        extra_test_data = extra_test_data,
        tags = tags,
        visibility = visibility,
    )

def system_test_recovery_suite(
        name,
        environments,
        release_builder = None,
        release_build_id = None,
        service_account = None,
        timeout = None,
        pave_timeout = None,
        cycle_timeout = None,
        cycle_count = None,
        device_connect_timeout = None,
        use_flash = False,
        check_abr = True,
        resolver_mode = None,
        extra_args = [],
        test_data = None,
        extra_test_data = [],
        tags = [],
        visibility = None):
    """Configures a System Test Recovery Suite (Bazel port of system_test_recovery_suite in system-tests.gni).

    Generates:
      - `<name>_system_tests_recovery_metadata`: JSON metadata & SystemTestSuiteMetadataInfo provider.
      - `<name>_system_tests_recovery`: `host_test` wrapping `system_tests_recovery` via `go_test_parser`.
      - `<name>`: `test_suite` grouping the generated host test target.

    Args:
      name: Name of the system test suite target.
      environments: List of environment dicts or fx_test_environment labels.
      release_builder: Optional release builder name.
      release_build_id: Optional release build ID.
      service_account: Optional service account for artifact downloads.
      timeout: Optional test timeout duration string.
      pave_timeout: Optional pave timeout duration string.
      cycle_timeout: Optional cycle timeout duration string.
      cycle_count: Optional number of test cycles.
      device_connect_timeout: Optional device connect timeout duration string.
      use_flash: Whether to use ffx flash instead of paving.
      check_abr: Whether to check ABR slot after boot.
      resolver_mode: Optional resolver mode string.
      extra_args: Additional CLI flags passed to the test binary.
      test_data: Optional replacement list of base runtime test_data labels.
      extra_test_data: Additional runtime test_data labels.
      tags: Additional Bazel tags applied to generated targets.
      visibility: Visibility applied to generated targets.
    """
    if release_builder != None and release_build_id != None:
        fail("release_builder and release_build_id are mutually exclusive for {}".format(name))

    host_test_target_name = "{}_system_tests_recovery".format(name)
    args = []

    if release_builder != None:
        args.extend([
            "-builder-name",
            _require_non_empty_str(release_builder, "release_builder", host_test_target_name),
        ])

    if release_build_id != None:
        args.extend([
            "-build-id",
            _require_non_empty_str(release_build_id, "release_build_id", host_test_target_name),
        ])

    if pave_timeout != None:
        args.extend([
            "-pave-timeout",
            _require_non_empty_str(pave_timeout, "pave timeout", host_test_target_name),
        ])

    if cycle_timeout != None:
        args.extend([
            "-cycle-timeout",
            _require_non_empty_str(cycle_timeout, "cycle timeout", host_test_target_name),
        ])

    if cycle_count != None:
        args.extend([
            "-cycle-count",
            _require_non_empty_str(cycle_count, "cycle count", host_test_target_name, allow_int = True),
        ])

    if device_connect_timeout != None:
        args.extend([
            "-device-connect-timeout",
            _require_non_empty_str(device_connect_timeout, "device connect timeout", host_test_target_name),
        ])

    if use_flash:
        args.append("-use-flash")

    if check_abr:
        args.append("--check-abr=true")
    else:
        args.append("--check-abr=false")

    if resolver_mode != None:
        args.append("--resolver-mode=" + _require_non_empty_str(resolver_mode, "resolver_mode", host_test_target_name))

    args.extend(extra_args)

    _define_system_test_suite(
        name = name,
        test_binary_name = "system_tests_recovery",
        test_binary_data = "//src/sys/pkg/tests/system-tests/recovery_test:system_tests_recovery.test_data",
        args = args,
        environments = environments,
        service_account = service_account,
        timeout = timeout,
        test_data = test_data,
        extra_test_data = extra_test_data,
        tags = tags,
        visibility = visibility,
    )
