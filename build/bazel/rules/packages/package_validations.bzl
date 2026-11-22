# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Build-time validations of platform Fuchsia packages.

These match the validations that GN's `fuchsia_package()` runs through
//build/components/package_creation_wrapper.py:

- ELF verification (opt out with `validate_elf_binaries = False` in Bazel,
  matching `disable_elf_binaries_checks = true` in GN).
- Structured config validation (opt out with `validate_structured_config =
  False` in both GN and Bazel).

The same functions create the validation actions of `fx_package()` and the
actions of the tests that check that invalid packages are rejected, so that
the tests exercise the same command lines that packages use.
"""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")

visibility(["//build/bazel/rules/packages/..."])

VERIFY_ELF_BINARIES_MNEMONIC = "FuchsiaPackageVerifyElfBinaries"
VALIDATE_STRUCTURED_CONFIG_MNEMONIC = "FuchsiaPackageValidateStructuredConfig"

FuchsiaPackageValidationInfo = provider(
    doc = "Inputs used to validate an `fx_package()`, exposed so tests can run the exact same validation action.",
    fields = {
        "prebuilt_unstripped_files": "List of unstripped Files for prebuilt stripped binaries in the package.",
    },
)

def _output_name(ctx, validation, expect_failure):
    return "{}_validations/{}{}.stamp".format(
        ctx.label.name,
        validation,
        "_expected_failure" if expect_failure != None else "",
    )

def _common_args(ctx, stamp, expect_failure):
    args = ctx.actions.args()
    args.add("--label", str(ctx.label))
    args.add("--stamp", stamp)
    if expect_failure != None:
        args.add("--expect-failure", expect_failure)
    return args

def validate_package_elf_binaries(
        ctx,
        validate_package_tool,
        package_info,
        debug_symbol_info,
        prebuilt_unstripped_files,
        expect_failure = None):
    """Creates an action that verifies the ELF binaries of a package.

    Every ELF binary outside of `data/`, `lib/firmware/` and `meta/` must find
    its `DT_NEEDED` libraries in the package, must be stripped, and must have
    a debug file in one of the package's `.build-id` directories (unless it is
    a prebuilt). See //build/python/modules/elf/verify_manifest_elf_binaries.py.

    Args:
        ctx: The rule context.
        validate_package_tool: The `validate_package` executable.
        package_info: The `FuchsiaPackageInfo` of the package.
        debug_symbol_info: The `FuchsiaDebugSymbolInfo` of the package. Its
          `.build-id` directories include those of the Clang and Rust
          toolchains, which hold the debug files of their runtime libraries.
        prebuilt_unstripped_files: The unstripped files of the binaries in
          the package that come with a prebuilt stripped file (e.g. ld.so.1
          from the sysroot), which are not in any `.build-id` directory.
        expect_failure: If not None, the action succeeds only if the
          verification fails with this message.

    Returns:
        The stamp file written by the action.
    """
    stamp = ctx.actions.declare_file(_output_name(ctx, "elf_binaries", expect_failure))
    fini_manifest = ctx.actions.declare_file(stamp.basename.removesuffix(".stamp") + ".fini", sibling = stamp)
    resources = sorted(package_info.package_resources, key = lambda r: r.dest)
    ctx.actions.write(
        fini_manifest,
        "".join(["{}={}\n".format(r.dest, r.src.path) for r in resources]),
    )
    build_id_dirs = depset(transitive = debug_symbol_info.build_id_dirs_mapping.values())

    args = _common_args(ctx, stamp, expect_failure)
    args.add("elf")
    args.add("--fini-manifest", fini_manifest)
    args.add_all(build_id_dirs, before_each = "--build-id-dir", expand_directories = False)
    args.add_all(prebuilt_unstripped_files, before_each = "--debug-file")

    ctx.actions.run(
        executable = validate_package_tool,
        arguments = [args],
        inputs = depset(
            [fini_manifest] + [r.src for r in resources] + prebuilt_unstripped_files,
            transitive = [build_id_dirs],
        ),
        outputs = [stamp],
        mnemonic = VERIFY_ELF_BINARIES_MNEMONIC,
        progress_message = "Verifying ELF binaries in package %{label}",
        # The toolchain .build-id directories are large source directories,
        # which are cheap to symlink into a local sandbox, but expensive to
        # upload for remote execution or hash for RBE cache lookups (which
        # never hit under --noremote_upload_local_results).
        **LOCAL_ONLY_ACTION_KWARGS
    )
    return stamp

def validate_package_structured_config(
        ctx,
        validate_package_tool,
        configc,
        package_info,
        expect_failure = None):
    """Creates an action that runs `configc validate-package` on a package.

    This checks that every component of the package that declares structured
    config has a matching configuration value file in the package.

    Args:
        ctx: The rule context.
        validate_package_tool: The `validate_package` executable.
        configc: The `configc` executable.
        package_info: The `FuchsiaPackageInfo` of the package.
        expect_failure: If not None, the action succeeds only if the
          validation fails with this message.

    Returns:
        The stamp file written by the action.
    """
    stamp = ctx.actions.declare_file(_output_name(ctx, "structured_config", expect_failure))

    args = _common_args(ctx, stamp, expect_failure)
    args.add("structured-config")
    args.add("--configc", configc)
    args.add("--package-manifest", package_info.package_manifest)

    ctx.actions.run(
        executable = validate_package_tool,
        arguments = [args],
        # `configc validate-package` only reads the component manifests and
        # configuration value files from the package's meta.far.
        inputs = [package_info.package_manifest, package_info.meta_far],
        tools = [configc],
        outputs = [stamp],
        mnemonic = VALIDATE_STRUCTURED_CONFIG_MNEMONIC,
        progress_message = "Validating structured config in package %{label}",
    )
    return stamp
