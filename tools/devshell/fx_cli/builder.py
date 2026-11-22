#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel host tool builder, stamp/manifest verifier, and staging materializer."""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, NoReturn, TextIO

_LOCK_UTILS_REL_PATH = Path("build/scripts/lock_utils.py")


def _find_import_root(start: Path) -> Path | None:
    """Walk ancestor directories from `start` to find the `build.scripts` root.

    Locates the nearest ancestor containing `build/scripts/lock_utils.py`
    rather than requiring checkout-root sentinels (`.fx-root` /
    `.jiri_manifest`), which are absent in Bazel runfiles and Swarming test
    isolates.

    Args:
        start: File or directory path inside a Fuchsia checkout, Bazel
            runfiles tree, or Swarming test isolate.

    Returns:
        The nearest ancestor directory containing `build/scripts/lock_utils.py`,
        or None if no matching ancestor exists.
    """
    abs_start = start.absolute()
    search_dirs = (
        (abs_start, *abs_start.parents)
        if abs_start.is_dir()
        else abs_start.parents
    )
    for parent in search_dirs:
        if (parent / _LOCK_UTILS_REL_PATH).is_file():
            return parent
    return None


_MODULE_PATH = Path(__file__).absolute()
_CANDIDATE_PATHS = (_MODULE_PATH, _MODULE_PATH.resolve())
for _candidate_path in _CANDIDATE_PATHS:
    _script_dir = _candidate_path.parent
    if _script_dir.is_dir() and str(_script_dir) not in sys.path:
        sys.path.insert(0, str(_script_dir))
for _candidate_path in _CANDIDATE_PATHS:
    _import_root = _find_import_root(_candidate_path)
    if _import_root is not None:
        _import_root_str = str(_import_root)
        if _import_root_str not in sys.path:
            sys.path.append(_import_root_str)
        break

import registry
from build.scripts import lock_utils

_RUNTIME_DIR_SUFFIX = ".runtime_dir"
_RUNTIME_DIR_SUFFIX_BYTES = _RUNTIME_DIR_SUFFIX.encode("ascii")
_ALWAYS_BUILD_MARKER = ".always_build"
_SOURCES_MANIFEST = "sources.manifest"
_PACKAGES_MANIFEST = "packages.manifest"
_HOST_BAZEL_ARGS_REL_PATH = Path("bazel_args/host.json")
_FX_REENTRY_REL_PATH = Path("scripts/fx-reentry")
_DEFAULT_BAZEL_TOP_DIR_REL = Path("gen/build/bazel")
_BAZEL_TOP_DIR_CFG_REL = Path("build/bazel/config/bazel_top_dir")
_STAGED_HOST_TOOLS_DIR_NAME = "_fx_host_tools"
_STAGED_HOST_TOOLS_REL_DIR = f"out/{_STAGED_HOST_TOOLS_DIR_NAME}"

_RUNTIME_DIR_ALT_RE = re.compile(
    rb"/([A-Za-z0-9_.+-]+)" + re.escape(_RUNTIME_DIR_SUFFIX_BYTES)
)
_BEP_FILE_URI_RE = re.compile(r'"uri"\s*:\s*"file://([^"]+)"')
_ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_BAZEL_PROGRESS_RE = re.compile(
    r"^(?:\[\s*[\d,]+\s*/\s*[\d,]+\]\s+.*"
    r"|Computing main repo mapping:.*"
    r"|Loading:.*"
    r"|Analyzing:.*"
    r"|INFO: Analyzed target\b.*)$"
)
# LINT.IfChange(host_tool_building_env)
_HOST_TOOL_BUILDING_ENV = "_FX_HOST_TOOL_BUILDING"
# LINT.ThenChange(//build/scripts/main_build.py:host_tool_building_env)
_FX_REENTRY_STRIP_ENV_KEYS = frozenset(
    {
        "_FX_BUILD_DIR",
        "FX_BUILD_QUIET",
        "FX_FORCE_BUILD",
        "FX_FORCE_BUILD_HOST_TOOL",
    }
)
_STAGED_PYTHON_ENV_KEYS = frozenset(
    {
        "PYTHONHOME",
        "PYTHONPATH",
        "PYTHONSAFEPATH",
        "PYTHONPYCACHEPREFIX",
    }
)


class HostToolBuildError(RuntimeError):
    """Raised when a registered host tool cannot be resolved or built."""


@dataclasses.dataclass(frozen=True)
class BuildContext:
    """Execution context for resolving, building, and staging host tools.

    Attributes:
        fuchsia_dir: Root directory of the Fuchsia checkout (resolved on init).
        build_dir: Optional active GN build directory, normalized relative to
            fuchsia_dir when provided; falls back to .fx-build-dir when None.
        env: Read-only environment mapping used for flag resolution, registry
            logging, and spawning fx-reentry (defaults to a copy of os.environ).
        stderr: Output stream for progress, warnings, and errors; injectable
            for unit tests (defaults to sys.stderr).
    """

    fuchsia_dir: Path
    build_dir: Path | None = None
    env: Mapping[str, str] = dataclasses.field(
        default_factory=lambda: dict(os.environ)
    )
    stderr: TextIO = dataclasses.field(default_factory=lambda: sys.stderr)

    def __post_init__(self) -> None:
        resolved_fuchsia_dir = self.fuchsia_dir.resolve()
        object.__setattr__(self, "fuchsia_dir", resolved_fuchsia_dir)
        if self.build_dir is not None:
            object.__setattr__(
                self,
                "build_dir",
                _normalize_build_dir_path(resolved_fuchsia_dir, self.build_dir),
            )

    @property
    def fx_reentry_script(self) -> Path:
        return self.fuchsia_dir / _FX_REENTRY_REL_PATH

    @property
    def out_dir(self) -> Path:
        out_root_str = self.env.get("FUCHSIA_OUT_DIR", "")
        return (
            Path(out_root_str) if out_root_str else (self.fuchsia_dir / "out")
        )

    def install_dir(self, vendor: str | None = None) -> Path:
        out_root_str = self.env.get("FUCHSIA_OUT_DIR", "")
        base = (
            Path(out_root_str) / _STAGED_HOST_TOOLS_DIR_NAME
            if out_root_str
            else self.fuchsia_dir / _STAGED_HOST_TOOLS_REL_DIR
        )
        return base / "vendor" / vendor if vendor else base

    def resolve_build_dir(self) -> Path | None:
        return _resolve_build_dir(self.fuchsia_dir, self.build_dir)

    def host_bazel_args_for_marker(
        self, always_build_marker: Path
    ) -> Path | None:
        if not always_build_marker.is_file():
            return None
        effective_build_dir = self.resolve_build_dir()
        if effective_build_dir is None:
            return None
        return effective_build_dir / _HOST_BAZEL_ARGS_REL_PATH

    def emit_registry_action(self, msg: str) -> None:
        _emit_registry_action(msg, self.stderr, self.env)

    def with_build_dir(self, build_dir: Path | None) -> BuildContext:
        return dataclasses.replace(self, build_dir=build_dir)


def _extract_bazel_progress(line: str) -> str | None:
    """Extract a single-line Bazel build progress status if present."""
    cleaned = _ANSI_ESCAPE_RE.sub("", line).strip("\r\n").strip()
    if not cleaned:
        return None
    if _BAZEL_PROGRESS_RE.match(cleaned):
        return cleaned
    return None


def _relative(path: Path, fuchsia_dir: Path) -> str:
    try:
        rel = path.relative_to(fuchsia_dir)
        return f"//{rel}"
    except ValueError:
        return str(path)


def _git_blob_sha1(data: bytes) -> str:
    header = f"blob {len(data)}\0".encode("ascii")
    return hashlib.sha1(header + data).hexdigest()


def _file_git_hash(path: Path) -> str:
    for attr_name in ("casfs.hash", "user.casfs.hash"):
        try:
            raw = os.getxattr(path, attr_name)
            if raw:
                decoded = raw.decode("utf-8", errors="ignore").strip()
                if len(decoded) == 40:
                    return decoded
        except OSError:
            continue
    return _git_blob_sha1(path.read_bytes())


def _tree_revision(fuchsia_dir: Path) -> str:
    jiri_head = fuchsia_dir / ".git" / "JIRI_HEAD"
    if jiri_head.is_file():
        try:
            rev = jiri_head.read_text(encoding="utf-8").strip()
            if rev:
                return rev
        except OSError:
            rev = ""
    env = os.environ.copy()
    env["GIT_OPTIONAL_LOCKS"] = "0"
    try:
        res = subprocess.run(
            ["git", "-C", str(fuchsia_dir), "rev-parse", "HEAD"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return "unknown"
    if res.returncode == 0 and res.stdout.strip():
        return res.stdout.strip()
    return "unknown"


def _normalize_build_dir_path(
    fuchsia_dir: Path, raw: str | Path | None
) -> Path | None:
    if raw is None:
        return None
    cleaned = str(raw).strip()
    if cleaned.startswith("//"):
        cleaned = cleaned[2:]
    if not cleaned:
        return None
    p = Path(cleaned)
    return (p if p.is_absolute() else (fuchsia_dir / p)).resolve()


def _resolve_build_dir(
    fuchsia_dir: Path, build_dir: Path | str | None = None
) -> Path | None:
    """Resolve the active GN build directory from build_dir or .fx-build-dir."""
    if build_dir is not None:
        norm = _normalize_build_dir_path(fuchsia_dir, build_dir)
        if norm is not None:
            return norm
    fx_build_dir_file = fuchsia_dir / ".fx-build-dir"
    if fx_build_dir_file.is_file():
        try:
            rel = fx_build_dir_file.read_text(encoding="utf-8").strip()
            norm = _normalize_build_dir_path(fuchsia_dir, rel)
            if norm is not None and norm.is_dir():
                return norm
        except OSError:
            return None
    return None


def compute_stamp_hash(
    fuchsia_dir: Path,
    target: str,
    *,
    packages_manifest: Path | None = None,
    host_bazel_args: Path | None = None,
) -> str:
    """Compute canonical stamp string matching _fx_host_tool_stamp_hash in fx-cmd-locator.sh."""
    clean_label = target.lstrip("@")
    label_no_slashes = clean_label.removeprefix("//")
    pkg_path = (
        label_no_slashes.split(":", 1)[0]
        if ":" in label_no_slashes
        else label_no_slashes
    )

    hash_files: list[Path] = []
    missing_entries: list[str] = []

    host_args_bytes: bytes | None = None
    if host_bazel_args is not None and host_bazel_args.is_file():
        try:
            host_args_bytes = host_bazel_args.read_bytes()
        except OSError:
            host_args_bytes = None

    for rel in (
        "manifests/jiri.lock",
        "integration/jiri.lock",
        "third_party/rust_crates/Cargo.lock",
        "third_party/rust_crates/BUILD.bazel",
        "third_party/golibs/go.sum",
        "third_party/golibs/BUILD.bazel",
    ):
        cand = fuchsia_dir / rel
        if cand.is_file():
            hash_files.append(cand)

    for pattern in (
        "manifests/third_party/**/*",
        "integration/fuchsia/third_party/*",
        "vendor/*/manifests/*",
    ):
        for cand in sorted(fuchsia_dir.glob(pattern), key=str):
            if cand.is_file():
                hash_files.append(cand)

    if pkg_path:
        pkg_build = fuchsia_dir / pkg_path / "BUILD.bazel"
        if pkg_build.is_file():
            hash_files.append(pkg_build)
    else:
        root_build = fuchsia_dir / "BUILD.bazel"
        if root_build.is_file():
            hash_files.append(root_build)

    for rel in (
        "build/prebuilt/cipd/BUILD.bazel",
        "tools/devshell/host_tools/cipd/BUILD.bazel",
        "build/bazel/rules/host/fx_host_tool.bzl",
    ):
        cand = fuchsia_dir / rel
        if cand.is_file():
            hash_files.append(cand)

    if packages_manifest is not None and packages_manifest.is_file():
        try:
            if packages_manifest.stat().st_size > 0:
                for line in packages_manifest.read_text(
                    encoding="utf-8"
                ).splitlines():
                    if not line:
                        continue
                    pkg_cand = fuchsia_dir / line
                    if pkg_cand.is_file():
                        hash_files.append(pkg_cand)
                    else:
                        missing_entries.append(line)
        except OSError:
            missing_entries = []

    hash_files = sorted(set(hash_files), key=str)
    missing_entries = sorted(set(missing_entries))

    if not hash_files and not missing_entries:
        if host_args_bytes is not None:
            h = hashlib.sha256(host_args_bytes).hexdigest()
            return f"{target}|sha256:{h}"
        return f"{target}|{_tree_revision(fuchsia_dir)}"

    lines: list[str] = []
    for hfile in hash_files:
        try:
            file_digest = _file_git_hash(hfile)
            lines.append(f"{file_digest}  {hfile}\n")
        except OSError:
            continue
    for missing in missing_entries:
        lines.append(f"missing:{missing}\n")
    if host_args_bytes is not None:
        h = hashlib.sha256(host_args_bytes).hexdigest()
        lines.append(f"{h}  {host_bazel_args}\n")

    digest = hashlib.sha256("".join(lines).encode("utf-8")).hexdigest()
    return f"{target}|sha256:{digest}"


def _is_nonempty_file(path: Path) -> bool:
    try:
        return path.is_file() and path.stat().st_size > 0
    except OSError:
        return False


def _sources_manifest_check(
    fuchsia_dir: Path, manifest_file: Path
) -> str | None:
    """Verify all entries in sources.manifest match current source files.

    Returns None when manifest_file is absent/empty or all sources match,
    or a human-readable rebuild reason string on mismatch.
    """
    if not _is_nonempty_file(manifest_file):
        return None
    try:
        lines = manifest_file.read_text(encoding="utf-8").splitlines()
    except OSError:
        return None

    for raw_line in lines:
        line = raw_line.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        expected_hash, rel_path = parts[0], parts[1].strip()
        if not expected_hash or not rel_path:
            continue
        src_path = fuchsia_dir / rel_path
        if not src_path.is_file():
            return f"source '{rel_path}' missing"
        try:
            actual_hash = _file_git_hash(src_path)
        except OSError:
            return f"source '{rel_path}' missing"
        if actual_hash != expected_hash:
            return f"source '{rel_path}' modified"
    return None


def _staged_host_tool_is_intact(cmd_name: str, tool_bin: Path) -> bool:
    """Verify staged binary and its runtime_dir have no broken symlinks."""
    if not (tool_bin.is_file() and os.access(tool_bin, os.X_OK)):
        return False
    host_tools_dir = tool_bin.parent
    rt_dir = host_tools_dir / f"{cmd_name}{_RUNTIME_DIR_SUFFIX}"
    has_rt_ref = False
    raw_bin = b""
    if not rt_dir.is_dir():
        try:
            raw_bin = tool_bin.read_bytes()
            has_rt_ref = _RUNTIME_DIR_SUFFIX_BYTES in raw_bin
        except OSError:
            raw_bin = b""
            has_rt_ref = False
    if rt_dir.is_dir() or has_rt_ref:
        check_dir = rt_dir
        if not check_dir.is_dir():
            m = _RUNTIME_DIR_ALT_RE.search(raw_bin)
            if m:
                alt_name = m.group(1).decode("ascii", errors="ignore")
                alt_dir = host_tools_dir / f"{alt_name}{_RUNTIME_DIR_SUFFIX}"
                if alt_dir.is_dir():
                    check_dir = alt_dir
                else:
                    return False
            else:
                return False

        base_depth = len(check_dir.parts)
        for root, dirnames, filenames in os.walk(check_dir, followlinks=False):
            root_path = Path(root)
            depth = len(root_path.parts) - base_depth
            if depth >= 16:
                dirnames.clear()
            for name in (*dirnames, *filenames):
                cand = root_path / name
                if cand.is_symlink() and not cand.exists():
                    return False
    return True


def _fx_registry_log_enabled(env: Mapping[str, str]) -> bool:
    val = env.get("FX_OUTPUT_REGISTRY_ACTIONS", "")
    return val not in ("", "0", "false")


def _emit_registry_action(
    msg: str,
    stderr: TextIO,
    env: Mapping[str, str],
) -> None:
    if _fx_registry_log_enabled(env):
        print(f"[fx-registry] {msg}", file=stderr)


def _find_active_build_pid(
    build_dir: Path | None,
    env: Mapping[str, str],
) -> str | None:
    sim = env.get(
        "FX_Simulate_Active_Build",
        env.get("FX_SIMULATE_ACTIVE_BUILD", ""),
    )
    if sim:
        return sim
    return lock_utils.find_active_build_pid(build_dir)


def _prune_and_get_log_path(log_dir: Path, cmd_name: str) -> Path:
    log_dir.mkdir(parents=True, exist_ok=True)
    try:
        logs = sorted(
            (p for p in log_dir.glob("*.log") if p.is_file()),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
    except OSError:
        logs = []
    for old_log in logs[9:]:
        old_log.unlink(missing_ok=True)
    safe_name = cmd_name.replace("/", "_")
    ts = time.strftime("%Y%m%d-%H%M%S")
    return log_dir / f"{safe_name}.{ts}.{os.getpid()}.log"


def _copy_file_abfs_aware(
    src: str | os.PathLike[str],
    dst: str | os.PathLike[str],
    *,
    follow_symlinks: bool = True,
) -> Path:
    src_path = Path(src)
    dst_path = Path(dst)
    for prefix in ("user.casfs.", "casfs."):
        try:
            ari_hash = os.getxattr(src_path, f"{prefix}rbe_ari_hash")
            if ari_hash:
                fsize = src_path.stat().st_size
                ari_str = f"rbe:{ari_hash.decode('utf-8', errors='ignore').strip()}:{fsize}:{fsize}"
                dst_path.unlink(missing_ok=True)
                dst_path.touch()
                shutil.copymode(
                    src_path, dst_path, follow_symlinks=follow_symlinks
                )
                os.chmod(dst_path, os.stat(dst_path).st_mode | 0o200)
                os.setxattr(dst_path, "casfs.ari_size", ari_str.encode("utf-8"))
                return dst_path
        except OSError:
            dst_path.unlink(missing_ok=True)
            continue
    dst_path.unlink(missing_ok=True)
    shutil.copy2(src_path, dst_path, follow_symlinks=follow_symlinks)
    try:
        os.chmod(dst_path, os.stat(dst_path).st_mode | 0o200)
    except OSError:
        pass
    return dst_path


def _make_dirs_writable(root_dir: Path) -> None:
    try:
        if not root_dir.is_symlink():
            os.chmod(root_dir, 0o755)
    except OSError:
        pass
    for root, dirs, _files in os.walk(root_dir, followlinks=False):
        root_path = Path(root)
        for d in dirs:
            sub = root_path / d
            if not sub.is_symlink():
                try:
                    os.chmod(sub, 0o755)
                except OSError:
                    pass


def _is_persistent_checkout_path(
    cand: Path,
    fuchsia_dir: Path,
    bazel_top_dir: Path | None = None,
    out_dir: Path | None = None,
    build_dir: Path | None = None,
) -> bool:
    try:
        cand_res = cand.resolve(strict=True)
    except OSError:
        return False
    cand_str = str(cand_res)
    prebuilt_prefix = str((fuchsia_dir / ("pre" + "built")).resolve())
    if (
        cand_str.startswith(prebuilt_prefix + os.sep)
        or cand_str == prebuilt_prefix
    ):
        return cand_res.is_dir() or cand.parent.name == ".bin"
    fuchsia_prefix = str(fuchsia_dir.resolve())
    if cand_str.startswith(fuchsia_prefix + os.sep):
        effective_out = (
            out_dir if out_dir is not None else (fuchsia_dir / "out")
        )
        out_prefix = str(effective_out.resolve())
        if cand_str.startswith(out_prefix + os.sep) or cand_str == out_prefix:
            return False
        if build_dir is not None:
            build_prefix = str(build_dir.resolve())
            if (
                cand_str.startswith(build_prefix + os.sep)
                or cand_str == build_prefix
            ):
                return False
        if bazel_top_dir is not None:
            bazel_prefix = str(bazel_top_dir.resolve())
            if (
                cand_str.startswith(bazel_prefix + os.sep)
                or cand_str == bazel_prefix
            ):
                return False
        return cand_res.is_dir()
    return False


def materialize_ephemeral_symlinks(
    stage_rt_dir: Path,
    fuchsia_dir: Path | None = None,
    bazel_top_dir: Path | None = None,
    out_dir: Path | None = None,
    build_dir: Path | None = None,
    depth: int = 0,
    root_rt_dir: Path | None = None,
) -> None:
    """Dereference symlinks pointing outside stage_rt_dir while keeping internal relative symlinks."""
    if not stage_rt_dir.is_dir() or depth > 8:
        return
    real_root = root_rt_dir or stage_rt_dir.resolve()
    real_root_str = str(real_root)
    effective_out_dir = (
        out_dir
        if out_dir is not None
        else ((fuchsia_dir / "out") if fuchsia_dir is not None else None)
    )

    symlinks: list[Path] = []
    for root, dirnames, filenames in os.walk(stage_rt_dir, followlinks=False):
        root_path = Path(root)
        for dname in dirnames:
            candidate = root_path / dname
            if candidate.is_symlink():
                symlinks.append(candidate)
        for fname in filenames:
            candidate = root_path / fname
            if candidate.is_symlink():
                symlinks.append(candidate)

    for link in symlinks:
        try:
            raw_target = os.readlink(link)
        except OSError as exc:
            raise HostToolBuildError(
                f"ERROR: Failed to read symlink '{link}': {exc}"
            ) from exc

        # Keep relative symlinks whose lexical target stays inside real_root.
        if not os.path.isabs(raw_target):
            lexical_abs = os.path.normpath(
                os.path.join(str(link.parent.resolve()), raw_target)
            )
            if lexical_abs == real_root_str or lexical_abs.startswith(
                real_root_str + os.sep
            ):
                try:
                    link.resolve(strict=True)
                except OSError as exc:
                    raise HostToolBuildError(
                        f"ERROR: Broken internal symlink '{link}' -> '{raw_target}': {exc}"
                    ) from exc
                continue

        try:
            resolved = link.resolve(strict=True)
        except OSError as exc:
            raise HostToolBuildError(
                f"ERROR: Broken symlink '{link}' -> '{raw_target}': {exc}"
            ) from exc

        if (
            fuchsia_dir is not None
            and effective_out_dir is not None
            and _is_persistent_checkout_path(
                link, fuchsia_dir, bazel_top_dir, effective_out_dir, build_dir
            )
        ):
            if os.path.isabs(raw_target) and str(resolved) == raw_target:
                continue
            link.unlink()
            link.symlink_to(resolved)
            continue

        link.unlink()
        if resolved.is_dir():
            shutil.copytree(
                resolved,
                link,
                symlinks=True,
                copy_function=_copy_file_abfs_aware,
            )
            _make_dirs_writable(link)
            materialize_ephemeral_symlinks(
                link,
                fuchsia_dir=fuchsia_dir,
                bazel_top_dir=bazel_top_dir,
                out_dir=effective_out_dir,
                build_dir=build_dir,
                depth=depth + 1,
                root_rt_dir=real_root,
            )
        else:
            _copy_file_abfs_aware(resolved, link)


def _check_not_recursive_build(
    context: BuildContext,
    cmd_name: str,
    target: str,
) -> None:
    """Guard against building a host tool while another host tool build is active.

    Raises:
        HostToolBuildError: If `_HOST_TOOL_BUILDING_ENV` indicates a parent host
            tool build is already running via `fx-reentry build`.
    """
    if parent_build := context.env.get(_HOST_TOOL_BUILDING_ENV, "").strip():
        context.emit_registry_action(
            f"error '{cmd_name}' ({target}): recursive host tool build while building '{parent_build}'",
        )
        msg = (
            f"ERROR: Recursive host tool build detected: cannot build '{cmd_name}' ({target}) "
            f"while building '{parent_build}' via fx-reentry build.\n"
            "ERROR: Tools invoked during 'fx build' must remain scripts rather than Bazel-built host tools."
        )
        print(msg, file=context.stderr)
        raise HostToolBuildError(msg)


def _raise_ancestor_build_lock_error(
    context: BuildContext,
    cmd_name: str,
    target: str,
    ancestor_pid: str,
) -> NoReturn:
    """Emit a diagnostic and abort when an ancestor `fx build` holds `.build_lock`.

    Raises:
        HostToolBuildError: Always raised to prevent deadlocking on an ancestor
            `fx build`'s lock.
    """
    context.emit_registry_action(
        f"error '{cmd_name}' ({target}): ancestor fx build (PID {ancestor_pid}) holds lock",
    )
    msg = (
        f"ERROR: Cannot build host tool '{cmd_name}' ({target}) while ancestor fx build (PID {ancestor_pid}) holds the build lock.\n"
        "ERROR: Tools invoked during 'fx build' must remain scripts rather than Bazel-built host tools."
    )
    print(msg, file=context.stderr)
    raise HostToolBuildError(msg)


def _build_fx_reentry_env(
    context: BuildContext,
    cmd_name: str,
) -> Mapping[str, str]:
    """Construct the subprocess environment for invoking `scripts/fx-reentry build`."""
    if context.build_dir is None:
        raise HostToolBuildError("No build directory configured.")
    staged_marker = f"/{_STAGED_HOST_TOOLS_DIR_NAME}/"
    if any(
        staged_marker in context.env.get(key, "")
        for key in ("PYTHONHOME", "PYTHONPATH")
    ):
        strip_keys = _FX_REENTRY_STRIP_ENV_KEYS | _STAGED_PYTHON_ENV_KEYS
    else:
        strip_keys = _FX_REENTRY_STRIP_ENV_KEYS
    return {
        **{k: v for k, v in context.env.items() if k not in strip_keys},
        "FUCHSIA_DIR": str(context.fuchsia_dir),
        "FUCHSIA_BUILD_DIR": str(context.build_dir),
        "FX_ENTRY_CMD": context.env.get("FX_ENTRY_CMD") or cmd_name,
        # Internal signaling env var coordinated with the fx bash layers to
        # disable terminal UI initialization during background host tool builds.
        "TUI_ENABLED": "0",
        _HOST_TOOL_BUILDING_ENV: cmd_name,
    }


def _resolve_bazel_top_dir(fuchsia_dir: Path, build_dir: Path) -> Path:
    top_dir_rel = _DEFAULT_BAZEL_TOP_DIR_REL
    top_dir_cfg = fuchsia_dir / _BAZEL_TOP_DIR_CFG_REL
    if top_dir_cfg.is_file():
        try:
            configured = top_dir_cfg.read_text(encoding="utf-8").strip()
            if configured:
                top_dir_rel = Path(configured)
        except OSError:
            pass
    return build_dir / top_dir_rel


def _invoke_bazel_build(
    context: BuildContext,
    cmd_name: str,
    target: str,
    clean_label: str,
    install_dir: Path,
    bep_file: Path,
) -> None:
    """Invoke `scripts/fx-reentry build` for a host tool and stream output to a log."""
    reentry_script = context.fx_reentry_script
    if not (reentry_script.is_file() and os.access(reentry_script, os.X_OK)):
        msg = f"ERROR: Cannot build host tool '{cmd_name}' ({target}): fx-reentry not found at {reentry_script}."
        print(msg, file=context.stderr)
        raise HostToolBuildError(msg)

    build_env = _build_fx_reentry_env(context, cmd_name)
    log_dir = install_dir / ".bazel_logs"
    log_file = _prune_and_get_log_path(log_dir, cmd_name)
    is_tty = bool(
        getattr(context.stderr, "isatty", None) and context.stderr.isatty()
    )
    wrote_progress = False

    print(
        f"INFO: Building required tool '{cmd_name}' with Bazel (log: {_relative(log_file, context.fuchsia_dir)})...",
        file=context.stderr,
    )
    build_cmd = (
        str(reentry_script),
        "build",
        "--no-checks",
        "--no-status",
        "--no-resultstore",
        "--skip-auth-check",
        "--host",
        f"@{clean_label}",
        "--",
        "--experimental_convenience_symlinks=ignore",
        f"--build_event_json_file={bep_file}",
    )
    with log_file.open("w", encoding="utf-8") as lf:
        proc = subprocess.Popen(
            build_cmd,
            cwd=str(context.fuchsia_dir),
            env=build_env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        try:
            if proc.stdout is not None:
                for line in proc.stdout:
                    lf.write(line)
                    lf.flush()
                    if is_tty:
                        status = _extract_bazel_progress(line)
                        if status:
                            max_cols = max(
                                40,
                                shutil.get_terminal_size((100, 20)).columns - 1,
                            )
                            truncated = f"  {status}"[:max_cols]
                            context.stderr.write(f"\r\x1b[K{truncated}")
                            context.stderr.flush()
                            wrote_progress = True
            build_rc = proc.wait()
        except BaseException:
            proc.terminate()
            proc.wait()
            raise
        finally:
            if proc.stdout is not None:
                proc.stdout.close()

    if is_tty and wrote_progress:
        context.stderr.write("\r\x1b[K")
        context.stderr.flush()

    if build_rc != 0:
        try:
            context.stderr.write(
                log_file.read_text(encoding="utf-8", errors="replace")
            )
        except OSError:
            pass
        raise HostToolBuildError(
            f"Failed to build host tool '{cmd_name}' ({target}) with exit code {build_rc}."
        )


def _find_binary_in_bep(
    bep_file: Path,
    cmd_name: str,
    clean_label: str,
    bazel_top_dir: Path,
) -> Path:
    """Locate and validate the built host tool binary from a Bazel BEP JSON file."""
    label_no_slashes = clean_label.removeprefix("//")
    if ":" in label_no_slashes:
        pkg_path, target_name = label_no_slashes.split(":", 1)
    else:
        pkg_path = label_no_slashes
        target_name = pkg_path.split("/")[-1] if "/" in pkg_path else pkg_path

    want1 = f"{pkg_path}/{cmd_name}" if pkg_path else cmd_name
    want2 = f"{pkg_path}/{target_name}" if pkg_path else target_name
    want3 = (
        f"{pkg_path}/{target_name}/{cmd_name}"
        if pkg_path
        else f"{target_name}/{cmd_name}"
    )

    bep_text = bep_file.read_text(encoding="utf-8", errors="replace")
    cand1: str | None = None
    fallback: str | None = None
    best: str | None = None

    for m in _BEP_FILE_URI_RE.finditer(bep_text):
        uri = urllib.parse.unquote(m.group(1))
        bazel_out_idx = uri.rfind("/bazel-out/")
        if bazel_out_idx != -1:
            bin_idx = uri.find("/bin/", bazel_out_idx)
            if bin_idx != -1:
                rel = uri[bin_idx + len("/bin/") :]
                if rel == want3:
                    best = uri
                    break
                if rel == want1 and cand1 is None:
                    cand1 = uri
                    if want1 == want2:
                        best = uri
                        break
                if rel == want2 and fallback is None:
                    fallback = uri

    if best is None and cand1 and fallback and cand1 != fallback:
        cand1_path = Path(cand1)
        fallback_path = Path(fallback)
        if (
            fallback_path.parent / f"{fallback_path.name}{_RUNTIME_DIR_SUFFIX}"
        ).is_dir():
            best = fallback
        elif (
            cand1_path.parent / f"{cand1_path.name}{_RUNTIME_DIR_SUFFIX}"
        ).is_dir():
            best = cand1

    built_bin_str = best or fallback or cand1
    if not built_bin_str:
        raise HostToolBuildError(
            f"Built host tool artifact not found in BEP output for @{clean_label}"
        )

    built_bin_path = Path(built_bin_str)
    expected_output_base = bazel_top_dir / "output_base"
    real_output_base: Path | None = None
    if expected_output_base.is_dir():
        real_output_base = expected_output_base.resolve()

    bin_parent_resolved = (
        built_bin_path.parent.resolve()
        if built_bin_path.parent.is_dir()
        else built_bin_path.parent
    )
    bin_in_dir = bin_parent_resolved / built_bin_path.name
    is_in_output_base = str(bin_in_dir).startswith(
        str(expected_output_base) + os.sep
    ) or (
        real_output_base is not None
        and str(bin_in_dir).startswith(str(real_output_base) + os.sep)
    )
    if not is_in_output_base or not built_bin_path.is_file():
        raise HostToolBuildError(
            f"Built host tool artifact '{built_bin_path}' is missing or outside Bazel output_base."
        )
    return built_bin_path


def _stage_artifacts_atomically(
    context: BuildContext,
    cmd_name: str,
    built_bin_path: Path,
    install_dir: Path,
    bazel_top_dir: Path,
    build_dir: Path,
) -> Path:
    """Copy a built host tool and its runtime_dir into install_dir atomically."""
    tool_bin = install_dir / cmd_name
    cmd_rt_dir_name = f"{cmd_name}{_RUNTIME_DIR_SUFFIX}"
    bazel_pkg_bin = built_bin_path.parent
    built_name = built_bin_path.name
    rt_dir_name = f"{built_name}{_RUNTIME_DIR_SUFFIX}"
    src_runtime_dir = bazel_pkg_bin / rt_dir_name
    dst_runtime_dir = install_dir / rt_dir_name

    install_dir.mkdir(parents=True, exist_ok=True)
    stage_dir = Path(
        tempfile.mkdtemp(dir=str(install_dir), prefix=f".{cmd_name}.stage.")
    )
    try:
        try:
            staged_bin = stage_dir / cmd_name
            _copy_file_abfs_aware(built_bin_path, staged_bin)
            os.chmod(staged_bin, 0o755)

            has_rt = False
            if src_runtime_dir.is_dir():
                has_rt = True
                stage_rt_dir = stage_dir / rt_dir_name
                shutil.copytree(
                    src_runtime_dir,
                    stage_rt_dir,
                    symlinks=True,
                    copy_function=_copy_file_abfs_aware,
                )
                _make_dirs_writable(stage_rt_dir)
                materialize_ephemeral_symlinks(
                    stage_rt_dir,
                    fuchsia_dir=context.fuchsia_dir,
                    bazel_top_dir=bazel_top_dir,
                    out_dir=context.out_dir,
                    build_dir=build_dir,
                )

            # Atomic swap
            old_rt: Path | None = None
            if dst_runtime_dir.is_symlink():
                dst_runtime_dir.unlink(missing_ok=True)
            elif dst_runtime_dir.is_dir():
                old_rt = install_dir / f".{rt_dir_name}.old.{os.getpid()}"
                if old_rt.is_dir() and not old_rt.is_symlink():
                    _make_dirs_writable(old_rt)
                shutil.rmtree(old_rt, ignore_errors=True)
                dst_runtime_dir.rename(old_rt)

            if has_rt:
                (stage_dir / rt_dir_name).rename(dst_runtime_dir)

            staged_bin.replace(tool_bin)
            if old_rt is not None:
                _make_dirs_writable(old_rt)
                shutil.rmtree(old_rt, ignore_errors=True)

            if rt_dir_name != cmd_rt_dir_name:
                link = install_dir / cmd_rt_dir_name
                if link.is_symlink() or link.is_file():
                    link.unlink(missing_ok=True)
                elif link.is_dir():
                    _make_dirs_writable(link)
                    shutil.rmtree(link, ignore_errors=True)
                if has_rt:
                    link.symlink_to(rt_dir_name)
        except (OSError, shutil.Error) as exc:
            raise HostToolBuildError(
                f"Staging failed for '{cmd_name}': {exc}"
            ) from exc
    finally:
        shutil.rmtree(stage_dir, ignore_errors=True)
    return dst_runtime_dir


def _write_staged_stamp(
    context: BuildContext,
    cmd_name: str,
    target: str,
    dst_runtime_dir: Path,
    stamp_file: Path,
) -> None:
    always_build_marker = dst_runtime_dir / _ALWAYS_BUILD_MARKER
    manifest_file = dst_runtime_dir / _SOURCES_MANIFEST
    packages_manifest = dst_runtime_dir / _PACKAGES_MANIFEST
    if always_build_marker.is_file() and not _is_nonempty_file(manifest_file):
        stamp_file.unlink(missing_ok=True)
        return
    host_bazel_args = context.host_bazel_args_for_marker(always_build_marker)
    final_stamp = compute_stamp_hash(
        context.fuchsia_dir,
        target,
        packages_manifest=packages_manifest,
        host_bazel_args=host_bazel_args,
    )
    stamp_file.parent.mkdir(parents=True, exist_ok=True)
    tmp_stamp = stamp_file.with_name(f".{cmd_name}.stamp.tmp.{os.getpid()}")
    tmp_stamp.write_text(f"{final_stamp}\n", encoding="utf-8")
    tmp_stamp.replace(stamp_file)


def _run_bazel_build_and_stage(
    context: BuildContext,
    cmd_name: str,
    target: str,
    install_dir: Path,
    stamp_file: Path,
) -> None:
    _check_not_recursive_build(context, cmd_name, target)
    build_dir = context.build_dir
    if (
        ancestor_pid := lock_utils.find_ancestor_build_pid(build_dir)
    ) is not None:
        _raise_ancestor_build_lock_error(
            context, cmd_name, target, ancestor_pid
        )
    if build_dir is None:
        raise HostToolBuildError("No build directory configured.")

    clean_label = target.lstrip("@")
    bazel_top_dir = _resolve_bazel_top_dir(context.fuchsia_dir, build_dir)
    bep_fd, bep_path_str = tempfile.mkstemp(prefix="fx_host_tool_bep.")
    os.close(bep_fd)
    bep_file = Path(bep_path_str)
    try:
        _invoke_bazel_build(
            context=context,
            cmd_name=cmd_name,
            target=target,
            clean_label=clean_label,
            install_dir=install_dir,
            bep_file=bep_file,
        )
        built_bin_path = _find_binary_in_bep(
            bep_file=bep_file,
            cmd_name=cmd_name,
            clean_label=clean_label,
            bazel_top_dir=bazel_top_dir,
        )
        dst_runtime_dir = _stage_artifacts_atomically(
            context=context,
            cmd_name=cmd_name,
            built_bin_path=built_bin_path,
            install_dir=install_dir,
            bazel_top_dir=bazel_top_dir,
            build_dir=build_dir,
        )
        _write_staged_stamp(
            context=context,
            cmd_name=cmd_name,
            target=target,
            dst_runtime_dir=dst_runtime_dir,
            stamp_file=stamp_file,
        )
    finally:
        bep_file.unlink(missing_ok=True)


_run_bazel_build_and_stage._is_default_impl = True  # type: ignore[attr-defined]


@dataclasses.dataclass(frozen=True)
class _ToolStagingPaths:
    """Filesystem paths for a single staged host tool."""

    install_dir: Path
    tool_bin: Path
    manifest_file: Path
    packages_manifest: Path
    always_build_marker: Path
    stamp_file: Path
    lock_file: Path

    @classmethod
    def for_tool(cls, install_dir: Path, cmd_name: str) -> _ToolStagingPaths:
        rt_dir = install_dir / f"{cmd_name}{_RUNTIME_DIR_SUFFIX}"
        stamp_dir = install_dir / ".stamps"
        return cls(
            install_dir=install_dir,
            tool_bin=install_dir / cmd_name,
            manifest_file=rt_dir / _SOURCES_MANIFEST,
            packages_manifest=rt_dir / _PACKAGES_MANIFEST,
            always_build_marker=rt_dir / _ALWAYS_BUILD_MARKER,
            stamp_file=stamp_dir / f"{cmd_name}.stamp",
            lock_file=stamp_dir / f"{cmd_name}.lock",
        )


def _compute_rebuild_reason(
    context: BuildContext,
    cmd_name: str,
    target: str,
    paths: _ToolStagingPaths,
    *,
    force_build: bool,
) -> str | None:
    """Return a human-readable rebuild reason, or None if the staged tool is up to date."""
    if force_build:
        return "--build forced rebuild"
    if not (paths.tool_bin.is_file() and os.access(paths.tool_bin, os.X_OK)):
        return f"staged binary missing at {_relative(paths.tool_bin, context.fuchsia_dir)}"
    if not paths.stamp_file.is_file():
        return "stamp missing"
    if paths.always_build_marker.is_file() and not _is_nonempty_file(
        paths.manifest_file
    ):
        return (
            f"target marked {_ALWAYS_BUILD_MARKER} without {_SOURCES_MANIFEST}"
        )
    try:
        current_stamp = paths.stamp_file.read_text(encoding="utf-8").strip()
    except OSError:
        current_stamp = ""
    host_bazel_args = context.host_bazel_args_for_marker(
        paths.always_build_marker
    )
    expected_stamp = compute_stamp_hash(
        context.fuchsia_dir,
        target,
        packages_manifest=paths.packages_manifest,
        host_bazel_args=host_bazel_args,
    )
    if current_stamp != expected_stamp:
        return "stamp out of date (BUILD.bazel or lockfile changed)"
    if (
        src_reason := _sources_manifest_check(
            context.fuchsia_dir, paths.manifest_file
        )
    ) is not None:
        return src_reason
    if not _staged_host_tool_is_intact(cmd_name, paths.tool_bin):
        return "staged runtime_dir incomplete or has broken symlinks"
    return None


def _wait_for_active_build(
    context: BuildContext,
    cmd_name: str,
    target: str,
    paths: _ToolStagingPaths,
    *,
    force_build: bool,
) -> Path | None:
    """Coordinate with a concurrent non-ancestor `fx build` holding `.build_lock`.

    Returns `paths.tool_bin` if building can be skipped (either by reusing an
    existing intact binary or because the tool became up to date while waiting),
    or None when the caller should proceed to build.
    """
    lock_pid = _find_active_build_pid(context.build_dir, context.env)
    if not lock_pid:
        return None
    if (
        not force_build
        and paths.tool_bin.is_file()
        and os.access(paths.tool_bin, os.X_OK)
        and _staged_host_tool_is_intact(cmd_name, paths.tool_bin)
    ):
        context.emit_registry_action(
            f"skip build '{cmd_name}' ({target}): active fx build (PID {lock_pid}) holds lock, "
            f"using existing {_relative(paths.tool_bin, context.fuchsia_dir)}",
        )
        print(
            f"WARNING: Skipping rebuild of '{cmd_name}' due to active fx build (PID {lock_pid}); running existing binary.",
            file=context.stderr,
        )
        return paths.tool_bin

    context.emit_registry_action(
        f"wait '{cmd_name}' ({target}): waiting for active fx build (PID {lock_pid}) before building...",
    )
    print(
        f"INFO: Waiting for active fx build (PID {lock_pid}) before building '{cmd_name}'...",
        file=context.stderr,
    )
    while _find_active_build_pid(context.build_dir, context.env):
        time.sleep(0.2)

    if (
        _compute_rebuild_reason(
            context, cmd_name, target, paths, force_build=force_build
        )
        is None
    ):
        context.emit_registry_action(
            f"skip build '{cmd_name}' ({target}): built during wait, using {_relative(paths.tool_bin, context.fuchsia_dir)}",
        )
        return paths.tool_bin
    return None


def ensure_registered_host_tool(
    context: BuildContext,
    cmd_name: str,
    *,
    entry: Any | None = None,
    target: str = "",
    script_rel: str = "",
    vendor: str | None = None,
    no_build: bool | None = None,
    force_build: bool | None = None,
) -> Path:
    """Resolve or build a registered fx host tool and return its executable Path."""
    fuchsia_dir = context.fuchsia_dir
    err_stream = context.stderr

    if entry is not None:
        if isinstance(entry, dict):
            target = target or str(entry.get("target") or "")
            script_rel = script_rel or str(entry.get("script") or "")
        else:
            target = target or str(getattr(entry, "target", "") or "")
            script_rel = script_rel or str(getattr(entry, "script", "") or "")

    if not target and not script_rel:
        merged = registry.load_merged_registry(fuchsia_dir, vendor=vendor)
        reg_entry = merged.get(cmd_name)
        if reg_entry is None:
            msg = f"ERROR: Unknown registered host tool '{cmd_name}'."
            print(msg, file=err_stream)
            raise HostToolBuildError(msg)
        target = str(reg_entry.target or "")
        script_rel = str(reg_entry.script or "")

    if no_build is True and force_build is True:
        msg = "ERROR: --no-build and --build are mutually exclusive."
        print(msg, file=err_stream)
        raise HostToolBuildError(msg)
    if force_build is True and no_build is None:
        no_build = False
    elif no_build is True and force_build is None:
        force_build = False
    elif no_build is None and force_build is None:
        env_force_host = context.env.get("FX_FORCE_BUILD_HOST_TOOL") == "1"
        env_no_host = context.env.get("FX_NO_BUILD_HOST_TOOL") == "1"
        if env_force_host and env_no_host:
            msg = "ERROR: --no-build and --build are mutually exclusive."
            print(msg, file=err_stream)
            raise HostToolBuildError(msg)
        if env_force_host:
            force_build = True
            no_build = False
        elif env_no_host:
            no_build = True
            force_build = False
        else:
            force_build = context.env.get("FX_FORCE_BUILD") == "1"
            no_build = not force_build and context.env.get("FX_NO_BUILD") == "1"
    else:
        no_build = bool(no_build)
        force_build = bool(force_build)

    # 1. Script mode
    if script_rel:
        clean_rel = script_rel.lstrip("@/")
        script_abs = fuchsia_dir / clean_rel
        if clean_rel.endswith(".py"):
            if not script_abs.is_file():
                msg = f"ERROR: Registered script for '{cmd_name}' does not exist: {script_abs}"
                print(msg, file=err_stream)
                raise HostToolBuildError(msg)
        else:
            if not (script_abs.is_file() and os.access(script_abs, os.X_OK)):
                msg = f"ERROR: Registered script for '{cmd_name}' is missing or not executable: {script_abs}"
                print(msg, file=err_stream)
                raise HostToolBuildError(msg)
        context.emit_registry_action(
            f"skip build '{cmd_name}': script mode, running //{clean_rel} directly",
        )
        return script_abs

    # 2. Target mode
    clean_label = target.lstrip("@")
    if not clean_label.startswith("//"):
        msg = f"ERROR: Invalid Bazel target '{target}' for registered host tool '{cmd_name}': must start with '//'"
        print(msg, file=err_stream)
        raise HostToolBuildError(msg)

    paths = _ToolStagingPaths.for_tool(
        context.install_dir(vendor=vendor), cmd_name
    )

    if no_build:
        if (
            paths.tool_bin.is_file()
            and os.access(paths.tool_bin, os.X_OK)
            and _staged_host_tool_is_intact(cmd_name, paths.tool_bin)
        ):
            context.emit_registry_action(
                f"skip build '{cmd_name}' ({target}): --no-build specified, using existing {_relative(paths.tool_bin, fuchsia_dir)}",
            )
            return paths.tool_bin
        context.emit_registry_action(
            f"build '{cmd_name}' ({target}): binary missing at {_relative(paths.tool_bin, fuchsia_dir)} and --no-build specified",
        )
        msg = (
            f"ERROR: Host tool '{cmd_name}' ({target}) has not been built yet and --no-build was specified.\n"
            f"ERROR: Run 'fx {cmd_name}' without --no-build or 'fx build --host @{clean_label}' first."
        )
        print(msg, file=err_stream)
        raise HostToolBuildError(msg)

    # Fast path (unlocked)
    if (
        _compute_rebuild_reason(
            context, cmd_name, target, paths, force_build=force_build
        )
        is None
    ):
        context.emit_registry_action(
            f"skip build '{cmd_name}' ({target}): up-to-date (stamp and {_SOURCES_MANIFEST} valid), using {_relative(paths.tool_bin, fuchsia_dir)}",
        )
        return paths.tool_bin

    # Slow path under per-tool staging flock
    _check_not_recursive_build(context, cmd_name, target)
    resolved_build_dir = context.resolve_build_dir()
    if (
        ancestor_pid := lock_utils.find_ancestor_build_pid(resolved_build_dir)
    ) is not None:
        if (
            not force_build
            and paths.tool_bin.is_file()
            and os.access(paths.tool_bin, os.X_OK)
            and _staged_host_tool_is_intact(cmd_name, paths.tool_bin)
        ):
            context.emit_registry_action(
                f"skip build '{cmd_name}' ({target}): ancestor fx build (PID {ancestor_pid}) holds lock, "
                f"using existing {_relative(paths.tool_bin, fuchsia_dir)}",
            )
            print(
                f"WARNING: Skipping rebuild of '{cmd_name}' because ancestor fx build (PID {ancestor_pid}) holds the build lock; running existing binary.",
                file=err_stream,
            )
            return paths.tool_bin
        _raise_ancestor_build_lock_error(
            context, cmd_name, target, ancestor_pid
        )

    is_default_impl = (
        getattr(_run_bazel_build_and_stage, "_is_default_impl", None) is True
    )
    if context.build_dir is None and is_default_impl:
        if resolved_build_dir is None:
            msg = (
                "ERROR: No build directory found.\n"
                'ERROR: Run "fx set" to create a new build directory, or specify one with --dir'
            )
            print(msg, file=err_stream)
            raise HostToolBuildError("No build directory found.")
        context = context.with_build_dir(resolved_build_dir)

    # Per-tool lock coordinates concurrent staging of `cmd_name`, whereas the
    # global `.build_lock` check coordinates with `fx build` and avoids
    # deadlocks on recursive invocations.
    with lock_utils.BlockingFileLock(
        paths.lock_file,
        waiting_message=(
            f"Waiting for concurrent build of host tool '{cmd_name}'..."
        ),
        message_stream=err_stream,
    ) as staging_lock:
        rebuild_reason = _compute_rebuild_reason(
            context, cmd_name, target, paths, force_build=force_build
        )
        if rebuild_reason is None:
            if staging_lock.waited:
                context.emit_registry_action(
                    f"skip build '{cmd_name}' ({target}): updated by concurrent invocation while waiting for lock, using {_relative(paths.tool_bin, fuchsia_dir)}",
                )
            else:
                context.emit_registry_action(
                    f"skip build '{cmd_name}' ({target}): up-to-date (stamp and {_SOURCES_MANIFEST} valid), using {_relative(paths.tool_bin, fuchsia_dir)}",
                )
            return paths.tool_bin

        if (
            reusable := _wait_for_active_build(
                context, cmd_name, target, paths, force_build=force_build
            )
        ) is not None:
            return reusable

        context.emit_registry_action(
            f"build '{cmd_name}' ({target}): building @{clean_label} with Bazel (reason: {rebuild_reason})",
        )
        _run_bazel_build_and_stage(
            context=context,
            cmd_name=cmd_name,
            target=target,
            install_dir=paths.install_dir,
            stamp_file=paths.stamp_file,
        )
        context.emit_registry_action(
            f"build '{cmd_name}' ({target}): staged binary to {_relative(paths.tool_bin, fuchsia_dir)}",
        )
        return paths.tool_bin


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="fx_cli.builder",
        description="Build or resolve a registered fx host tool.",
    )
    parser.add_argument(
        "--fuchsia-dir", default="", help="Path to FUCHSIA_DIR."
    )
    parser.add_argument(
        "--build-dir", default="", help="Path to FUCHSIA_BUILD_DIR."
    )
    parser.add_argument("--vendor", default="", help="Optional vendor name.")
    parser.add_argument("--target", default="", help="Bazel target label.")
    parser.add_argument("--script", default="", help="Script path.")
    build_group = parser.add_mutually_exclusive_group()
    build_group.add_argument(
        "--no-build", action="store_true", help="Do not build."
    )
    build_group.add_argument(
        "--build", action="store_true", help="Force build."
    )
    parser.add_argument("command", help="Command name.")
    ns = parser.parse_args(argv)

    fdir = (
        Path(ns.fuchsia_dir).resolve()
        if ns.fuchsia_dir
        else Path(os.environ.get("FUCHSIA_DIR", ".")).resolve()
    )
    bdir = (
        _normalize_build_dir_path(fdir, ns.build_dir) if ns.build_dir else None
    )
    context = BuildContext(fuchsia_dir=fdir, build_dir=bdir)
    try:
        resolved = ensure_registered_host_tool(
            context,
            ns.command,
            target=ns.target,
            script_rel=ns.script,
            vendor=ns.vendor or None,
            no_build=True if ns.no_build else None,
            force_build=True if ns.build else None,
        )
    except HostToolBuildError:
        return 1
    print(resolved)
    return 0


if __name__ == "__main__":
    sys.exit(main())
