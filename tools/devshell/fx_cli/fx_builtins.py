#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Built-in fx subcommands implemented in Python."""

import contextlib
import dataclasses
import os
import pathlib
import platform
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from typing import TextIO

import builder
import locator
import metadata
import registry

SHORT_USAGE = """\
usage: fx [--dir BUILD_DIR] [-t DEVICE_NAME] [-i] [-x] [--build|--no-build] COMMAND [...]
Run Fuchsia development commands. Must be run from a directory
that is contained in a Platform Source Tree.

  For full help, use `fx help --full`.

  For detailed help on any command, use `fx help <command>`.


host shell extensions: (requires "source scripts/fx-env.sh")
  fx-update-path            Add useful tools to the PATH
  fx-set-prompt             Display the current configuration in the shell prompt

configure and build
  set <product>.<board>     Set the product and board that will be built
  list-products             List available products
  list-boards               List available  boards
  status                    Show the current target configuration
  build                     Build the currently set configuration
  clean                     Clean the build artifacts

use the emulator
  ffx emu start             Emulate a device and interact with it
  ffx emu start --headless  Run without a graphic terminal
  shell power shutdown      Shut down Fuchsia on the emulator
  ffx emu --help            Show emu help

test
  test <component>          Run a test
  test --info [<pattern>]   List tests in the current build
  help test                 Detailed help for the test command

preparing a fuchsia target and device
  list-devices              List all reachable Fuchsia targets
  set-device                Set the default target for the build
  unset-device              Unset the default target
  flash                     Install Fuchsia from fastboot
  ota                       Send incremental over-the-air update to the device
  serve                     Start the package server and attach to a
                            running Fuchsia target

start a remote shell on a Fuchsia target
  shell                     Initiate an interactive SSH session
  shell <cmd>               Run a command

other
  format-code               Format code, usually to prepare for review
  rfc                       Scaffold RFCs from the template and automatically fills metadata files

example basic workflow using an emulator
  $ fx set core.x64 --with //examples/hello_world
  $ fx build
  $ fx ffx emu start --headless --net tap  # headless with tun/tap-based networking
  $ fx serve                               # [Start in a new window]
  $ fx test hello-world-cpp-unittests      # [Start in a new window] Run test
  $ fx ffx emu stop                        # Shut down the emulator

online documentation
  Fuchsia development:    https://fuchsia.dev/fuchsia-src/development
  Fuchsia workflows:      https://fuchsia.dev/fuchsia-src/development/build/fx
"""

_SHORT_USAGE = SHORT_USAGE

OptionalFeature = metadata.OptionalFeature
_find_lib_file = metadata.find_lib_file
_read_lib_text = metadata.read_lib_text
load_agent_env_vars = metadata.load_agent_env_vars
load_optional_features_meta = metadata.load_optional_features_meta
resolve_build_dir = metadata.resolve_build_dir


@dataclasses.dataclass(frozen=True)
class BuiltinCommand:
    """Metadata and handler for a built-in Python fx subcommand."""

    name: str
    handler: Callable[..., int]
    can_handle: Callable[[Sequence[str]], bool] | None = None


BUILTINS: dict[str, BuiltinCommand] = {}


def builtin(
    name: str,
    *,
    can_handle: Callable[[Sequence[str]], bool] | None = None,
) -> Callable[[Callable[..., int]], Callable[..., int]]:
    """Register a Python function as an `fx` built-in subcommand."""

    def _decorator(fn: Callable[..., int]) -> Callable[..., int]:
        BUILTINS[name] = BuiltinCommand(
            name=name,
            handler=fn,
            can_handle=can_handle,
        )
        return fn

    return _decorator


def _is_feature_enabled(feature: str, default_enabled: bool) -> bool:
    env_name = f"FUCHSIA_DISABLED_{feature}"
    if env_name not in os.environ:
        return default_enabled
    return os.environ[env_name] != "1"


def _format_global_options(cmd_name: str, fuchsia_dir: pathlib.Path) -> str:
    lines = [
        "",
        f"Global fx options: fx [OPTION] {cmd_name} ...",
        "  --dir=BUILD_DIR                  Path to the build directory to use when running COMMAND.",
        "  -t|--target DEVICE[:SSH_PORT]    Target a specific device.",
        "                                   DEVICE may be a Fuchsia device name to be resolved using",
        "                                   ffx or an IP address.",
        "                                   An IPv4 address must be specified directly, while an IPv6",
        "                                   need to be surrounded by brackets.",
        "                                   SSH_PORT, if specified, will be used for all commands",
        "                                   that rely on SSH to connect to the device instead of the",
        "                                   default SSH port (22).",
        '                                   Note: See "fx help set-device" for more help and to set',
        "                                   the default device for a BUILD_DIR.",
        "  -i                               Iterative mode. Repeat the command whenever a file is",
        "                                   modified under your Fuchsia directory, not including",
        "                                   out/.",
        "  -x                               Print commands and their arguments as they are executed.",
        "  -xx                              Print extra logging of the fx tool itself (implies -x)",
        "  --no-build                       Skip building registered Bazel/GN host tools; fail if missing.",
        "  --build                          Force building registered Bazel host tools on invocation.",
        "  --invoker=INVOKER                Override the command invoker for metrics and logging.",
        "  --enable|disable=FEATURE  Enable or disable a feature (non-persistent). Valid features:",
    ]
    indent = "      "
    for feat in load_optional_features_meta(fuchsia_dir):
        default_str = "enabled" if feat.default_enabled else "disabled"
        current_str = (
            "enabled"
            if _is_feature_enabled(feat.name, feat.default_enabled)
            else "disabled"
        )
        lines.append(f"{indent}{feat.name}")
        lines.append(
            f"{indent}{indent}(default={default_str}, current={current_str})"
        )
        for dline in feat.description:
            lines.append(f"{indent}{indent}{dline}")
    return "\n".join(lines) + "\n"


def _is_text_file(path: pathlib.Path) -> bool:
    try:
        with path.open("rb") as f:
            chunk = f.read(4096)
        return b"\x00" not in chunk
    except OSError:
        return False


def _extract_doc_comments(
    script_path: pathlib.Path, build_dir_str: str
) -> Sequence[str]:
    """Extract ## documentation lines from a script header (matching fx-print-command-help)."""
    try:
        raw_lines = script_path.read_text(
            encoding="utf-8", errors="replace"
        ).splitlines()
    except OSError:
        return ()
    if not any(line.startswith("## ") for line in raw_lines):
        return ()
    return tuple(
        (line[3:] if line.startswith("## ") else "").replace(
            "_FX_BUILD_DIR", build_dir_str
        )
        for line in raw_lines
        if line.startswith("## ") or line == "##"
    )


def _resolve_legacy_help_path(exec_path: pathlib.Path) -> pathlib.Path:
    if not exec_path.name.endswith(".fx"):
        sibling_fx = exec_path.with_name(f"{exec_path.name}.fx")
        if sibling_fx.is_file():
            return sibling_fx
    return exec_path


_PLATFORM_ASSIGN_RE = re.compile(r'^readonly\s+([A-Z0-9_]+)="([^"]+)"')
_PLATFORM_APPEND_RE = re.compile(r'^PREBUILT_ALL_PATHS\+="([^"]+)"')
_PLATFORM_VAR_REF_RE = re.compile(r"\$\{([A-Z0-9_]+)\}")


def _parse_prebuilt_all_paths(
    text: str, fuchsia_dir: pathlib.Path, host_platform: str
) -> str:
    env_vars: dict[str, str] = {
        "FUCHSIA_DIR": str(fuchsia_dir),
        "HOST_PLATFORM": host_platform,
    }
    parts: list[str] = []
    for raw_line in text.splitlines():
        line = raw_line.strip()
        m_assign = _PLATFORM_ASSIGN_RE.match(line)
        if m_assign:
            var_name, val_template = m_assign.groups()
            if var_name.startswith("PREBUILT_"):
                env_vars[var_name] = _PLATFORM_VAR_REF_RE.sub(
                    lambda m: env_vars.get(m.group(1), ""), val_template
                )
            continue
        m_append = _PLATFORM_APPEND_RE.match(line)
        if m_append:
            parts.append(
                _PLATFORM_VAR_REF_RE.sub(
                    lambda m: env_vars.get(m.group(1), ""), m_append.group(1)
                )
            )
    return "".join(parts)


def compute_prebuilt_all_paths(fuchsia_dir: pathlib.Path) -> str:
    """Compute the colon-separated PREBUILT_ALL_PATHS list from tools/devshell/lib/platform.sh."""
    host_os = "mac" if sys.platform == "darwin" else "linux"
    machine = platform.machine()
    host_cpu = "arm64" if machine in ("aarch64", "arm64") else "x64"
    host_platform = f"{host_os}-{host_cpu}"
    text = _read_lib_text(fuchsia_dir, "platform.sh")
    parsed = (
        _parse_prebuilt_all_paths(text, fuchsia_dir, host_platform)
        if text
        else ""
    )
    if not parsed and fuchsia_dir is not None:
        fallback_text = _read_lib_text(None, "platform.sh")
        if fallback_text:
            parsed = _parse_prebuilt_all_paths(
                fallback_text, fuchsia_dir, host_platform
            )
    return parsed


@builtin("get-src-dir")
def run_get_src_dir(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    *,
    build_dir_override: str | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Built-in implementation of `fx get-src-dir`."""
    del build_dir_override
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    if args:
        if args[0] in ("-h", "--help"):
            print("usage: fx get-src-dir", file=out)
            print("", file=out)
            print(
                "Prints the root directory of the Fuchsia source checkout.",
                file=out,
            )
            return 0
        print("usage: fx get-src-dir", file=err)
        return 2
    print(fuchsia_dir.resolve(), file=out)
    return 0


@builtin("get-build-dir")
def run_get_build_dir(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    *,
    build_dir_override: str | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Built-in implementation of `fx get-build-dir`."""
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    del args
    resolved = resolve_build_dir(fuchsia_dir, build_dir_override)
    if resolved is None:
        print(
            'ERROR: No build directory found. Run "fx set" or specify a build dir with --dir or "fx use".',
            file=err,
        )
        return 1
    if not (
        (resolved / "fx.config").is_file() or (resolved / "args.gn").is_file()
    ):
        print(
            f"ERROR: Build directory missing or removed. ({resolved})",
            file=err,
        )
        print(
            'ERROR: run "fx set", or specify a build dir with --dir or "fx use"',
            file=err,
        )
        return 1
    print(resolved, file=out)
    return 0


@builtin("registry")
def run_registry(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    *,
    build_dir_override: str | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Built-in implementation of `fx registry` dispatching to registry.main."""
    del build_dir_override
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    full_args = ["--fuchsia-dir", str(fuchsia_dir), *args]
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            return registry.main(full_args)
        except SystemExit as exc:
            code = exc.code
            return int(code) if isinstance(code, int) else 1


def _can_handle_help_directly(cmd_args: Sequence[str]) -> bool:
    return (
        not cmd_args
        or (
            cmd_args[0].startswith("--")
            and not cmd_args[0].startswith("--full")
        )
        or (len(cmd_args) == 1 and not cmd_args[0].startswith("-"))
    )


@builtin("help", can_handle=_can_handle_help_directly)
def run_help(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    *,
    vendor: str | None = None,
    build_dir_override: str | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Built-in implementation of `fx help`."""
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    # Bare `fx help` / `fx -h` / `fx --help` (and `--*` flags other than `--full*`)
    if not args or (
        args[0].startswith("--") and not args[0].startswith("--full")
    ):
        out.write(_SHORT_USAGE)
        return 0

    assert len(args) == 1
    assert not args[0].startswith("-")

    cmd_name = args[0]
    resolved_bdir = resolve_build_dir(fuchsia_dir, build_dir_override)
    bdir_str = (
        str(resolved_bdir) if resolved_bdir is not None else "out/default"
    )
    try:
        resolved_cmd = locator.resolve_command(
            fuchsia_dir,
            cmd_name,
            build_dir=resolved_bdir,
            vendor_name=vendor,
        )
    except registry.RegistryError as exc:
        print(f"ERROR: {exc}", file=err)
        return 2

    kind = resolved_cmd.kind
    effective_vendor = (
        vendor
        if vendor is not None
        else (resolved_cmd.vendor if resolved_cmd.vendor else None)
    )
    if kind in ("registered_script", "registered_target"):
        script_rel = str(resolved_cmd.script or "")
        target_label = str(resolved_cmd.target or "")
        inject_val = 1 if resolved_cmd.inject_prebuilt_paths else 0
        prebuilt_all_paths = os.environ.get(
            "PREBUILT_ALL_PATHS"
        ) or compute_prebuilt_all_paths(fuchsia_dir)
        existing_pythonpath = os.environ.get("PYTHONPATH", "")
        if existing_pythonpath:
            if str(fuchsia_dir) in existing_pythonpath.split(":"):
                new_pythonpath = existing_pythonpath
            else:
                new_pythonpath = f"{fuchsia_dir}:{existing_pythonpath}"
        else:
            new_pythonpath = str(fuchsia_dir)
        help_env = os.environ | {
            "PYTHONPATH": new_pythonpath,
            "FX_INJECT_PREBUILT_PATHS": str(inject_val),
            "PATH": locator.compute_isolated_path(
                inject_prebuilt_paths=inject_val,
                current_path=os.environ.get("PATH", ""),
                prebuilt_all_paths=prebuilt_all_paths,
                saved_prebuilt_all_paths=os.environ.get(
                    "_FX_SAVED_PREBUILT_ALL_PATHS", ""
                ),
            ),
        }
        if script_rel:
            script_abs = fuchsia_dir / script_rel.lstrip("@/")
            if script_abs.is_file():
                completed_process: subprocess.CompletedProcess[str] | None
                if script_abs.suffix == ".py" or os.access(script_abs, os.X_OK):
                    cmd_argv = (
                        (sys.executable, str(script_abs), "--help")
                        if script_abs.suffix == ".py"
                        else (str(script_abs), "--help")
                    )
                    try:
                        completed_process = subprocess.run(
                            cmd_argv,
                            cwd=str(fuchsia_dir),
                            env=help_env,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            text=True,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired):
                        completed_process = None
                else:
                    completed_process = None
                if (
                    completed_process is not None
                    and completed_process.returncode == 0
                    and completed_process.stdout.strip()
                ):
                    out.write(completed_process.stdout)
                    if completed_process.stderr:
                        err.write(completed_process.stderr)
                    if (
                        "For global options, try"
                        not in completed_process.stdout
                    ):
                        out.write(_format_global_options(cmd_name, fuchsia_dir))
                    return 0
                doc_lines = _extract_doc_comments(script_abs, bdir_str)
                if doc_lines:
                    for dline in doc_lines:
                        print(dline, file=out)
                    out.write(_format_global_options(cmd_name, fuchsia_dir))
                    return 0
                if completed_process is not None:
                    if completed_process.stdout:
                        out.write(completed_process.stdout)
                    if completed_process.stderr:
                        err.write(completed_process.stderr)
                    if (
                        completed_process.returncode == 0
                        and "For global options, try"
                        not in (completed_process.stdout or "")
                    ):
                        out.write(_format_global_options(cmd_name, fuchsia_dir))
                    return completed_process.returncode
            print(f"{resolved_cmd.summary}\n", file=out)
            print(f"Category: {resolved_cmd.category}", file=out)
            print(
                f"Registry: {resolved_cmd.mode} {resolved_cmd.spec}",
                file=out,
            )
            out.write(_format_global_options(cmd_name, fuchsia_dir))
            return 0
        if target_label:
            out_root = pathlib.Path(
                os.environ.get("FUCHSIA_OUT_DIR") or (fuchsia_dir / "out")
            )
            vendor_sub = (
                pathlib.Path("vendor") / effective_vendor
                if effective_vendor is not None
                else pathlib.Path()
            )
            candidates = (
                (
                    out_root / "_fx_host_tools" / vendor_sub / cmd_name,
                    resolved_bdir.parent
                    / "_fx_host_tools"
                    / vendor_sub
                    / cmd_name,
                    resolved_bdir / "host-tools" / cmd_name,
                )
                if resolved_bdir is not None and resolved_bdir.is_dir()
                else (out_root / "_fx_host_tools" / vendor_sub / cmd_name,)
            )
            for tool_bin in candidates:
                if tool_bin.is_file() and os.access(tool_bin, os.X_OK):
                    try:
                        completed_process = subprocess.run(
                            (str(tool_bin), "--help"),
                            cwd=str(fuchsia_dir),
                            env=help_env,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            text=True,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired):
                        continue
                    if completed_process.stdout:
                        out.write(completed_process.stdout)
                    if completed_process.stderr:
                        err.write(completed_process.stderr)
                    if (
                        completed_process.returncode == 0
                        and "For global options, try"
                        not in (completed_process.stdout or "")
                    ):
                        out.write(_format_global_options(cmd_name, fuchsia_dir))
                    return completed_process.returncode
            summary = (
                str(resolved_cmd.summary or "")
                or f"registered host tool ({target_label})"
            )
            category = str(resolved_cmd.category or "")
            print(f"fx {cmd_name}: {summary}", file=out)
            if category:
                print(f"  Category: {category}", file=out)
            print(f"  Target: {target_label}\n", file=out)
            print(
                f"Run fx {cmd_name} --help to build and view full flag documentation.",
                file=out,
            )
            out.write(_format_global_options(cmd_name, fuchsia_dir))
            return 0
    elif kind in ("legacy_script", "legacy_fx_host_tool"):
        exec_path_str = str(resolved_cmd.exec_path or "")
        if exec_path_str:
            exec_path = pathlib.Path(exec_path_str)
            if exec_path.is_file():
                help_path = _resolve_legacy_help_path(exec_path)
                if _is_text_file(help_path):
                    doc_lines = _extract_doc_comments(help_path, bdir_str)
                    if doc_lines:
                        for dline in doc_lines:
                            print(dline, file=out)
                    else:
                        print(
                            f"No help found. Try `fx {cmd_name} -h`",
                            file=out,
                        )
                    out.write(_format_global_options(cmd_name, fuchsia_dir))
                    return 0
                rel_str = builder._relative(help_path, fuchsia_dir)
                print(
                    f"'{rel_str}' is a host tool and no metadata was found. "
                    f"Try running `fx {cmd_name} -h`",
                    file=out,
                )
                return 0
        print(f"Command {cmd_name} not found. Try `fx help`", file=out)
        return 0

    print(f"Command {cmd_name} not found. Try `fx help`", file=out)
    return 0
