#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Update a vendored Bazel dependency version in MODULE.bazel files and vendor it."""

import argparse
import re
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

_SCRIPT_DIR = Path(__file__).resolve().parent
_FUCHSIA_DIR = _SCRIPT_DIR.parents[3]

_MODULE_FILES = (
    Path("build/bazel/toplevel.MODULE.bazel"),
    Path("build/bazel_sdk/tests/MODULE.bazel"),
)

_BAZEL_VENDOR_DIR = Path("third_party/bazel_vendor/fuchsia_in_tree")
_BAZEL_REGISTRY_DIR = Path(
    "third_party/bazel_registries/bcr.bazel.build/modules"
)

_VALID_DEP_NAME_RE = re.compile(r"^[a-zA-Z0-9_.-]+$")
_VALID_VERSION_RE = re.compile(r"^[a-zA-Z0-9_.+-]+$")

_TEST_COMMAND = "fx build //:default //build/bazel/rules/tests"


def _update_module_file(path: Path, dep_name: str, new_version: str) -> bool:
    """Update bazel_dep and single_version_override for `dep_name` in `path`.

    Returns True if `dep_name` was found in `path`, False otherwise.
    """
    content = path.read_text()

    bazel_dep_pattern = re.compile(
        rf'(bazel_dep\(\s*name\s*=\s*"{re.escape(dep_name)}"\s*,\s*version\s*=\s*")([^"]+)(")'
    )
    content, dep_count = bazel_dep_pattern.subn(
        rf"\g<1>{new_version}\g<3>", content
    )

    override_pattern = re.compile(
        rf'(single_version_override\(\s*(?:[^)]*\n)*?\s*module_name\s*=\s*"{re.escape(dep_name)}"\s*,\s*(?:[^)]*\n)*?\s*version\s*=\s*")([^"]+)(")'
    )
    content, override_count = override_pattern.subn(
        rf"\g<1>{new_version}\g<3>", content
    )

    if dep_count == 0 and override_count == 0:
        return False

    path.write_text(content)
    return True


def _create_argparser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="Example: %(prog)s rules_cc 0.2.25",
    )
    parser.add_argument(
        "dep_name",
        help="Name of the vendored Bazel dependency to uprev (e.g., rules_cc).",
    )
    parser.add_argument(
        "new_version",
        help="Target version to uprev to (e.g., 0.2.25).",
    )
    parser.add_argument(
        "--commit",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Commit the results to Git (default: True).",
    )
    return parser


def _check_workspace_clean(fuchsia_dir: Path) -> tuple[bool, str]:
    """Check if the git workspace has any uncommitted changes."""
    res = subprocess.run(
        ["git", "--no-optional-locks", "status", "--porcelain"],
        cwd=fuchsia_dir,
        capture_output=True,
        text=True,
    )
    if res.returncode != 0:
        return False, res.stderr.strip()
    output = res.stdout.strip()
    return len(output) == 0, output


def _get_vendor_dir(dep_name: str) -> Path:
    """Find the vendored directory for dep_name under third_party/bazel_vendor/fuchsia_in_tree."""
    for candidate_name in (f"{dep_name}+", dep_name):
        candidate_path = _BAZEL_VENDOR_DIR / candidate_name
        if (_FUCHSIA_DIR / candidate_path).is_dir():
            return candidate_path
    return _BAZEL_VENDOR_DIR / f"{dep_name}+"


def _get_registry_dir(dep_name: str) -> Path:
    """Find the registry directory for dep_name under BCR modules."""
    return _BAZEL_REGISTRY_DIR / dep_name


def _update_module_files(dep_name: str, new_version: str) -> bool:
    """Update all in-tree MODULE.bazel files for dep_name to new_version.

    Returns True if all files were updated successfully, False otherwise.
    """
    for rel_path in _MODULE_FILES:
        module_path = _FUCHSIA_DIR / rel_path
        if not _update_module_file(module_path, dep_name, new_version):
            print(
                f"Error: dependency {dep_name!r} not found in {rel_path}",
                file=sys.stderr,
            )
            return False
        print(f"Updated {dep_name} to {new_version} in {rel_path}")
    return True


def _run_fx_update_bazel_vendor_dir() -> int:
    """Run fx update-bazel-vendor-dir to re-vendor dependencies."""
    fx_path = _FUCHSIA_DIR / ".jiri_root" / "bin" / "fx"
    cmd = [str(fx_path), "update-bazel-vendor-dir"]
    print(f"\nRunning: {' '.join(cmd)}")
    res = subprocess.run(cmd, cwd=_FUCHSIA_DIR)
    return res.returncode


def _commit_files(dep_name: str, new_version: str) -> int:
    """Stage modified MODULE files and vendor/registry directories and commit them."""
    files_to_add: list[str] = [str(rel_path) for rel_path in _MODULE_FILES]
    registry_dir = _get_registry_dir(dep_name)
    if (_FUCHSIA_DIR / registry_dir).exists():
        files_to_add.append(str(registry_dir))
    vendor_dir = _get_vendor_dir(dep_name)
    if (_FUCHSIA_DIR / vendor_dir).exists():
        files_to_add.append(str(vendor_dir))

    print(f"\nStaging: {' '.join(files_to_add)}")
    git_add_res = subprocess.run(
        ["git", "add", "--"] + files_to_add, cwd=_FUCHSIA_DIR
    )
    if git_add_res.returncode != 0:
        return git_add_res.returncode

    command_str = (
        f"build/bazel/scripts/manual/uprev_bazel_vendor_dep.py "
        f"{dep_name} {new_version}"
    )
    commit_msg = (
        f"[bazel] Update {dep_name} to {new_version}\n\n"
        f"{command_str}\n\n"
        f"Test: {_TEST_COMMAND}\n"
    )
    print("\nCommitting changes...")
    git_commit_res = subprocess.run(
        ["git", "commit", "-m", commit_msg], cwd=_FUCHSIA_DIR
    )
    if git_commit_res.returncode != 0:
        return git_commit_res.returncode

    print(
        f"\nCommitted the results.\n\n"
        f"Further modifications may be necessary if the new version includes incompatible changes.\n\n"
        f"Please run the following command to test the build:\n"
        f"  {_TEST_COMMAND}\n\n"
        f'If "If This Then That" checks fail after uploading to Gerrit, consider adding the following footer to the commit message:\n'
        f"  NO_IFTTT=Ignore incompatible IFTTT comments in third-party code."
    )
    return 0


def main(argv: Sequence[str]) -> int:
    parser = _create_argparser()
    if not argv:
        parser.print_help()
        return 1

    args = parser.parse_args(argv)

    if not _VALID_DEP_NAME_RE.match(args.dep_name):
        parser.error(f"Invalid dependency name: {args.dep_name!r}")
    if not _VALID_VERSION_RE.match(args.new_version):
        parser.error(f"Invalid version string: {args.new_version!r}")

    is_clean, dirty_status = _check_workspace_clean(_FUCHSIA_DIR)
    if not is_clean:
        print(
            "Error: Git workspace is not clean. Please commit or stash changes before running:\n"
            + dirty_status,
            file=sys.stderr,
        )
        return 1

    if not _update_module_files(args.dep_name, args.new_version):
        return 1

    fx_returncode = _run_fx_update_bazel_vendor_dir()
    if fx_returncode != 0:
        return fx_returncode

    if args.commit:
        return _commit_files(args.dep_name, args.new_version)

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
