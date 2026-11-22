#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Materializes standalone no_workspace repositories via `bazel vendor`."""

import argparse
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parents[3]
_PRESERVED_FILENAMES = ("OWNERS", "README.md")
_THIRD_PARTY_PREFIX = "+third_party+"


def _discover_repos(
    bazel: pathlib.Path, workspace: pathlib.Path
) -> Sequence[str]:
    """Discovers canonical `+third_party+*` repo names from `MODULE.bazel`."""
    completed = subprocess.run(
        (str(bazel), "mod", "dump_repo_mapping", ""),
        cwd=workspace,
        stdout=subprocess.PIPE,
        check=True,
        text=True,
    )
    mapping = json.loads(completed.stdout)
    return tuple(
        sorted(
            {
                f"@@{canonical}"
                for canonical in mapping.values()
                if isinstance(canonical, str)
                and canonical.startswith(_THIRD_PARTY_PREFIX)
            }
        )
    )


def _clean_gitignored_files(vendor_dir: pathlib.Path):
    """Removes gitignored files in vendor_dir when vendor_dir itself is tracked."""
    resolved_vendor_dir = vendor_dir.resolve()
    # Probe a non-gitignored filename inside vendor_dir before running
    # `git clean -fdX` so we never wipe a directory located under a gitignored
    # parent (such as `/out/` or `/tmp/`):
    #   0   -> path is ignored by Git (skip `git clean -fdX`)
    #   1   -> inside a Git worktree and path is NOT ignored (safe to clean)
    #   128 -> outside any Git repository (skip `git clean -fdX`)
    ignore_probe = subprocess.run(
        ("git", "check-ignore", "-q", str(resolved_vendor_dir / "OWNERS")),
        cwd=resolved_vendor_dir,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
        text=True,
    )
    if ignore_probe.returncode == 1:
        subprocess.run(
            ("git", "clean", "-fdX", str(resolved_vendor_dir)),
            cwd=resolved_vendor_dir,
            stdout=subprocess.DEVNULL,
            check=True,
            text=True,
        )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=pathlib.Path,
        default=_FUCHSIA_DIR,
        help=(
            "Root of the repository containing manifests/third_party and"
            " third_party/bazel_vendor/no_workspace (defaults to checkout root;"
            " pass e.g. vendor/google for vendor repositories)"
        ),
    )
    parser.add_argument(
        "--bazel",
        type=pathlib.Path,
        help="Path to Bazel executable (defaults to 'bazel' on PATH)",
    )
    parser.add_argument(
        "--workspace",
        type=pathlib.Path,
        help=(
            "Path to standalone Bazel workspace directory (defaults to"
            " <repo-root>/manifests/third_party)"
        ),
    )
    parser.add_argument(
        "--bazel-vendor-dir",
        type=pathlib.Path,
        help=(
            "Destination no_workspace vendor directory (defaults to"
            " <repo-root>/third_party/bazel_vendor/no_workspace)"
        ),
    )
    parser.add_argument(
        "--repo",
        action="append",
        help=(
            "Canonical Bazel repository name to vendor (defaults to all"
            " +third_party+* repositories in MODULE.bazel)"
        ),
    )
    args = parser.parse_args(argv)

    if args.bazel is not None:
        bazel = args.bazel
    else:
        found_bazel = shutil.which("bazel")
        if found_bazel is None:
            parser.error(
                "Could not find 'bazel' on PATH; run via"
                " scripts/fuchsia-vendored-python or pass --bazel"
            )
        bazel = pathlib.Path(found_bazel)

    if args.workspace is not None:
        workspace = args.workspace
    else:
        workspace = args.repo_root / "manifests/third_party"

    if args.bazel_vendor_dir is not None:
        vendor_dir = args.bazel_vendor_dir
    else:
        vendor_dir = args.repo_root / "third_party/bazel_vendor/no_workspace"

    if not str(vendor_dir).strip():
        parser.error("--bazel-vendor-dir must not be empty")

    if args.repo:
        repos = tuple(args.repo)
    else:
        repos = _discover_repos(bazel, workspace)
        if not repos:
            parser.error(
                f"No {_THIRD_PARTY_PREFIX}* repositories found in {workspace}"
            )

    with tempfile.TemporaryDirectory() as tmp:
        temp_dir = pathlib.Path(tmp) / "vendor"
        temp_dir.mkdir(parents=True, exist_ok=True)
        cmd = (
            str(bazel),
            "vendor",
            f"--vendor_dir={temp_dir}",
            *(f"--repo={repo}" for repo in repos),
        )
        print(" ".join(cmd))
        subprocess.run(cmd, cwd=workspace, check=True, text=True)

        # Strip Bazel vendor-mode bookkeeping artifacts: no_workspace is
        # consumed via filesystem paths/symlinks rather than `--vendor_dir`.
        (temp_dir / "VENDOR.bazel").unlink(missing_ok=True)
        (temp_dir / "bazel-external").unlink(missing_ok=True)
        shutil.rmtree(temp_dir / "_registries", ignore_errors=True)
        for marker in sorted(temp_dir.glob("@*.marker")):
            marker.unlink()

        for filename in _PRESERVED_FILENAMES:
            existing_file = vendor_dir / filename
            if existing_file.exists():
                shutil.move(existing_file, temp_dir / filename)

        shutil.rmtree(vendor_dir, ignore_errors=True)
        vendor_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(temp_dir, vendor_dir)

    _clean_gitignored_files(vendor_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
