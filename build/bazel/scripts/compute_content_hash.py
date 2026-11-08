#!/usr/bin/env python3
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generate a content hash file from one or more source repository content.

By default, scan all files in the source paths (file or directory) and hash
their content.

- For symlinks, the link value is used as input (not the target file). If the
  link points to another symlink, the whole chain of link values is used.

- For regular files, its sha1 digest is used as input.

- For git directories (which have a .git entry), use the HEAD commit as input
  to speed things dramatically. NOTE: This ignores changes to the index,
  during development.

- If --cipd-name=NAME is set, and <source_path>/.versions/NAME.cipd_version
  exists, its content will be used as input.

- Otherwise, for directories, all files in it are found recursively,
  and used as hash input independently.
"""

import argparse
import hashlib
import os
import sys
import typing as T
from pathlib import Path

from build.bazel.scripts import get_git_head_commit as gghc

_HASH = "sha1"


def _depfile_quote(p: Path | str) -> str:
    """Quote a Path value for depfile output."""
    return str(p).replace("\\", "\\\\").replace(" ", "\\ ")


def _next_symlink_hop(link_path: Path, target: Path) -> Path:
    """Return the absolute path of the entry a symlink points to.

    All directory components of the result are resolved, so that a relative
    target containing '..' is applied to the real directory containing the
    link, not to a lexical path that may go through symlinked directories.
    The final component is left unresolved so callers can inspect it to see
    whether it is itself a symlink (i.e. the next hop in a symlink chain).

    Args:
        link_path: Path to a symlink.
        target: The value of the symlink, as returned by readlink().
    """
    # Note that os.path.join() ignores the first argument when target is
    # absolute.
    joined = os.path.join(os.path.dirname(link_path), target)
    head, tail = os.path.split(joined)
    if tail in ("", os.curdir, os.pardir):
        # The target ends with a directory-only component (e.g. 'foo/' or
        # '..'), which cannot be a symlink itself, so resolve it entirely.
        return Path(os.path.realpath(joined))
    return Path(os.path.realpath(head)) / tail


class FileState(object):
    """State object used to hash one or more source paths.

    Usage is:
       - Create instance
       - Call hash_source_path() as many times as possible.
       - Use content_hash property to get final result.
       - Use sorted_input_files to get list of input files.
    """

    def __init__(
        self,
        cipd_names: T.Sequence[str] = [],
        exclude_suffixes: T.Sequence[str] = [],
        git_binary: Path = Path("git"),
    ) -> None:
        """Create new instance.

        Args:
            cipd_names: A sequence of cipd names for prebuilt directories.
            exclude_suffixes: A sequence of filename suffixes to exclude from hashing.
            git_binary: Path to the git binary to use for .git repositories.
        """
        self._cipd_names = cipd_names
        self._exclude_suffixes = tuple(exclude_suffixes)
        self._git_binary = git_binary
        self._input_files: set[Path] = set()
        self._sorted_input_files: list[str] | None = None
        self._hstate = hashlib.new(_HASH)

    def hash_source_path(self, source_path: Path) -> None:
        """Process and hash a given source file, updating internal state."""
        self._hstate.update(self.process_source_path(source_path).encode())

    def find_directory_files(self, source_path: Path) -> set[Path]:
        source_path.is_dir(), f"Input source path is not a directory: {source_path}"

        if self._cipd_names:
            for cipd_name in self._cipd_names:
                clang_version_file = (
                    source_path / ".versions" / f"{cipd_name}.cipd_version"
                )
                if clang_version_file.exists():
                    return set([clang_version_file])

        # Find all files in direcrory.
        dir_files: set[Path] = set()
        for dirpath, dirnames, filenames in os.walk(source_path):
            for filename in filenames:
                if filename.endswith(self._exclude_suffixes):
                    continue
                file_path = Path(os.path.join(dirpath, filename))
                dir_files.add(file_path)

        return dir_files

    def process_source_path(self, source_path: Path) -> str:
        """Process a given source file, and return a string descriptor for it.

        The first letter of the result corresponds to the type of the source path.
        This function is useful for unit-testing the implementation and verify
        that different types of source paths are handled correctly. Apart from
        that, consider this as an implementation detail.
        """
        if not source_path.exists():
            raise ValueError(f"Path does not exist: {source_path}")

        if source_path.is_dir() and (source_path / ".git").exists():
            head_commit = gghc.get_git_head_commit(
                source_path, self._git_binary
            )
            self._input_files.update(gghc.find_git_head_inputs(source_path))
            return "G" + head_commit

        if source_path.is_symlink():
            # Follow the whole symlink chain, so that every intermediate link
            # is an input (a change to any of them must trigger a rehash) and
            # every link value is part of the descriptor. Only inspecting the
            # first hop would miss changes to the target of a chained link.
            #
            # Intermediate links are recorded relative to the current directory
            # (like source paths rebased on the build directory) unless
            # source_path is absolute. The chain is finite because
            # source_path.exists() is checked above, which fails on loops.
            link_path = source_path
            targets = []
            while link_path.is_symlink():
                self._input_files.add(link_path)
                target = link_path.readlink()
                targets.append(str(target))
                next_hop = _next_symlink_hop(link_path, target)
                if source_path.is_absolute():
                    link_path = next_hop
                else:
                    link_path = Path(os.path.relpath(next_hop))
            return "S" + "->".join(targets)

        if source_path.is_file():
            self._input_files.add(source_path)
            with source_path.open("rb") as f:
                digest = hashlib.file_digest(f, _HASH)
            return "F" + digest.hexdigest()

        assert source_path.is_dir(), f"Unexpected file type for {source_path}"

        if self._cipd_names:
            for cipd_name in self._cipd_names:
                clang_version_file = (
                    source_path / ".versions" / f"{cipd_name}.cipd_version"
                )
                if clang_version_file.exists():
                    return self.process_source_path(clang_version_file)

        # Get the list of files relative to the source directory.
        dir_files: list[str] = [
            os.path.relpath(f, source_path)
            for f in self.find_directory_files(source_path)
        ]

        # Process them recursively to build a directory description text
        # where each line looks like: <file> <type><digest>
        dir_content = "D\n"
        for dir_file in sorted(dir_files):
            file_hash = self.process_source_path(source_path / dir_file)
            dir_content += f" {dir_file} {file_hash}\n"
        return dir_content

    @property
    def content_hash(self) -> str:
        """Return final content hash as hexadecimal string."""
        return self._hstate.hexdigest()

    @property
    def sorted_input_files(self) -> list[str]:
        """Return the list of input files used by this instance."""
        if self._sorted_input_files is None:
            self._sorted_input_files = sorted(
                [str(p) for p in self._input_files]
            )
        return self._sorted_input_files

    def get_input_file_paths(self) -> set[Path]:
        """Return the set of input file Path values used by this instance."""
        return self._input_files


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawTextHelpFormatter
    )
    parser.add_argument(
        "--cipd-name",
        action="append",
        default=[],
        help="Provide name for optional CIPD version file. Can be used multiple times.",
    )
    parser.add_argument(
        "--exclude-suffix",
        action="append",
        default=[],
        help='Exclude directory entries with given suffix (e.g. ".pyc").\n'
        + "Can be used multiple times.",
    )
    parser.add_argument(
        "--git-binary",
        type=Path,
        default=Path("git"),
        help="Specify git binary to use for git repositories.",
    )
    parser.add_argument(
        "source_path",
        type=Path,
        nargs="+",
        help="Source file or directory path.",
    )
    parser.add_argument(
        "--output", type=Path, help="Optional output file path."
    )
    parser.add_argument(
        "--depfile", type=Path, help="Optional Ninja depfile output file path."
    )
    parser.add_argument(
        "--inputs-list",
        type=Path,
        help="Write list of inputs to file, one path per line.",
    )
    args = parser.parse_args()

    if args.depfile and not args.output:
        parser.error("--depfile option requires --output.")

    fstate = FileState(args.cipd_name, args.exclude_suffix, args.git_binary)
    for source_path in args.source_path:
        fstate.hash_source_path(source_path)

    if args.output:
        # Do not modify existing output if it has the same content.
        current_content = "~~~"
        if args.output.exists():
            current_content = args.output.read_text()
        if current_content != fstate.content_hash:
            args.output.write_text(fstate.content_hash)
    else:
        print(fstate.content_hash)

    if args.inputs_list:
        args.inputs_list.write_text("\n".join(fstate.sorted_input_files) + "\n")

    if args.depfile:
        args.depfile.write_text(
            "%s: \\\n  %s\n"
            % (
                args.output,
                " \\\n  ".join(
                    _depfile_quote(f) for f in fstate.sorted_input_files
                ),
            )
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
