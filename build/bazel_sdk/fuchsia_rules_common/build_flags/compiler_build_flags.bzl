# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""compiler_build_flags() is the Bazel equivalent to the GN compiler_config() template."""

load(":build_flags.bzl", "build_flags")

def _get_flag_list(kwargs, key):
    """Extract a list or select() attribute from kwargs as a safe copy."""
    val = kwargs.pop(key, None)
    if val == None:
        return []
    if type(val) == "list":
        return list(val)
    return val

def compiler_build_flags(
        name,
        compiler_subflags = [],
        c_family_flags = [],
        llvm_flags = [],
        rust_llvm_flags = [],
        linker_flags = [],
        rust_linker_flags = [],
        linker_llvm_flags = [],
        rust_linker_llvm_flags = [],
        rust_target_features = [],
        **kwargs):
    """A Bazel equivalent to the GN compiler_config() template.

    Note that rust_target_edits is not supported as it should never
    be set in Bazel (only for GN Rust kernel targets).

    Args:
        name: Target name.
        compiler_subflags: A list of compiler_build_flags() dependencies.
            Just like subflags for build_flags(), their flags will be
            injected directly into the current instance.
        c_family_flags: Flags added to cflags and ldflags. (Note that
            asmflags is not supported by Bazel build_flags()).
        llvm_flags: LLVM specific flags, each one will be added as
            `-mllvm <arg>` to cflags, and as
            `-Cllvm-args=<arg>` to rustflags.
        rust_llvm_flags: Rust-only LLVM specific flags, each one will be
            added as `-Cllvm-args=<arg>` to rustflags.
        linker_flags: These flags are added to both `ldflags` and
            `rustflags`, with a `-Clink-arg=-Wl,` prefix for Rust.
        rust_linker_flags: Rust-specific linker flags. Also added with
            an implicit `-Clink-arg=-Wl,` prefix.
        linker_llvm_flags: LLVM specific linker flags, added as
            `--mllvm=<arg>` to linker_flags and rust_linker_flags.
        rust_linker_llvm_flags: Rust-only LLVM specific linker flags,
            added as `--mllvm=<arg>` to rust_linker_flags.
        rust_target_features: Each item is added as
            `-Ctarget-feature=<feature>` to rustflags.
        **kwargs: Additional attributes forwarded to build_flags().
    """
    if "rust_target_edits" in kwargs:
        fail("rust_target_edits cannot be set in Bazel")

    cflags = _get_flag_list(kwargs, "cflags") + c_family_flags
    ldflags = _get_flag_list(kwargs, "ldflags") + c_family_flags

    for arg in llvm_flags:
        cflags += ["-mllvm", arg]

    rustflags = _get_flag_list(kwargs, "rustflags")
    for arg in llvm_flags + rust_llvm_flags:
        rustflags += ["-Cllvm-args={}".format(arg)]

    _linker_flags = list(linker_flags) + [
        "--mllvm={}".format(arg)
        for arg in linker_llvm_flags
    ]
    _rust_linker_flags = (
        list(linker_flags) +
        list(rust_linker_flags) +
        [
            "--mllvm={}".format(arg)
            for arg in linker_llvm_flags + rust_linker_llvm_flags
        ]
    )

    if _linker_flags:
        ldflags += ["-Wl," + ",".join(_linker_flags)]

    if _rust_linker_flags:
        # Bazel invocations always require the -Wl,
        # prefix, as the Rust toolchain configuration
        # differs from the GN one.
        rustflags += [
            "-Clink-arg=-Wl," + ",".join(_rust_linker_flags),
        ]

    rustflags += ["-Ctarget-feature={}".format(feature) for feature in rust_target_features]

    kwargs["cflags"] = cflags
    kwargs["ldflags"] = ldflags
    kwargs["rustflags"] = rustflags

    # subflags is the same as configs
    kwargs["subflags"] = _get_flag_list(kwargs, "subflags") + compiler_subflags

    build_flags(
        name = name,
        **kwargs
    )
