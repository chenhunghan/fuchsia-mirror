# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""intl_strings() macro definition."""

load("@fuchsia_rules_common//:utils.bzl", "make_resource_struct")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageResourcesInfo")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")

visibility(["//..."])

def _intl_strings_to_json_impl(ctx):
    out = ctx.outputs.out
    args = [
        "--replace-missing-with-warning",
        "--source-locale",
        ctx.attr.source_locale,
        "--source-strings-file",
        ctx.file.source_strings_file.path,
        "--target-locale",
        ctx.attr.target_locale,
        "--target-strings-file",
        ctx.file.target_strings_file.path,
        "--output",
        out.path,
    ]
    ctx.actions.run(
        outputs = [out],
        inputs = depset([ctx.file.source_strings_file, ctx.file.target_strings_file]),
        executable = ctx.executable._tool,
        arguments = args,
        env = {"RUST_BACKTRACE": "1"},
        mnemonic = "IntlStringsToJson",
        progress_message = "Generating intl JSON for locale %s in %s" % (ctx.attr.target_locale, ctx.label),
    )
    return [
        DefaultInfo(
            files = depset([out]),
            runfiles = ctx.runfiles(files = [out]),
        ),
        FuchsiaPackageResourcesInfo(
            resources = [
                make_resource_struct(
                    src = out,
                    dest = "assets/locales/%s/%s.json" % (ctx.attr.target_locale, ctx.attr.base_name),
                ),
            ],
        ),
        CcInfo(),
    ]

_intl_strings_to_json = rule(
    implementation = _intl_strings_to_json_impl,
    attrs = {
        "base_name": attr.string(mandatory = True),
        "out": attr.output(mandatory = True),
        "source_locale": attr.string(mandatory = True),
        "source_strings_file": attr.label(allow_single_file = True, mandatory = True),
        "target_locale": attr.string(mandatory = True),
        "target_strings_file": attr.label(allow_single_file = True, mandatory = True),
        "_tool": attr.label(
            default = "//src/intl/strings_to_json:strings_to_json_bin",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _intl_strings_to_fidl_impl(ctx):
    out = ctx.outputs.out
    args = [
        "--input",
        ctx.file.src.path,
        "--output",
        out.path,
        "--library",
        ctx.attr.library,
    ]
    ctx.actions.run(
        outputs = [out],
        inputs = depset([ctx.file.src]),
        executable = ctx.executable._tool,
        arguments = args,
        env = {"RUST_BACKTRACE": "1"},
        mnemonic = "IntlStringsToFidl",
        progress_message = "Generating intl FIDL %s in %s" % (ctx.attr.library, ctx.label),
    )
    return [
        DefaultInfo(
            files = depset([out]),
            runfiles = ctx.runfiles(files = [out]),
        ),
    ]

_intl_strings_to_fidl = rule(
    implementation = _intl_strings_to_fidl_impl,
    attrs = {
        "library": attr.string(mandatory = True),
        "out": attr.output(mandatory = True),
        "src": attr.label(allow_single_file = True, mandatory = True),
        "_tool": attr.label(
            default = "//src/intl/strings_to_fidl:strings_to_fidl_bin",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _intl_strings_assets_impl(ctx):
    files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps])
    resources = []
    for d in ctx.attr.deps:
        if FuchsiaPackageResourcesInfo in d:
            resources.extend(d[FuchsiaPackageResourcesInfo].resources)
    return [
        DefaultInfo(
            files = files,
            runfiles = ctx.runfiles(transitive_files = files),
        ),
        FuchsiaPackageResourcesInfo(resources = resources),
        CcInfo(),
    ]

_intl_strings_assets = rule(
    implementation = _intl_strings_assets_impl,
    attrs = {
        "deps": attr.label_list(mandatory = True),
    },
)

def intl_strings(
        *,
        name,
        source,
        output_locales,
        library,
        source_locale = "en-US",
        same_dir_locales = False,
        testonly = False,
        visibility = None):
    """Generates localized string assets and a FIDL library of message IDs from an XML dictionary.

    This is the Bazel counterpart of the GN `intl_strings()` template defined in
    `//build/intl/intl_strings.gni`.

    Args:
        name: Target name.
        source: Source XML strings file (e.g. `"strings.xml"`).
        output_locales: List of locale identifiers to generate JSON assets for.
        library: Name of the generated FIDL library (e.g. `"fuchsia.intl.l10n"`).
        source_locale: Source locale identifier (defaults to `"en-US"`).
        same_dir_locales: If `True`, look for `"<base>_<locale>.xml"` in the same directory as
            `source` instead of `"<locale>/strings.xml"`.
        testonly: Standard Bazel `testonly` attribute.
        visibility: Standard Bazel `visibility` attribute.
    """
    source_clean = source.removeprefix(":")
    if "/" in source_clean:
        source_dir, _, source_filename = source_clean.rpartition("/")
    else:
        source_dir = ""
        source_filename = source_clean

    if "." in source_filename:
        base_source, _, _ = source_filename.rpartition(".")
    else:
        base_source = source_filename

    locale_targets = []
    for locale in output_locales:
        if same_dir_locales:
            locale_file = ("%s/%s_%s.xml" % (source_dir, base_source, locale)) if source_dir else ("%s_%s.xml" % (base_source, locale))
        else:
            locale_file = ("%s/%s/%s.xml" % (source_dir, locale, base_source)) if source_dir else ("%s/%s.xml" % (locale, base_source))

        locale_target = "%s_to_json_%s" % (name, locale)
        locale_targets.append(":%s" % locale_target)
        _intl_strings_to_json(
            name = locale_target,
            base_name = name,
            out = "%s/%s.json" % (locale, name),
            source_locale = source_locale,
            source_strings_file = source,
            target_locale = locale,
            target_strings_file = locale_file,
            testonly = testonly,
            visibility = visibility,
        )

    l10n_assets_target = "%s_l10n_assets" % name
    _intl_strings_assets(
        name = l10n_assets_target,
        deps = locale_targets,
        testonly = testonly,
        visibility = visibility,
    )

    fidl_gen_target = "%s_fidl" % name
    _intl_strings_to_fidl(
        name = fidl_gen_target,
        src = source,
        out = "%s.fidl" % base_source,
        library = library,
        testonly = testonly,
        visibility = visibility,
    )

    fidl_library(
        name = library,
        srcs = [":%s" % fidl_gen_target],
        enable_hlcpp = True,
        excluded_checks = ["invalid-copyright-for-platform-source-library"],
        versioned = "fuchsia:HEAD" if (testonly and library.startswith("fuchsia.")) else None,
        testonly = testonly,
        visibility = visibility,
    )

    _intl_strings_assets(
        name = name,
        deps = [
            ":%s" % l10n_assets_target,
            ":%s" % fidl_gen_target,
        ],
        testonly = testonly,
        visibility = visibility,
    )
