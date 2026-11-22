#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia-specific wrapper for system-wide profile collection."""

import argparse
import os
import subprocess
import sys
import time
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import TextIO

from build.scripts import signal_utils

_SCRIPT_PATH = Path(__file__).resolve()
_SCRIPT_NAME = _SCRIPT_PATH.name
_SCRIPT_DIR = _SCRIPT_PATH.parent
_DEFAULT_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent

LOGS_SUBDIR: str = "build_profile"
SYSTEM_PROFILE_JSON_NAME: str = "system_profile.json"
HARDWARE_PROFILE_JSON_NAME: str = "hardware_profile.json"
HARDWARE_PROFILE_CACHE_JSON_NAME: str = "hardware_profile.cache.json"

_SYSTEM_PROFILER_SCRIPT = _SCRIPT_DIR / "system_profiler.py"


def _ts_msg(text: str, file: TextIO = sys.stderr) -> None:
    """Prints a message prefixed with the epoch timestamp and script name."""
    print(f"[@{time.time():.6f}] [{_SCRIPT_NAME}] {text}", file=file)


def profile_artifacts(log_dir: Path) -> dict[str, Path]:
    """Returns standard artifact paths produced in a build_profile log directory."""
    return {
        "system_profile": log_dir / SYSTEM_PROFILE_JSON_NAME,
        "hardware_profile": log_dir / HARDWARE_PROFILE_JSON_NAME,
    }


def generate_profile_prefix(
    system_log: Path,
    interval: int | float = 1,
    verbose: bool = False,
    wrapper_path: Path | None = None,
) -> Iterable[str]:
    """Yields command tokens for the profile wrapper prefix up to and including '--'."""
    wrap_bin = wrapper_path or _SCRIPT_PATH
    yield str(wrap_bin)
    yield "--system-log"
    yield str(system_log)
    if interval != 1:
        yield "-n"
        yield str(interval)
    if verbose:
        yield "--verbose"
    yield "--"


def _parse_args(
    argv: Sequence[str],
) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(
        description=__doc__,
        usage="%(prog)s [options] -- command [args...]",
    )
    parser.add_argument(
        "--system-log",
        type=Path,
        required=True,
        help="Path where system profile data should be saved.",
    )
    parser.add_argument(
        "-n",
        dest="interval",
        type=float,
        default=1.0,
        help="Sampling interval in seconds (default: 1.0).",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print verbose execution details.",
    )

    args_list = list(argv)
    if "--" in args_list:
        sep_idx = args_list.index("--")
        opt_args = args_list[:sep_idx]
        wrapped_command = args_list[sep_idx + 1 :]
    else:
        opt_args = args_list
        wrapped_command = []

    parsed_args = parser.parse_args(opt_args)
    return parsed_args, wrapped_command


def _run_profiled_command(
    command: Sequence[str],
    system_log: Path,
    env: Mapping[str, str],
    interval: float = 1.0,
    verbose: bool = False,
) -> int:
    """Runs a command while collecting background system metrics."""
    system_log.unlink(missing_ok=True)
    system_log.parent.mkdir(parents=True, exist_ok=True)

    profiler_proc: subprocess.Popen[bytes] | None = None
    if _SYSTEM_PROFILER_SCRIPT.is_file():
        py_bin = sys.executable
        profiler_cmd = [
            py_bin,
            "-S",
            "-u",
            str(_SYSTEM_PROFILER_SCRIPT),
            "--interval",
            str(interval),
            "--output",
            str(system_log),
            "--pid",
            str(os.getpid()),
            "--metadata",
            f"FX_BUILD_UUID:{env.get('FX_BUILD_UUID', '')}",
        ]

        if verbose:
            _ts_msg("Starting background profile collection...")

        profiler_proc = subprocess.Popen(profiler_cmd)
    elif verbose:
        _ts_msg(
            f"Warning: {_SYSTEM_PROFILER_SCRIPT} not found; skipping background profile collection."
        )

    try:
        return signal_utils.run_command(
            command,
            env=dict(env),
            separate_pgrp=True,
            verbose=verbose,
        )
    except signal_utils.BuildInterruptedError as err:
        return err.return_code
    finally:
        if profiler_proc is not None and profiler_proc.poll() is None:
            if verbose:
                _ts_msg("Shutting down system profiler...")
            profiler_proc.terminate()
            try:
                profiler_proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                profiler_proc.kill()
                profiler_proc.wait()


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    args, wrapped_command = _parse_args(list(argv))

    if not wrapped_command:
        sys.stderr.write(
            f"{_SCRIPT_NAME}: error: A command must be specified after '--'.\n"
        )
        return 2

    return _run_profiled_command(
        command=wrapped_command,
        system_log=args.system_log,
        env=env,
        interval=args.interval,
        verbose=args.verbose,
    )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
