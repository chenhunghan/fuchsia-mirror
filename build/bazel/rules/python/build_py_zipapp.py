#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Builds a Python zipapp (.pyz) for the py_zipapp_binary() Bazel rule.

See //build/bazel/rules/python/py_zipapp_binary.bzl for the archive layout.
"""

import argparse
import os
import pathlib
import sys
import zipfile

# Generated entry point, like the one of the GN python_binary() template (see
# //build/python/package_python_binary.py), minus its filter for SyntaxWarnings
# from //third_party/mako, which no Bazel target depends on.
_MAIN_TEMPLATE = """\
import sys
import inspect

from {main_module} import *

if inspect.iscoroutinefunction({main_callable}):
    import asyncio
    asyncio.run({main_callable}())
else:
    sys.exit({main_callable}())
"""

# Fixed timestamp for all archive entries: the earliest one the zip format can
# represent. See _write_archive().
_ZIP_EPOCH = (1980, 1, 1, 0, 0, 0)


class _ArchiveError(Exception):
    """Raised when the inputs can't be packaged into a valid archive."""


class _ArchiveEntries:
    """Maps archive paths to their contents, rejecting conflicting entries."""

    def __init__(self) -> None:
        self._entries: dict[str, tuple[bytes, str]] = {}

    def add(self, arcname: str, data: bytes, origin: str) -> None:
        existing = self._entries.get(arcname)
        if existing is None:
            self._entries[arcname] = (data, origin)
        elif existing[0] != data:
            # The same file is often reachable through several import roots,
            # but two different files must not map to the same archive path.
            raise _ArchiveError(
                f"Conflicting archive entry {arcname!r}: from {existing[1]} "
                f"and {origin}"
            )

    def add_file(self, arcname: str, path: str) -> None:
        self.add(arcname, pathlib.Path(path).read_bytes(), path)

    def names(self) -> list[str]:
        return sorted(self._entries)

    def data(self, arcname: str) -> bytes:
        return self._entries[arcname][0]


def _import_relative_paths(runfiles_path: str, imports: list[str]) -> set[str]:
    """Returns the paths at which `runfiles_path` is importable.

    Like py_binary(), which puts the root of every repository in its runfiles
    and each `PyInfo.imports` entry on `sys.path`, returns the path of the file
    relative to each of these import roots that contains it.

    Args:
        runfiles_path: Path of the file relative to the runfiles root, e.g.
            "_main/build/foo/bar.py".
        imports: Import roots relative to the runfiles root, e.g.
            "_main/build/foo".
    """
    repo_root = runfiles_path.split("/", 1)[0]
    paths = set()
    for root in (repo_root, *imports):
        prefix = root.rstrip("/") + "/"
        if runfiles_path.startswith(prefix):
            paths.add(runfiles_path[len(prefix) :])
    return paths


def _collect_entries(args: argparse.Namespace) -> _ArchiveEntries:
    entries = _ArchiveEntries()

    # The binary's own sources are at the archive root, like in GN
    # python_binary(), so that `main` can import them by module name.
    for source in args.binary_source:
        basename = os.path.basename(source)
        if basename == "__main__.py":
            raise _ArchiveError(
                "__main__.py in sources of py_zipapp_binary() is not "
                "supported, it is generated."
            )
        entries.add_file(basename, source)

    if len(args.source) != len(args.source_runfiles_path):
        raise _ArchiveError(
            "--source and --source_runfiles_path must be given the same "
            "number of times"
        )
    for path, runfiles_path in zip(args.source, args.source_runfiles_path):
        for arcname in _import_relative_paths(runfiles_path, args.imports):
            entries.add_file(arcname, path)

    if args.data_package_name:
        # Like GN python_binary(), store data sources in a package at the
        # archive root.
        package_dir = args.data_package_name
        entries.add(f"{package_dir}/__init__.py", b"", "<data package>")
        for data_source in args.data_source:
            entries.add_file(
                f"{package_dir}/{os.path.basename(data_source)}", data_source
            )
    elif args.data_source:
        raise _ArchiveError("--data_package_name is required with data sources")

    main = _MAIN_TEMPLATE.format(
        main_module=args.main_module, main_callable=args.main_callable
    )
    entries.add("__main__.py", main.encode(), "<generated __main__.py>")
    return entries


def _directory_names(file_names: list[str]) -> set[str]:
    """Returns the names of the directory entries containing `file_names`."""
    directories = set()
    for name in file_names:
        parts = name.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            directories.add("/".join(parts[:i]) + "/")
    return directories


def _write_archive(
    output: pathlib.Path, shebang: str, entries: _ArchiveEntries
) -> None:
    """Writes `entries` to an executable zip archive starting with `shebang`.

    This doesn't use `zipapp.create_archive()`, which stores the modification
    time of each file in the archive. Those times vary between checkouts,
    sandboxes and builds, so the output would differ between builds of the same
    inputs. That breaks Bazel's assumption that actions are deterministic:
    it defeats action and remote caching, and changes the output of every
    action that depends on the archive. Instead, write entries in sorted order,
    with a fixed timestamp and permissions.

    Like `zipapp.create_archive()`, also write an entry for each directory:
    before Python 3.14, `zipimport` can't find namespace packages (directories
    without an `__init__.py`, such as `build/` for `from build.foo import bar`)
    without them.
    """
    file_names = entries.names()
    with output.open("wb") as f:
        f.write(f"#!{shebang}\n".encode())
        with zipfile.ZipFile(f, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            for name in sorted(_directory_names(file_names) | set(file_names)):
                info = zipfile.ZipInfo(name, date_time=_ZIP_EPOCH)
                if name.endswith("/"):
                    info.compress_type = zipfile.ZIP_STORED
                    # Unix directory mode, plus the MS-DOS directory flag.
                    info.external_attr = (0o40755 << 16) | 0x10
                    zf.writestr(info, b"")
                else:
                    info.compress_type = zipfile.ZIP_DEFLATED
                    info.external_attr = 0o644 << 16
                    zf.writestr(info, entries.data(name))
    output.chmod(0o755)


def main() -> int:
    # Bazel passes the arguments in a param file, one per line.
    parser = argparse.ArgumentParser(
        description=__doc__, fromfile_prefix_chars="@"
    )
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--main_module", required=True)
    parser.add_argument("--main_callable", required=True)
    parser.add_argument("--unbuffered", action="store_true")
    parser.add_argument(
        "--binary_source",
        action="append",
        default=[],
        help="Source of the binary, placed at the archive root.",
    )
    parser.add_argument(
        "--source",
        action="append",
        default=[],
        help="Path of a source placed under its import roots.",
    )
    parser.add_argument(
        "--source_runfiles_path",
        action="append",
        default=[],
        help="Runfiles path of the --source at the same position.",
    )
    parser.add_argument(
        "--import",
        dest="imports",
        action="append",
        default=[],
        help="Import root, relative to the runfiles root.",
    )
    parser.add_argument("--data_package_name")
    parser.add_argument("--data_source", action="append", default=[])
    args = parser.parse_args()

    if args.unbuffered:
        shebang = "/usr/bin/env -S fuchsia-vendored-python -u"
    else:
        shebang = "/usr/bin/env fuchsia-vendored-python"

    try:
        entries = _collect_entries(args)
    except _ArchiveError as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 1
    _write_archive(args.output, shebang, entries)
    return 0


if __name__ == "__main__":
    sys.exit(main())
