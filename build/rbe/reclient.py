#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Common configuration, flags, and canonical path helpers for reclient."""

import posixpath
from collections.abc import Iterator
from pathlib import Path, PurePath

from build.rbe import cl_utils

# Internal implementation detail of reclient when `--canonicalize_working_dir`
# is enabled (reproxy replaces the first working-directory path component with
# `set_by_reclient` and remaining components with `a`).
_CANONICAL_DIR_PREFIX = "set_by_reclient"

_SCRIPT_DIR = Path(__file__).parent
_GCERT_AUTH_CFG: Path = _SCRIPT_DIR / "fuchsia-reproxy-gcertauth.cfg"


def cfg_path_to_flags(cfg_path: Path) -> list[str]:
    """Converts a reclient config file into command-line flags."""
    cfg_dict = cl_utils.read_config_file_lines(
        cfg_path.read_text().splitlines()
    )
    return [f"--{k}={v}" for k, v in cfg_dict.items()]


def gcert_auth_flags() -> list[str]:
    """Returns reclient command-line flags for LOAS (gcert) authentication."""
    return cfg_path_to_flags(_GCERT_AUTH_CFG)


def gce_auth_flags() -> list[str]:
    """Returns reclient command-line flags for GCE machine authentication."""
    return ["--use_gce_credentials=true"]


def adc_auth_flags() -> list[str]:
    """Returns reclient command-line flags for Application Default Credentials."""
    return ["--use_application_default_credentials=true"]


def _canonical_working_dir_components(
    subdir_components: Iterator[str],
) -> Iterator[str]:
    """Computes the path used by rewrapper --canonicalize_working_dir=true.

    The exact values returned are an implementation detail of reclient
    that is not reliable, so this should only be used as a last resort
    in workarounds.

    https://team.git.corp.google.com/foundry-x/re-client/+/refs/heads/master/internal/pkg/reproxy/action.go#177

    Args:
      subdir_components: a relative path like ('out', 'default', ...)

    Yields:
      Replacement path components like ('set_by_reclient', 'a', ...)
    """
    first = next(subdir_components, None)
    if first is None or first == "":
        return  # no components
    yield _CANONICAL_DIR_PREFIX
    for _ in subdir_components:
        yield "a"


def canonical_working_dir(build_subdir: Path) -> Path:
    return Path(*_canonical_working_dir_components(iter(build_subdir.parts)))


def is_canonical_working_dir_path(rel_path: PurePath) -> bool:
    """Returns True if an exec-root-relative path starts with set_by_reclient."""
    return bool(rel_path.parts and rel_path.parts[0] == _CANONICAL_DIR_PREFIX)


def normalized_exec_root_relpath(path: Path, build_subdir: Path) -> Path | None:
    """Extracts an exec-root-relative path anchored at the canonical working dir.

    When rewrapper runs with `--canonicalize_working_dir=true`, the working
    directory inside the remote execution root is `set_by_reclient/a/...`.
    Tools on remote workers (such as BuildGrid workers using dynamic sandbox
    prefixes) may emit depfile paths like:
      * `/worker/sandbox/set_by_reclient/a/gen/foo.h`
      * `set_by_reclient/a/gen/foo.h`
      * `../../set_by_reclient/a/gen/foo.h`

    Relative paths with non-`..` leading directories (such as
    `obj/set_by_reclient/foo.h`) are left untouched because they denote a
    literal subdirectory under the current working directory rather than the
    canonical working directory root.

    Args:
      path: An absolute or relative dependency path emitted by a remote tool.
      build_subdir: The relative path from the execution root to the build
        working directory (e.g. `Path("out/default")`).

    Returns:
      Exec-root-relative Path (e.g. `Path("set_by_reclient/a/gen/foo.h")`), or
      None if `build_subdir` has no canonical working directory, `path` is not
      anchored at that canonical working directory, `path` has a mismatched
      leading `..` depth, or `path` escapes above the exec root via `..`.
    """
    parts = path.parts
    if _CANONICAL_DIR_PREFIX not in parts:
        return None

    start_parts = canonical_working_dir(build_subdir).parts
    depth = len(start_parts)
    if not depth:
        return None

    if not path.is_absolute():
        if parts[:depth] == start_parts:
            idx = 0
        elif parts[: 2 * depth] == ("..",) * depth + start_parts:
            idx = depth
        else:
            return None
    elif (
        idx_match := next(
            (
                i
                for i in range(1, len(parts) - depth + 1)
                if parts[i : i + depth] == start_parts
            ),
            None,
        )
    ) is not None:
        idx = idx_match
    else:
        return None

    norm_rel = posixpath.normpath(posixpath.join(*parts[idx:]))
    rel_path = Path(norm_rel)
    if rel_path.parts and rel_path.parts[0] == "..":
        return None
    return rel_path


def relativize_canonical_path(path: Path, build_subdir: Path) -> Path | None:
    """Relativizes a `set_by_reclient`-anchored path to the canonical working dir.

    Args:
      path: An absolute or relative dependency path emitted by a remote tool.
      build_subdir: The relative path from the execution root to the build
        working directory (e.g. `Path("out/default")`).

    Returns:
      A Path relative to the canonical working directory corresponding to
      `build_subdir`, or None if `build_subdir` has no canonical working
      directory, `path` is not anchored at that canonical working directory,
      `path` has a mismatched leading `..` depth, or `path` escapes above the
      execution root.
    """
    if (
        exec_root_rel := normalized_exec_root_relpath(path, build_subdir)
    ) is None:
        return None
    return cl_utils.relpath(
        Path("/") / exec_root_rel,
        start=Path("/") / canonical_working_dir(build_subdir),
    )
