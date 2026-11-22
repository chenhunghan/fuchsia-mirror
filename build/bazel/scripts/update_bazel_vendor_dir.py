#!/usr/bin/env fuchsia-vendored-python

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Sequence


def merge_bcr_registry(src_bcr_dir: Path, dst_bcr_dir: Path) -> None:
    """Merge BCR files from src_bcr_dir into dst_bcr_dir without deleting existing files."""
    if src_bcr_dir.exists():
        shutil.copytree(src_bcr_dir, dst_bcr_dir, dirs_exist_ok=True)


def _vendor_dir_priority_key(vendor_dir: Path) -> tuple[bool, str]:
    """Sort key placing fuchsia_in_tree first, then other vendor dirs alphabetically."""
    return (vendor_dir.name != "fuchsia_in_tree", vendor_dir.name)


def get_reference_vendor_dirs(vendor_dir: Path) -> list[Path]:
    """Return higher-priority sibling vendor directories in deterministic order."""
    parent = vendor_dir.parent
    if not parent.is_dir():
        return []
    target_key = _vendor_dir_priority_key(vendor_dir)
    siblings = [
        d
        for d in parent.iterdir()
        if d.is_dir()
        and not d.is_symlink()
        and d != vendor_dir
        and (d / "VENDOR.bazel").is_file()
        and _vendor_dir_priority_key(d) < target_key
    ]
    return sorted(siblings, key=_vendor_dir_priority_key)


def dirs_are_equal(dir1: Path, dir2: Path) -> bool:
    """Return True if two real directory trees have identical entries and file contents."""
    if not dir1.is_dir() or dir1.is_symlink():
        return False
    if not dir2.is_dir() or dir2.is_symlink():
        return False

    entries1 = sorted(p.name for p in dir1.iterdir())
    entries2 = sorted(p.name for p in dir2.iterdir())
    if entries1 != entries2:
        return False

    for name in entries1:
        p1 = dir1 / name
        p2 = dir2 / name
        if p1.is_symlink() != p2.is_symlink():
            return False
        if p1.is_symlink():
            if p1.readlink() != p2.readlink():
                return False
        elif p1.is_dir():
            if not p2.is_dir() or not dirs_are_equal(p1, p2):
                return False
        elif p1.is_file():
            if not p2.is_file():
                return False
            if p1.stat().st_size != p2.stat().st_size:
                return False
            if p1.read_bytes() != p2.read_bytes():
                return False
        else:
            return False

    return True


def deduplicate_vendor_dir(vendor_dir: Path) -> list[str]:
    """Replace repo directories in vendor_dir that match a reference sibling with relative symlinks.

    Returns the sorted list of deduplicated repository names.
    """
    ref_dirs = get_reference_vendor_dirs(vendor_dir)
    if not ref_dirs:
        return []

    deduped: list[str] = []
    for entry in sorted(vendor_dir.iterdir(), key=lambda p: p.name):
        if not entry.is_dir() or entry.is_symlink():
            continue
        for ref_dir in ref_dirs:
            candidate = ref_dir / entry.name
            if dirs_are_equal(entry, candidate):
                shutil.rmtree(entry)
                target_rel = Path("..") / ref_dir.name / entry.name
                entry.symlink_to(target_rel)
                print(
                    f"Deduplicated {vendor_dir.name}/{entry.name} -> {target_rel}"
                )
                deduped.append(entry.name)
                break
    return deduped


def find_check_licenses_readmes_base(vendor_dir: Path) -> Path | None:
    """Return the check-licenses readmes base dir for vendor_dir's parent, if present.

    For `<fuchsia_dir>/third_party/bazel_vendor/<subdir>`, this returns
    `<fuchsia_dir>/tools/check-licenses/assets/readmes/third_party/bazel_vendor`
    if `<fuchsia_dir>/tools/check-licenses/assets/readmes` exists.
    """
    resolved = vendor_dir.resolve()
    if (
        len(resolved.parts) >= 4
        and resolved.parent.name == "bazel_vendor"
        and resolved.parent.parent.name == "third_party"
    ):
        fuchsia_dir = resolved.parent.parent.parent
        readmes_root = fuchsia_dir / "tools/check-licenses/assets/readmes"
        if readmes_root.is_dir():
            return readmes_root / "third_party/bazel_vendor"
    return None


def sync_check_licenses_readmes(
    vendor_dir: Path,
    readmes_base: Path,
    requested_repos: set[str] | None = None,
) -> None:
    """Synchronize check-licenses README.fuchsia directories for vendor_dir.

    - Removes README directories under `readmes_base / vendor_dir.name` for
      repositories that no longer exist, have an in-tree README.fuchsia, or have
      been deduplicated into symlinks.
    - Copies missing README directories from sibling vendor subdirectories under
      `readmes_base` for real (non-symlink) repository directories that do not
      have an in-tree README.fuchsia.
    - Emits a warning with instructions if a real repository directory still
      lacks a README.fuchsia file.
    """
    subdir_readmes = readmes_base / vendor_dir.name
    if subdir_readmes.is_dir():
        for entry in sorted(subdir_readmes.iterdir(), key=lambda p: p.name):
            vendor_entry = vendor_dir / entry.name
            keep = (
                vendor_entry.is_dir()
                and not vendor_entry.is_symlink()
                and not (vendor_entry / "README.fuchsia").exists()
            ) or (
                not vendor_entry.exists()
                and not vendor_entry.is_symlink()
                and requested_repos is not None
                and entry.name in requested_repos
            )
            if not keep:
                if entry.is_dir() and not entry.is_symlink():
                    shutil.rmtree(entry)
                else:
                    entry.unlink()
                print(f"Removed redundant check-licenses readme: {entry}")
        if not any(subdir_readmes.iterdir()):
            subdir_readmes.rmdir()

    sibling_readme_dirs = []
    if readmes_base.is_dir():
        sibling_readme_dirs = sorted(
            (
                d
                for d in readmes_base.iterdir()
                if d.is_dir()
                and not d.is_symlink()
                and d.name != vendor_dir.name
            ),
            key=_vendor_dir_priority_key,
        )

    missing_repos: list[Path] = []
    for vendor_entry in sorted(vendor_dir.iterdir(), key=lambda p: p.name):
        if not vendor_entry.is_dir() or vendor_entry.is_symlink():
            continue
        if (vendor_entry / "README.fuchsia").exists():
            continue
        target_readme_dir = subdir_readmes / vendor_entry.name
        if (target_readme_dir / "README.fuchsia").is_file():
            continue

        copied = False
        for sibling_dir in sibling_readme_dirs:
            candidate_dir = sibling_dir / vendor_entry.name
            if (candidate_dir / "README.fuchsia").is_file():
                target_readme_dir.parent.mkdir(parents=True, exist_ok=True)
                shutil.copytree(
                    candidate_dir, target_readme_dir, dirs_exist_ok=True
                )
                print(
                    f"Copied check-licenses readme from {sibling_dir.name}:"
                    f" {target_readme_dir}"
                )
                copied = True
                break

        if not copied:
            missing_repos.append(target_readme_dir / "README.fuchsia")

    if missing_repos:
        print(
            "WARNING: The following vendored repositories are missing"
            " README.fuchsia files required by `fx check-licenses`:\n"
            + "\n".join(f"  - {p}" for p in missing_repos)
            + "\nPlease create them before running `fx check-licenses`.",
            file=sys.stderr,
        )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--bazel",
        type=Path,
        required=True,
        help="Path to Bazel executable",
    )
    parser.add_argument(
        "--workspace",
        type=Path,
        required=True,
        help="Path to Bazel workspace",
    )
    parser.add_argument(
        "--bazel-vendor-dir",
        type=Path,
        required=True,
        help="Bazel vendor directory to write to",
    )
    parser.add_argument(
        "--bazel-registry-dir",
        type=Path,
        required=True,
        help="Bazel registry directory to write to",
    )
    parser.add_argument(
        "--merge-registry",
        action="store_true",
        help=(
            "Merge new BCR module definitions into --bazel-registry-dir"
            " instead of replacing it"
        ),
    )
    parser.add_argument(
        "--repo",
        action="append",
        required=True,
        help="Bazel repositories to vendor",
    )
    parser.add_argument(
        "--stub-repo",
        action="append",
        default=[],
        help=(
            "Bazel repositories that are manually stubbed out, their existing"
            " content should be preserved after the update"
        ),
    )
    parser.add_argument(
        "--exclude",
        action="append",
        default=[],
        help=(
            "Glob pattern, relative to the Bazel vendor directory, of files or"
            " directories to remove from vendored repositories (e.g."
            " 'rules_rust+/*/cargo-bazel')"
        ),
    )
    args = parser.parse_args(argv)

    # Check for empty values, otherwise the rmdir / copytree operations
    # below will erase your complete Fuchsia checkout. Ask me how I know :)
    assert args.bazel_vendor_dir, "Empty --bazel-vendor-dir!"
    assert args.bazel_registry_dir, "Empty --bazel-registry-dir!"

    # Always do a clean vendor in a temporary directory to drop unwanted repos.
    with tempfile.TemporaryDirectory() as tmp:
        temp_dir = Path(tmp) / "vendor"
        temp_dir.mkdir(parents=True, exist_ok=True)
        cmd = [
            args.bazel,
            "vendor",
            f"--vendor_dir={temp_dir}",
        ]
        cmd += [f"--repo={repo}" for repo in args.repo]

        print(" ".join(str(s) for s in cmd))

        subprocess.check_call(
            cmd,
            cwd=args.workspace,
        )

        # Persist manually stubbed repos from the existing vendor dir (or a
        # reference sibling vendor dir when creating a new subdirectory) before
        # pinning them in VENDOR.bazel and running the second `bazel vendor`
        # validation pass.
        for repo in args.stub_repo:
            repo_name = repo.lstrip("@")
            src_stub = args.bazel_vendor_dir / repo_name
            if not src_stub.exists():
                for ref_dir in get_reference_vendor_dirs(args.bazel_vendor_dir):
                    candidate = ref_dir / repo_name
                    if candidate.exists():
                        src_stub = candidate
                        break
            shutil.copytree(src_stub.resolve(), temp_dir / repo_name)

        # Pin all the vendored and stub directories, to make sure Bazel doesn't try
        # to update any of them when building.
        with open(temp_dir / "VENDOR.bazel", "w") as f:
            f.write(
                """################################################################################
# @generated
# DO NOT MODIFY: This file is auto-generated by the following command, instead
# of changing this file directly, rerun this command:
#
#    fx update-bazel-vendor-dir
################################################################################

"""
            )
            for repo in sorted(args.repo + args.stub_repo):
                f.write(f'pin("{repo}")\n')

        # Vendor again after all repositories are pinned to drop the `.marker` files.
        # This also verifies the VENDOR.bazel file re-written above is valid.
        subprocess.check_call(
            cmd,
            cwd=args.workspace,
            # Suppress stdout and stderr to avoid user confusion.
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        # Remove excluded content from vendored repositories.
        for pattern in args.exclude:
            if not pattern or Path(pattern).is_absolute() or ".." in pattern:
                parser.error(f"Invalid --exclude pattern: {pattern!r}")
            matches = sorted(temp_dir.glob(pattern))
            if not matches:
                print(
                    f"WARNING: --exclude pattern {pattern!r} did not match"
                    " anything.",
                    file=sys.stderr,
                )
                continue
            for match in matches:
                assert match.is_relative_to(temp_dir), match
                print(f"Excluding {match.relative_to(temp_dir)}")
                if match.is_dir() and not match.is_symlink():
                    shutil.rmtree(match)
                else:
                    match.unlink()

        # Update registry directory with new content.
        if not args.bazel_registry_dir.resolve().is_relative_to(
            args.bazel_vendor_dir.resolve()
        ):
            src_bcr_dir = temp_dir / "_registries/bcr.bazel.build"
            if args.merge_registry:
                merge_bcr_registry(src_bcr_dir, args.bazel_registry_dir)
            else:
                shutil.rmtree(args.bazel_registry_dir, ignore_errors=True)
                if src_bcr_dir.exists():
                    args.bazel_registry_dir.parent.mkdir(
                        parents=True, exist_ok=True
                    )
                    shutil.move(src_bcr_dir, args.bazel_registry_dir)
            shutil.rmtree(temp_dir / "_registries", ignore_errors=True)

        # Remove the local output_base symlink generated by `bazel vendor`.
        (temp_dir / "bazel-external").unlink(missing_ok=True)

        for filename in ("README.md", "OWNERS"):
            existing_file = args.bazel_vendor_dir / filename
            if existing_file.exists():
                shutil.move(existing_file, temp_dir / filename)

        shutil.rmtree(args.bazel_vendor_dir, ignore_errors=True)
        args.bazel_vendor_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(temp_dir, args.bazel_vendor_dir)

        # Remove any gitignored files inside the vendor directory (e.g.
        # Cargo.toml, Cargo.lock, .pyc, .orig) so they are not left untracked on
        # disk when switching commits.
        #
        # IMPORTANT: Only run `git clean -fdX` after verifying that VENDOR.bazel
        # itself is not ignored by Git (`git check-ignore -q` returns 1).
        # Otherwise, if `--bazel-vendor-dir` is inside a gitignored parent
        # directory (such as `/out/` during unit tests, or a new `/third_party/*`
        # subdirectory not yet un-ignored in `.gitignore`), Git matches the
        # ignored parent directory rule and `git clean -fdX` will wipe out all
        # the vendored files (and even the enclosing `/out/` directory)!
        #
        # Note: `git check-ignore -q` returns:
        #   0   -> path is ignored by Git (do NOT run `git clean -fdX`)
        #   1   -> inside a Git worktree and path is NOT ignored (safe to clean)
        #   128 -> not inside a Git repository (skip `git clean -fdX`)
        resolved_vendor_dir = args.bazel_vendor_dir.resolve()
        if (
            subprocess.run(
                [
                    "git",
                    "check-ignore",
                    "-q",
                    str(resolved_vendor_dir / "VENDOR.bazel"),
                ],
                cwd=resolved_vendor_dir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 1
        ):
            subprocess.run(
                ["git", "clean", "-fdX", str(resolved_vendor_dir)],
                cwd=resolved_vendor_dir,
                stdout=subprocess.DEVNULL,
                check=True,
            )

        deduplicate_vendor_dir(args.bazel_vendor_dir)
        readmes_base = find_check_licenses_readmes_base(args.bazel_vendor_dir)
        if readmes_base is not None:
            requested_repos = {repo.lstrip("@") for repo in args.repo}
            sync_check_licenses_readmes(
                args.bazel_vendor_dir, readmes_base, requested_repos
            )

    return 0


if __name__ == "__main__":
    sys.exit(main())
