#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""RBE profile dataclass, loaders, and subcommand CLI."""

import dataclasses
import fnmatch
import json
import os
import pathlib
import posixpath
import sys
from collections.abc import Mapping
from typing import Any

from build.python.modules.serialization import serialization
from build.scripts import service_profile

# Environment variables used for serialization and child process communication.
# RBE_instance is read directly by reclient's reproxy tool to select the RBE endpoint.
_ENV_RBE_INSTANCE = "RBE_instance"
_ENV_EXEC_ROOT = "FX_BUILD_INTERNAL_RBE_REMOTE_EXEC_ROOT"
_ENV_CONTAINER_IMAGE = "FX_BUILD_INTERNAL_RBE_CONTAINER_IMAGE"
_ENV_OS_FAMILY = "FX_BUILD_INTERNAL_RBE_OS_FAMILY"
_ENV_WORKER_POOLS = "FX_BUILD_INTERNAL_RBE_WORKER_POOLS"
_ENV_AUTH_TYPE = "FX_BUILD_INTERNAL_RBE_AUTH_TYPE"
# Unlike ENV_PROFILE_NAME, this isn't in the @fuchsia_build_config repository
# rule's `environ`, so exporting it to builds doesn't make Bazel re-run that
# rule.
_ENV_INTERNAL_PROFILE_NAME = "FX_BUILD_INTERNAL_RBE_PROFILE_NAME"
# Lets users pick a profile for one command. Unlike the variables above, it's
# not set by to_env_dict(), because the @fuchsia_build_config repository rule
# re-runs whenever its value changes between Bazel commands.
ENV_PROFILE_NAME = "FX_BUILD_RBE_PROFILE"
# RBE_platform is read directly by reclient's rewrapper tool to select default platform properties.
_ENV_RBE_PLATFORM = "RBE_platform"

# Mandatory pool entries that must be present in worker_pools.
_REQUIRED_WORKER_POOLS = frozenset({"default", "large"})

# Path relative to fuchsia directory for RBE profiles directory
_PROFILES_DIR = pathlib.Path("build/rbe/profiles")

# Path relative to fuchsia directory for the active sticky RBE profile JSON config.
_CONFIG_PATH = pathlib.Path(".fx/config/rbe_profile.json")

_JSONPrimitive = str | int | float | bool | None
JSONValue = _JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]


def _has_glob_magic(part: str) -> bool:
    """Returns True if a path segment contains fnmatch wildcard characters."""
    return any(c in "*?[]" for c in part)


def _normalize_posix_abspath(
    path: pathlib.PurePosixPath,
) -> pathlib.PurePosixPath:
    """Lexically normalizes '.' and '..' segments in an absolute POSIX path."""
    return pathlib.PurePosixPath(posixpath.normpath(str(path)))


def _validate_remote_exec_root(
    root: pathlib.PurePosixPath,
) -> pathlib.PurePosixPath:
    """Validates and normalizes an RbeProfile remote_exec_root path/pattern.

    Args:
        root: Configured remote execution workspace root path or fixed-depth
            single-segment glob pattern.

    Returns:
        Lexically normalized PurePosixPath.

    Raises:
        ValueError: If `root` is not an absolute single-slash POSIX path,
            contains `..` segments, or contains recursive `**` wildcards.
    """
    if not root.is_absolute() or root.parts[0] != "/":
        raise ValueError(
            f"remote_exec_root must be an absolute path, got '{root}'"
        )
    exec_root_parts = root.parts[1:]
    if ".." in exec_root_parts:
        raise ValueError(
            f"remote_exec_root must not contain '..' segments, got '{root}'"
        )
    if any("**" in part for part in exec_root_parts):
        raise ValueError(
            "remote_exec_root does not support recursive '**' wildcards "
            f"(only fixed-depth single-segment globs are allowed), got '{root}'"
        )
    return _normalize_posix_abspath(root)


@dataclasses.dataclass
class RbeProfile:
    """Represents a validated Remote Build Execution (RBE) profile configuration.

    Attributes:
        rbe_instance: The fully-qualified RBE instance name. Must match the
            standard format: `projects/{project_id}/instances/{instance_name}`
            (e.g., `projects/rbe-fuchsia-prod/instances/default`).
        remote_exec_root: The absolute root path (or fixed-depth single-segment
            glob pattern, such as `/tmp/buildbox-*` or `/`) of the execution
            workspace on remote workers (typically `/b/f/w`). Recursive `**`
            wildcards are not supported.
        container_image: The fully-qualified Docker container image URI for
            the baseline remote execution environment (e.g. `docker://gcr.io/...`).
            Individual actions can override this per-action (in GN/Ninja via
            `--platform=container-image=<uri>` and in Bazel via
            `exec_properties = {"container-image": "<uri>"}`).
            Note: If multiple named container environments are needed in the
            future (e.g. clang_tot, android_ndk, gpu), an optional
            `container_images: dict[str, str]` mapping abstract environment
            names to concrete URIs can be added to the profile schema.
        worker_pools: A dictionary mapping abstract worker pool names (e.g.,
            "default", "large") to concrete remote execution platform property
            strings formatted as "key=value" (e.g., "Pool=default",
            "Pool=large_worker"). In reclient/rewrapper, these are forwarded via
            `--platform=key=value`. In Bazel, these are parsed into
            `exec_properties = {"key": "value"}`. Both the property key (e.g.,
            "Pool") and property value are derived directly from this string.
        auth_type: Authentication mechanism ("google" or "none").
    """

    rbe_instance: str
    remote_exec_root: pathlib.PurePosixPath
    container_image: str
    # LINT.IfChange(rbe_worker_pools)
    worker_pools: dict[str, str]
    # LINT.ThenChange(//build/bazel_sdk/bazel_rules_fuchsia/common/fuchsia_platform_build.bzl:rbe_worker_pools)
    profile_name: str
    os_family: str
    auth_type: str = "google"

    def __post_init__(self) -> None:
        if not self.rbe_instance.startswith("projects/"):
            raise ValueError(
                f"rbe_instance must start with 'projects/', got '{self.rbe_instance}'"
            )
        self.remote_exec_root = _validate_remote_exec_root(
            self.remote_exec_root
        )
        missing_pools = _REQUIRED_WORKER_POOLS - set(self.worker_pools.keys())
        if missing_pools:
            raise ValueError(
                f"worker_pools is missing required pool entries: {sorted(missing_pools)}"
            )
        for pool_name, pool_spec in self.worker_pools.items():
            if not isinstance(pool_spec, str) or "=" not in pool_spec:
                raise ValueError(
                    f"worker_pools['{pool_name}'] must be a 'key=value' platform property string, got '{pool_spec}'"
                )

    @property
    def remote_exec_root_has_wildcards(self) -> bool:
        """Returns True if remote_exec_root contains any glob wildcards."""
        return any(
            _has_glob_magic(part) for part in self.remote_exec_root.parts[1:]
        )

    def _remote_exec_root_suffix(
        self, path: pathlib.PurePath | str
    ) -> tuple[str, ...] | None:
        raw_path = pathlib.PurePosixPath(path)
        if not raw_path.is_absolute() or raw_path.parts[0] != "/":
            return None
        norm_path = _normalize_posix_abspath(raw_path)
        pattern_parts = self.remote_exec_root.parts[1:]
        actual_parts = norm_path.parts[1:]
        prefix_len = len(pattern_parts)
        if len(actual_parts) < prefix_len:
            return None
        if not all(
            fnmatch.fnmatchcase(actual, pat)
            for actual, pat in zip(actual_parts[:prefix_len], pattern_parts)
        ):
            return None
        return actual_parts[prefix_len:]

    def matches_remote_exec_root(
        self, actual_root: pathlib.PurePath | str
    ) -> bool:
        """Returns True if an observed worker root path matches remote_exec_root."""
        return self._remote_exec_root_suffix(actual_root) == ()

    def strip_remote_exec_root(
        self, path: pathlib.PurePath | str
    ) -> pathlib.PurePosixPath | None:
        """Strips the remote_exec_root prefix from an absolute descendant path.

        Returns the relative path under remote_exec_root if `path` is strictly
        inside a directory matching `remote_exec_root`, or None otherwise.
        """
        suffix = self._remote_exec_root_suffix(path)
        if not suffix:
            return None
        return pathlib.PurePosixPath(*suffix)

    def format_remote_exec_root_mismatch(
        self, actual_root: pathlib.PurePath | str
    ) -> str:
        """Formats an actionable error message when a worker root mismatches."""
        return (
            f"Remote worker execution root '{actual_root}' does not match "
            f"remote_exec_root '{self.remote_exec_root}' configured in RBE "
            f"profile '{self.profile_name}'.\n"
            'Update "remote_exec_root" in your RBE profile JSON '
            "(fixed-depth single-segment globs like '/tmp/buildbox-*' and '/' "
            "are supported; recursive '**' wildcards are not supported)."
        )

    @property
    def quota_project(self) -> str:
        """Extracts the GCP project ID from rbe_instance, or empty string if not found."""
        if self.rbe_instance.startswith("projects/"):
            parts = self.rbe_instance.split("/")
            if len(parts) >= 2:
                return parts[1]
        return ""

    def to_env_dict(self) -> dict[str, str]:
        """Serializes properties to standard environment variables for subprocesses."""
        default_pool = self.worker_pools.get("default")
        if not default_pool:
            raise ValueError(
                "worker_pools must define a 'default' worker pool entry"
            )
        platform_spec = f"container-image={self.container_image},{default_pool}"
        return {
            _ENV_RBE_INSTANCE: self.rbe_instance,
            _ENV_EXEC_ROOT: str(self.remote_exec_root),
            _ENV_CONTAINER_IMAGE: self.container_image,
            _ENV_OS_FAMILY: self.os_family,
            _ENV_WORKER_POOLS: json.dumps(self.worker_pools),
            _ENV_AUTH_TYPE: self.auth_type,
            _ENV_INTERNAL_PROFILE_NAME: self.profile_name,
            _ENV_RBE_PLATFORM: platform_spec,
        }

    @classmethod
    def from_dict(cls, data: JSONObject) -> "RbeProfile":
        """Creates an RbeProfile instance from a raw dictionary."""
        try:
            return serialization.instance_from_dict(cls, data)
        except (KeyError, TypeError) as e:
            raise ValueError(f"Validation failed when parsing RbeProfile: {e}.")

    @classmethod
    def from_env_dict(cls, environ: Mapping[str, str]) -> "RbeProfile":
        """Reconstructs an RbeProfile instance from environment variables."""
        if _ENV_RBE_INSTANCE not in environ:
            raise ValueError(
                f"{_ENV_RBE_INSTANCE} must be present in environment"
            )
        rbe_instance = environ[_ENV_RBE_INSTANCE]

        exec_root_str = environ.get(_ENV_EXEC_ROOT)
        if not exec_root_str or not exec_root_str.startswith("/"):
            raise ValueError(
                f"{_ENV_EXEC_ROOT} must be an absolute path in environment, got '{exec_root_str}'"
            )

        container_image = environ.get(_ENV_CONTAINER_IMAGE)
        if not container_image:
            raise ValueError(
                f"{_ENV_CONTAINER_IMAGE} must be present in environment"
            )

        os_family = environ.get(_ENV_OS_FAMILY)
        if not os_family:
            raise ValueError(f"{_ENV_OS_FAMILY} must be present in environment")

        worker_pools_raw = environ.get(_ENV_WORKER_POOLS, "{}")
        try:
            worker_pools = json.loads(worker_pools_raw)
        except json.JSONDecodeError as e:
            raise ValueError(
                f"Failed to decode worker pools JSON from environment: {e}"
            )
        missing_pools = _REQUIRED_WORKER_POOLS - set(worker_pools.keys())
        if missing_pools:
            raise ValueError(
                f"worker_pools in environment is missing required pool entries: {sorted(missing_pools)}"
            )

        auth_type = environ.get(_ENV_AUTH_TYPE, "google")
        profile_name = environ.get(_ENV_INTERNAL_PROFILE_NAME, "default")

        return cls(
            rbe_instance=rbe_instance,
            remote_exec_root=pathlib.PurePosixPath(exec_root_str),
            container_image=container_image,
            worker_pools=worker_pools,
            profile_name=profile_name,
            os_family=os_family,
            auth_type=auth_type,
        )


def load(
    fuchsia_dir: pathlib.Path,
    environ: Mapping[str, str],
    name: str | None = None,
) -> RbeProfile:
    """Loads the active RBE profile, prioritizing env variables over on-disk files."""
    if _ENV_RBE_INSTANCE in environ and name is None:
        return RbeProfile.from_env_dict(environ)

    try:
        return _MANAGER.load(fuchsia_dir, environ, name=name)
    except FileNotFoundError as e:
        raise ValueError(f"Unknown RBE profile '{name}': {e}")


def fake(**overrides: Any) -> RbeProfile:
    """Factory helper generating valid in-memory RbeProfile instances for tests."""
    defaults: dict[str, Any] = {
        "rbe_instance": "projects/fake-project/instances/fake-instance",
        "remote_exec_root": pathlib.PurePosixPath("/fake/root"),
        "container_image": "docker://gcr.io/fake/image",
        "worker_pools": {
            "default": "Pool=fake-default-pool",
            "large": "Pool=fake-large-pool",
        },
        "profile_name": "fake-profile",
        "os_family": "fake-os",
        "auth_type": "fake-auth",
    }
    valid_keys = {
        "rbe_instance",
        "remote_exec_root",
        "container_image",
        "worker_pools",
        "profile_name",
        "os_family",
        "auth_type",
    }
    for k in overrides:
        if k not in valid_keys:
            raise ValueError(
                f"Unknown override '{k}'. Expected one of {valid_keys}"
            )
    defaults.update(overrides)
    if isinstance(defaults["remote_exec_root"], (str, pathlib.PurePath)):
        defaults["remote_exec_root"] = pathlib.PurePosixPath(
            defaults["remote_exec_root"]
        )
    return RbeProfile(**defaults)


_SPEC = service_profile.ServiceProfileSpec(
    service_name="rbe",
    env_profile_var=ENV_PROFILE_NAME,
    profiles_relpath=_PROFILES_DIR,
    config_relpath=_CONFIG_PATH,
    description="Manage RBE (Remote Build Execution) configuration profiles.",
)

_MANAGER = service_profile.ServiceProfileManager(RbeProfile, _SPEC)

resolve_paths = _MANAGER.resolve_paths
main = _MANAGER.main


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
