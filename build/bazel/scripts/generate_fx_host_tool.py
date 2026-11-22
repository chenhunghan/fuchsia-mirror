#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generate a relocatable fx_host_tool launcher and runtime directory."""

import argparse
import hashlib
import os
import re
import shlex
import shutil
import sys
from collections.abc import Collection, Mapping, Sequence
from pathlib import Path

from build.bazel.scripts import runfiles_utils

_SCRIPT_DIR = Path(__file__).resolve().parent
_DEFAULT_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent.parent
_COMPACT_PYTHON_REAL_RE = re.compile(r"^python(\d+)\.(\d+)-real$")


def _link_or_copy_file(src: str | Path, dst: str | Path) -> None:
    src_resolved = Path(src).resolve()
    dst_path = Path(dst)
    dst_path.parent.mkdir(parents=True, exist_ok=True)
    if dst_path.exists() or dst_path.is_symlink():
        dst_path.unlink()
    try:
        os.link(src_resolved, dst_path)
    except OSError:
        shutil.copy2(src_resolved, dst_path)


def _force_symlink(dst: Path, target: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists() or dst.is_symlink():
        if dst.is_dir() and not dst.is_symlink():
            _make_dirs_writable(dst)
            shutil.rmtree(dst)
        else:
            dst.unlink()
    dst.symlink_to(target)


def _make_dirs_writable(root: Path) -> None:
    if root.is_dir() and not root.is_symlink():
        root.chmod(root.stat().st_mode | 0o200)
        for dirpath, dirnames, _ in os.walk(root):
            dp = Path(dirpath)
            if not dp.is_symlink():
                dp.chmod(dp.stat().st_mode | 0o200)
            for d in dirnames:
                sub = dp / d
                if not sub.is_symlink():
                    sub.chmod(sub.stat().st_mode | 0o200)


def _infer_fuchsia_dir(
    explicit_fuchsia_dir: Path | None, execroot: Path
) -> Path | None:
    if explicit_fuchsia_dir is not None:
        return explicit_fuchsia_dir.resolve()
    if (
        execroot.is_relative_to(_DEFAULT_FUCHSIA_DIR)
        and (_DEFAULT_FUCHSIA_DIR / "tools" / "devshell").is_dir()
    ):
        return _DEFAULT_FUCHSIA_DIR
    return None


def _resolve_persistent_prebuilt_dir(
    src_resolved: Path,
    execroot: Path,
    fuchsia_dir: Path | None,
) -> Path | None:
    if fuchsia_dir is None or not src_resolved.is_dir():
        return None
    prebuilt_root = fuchsia_dir / "prebuilt"
    if (
        src_resolved != prebuilt_root
        and src_resolved.is_relative_to(prebuilt_root)
        and not {"bazel-out", "external", "prebuilt_out"}.intersection(
            src_resolved.parts
        )
    ):
        return src_resolved
    if src_resolved.is_relative_to(
        fuchsia_dir
    ) and not src_resolved.is_relative_to(execroot):
        return None
    search_parts = (
        src_resolved.relative_to(execroot).parts
        if src_resolved.is_relative_to(execroot)
        else src_resolved.parts
    )
    for idx, part in enumerate(search_parts):
        if part == "prebuilt" and idx + 1 < len(search_parts):
            candidate = fuchsia_dir.joinpath(*search_parts[idx:])
            if candidate.is_dir():
                return candidate
    return None


def _materialize_ephemeral_directory(
    src_resolved: Path,
    dst: Path,
    execroot: Path,
    fuchsia_dir: Path | None,
) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists() or dst.is_symlink():
        if dst.is_dir() and not dst.is_symlink():
            _make_dirs_writable(dst)
            shutil.rmtree(dst)
        else:
            dst.unlink()
    shutil.copytree(
        src_resolved,
        dst,
        symlinks=True,
        copy_function=_link_or_copy_file,
    )
    _make_dirs_writable(dst)
    src_real = src_resolved.resolve()
    dst_real = dst.resolve()
    for dirpath, dirnames, filenames in os.walk(dst):
        parent = Path(dirpath)
        rel_parent = parent.relative_to(dst)
        for name in list(dirnames) + list(filenames):
            entry = parent / name
            if not entry.is_symlink():
                continue
            raw_target = os.readlink(entry)
            src_entry = src_real / rel_parent / name
            try:
                resolved = src_entry.resolve()
            except OSError:
                continue
            if not resolved.exists():
                continue
            if not os.path.isabs(raw_target) and (
                resolved == src_real or resolved.is_relative_to(src_real)
            ):
                try:
                    dst_target = entry.resolve()
                except OSError:
                    dst_target = None
                if (
                    dst_target is not None
                    and (
                        dst_target == dst_real
                        or dst_target.is_relative_to(dst_real)
                    )
                    and dst_target.exists()
                ):
                    continue
            persistent_dir = _resolve_persistent_prebuilt_dir(
                resolved, execroot, fuchsia_dir
            )
            if persistent_dir is not None:
                if name in dirnames:
                    dirnames.remove(name)
                if raw_target != str(persistent_dir):
                    _force_symlink(entry, persistent_dir)
                continue
            if name in dirnames:
                dirnames.remove(name)
            _stage_artifact(resolved, entry, execroot, fuchsia_dir)


def _stage_artifact(
    src_resolved: Path,
    dst: Path,
    execroot: Path,
    fuchsia_dir: Path | None,
    *,
    require_executable: bool = False,
) -> None:
    if src_resolved.is_dir():
        persistent_dir = _resolve_persistent_prebuilt_dir(
            src_resolved, execroot, fuchsia_dir
        )
        if persistent_dir is not None:
            _force_symlink(dst, persistent_dir)
        else:
            _materialize_ephemeral_directory(
                src_resolved, dst, execroot, fuchsia_dir
            )
        return
    if require_executable and not os.access(src_resolved, os.X_OK):
        dst.parent.mkdir(parents=True, exist_ok=True)
        if dst.exists() or dst.is_symlink():
            dst.unlink()
        shutil.copy2(src_resolved, dst)
        dst.chmod(dst.stat().st_mode | 0o111)
    else:
        _link_or_copy_file(src_resolved, dst)


def _iter_directory_executables(pkg_dir: Path) -> list[Path]:
    scan_dir = (pkg_dir / "bin") if (pkg_dir / "bin").is_dir() else pkg_dir
    abs_scan_dir = Path(os.path.abspath(scan_dir))
    return [
        abs_scan_dir / entry.name
        for entry in sorted(scan_dir.iterdir(), key=lambda p: p.name)
        if not entry.is_dir() and os.access(entry, os.X_OK)
    ]


def _parse_data_runfile(spec: str, bazel_execroot: Path) -> tuple[str, Path]:
    if "=" in spec:
        short_path, exec_path = spec.split("=", 1)
    else:
        short_path = spec
        exec_path = ""

    if short_path.startswith("../"):
        rlocation = short_path[3:]
        default_exec = Path("external") / rlocation
    else:
        rlocation = f"_main/{short_path}"
        default_exec = Path(short_path)

    raw_target = Path(exec_path) if exec_path else default_exec
    artifact = (
        raw_target if raw_target.is_absolute() else bazel_execroot / raw_target
    ).resolve()
    return rlocation, artifact


def _file_git_sha1(path: Path) -> str:
    for attr in ("casfs.hash", "user.casfs.hash"):
        try:
            raw = os.getxattr(path, attr).decode("ascii").strip()
            if len(raw) == 40:
                return raw
        except (OSError, UnicodeDecodeError):
            pass
    data = path.read_bytes()
    return hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()


def _normalize_compact_python_runtimes(
    output_runfiles_dir: Path,
) -> Sequence[str]:
    """Relayout staged compact Python runtimes to CPython's native prefix layout.

    Fuchsia's @prebuilt_python compact runtime stages `python<X>.<Y>-real` and
    `lib_python<X>.<Y>.zip` at the repo root and relies on a wrapper script that
    exports PYTHONHOME and prepends the zip to PYTHONPATH. Those variables leak
    into child processes spawned by fx tools (breaking nested `fx` or Python
    invocations).

    Because fx_host_tool materializes `python<X>.<Y>-real` as a regular file,
    moving it to `bin/python<X>.<Y>` and the stdlib zip to `lib/python<X><Y>.zip`
    lets CPython derive its prefix natively from argv[0] without PYTHONHOME or
    PYTHONPATH. The rewritten `python3` wrapper unsets PYTHONHOME and converts
    rules_python's injected PYTHONSAFEPATH into the non-inheritable `-P` flag.
    """
    normalized_repos = []
    for repo_dir in sorted(output_runfiles_dir.iterdir(), key=lambda p: p.name):
        if not repo_dir.is_dir() or repo_dir.is_symlink():
            continue
        wrapper_path = repo_dir / "python3"
        if not wrapper_path.is_file():
            continue

        matched = next(
            (
                (child, match.group(1), match.group(2))
                for child in sorted(repo_dir.iterdir(), key=lambda p: p.name)
                if (match := _COMPACT_PYTHON_REAL_RE.match(child.name))
                is not None
                and child.is_file()
            ),
            None,
        )
        if matched is None:
            continue

        real_bin, major, minor = matched
        version = f"{major}.{minor}"
        zip_src = repo_dir / f"lib_python{version}.zip"
        if not zip_src.is_file():
            continue

        _make_dirs_writable(repo_dir)

        bin_rel = f"bin/python{version}"
        bin_dst = repo_dir / bin_rel
        bin_dst.parent.mkdir(parents=True, exist_ok=True)
        os.replace(real_bin, bin_dst)

        zip_rel = f"lib/python{major}{minor}.zip"
        zip_dst = repo_dir / zip_rel
        zip_dst.parent.mkdir(parents=True, exist_ok=True)
        os.replace(zip_src, zip_dst)

        # Unlink before writing so we never overwrite a hardlinked file in
        # Bazel's external repository cache.
        wrapper_path.unlink()
        wrapper_path.write_text(
            "#!/bin/bash\n"
            "# Generated by generate_fx_host_tool.py - self-contained Python launcher.\n"
            'readonly _REAL_SCRIPT="$(realpath "${BASH_SOURCE[0]}")"\n'
            'readonly _SCRIPT_DIR="$(dirname "${_REAL_SCRIPT}")"\n'
            "unset PYTHONHOME\n"
            'if [[ "${_FX_UNSET_PYTHONSAFEPATH:-}" == "1" && -n "${PYTHONSAFEPATH:-}" ]]; then\n'
            "  unset PYTHONSAFEPATH _FX_UNSET_PYTHONSAFEPATH\n"
            f'  exec "${{_SCRIPT_DIR}}/{bin_rel}" -P -S -s "$@"\n'
            "fi\n"
            f'exec "${{_SCRIPT_DIR}}/{bin_rel}" -S -s "$@"\n',
            encoding="utf-8",
        )
        wrapper_path.chmod(0o755)

        normalized_repos.append(repo_dir.name)

    return tuple(normalized_repos)


def _entry_uses_compact_python(
    entry_path: Path, repo_names: Collection[str]
) -> bool:
    """Return True if entry_path is a script referencing a compact Python wrapper."""
    if not repo_names or not entry_path.is_file():
        return False
    with entry_path.open("rb") as f:
        header = f.read(65536)
    return header.startswith(b"#!") and any(
        f"{repo}/python3".encode("utf-8") in header for repo in repo_names
    )


def generate_fx_host_tool(
    *,
    entry_point: Path,
    entry_runfiles_manifest: Path | None,
    output_launcher: Path,
    output_runtime_dir: Path,
    bazel_execroot: Path,
    data_runfiles: Sequence[str],
    runtime_tools: Mapping[str, Path],
    runtime_tool_manifests: Mapping[str, Path] | None = None,
    runtime_cipd_dirs: Mapping[str, tuple[Path, str]] | None = None,
    runtime_fx_tools: Mapping[str, tuple[Path, Path]] | None = None,
    fuchsia_dir: Path | None = None,
    always_build: bool = False,
    source_files: Sequence[str] | None = None,
    source_packages: Sequence[str] | None = None,
) -> int:
    execroot = bazel_execroot.resolve()
    effective_fuchsia_dir = _infer_fuchsia_dir(fuchsia_dir, execroot)

    entry_src = (
        entry_point if entry_point.is_absolute() else execroot / entry_point
    )
    if not entry_src.exists():
        print(f"ERROR: Missing entry point: {entry_src}", file=sys.stderr)
        return 1

    resolved_tool_manifests = runtime_tool_manifests or {}
    for tool_alias in resolved_tool_manifests:
        if tool_alias not in runtime_tools:
            print(
                f"ERROR: runtime_tool_manifest '{tool_alias}' has no matching runtime_tool",
                file=sys.stderr,
            )
            return 1

    if output_runtime_dir.exists():
        _make_dirs_writable(output_runtime_dir)
        shutil.rmtree(output_runtime_dir)
    output_runtime_dir.mkdir(parents=True, exist_ok=True)

    entry_name = entry_point.name
    output_entry = output_runtime_dir / entry_name
    entry_resolved = entry_src.resolve()
    _stage_artifact(
        entry_resolved,
        output_entry,
        execroot,
        effective_fuchsia_dir,
        require_executable=True,
    )

    if always_build:
        (output_runtime_dir / ".always_build").write_text("", encoding="utf-8")

    has_runfiles = False
    manifest_sources: list[tuple[str, Path]] = []
    if entry_runfiles_manifest is not None:
        manifest_sources.append(("entry", entry_runfiles_manifest))
    for tool_alias, rt_manifest in resolved_tool_manifests.items():
        manifest_sources.append((f"runtime_tool '{tool_alias}'", rt_manifest))

    if manifest_sources or data_runfiles:
        has_runfiles = True
        output_runfiles_dir = output_runtime_dir / f"{entry_name}.runfiles"
        output_runfiles_dir.mkdir(parents=True, exist_ok=True)
        saw_repo_mapping = False
        repo_mapping_entries: dict[tuple[str, str], str] = {}

        for manifest_label, raw_manifest in manifest_sources:
            manifest_src = (
                raw_manifest
                if raw_manifest.is_absolute()
                else execroot / raw_manifest
            )
            if not manifest_src.exists():
                print(
                    f"ERROR: Missing Bazel runfiles manifest ({manifest_label}): {manifest_src}",
                    file=sys.stderr,
                )
                return 1

            input_manifest = runfiles_utils.RunfilesManifest.CreateFrom(
                manifest_src.read_text()
            )
            input_manifest.remove_legacy_external_runfiles(
                workspace_name="_main"
            )

            for rlocation, target_str in input_manifest.as_dict().items():
                if rlocation == "MANIFEST":
                    continue
                if rlocation == "_repo_mapping":
                    saw_repo_mapping = True
                    if target_str:
                        rm_path = Path(target_str)
                        if not rm_path.is_absolute():
                            rm_path = execroot / rm_path
                        if not rm_path.exists():
                            print(
                                f"ERROR: Missing Bazel _repo_mapping file ({manifest_label}): {rm_path}",
                                file=sys.stderr,
                            )
                            return 1
                        for line in rm_path.read_text(
                            encoding="utf-8"
                        ).splitlines():
                            if not line:
                                continue
                            items = line.split(",")
                            if len(items) != 3:
                                print(
                                    f"ERROR: Invalid _repo_mapping line in {rm_path}: {line}",
                                    file=sys.stderr,
                                )
                                return 1
                            src_repo, apparent_name, canonical_name = items
                            prev = repo_mapping_entries.get(
                                (src_repo, apparent_name)
                            )
                            if prev is not None and prev != canonical_name:
                                print(
                                    f"ERROR: Conflicting _repo_mapping for ({src_repo!r}, {apparent_name!r}): "
                                    f"{prev!r} vs {canonical_name!r}",
                                    file=sys.stderr,
                                )
                                return 1
                            repo_mapping_entries[
                                (src_repo, apparent_name)
                            ] = canonical_name
                    continue

                dest_path = output_runfiles_dir / rlocation
                if not target_str:
                    if not dest_path.exists() and not dest_path.is_symlink():
                        dest_path.parent.mkdir(parents=True, exist_ok=True)
                        dest_path.write_text("")
                else:
                    target_path = Path(target_str)
                    if not target_path.is_absolute():
                        target_path = execroot / target_path
                    resolved_target = target_path.resolve()
                    _stage_artifact(
                        resolved_target,
                        dest_path,
                        execroot,
                        effective_fuchsia_dir,
                    )

        if saw_repo_mapping:
            (output_runfiles_dir / "_repo_mapping").write_text(
                "".join(
                    f"{src_repo},{apparent_name},{canonical_name}\n"
                    for (
                        src_repo,
                        apparent_name,
                    ), canonical_name in repo_mapping_entries.items()
                ),
                encoding="utf-8",
            )

        for short_path in data_runfiles:
            rlocation, resolved_target = _parse_data_runfile(
                short_path, execroot
            )
            dest_path = output_runfiles_dir / rlocation
            _stage_artifact(
                resolved_target, dest_path, execroot, effective_fuchsia_dir
            )
        compact_python_repos = _normalize_compact_python_runtimes(
            output_runfiles_dir
        )
        entry_uses_compact_python = _entry_uses_compact_python(
            output_entry, compact_python_repos
        )
    else:
        entry_uses_compact_python = False

    resolved_cipd_dirs = runtime_cipd_dirs or {}
    resolved_fx_tools = runtime_fx_tools or {}
    has_bin_dir = bool(runtime_tools or resolved_cipd_dirs or resolved_fx_tools)
    if has_bin_dir:
        bin_dir = output_runtime_dir / ".bin"
        bin_dir.mkdir(parents=True, exist_ok=True)
        staged_commands: dict[str, str] = {}
        for tool_alias, (
            tree_rel,
            source_rel_path,
        ) in resolved_cipd_dirs.items():
            tree_src = (
                tree_rel if tree_rel.is_absolute() else execroot / tree_rel
            )
            if not tree_src.exists():
                print(
                    f"ERROR: Missing runtime_cipd_dir '{tool_alias}': {tree_src}",
                    file=sys.stderr,
                )
                return 1
            norm_rel = source_rel_path.removeprefix("//").strip("/")
            if (
                effective_fuchsia_dir is not None
                and norm_rel
                and (effective_fuchsia_dir / norm_rel).is_dir()
            ):
                cipd_pkg_dir = effective_fuchsia_dir / norm_rel
            else:
                cipd_pkg_dir = tree_src.resolve()
            source_label = f"runtime_cipd_dir '{tool_alias}' ({cipd_pkg_dir})"
            cipd_exes = _iter_directory_executables(cipd_pkg_dir)
            for exe_path in cipd_exes:
                if exe_path.name in staged_commands:
                    print(
                        f"ERROR: Duplicate runtime tool command '{exe_path.name}' in .bin: "
                        f"provided by both {staged_commands[exe_path.name]} and {source_label}",
                        file=sys.stderr,
                    )
                    return 1
                staged_commands[exe_path.name] = source_label
            for exe_path in cipd_exes:
                _force_symlink(bin_dir / exe_path.name, exe_path)

        for tool_alias, tool_rel in runtime_tools.items():
            tool_src = (
                tool_rel if tool_rel.is_absolute() else execroot / tool_rel
            )
            if not tool_src.exists():
                print(
                    f"ERROR: Missing runtime_tool '{tool_alias}': {tool_src}",
                    file=sys.stderr,
                )
                return 1
            if tool_src.is_dir():
                print(
                    f"ERROR: runtime_tool '{tool_alias}' is a directory; use cipd_prebuilt_directory: {tool_src}",
                    file=sys.stderr,
                )
                return 1
            source_label = f"runtime_tool '{tool_alias}' ({tool_src})"
            if tool_alias in staged_commands:
                print(
                    f"ERROR: Duplicate runtime tool command '{tool_alias}' in .bin: "
                    f"provided by both {staged_commands[tool_alias]} and {source_label}",
                    file=sys.stderr,
                )
                return 1
            staged_commands[tool_alias] = source_label
            _stage_artifact(
                tool_src.resolve(),
                bin_dir / tool_alias,
                execroot,
                effective_fuchsia_dir,
                require_executable=True,
            )
            if has_runfiles:
                _force_symlink(
                    bin_dir / f"{tool_alias}.runfiles",
                    Path("..") / f"{entry_name}.runfiles",
                )

        for tool_alias, (
            fx_launcher_rel,
            fx_runtime_rel,
        ) in resolved_fx_tools.items():
            fx_launcher_src = (
                fx_launcher_rel
                if fx_launcher_rel.is_absolute()
                else execroot / fx_launcher_rel
            )
            fx_runtime_src = (
                fx_runtime_rel
                if fx_runtime_rel.is_absolute()
                else execroot / fx_runtime_rel
            )
            if not fx_launcher_src.is_file():
                print(
                    f"ERROR: Missing runtime_fx_tool launcher '{tool_alias}': {fx_launcher_src}",
                    file=sys.stderr,
                )
                return 1
            if not fx_runtime_src.is_dir():
                print(
                    f"ERROR: Missing runtime_fx_tool runtime_dir '{tool_alias}': {fx_runtime_src}",
                    file=sys.stderr,
                )
                return 1
            source_label = f"runtime_fx_tool '{tool_alias}' ({fx_launcher_src})"
            if tool_alias in staged_commands:
                print(
                    f"ERROR: Duplicate runtime tool command '{tool_alias}' in .bin: "
                    f"provided by both {staged_commands[tool_alias]} and {source_label}",
                    file=sys.stderr,
                )
                return 1
            staged_commands[tool_alias] = source_label
            staged_launcher = bin_dir / tool_alias
            if staged_launcher.exists() or staged_launcher.is_symlink():
                staged_launcher.unlink()
            shutil.copy2(fx_launcher_src.resolve(), staged_launcher)
            staged_launcher.chmod(0o755)

            staged_runtime = bin_dir / f"{tool_alias}.runtime_dir"
            shutil.copytree(
                fx_runtime_src.resolve(),
                staged_runtime,
                symlinks=True,
                copy_function=_link_or_copy_file,
                dirs_exist_ok=True,
            )
            _make_dirs_writable(staged_runtime)

    runtime_rel = shlex.quote(
        os.path.relpath(output_runtime_dir, output_launcher.parent)
    )
    quoted_entry = shlex.quote(entry_name)
    launcher_lines = [
        "#!/bin/bash",
        "# Copyright 2026 The Fuchsia Authors. All rights reserved.",
        "# Use of this source code is governed by a BSD-style license that can be",
        "# found in the LICENSE file.",
        "set -e",
        '_FX_TOOL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"',
        f'_FX_RUNTIME_DIR="${{_FX_TOOL_DIR}}"/{runtime_rel}',
        'if [[ -n "${BUILD_WORKING_DIRECTORY:-}" && -d "${BUILD_WORKING_DIRECTORY}" ]]; then',
        '  cd "${BUILD_WORKING_DIRECTORY}"',
        "fi",
        "unset BUILD_WORKING_DIRECTORY",
    ]
    if has_runfiles:
        launcher_lines.extend(
            [
                f'export RUNFILES_DIR="${{_FX_RUNTIME_DIR}}"/{quoted_entry}.runfiles',
                "unset RUNFILES_MANIFEST_FILE",
            ]
        )
    if has_bin_dir:
        launcher_lines.extend(
            [
                'export FX_RUNTIME_TOOLS_DIR="${_FX_RUNTIME_DIR}/.bin"',
                'export PATH="${FX_RUNTIME_TOOLS_DIR}${PATH:+:$PATH}"',
            ]
        )
    if entry_uses_compact_python:
        launcher_lines.extend(
            (
                'if [[ -z "${PYTHONSAFEPATH+x}" ]]; then',
                "  export _FX_UNSET_PYTHONSAFEPATH=1",
                "else",
                "  unset _FX_UNSET_PYTHONSAFEPATH",
                "fi",
            )
        )
    launcher_lines.append(f'exec "${{_FX_RUNTIME_DIR}}"/{quoted_entry} "$@"')

    manifest_entries: dict[str, str] = {}
    for item in source_files or []:
        if "=" in item:
            rel_path, target = item.split("=", 1)
        else:
            rel_path, target = item, item
        if rel_path.startswith(
            (
                "../",
                "out/",
                "bazel-out/",
                "external/",
                "prebuilt/",
                "prebuilt_out/",
                "third_party/",
            )
        ):
            continue
        target_path = (
            Path(target) if Path(target).is_absolute() else execroot / target
        )
        if target_path.is_file() and rel_path not in manifest_entries:
            manifest_entries[rel_path] = _file_git_sha1(target_path)

    manifest_lines = [
        f"{h}  {path}" for path, h in sorted(manifest_entries.items())
    ]
    if manifest_lines:
        (output_runtime_dir / "sources.manifest").write_text(
            "\n".join(manifest_lines) + "\n", encoding="utf-8"
        )

    if source_packages:
        pkg_manifest_lines = []
        for pkg in sorted(set(source_packages)):
            pkg_clean = pkg.strip("/")
            if pkg_clean:
                pkg_manifest_lines.append(f"{pkg_clean}/BUILD.bazel")
            else:
                pkg_manifest_lines.append("BUILD.bazel")
        if pkg_manifest_lines:
            (output_runtime_dir / "packages.manifest").write_text(
                "\n".join(pkg_manifest_lines) + "\n", encoding="utf-8"
            )

    output_launcher.parent.mkdir(parents=True, exist_ok=True)
    output_launcher.write_text("\n".join(launcher_lines) + "\n")
    output_launcher.chmod(0o755)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--entry-point", type=Path, required=True)
    parser.add_argument("--entry-runfiles-manifest", type=Path, default=None)
    parser.add_argument("--output-launcher", type=Path, required=True)
    parser.add_argument("--output-runtime-dir", type=Path, required=True)
    parser.add_argument(
        "--bazel-execroot", type=Path, default=Path.cwd(), required=False
    )
    parser.add_argument("--fuchsia-dir", type=Path, default=None)
    parser.add_argument("--data-runfile", action="append", default=[], type=str)
    parser.add_argument(
        "--runtime-tool",
        action="append",
        default=[],
        type=str,
        help="Mapping of <name>=<path> to expose in PATH at runtime.",
    )
    parser.add_argument(
        "--runtime-tool-manifest",
        action="append",
        default=[],
        type=str,
        help="Mapping of <name>=<manifest_path> for Bazel runfiles of a runtime_tool.",
    )
    parser.add_argument(
        "--runtime-cipd-dir",
        action="append",
        default=[],
        type=str,
        help="Mapping of <name>=<tree_path>:<source_rel_path> for CIPD package directories.",
    )
    parser.add_argument(
        "--runtime-fx-tool",
        action="append",
        default=[],
        type=str,
        help="Mapping of <name>=<launcher_path>:<runtime_dir_path> for nested fx_host_tool targets.",
    )
    parser.add_argument(
        "--always-build",
        action="store_true",
        default=False,
        help="Write a .always_build marker into the runtime directory for compiled host tools.",
    )
    parser.add_argument(
        "--source-file",
        action="append",
        default=[],
        type=str,
        help="Workspace-relative source files to include in sources.manifest (format: [rel_path=]path)",
    )
    parser.add_argument(
        "--source-package",
        action="append",
        default=[],
        dest="source_packages",
        help="Transitive Bazel package path containing a BUILD.bazel file.",
    )
    args = parser.parse_args(argv)

    runtime_tools: dict[str, Path] = {}
    for spec in args.runtime_tool:
        alias, sep, raw_path = spec.partition("=")
        if sep != "=" or not alias or not raw_path:
            print(
                f"ERROR: Invalid --runtime-tool='{spec}', expected <name>=<path>",
                file=sys.stderr,
            )
            return 1
        if alias in runtime_tools:
            print(
                f"ERROR: Duplicate --runtime-tool command '{alias}': "
                f"{runtime_tools[alias]} vs {raw_path}",
                file=sys.stderr,
            )
            return 1
        runtime_tools[alias] = Path(raw_path)

    runtime_tool_manifests: dict[str, Path] = {}
    for spec in args.runtime_tool_manifest:
        alias, sep, raw_path = spec.partition("=")
        if sep != "=" or not alias or not raw_path:
            print(
                f"ERROR: Invalid --runtime-tool-manifest='{spec}', expected <name>=<manifest_path>",
                file=sys.stderr,
            )
            return 1
        if alias in runtime_tool_manifests:
            print(
                f"ERROR: Duplicate --runtime-tool-manifest for '{alias}': "
                f"{runtime_tool_manifests[alias]} vs {raw_path}",
                file=sys.stderr,
            )
            return 1
        runtime_tool_manifests[alias] = Path(raw_path)

    runtime_cipd_dirs: dict[str, tuple[Path, str]] = {}
    for spec in args.runtime_cipd_dir:
        alias, sep, raw_pair = spec.partition("=")
        tree_raw, colon, src_rel = raw_pair.partition(":")
        if (
            sep != "="
            or colon != ":"
            or not alias
            or not tree_raw
            or not src_rel
        ):
            print(
                f"ERROR: Invalid --runtime-cipd-dir='{spec}', expected <name>=<tree_path>:<source_rel_path>",
                file=sys.stderr,
            )
            return 1
        if alias in runtime_cipd_dirs:
            print(
                f"ERROR: Duplicate --runtime-cipd-dir '{alias}'",
                file=sys.stderr,
            )
            return 1
        runtime_cipd_dirs[alias] = (Path(tree_raw), src_rel)

    runtime_fx_tools: dict[str, tuple[Path, Path]] = {}
    for spec in args.runtime_fx_tool:
        alias, sep, raw_pair = spec.partition("=")
        launcher_raw, colon, runtime_raw = raw_pair.partition(":")
        if (
            sep != "="
            or colon != ":"
            or not alias
            or not launcher_raw
            or not runtime_raw
        ):
            print(
                f"ERROR: Invalid --runtime-fx-tool='{spec}', expected <name>=<launcher_path>:<runtime_dir_path>",
                file=sys.stderr,
            )
            return 1
        if alias in runtime_fx_tools:
            print(
                f"ERROR: Duplicate --runtime-fx-tool command '{alias}'",
                file=sys.stderr,
            )
            return 1
        runtime_fx_tools[alias] = (Path(launcher_raw), Path(runtime_raw))

    return generate_fx_host_tool(
        entry_point=args.entry_point,
        entry_runfiles_manifest=args.entry_runfiles_manifest,
        output_launcher=args.output_launcher,
        output_runtime_dir=args.output_runtime_dir,
        bazel_execroot=args.bazel_execroot,
        data_runfiles=args.data_runfile,
        runtime_tools=runtime_tools,
        runtime_tool_manifests=runtime_tool_manifests,
        runtime_cipd_dirs=runtime_cipd_dirs,
        runtime_fx_tools=runtime_fx_tools,
        fuchsia_dir=args.fuchsia_dir,
        always_build=args.always_build,
        source_files=args.source_file,
        source_packages=args.source_packages,
    )


if __name__ == "__main__":
    sys.exit(main())
