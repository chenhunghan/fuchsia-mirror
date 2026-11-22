# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Starlark rules and transitions for debug symbol tests."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_build_config//:defs.bzl", "build_config")
load("//build/bazel/debug_symbols:aspects.bzl", _generate_manifest_aspect = "generate_manifest")

visibility("private")

def _multi_compilation_mode_transition_impl(_settings, _attr):
    return {
        "Debug build": {"//command_line_option:compilation_mode": "dbg"},
        "Fastbuild build": {"//command_line_option:compilation_mode": "fastbuild"},
        "Optimized build": {"//command_line_option:compilation_mode": "opt"},
    }

multi_compilation_mode_transition = transition(
    implementation = _multi_compilation_mode_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:compilation_mode"],
)

def _multi_compilation_mode_filegroup_impl(ctx):
    # Return a DefaultInfo value that points to all files from all dependency variants.
    # There is no point in running such a target, so do not try to compute
    # an all_runfiles value.
    all_files = depset([], transitive = [d[DefaultInfo].files for d in ctx.attr.srcs])
    return [
        DefaultInfo(files = all_files),
    ]

multi_compilation_mode_filegroup = rule(
    doc = """A filegroup-like rule that builds its 'srcs' in multiple compilation modes.

    This rule builds 'srcs' three times, in "dbg", "fastbuild" and "opt" modes,
    then combines all their output files in its DefaultInfo provider.
    """,
    implementation = _multi_compilation_mode_filegroup_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = "A list of labels to targets that will be built in all compilation modes.",
            default = [],
            cfg = multi_compilation_mode_transition,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _debug_symbols_manifest_impl(ctx):
    # Just re-export the manifest itself.
    if ctx.attr.output:
        output = ctx.outputs.output
    else:
        output = ctx.actions.declare_file(ctx.target.label.name)

    dep_outputs = ctx.attr.dep[OutputGroupInfo]
    manifest = dep_outputs.debug_symbol_manifest.to_list()[0]
    ctx.actions.symlink(output = output, target_file = manifest)
    return [
        DefaultInfo(files = depset([output])),
        OutputGroupInfo(
            debug_symbol_files = dep_outputs.debug_symbol_files,
        ),
    ]

debug_symbols_manifest = rule(
    doc = """Generate a debug symbols manifest from transitive dependencies of a given target.""",
    implementation = _debug_symbols_manifest_impl,
    attrs = {
        "output": attr.output(
            doc = "Optional output file name.",
            mandatory = False,
        ),
        "dep": attr.label(
            doc = "Top-level dependency to extract debug symbols from.",
            mandatory = True,
            providers = [OutputGroupInfo],
            aspects = [_generate_manifest_aspect],
        ),
    },
)

def _host_target_transition_impl(_settings, _attr):
    return {
        "//command_line_option:platforms": "@//build/bazel/platforms:" + build_config.host_tag_alt,
    }

_host_target_transition = transition(
    implementation = _host_target_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _host_target_filegroup_impl(ctx):
    all_files = depset([], transitive = [d[DefaultInfo].files for d in ctx.attr.srcs])
    return [DefaultInfo(files = all_files)]

host_target_filegroup = rule(
    doc = """Transitions srcs to the host target platform without exec -g0.""",
    implementation = _host_target_filegroup_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = "Targets to transition onto the host target platform.",
            default = [],
            cfg = _host_target_transition,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

# Simulates an out-of-tree `@rules_fuchsia` build from inside the in-tree
# workspace by resetting `@fuchsia_rules_common//build_flags:use_default_build_flags`
# to `False` (its default in `@rules_fuchsia`, overridden to `True` only by the
# in-tree `template.bazelrc`). Because `cc_features.bzl` and `toolchain_utils.bzl`
# branch directly on that single setting, no SDK flag lists are duplicated here.
# Using a transition lets `quick_tests` compile real ELF binaries and inspect
# their emitted DWARF and `.rodata` sections (whereas `//build/bazel/bazel_sdk:tests`
# analysis tests only inspect dummy files without invoking Clang).
def _oot_mode_transition_impl(_settings, _attr):
    return {
        mode: {
            "//command_line_option:platforms": "@//build/bazel/debug_symbols/tests:oot_host_platform",
            "//command_line_option:compilation_mode": mode,
            "@fuchsia_rules_common//build_flags:use_default_build_flags": False,
        }
        for mode in ("dbg", "fastbuild", "opt")
    }

_oot_mode_transition = transition(
    implementation = _oot_mode_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:platforms",
        "//command_line_option:compilation_mode",
        "@fuchsia_rules_common//build_flags:use_default_build_flags",
    ],
)

def _oot_mode_filegroup_impl(ctx):
    all_files = depset([], transitive = [d[DefaultInfo].files for d in ctx.attr.srcs])
    return [DefaultInfo(files = all_files)]

oot_mode_filegroup = rule(
    doc = """Transitions srcs onto :oot_host_platform with use_default_build_flags = False across dbg, fastbuild, and opt.""",
    implementation = _oot_mode_filegroup_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = "Targets to transition into Linux out-of-tree dbg, fastbuild, and opt modes.",
            default = [],
            cfg = _oot_mode_transition,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _verify_debug_symbols_manifest_impl(ctx):
    manifest_file = ctx.file.dep
    default_dwarf_version = (
        ctx.attr._dwarf_version[BuildSettingInfo].value if ctx.attr.mode == "in_tree" else 5
    )
    args = ctx.actions.args()
    args.add("--manifest", manifest_file)
    args.add("--mode", ctx.attr.mode)
    args.add("--default-dwarf-version", default_dwarf_version)
    args.add("--llvm-dwarfdump", ctx.executable._llvm_dwarfdump)
    args.add("--llvm-readelf", ctx.executable._llvm_readelf)
    args.add_all(ctx.attr.expected_targets, before_each = "--expect-target")
    args.add_all(ctx.attr.expected_configs, before_each = "--expect-config")
    args.add("--stamp", ctx.outputs.stamp)
    ctx.actions.run(
        executable = ctx.executable._checker,
        arguments = [args],
        inputs = depset(
            [manifest_file],
            transitive = [ctx.attr.dep[OutputGroupInfo].debug_symbol_files],
        ),
        tools = [ctx.executable._llvm_dwarfdump, ctx.executable._llvm_readelf],
        outputs = [ctx.outputs.stamp],
        mnemonic = "VerifyDebugSymbols",
    )
    return [DefaultInfo(files = depset([ctx.outputs.stamp]))]

verify_debug_symbols_manifest = rule(
    doc = """Runs check_debug_binaries.py with llvm-dwarfdump and llvm-readelf at build time.""",
    implementation = _verify_debug_symbols_manifest_impl,
    attrs = {
        "dep": attr.label(
            doc = "A debug_symbols_manifest target whose JSON manifest and debug_symbol_files are verified.",
            mandatory = True,
            allow_single_file = [".json"],
            providers = [OutputGroupInfo],
        ),
        "expected_configs": attr.string_list(
            doc = "Compilation mode segments (e.g. dbg, fastbuild, opt) expected for each target in expected_targets.",
            default = [],
        ),
        "expected_targets": attr.string_list(
            doc = "Base target names under //build/bazel/debug_symbols/tests expected in the manifest.",
            default = [],
        ),
        "mode": attr.string(
            doc = "Set of path invariants to check (in_tree or oot).",
            values = ["in_tree", "oot"],
            mandatory = True,
        ),
        "stamp": attr.output(
            doc = "Stamp file written by the checker when verification succeeds.",
            mandatory = True,
        ),
        "_checker": attr.label(
            default = ":check_debug_binaries",
            executable = True,
            cfg = "exec",
        ),
        "_dwarf_version": attr.label(
            default = "//build/bazel/config/variables:dwarf_version",
            providers = [BuildSettingInfo],
        ),
        "_llvm_dwarfdump": attr.label(
            default = "@prebuilt_clang//:bin/llvm-dwarfdump",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
        ),
        "_llvm_readelf": attr.label(
            default = "@prebuilt_clang//:bin/llvm-readelf",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
        ),
    },
)
