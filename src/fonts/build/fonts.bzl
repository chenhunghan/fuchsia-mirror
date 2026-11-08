# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel rules and macros for Fuchsia font bundles and collections."""

load("@fuchsia_build_info//:args.bzl", "compilation_mode", "include_internal_fonts")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageResourcesInfo")
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "package_resources_providers",
    "resource",
)
load(
    "//build/bazel/rules:cipd_prebuilt.bzl",
    "cipd_prebuilt_directory",
    "cipd_prebuilt_file",
)
load("//build/bazel/rules/components:fx_component.bzl", "fx_component")
load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")

visibility([
    "//src/fonts/...",
    "//vendor/google/...",
])

# Path relative to component's `/pkg` dir where font assets are stored.
_ASSETS_RELATIVE_PATH = "data/assets"

# Default prefix for the manifest file name in /config/data.
_DEFAULT_MANIFEST_PREFIX = "all"

_FONT_MANIFEST_GENERATOR = "@gn_targets//toolchain_host_x64/src/fonts/tools/manifest_generator:font_manifest_generator"

# Mapping from font file_name to path_prefix from
# //prebuilt/third_party/fonts/fuchsia.font_pkgs.json.
FONT_PKG_PATHS = {
    "AlegreyaSansSC-Bold.ttf": "alegreyasanssc/",
    "AlegreyaSansSC-BoldItalic.ttf": "alegreyasanssc/",
    "AlegreyaSansSC-Italic.ttf": "alegreyasanssc/",
    "AlegreyaSansSC-Regular.ttf": "alegreyasanssc/",
    "Arimo-Bold.ttf": "arimo/",
    "Arimo-BoldItalic.ttf": "arimo/",
    "Arimo-Italic.ttf": "arimo/",
    "Arimo-Regular.ttf": "arimo/",
    "Cousine-Bold.ttf": "cousine/",
    "Cousine-BoldItalic.ttf": "cousine/",
    "Cousine-Italic.ttf": "cousine/",
    "Cousine-Regular.ttf": "cousine/",
    "CutiveMono-Regular.ttf": "cutivemono/",
    "DroidSans-Bold.ttf": "droidsans/",
    "DroidSans-Regular.ttf": "droidsans/",
    "DroidSansMono-Regular.ttf": "droidsansmono/",
    "DroidSerif-Bold.ttf": "droidserif/",
    "DroidSerif-BoldItalic.ttf": "droidserif/",
    "DroidSerif-Italic.ttf": "droidserif/",
    "DroidSerif-Regular.ttf": "droidserif/",
    "MTLc3m.ttf": "motoya-l-cedar/",
    "MaterialIcons-Regular.ttf": "material/",
    "NanumGothic-Regular.ttf": "nanumgothic/",
    "NotoColorEmoji.ttf": "noto-emoji/",
    "NotoColorEmoji.v121.ttf": "noto-emoji/",
    "NotoNaskhArabicUI-Regular.ttf": "notonaskhui/",
    "NotoSansArmenian-Regular.ttf": "notosans/",
    "NotoSansBengaliUI-Regular.ttf": "notosansui/",
    "NotoSansCJK-Black.ttc": "noto-cjk/",
    "NotoSansCJK-Bold.ttc": "noto-cjk/",
    "NotoSansCJK-DemiLight.ttc": "noto-cjk/",
    "NotoSansCJK-Light.ttc": "noto-cjk/",
    "NotoSansCJK-Medium.ttc": "noto-cjk/",
    "NotoSansCJK-Regular.ttc": "noto-cjk/",
    "NotoSansCJK-Thin.ttc": "noto-cjk/",
    "NotoSansDevanagariUI-Regular.ttf": "notosansui/",
    "NotoSansEthiopic-Regular.ttf": "notosans/",
    "NotoSansGeorgian-Regular.ttf": "notosans/",
    "NotoSansGujaratiUI-Regular.ttf": "notosansui/",
    "NotoSansHebrew-Regular.ttf": "notosans/",
    "NotoSansKannada-Regular.ttf": "notosans/",
    "NotoSansMalayalamUI-Regular.ttf": "notosansui/",
    "NotoSansSymbols-Regular.ttf": "notosans/",
    "NotoSansSymbols2-Regular.ttf": "notosans/",
    "NotoSansTamilUI-Regular.ttf": "notosansui/",
    "NotoSansTelugu-Regular.ttf": "notosans/",
    "NotoSansThaiUI-Regular.ttf": "notosansui/",
    "NotoSerifCJK-Black.ttc": "noto-cjk/",
    "NotoSerifCJK-Bold.ttc": "noto-cjk/",
    "NotoSerifCJK-ExtraLight.ttc": "noto-cjk/",
    "NotoSerifCJK-Light.ttc": "noto-cjk/",
    "NotoSerifCJK-Medium.ttc": "noto-cjk/",
    "NotoSerifCJK-Regular.ttc": "noto-cjk/",
    "NotoSerifCJK-SemiBold.ttc": "noto-cjk/",
    "Quintessential-Regular.ttf": "quintessential/",
    "Roboto-Black.ttf": "roboto/",
    "Roboto-BlackItalic.ttf": "roboto/",
    "Roboto-Bold.ttf": "roboto/",
    "Roboto-BoldItalic.ttf": "roboto/",
    "Roboto-Italic.ttf": "roboto/",
    "Roboto-Light.ttf": "roboto/",
    "Roboto-LightItalic.ttf": "roboto/",
    "Roboto-Medium.ttf": "roboto/",
    "Roboto-MediumItalic.ttf": "roboto/",
    "Roboto-Regular.ttf": "roboto/",
    "Roboto-Thin.ttf": "roboto/",
    "Roboto-ThinItalic.ttf": "roboto/",
    "RobotoCondensed-Bold.ttf": "robotocondensed/",
    "RobotoCondensed-BoldItalic.ttf": "robotocondensed/",
    "RobotoCondensed-Italic.ttf": "robotocondensed/",
    "RobotoCondensed-Light.ttf": "robotocondensed/",
    "RobotoCondensed-LightItalic.ttf": "robotocondensed/",
    "RobotoCondensed-Regular.ttf": "robotocondensed/",
    "RobotoMono-Bold.ttf": "robotomono/",
    "RobotoMono-BoldItalic.ttf": "robotomono/",
    "RobotoMono-Italic.ttf": "robotomono/",
    "RobotoMono-Light.ttf": "robotomono/",
    "RobotoMono-LightItalic.ttf": "robotomono/",
    "RobotoMono-Medium.ttf": "robotomono/",
    "RobotoMono-MediumItalic.ttf": "robotomono/",
    "RobotoMono-Regular.ttf": "robotomono/",
    "RobotoMono-Thin.ttf": "robotomono/",
    "RobotoMono-ThinItalic.ttf": "robotomono/",
    "RobotoSlab-Bold.ttf": "robotoslab/",
    "RobotoSlab-Light.ttf": "robotoslab/",
    "RobotoSlab-Regular.ttf": "robotoslab/",
    "RobotoSlab-Thin.ttf": "robotoslab/",
    "ShortStack-Regular.ttf": "shortstack/",
    "Tinos-Bold.ttf": "tinos/",
    "Tinos-BoldItalic.ttf": "tinos/",
    "Tinos-Italic.ttf": "tinos/",
    "Tinos-Regular.ttf": "tinos/",
}

FontBundleInfo = provider(
    doc = "Information about a local font bundle.",
    fields = {
        "asset_names": "depset of font file names in the bundle.",
        "resources": "depset of package resource structs for the bundled fonts.",
    },
)

def _dedupe_resources(resources):
    seen = {}
    deduped = []
    for r in resources:
        if r.dest not in seen:
            seen[r.dest] = True
            deduped.append(r)
    return deduped

def _local_font_bundle_impl(ctx):
    resources = []
    for dep in ctx.attr.resources:
        resources.extend(dep[FuchsiaPackageResourcesInfo].resources)
    resources = _dedupe_resources(resources)
    return package_resources_providers(ctx, resources) + [
        FontBundleInfo(
            asset_names = depset(ctx.attr.asset_names),
            resources = depset(resources),
        ),
    ]

_local_font_bundle = rule(
    doc = "Bundles local font asset resources and provides FontBundleInfo.",
    implementation = _local_font_bundle_impl,
    attrs = {
        "asset_names": attr.string_list(
            doc = "List of font file names in this bundle.",
            mandatory = True,
        ),
        "resources": attr.label_list(
            doc = "Resource targets packaging each font file into data/assets/.",
            mandatory = True,
            providers = [FuchsiaPackageResourcesInfo],
        ),
    },
)

def local_font_bundle(
        name,
        asset_names,
        font_service_pkg = None,
        testonly = False,
        visibility = None):
    """Defines a target to bundle all of the given font files.

    Mirrors `local_font_bundle()` in `//src/fonts/build/fonts.gni`.

    Args:
        name: Target name.
        asset_names: List of font file names, e.g. `["Roboto-Regular.ttf"]`.
        font_service_pkg: Unused legacy parameter preserved for GN parity.
        testonly: Standard Bazel meaning.
        visibility: Standard Bazel meaning.
    """
    _ = font_service_pkg  # buildifier: disable=unused-variable
    not_found = [a for a in asset_names if a not in FONT_PKG_PATHS]
    if not_found:
        fail("font_pkg_entries not found for %s" % not_found)

    resource_labels = []
    for asset_name in asset_names:
        prebuilt_target = "_prebuilt_" + asset_name
        if not native.existing_rule(prebuilt_target):
            cipd_prebuilt_file(
                name = prebuilt_target,
                out = "assets/" + asset_name,
                path = "//prebuilt/third_party/fonts/" + FONT_PKG_PATHS[asset_name] + asset_name,
                visibility = ["//visibility:private"],
            )
        res_name = "_%s_%s_asset" % (name, asset_name)
        resource(
            name = res_name,
            src = ":" + prebuilt_target,
            dest = "%s/%s" % (_ASSETS_RELATIVE_PATH, asset_name),
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        resource_labels.append(":" + res_name)

    _local_font_bundle(
        name = name,
        asset_names = asset_names,
        resources = resource_labels,
        testonly = testonly,
        visibility = visibility,
    )

def _local_fonts_json_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.output_name)
    asset_names = depset(
        transitive = [
            b[FontBundleInfo].asset_names
            for b in ctx.attr.local_font_bundles
        ],
    ).to_list()
    ctx.actions.write(
        output = out,
        content = json.encode(asset_names) + "\n",
    )
    return [DefaultInfo(files = depset([out]))]

_local_fonts_json = rule(
    doc = "Generates `<name>_local_fonts.json` listing local font file names.",
    implementation = _local_fonts_json_impl,
    attrs = {
        "local_font_bundles": attr.label_list(
            providers = [FontBundleInfo],
            mandatory = True,
        ),
        "output_name": attr.string(mandatory = True),
    },
)

def _font_manifest_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.output_name)
    args = []
    inputs = []

    if not ctx.attr.empty and ctx.files.font_pkgs and ctx.files.font_catalogs:
        args.extend(["--local-fonts", ctx.file.local_fonts.path])
        inputs.append(ctx.file.local_fonts)

        args.extend(["--font-dir", ctx.file.fonts_dir.path])
        inputs.append(ctx.file.fonts_dir)

        args.append("--font-pkgs")
        for pkg_file in ctx.files.font_pkgs:
            args.append(pkg_file.path)
            inputs.append(pkg_file)

        args.append("--font-catalog")
        for catalog_file in ctx.files.font_catalogs:
            args.append(catalog_file.path)
            inputs.append(catalog_file)

        if ctx.file.product_config:
            args.extend(["--product-config", ctx.file.product_config.path])
            inputs.append(ctx.file.product_config)

        if ctx.attr.target_asset_dir:
            args.extend(["--target-asset-dir", ctx.attr.target_asset_dir])
    else:
        args.append("--empty")

    if ctx.attr.pretty_print:
        args.append("--pretty-print")

    args.extend(["--output", out.path])

    ctx.actions.run(
        executable = ctx.executable._generator,
        arguments = args,
        inputs = inputs,
        outputs = [out],
        mnemonic = "FontManifest",
        progress_message = "Generating font manifest %{label}",
    )

    return [DefaultInfo(files = depset([out]))]

_font_manifest = rule(
    doc = "Generates a `.font_manifest.json` file using `font_manifest_generator`.",
    implementation = _font_manifest_impl,
    attrs = {
        "empty": attr.bool(default = False),
        "font_catalogs": attr.label_list(allow_files = True),
        "font_pkgs": attr.label_list(allow_files = True),
        "fonts_dir": attr.label(allow_single_file = True),
        "local_fonts": attr.label(
            allow_single_file = True,
            mandatory = True,
        ),
        "output_name": attr.string(mandatory = True),
        "pretty_print": attr.bool(default = False),
        "product_config": attr.label(allow_single_file = True),
        "target_asset_dir": attr.string(default = ""),
        "_generator": attr.label(
            default = _FONT_MANIFEST_GENERATOR,
            executable = True,
            cfg = "exec",
        ),
    },
)

def _font_collection_group_impl(ctx):
    resources = []
    for dep in ctx.attr.deps:
        if FuchsiaPackageResourcesInfo in dep:
            resources.extend(dep[FuchsiaPackageResourcesInfo].resources)
    resources = _dedupe_resources(resources)

    files = depset(
        transitive = [m[DefaultInfo].files for m in ctx.attr.manifests],
    )
    providers = package_resources_providers(ctx, resources)
    runfiles = providers[1].default_runfiles
    return [
        providers[0],
        DefaultInfo(
            files = files,
            runfiles = runfiles,
        ),
    ]

_font_collection_group = rule(
    doc = "Collects font bundle resources and generated manifest resources for a font_collection.",
    implementation = _font_collection_group_impl,
    attrs = {
        "deps": attr.label_list(
            providers = [FuchsiaPackageResourcesInfo],
            mandatory = True,
        ),
        "manifests": attr.label_list(mandatory = True),
    },
)

def _ensure_font_catalog_prebuilts():
    if not native.existing_rule("_prebuilt_fonts_dir"):
        cipd_prebuilt_directory(
            name = "_prebuilt_fonts_dir",
            path = "//prebuilt/third_party/fonts",
            visibility = ["//visibility:private"],
        )
    if not native.existing_rule("_prebuilt_fuchsia.font_catalog.json"):
        cipd_prebuilt_file(
            name = "_prebuilt_fuchsia.font_catalog.json",
            out = "catalogs/fuchsia.font_catalog.json",
            path = "//prebuilt/third_party/fonts/fuchsia.font_catalog.json",
            visibility = ["//visibility:private"],
        )
    if not native.existing_rule("_prebuilt_fuchsia.font_pkgs.json"):
        cipd_prebuilt_file(
            name = "_prebuilt_fuchsia.font_pkgs.json",
            out = "catalogs/fuchsia.font_pkgs.json",
            path = "//prebuilt/third_party/fonts/fuchsia.font_pkgs.json",
            visibility = ["//visibility:private"],
        )
    if include_internal_fonts:
        if not native.existing_rule("_prebuilt_internal.font_catalog.json"):
            cipd_prebuilt_file(
                name = "_prebuilt_internal.font_catalog.json",
                out = "catalogs/internal.font_catalog.json",
                path = "//prebuilt/third_party/fonts/internal.font_catalog.json",
                visibility = ["//visibility:private"],
            )
        if not native.existing_rule("_prebuilt_internal.font_pkgs.json"):
            cipd_prebuilt_file(
                name = "_prebuilt_internal.font_pkgs.json",
                out = "catalogs/internal.font_pkgs.json",
                path = "//prebuilt/third_party/fonts/internal.font_pkgs.json",
                visibility = ["//visibility:private"],
            )

def font_collection(
        name,
        local_font_bundles = [],
        local_asset_names = [],
        product_config_path = None,
        manifest_prefix = _DEFAULT_MANIFEST_PREFIX,
        empty = False,
        assets_root_dir = "/pkg",
        testonly = False,
        visibility = None):
    """Generates font manifests and package resources for a product's font collection.

    Mirrors `font_collection()` in `//src/fonts/build/fonts.gni`.

    Args:
        name: Target name.
        local_font_bundles: Labels of `local_font_bundle` targets.
        local_asset_names: Optional list of local font file names.
        product_config_path: Optional path to a `.fontcfg.json5` file.
        manifest_prefix: Prefix for the generated manifest file name.
        empty: If True, indicates that this collection is intentionally empty.
        assets_root_dir: Prefix directory for hermetic font assets (default `"/pkg"`).
        testonly: Standard Bazel meaning.
        visibility: Standard Bazel meaning.
    """
    if testonly and manifest_prefix == _DEFAULT_MANIFEST_PREFIX:
        fail("Can't use default manifest_prefix '%s' with a testonly font_collection." % manifest_prefix)

    bundles = list(local_font_bundles)
    if local_asset_names:
        bundle_name = "_%s_local_font_bundle" % name
        local_font_bundle(
            name = bundle_name,
            asset_names = local_asset_names,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        bundles.append(":" + bundle_name)

    if (len(bundles) == 0) != empty:
        fail("An empty font_collection must explicitly declare `empty = True`")

    _ensure_font_catalog_prebuilts()

    local_fonts_json_label = "%s_local_fonts.json" % name
    _local_fonts_json(
        name = local_fonts_json_label,
        local_font_bundles = bundles,
        output_name = local_fonts_json_label,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    font_pkgs = [":_prebuilt_fuchsia.font_pkgs.json"]
    font_catalogs = [":_prebuilt_fuchsia.font_catalog.json"]
    if include_internal_fonts:
        font_pkgs.append(":_prebuilt_internal.font_pkgs.json")
        font_catalogs.append(":_prebuilt_internal.font_catalog.json")

    is_debug = compilation_mode == "debug"

    manifest_file_label = "%s_manifest_file" % name
    manifest_file_name = "%s_all.font_manifest.json" % name
    _font_manifest(
        name = manifest_file_label,
        empty = empty,
        font_catalogs = font_catalogs,
        font_pkgs = font_pkgs,
        fonts_dir = ":_prebuilt_fonts_dir",
        local_fonts = ":" + local_fonts_json_label,
        output_name = manifest_file_name,
        pretty_print = is_debug,
        product_config = product_config_path,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    asset_font_manifest_label = "%s_asset_manifest_file" % name
    asset_font_manifest_name = "%s_all.hermetic_assets.font_manifest.json" % name
    _font_manifest(
        name = asset_font_manifest_label,
        empty = empty,
        font_catalogs = font_catalogs,
        font_pkgs = font_pkgs,
        fonts_dir = ":_prebuilt_fonts_dir",
        local_fonts = ":" + local_fonts_json_label,
        output_name = asset_font_manifest_name,
        pretty_print = is_debug,
        product_config = product_config_path,
        target_asset_dir = "%s/%s" % (assets_root_dir.rstrip("/"), _ASSETS_RELATIVE_PATH),
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    font_assets_label = "%s_font_assets" % name
    resource(
        name = font_assets_label,
        src = ":" + asset_font_manifest_label,
        dest = "%s/%s" % (_ASSETS_RELATIVE_PATH, asset_font_manifest_name),
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    _font_collection_group(
        name = name,
        deps = bundles + [":" + font_assets_label],
        manifests = [
            ":" + manifest_file_label,
            ":" + asset_font_manifest_label,
        ],
        testonly = testonly,
        visibility = visibility,
    )

def font_packages(name = "packages", visibility = None):
    """Generates a single-font `fx_package` for every entry in `FONT_PKG_PATHS`.

    Mirrors `//src/fonts/packages/BUILD.gn`.

    Args:
        name: Name of the aggregate filegroup target (default `"packages"`).
        visibility: Visibility applied to the generated packages and aggregate group.
    """
    all_packages = []
    for file_name, path_prefix in FONT_PKG_PATHS.items():
        safe_name = file_name.lower().replace(".", "-")
        prebuilt_target = "_prebuilt_" + file_name
        if not native.existing_rule(prebuilt_target):
            cipd_prebuilt_file(
                name = prebuilt_target,
                out = "assets/" + file_name,
                path = "//prebuilt/third_party/fonts/" + path_prefix + file_name,
                visibility = ["//visibility:private"],
            )

        resource_target = "resource-" + safe_name
        resource(
            name = resource_target,
            src = ":" + prebuilt_target,
            dest = "%s/%s" % (_ASSETS_RELATIVE_PATH, file_name),
            visibility = ["//visibility:private"],
        )

        package_name = "font-package-" + safe_name
        all_packages.append(":" + package_name)
        fx_package(
            name = package_name,
            package_name = package_name,
            resources = [":" + resource_target],
            visibility = visibility,
        )

    native.filegroup(
        name = name,
        srcs = all_packages,
        visibility = visibility,
    )

def font_asset_provider(
        name,
        deps = [],
        public_deps = [],
        testonly = False,
        visibility = None):
    """Declares a build for the font asset provider package.

    Mirrors `font_asset_provider()` in `//src/fonts/build/font_asset_provider.gni`.

    Args:
        name: Target name.
        deps: List of `font_collection` targets to be bundled into this asset provider.
        public_deps: Additional dependencies to bundle into this asset provider.
        testonly: Standard Bazel meaning.
        visibility: Standard Bazel meaning.
    """
    component_name = "%s.asset-provider.component" % name
    fx_component(
        name = component_name,
        compiled_manifest = "//src/fonts/asset_providers:asset-provider.manifest",
        deps = deps + public_deps,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fx_package(
        name = name,
        archive_name = name,
        package_name = "asset-provider",
        components = [":" + component_name],
        testonly = testonly,
        visibility = visibility,
    )
