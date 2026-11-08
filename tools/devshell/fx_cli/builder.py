#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel host tool builder, stamp/manifest verifier, and staging materializer."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path
from typing import Any, Sequence, TextIO

try:
    from fx_cli import registry
except ImportError:
    import registry

_RUNTIME_DIR_ALT_RE = re.compile(rb"/([A-Za-z0-9_.+-]+)\.runtime_dir")
_ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_BAZEL_PROGRESS_RE = re.compile(
    r"^(?:\[\s*[\d,]+\s*/\s*[\d,]+\]\s+.*"
    r"|Computing main repo mapping:.*"
    r"|Loading:.*"
    r"|Analyzing:.*"
    r"|INFO: Analyzed target\b.*)$"
)


class HostToolBuildError(RuntimeError):
    """Raised when a registered host tool cannot be resolved or built."""


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
    rt_dir = host_tools_dir / f"{cmd_name}.runtime_dir"
    has_rt_ref = False
    if not rt_dir.is_dir():
        try:
            raw_bin = tool_bin.read_bytes()
            has_rt_ref = b".runtime_dir" in raw_bin
        except OSError:
            raw_bin = b""
            has_rt_ref = False
    if rt_dir.is_dir() or has_rt_ref:
        check_dir = rt_dir
        if not check_dir.is_dir():
            m = _RUNTIME_DIR_ALT_RE.search(raw_bin)
            if m:
                alt_name = m.group(1).decode("ascii", errors="ignore")
                alt_dir = host_tools_dir / f"{alt_name}.runtime_dir"
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


def _fx_registry_log_enabled() -> bool:
    val = os.environ.get("FX_OUTPUT_REGISTRY_ACTIONS", "")
    return val not in ("", "0", "false")


def _emit_registry_action(msg: str, stderr: TextIO) -> None:
    if _fx_registry_log_enabled():
        print(f"[fx-registry] {msg}", file=stderr)


def _is_ancestor_pid(want_pid: str) -> bool:
    if not (want_pid and want_pid.isdigit()):
        return False
    target = int(want_pid)
    cur = os.getpid()
    hops = 0
    while cur > 1 and hops < 64:
        if cur == target:
            return True
        stat_path = Path(f"/proc/{cur}/stat")
        if not stat_path.is_file():
            break
        try:
            content = stat_path.read_text(encoding="utf-8")
            # /proc/[pid]/stat format: pid (comm) state ppid ...
            rparen = content.rfind(")")
            if rparen == -1:
                break
            fields = content[rparen + 2 :].split()
            cur = int(fields[1])  # ppid is field index 1 after state
            hops += 1
        except (OSError, IndexError, ValueError):
            break
    return False


def _find_active_build_pid(build_dir: Path | None) -> str | None:
    sim = os.environ.get(
        "FX_Simulate_Active_Build",
        os.environ.get("FX_SIMULATE_ACTIVE_BUILD", ""),
    )
    if sim:
        return sim

    if build_dir is not None:
        lock_file = build_dir.parent / f"{build_dir.name}.build_lock"
        if not lock_file.is_file():
            lock_file = build_dir / ".build_lock"
        if lock_file.is_file():
            try:
                line = lock_file.read_text(encoding="utf-8").strip().split()
                if line:
                    pid_str = line[0]
                    if pid_str.isdigit():
                        pid = int(pid_str)
                        try:
                            os.kill(pid, 0)
                            if not _is_ancestor_pid(pid_str):
                                return pid_str
                        except OSError:
                            pass
            except OSError:
                pass
    return None


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


def _run_bazel_build_and_stage(
    fuchsia_dir: Path,
    cmd_name: str,
    target: str,
    install_dir: Path,
    stamp_file: Path,
    expected_stamp: str,
    build_dir: Path | None,
    stderr: TextIO,
    *,
    vendor: str | None = None,
    no_build: bool = False,
    force_build: bool = False,
    rebuild_reason: str = "",
) -> None:
    clean_label = target.lstrip("@")
    label_no_slashes = clean_label.removeprefix("//")
    if ":" in label_no_slashes:
        pkg_path, target_name = label_no_slashes.split(":", 1)
    else:
        pkg_path = label_no_slashes
        target_name = pkg_path.split("/")[-1] if "/" in pkg_path else pkg_path

    top_dir_rel = "gen/build/bazel"
    top_dir_cfg = fuchsia_dir / "build/bazel/config/bazel_top_dir"
    if top_dir_cfg.is_file():
        try:
            top_dir_rel = (
                top_dir_cfg.read_text(encoding="utf-8").strip() or top_dir_rel
            )
        except OSError:
            pass

    if build_dir is None:
        raise HostToolBuildError("No build directory configured.")

    bazel_top_dir = build_dir / top_dir_rel
    bazel_bin = bazel_top_dir / "bazel"
    bazel_workspace_dir = bazel_top_dir / "workspace"

    if not (bazel_bin.is_file() and os.access(bazel_bin, os.X_OK)):
        if (build_dir / "args.gn").is_file():
            print(
                f"WARN: Bazel workspace not generated in {_relative(build_dir, fuchsia_dir)}; running 'fx gen'...",
                file=stderr,
            )
            fx_script = fuchsia_dir / "scripts/fx"
            gen_res = subprocess.run(
                [str(fx_script), "--dir", str(build_dir), "gen"],
                cwd=str(fuchsia_dir),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
            if gen_res.stdout:
                stderr.write(gen_res.stdout)
                stderr.flush()
            if gen_res.returncode != 0:
                raise HostToolBuildError(
                    f"Failed to generate Bazel workspace (exit code {gen_res.returncode})."
                )
        if not (bazel_bin.is_file() and os.access(bazel_bin, os.X_OK)):
            raise HostToolBuildError(
                f"Cannot build host tool '{cmd_name}' ({target}): Bazel wrapper not found at {bazel_bin}.\n"
                "Run 'fx set' first to configure a build directory."
            )

    bazel_env = os.environ.copy()
    existing_py = bazel_env.get("PYTHONPATH", "")
    bazel_env["PYTHONPATH"] = (
        f"{fuchsia_dir}:{existing_py}" if existing_py else str(fuchsia_dir)
    )
    bazel_env["FX_BUILD_LOGDIR"] = bazel_env.get("FX_BUILD_LOGDIR") or str(
        build_dir
    )
    if (
        not bazel_env.get("FX_BUILD_RBE_AUTH_TYPE")
        and Path(
            "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
        ).is_file()
    ):
        bazel_env["FX_BUILD_RBE_AUTH_TYPE"] = "loas"

    bazel_build_args = [
        "--config=host",
        "--experimental_convenience_symlinks=ignore",
    ]
    if not (
        bazel_workspace_dir / "fuchsia_build_generated" / "gn_targets_dir"
    ).is_dir():
        bazel_build_args.append(
            f"--override_repository=gn_targets+={fuchsia_dir}/build/bazel/local_repositories/empty_gn_targets"
        )

    log_dir = install_dir / ".bazel_logs"
    log_file = _prune_and_get_log_path(log_dir, cmd_name)
    bep_fd, bep_path_str = tempfile.mkstemp(prefix="fx_host_tool_bep.")
    os.close(bep_fd)
    bep_file = Path(bep_path_str)

    tool_bin = install_dir / cmd_name
    cmd_rt_dir_name = f"{cmd_name}.runtime_dir"
    stage_dir: Path | None = None
    is_tty = bool(getattr(stderr, "isatty", None) and stderr.isatty())
    wrote_progress = False
    try:
        print(
            f"INFO: Building required tool '{cmd_name}' with Bazel (log: {_relative(log_file, fuchsia_dir)})...",
            file=stderr,
        )
        with log_file.open("w", encoding="utf-8") as lf:
            proc = subprocess.Popen(
                [
                    str(bazel_bin),
                    "build",
                    *bazel_build_args,
                    f"--build_event_json_file={bep_file}",
                    f"@{clean_label}",
                ],
                cwd=str(fuchsia_dir),
                env=bazel_env,
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
                    try:
                        for line in proc.stdout:
                            lf.write(line)
                            lf.flush()
                            if is_tty:
                                status = _extract_bazel_progress(line)
                                if status:
                                    max_cols = max(
                                        40,
                                        shutil.get_terminal_size(
                                            (100, 20)
                                        ).columns
                                        - 1,
                                    )
                                    truncated = f"  {status}"[:max_cols]
                                    stderr.write(f"\r\x1b[K{truncated}")
                                    stderr.flush()
                                    wrote_progress = True
                    finally:
                        proc.stdout.close()
                build_rc = proc.wait()
            except BaseException:
                proc.terminate()
                proc.wait()
                raise

        if is_tty and wrote_progress:
            stderr.write("\r\x1b[K")
            stderr.flush()

        if build_rc != 0:
            try:
                stderr.write(
                    log_file.read_text(encoding="utf-8", errors="replace")
                )
            except OSError:
                pass
            raise HostToolBuildError(
                f"Failed to build host tool '{cmd_name}' ({target}) with exit code {build_rc}."
            )

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

        uri_re = re.compile(r'"uri"\s*:\s*"file://([^"]+)"')
        for m in uri_re.finditer(bep_text):
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
                fallback_path.parent / f"{fallback_path.name}.runtime_dir"
            ).is_dir():
                best = fallback
            elif (
                cand1_path.parent / f"{cand1_path.name}.runtime_dir"
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

        bazel_pkg_bin = built_bin_path.parent
        built_name = built_bin_path.name
        rt_dir_name = f"{built_name}.runtime_dir"
        src_runtime_dir = bazel_pkg_bin / rt_dir_name
        dst_runtime_dir = install_dir / rt_dir_name

        install_dir.mkdir(parents=True, exist_ok=True)
        stage_dir = Path(
            tempfile.mkdtemp(dir=str(install_dir), prefix=f".{cmd_name}.stage.")
        )
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
                out_root = Path(
                    os.environ.get("FUCHSIA_OUT_DIR") or (fuchsia_dir / "out")
                )
                materialize_ephemeral_symlinks(
                    stage_rt_dir,
                    fuchsia_dir=fuchsia_dir,
                    bazel_top_dir=bazel_top_dir,
                    out_dir=out_root,
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

        # Write stamp file
        always_build_marker = dst_runtime_dir / ".always_build"
        manifest_file = dst_runtime_dir / "sources.manifest"
        packages_manifest = dst_runtime_dir / "packages.manifest"
        if always_build_marker.is_file() and not _is_nonempty_file(
            manifest_file
        ):
            stamp_file.unlink(missing_ok=True)
        else:
            host_bazel_args = (
                (build_dir / "bazel_args" / "host.json")
                if (always_build_marker.is_file() and build_dir is not None)
                else None
            )
            final_stamp = compute_stamp_hash(
                fuchsia_dir,
                target,
                packages_manifest=packages_manifest,
                host_bazel_args=host_bazel_args,
            )
            stamp_file.parent.mkdir(parents=True, exist_ok=True)
            tmp_stamp = stamp_file.with_name(
                f".{cmd_name}.stamp.tmp.{os.getpid()}"
            )
            tmp_stamp.write_text(f"{final_stamp}\n", encoding="utf-8")
            tmp_stamp.replace(stamp_file)
    finally:
        bep_file.unlink(missing_ok=True)
        if stage_dir is not None:
            shutil.rmtree(stage_dir, ignore_errors=True)


_run_bazel_build_and_stage._is_default_impl = True  # type: ignore[attr-defined]


def ensure_registered_host_tool(
    fuchsia_dir: Path,
    cmd_name: str,
    *,
    entry: Any | None = None,
    target: str = "",
    script_rel: str = "",
    vendor: str | None = None,
    build_dir: Path | None = None,
    no_build: bool | None = None,
    force_build: bool | None = None,
    stderr: TextIO | None = None,
) -> Path:
    """Resolve or build a registered fx host tool and return its executable Path."""
    err_stream = stderr if stderr is not None else sys.stderr
    fuchsia_dir = fuchsia_dir.resolve()
    if build_dir is not None:
        build_dir = _normalize_build_dir_path(fuchsia_dir, build_dir)

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
        env_force_host = os.environ.get("FX_FORCE_BUILD_HOST_TOOL") == "1"
        env_no_host = os.environ.get("FX_NO_BUILD_HOST_TOOL") == "1"
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
            force_build = os.environ.get("FX_FORCE_BUILD") == "1"
            no_build = not force_build and os.environ.get("FX_NO_BUILD") == "1"
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
        _emit_registry_action(
            f"skip build '{cmd_name}': script mode, running //{clean_rel} directly",
            err_stream,
        )
        return script_abs

    # 2. Target mode
    clean_label = target.lstrip("@")
    if not clean_label.startswith("//"):
        msg = f"ERROR: Invalid Bazel target '{target}' for registered host tool '{cmd_name}': must start with '//'"
        print(msg, file=err_stream)
        raise HostToolBuildError(msg)

    out_root_str = os.environ.get("FUCHSIA_OUT_DIR", "")
    out_root = Path(out_root_str) if out_root_str else (fuchsia_dir / "out")
    install_dir = (
        out_root / "_fx_host_tools" / "vendor" / vendor
        if vendor
        else out_root / "_fx_host_tools"
    )
    tool_bin = install_dir / cmd_name
    cmd_rt_dir_name = f"{cmd_name}.runtime_dir"
    manifest_file = install_dir / cmd_rt_dir_name / "sources.manifest"
    packages_manifest = install_dir / cmd_rt_dir_name / "packages.manifest"
    always_build_marker = install_dir / cmd_rt_dir_name / ".always_build"
    stamp_dir = install_dir / ".stamps"
    stamp_file = stamp_dir / f"{cmd_name}.stamp"
    lock_file = stamp_dir / f"{cmd_name}.lock"

    if no_build:
        if (
            tool_bin.is_file()
            and os.access(tool_bin, os.X_OK)
            and _staged_host_tool_is_intact(cmd_name, tool_bin)
        ):
            _emit_registry_action(
                f"skip build '{cmd_name}' ({target}): --no-build specified, using existing {_relative(tool_bin, fuchsia_dir)}",
                err_stream,
            )
            return tool_bin
        _emit_registry_action(
            f"build '{cmd_name}' ({target}): binary missing at {_relative(tool_bin, fuchsia_dir)} and --no-build specified",
            err_stream,
        )
        msg = (
            f"ERROR: Host tool '{cmd_name}' ({target}) has not been built yet and --no-build was specified.\n"
            f"ERROR: Run 'fx {cmd_name}' without --no-build or 'fx build --host @{clean_label}' first."
        )
        print(msg, file=err_stream)
        raise HostToolBuildError(msg)

    effective_build_dir = (
        _resolve_build_dir(fuchsia_dir, build_dir)
        if always_build_marker.is_file()
        else build_dir
    )
    host_bazel_args = (
        effective_build_dir / "bazel_args" / "host.json"
        if (always_build_marker.is_file() and effective_build_dir is not None)
        else None
    )
    expected_stamp = compute_stamp_hash(
        fuchsia_dir,
        target,
        packages_manifest=packages_manifest,
        host_bazel_args=host_bazel_args,
    )

    # Fast path (unlocked)
    if (
        not force_build
        and tool_bin.is_file()
        and os.access(tool_bin, os.X_OK)
        and stamp_file.is_file()
        and (
            not always_build_marker.is_file()
            or _is_nonempty_file(manifest_file)
        )
    ):
        try:
            current_stamp = stamp_file.read_text(encoding="utf-8").strip()
        except OSError:
            current_stamp = ""
        if (
            current_stamp == expected_stamp
            and _sources_manifest_check(fuchsia_dir, manifest_file) is None
            and _staged_host_tool_is_intact(cmd_name, tool_bin)
        ):
            _emit_registry_action(
                f"skip build '{cmd_name}' ({target}): up-to-date (stamp and sources.manifest valid), using {_relative(tool_bin, fuchsia_dir)}",
                err_stream,
            )
            return tool_bin

    # Slow path under flock
    is_default_impl = (
        getattr(_run_bazel_build_and_stage, "_is_default_impl", None) is True
    )
    if build_dir is None and is_default_impl:
        build_dir = _resolve_build_dir(fuchsia_dir, None)
        if build_dir is None:
            msg = (
                "ERROR: No build directory found.\n"
                'ERROR: Run "fx set" to create a new build directory, or specify one with --dir'
            )
            print(msg, file=err_stream)
            raise HostToolBuildError("No build directory found.")

    lock_file.parent.mkdir(parents=True, exist_ok=True)
    lock_fd = os.open(lock_file, os.O_CREAT | os.O_RDWR, 0o644)
    waited_for_lock = False
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            waited_for_lock = True
            print(
                f"Waiting for concurrent build of host tool '{cmd_name}'...",
                file=err_stream,
            )
            fcntl.flock(lock_fd, fcntl.LOCK_EX)

        effective_build_dir = (
            _resolve_build_dir(fuchsia_dir, build_dir)
            if always_build_marker.is_file()
            else build_dir
        )
        host_bazel_args = (
            effective_build_dir / "bazel_args" / "host.json"
            if (
                always_build_marker.is_file()
                and effective_build_dir is not None
            )
            else None
        )
        expected_stamp = compute_stamp_hash(
            fuchsia_dir,
            target,
            packages_manifest=packages_manifest,
            host_bazel_args=host_bazel_args,
        )
        rebuild_reason = ""
        if force_build:
            rebuild_reason = "--build forced rebuild"
        elif not (tool_bin.is_file() and os.access(tool_bin, os.X_OK)):
            rebuild_reason = (
                f"staged binary missing at {_relative(tool_bin, fuchsia_dir)}"
            )
        elif not stamp_file.is_file():
            rebuild_reason = "stamp missing"
        elif always_build_marker.is_file() and not _is_nonempty_file(
            manifest_file
        ):
            rebuild_reason = (
                "target marked .always_build without sources.manifest"
            )
        else:
            try:
                current_stamp = stamp_file.read_text(encoding="utf-8").strip()
            except OSError:
                current_stamp = ""
            if current_stamp != expected_stamp:
                rebuild_reason = (
                    "stamp out of date (BUILD.bazel or lockfile changed)"
                )
            else:
                src_reason = _sources_manifest_check(fuchsia_dir, manifest_file)
                if src_reason is not None:
                    rebuild_reason = src_reason
                elif not _staged_host_tool_is_intact(cmd_name, tool_bin):
                    rebuild_reason = (
                        "staged runtime_dir incomplete or has broken symlinks"
                    )

        if not rebuild_reason:
            if waited_for_lock:
                _emit_registry_action(
                    f"skip build '{cmd_name}' ({target}): updated by concurrent invocation while waiting for lock, using {_relative(tool_bin, fuchsia_dir)}",
                    err_stream,
                )
            else:
                _emit_registry_action(
                    f"skip build '{cmd_name}' ({target}): up-to-date (stamp and sources.manifest valid), using {_relative(tool_bin, fuchsia_dir)}",
                    err_stream,
                )
            return tool_bin

        lock_pid = _find_active_build_pid(build_dir)
        if lock_pid:
            if (
                not force_build
                and tool_bin.is_file()
                and os.access(tool_bin, os.X_OK)
                and _staged_host_tool_is_intact(cmd_name, tool_bin)
            ):
                _emit_registry_action(
                    f"skip build '{cmd_name}' ({target}): active fx build (PID {lock_pid}) holds lock, "
                    f"using existing {_relative(tool_bin, fuchsia_dir)}",
                    err_stream,
                )
                print(
                    f"WARNING: Skipping rebuild of '{cmd_name}' due to active fx build (PID {lock_pid}); running existing binary.",
                    file=err_stream,
                )
                return tool_bin
            _emit_registry_action(
                f"wait '{cmd_name}' ({target}): waiting for active fx build (PID {lock_pid}) before building...",
                err_stream,
            )
            print(
                f"INFO: Waiting for active fx build (PID {lock_pid}) before building '{cmd_name}'...",
                file=err_stream,
            )
            while _find_active_build_pid(build_dir):
                time.sleep(0.2)
            effective_build_dir = (
                _resolve_build_dir(fuchsia_dir, build_dir)
                if always_build_marker.is_file()
                else build_dir
            )
            host_bazel_args = (
                effective_build_dir / "bazel_args" / "host.json"
                if (
                    always_build_marker.is_file()
                    and effective_build_dir is not None
                )
                else None
            )
            expected_stamp = compute_stamp_hash(
                fuchsia_dir,
                target,
                packages_manifest=packages_manifest,
                host_bazel_args=host_bazel_args,
            )
            if (
                not force_build
                and tool_bin.is_file()
                and os.access(tool_bin, os.X_OK)
                and stamp_file.is_file()
                and (
                    not always_build_marker.is_file()
                    or _is_nonempty_file(manifest_file)
                )
            ):
                try:
                    current_stamp = stamp_file.read_text(
                        encoding="utf-8"
                    ).strip()
                except OSError:
                    current_stamp = ""
                if (
                    current_stamp == expected_stamp
                    and _sources_manifest_check(fuchsia_dir, manifest_file)
                    is None
                    and _staged_host_tool_is_intact(cmd_name, tool_bin)
                ):
                    _emit_registry_action(
                        f"skip build '{cmd_name}' ({target}): built during wait, using {_relative(tool_bin, fuchsia_dir)}",
                        err_stream,
                    )
                    return tool_bin

        _emit_registry_action(
            f"build '{cmd_name}' ({target}): building @{clean_label} with Bazel (reason: {rebuild_reason})",
            err_stream,
        )
        _run_bazel_build_and_stage(
            fuchsia_dir=fuchsia_dir,
            cmd_name=cmd_name,
            target=target,
            install_dir=install_dir,
            stamp_file=stamp_file,
            expected_stamp=expected_stamp,
            build_dir=build_dir,
            stderr=err_stream,
            vendor=vendor,
            no_build=bool(no_build),
            force_build=bool(force_build),
            rebuild_reason=rebuild_reason,
        )
        _emit_registry_action(
            f"build '{cmd_name}' ({target}): staged binary to {_relative(tool_bin, fuchsia_dir)}",
            err_stream,
        )
        return tool_bin
    finally:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
        finally:
            os.close(lock_fd)


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
    try:
        resolved = ensure_registered_host_tool(
            fdir,
            ns.command,
            target=ns.target,
            script_rel=ns.script,
            vendor=ns.vendor or None,
            build_dir=bdir,
            no_build=True if ns.no_build else None,
            force_build=True if ns.build else None,
        )
    except HostToolBuildError:
        return 1
    print(resolved)
    return 0


if __name__ == "__main__":
    sys.exit(main())
