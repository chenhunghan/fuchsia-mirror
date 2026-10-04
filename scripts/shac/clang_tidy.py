#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Driver script for running clang-tidy in SHAC and parsing diagnostics.

Accepts repository-relative C/C++ source and header file paths, resolves
companion translation units for standalone modified headers, invokes clang-tidy
concurrently, and emits deduplicated JSON findings to stdout.
"""

import argparse
import concurrent.futures
import json
import os
import pathlib
import re
import subprocess
import sys
from collections import abc

_CPP_EXTENSIONS = (".c", ".cc", ".cpp")
_HEADER_EXTENSIONS = (".h", ".hh", ".hpp")
_COMPANION_SUFFIXES = (".cc", ".cpp", ".c", "_test.cc", "_unittest.cc")
_DIAGNOSTIC_RE = re.compile(r":([0-9]+):([0-9]+):\s*(warning|error):\s*(.*)$")


def _candidate_companion_stems(header: str) -> abc.Sequence[str]:
    """Returns candidate path stems (without extension) for a header's companion source."""
    posix = pathlib.PurePosixPath(header)
    stem = posix.stem
    parent = posix.parent
    base_stems = (
        str(parent / stem),
        str(parent / "src" / stem),
    )
    parts = posix.parts
    if "include" not in parts:
        return base_stems

    idx = parts.index("include")
    pkg_dir = (
        pathlib.PurePosixPath(*parts[:idx])
        if idx > 0
        else pathlib.PurePosixPath(".")
    )
    sub_parts = parts[idx + 1 : -1]
    include_stems = (
        (
            str(pkg_dir / stem),
            str(pkg_dir / "src" / stem),
            str(pkg_dir.joinpath("src", *sub_parts, stem)),
        )
        if sub_parts
        else (
            str(pkg_dir / stem),
            str(pkg_dir / "src" / stem),
        )
    )
    return (*base_stems, *include_stems)


def resolve_files_to_check(
    root: pathlib.Path,
    cpp_files: abc.Sequence[str],
    header_files: abc.Sequence[str],
) -> abc.Sequence[str]:
    """Determines compilation units to pass to clang-tidy."""
    if not header_files:
        return tuple(cpp_files)

    files_to_check = list(cpp_files)
    checked_set = set(files_to_check)
    for h in header_files:
        candidate_stems = _candidate_companion_stems(h)
        if any(
            (stem + ext) in checked_set
            for stem in candidate_stems
            for ext in _COMPANION_SUFFIXES
        ):
            continue

        target_to_run = next(
            (
                stem + ext
                for stem in candidate_stems
                for ext in _COMPANION_SUFFIXES
                if (root / (stem + ext)).is_file()
            ),
            h,
        )
        if target_to_run not in checked_set:
            checked_set.add(target_to_run)
            files_to_check.append(target_to_run)

    return tuple(files_to_check)


def build_base_cmd(
    clang_tidy_bin: str,
    build_dir: str,
    header_files: abc.Sequence[str],
) -> abc.Sequence[str]:
    """Constructs the base clang-tidy command."""
    header_flag = (
        f"--header-filter=(^|/)({'|'.join(re.escape(h) for h in header_files)})$"
        if header_files
        else "--header-filter="
    )
    return (
        clang_tidy_bin,
        "-p",
        build_dir,
        "-quiet",
        header_flag,
    )


def _resolve_matched_file(
    file_part: str,
    abs_to_target: abc.Mapping[str, str],
    norm_build_dir: str,
    scm_root: str,
) -> str | None:
    """Resolves a clang-tidy diagnostic path to a repository-relative path."""
    candidates = (
        (os.path.normpath(file_part),)
        if file_part.startswith("/")
        else (
            os.path.normpath(os.path.join(norm_build_dir, file_part)),
            os.path.normpath(os.path.join(scm_root, file_part)),
        )
    )
    for cand in candidates:
        if cand in abs_to_target:
            return abs_to_target[cand]
    return None


def parse_clang_tidy_output(
    outputs: abc.Sequence[str],
    affected_targets: abc.Sequence[str],
    build_dir: str,
    scm_root: str,
) -> abc.Sequence[dict[str, object]]:
    """Parses clang-tidy stdout strings into deduplicated finding dicts."""
    norm_scm_root = os.path.normpath(scm_root)
    norm_build_dir = os.path.normpath(build_dir)
    abs_to_target = {
        os.path.normpath(os.path.join(norm_scm_root, t)): t
        for t in affected_targets
    }

    emitted = set()
    findings = []

    for stdout in outputs:
        if not stdout or "[clang-diagnostic-error]" in stdout:
            continue
        for line in stdout.splitlines():
            if line.startswith("Error while processing "):
                continue
            if ": warning: " not in line and ": error: " not in line:
                continue

            file_part = line.split(":", 1)[0]
            matched_file = _resolve_matched_file(
                file_part,
                abs_to_target,
                norm_build_dir,
                norm_scm_root,
            )
            if not matched_file:
                continue

            m = _DIAGNOSTIC_RE.search(line)
            if m:
                line_num = int(m.group(1))
                col_num = int(m.group(2))
                msg = m.group(4)
                key = (matched_file, line_num, col_num, msg)
                if key not in emitted:
                    emitted.add(key)
                    findings.append(
                        {
                            "message": msg,
                            "filepath": matched_file,
                            "line": line_num,
                            "col": col_num,
                        }
                    )
            else:
                msg = re.sub(r"^.*?:\s*(?:warning|error):\s*", "", line)
                key = (matched_file, 0, 0, msg)
                if key not in emitted:
                    emitted.add(key)
                    findings.append(
                        {
                            "message": msg,
                            "filepath": matched_file,
                        }
                    )

    return tuple(findings)


def main(argv: abc.Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--clang-tidy", required=True, help="Path to clang-tidy"
    )
    parser.add_argument("--build-dir", required=True, help="Build directory")
    parser.add_argument("--root", required=True, help="SCM root directory")
    parser.add_argument(
        "files", nargs="*", help="Affected C/C++ source and header files"
    )
    args = parser.parse_args(argv)

    compile_commands = pathlib.Path(args.build_dir) / "compile_commands.json"
    if not compile_commands.is_file():
        print(json.dumps([]))
        return 0

    cpp_files = tuple(f for f in args.files if f.endswith(_CPP_EXTENSIONS))
    header_files = tuple(
        f for f in args.files if f.endswith(_HEADER_EXTENSIONS)
    )
    if not cpp_files and not header_files:
        print(json.dumps([]))
        return 0

    root_path = pathlib.Path(args.root)
    files_to_check = resolve_files_to_check(root_path, cpp_files, header_files)
    base_cmd = build_base_cmd(args.clang_tidy, args.build_dir, header_files)

    def _run_one(filepath: str) -> str:
        proc = subprocess.run(
            (*base_cmd, filepath),
            cwd=args.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        return proc.stdout

    with concurrent.futures.ThreadPoolExecutor() as executor:
        outputs = tuple(executor.map(_run_one, files_to_check))

    skipped_count = sum(
        1
        for stdout in outputs
        if stdout and "[clang-diagnostic-error]" in stdout
    )
    if skipped_count:
        print(
            f"Skipped {skipped_count} file(s) due to compilation errors "
            "(missing generated headers or unbuilt targets); "
            "run 'fx build' for full clang-tidy analysis.",
            file=sys.stderr,
        )

    findings = parse_clang_tidy_output(
        outputs=outputs,
        affected_targets=(*cpp_files, *header_files),
        build_dir=args.build_dir,
        scm_root=args.root,
    )
    print(json.dumps(findings))
    return 0


if __name__ == "__main__":
    sys.exit(main())
