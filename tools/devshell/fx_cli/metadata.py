# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Dependency-free metadata readers for Fuchsia devshell lib files."""

import contextlib
import dataclasses
import importlib
import os
import pathlib
import re
import types
from collections.abc import Mapping
from importlib import resources

data: types.ModuleType | None
try:
    data = importlib.import_module("data")
except ImportError:
    data = None

_FEATURE_SPEC_LINE_RE = re.compile(r'^"([^"|]+)\|(enabled|disabled)\|(.*)"$')


def _normalize_build_dir(
    fuchsia_dir: pathlib.Path, raw: str
) -> pathlib.Path | None:
    """Normalize a build directory path matching vars.sh:_fx-normalize-build-dir."""
    cleaned = raw.strip().rstrip("/")
    if not cleaned:
        return None
    if cleaned.startswith("//"):
        return fuchsia_dir / cleaned.lstrip("/")
    candidate = pathlib.Path(cleaned)
    return candidate if candidate.is_absolute() else fuchsia_dir / candidate


@dataclasses.dataclass(frozen=True)
class OptionalFeature:
    """Metadata for an fx optional feature (--enable/--disable)."""

    name: str
    default_enabled: bool
    description: tuple[str, ...]


def find_lib_file(
    fuchsia_dir: pathlib.Path | None, filename: str
) -> pathlib.Path | None:
    """Locate a file under tools/devshell/lib relative to fuchsia_dir or this module."""
    if fuchsia_dir is not None:
        candidate = fuchsia_dir / "tools" / "devshell" / "lib" / filename
        if candidate.is_file():
            return candidate
    rel_candidate = (
        pathlib.Path(__file__).resolve().parent.parent / "lib" / filename
    )
    if rel_candidate.is_file():
        return rel_candidate
    return None


def read_lib_text(fuchsia_dir: pathlib.Path | None, filename: str) -> str:
    """Read UTF-8 text from a devshell lib file, falling back to packaged data."""
    path = find_lib_file(fuchsia_dir, filename)
    if path is not None:
        with contextlib.suppress(OSError):
            return path.read_text(encoding="utf-8", errors="replace")
    if data is not None:
        with contextlib.suppress(FileNotFoundError, OSError, TypeError):
            with resources.files(data).joinpath(filename).open(
                "r", encoding="utf-8"
            ) as f:
                return f.read()
    return ""


def load_agent_env_vars(
    fuchsia_dir: pathlib.Path | None = None,
) -> tuple[str, ...]:
    """Read agent environment variable names from agents.txt and agent_session_ids.txt."""
    seen: set[str] = set()
    env_vars: list[str] = []
    for filename in ("agents.txt", "agent_session_ids.txt"):
        if (
            fuchsia_dir is not None
            and find_lib_file(fuchsia_dir, filename) is None
        ):
            text = read_lib_text(None, filename)
        else:
            text = read_lib_text(fuchsia_dir, filename)
        for raw_line in text.splitlines():
            var = raw_line.strip()
            if var and not var.startswith("#") and var not in seen:
                seen.add(var)
                env_vars.append(var)
    return tuple(env_vars)


def load_optional_features_meta(
    fuchsia_dir: pathlib.Path | None = None,
) -> tuple[OptionalFeature, ...]:
    """Parse optional features metadata from tools/devshell/lib/fx-optional-features.sh."""
    if (
        fuchsia_dir is not None
        and find_lib_file(fuchsia_dir, "fx-optional-features.sh") is None
    ):
        text = read_lib_text(None, "fx-optional-features.sh")
    else:
        text = read_lib_text(fuchsia_dir, "fx-optional-features.sh")
    grouped: dict[str, tuple[bool, list[str]]] = {}
    in_spec = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if line.startswith("_FX_OPTIONAL_FEATURES_SPEC=("):
            grouped.clear()
            in_spec = True
            continue
        if in_spec:
            if line.startswith(")"):
                in_spec = False
                continue
            m = _FEATURE_SPEC_LINE_RE.match(line)
            if m:
                name, state_str, desc_line = m.groups()
                default_en = state_str == "enabled"
                if name not in grouped:
                    grouped[name] = (default_en, [desc_line])
                else:
                    grouped[name][1].append(desc_line)
    return tuple(
        OptionalFeature(
            name=name,
            default_enabled=default_en,
            description=tuple(desc_lines),
        )
        for name, (default_en, desc_lines) in grouped.items()
    )


def resolve_build_dir(
    fuchsia_dir: pathlib.Path,
    build_dir_override: str | None = None,
    *,
    env: Mapping[str, str] | None = None,
) -> pathlib.Path | None:
    """Resolve the active Fuchsia build directory without invoking bash."""
    env_map = os.environ if env is None else env
    for raw in (
        build_dir_override,
        env_map.get("_FX_BUILD_DIR"),
        env_map.get("FUCHSIA_BUILD_DIR"),
    ):
        if (
            raw is not None
            and (normalized := _normalize_build_dir(fuchsia_dir, raw))
            is not None
        ):
            return normalized
    fx_build_dir_file = fuchsia_dir / ".fx-build-dir"
    if fx_build_dir_file.is_file():
        try:
            lines = fx_build_dir_file.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            return None
        if lines:
            return _normalize_build_dir(fuchsia_dir, lines[0])
    return None
