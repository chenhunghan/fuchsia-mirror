#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Python CLI front door for `fx`."""

import contextlib
import dataclasses
import os
import pathlib
import re
import select
import shutil
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from typing import Callable, TextIO

_SCRIPT_DIR = pathlib.Path(__file__).absolute().parent
if _SCRIPT_DIR.is_dir() and str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import builder
import fx_builtins
import locator
import metrics
import registry

_DEBUGINFOD_PUBLIC_URL = "https://storage.googleapis.com/fuchsia-artifacts"
_INOTIFY_EXCLUDE_REGEX = r"/(\.|lock|compile_commands.json|lcov.info)"
_INOTIFY_BATCH_TIMEOUT_SEC = 0.01
_PRE_RESOLVED_ENV_VARS = (
    "_FX_PRE_RESOLVED_CMD",
    "_FX_PRE_RESOLVED_KIND",
    "_FX_PRE_RESOLVED_VENDOR",
    "_FX_PRE_RESOLVED_EXEC",
    "_FX_PRE_RESOLVED_MODE",
)


def _clear_pre_resolved_env() -> None:
    """Remove any inherited pre-resolved dispatch handoff variables."""
    for key in _PRE_RESOLVED_ENV_VARS:
        os.environ.pop(key, None)


_FORWARDED_SIGNALS = tuple(
    sig
    for sig in (
        getattr(signal, "SIGTERM", None),
        getattr(signal, "SIGHUP", None),
    )
    if sig is not None
)

# Matches is-valid-device in tools/devshell/lib/vars.sh
# LINT.IfChange
_IPV4_PORT_RE = re.compile(
    r"^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}(:[0-9]{1,5})?$"
)
_IPV6_BRACKET_RE = re.compile(
    r"^\[[0-9a-fA-F]{0,4}(:[0-9a-fA-F]{0,4}){2,7}(%[0-9a-zA-Z-]+)?\](:[0-9]{1,5})?$"
)
_IPV6_RAW_RE = re.compile(
    r"^[0-9a-fA-F]{0,4}(:[0-9a-fA-F]{0,4}){2,7}(%[0-9a-zA-Z-]+)?$"
)
_NODENAME_RE = re.compile(r"^([a-z0-9][.a-z0-9-]*)?(:[0-9]{1,5})?$")
_USB_OR_VSOCK_RE = re.compile(r"^(usb|vsock):cid:\d+$")
_SERIAL_OR_ID_RE = re.compile(r"^(serial|id):[A-Za-z0-9_.-]+$")
_UART_RE = re.compile(r"^uart:\S+$")
# LINT.ThenChange(//tools/devshell/lib/vars.sh)


_SHORT_USAGE = fx_builtins.SHORT_USAGE


class GlobalArgError(ValueError):
    """Raised when global fx flags are invalid."""

    def __init__(self, message: str, *, show_usage: bool = False) -> None:
        super().__init__(message)
        self.show_usage = show_usage


@dataclasses.dataclass(frozen=True)
class GlobalArgs:
    """Parsed global fx options and target subcommand."""

    build_dir: str | None = None
    target_device: str | None = None
    iterative: bool = False
    trace_x: bool = False
    trace_xx: bool = False
    no_build: bool = False
    force_build: bool = False
    feature_flags: Sequence[tuple[str, str]] = ()
    invoker: str | None = None
    command_name: str = "help"
    command_args: Sequence[str] = ()
    raw_global_args: Sequence[str] = ()


def _is_invoked_by_agent(fuchsia_dir: pathlib.Path) -> bool:
    return any(
        bool(os.environ.get(var))
        for var in fx_builtins.load_agent_env_vars(fuchsia_dir)
    )


def _check_running_on_supported_platform(stderr: TextIO) -> bool:
    """Check whether fx is running on a supported host OS."""
    if sys.platform == "darwin":
        print(
            "ERROR: This tool is no longer supported on macOS per go/fuchsia-on-mac.",
            file=stderr,
        )
        return False
    return True


def _check_running_in_agent_sandbox(stderr: TextIO) -> bool:
    """Check whether fx is running inside an agent sandbox and block execution."""
    if os.environ.get("ANTIGRAVITY_AGENT") != "1":
        return True
    status_path = pathlib.Path(
        os.environ.get("_FX_PROC_STATUS") or "/proc/self/status"
    )
    if not status_path.is_file():
        return True
    try:
        content = status_path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return True
    if re.search(r"^Seccomp:\s*[1-9]", content, re.MULTILINE):
        print(
            "ERROR: Running fx in a sandboxed environment is not supported.",
            file=stderr,
        )
        print(
            "ERROR: Please re-run the command with 'BypassSandbox: true'.",
            file=stderr,
        )
        return False
    return True


def _apply_global_env(
    fuchsia_dir: pathlib.Path, parsed: GlobalArgs, argv: Sequence[str]
) -> None:
    os.environ["FUCHSIA_DIR"] = str(fuchsia_dir)
    if not os.environ.get("FUCHSIA_OUT_DIR"):
        os.environ["FUCHSIA_OUT_DIR"] = str(fuchsia_dir / "out")
    os.environ.pop("PYTHONHOME", None)
    os.environ.pop("PYTHONSAFEPATH", None)
    existing_pythonpath = os.environ.get("PYTHONPATH", "")
    if existing_pythonpath:
        if str(fuchsia_dir) not in existing_pythonpath.split(":"):
            os.environ["PYTHONPATH"] = f"{fuchsia_dir}:{existing_pythonpath}"
    else:
        os.environ["PYTHONPATH"] = str(fuchsia_dir)
    os.environ["FX_CACHE_DIR"] = str(fuchsia_dir / ".fx")
    debuginfod_urls = os.environ.get("DEBUGINFOD_URLS", "")
    if _DEBUGINFOD_PUBLIC_URL not in debuginfod_urls:
        os.environ["DEBUGINFOD_URLS"] = (
            f"{debuginfod_urls} {_DEBUGINFOD_PUBLIC_URL}"
            if debuginfod_urls
            else _DEBUGINFOD_PUBLIC_URL
        )
    scripts_dir = str(fuchsia_dir / "scripts")
    current_path = os.environ.get("PATH", "")
    path_entries = [p for p in current_path.split(":") if p]
    if scripts_dir not in path_entries:
        os.environ["PATH"] = (
            f"{scripts_dir}:{current_path}" if current_path else scripts_dir
        )

    if parsed.build_dir is not None:
        os.environ.pop("FUCHSIA_BUILD_DIR", None)
        bdir_clean = parsed.build_dir.rstrip("/")
        if bdir_clean.startswith("//"):
            os.environ[
                "_FX_BUILD_DIR"
            ] = f"{fuchsia_dir}/{bdir_clean.lstrip('/')}"
        else:
            os.environ["_FX_BUILD_DIR"] = bdir_clean
            os.environ["FUCHSIA_BUILD_DIR_FROM_FX"] = bdir_clean

    if not os.environ.get("PYTHONPYCACHEPREFIX"):
        os.environ[
            "PYTHONPYCACHEPREFIX"
        ] = f"{os.environ['FUCHSIA_OUT_DIR']}/_build_caches/__pycache__"

    if parsed.target_device is not None:
        os.environ.pop("FUCHSIA_DEVICE_ADDR", None)
        os.environ.pop("FUCHSIA_NODENAME_IS_FROM_FILE", None)
        os.environ["FUCHSIA_NODENAME"] = parsed.target_device
        os.environ["_FX_NODENAME"] = parsed.target_device
        os.environ[
            "FUCHSIA_NODENAME_SET_BY_FX_FLAG"
        ] = "FUCHSIA_NODENAME_SET_BY_FX_FLAG"

    if parsed.iterative:
        os.environ["FUCHSIA_FX_ITERATIVE"] = "1"

    if parsed.trace_x:
        os.environ["FX_ENABLE_SHELL_TRACING"] = "true"
        os.environ["FUCHSIA_DEVSHELL_VERBOSITY"] = "1"

    if parsed.no_build:
        os.environ["FX_NO_BUILD_HOST_TOOL"] = "1"
        os.environ.pop("FX_FORCE_BUILD_HOST_TOOL", None)
        os.environ.pop("FX_FORCE_BUILD", None)
    elif parsed.force_build:
        os.environ["FX_FORCE_BUILD_HOST_TOOL"] = "1"
        os.environ.pop("FX_NO_BUILD_HOST_TOOL", None)
        os.environ.pop("FX_NO_BUILD", None)

    for mode, feat in parsed.feature_flags:
        os.environ[f"FX_ENABLE_{feat.upper()}"] = (
            "1" if mode == "enable" else "0"
        )
        os.environ[f"FUCHSIA_DISABLED_{feat}"] = (
            "0" if mode == "enable" else "1"
        )

    for opt_feat in fx_builtins.load_optional_features_meta(fuchsia_dir):
        disabled_var = f"FUCHSIA_DISABLED_{opt_feat.name}"
        if disabled_var not in os.environ:
            os.environ[disabled_var] = "0" if opt_feat.default_enabled else "1"
        else:
            os.environ[disabled_var] = (
                "0" if os.environ[disabled_var] != "1" else "1"
            )

    if parsed.invoker is not None:
        os.environ["FUCHSIA_FX_INVOKER"] = parsed.invoker

    if _is_invoked_by_agent(fuchsia_dir):
        if os.environ.get("FUCHSIA_DEVSHELL_VERBOSITY") != "1":
            os.environ["FX_BUILD_QUIET"] = "1"
        if not os.environ.get("FUCHSIA_FX_INVOKER"):
            os.environ["FUCHSIA_FX_INVOKER"] = "antigravity"

    os.environ["FX_ARGV"] = " ".join(argv)
    if parsed.command_name != "fx-reentry":
        os.environ["FX_ENTRY_CMD"] = parsed.command_name
        os.environ["FX_ENTRY_ARGS"] = " ".join(parsed.command_args)


def _drain_inotify_batch(fd: int) -> None:
    """Drain subsequent inotifywait events arriving within 10ms (matching read -t .01)."""
    while True:
        ready, _, _ = select.select([fd], [], [], _INOTIFY_BATCH_TIMEOUT_SEC)
        if not ready:
            break
        chunk = os.read(fd, 65536)
        if not chunk:
            break


def _run_iterative(
    fuchsia_dir: pathlib.Path,
    display_name: str,
    run_once: Callable[[], int],
    *,
    stdout: TextIO,
) -> int:
    """Monitor source changes with inotifywait and re-run run_once on modify events."""
    if shutil.which("inotifywait") is None:
        print("Missing inotifywait", file=stdout)
        print("Try: sudo apt-get install inotify-tools", file=stdout)
        return 1

    inotify_cmd = (
        "inotifywait",
        "-qrme",
        "modify",
        "--exclude",
        _INOTIFY_EXCLUDE_REGEX,
        str(fuchsia_dir),
        f"@{fuchsia_dir}/out",
        f"@{fuchsia_dir}/zircon/public",
    )
    proc = subprocess.Popen(
        inotify_cmd,
        stdout=subprocess.PIPE,
    )
    assert proc.stdout is not None
    fd = proc.stdout.fileno()
    try:
        while True:
            buf = b""
            while b"\n" not in buf:
                chunk = os.read(fd, 4096)
                if not chunk:
                    return 0
                buf += chunk

            _drain_inotify_batch(fd)

            print(
                f"---------------------------------- fx -i {display_name} ---------------------------------------",
                file=stdout,
                flush=True,
            )
            start_sec = int(time.time())
            if run_once() == 130:
                return 130
            end_sec = int(time.time())
            print(
                f"--- Done in {end_sec - start_sec} seconds",
                file=stdout,
                flush=True,
            )
    except KeyboardInterrupt:
        return 130
    finally:
        proc.stdout.close()
        proc.terminate()
        try:
            proc.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def _apply_registered_path(
    fuchsia_dir: pathlib.Path, inject_prebuilt_paths: int
) -> None:
    os.environ["FX_INJECT_PREBUILT_PATHS"] = (
        "1" if inject_prebuilt_paths else "0"
    )
    prebuilt_all_paths = fx_builtins.compute_prebuilt_all_paths(fuchsia_dir)
    current_path = os.environ.get("PATH", "")
    saved_prebuilt = os.environ.get("_FX_SAVED_PREBUILT_ALL_PATHS", "")
    os.environ["PATH"] = locator.compute_isolated_path(
        inject_prebuilt_paths=1 if inject_prebuilt_paths else 0,
        current_path=current_path,
        prebuilt_all_paths=prebuilt_all_paths,
        saved_prebuilt_all_paths=saved_prebuilt,
    )


def _emit_lookup_log(
    cmd_name: str, dispatch: locator.ResolvedCommand, stderr: TextIO
) -> None:
    env = os.environ
    if not builder._fx_registry_log_enabled(env):
        return
    kind = str(dispatch.kind or "")
    vendor = str(dispatch.vendor or "")
    target = str(dispatch.target or "")
    script = str(dispatch.script or "")
    if kind == "registered_target":
        if vendor:
            builder._emit_registry_action(
                f"lookup '{cmd_name}': found in vendor '{vendor}' registry (mode=target, target={target})",
                stderr,
                env,
            )
        else:
            builder._emit_registry_action(
                f"lookup '{cmd_name}': found in core registry (mode=target, target={target})",
                stderr,
                env,
            )
    elif kind == "registered_script":
        if vendor:
            builder._emit_registry_action(
                f"lookup '{cmd_name}': found in vendor '{vendor}' registry (mode=script, script={script})",
                stderr,
                env,
            )
        else:
            builder._emit_registry_action(
                f"lookup '{cmd_name}': found in core registry (mode=script, script={script})",
                stderr,
                env,
            )


def _normalize_returncode(returncode: int) -> int:
    return 128 + (-returncode) if returncode < 0 else returncode


def _run_child_process(
    cmd_argv: Sequence[str],
    *,
    stdout: TextIO,
    stderr: TextIO,
) -> int:
    """Run cmd_argv as a child process, forwarding SIGTERM/SIGHUP if received."""
    proc: subprocess.Popen[str] | None = None
    prev_handlers: dict[int, object] = {}
    is_main_thread = threading.current_thread() is threading.main_thread()

    def _forward_signal(signum: int, _frame: object) -> None:
        if proc is not None and proc.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                proc.send_signal(signum)

    try:
        if is_main_thread:
            for sig in _FORWARDED_SIGNALS:
                prev_handlers[sig] = signal.getsignal(sig)
                signal.signal(sig, _forward_signal)

        proc = subprocess.Popen(
            cmd_argv,
            stdout=stdout if stdout is sys.stdout else subprocess.PIPE,
            stderr=stderr if stderr is sys.stderr else subprocess.PIPE,
            text=True,
        )
        out_str, err_str = proc.communicate()
    except KeyboardInterrupt:
        if proc is not None and proc.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                proc.send_signal(signal.SIGINT)
            proc.wait()
        return 130
    finally:
        if is_main_thread:
            for sig, handler in prev_handlers.items():
                signal.signal(sig, handler)  # type: ignore[arg-type]

    if stdout is not sys.stdout and out_str:
        stdout.write(out_str)
    if stderr is not sys.stderr and err_str:
        stderr.write(err_str)
    return _normalize_returncode(proc.returncode)


def _dispatch_direct_command(
    fuchsia_dir: pathlib.Path,
    cmd_name: str,
    cmd_args: Sequence[str],
    *,
    build_dir_override: str | None = None,
    no_build: bool | None = None,
    force_build: bool | None = None,
    pre_resolved_out: dict[str, str] | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
    before_execute: Callable[[], None] | None = None,
) -> int | None:
    """Attempt to execute cmd_name directly in Python.

    Returns an integer exit code if handled (built-ins and commands registered
    in fx_host_tools.json), or None for any non-built-in, non-registered command
    (legacy devshell scripts, host-tools binaries, or unknown commands) so the
    caller can hand off to //scripts/fx-legacy.
    """
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    # 1. Pure Python built-in subcommands registered via @fx_builtins.builtin
    builtin_cmd = fx_builtins.BUILTINS.get(cmd_name)
    if builtin_cmd is not None:
        if builtin_cmd.can_handle is not None and not builtin_cmd.can_handle(
            cmd_args
        ):
            return None
        if before_execute is not None:
            before_execute()
        return builtin_cmd.handler(
            fuchsia_dir,
            cmd_args,
            build_dir_override=build_dir_override,
            stdout=out,
            stderr=err,
        )

    # 2. Check registered host tools (including `fx vendor <v> <cmd>`)
    if (
        cmd_name == "vendor"
        and len(cmd_args) >= 2
        and bool(cmd_args[0])
        and bool(cmd_args[1])
        and not cmd_args[0].startswith("-")
        and not cmd_args[1].startswith("-")
    ):
        vendor = cmd_args[0]
        target_cmd = cmd_args[1]
        target_args = cmd_args[2:]
    else:
        vendor = None
        target_cmd = cmd_name
        target_args = cmd_args

    resolved_bdir = fx_builtins.resolve_build_dir(
        fuchsia_dir, build_dir_override
    )
    try:
        dispatch = locator.resolve_command(
            fuchsia_dir,
            target_cmd,
            build_dir=resolved_bdir,
            vendor_name=vendor,
        )
    except registry.RegistryError as exc:
        print(f"ERROR: {exc}", file=err)
        return 2

    kind = dispatch.kind
    if kind not in ("registered_script", "registered_target"):
        if pre_resolved_out is not None and vendor is None:
            pre_resolved_out["_FX_PRE_RESOLVED_CMD"] = target_cmd
            pre_resolved_out["_FX_PRE_RESOLVED_KIND"] = str(kind or "")
            pre_resolved_out["_FX_PRE_RESOLVED_VENDOR"] = str(
                dispatch.vendor or ""
            )
            pre_resolved_out["_FX_PRE_RESOLVED_EXEC"] = str(
                dispatch.exec_path or ""
            )
            pre_resolved_out["_FX_PRE_RESOLVED_MODE"] = str(dispatch.mode or "")
        return None

    lookup_name = f"vendor {vendor} {target_cmd}" if vendor else target_cmd
    _emit_lookup_log(lookup_name, dispatch, err)

    valid_bdir = (
        resolved_bdir
        if (resolved_bdir is not None and resolved_bdir.is_dir())
        else None
    )
    if kind == "registered_target" and valid_bdir is not None:
        os.environ["FUCHSIA_BUILD_DIR"] = str(valid_bdir)
        if not os.environ.get("FUCHSIA_NODENAME_SET_BY_FX_FLAG") and (
            not os.environ.get("FUCHSIA_NODENAME")
            or os.environ.get("FUCHSIA_NODENAME_IS_FROM_FILE") == "true"
        ):
            device_file = pathlib.Path(f"{valid_bdir}.device")
            if device_file.is_file():
                try:
                    os.environ["FUCHSIA_NODENAME"] = device_file.read_text(
                        encoding="utf-8"
                    ).strip()
                    os.environ["FUCHSIA_NODENAME_IS_FROM_FILE"] = "true"
                except OSError:
                    pass

    try:
        exec_path = builder.ensure_registered_host_tool(
            builder.BuildContext(
                fuchsia_dir=fuchsia_dir,
                build_dir=valid_bdir,
                stderr=err,
            ),
            target_cmd,
            target=str(dispatch.target or ""),
            script_rel=str(dispatch.script or ""),
            vendor=vendor,
            no_build=no_build,
            force_build=force_build,
        )
    except builder.HostToolBuildError:
        if os.environ.get("FX_NO_BUILD_HOST_TOOL") != "1":
            for sub_arg in target_args:
                if sub_arg in ("--no-build", "--build"):
                    print(
                        f"WARNING: Note: '{sub_arg}' is a global fx option and must precede "
                        f"'{target_cmd}': fx {sub_arg} {target_cmd} ...",
                        file=err,
                    )
                    break
        return 1

    inject_flag = 1 if dispatch.inject_prebuilt_paths else 0
    _apply_registered_path(fuchsia_dir, inject_flag)

    if before_execute is not None:
        before_execute()

    cmd_argv = (
        (sys.executable, str(exec_path), *target_args)
        if str(exec_path).endswith(".py")
        else (str(exec_path), *target_args)
    )
    return _run_child_process(cmd_argv, stdout=out, stderr=err)


def is_valid_device(device: str) -> bool:
    """Validate device target format matching is-valid-device in vars.sh."""
    # Empty target is valid (e.g. `fx -t ""` clears any default device).
    if not device:
        return True
    return bool(
        _IPV4_PORT_RE.match(device)
        or _IPV6_BRACKET_RE.match(device)
        or _IPV6_RAW_RE.match(device)
        or _NODENAME_RE.match(device)
        or _USB_OR_VSOCK_RE.match(device)
        or _SERIAL_OR_ID_RE.match(device)
        or _UART_RE.match(device)
    )


_GLOBAL_VALUE_FLAGS = frozenset(
    ("--dir", "-t", "--target", "--enable", "--disable", "--invoker")
)


def _missing_value_error(flag: str) -> GlobalArgError:
    if flag == "--dir":
        return GlobalArgError(
            "ERROR: Missing path to build directory for --dir argument",
            show_usage=True,
        )
    if flag in ("-t", "--target"):
        return GlobalArgError(
            f"ERROR: Missing device name for {flag} argument",
            show_usage=True,
        )
    return GlobalArgError(f"ERROR: Missing argument to {flag}", show_usage=True)


def parse_global_args(
    argv: Sequence[str], *, fuchsia_dir: pathlib.Path | None = None
) -> GlobalArgs:
    """Parse global fx flags preceding the subcommand name."""
    args = tuple(argv)
    build_dir: str | None = None
    target_device: str | None = None
    iterative = False
    trace_x = False
    trace_xx = False
    no_build = False
    force_build = False
    feature_flags: list[tuple[str, str]] = []
    invoker: str | None = None
    raw_global_args: list[str] = []
    valid_optional_features: frozenset[str] | None = None
    i = 0
    n = len(args)

    while i < n:
        arg = args[i]
        if arg == "--":
            raw_global_args.append(arg)
            i += 1
            break
        if arg in ("-h", "--help"):
            raw_global_args.append(arg)
            return GlobalArgs(
                build_dir=build_dir,
                target_device=target_device,
                iterative=iterative,
                trace_x=trace_x,
                trace_xx=trace_xx,
                no_build=no_build,
                force_build=force_build,
                feature_flags=tuple(feature_flags),
                invoker=invoker,
                command_name="help",
                command_args=args[i + 1 :],
                raw_global_args=tuple(raw_global_args),
            )
        if not arg.startswith("-"):
            break

        if arg == "-i":
            raw_global_args.append(arg)
            iterative = True
            i += 1
            continue
        if arg in ("-x", "-xx"):
            raw_global_args.append(arg)
            trace_x = True
            if arg == "-xx":
                trace_xx = True
            i += 1
            continue
        if arg in ("--no-build", "--build"):
            raw_global_args.append(arg)
            no_build = arg == "--no-build"
            force_build = arg == "--build"
            i += 1
            continue

        key, eq, inline_val = arg.partition("=")
        if key in _GLOBAL_VALUE_FLAGS:
            if eq:
                val = inline_val
                raw_global_args.append(arg)
                i += 1
            else:
                if i + 1 >= n:
                    raise _missing_value_error(key)
                val = args[i + 1]
                raw_global_args.extend((arg, val))
                i += 2

            if key == "--dir":
                if "out/" not in val and val != "out":
                    raise GlobalArgError(
                        "ERROR: Build directory (--dir BUILD_DIR) must be a subdirectory of $FUCHSIA_DIR/out.\n"
                        "Examples: fx --dir out/<folder> ..., fx --dir ~/fuchsia/out/<folder> ..."
                    )
                build_dir = val
            elif key in ("-t", "--target"):
                if not is_valid_device(val):
                    raise GlobalArgError(
                        f"ERROR: Invalid device: {val}. See valid values in 'fx help set-device'"
                    )
                target_device = val
            elif key in ("--enable", "--disable"):
                if valid_optional_features is None:
                    valid_optional_features = frozenset(
                        feat.name
                        for feat in fx_builtins.load_optional_features_meta(
                            fuchsia_dir
                        )
                    )
                if val not in valid_optional_features:
                    valid_str = " ".join(sorted(valid_optional_features))
                    raise GlobalArgError(
                        f'ERROR: Unknown feature "{val}". Valid values are: {valid_str}'
                    )
                feature_flags.append(
                    ("enable" if key == "--enable" else "disable", val)
                )
            elif key == "--invoker":
                invoker = val
            continue

        raise GlobalArgError(
            f"ERROR: Unknown global argument {arg}", show_usage=True
        )

    if i >= n:
        raise GlobalArgError("ERROR: Missing command name", show_usage=True)

    return GlobalArgs(
        build_dir=build_dir,
        target_device=target_device,
        iterative=iterative,
        trace_x=trace_x,
        trace_xx=trace_xx,
        no_build=no_build,
        force_build=force_build,
        feature_flags=tuple(feature_flags),
        invoker=invoker,
        command_name=args[i],
        command_args=args[i + 1 :],
        raw_global_args=tuple(raw_global_args),
    )


def exec_legacy_fx(fuchsia_dir: pathlib.Path, argv: Sequence[str]) -> int:
    """Replace this process with //scripts/fx in legacy fallback mode, passing argv through unchanged.

    This is the normal path for most fx commands, not an error path: main() only
    handles Python built-ins and commands registered in fx_host_tools.json, and
    hands everything else (legacy tools/devshell scripts, .fx metadata files,
    contrib and vendor scripts, $FUCHSIA_BUILD_DIR/host-tools binaries, unknown
    commands and their suggestions) to the bash dispatcher in //scripts/fx.
    Because this is an exec rather than a subprocess, exit codes, signals, and
    TTY behaviour are identical to invoking scripts/fx directly.
    """
    os.environ["_FX_LEGACY_FALLBACK"] = "1"
    legacy_fx = fuchsia_dir / "scripts" / "fx"
    os.execv(str(legacy_fx), [str(legacy_fx), *argv])
    return 1


def run_fx_reentry(
    fuchsia_dir: pathlib.Path,
    argv: Sequence[str],
    *,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Handle `fx fx-reentry [-f] <cmd> [args...]`."""
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    _clear_pre_resolved_env()
    if not os.environ.get("FX_ENTRY_CMD"):
        print(
            "ERROR: Attempting to use fx-reentry without FX_ENTRY_CMD set. "
            "This could be an entry script defect.",
            file=err,
        )
        return 1
    if not argv:
        print("ERROR: No command or function specified.", file=err)
        return 1

    # If -f (function mode) is requested, delegate to scripts/fx-reentry
    if argv[0] != "-f":
        rc = _dispatch_direct_command(
            fuchsia_dir,
            argv[0],
            argv[1:],
            stdout=out,
            stderr=err,
        )
        if rc is not None:
            return rc
    elif len(argv) == 1:
        print("ERROR: No command or function specified.", file=err)
        return 1

    reentry_script = fuchsia_dir / "scripts" / "fx-reentry"
    if reentry_script.is_file():
        return _run_child_process(
            (str(reentry_script), *argv), stdout=out, stderr=err
        )
    return 1


def main(
    argv: Sequence[str] | None = None,
    *,
    fuchsia_dir: pathlib.Path | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    raw_argv = tuple(sys.argv[1:] if argv is None else argv)

    env_start = os.environ.pop("_FX_START_TIME_MICROS", "")
    _clear_pre_resolved_env()
    try:
        start_time_micros = (
            int(env_start) if env_start else int(time.time() * 1_000_000)
        )
    except ValueError:
        start_time_micros = int(time.time() * 1_000_000)

    if fuchsia_dir is None:
        env_fdir = (
            os.environ.get("_FX_TEST_FUCHSIA_DIR")
            or os.environ.get("FUCHSIA_DIR")
            or ""
        )
        if env_fdir:
            fuchsia_dir = pathlib.Path(env_fdir).resolve()
        else:
            fuchsia_dir = (
                pathlib.Path(__file__).resolve().parent.parent.parent.parent
            )

    try:
        parsed = parse_global_args(raw_argv, fuchsia_dir=fuchsia_dir)
    except GlobalArgError as exc:
        if exc.show_usage:
            out.write(_SHORT_USAGE)
        msg = str(exc).replace("$FUCHSIA_DIR", str(fuchsia_dir))
        print(msg, file=err)
        return 1

    orig_env = dict(os.environ)
    _apply_global_env(fuchsia_dir, parsed, raw_argv)

    if parsed.command_name == "fx-reentry":
        return run_fx_reentry(
            fuchsia_dir, parsed.command_args, stdout=out, stderr=err
        )

    prior_call_stack = os.environ.get("FUCHSIA_METRICS_CALL_STACK", "")
    if parsed.command_name != "help":
        if not _check_running_on_supported_platform(stderr=err):
            return 1
        if not _check_running_in_agent_sandbox(stderr=err):
            return 1

    metrics_state: metrics.MetricsConfig | None = None
    executed_subcommand = False

    def _on_before_execute() -> None:
        nonlocal executed_subcommand, metrics_state
        executed_subcommand = True
        if parsed.command_name != "help":
            try:
                metrics_state = metrics.init_command_metrics(
                    fuchsia_dir,
                    parsed.command_name,
                    parsed.command_args,
                    stderr=err,
                )
            except (OSError, ValueError):
                metrics_state = None
            os.environ["FUCHSIA_METRICS_CALL_STACK"] = (
                f"{prior_call_stack} fx" if prior_call_stack else "fx"
            )
            if metrics_state is not None:
                with contextlib.suppress(OSError, ValueError):
                    metrics.spawn_command_start_metrics(
                        fuchsia_dir,
                        parsed.command_name,
                        parsed.command_args,
                        metrics_state,
                    )

    def _invoke_direct(
        before_execute: Callable[[], None] | None = None,
        pre_resolved_out: dict[str, str] | None = None,
    ) -> int | None:
        return _dispatch_direct_command(
            fuchsia_dir,
            parsed.command_name,
            parsed.command_args,
            build_dir_override=parsed.build_dir,
            no_build=True if parsed.no_build else None,
            force_build=True if parsed.force_build else None,
            pre_resolved_out=pre_resolved_out,
            stdout=out,
            stderr=err,
            before_execute=before_execute,
        )

    pre_resolved_env: dict[str, str] = {}
    rc = _invoke_direct(
        before_execute=_on_before_execute,
        pre_resolved_out=pre_resolved_env,
    )
    if rc is not None:
        if parsed.iterative and rc != 130 and parsed.command_name != "help":
            display_name = " ".join((parsed.command_name, *parsed.command_args))
            return _run_iterative(
                fuchsia_dir,
                display_name,
                lambda: rc_val
                if (rc_val := _invoke_direct()) is not None
                else 0,
                stdout=out,
            )
        if (
            executed_subcommand
            and parsed.command_name != "help"
            and metrics_state is not None
        ):
            end_time_micros = int(time.time() * 1_000_000)
            with contextlib.suppress(OSError, ValueError):
                metrics.spawn_command_finish_metrics(
                    fuchsia_dir,
                    parsed.command_name,
                    parsed.command_args,
                    start_time_micros,
                    end_time_micros,
                    rc,
                    metrics_state,
                )
        return rc

    # Not a built-in or registered command: hand off to the bash dispatcher.
    os.environ.clear()
    os.environ.update(orig_env)
    os.environ.update(pre_resolved_env)
    os.environ["_FX_START_TIME_MICROS"] = str(start_time_micros)
    return exec_legacy_fx(fuchsia_dir, raw_argv)


if __name__ == "__main__":
    sys.exit(main())
