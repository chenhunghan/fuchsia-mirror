# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Library to normalize clang and lld command arguments for C, C++, Assembly, and Link actions."""

import os
import shlex
from collections.abc import Sequence

from build.beads.scripts import path_normalizer

# Action types supported for normalization.
ACTION_C_COMPILE = "c_compile"
ACTION_CPP_COMPILE = "cpp_compile"
ACTION_ASSEMBLE = "assemble"
ACTION_CPP_LINK = "cpp_link"

ALL_ACTION_TYPES = {
    ACTION_C_COMPILE,
    ACTION_CPP_COMPILE,
    ACTION_ASSEMBLE,
    ACTION_CPP_LINK,
}

# List of whole arguments to ignore when comparing GN and Bazel commands.
_ARGS_TO_IGNORE = set(
    [
        # Output and compilation control
        "-c",
        # Dependency files
        "-MD",
        "-MMD",
        # Diagnostic / formatting options
        "-fcolor-diagnostics",
        "-fno-color-diagnostics",
        "-Wl,--color-diagnostics",
        # RBE / Remote execution flags
        "--remote-only",
        "--local-only",
    ]
)

# List of arg prefixes to ignore when comparing GN and Bazel commands.
_ARG_PREFIXES_TO_IGNORE = (
    # Output and compilation control
    "-c",
    "-o=",
    "--output=",
    # Dependency files
    "-MF=",
    "-MT=",
    "-MQ=",
    # Linker map and dependency files
    "-Wl,--Map=",
    "-Wl,-Map=",
    "-Wl,--dependency-file=",
    "-Wl,-dependency-file=",
    # Driver mode flags added explicitly by Bazel
    "--driver-mode=",
    # Environment variables
    "PATH=",
    "PWD=",
    "TMPDIR=",
    # RBE / Remote execution flags
    "--remote-flag=",
    "--remote-worker-pool=",
    # Crash diagnostic / reproducers
    "-fcrash-diagnostics-dir=",
    # ThinLTO cache directory is used by GN/Ninja for local incremental builds,
    # whereas Bazel intentionally omits it as it is incompatible with action
    # sandboxing and remote execution.
    "-Wl,--thinlto-cache-dir=",
    # Sysroot paths differ structurally between GN (gen/zircon/public/sysroot)
    # and Bazel (@fuchsia_platform_sysroot or @fuchsia_sdk//arch/<cpu>/sysroot).
    "--sysroot=",
    # Fuchsia API level is forwarded to ldflags by GN compiler_fuchsia_api_level
    # (ignored by LLD) and applied via package transitions in fuchsia_sdk.
    "-ffuchsia-api-level=",
    # frandom-seed is injected per-object by Bazel rules_cc for deterministic hashing.
    "-frandom-seed=",
)


def _normalize_path(
    path: str, normalizer: path_normalizer.PathNormalizer
) -> str:
    """Normalize a path using the normalizer if available, or fallback to relative stripping."""
    try:
        return normalizer.normalize_path(path)
    except ValueError:
        return path


# Linker flags forwarded via `-Wl,` that accept a separate argument in the next
# comma-delimited (or consecutive `-Wl,`) slot, mapped to their canonical
# single-token prefix after `-Wl,`.
_WL_TWO_ARG_PREFIX_MAP = {
    "-z": "-z",
    "-L": "-L",
    "-T": "-T",
    "--script": "-T",
    "-mllvm": "--mllvm=",
    "--mllvm": "--mllvm=",
    "-plugin-opt": "--plugin-opt=",
    "--plugin-opt": "--plugin-opt=",
    "-soname": "-soname=",
    "--soname": "-soname=",
    "-h": "-soname=",
    "-Map": "--Map=",
    "--Map": "--Map=",
    "-dependency-file": "--dependency-file=",
    "--dependency-file": "--dependency-file=",
    "-rpath": "-rpath=",
    "--rpath": "-rpath=",
    "-rpath-link": "-rpath-link=",
    "--rpath-link": "-rpath-link=",
    "-dynamic-linker": "-dynamic-linker=",
    "--dynamic-linker": "-dynamic-linker=",
    "--version-script": "--version-script=",
    "-oso_prefix": "-oso_prefix=",
}


def _canonicalize_wl_inner(
    inner: str, normalizer: path_normalizer.PathNormalizer
) -> str:
    """Canonicalize the payload after `-Wl,` for single-token linker flags."""
    opt, eq, val = inner.partition("=")
    if eq and opt in _WL_TWO_ARG_PREFIX_MAP and opt not in ("-z", "-L"):
        inner = f"{_WL_TWO_ARG_PREFIX_MAP[opt]}{val}"

    if inner.startswith("-T") and len(inner) > 2:
        script_path = inner[3:] if inner.startswith("-T=") else inner[2:]
        return f"-T{_normalize_path(script_path, normalizer)}"
    if inner.startswith("--version-script="):
        vs_path = inner[len("--version-script=") :]
        return f"--version-script={_normalize_path(vs_path, normalizer)}"
    for rpath_prefix in ("-rpath=", "-rpath-link="):
        if inner.startswith(rpath_prefix):
            rpath_val = inner[len(rpath_prefix) :]
            if not rpath_val.startswith(("/", "$")):
                rpath_val = _normalize_path(rpath_val, normalizer)
            return f"{rpath_prefix}{rpath_val}"
    return inner


def _join_and_canonicalize_wl_tokens(
    tokens: Sequence[str], normalizer: path_normalizer.PathNormalizer
) -> list[str]:
    """Join two-argument `-Wl,` pairs and canonicalize `-Wl,` spellings."""
    result: list[str] = []
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if tok.startswith("-Wl,"):
            opt = tok[4:]
            if (
                opt in _WL_TWO_ARG_PREFIX_MAP
                and i + 1 < len(tokens)
                and tokens[i + 1].startswith("-Wl,")
            ):
                val = tokens[i + 1][4:]
                joined_inner = f"{_WL_TWO_ARG_PREFIX_MAP[opt]}{val}"
                result.append(
                    f"-Wl,{_canonicalize_wl_inner(joined_inner, normalizer)}"
                )
                i += 2
                continue
            result.append(f"-Wl,{_canonicalize_wl_inner(opt, normalizer)}")
        else:
            result.append(tok)
        i += 1
    return result


_PREFIX_MAP_OPTIONS = {
    "-ffile-prefix-map",
    "-fdebug-prefix-map",
    "-fmacro-prefix-map",
    "-fcoverage-prefix-map",
    "-fprofile-prefix-map",
    "-ffile-compilation-dir",
    "--remap-path-prefix",
}

# LINT.IfChange(in_tree_clang_prefix_map)
# Expected (FROM, TO) path-remapping pairs injected only on in-tree Bazel Clang
# compile and assemble actions so execroot-relative paths ("src/foo/lib.cc")
# match GN's build-dir-relative "../../<path>" DWARF and coverage conventions
# (which GN gets implicitly by running two directory levels below $FUCHSIA_DIR
# without prefix-map flags):
# - `-fdebug-prefix-map` comes from `_IN_TREE_DEBUG_PREFIX_MAP_FLAGS` in
#   `//build/config/BUILD.bazel` (`in_tree_clang_prefix_map`).
# - `-fcoverage-prefix-map` comes from `_IN_TREE_COVERAGE_PREFIX_MAP_FLAGS` in
#   `//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl`
#   (`in_tree_clang_prefix_map`).
# Stripping this exact canonical set during Bazel-vs-GN command comparison
# avoids false-positive flag diffs while still surfacing unexpected remap flags
# (or any remap flags mistakenly passed on link actions).
_CANONICAL_BAZEL_CLANG_PREFIX_MAP_PAIRS = (
    ("", "../../"),  # Fallback: prepend "../../" to checkout-relative paths.
    (".", "."),  # Preserve "." (-ffile-compilation-dir=.) instead of "../../.".
    ("./", "../../"),  # Strip "./" (from -iquote .) so "./foo" -> "../../foo".
    ("./bazel-out/", "bazel-out/"),  # Guard -iquote . ./bazel-out/ lookups.
    ("./external/", "external/"),  # Guard -iquote . ./external/ lookups.
    (
        "./fuchsia_build_generated/",
        "fuchsia_build_generated/",
    ),  # Guard -iquote . GN-bridged lookups.
    ("../", "../"),  # Avoid doubling "../" on already-relative parent paths.
    ("bazel-out/", "bazel-out/"),  # Keep generated outputs unprefixed.
    ("external/", "external/"),  # Keep external repository files unprefixed.
    (
        "fuchsia_build_generated/",
        "fuchsia_build_generated/",
    ),  # Keep GN-bridged files.
    ("/", "/"),  # Keep absolute sysroot/toolchain paths unchanged.
)
# LINT.ThenChange(
#   //build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl:in_tree_clang_prefix_map,
#   //build/beads/scripts/normalize_clang_args_test.py:in_tree_clang_prefix_map,
#   //build/config/BUILD.bazel:in_tree_clang_prefix_map,
# )

# Canonical `-fmacro-prefix-map` flag from `//build/config:relative_paths`
# stripping `-iquote .`'s leading `./` from header `__FILE__` expansions.
_CANONICAL_BAZEL_CLANG_MACRO_PREFIX_MAP_FLAG = "-fmacro-prefix-map=./="

# Full `-fdebug-prefix-map=<old>=<new>`, `-fcoverage-prefix-map=<old>=<new>`,
# and `-fmacro-prefix-map=./=` flag strings stripped on Bazel non-link actions.
_CANONICAL_BAZEL_CLANG_PREFIX_MAP_FLAGS = frozenset(
    [
        *(
            f"{opt}={old}={new}"
            for opt in ("-fdebug-prefix-map", "-fcoverage-prefix-map")
            for old, new in _CANONICAL_BAZEL_CLANG_PREFIX_MAP_PAIRS
        ),
        _CANONICAL_BAZEL_CLANG_MACRO_PREFIX_MAP_FLAG,
    ]
)


def normalize_prefix_map_flag(
    arg: str, normalizer: path_normalizer.PathNormalizer
) -> tuple[bool, str]:
    """Normalize a prefix-map flag into a canonical format.

    Supports:
      - -ffile-prefix-map=from=to
      - -fdebug-prefix-map=from=to
      - -fmacro-prefix-map=from=to
      - -fcoverage-prefix-map=from=to
      - -fprofile-prefix-map=from=to
      - -ffile-compilation-dir=dir
      - --remap-path-prefix=from=to
    """
    option, equal, value = arg.partition("=")
    if not equal:
        return False, arg

    if option not in _PREFIX_MAP_OPTIONS:
        return False, arg

    from_path, equal, to_path = value.partition("=")
    if equal:
        normalized_from = _normalize_path(from_path, normalizer)
        normalized_to = _normalize_path(to_path, normalizer)
        value = f"{normalized_from}={normalized_to}"
    else:
        if value == ".":
            # Preserve literal -ffile-compilation-dir=. without translating to
            # {BUILD_DIR} or {SOURCE_ROOT}.
            pass
        else:
            value = _normalize_path(value, normalizer)

    return True, f"{option}={value}"


def normalize_clang_arg(
    arg: str,
    normalizer: path_normalizer.PathNormalizer,
    action_type: str = ACTION_CPP_COMPILE,
    is_linux_target: bool = False,
) -> str:
    """Normalize a single clang or lld argument.

    Args:
        arg: The argument string to normalize.
        normalizer: PathNormalizer instance.
        action_type: One of c_compile, cpp_compile, assemble, cpp_link.
        is_linux_target: True if target OS is Linux.

    Returns:
        The normalized argument string, or empty string if it should be ignored.
    """
    if not arg:
        return ""

    if arg in _ARGS_TO_IGNORE:
        return ""

    if arg.startswith(_ARG_PREFIXES_TO_IGNORE):
        return ""

    # On ACTION_ASSEMBLE, filter C/C++-only cflags forwarded by @rules_cc copts
    # that Clang's assembler driver ignores (GN passes only asmflags + defines + include_dirs).
    if action_type == ACTION_ASSEMBLE:
        if (
            arg.startswith("-O")
            or (arg.startswith("-W") and not arg.startswith("-Wa,"))
            or arg
            in (
                "-fdata-sections",
                "-ffunction-sections",
                "-ffat-lto-objects",
                "-fno-omit-frame-pointer",
                "-fomit-frame-pointer",
                "-momit-leaf-frame-pointer",
                "-fstack-clash-protection",
                "-mbranches-within-32B-boundaries",
            )
            or arg.startswith(
                (
                    "-ftrivial-auto-var-init=",
                    "-fgnuc-version=",
                    "-march=",
                    "-mtune=",
                    "-mcpu=",
                    "-msmall-data-limit=",
                    "-Xclang=",
                    "-mllvm=",
                )
            )
        ):
            return ""

    # On Linux non-link actions, -fPIE is redundant as Fuchsia prebuilt Clang defaults to PIE on Linux.
    if is_linux_target and action_type != ACTION_CPP_LINK:
        if arg == "-fPIE":
            return ""

    # Normalize prefix-map / determinism flags
    if arg.startswith("-"):
        if (
            isinstance(normalizer, path_normalizer.BazelPathNormalizer)
            and action_type != ACTION_CPP_LINK
            and arg in _CANONICAL_BAZEL_CLANG_PREFIX_MAP_FLAGS
        ):
            return ""
        changed, arg = normalize_prefix_map_flag(arg, normalizer)
        if changed:
            return arg

        # Normalize include flags: -Ipath, -isystem path, -iquote path, -idirafter path
        for inc_prefix in ("-I", "-isystem", "-iquote", "-idirafter"):
            if arg.startswith(inc_prefix):
                val = arg[len(inc_prefix) :]
                val = _normalize_path(val, normalizer)
                # Ignore build-system root include search dirs
                if inc_prefix == "-I" and (
                    val in ("{SOURCE_ROOT}", "gen")
                    or (val.endswith("/gen") and val.count("/") == 1)
                ):
                    return ""
                if inc_prefix == "-iquote" and (
                    val == "{SOURCE_ROOT}" or val.startswith("{BAZEL_OUT}")
                ):
                    return ""
                return f"{inc_prefix}{val}"

        # Normalize library search dirs: -Lpath, -Wl,-Lpath
        if arg.startswith("-L"):
            val = _normalize_path(arg[2:], normalizer)
            if action_type == ACTION_CPP_LINK and val.startswith("{BAZEL_OUT}"):
                return ""
            return f"-L{val}"
        if arg.startswith("-Wl,-L"):
            val = _normalize_path(arg[6:], normalizer)
            if action_type == ACTION_CPP_LINK and val.startswith("{BAZEL_OUT}"):
                return ""
            return f"-L{val}"

        # Filter link-specific inputs and response files
        if action_type == ACTION_CPP_LINK:
            if arg.startswith("@") and arg.endswith(".rsp"):
                return ""
            if arg in ("-Wl,--start-group", "-Wl,--end-group"):
                return ""
            if arg.startswith("-l"):
                return ""

    # Normalize tool names & non-flag arguments
    else:
        base = os.path.basename(arg)
        if base in ("clang", "clang++"):
            return "clang"
        if base in (
            "ld.lld",
            "lld",
            "llvm-ar",
            "llvm-strip",
        ):
            return base

        # Filter out intermediate object files and library inputs in link commands
        if action_type == ACTION_CPP_LINK:
            if arg.endswith((".o", ".obj", ".so", ".a")):
                return ""
            if arg.startswith("@") and arg.endswith(".rsp"):
                return ""

        # Normalize source file paths
        if action_type in (
            ACTION_C_COMPILE,
            ACTION_CPP_COMPILE,
            ACTION_ASSEMBLE,
        ):
            if any(
                arg.endswith(ext)
                for ext in (
                    ".c",
                    ".cc",
                    ".cpp",
                    ".cxx",
                    ".C",
                    ".S",
                    ".s",
                    ".asm",
                )
            ):
                return _normalize_path(arg, normalizer)

        # Ignore raw output/temporary paths
        if (
            arg.startswith("obj/")
            or arg.startswith("bazel-out/")
            or arg.startswith("out/")
        ):
            return ""

        # Normalize any other file paths
        return _normalize_path(arg, normalizer)

    return arg


def normalize_clang_cmd(
    cmd: str,
    normalizer: path_normalizer.PathNormalizer,
    action_type: str = ACTION_CPP_COMPILE,
) -> list[str]:
    """Normalize a full clang / lld command.

    Args:
        cmd: The raw command string.
        normalizer: Optional PathNormalizer instance.
        action_type: One of c_compile, cpp_compile, assemble, cpp_link.

    Returns:
        A sorted list of unique normalized arguments.
    """
    tokens = shlex.split(cmd)
    merged_tokens: list[str] = []
    skip_next = False

    flags_with_values = {
        "-o": "-o=",
        "-MF": "-MF=",
        "-MT": "-MT=",
        "-MQ": "-MQ=",
        "-I": "-I",
        "-L": "-L",
        "-isystem": "-isystem",
        "-iquote": "-iquote",
        "-idirafter": "-idirafter",
        "--sysroot": "--sysroot=",
        "--target": "--target=",
        "-Xclang": "-Xclang=",
        "-mllvm": "-mllvm=",
        "-fdebug-prefix-map": "-fdebug-prefix-map=",
        "-fmacro-prefix-map": "-fmacro-prefix-map=",
        "-fcoverage-prefix-map": "-fcoverage-prefix-map=",
    }

    for i, token in enumerate(tokens):
        if skip_next:
            skip_next = False
            continue

        if token in flags_with_values and i + 1 < len(tokens):
            merged_tokens.append(f"{flags_with_values[token]}{tokens[i + 1]}")
            skip_next = True
        elif (
            token.startswith("-o")
            and len(token) > 2
            and not token.startswith("-o=")
        ):
            merged_tokens.append(f"-o={token[2:]}")
        elif token.startswith("-Wl,"):
            merged_tokens.extend(
                f"-Wl,{part}" for part in token[4:].split(",") if part
            )
        else:
            merged_tokens.append(token)

    merged_tokens = _join_and_canonicalize_wl_tokens(merged_tokens, normalizer)

    is_linux_target = any(
        t.startswith("--target=") and "linux" in t for t in merged_tokens
    )

    normalized_args = []
    for t in merged_tokens:
        norm = normalize_clang_arg(
            t,
            normalizer=normalizer,
            action_type=action_type,
            is_linux_target=is_linux_target,
        )
        if norm:
            normalized_args.append(norm)

    return sorted(set(normalized_args))
