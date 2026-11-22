# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""third_party_icu_headers() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/icu:icu_names.bzl", "icu_flavored_name")

visibility(["//..."])

def _generate_icu_header_impl(ctx):
    out = ctx.outputs.out
    lines = [
        "#ifndef FUCHSIA_ICU_VARIANT",
        '#error "Expected FUCHSIA_ICU_VARIANT to be defined."',
        "#elif FUCHSIA_ICU_VARIANT == 0",
    ]
    for h in ctx.attr.headers:
        lines.append('#include "%s"' % h.replace("third_party/icu/", "third_party/icu/default/"))
    lines.append("#elif FUCHSIA_ICU_VARIANT == 1")
    for h in ctx.attr.headers:
        lines.append('#include "%s"' % h.replace("third_party/icu/", "third_party/icu/stable/"))
    lines.append("#elif FUCHSIA_ICU_VARIANT == 2")
    for h in ctx.attr.headers:
        lines.append('#include "%s"' % h.replace("third_party/icu/", "third_party/icu/latest/"))
    lines.append("#elif FUCHSIA_ICU_VARIANT == 3")
    for h in ctx.attr.headers:
        lines.append('#include "%s"' % h.replace("third_party/icu/", "third_party/icu/default/"))
    lines.extend([
        "#else",
        '#error "Unexpected FUCHSIA_ICU_VARIANT value."',
        "#endif",
        "",
    ])
    ctx.actions.write(
        output = out,
        content = "\n".join(lines),
    )
    return [DefaultInfo(files = depset([out]))]

_generate_icu_header = rule(
    implementation = _generate_icu_header_impl,
    attrs = {
        "headers": attr.string_list(mandatory = True, allow_empty = False),
        "out": attr.output(mandatory = True),
    },
)

def third_party_icu_headers(
        *,
        name,
        headers,
        icu_assembly = False,  # buildifier: disable=unused-variable - For GN conversion only.
        testonly = None,
        visibility = None):
    """Generates a C++ header wrapper that dispatches to the configured ICU flavor's headers.

    This is the Bazel counterpart of the GN `third_party_icu_headers()` template defined in
    `//build/icu/third_party_icu_headers.gni`.

    Args:
        name: Target name (generates `<name>.h`).
        headers: List of ICU headers to include, prefixed with `"third_party/icu/"`.
        icu_assembly: Forwarded to GN `third_party_icu_headers()` by `bazel2gn`.
        testonly: Standard Bazel `testonly` attribute.
        visibility: Standard Bazel `visibility` attribute.
    """
    gen_name = "%s_gen" % name
    header_out = "%s.h" % name
    _generate_icu_header(
        name = gen_name,
        headers = headers,
        out = header_out,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    for icu_flavor in icu_flavors:
        variant_num = "0" if icu_flavor == "default" else "2"
        fx_cc_library(
            name = icu_flavored_name(name, icu_flavor),
            hdrs = [":%s" % header_out],
            defines = ["FUCHSIA_ICU_VARIANT=%s" % variant_num],
            deps = ["//src/lib/icu:icu_%s" % icu_flavor],
            testonly = testonly,
            visibility = visibility,
        )

    fx_cc_library(
        name = name,
        hdrs = [":%s" % header_out],
        defines = ["FUCHSIA_ICU_VARIANT=3"],
        deps = [
            "//src/lib/icu:data",
            "//src/lib/icu:lib",
        ],
        testonly = testonly,
        visibility = visibility,
    )
