# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verification rules for `fx_bootfs_files_package()` tests."""

load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")
load(
    "//build/bazel/debug_symbols:aspects.bzl",
    "DebugSymbolManifestInfo",
    "collect_debug_symbols_manifest_aspect",
)

visibility("private")

def _verify_bootfs_files_package_impl(ctx):
    pkg_info = ctx.attr.package[FuchsiaPackageInfo]
    if pkg_info.package_name != ctx.attr.expected_package_name:
        fail("Expected package_name %r, got %r" % (
            ctx.attr.expected_package_name,
            pkg_info.package_name,
        ))

    actual_dests = sorted([r.dest for r in pkg_info.package_resources])
    for expected_dest in ctx.attr.expected_dests:
        if expected_dest not in actual_dests:
            fail("Expected %r in package_resources of %s, got %s" % (
                expected_dest,
                ctx.attr.package.label,
                actual_dests,
            ))

    for forbidden_dest in ctx.attr.forbidden_dests:
        if forbidden_dest in actual_dests:
            fail("Did not expect %r in package_resources of %s, got %s" % (
                forbidden_dest,
                ctx.attr.package.label,
                actual_dests,
            ))

    for prefix in ctx.attr.expected_dest_prefixes:
        if not any([d.startswith(prefix) for d in actual_dests]):
            fail("Expected a resource starting with %r in %s, got %s" % (
                prefix,
                ctx.attr.package.label,
                actual_dests,
            ))

    if ctx.attr.expected_debug_dest_paths:
        debug_info = ctx.attr.package[DebugSymbolManifestInfo]
        actual_debug_dests = {
            f.basename: dest
            for f, dest in debug_info.dest_paths.to_list()
            if not f.owner.repo_name
        }
        for basename, expected_dest in ctx.attr.expected_debug_dest_paths.items():
            if actual_debug_dests.get(basename) != expected_dest:
                fail("Expected DebugSymbolManifestInfo dest_path[%r] == %r, got %s" % (
                    basename,
                    expected_dest,
                    actual_debug_dests,
                ))

        unstripped_basenames = [f.basename for f in debug_info.unstripped_binaries.to_list()]
        for basename in ctx.attr.expected_debug_dest_paths.keys():
            if basename not in unstripped_basenames:
                fail("Expected %r in DebugSymbolManifestInfo.unstripped_binaries, got %s" % (
                    basename,
                    unstripped_basenames,
                ))

    # Also verify the generated `package_manifest.json` emitted by `package-tool`
    # lists every expected BootFS blob directly in `blobs` (not buried inside
    # `meta.far`) and omits every forbidden blob path. Use pure bash built-ins
    # rather than `python3` so the action runs hermetically in the RBE/CQ
    # linux-sandbox without external tool dependencies.
    stamp = ctx.actions.declare_file(ctx.label.name + ".stamp")
    checks = [
        "set -euo pipefail",
        'manifest=$(< "$1")',
    ]
    for expected in ctx.attr.expected_blob_paths:
        needle = '"path": "%s"' % expected
        checks.append(
            'if [[ "$manifest" != *%s* ]]; then echo "Missing expected blob %s in $1" >&2; exit 1; fi' % (
                repr(needle),
                expected,
            ),
        )
    for forbidden in ctx.attr.forbidden_dests:
        needle = '"path": "%s"' % forbidden
        checks.append(
            'if [[ "$manifest" == *%s* ]]; then echo "Found forbidden blob %s in $1" >&2; exit 1; fi' % (
                repr(needle),
                forbidden,
            ),
        )
    checks.append('printf "ok\\n" > "$2"')
    ctx.actions.run_shell(
        inputs = [pkg_info.package_manifest],
        outputs = [stamp],
        command = "\n".join(checks),
        arguments = [
            pkg_info.package_manifest.path,
            stamp.path,
        ],
        mnemonic = "VerifyBootfsPackageManifest",
    )

    return [DefaultInfo(files = depset([stamp]))]

verify_bootfs_files_package = rule(
    doc = "Verifies `FuchsiaPackageInfo`, `package_manifest.json` blobs, and `DebugSymbolManifestInfo` of an `fx_bootfs_files_package()` target.",
    implementation = _verify_bootfs_files_package_impl,
    attrs = {
        "package": attr.label(
            providers = [FuchsiaPackageInfo],
            aspects = [collect_debug_symbols_manifest_aspect],
            mandatory = True,
        ),
        "expected_package_name": attr.string(mandatory = True),
        "expected_dests": attr.string_list(),
        "expected_blob_paths": attr.string_list(),
        "expected_dest_prefixes": attr.string_list(),
        "forbidden_dests": attr.string_list(),
        "expected_debug_dest_paths": attr.string_dict(),
    },
)
