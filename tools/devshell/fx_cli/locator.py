#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""CLI for fx subcommand resolution, PATH computation, and help rendering."""

import argparse
import os
import subprocess
import sys
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from typing import Any

try:
    from fx_cli.registry import (
        Category,
        RegistryEntry,
        RegistryError,
        load_registry_file,
        resolve_fuchsia_dir,
    )
except ImportError:
    from registry import (
        Category,
        RegistryEntry,
        RegistryError,
        load_registry_file,
        resolve_fuchsia_dir,
    )


class CommandKind(StrEnum):
    REGISTERED_SCRIPT = "registered_script"
    REGISTERED_TARGET = "registered_target"
    LEGACY_SCRIPT = "legacy_script"
    LEGACY_FX_HOST_TOOL = "legacy_fx_host_tool"
    NOT_FOUND = "not_found"


class ResolutionMode(StrEnum):
    SCRIPT = "script"
    TARGET = "target"
    LEGACY = "legacy"
    LEGACY_FX = "legacy_fx"
    LEGACY_HOST_TOOL = "legacy_host_tool"
    NONE = "none"


@dataclass
class ResolvedCommand:
    """Result of 5-level vendor-first fx subcommand resolution."""

    kind: CommandKind
    vendor: str = ""
    target: str = ""
    script: str = ""
    exec_path: str = ""
    inject_prebuilt_paths: int = 0
    category: str = Category.OTHER
    summary: str = ""
    contrib: int = 0
    deprecated: int = 0
    source: str = ""
    spec: str = ""
    mode: ResolutionMode = ResolutionMode.NONE

    @classmethod
    def from_registry_entry(
        cls,
        fuchsia_dir: Path,
        entry: RegistryEntry,
        vendor: str = "",
        source: Path | None = None,
    ) -> "ResolvedCommand":
        is_script = entry.is_script
        target_spec = entry.spec
        exec_path = (
            str(fuchsia_dir / (entry.script or "")[2:])
            if is_script
            else target_spec
        )
        return cls(
            kind=(
                CommandKind.REGISTERED_SCRIPT
                if is_script
                else CommandKind.REGISTERED_TARGET
            ),
            vendor=vendor,
            target=entry.target or "",
            script=entry.script or "",
            exec_path=exec_path,
            inject_prebuilt_paths=1 if entry.inject_prebuilt_paths else 0,
            category=entry.category,
            summary=entry.summary,
            contrib=1 if entry.contrib else 0,
            deprecated=1 if entry.deprecated else 0,
            source=str(source) if source else (entry.source or ""),
            spec=target_spec,
            mode=ResolutionMode.SCRIPT if is_script else ResolutionMode.TARGET,
        )

    @classmethod
    def not_found(cls) -> "ResolvedCommand":
        return cls(kind=CommandKind.NOT_FOUND)

    def to_dict(self) -> dict[str, Any]:
        if self.kind == CommandKind.NOT_FOUND:
            return {"kind": self.kind}
        return {
            "kind": self.kind,
            "vendor": self.vendor,
            "target": self.target,
            "script": self.script,
            "exec_path": self.exec_path,
            "inject_prebuilt_paths": self.inject_prebuilt_paths,
            "category": self.category,
            "summary": self.summary,
            "contrib": self.contrib,
            "deprecated": self.deprecated,
            "source": self.source,
            "spec": self.spec,
            "mode": self.mode,
        }

    def __getitem__(self, key: str) -> Any:
        d = self.to_dict()
        if key in d:
            return d[key]
        raise KeyError(key)

    def get(self, key: str, default: Any = None) -> Any:
        return self.to_dict().get(key, default)

    def __contains__(self, key: str) -> bool:
        return key in self.to_dict()

    def keys(self) -> Any:
        return self.to_dict().keys()


def resolve_command(
    fuchsia_dir: Path,
    cmd_name: str,
    build_dir: Path | None = None,
    vendor_name: str | None = None,
) -> ResolvedCommand:
    vendor_root = fuchsia_dir / "vendor"

    # Level 1: Vendor registered tools
    if vendor_name:
        vd = vendor_root / vendor_name
        v_reg = vd / "scripts" / "devshell" / "fx_host_tools.json"
        v_entries = load_registry_file(
            v_reg, source_label=f"vendor/{vendor_name}"
        )
        if cmd_name in v_entries:
            return ResolvedCommand.from_registry_entry(
                fuchsia_dir,
                v_entries[cmd_name],
                vendor=vendor_name,
                source=v_reg,
            )
    elif vendor_root.is_dir():
        matched_vendor_entries: list[tuple[str, RegistryEntry, Path]] = []
        for vd in sorted(vendor_root.iterdir(), key=lambda p: p.name):
            if vd.is_dir():
                v_reg = vd / "scripts" / "devshell" / "fx_host_tools.json"
                v_entries = load_registry_file(
                    v_reg, source_label=f"vendor/{vd.name}"
                )
                if cmd_name in v_entries:
                    matched_vendor_entries.append(
                        (vd.name, v_entries[cmd_name], v_reg)
                    )

        if len(matched_vendor_entries) > 1:
            matched_names = [v for v, _, _ in matched_vendor_entries]
            raise RegistryError(
                f"Command '{cmd_name}' is registered in multiple vendor repositories "
                f"({' '.join(matched_names)}). Run 'fx vendor <vendor> {cmd_name}' instead."
            )
        elif len(matched_vendor_entries) == 1:
            v_name, entry, v_reg = matched_vendor_entries[0]
            return ResolvedCommand.from_registry_entry(
                fuchsia_dir, entry, vendor=v_name, source=v_reg
            )

    # Level 2: Vendor legacy scripts / .fx metadata
    vendor_dirs: list[Path] = []
    if vendor_name:
        vpath = vendor_root / vendor_name
        if vpath.is_dir():
            vendor_dirs.append(vpath)
    elif vendor_root.is_dir():
        for vd in sorted(vendor_root.iterdir(), key=lambda p: p.name):
            if vd.is_dir():
                vendor_dirs.append(vd)

    for vd in vendor_dirs:
        for subdir in ("scripts/devshell", "scripts/devshell/contrib"):
            dir_path = vd / subdir
            if not dir_path.is_dir():
                continue
            cand = dir_path / cmd_name
            if cand.is_file() and os.access(cand, os.X_OK):
                return ResolvedCommand(
                    kind=CommandKind.LEGACY_SCRIPT,
                    vendor=vd.name,
                    script=str(cand),
                    exec_path=str(cand),
                    inject_prebuilt_paths=1,
                    category=Category.OTHER,
                    contrib=1 if "contrib" in subdir else 0,
                    source=str(dir_path),
                    spec=str(cand),
                    mode=ResolutionMode.LEGACY,
                )
            cand_fx = dir_path / f"{cmd_name}.fx"
            if cand_fx.is_file():
                return ResolvedCommand(
                    kind=CommandKind.LEGACY_FX_HOST_TOOL,
                    vendor=vd.name,
                    script=str(cand_fx),
                    exec_path=str(cand_fx),
                    inject_prebuilt_paths=1,
                    category=Category.OTHER,
                    contrib=1 if "contrib" in subdir else 0,
                    source=str(cand_fx),
                    spec=str(cand_fx),
                    mode=ResolutionMode.LEGACY_FX,
                )

    # If --vendor was specified, do not search core tools
    if vendor_name:
        return ResolvedCommand.not_found()

    # Level 3: Core registered tools
    core_reg = fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
    core_entries = load_registry_file(core_reg, source_label="core")
    if cmd_name in core_entries:
        return ResolvedCommand.from_registry_entry(
            fuchsia_dir, core_entries[cmd_name], vendor="", source=core_reg
        )

    # Level 4: Core legacy scripts / .fx metadata
    for subdir in ("tools/devshell", "tools/devshell/contrib"):
        dir_path = fuchsia_dir / subdir
        if not dir_path.is_dir():
            continue
        cand = dir_path / cmd_name
        if cand.is_file() and os.access(cand, os.X_OK):
            return ResolvedCommand(
                kind=CommandKind.LEGACY_SCRIPT,
                script=str(cand),
                exec_path=str(cand),
                inject_prebuilt_paths=1,
                category=Category.OTHER,
                contrib=1 if "contrib" in subdir else 0,
                source=str(dir_path),
                spec=str(cand),
                mode=ResolutionMode.LEGACY,
            )
        cand_fx = dir_path / f"{cmd_name}.fx"
        if cand_fx.is_file():
            return ResolvedCommand(
                kind=CommandKind.LEGACY_FX_HOST_TOOL,
                script=str(cand_fx),
                exec_path=str(cand_fx),
                inject_prebuilt_paths=1,
                category=Category.OTHER,
                contrib=1 if "contrib" in subdir else 0,
                source=str(cand_fx),
                spec=str(cand_fx),
                mode=ResolutionMode.LEGACY_FX,
            )

    # Level 5: Legacy host tools fallback in build_dir/host-tools
    if build_dir is not None:
        cand_host_tool = build_dir / "host-tools" / cmd_name
        if cand_host_tool.is_file() and os.access(cand_host_tool, os.X_OK):
            return ResolvedCommand(
                kind=CommandKind.LEGACY_FX_HOST_TOOL,
                script=str(cand_host_tool),
                exec_path=str(cand_host_tool),
                inject_prebuilt_paths=1,
                category=Category.OTHER,
                source=str(cand_host_tool),
                spec=str(cand_host_tool),
                mode=ResolutionMode.LEGACY_HOST_TOOL,
            )

    return ResolvedCommand.not_found()


def compute_isolated_path(
    inject_prebuilt_paths: int,
    current_path: str,
    prebuilt_all_paths: str,
    saved_prebuilt_all_paths: str,
) -> str:
    path_segments = [p for p in current_path.split(":") if p]

    # Always strip any runtime_dir bin dirs from parent Bazel invocations
    cleaned_segments: list[str] = []
    for seg in path_segments:
        norm = seg.rstrip("/")
        if norm.endswith(".runtime_dir/.bin") or norm.endswith(
            ".runtime_dir/bin"
        ):
            continue
        cleaned_segments.append(seg)

    prebuilt_segments = [p for p in prebuilt_all_paths.split(":") if p]
    saved_segments = [p for p in saved_prebuilt_all_paths.split(":") if p]

    prebuilt_set = set(prebuilt_segments) | set(saved_segments)
    stripped_segments = [
        seg for seg in cleaned_segments if seg not in prebuilt_set
    ]
    if inject_prebuilt_paths == 0:
        return ":".join(stripped_segments)

    # inject_prebuilt_paths == 1: prepend prebuilts before ambient PATH segments
    restore_candidates = saved_segments if saved_segments else prebuilt_segments
    result: list[str] = []
    for seg in restore_candidates + stripped_segments:
        if seg not in result:
            result.append(seg)
    return ":".join(result)


def _normalize_build_dir(
    fuchsia_dir: Path, raw_build_dir: str | None
) -> Path | None:
    if not raw_build_dir:
        return None
    cleaned = raw_build_dir.strip()
    if cleaned.startswith("//"):
        cleaned = cleaned[2:]
    if not cleaned:
        return None
    bpath = Path(cleaned)
    return bpath if bpath.is_absolute() else (fuchsia_dir / bpath)


def _cmd_resolve(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    build_dir = _normalize_build_dir(fuchsia_dir, args.build_dir)
    res = resolve_command(
        fuchsia_dir,
        args.command,
        build_dir=build_dir,
        vendor_name=args.vendor,
    )
    if res.kind == CommandKind.NOT_FOUND:
        print("KIND=not_found")
        return 1

    print(f"KIND={res.kind}")
    print(f"VENDOR={res.vendor}")
    print(f"TARGET={res.target}")
    print(f"SCRIPT={res.script}")
    print(f"EXEC_PATH={res.exec_path}")
    print(f"INJECT_PREBUILT_PATHS={res.inject_prebuilt_paths}")
    if args.path:
        computed_path = compute_isolated_path(
            inject_prebuilt_paths=res.inject_prebuilt_paths,
            current_path=args.path,
            prebuilt_all_paths=args.prebuilt_all_paths or "",
            saved_prebuilt_all_paths=args.saved_prebuilt_all_paths or "",
        )
        print(f"COMPUTED_PATH={computed_path}")
    print(f"CATEGORY={res.category}")
    print(f"SUMMARY={res.summary}")
    print(f"CONTRIB={res.contrib}")
    print(f"DEPRECATED={res.deprecated}")
    print(f"SOURCE={res.source}")
    print(f"SPEC={res.spec}")
    print(f"MODE={res.mode}")
    return 0


def _cmd_help_command(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    build_dir = _normalize_build_dir(fuchsia_dir, args.build_dir)
    cmd = args.command
    res = resolve_command(
        fuchsia_dir,
        cmd,
        build_dir=build_dir,
        vendor_name=args.vendor,
    )
    if res.kind not in (
        CommandKind.REGISTERED_SCRIPT,
        CommandKind.REGISTERED_TARGET,
    ):
        return 1

    env = os.environ.copy()
    env["FX_INJECT_PREBUILT_PATHS"] = str(res.inject_prebuilt_paths)
    env["PATH"] = compute_isolated_path(
        inject_prebuilt_paths=res.inject_prebuilt_paths,
        current_path=env.get("PATH", ""),
        prebuilt_all_paths=env.get("PREBUILT_ALL_PATHS", ""),
        saved_prebuilt_all_paths=env.get("_FX_SAVED_PREBUILT_ALL_PATHS", ""),
    )

    if res.kind == CommandKind.REGISTERED_SCRIPT:
        script_path = Path(res.exec_path)
        if script_path.is_file() and (
            script_path.suffix == ".py" or os.access(script_path, os.X_OK)
        ):
            cmd_argv = (
                [sys.executable, str(script_path), "--help"]
                if script_path.suffix == ".py"
                else [str(script_path), "--help"]
            )
            proc = subprocess.run(
                cmd_argv,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            if proc.stdout:
                sys.stdout.write(proc.stdout)
            if proc.stderr:
                sys.stderr.write(proc.stderr)
            return proc.returncode
        print(f"{res.summary}\n")
        print(f"Category: {res.category}")
        print(f"Registry: {res.mode} {res.spec}")
        return 0

    # Registered target mode
    out_root = Path(env.get("FUCHSIA_OUT_DIR") or (fuchsia_dir / "out"))
    vendor_name = args.vendor or res.vendor
    vendor_sub = f"vendor/{vendor_name}" if vendor_name else ""
    candidates = [out_root / "_fx_host_tools" / vendor_sub / cmd]
    if build_dir is not None and build_dir.is_dir():
        candidates.extend(
            [
                build_dir.parent / "_fx_host_tools" / vendor_sub / cmd,
                build_dir / "host-tools" / cmd,
            ]
        )
    for cand in candidates:
        if cand.is_file() and os.access(cand, os.X_OK):
            proc = subprocess.run(
                [str(cand), "--help"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            if proc.stdout:
                sys.stdout.write(proc.stdout)
            if proc.stderr:
                sys.stderr.write(proc.stderr)
            return proc.returncode

    print(f"fx {cmd}: {res.summary or f'registered host tool ({res.spec})'}")
    if res.category:
        print(f"  Category: {res.category}")
    print(f"  Target: {res.spec}\n")
    print(f"Run fx {cmd} --help to build and view full flag documentation.")
    return 0


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fx_cli.locator",
        description="Subcommand locator and environment preparation for fx.",
    )
    parser.add_argument(
        "--fuchsia-dir",
        help="Path to Fuchsia source checkout root.",
    )

    subparsers = parser.add_subparsers(dest="subcommand")

    resolve_parser = subparsers.add_parser(
        "resolve",
        help="Resolve subcommand using 4-level vendor-first resolution.",
    )
    resolve_parser.add_argument("command", help="Command name.")
    resolve_parser.add_argument(
        "--vendor",
        help="Vendor name if restricted to a specific vendor.",
    )
    resolve_parser.add_argument(
        "--build-dir",
        help="Fuchsia build directory path.",
    )
    resolve_parser.add_argument(
        "--path",
        help="Current PATH value to compute isolated or injected PATH.",
    )
    resolve_parser.add_argument(
        "--prebuilt-all-paths",
        default="",
        help="Colon-separated PREBUILT_ALL_PATHS.",
    )
    resolve_parser.add_argument(
        "--saved-prebuilt-all-paths",
        default="",
        help="Colon-separated _FX_SAVED_PREBUILT_ALL_PATHS.",
    )

    help_parser = subparsers.add_parser(
        "help-command",
        help="Print help metadata for registered command.",
    )
    help_parser.add_argument("command", help="Command name.")
    help_parser.add_argument(
        "--vendor",
        help="Vendor name if restricted to a specific vendor.",
    )
    help_parser.add_argument(
        "--build-dir",
        help="Fuchsia build directory path.",
    )

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    fuchsia_dir = (
        Path(args.fuchsia_dir).resolve()
        if args.fuchsia_dir
        else resolve_fuchsia_dir()
    )

    try:
        if args.subcommand == "resolve":
            return _cmd_resolve(args, fuchsia_dir)
        if args.subcommand == "help-command":
            return _cmd_help_command(args, fuchsia_dir)
    except RegistryError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    except OSError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    parser.print_help()
    return 1


if __name__ == "__main__":
    sys.exit(main())
