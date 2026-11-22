#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verify debug_symbols.json binaries, DWARF source paths, and .rodata literals."""

import argparse
import dataclasses
import json
import pathlib
import re
import subprocess
import sys
from collections.abc import Collection, Mapping, Sequence

# Allow this script to be run directly from the command-line to help debugging.
_SCRIPT_DIR = pathlib.Path(__file__).parent
_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent.parent.parent
_MODULES_DIR = _FUCHSIA_DIR / "build/python/modules"
if _MODULES_DIR.is_dir():
    sys.path.insert(0, str(_MODULES_DIR))

import elf

# Workspace-relative directory of the test fixtures verified in this package.
_TEST_PKG_PREFIX = "build/bazel/debug_symbols/tests/"

# Expected in-tree "../../"-remapped test package prefix, forbidden in OOT DWARF
# and expected in in-tree C++/assembly DWARF.
_IN_TREE_TEST_PKG_PREFIX = f"../../{_TEST_PKG_PREFIX}"

# Matches any path prefix ending in a directory separator immediately preceding
# _TEST_PKG_PREFIX in raw .rodata bytes (e.g. "../../", "./", "/abs/path/", or
# "bazel-out/.../"), without false-matching non-NUL-terminated .rodata.cst*
# constants that end in ASCII characters (e.g. "Hello wo" before "build/...").
_PREFIXED_TEST_PKG_RODATA_RE = re.compile(
    rb"[A-Za-z0-9_./\\-]*[/\\]" + re.escape(_TEST_PKG_PREFIX.encode("utf-8"))
)

# Bazel sandbox and output-base path components whose presence in DWARF or
# .rodata indicates an unstripped local or RBE build environment path leak.
_FORBIDDEN_SANDBOX_MARKERS = ("execroot", "output_base")
_FORBIDDEN_SANDBOX_BYTES = (b"execroot", b"output_base")

# Relative DWARF prefixes that indicate unguarded, double-remapped, or
# unremapped (`-iquote .`) paths in in_tree mode.
_FORBIDDEN_IN_TREE_DWARF_PREFIXES = (
    "../../external/",
    "../../bazel-out/",
    "../../fuchsia_build_generated/",
    "../.././",
    "../../../../",
    f"./{_TEST_PKG_PREFIX}",
)

# Relative ../llvm-llvm-project paths embedded in statically linked prebuilt
# LLVM runtime archives (libc++, libc++abi, libunwind).
_PREBUILT_LLVM_RUNTIME_RE = re.compile(r"^(?:\.\./)+llvm-llvm-project(?:/|$)")

# LUCI builder absolute paths (/b/s/w/ir/...) pre-baked into statically linked
# prebuilt LLVM runtime archives, sysroot headers, and Rust stdlib/third-party
# crates (pulled in via transitive exec host tools in Fuchsia manifests). Do
# not widen this to all of /b/s/w/ir/: Fuchsia's own LUCI CQ checkouts run
# under /b/s/w/ir/, so a broader prefix would mask real workspace path leaks on
# CQ. Update this allowlist if prebuilt builder layouts change.
_PREBUILT_TOOLCHAIN_ABS_RE = re.compile(
    r"^/b/s/w/ir/(?:x/w(?:$|/(?:llvm-llvm-project|llvm_build|cipd|sdk|rust-rust|fuchsia-third_party-rust)(?:/|$))|(?:x/t/\.cargo|k/rust)(?:/|$))"
)

# Regexes matching `llvm-dwarfdump --debug-info --debug-line` output across
# DWARF v4 and v5 compile units, attributes, and line-table file/dir tables.
_CU_HEADER_RE = re.compile(r"Compile Unit:.*?\bversion = 0x([0-9a-fA-F]+)")
_TAG_LINE_RE = re.compile(r"^\s*0x[0-9a-fA-F]+:\s+(DW_TAG_\S+)")
_ATTR_NAME_RE = re.compile(r'\bDW_AT_name\s+\("([^"]*)"\)')
_ATTR_COMP_DIR_RE = re.compile(r'\bDW_AT_comp_dir\s+\("([^"]*)"\)')
_INC_DIR_RE = re.compile(r'^\s*include_directories\[\s*(\d+)\]\s*=\s*"([^"]*)"')
_FILE_NAME_RE = re.compile(r'^\s*name:\s*"([^"]*)"')
_DIR_INDEX_RE = re.compile(r"^\s*dir_index:\s*(\d+)")

# llvm-readelf -x precedes each hex word (1-4 byte pairs) with a single space,
# while partial trailing lines (< 4 words) pad missing columns with >= 10 spaces
# before the ASCII preview column. Requiring a single space prevents bridging
# across padding into ASCII text.
_HEX_DUMP_LINE_RE = re.compile(
    r"^\s*0x[0-9a-fA-F]+((?: (?:[0-9a-fA-F]{2}){1,4}){1,4})"
)

# Base target names in //build/bazel/debug_symbols/tests compiled from a_main.cc.
_CPP_MAIN_BASE_TARGETS = frozenset(
    (
        "a_cc_binary",
        "a_cc_test",
        "a_fuchsia_cc_binary",
        "a_fuchsia_cc_test",
        "an_fx_cc_binary",
        "an_fx_cc_binary_dwarf4",
        "an_fx_cc_binary_thinlto",
    )
)


@dataclasses.dataclass
class _CompileUnitInfo:
    """Metadata extracted from a single DWARF DW_TAG_compile_unit entry."""

    version: int
    name: str | None = None
    comp_dir: str | None = None


@dataclasses.dataclass(frozen=True)
class _DwarfDumpResult:
    """Parsed .debug_info compile units and .debug_line paths from llvm-dwarfdump."""

    compile_units: Sequence[_CompileUnitInfo]
    include_dirs: Sequence[str]
    line_files: Sequence[str]


def _is_forbidden_in_tree_dwarf_prefix(path: str) -> bool:
    """Return True if path uses a disallowed relative DWARF prefix in in_tree mode."""
    if _PREBUILT_LLVM_RUNTIME_RE.match(path) is not None:
        return False
    return any(
        path == prefix.rstrip("/") or path.startswith(prefix)
        for prefix in _FORBIDDEN_IN_TREE_DWARF_PREFIXES
    )


def _is_disallowed_abs_path(path: str) -> bool:
    """Return True if path is a non-prebuilt leading-'/' absolute path leak."""
    return (
        path.startswith("/") and _PREBUILT_TOOLCHAIN_ABS_RE.match(path) is None
    )


def _check_debug_binary_path(path: pathlib.Path) -> str | None:
    """Verify that a debug binary exists and is an unstripped ELF file.

    Args:
        path: Filesystem path to the candidate debug ELF binary.

    Returns:
        An error diagnostic string if validation fails, or None on success.
    """
    if not path.exists():
        return f"Missing debug binary file: {path}"

    info = elf.elfinfo.get_elf_info(str(path))
    if info is None:
        return f"Not an ELF file: {path}"

    if info.stripped:
        return f"Binary does not contain debug symbols: {path}"

    return None


def _parse_dwarf_dump(output: str) -> _DwarfDumpResult:
    """Parse llvm-dwarfdump --debug-info --debug-line text output.

    Resolves .debug_line file entries against each line-table header's
    include_directories table (handling both DWARF v4 1-based and DWARF v5
    0-based indices) and collects DW_TAG_compile_unit metadata.

    Args:
        output: Raw stdout captured from llvm-dwarfdump.

    Returns:
        Parsed compile units, raw include_directories entries, and resolved
        .debug_line file paths.
    """
    compile_units = []
    all_include_dirs = []
    line_files = []

    current_cu: _CompileUnitInfo | None = None
    in_cu_tag = False
    section = ""
    include_dir_map: dict[int, str] = {}
    pending_file_name: str | None = None

    for line in output.splitlines():
        if line.startswith(".debug_info contents:"):
            section = "debug_info"
            in_cu_tag = False
            continue
        if line.startswith(".debug_line contents:"):
            section = "debug_line"
            in_cu_tag = False
            continue

        if section == "debug_info":
            if (header_match := _CU_HEADER_RE.search(line)) is not None:
                current_cu = _CompileUnitInfo(
                    version=int(header_match.group(1), 16)
                )
                compile_units.append(current_cu)
                in_cu_tag = False
                continue

            if (tag_match := _TAG_LINE_RE.match(line)) is not None:
                in_cu_tag = tag_match.group(1) == "DW_TAG_compile_unit"
                continue

            if line.strip().endswith("NULL"):
                in_cu_tag = False
                continue

            if in_cu_tag and current_cu is not None:
                if (name_match := _ATTR_NAME_RE.search(line)) is not None:
                    current_cu.name = name_match.group(1)
                    continue
                if (
                    comp_dir_match := _ATTR_COMP_DIR_RE.search(line)
                ) is not None:
                    current_cu.comp_dir = comp_dir_match.group(1)
                    continue

        elif section == "debug_line":
            if line.startswith("debug_line["):
                include_dir_map = {}
                pending_file_name = None
                continue

            if (inc_match := _INC_DIR_RE.match(line)) is not None:
                idx = int(inc_match.group(1))
                inc_path = inc_match.group(2)
                include_dir_map[idx] = inc_path
                all_include_dirs.append(inc_path)
                continue

            if (file_match := _FILE_NAME_RE.match(line)) is not None:
                pending_file_name = file_match.group(1)
                continue

            if (
                dir_match := _DIR_INDEX_RE.match(line)
            ) is not None and pending_file_name is not None:
                dir_idx = int(dir_match.group(1))
                inc_dir = include_dir_map.get(dir_idx, "")
                if pending_file_name.startswith("/") or inc_dir in ("", "."):
                    resolved = pending_file_name
                else:
                    resolved = f"{inc_dir.rstrip('/')}/{pending_file_name}"
                line_files.append(resolved)
                pending_file_name = None

    return _DwarfDumpResult(
        compile_units=tuple(compile_units),
        include_dirs=tuple(all_include_dirs),
        line_files=tuple(line_files),
    )


def _format_subprocess_error(
    tool_name: str, exc: OSError | subprocess.CalledProcessError
) -> str:
    """Format an OS or subprocess invocation failure into a concise message.

    Args:
        tool_name: Human-readable executable name for error reporting.
        exc: Exception raised by subprocess.run().

    Returns:
        Formatted single-line diagnostic containing exit code or stderr detail.
    """
    if isinstance(exc, subprocess.CalledProcessError):
        detail = (exc.stderr or "").strip() or f"exit status {exc.returncode}"
        return f"{tool_name} failed (exit {exc.returncode}): {detail}"
    return f"{tool_name} failed: {exc}"


def _run_dwarfdump(
    dwarfdump_bin: pathlib.Path, binary_path: pathlib.Path
) -> _DwarfDumpResult:
    """Invoke llvm-dwarfdump on an ELF binary and parse its DWARF tables.

    Args:
        dwarfdump_bin: Path to the llvm-dwarfdump executable.
        binary_path: Path to the unstripped ELF binary to inspect.

    Returns:
        Parsed DWARF compile-unit metadata and .debug_line entries.

    Raises:
        OSError: If dwarfdump_bin cannot be executed.
        subprocess.CalledProcessError: If llvm-dwarfdump exits non-zero.
    """
    proc = subprocess.run(
        [str(dwarfdump_bin), "--debug-info", "--debug-line", str(binary_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=True,
    )
    return _parse_dwarf_dump(proc.stdout)


def _parse_readelf_hex_dump(output: str) -> bytes:
    """Decode raw section bytes from `llvm-readelf -x` hexadecimal dump lines.

    Args:
        output: Raw stdout captured from `llvm-readelf -x <section>`.

    Returns:
        Concatenated raw section bytes decoded from the hex columns.
    """
    chunks = []
    for line in output.splitlines():
        if (m := _HEX_DUMP_LINE_RE.match(line)) is not None:
            chunks.append(bytes.fromhex(m.group(1).replace(" ", "")))
    return b"".join(chunks)


def _run_readelf_rodata(
    readelf_bin: pathlib.Path, binary_path: pathlib.Path
) -> bytes:
    """Extract raw .rodata section bytes from an ELF binary using llvm-readelf.

    Args:
        readelf_bin: Path to the llvm-readelf executable.
        binary_path: Path to the ELF binary to inspect.

    Returns:
        Raw bytes of the binary's .rodata section (empty if no hex lines match).

    Raises:
        OSError: If readelf_bin cannot be executed.
        subprocess.CalledProcessError: If llvm-readelf exits non-zero.
    """
    proc = subprocess.run(
        [str(readelf_bin), "-x", ".rodata", str(binary_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=True,
    )
    return _parse_readelf_hex_dump(proc.stdout)


def _has_exact_unprefixed_cstr(rodata: bytes, expected_str: str) -> bool:
    """Check for an exact NUL-terminated string literal without a path prefix.

    Used to verify unprefixed __FILE__ strings in .rodata even when adjacent
    non-NUL-terminated .rodata.cst* constant data (including ASCII bytes such
    as "Hello wo") immediately precedes .rodata.str1.1 in merged .rodata output.

    Args:
        rodata: Raw .rodata section bytes.
        expected_str: Exact source path string expected without a leading '/'
            or '\\' directory separator.

    Returns:
        True if expected_str appears NUL-terminated without a preceding slash.
    """
    pattern = (
        rb"(?<![/\\])" + re.escape(expected_str.encode("utf-8")) + rb"\x00"
    )
    return re.search(pattern, rodata) is not None


def _target_name_from_label(label: str) -> str:
    """Extract the target name component after ':' from a Bazel label."""
    _, _, name = label.partition(":")
    return name


def _is_test_pkg_label(label: str) -> bool:
    """Return True if label belongs to //build/bazel/debug_symbols/tests."""
    return label.startswith(
        (
            "@@//build/bazel/debug_symbols/tests:",
            "//build/bazel/debug_symbols/tests:",
        )
    )


def _check_manifest_completeness(
    manifest: Collection[Mapping[str, str]],
    expected_targets: Collection[str],
    expected_configs: Collection[str],
) -> Sequence[str]:
    """Verify that expected test targets and configs appear in the manifest.

    Args:
        manifest: Parsed JSON entries from debug_symbols.json.
        expected_targets: Base target names under
            //build/bazel/debug_symbols/tests required in the manifest; empty
            disables completeness checking.
        expected_configs: Compilation mode segments (e.g. "dbg", "fastbuild",
            "opt") expected in `bazel-out/<config>/` (excluding "-exec" tool
            configs) for each expected target.

    Returns:
        Sequence of diagnostic strings describing any missing target or config pairs.
    """
    if not expected_targets:
        return []

    seen_pairs = set()
    for entry in manifest:
        label = entry.get("label", "")
        if not _is_test_pkg_label(label):
            continue
        base_name = _target_name_from_label(label).split(".")[0]
        parts = pathlib.Path(entry.get("debug", "")).parts
        config_dir = (
            parts[1] if len(parts) >= 2 and parts[0] == "bazel-out" else ""
        )
        seen_pairs.add((base_name, config_dir))

    errors = []
    if expected_configs:
        for target in expected_targets:
            for cfg in expected_configs:
                cfg_re = re.compile(rf"(?:^|-){re.escape(cfg)}(?:-|$)")
                if not any(
                    b == target
                    and "exec" not in cdir.split("-")
                    and cfg_re.search(cdir) is not None
                    for b, cdir in seen_pairs
                ):
                    errors.append(
                        f"Manifest missing expected entry for target {target!r} "
                        f"with config {cfg!r} (saw {sorted(seen_pairs)!r})"
                    )
    else:
        seen_targets = {b for b, _ in seen_pairs}
        for target in expected_targets:
            if target not in seen_targets:
                errors.append(
                    f"Manifest missing expected target {target!r} "
                    f"(saw {sorted(seen_targets)!r})"
                )
    return errors


def _check_cpp_rodata(
    rodata: bytes,
    label: str,
    binary_path: pathlib.Path,
    expected_fnames: Collection[str],
) -> Sequence[str]:
    """Verify unprefixed C++ __FILE__ literals and absence of prefixed test paths.

    Args:
        rodata: Raw .rodata section bytes.
        label: Bazel target label from the debug symbols manifest.
        binary_path: Resolved filesystem path to the ELF binary.
        expected_fnames: Source filenames under _TEST_PKG_PREFIX whose exact
            unprefixed __FILE__ string must appear NUL-terminated in .rodata.

    Returns:
        Sequence of diagnostic strings for missing or prefixed C++ .rodata paths.
    """
    errors = []
    for fname in expected_fnames:
        expected_rodata = f"{_TEST_PKG_PREFIX}{fname}"
        if not _has_exact_unprefixed_cstr(rodata, expected_rodata):
            errors.append(
                f"{label} ({binary_path}): missing unprefixed NUL-terminated __FILE__ string {expected_rodata!r} in .rodata"
            )
    if (
        prefixed_rodata_match := _PREFIXED_TEST_PKG_RODATA_RE.search(rodata)
    ) is not None:
        leaked_prefix = prefixed_rodata_match.group(0).decode(
            "utf-8", errors="replace"
        )
        errors.append(
            f"{label} ({binary_path}): unexpected prefixed test path {leaked_prefix!r} in C++ .rodata"
        )
    return errors


def _check_binary_dwarf_and_rodata(
    entry: Mapping[str, str],
    binary_path: pathlib.Path,
    mode: str,
    dwarfdump_bin: pathlib.Path,
    readelf_bin: pathlib.Path,
    default_dwarf_version: int,
) -> Sequence[str]:
    """Verify DWARF, .debug_line, and .rodata path invariants for one binary.

    Args:
        entry: Manifest dictionary containing at least `"label"` and `"debug"`.
        binary_path: Resolved filesystem path to the unstripped ELF binary.
        mode: Path remapping mode to validate ("in_tree" or "oot").
        dwarfdump_bin: Path to the llvm-dwarfdump executable.
        readelf_bin: Path to the llvm-readelf executable.
        default_dwarf_version: Expected DWARF version for test CUs without a
            per-target override.

    Returns:
        Sequence of diagnostic strings for any DWARF, .debug_line, or .rodata
        checks that failed for this binary.
    """
    errors = []
    label = entry.get("label", "")
    target_name = _target_name_from_label(label)
    base_name = target_name.split(".")[0]

    try:
        dump = _run_dwarfdump(dwarfdump_bin, binary_path)
    except (OSError, subprocess.CalledProcessError) as exc:
        errors.append(
            f"{label} ({binary_path}): {_format_subprocess_error('llvm-dwarfdump', exc)}"
        )
        return errors

    try:
        rodata = _run_readelf_rodata(readelf_bin, binary_path)
    except (OSError, subprocess.CalledProcessError) as exc:
        errors.append(
            f"{label} ({binary_path}): {_format_subprocess_error('llvm-readelf', exc)}"
        )
        return errors

    # Global leak and guard checks across every compile unit and .debug_line entry.
    matched_test_cus = 0
    for cu in dump.compile_units:
        for val in (cu.comp_dir, cu.name):
            if val is None:
                continue
            if _is_disallowed_abs_path(val):
                errors.append(
                    f"{label} ({binary_path}): unexpected absolute path in CU metadata {val!r}"
                )
            for marker in _FORBIDDEN_SANDBOX_MARKERS:
                if marker in val:
                    errors.append(
                        f"{label} ({binary_path}): sandbox marker {marker!r} leaked in CU metadata {val!r}"
                    )
            if mode == "in_tree":
                if _is_forbidden_in_tree_dwarf_prefix(val):
                    errors.append(
                        f"{label} ({binary_path}): forbidden DWARF CU prefix in {val!r}"
                    )
            elif (
                val.startswith("../../")
                and _PREBUILT_LLVM_RUNTIME_RE.match(val) is None
            ):
                errors.append(
                    f"{label} ({binary_path}): unexpected '../../' DWARF CU prefix in {val!r}"
                )

        if cu.name and _TEST_PKG_PREFIX in cu.name:
            matched_test_cus += 1
            if mode == "in_tree" and base_name.endswith("_dwarf4"):
                expected_cu_version = 4
            elif mode == "in_tree" and base_name.endswith("_thinlto"):
                expected_cu_version = 5
            else:
                expected_cu_version = default_dwarf_version
            if cu.version != expected_cu_version:
                errors.append(
                    f"{label} ({binary_path}): expected DWARF v{expected_cu_version} for test CU {cu.name!r}, got {cu.version!r}"
                )
            if cu.comp_dir != ".":
                errors.append(
                    f"{label} ({binary_path}): expected DW_AT_comp_dir='.' for test CU {cu.name!r}, got {cu.comp_dir!r}"
                )
            if mode == "in_tree":
                if not cu.name.startswith(_IN_TREE_TEST_PKG_PREFIX):
                    errors.append(
                        f"{label} ({binary_path}): expected in-tree DW_AT_name starting with {_IN_TREE_TEST_PKG_PREFIX!r}, got {cu.name!r}"
                    )
            else:
                if not cu.name.startswith(
                    _TEST_PKG_PREFIX
                ) or cu.name.startswith("../../"):
                    errors.append(
                        f"{label} ({binary_path}): expected OOT DW_AT_name starting with {_TEST_PKG_PREFIX!r} without '../../', got {cu.name!r}"
                    )

    for inc_dir in dump.include_dirs:
        if _is_disallowed_abs_path(inc_dir):
            errors.append(
                f"{label} ({binary_path}): unexpected absolute include_dir {inc_dir!r}"
            )
        for marker in _FORBIDDEN_SANDBOX_MARKERS:
            if marker in inc_dir:
                errors.append(
                    f"{label} ({binary_path}): sandbox marker {marker!r} leaked in include_dir {inc_dir!r}"
                )
        if mode == "in_tree":
            if _is_forbidden_in_tree_dwarf_prefix(inc_dir):
                errors.append(
                    f"{label} ({binary_path}): forbidden DWARF include_dir prefix in {inc_dir!r}"
                )
        elif (
            inc_dir.startswith("../../")
            and _PREBUILT_LLVM_RUNTIME_RE.match(inc_dir) is None
        ):
            errors.append(
                f"{label} ({binary_path}): unexpected '../../' include_dir prefix in {inc_dir!r}"
            )

    for lf in dump.line_files:
        if _is_disallowed_abs_path(lf):
            errors.append(
                f"{label} ({binary_path}): unexpected absolute .debug_line path {lf!r}"
            )
        for marker in _FORBIDDEN_SANDBOX_MARKERS:
            if marker in lf:
                errors.append(
                    f"{label} ({binary_path}): sandbox marker {marker!r} leaked in .debug_line path {lf!r}"
                )
        if mode == "in_tree":
            if _is_forbidden_in_tree_dwarf_prefix(lf):
                errors.append(
                    f"{label} ({binary_path}): forbidden .debug_line prefix in {lf!r}"
                )
        elif (
            lf.startswith("../../")
            and _PREBUILT_LLVM_RUNTIME_RE.match(lf) is None
        ):
            errors.append(
                f"{label} ({binary_path}): unexpected '../../' .debug_line prefix in {lf!r}"
            )

    for marker_bytes in _FORBIDDEN_SANDBOX_BYTES:
        if marker_bytes in rodata:
            errors.append(
                f"{label} ({binary_path}): sandbox marker {marker_bytes.decode('ascii')!r} leaked in .rodata"
            )

    if not _is_test_pkg_label(label):
        return errors

    if matched_test_cus == 0:
        cu_names = tuple(cu.name for cu in dump.compile_units)
        errors.append(
            f"{label} ({binary_path}): expected at least one test CU containing {_TEST_PKG_PREFIX!r}, saw {cu_names!r}"
        )

    expected_prefix = (
        _IN_TREE_TEST_PKG_PREFIX if mode == "in_tree" else _TEST_PKG_PREFIX
    )
    normalized_line_files = (
        dump.line_files
        if mode == "in_tree"
        else tuple(lf.removeprefix("./") for lf in dump.line_files)
    )

    # Per-target checks for //build/bazel/debug_symbols/tests targets.
    cu_name_set = {cu.name for cu in dump.compile_units if cu.name is not None}
    if base_name in _CPP_MAIN_BASE_TARGETS:
        for fname in ("a_main.cc", "a_library.cc", "a_asm.S", "a_raw_asm.s"):
            expected_cu = f"{expected_prefix}{fname}"
            if expected_cu not in cu_name_set:
                errors.append(
                    f"{label} ({binary_path}): missing expected test CU {expected_cu!r} (saw {sorted(cu_name_set)!r})"
                )
        for fname in (
            "a_main.cc",
            "a_library.cc",
            "a_library.h",
            "a_asm.S",
            "a_raw_asm.s",
        ):
            expected_path = f"{expected_prefix}{fname}"
            if not any(lf == expected_path for lf in normalized_line_files):
                errors.append(
                    f"{label} ({binary_path}): missing .debug_line path {expected_path!r}"
                )
        if not any(
            lf.startswith("bazel-out/") and lf.endswith("/generated_header.h")
            for lf in normalized_line_files
        ):
            errors.append(
                f"{label} ({binary_path}): missing unprefixed bazel-out/.../generated_header.h in .debug_line"
            )
        if not any(
            lf.startswith("external/") and "vector" in lf
            for lf in normalized_line_files
        ):
            errors.append(
                f"{label} ({binary_path}): missing unprefixed external/.../vector header in .debug_line"
            )
        errors.extend(
            _check_cpp_rodata(
                rodata, label, binary_path, ("a_main.cc", "a_library.cc")
            )
        )

    elif base_name == "a_cc_shared_library":
        expected_cu = f"{expected_prefix}a_library.cc"
        if expected_cu not in cu_name_set:
            errors.append(
                f"{label} ({binary_path}): missing expected test CU {expected_cu!r} (saw {sorted(cu_name_set)!r})"
            )
        for fname in ("a_library.cc", "a_library.h"):
            expected_path = f"{expected_prefix}{fname}"
            if not any(lf == expected_path for lf in normalized_line_files):
                errors.append(
                    f"{label} ({binary_path}): missing .debug_line path {expected_path!r}"
                )
        errors.extend(
            _check_cpp_rodata(rodata, label, binary_path, ("a_library.cc",))
        )

    else:
        errors.append(
            f"{label} ({binary_path}): unhandled test target {base_name!r} in check_debug_binaries.py"
        )

    return errors


def main() -> int:
    """Run debug binary manifest, DWARF, and .rodata verification CLI.

    Returns:
        0 if all checks pass, or 1 if manifest parsing or any check fails.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        required=True,
        help="Input manifest path.",
    )
    parser.add_argument(
        "--base-dir",
        type=pathlib.Path,
        default=pathlib.Path.cwd(),
        help="Base directory for file paths in manifest.",
    )
    parser.add_argument(
        "--mode",
        choices=("in_tree", "oot"),
        default="oot",
        help="Path remapping mode to verify.",
    )
    parser.add_argument(
        "--default-dwarf-version",
        type=int,
        choices=(2, 3, 4, 5),
        default=5,
        help="Default DWARF version expected for test CUs without an explicit per-target override.",
    )
    parser.add_argument(
        "--llvm-dwarfdump",
        type=pathlib.Path,
        default=None,
        help="Optional path to llvm-dwarfdump binary for DWARF verification.",
    )
    parser.add_argument(
        "--llvm-readelf",
        type=pathlib.Path,
        default=None,
        help="Optional path to llvm-readelf binary for .rodata verification.",
    )
    parser.add_argument(
        "--expect-target",
        dest="expected_targets",
        action="append",
        default=[],
        help="Expected //build/bazel/debug_symbols/tests base target name (repeatable).",
    )
    parser.add_argument(
        "--expect-config",
        dest="expected_configs",
        action="append",
        default=[],
        help="Expected compilation mode segment in debug path (e.g. dbg, fastbuild, opt; repeatable).",
    )
    parser.add_argument(
        "--stamp",
        type=pathlib.Path,
        default=None,
        help="Optional output stamp file to write upon success.",
    )
    args = parser.parse_args()

    if (args.llvm_dwarfdump is not None) != (args.llvm_readelf is not None):
        parser.error(
            "--llvm-dwarfdump and --llvm-readelf must be provided together"
        )
    if args.expected_configs and not args.expected_targets:
        parser.error("--expect-config requires --expect-target")

    with args.manifest.open("rt") as f:
        manifest = json.load(f)

    if not isinstance(manifest, list):
        print("ERROR: Manifest is not a JSON list!", file=sys.stderr)
        return 1

    if len(manifest) == 0:
        print("ERROR: Manifest is empty!", file=sys.stderr)
        return 1

    errors: list[str] = []
    errors.extend(
        _check_manifest_completeness(
            manifest,
            args.expected_targets,
            args.expected_configs,
        )
    )

    for entry in manifest:
        debug_binary_path = args.base_dir / entry["debug"]
        if (error := _check_debug_binary_path(debug_binary_path)) is not None:
            errors.append(error)
            continue

        if args.llvm_dwarfdump is not None and args.llvm_readelf is not None:
            errors.extend(
                _check_binary_dwarf_and_rodata(
                    entry=entry,
                    binary_path=debug_binary_path,
                    mode=args.mode,
                    dwarfdump_bin=args.llvm_dwarfdump,
                    readelf_bin=args.llvm_readelf,
                    default_dwarf_version=args.default_dwarf_version,
                )
            )

    if errors:
        print(f"ERROR: Found {len(errors)} errors:", file=sys.stderr)
        for error in errors:
            print(error, file=sys.stderr)
        return 1

    if args.stamp is not None:
        args.stamp.parent.mkdir(parents=True, exist_ok=True)
        args.stamp.write_text("ok\n", encoding="utf-8")

    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
