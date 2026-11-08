#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A wrapper script that launches Buildminder and sets up the build environment.

Buildminder is a build monitoring daemon and Terminal UI (TUI) provided by the
rsclient tool suite (under prebuilt/rsclient/$HOST_PLATFORM). It listens on a
local gRPC socket for JobStatus build events streamed by tool-specific proxies:
  * rsproxy (for Ninja actions)
  * besproxy (for Bazel Build Event Streams)

Buildminder operates in two primary modes:
  * Interactive TUI (--tui): Renders a live terminal dashboard of active
    build actions, resource usage, and logs.
  * Headless (--headless, e.g. under --summarize-failures): Monitors build
    progress in the background without terminal UI rendering, writing out
    structured failure reports (unsuccessful_actions.json) and formatted
    summaries (failure_summary.txt) upon completion.

This module is dual-use:
  * Importable: Provides generate_buildminder_prefix() and buildminder_env()
    for direct integration into top_build_wrap.py without subshell hops.
  * Executable: Acts as a standalone CLI wrapper around any arbitrary build
    command.

It also exports FX_INTERNAL_BAZEL_BESPROXY=1 into the child environment so child
Bazel invocations know to enable besproxy, and sets a hermetic PYTHONPATH.
"""

import argparse
import os
import sys
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path

ENV_FX_INTERNAL_BAZEL_BESPROXY = "FX_INTERNAL_BAZEL_BESPROXY"

_DEFAULT_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent

_PREBUILT_RSCLIENT_DIR = Path("prebuilt/rsclient")
_BIN_DIR = Path("bin")

_BUILDMINDER_WRAP_SH = Path("buildminder-wrap.sh")
_BUILDMINDER_BIN = Path("buildminder")


def _get_host_tag() -> str:
    """Returns the host platform-arch tag (e.g. linux-x64, mac-arm64).

    Note: This is copied from `build/bazel/scripts/build_utils.py`'s `get_host_tag()`
    rather than imported. Importing `build_utils` introduces a heavy transitive
    dependency on the Bazel build utility graph into `build.tui` (and transitively
    into `main_build` / `fx_build_test`), which complicates sandbox test fixtures
    and creates cross-subsystem coupling for a simple OS/arch detection utility.
    See b/567721959 regarding migrating devshell build tests to Python to avoid
    manual file dependency tracking.
    """
    platform = (
        "linux"
        if sys.platform.startswith("linux")
        else "mac"
        if sys.platform == "darwin"
        else os.uname().sysname
    )
    arch = os.uname().machine
    if arch == "x86_64":
        arch = "x64"
    elif arch.startswith(("armv8", "aarch64")):
        arch = "arm64"
    return f"{platform}-{arch}"


def _rsclient_bin_dir(fuchsia_dir: Path, host_tag: str | None = None) -> Path:
    """Returns the prebuilt rsclient bin directory for the given host tag."""
    tag = host_tag or _get_host_tag()
    return fuchsia_dir / _PREBUILT_RSCLIENT_DIR / tag / _BIN_DIR


def generate_buildminder_prefix(
    fuchsia_dir: Path,
    log_dir: Path | None = None,
    buildminder_options: Sequence[str] = (),
    verbose: bool = False,
) -> Iterable[str]:
    """Yields arguments for the buildminder-wrap.sh command prefix."""
    bin_dir = _rsclient_bin_dir(fuchsia_dir)
    buildminder_wrap = bin_dir / _BUILDMINDER_WRAP_SH
    buildminder = bin_dir / _BUILDMINDER_BIN

    yield str(buildminder_wrap)
    yield "--buildminder"
    yield str(buildminder)
    yield "--mode"
    yield "integrated"

    if verbose:
        yield "--verbose"

    if log_dir:
        yield "--log-dir"
        yield str(log_dir)

    if buildminder_options:
        yield "--buildminder_options"
        yield from buildminder_options

    yield "--"


def _compose_buildminder_command(
    command: Sequence[str],
    fuchsia_dir: Path,
    log_dir: Path | None = None,
    buildminder_options: Sequence[str] = (),
    verbose: bool = False,
) -> list[str]:
    prefix = list(
        generate_buildminder_prefix(
            fuchsia_dir=fuchsia_dir,
            log_dir=log_dir,
            buildminder_options=buildminder_options,
            verbose=verbose,
        )
    )
    return prefix + list(command)


def buildminder_env(
    base_env: Mapping[str, str],
    fuchsia_dir: Path,
    python_dir: Path,
) -> dict[str, str]:
    """Prepares the environment for running under buildminder.

    Args:
        base_env: Base environment mapping to extend.
        fuchsia_dir: Path to the Fuchsia checkout root.
        python_dir: Path to the prebuilt Python directory to prepend to PATH.

    Returns:
        A new environment dictionary configured for Buildminder execution.
    """
    env = dict(base_env)
    env[ENV_FX_INTERNAL_BAZEL_BESPROXY] = "1"

    # Prepend fuchsia_dir to PYTHONPATH so child Python processes can import
    # build tools hermetically without breaking any existing PYTHONPATH entries.
    current_pp = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        f"{fuchsia_dir}:{current_pp}" if current_pp else str(fuchsia_dir)
    )

    # Prebuilt buildminder-wrap.sh uses python3 as an alternative sleep
    # implementation supporting fractional seconds if system sleep is not present
    # or lacks subsecond precision. Ensure the prebuilt python directory is in PATH.
    current_path = env.get("PATH", "")
    env["PATH"] = (
        f"{python_dir}:{current_path}" if current_path else str(python_dir)
    )

    return env


def _parse_args(
    argv: Sequence[str],
) -> tuple[argparse.Namespace, list[str], list[str]]:
    parser = argparse.ArgumentParser(
        description=__doc__,
        usage="%(prog)s [options] -- command [args...]",
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=Path,
        default=_DEFAULT_FUCHSIA_DIR,
        help="Path to the Fuchsia checkout root.",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        default=None,
        help="Directory where buildminder logs should be saved.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print verbose diagnostics.",
    )

    args_list = list(argv)
    if "--" in args_list:
        sep_idx = args_list.index("--")
        opt_args = args_list[:sep_idx]
        wrapped_command = args_list[sep_idx + 1 :]
    else:
        opt_args = args_list
        wrapped_command = []

    parsed_args, extra_bm_options = parser.parse_known_args(opt_args)
    if not wrapped_command:
        parser.error("A command must be specified after '--'.")

    return parsed_args, extra_bm_options, wrapped_command


def main(argv: Sequence[str]) -> int:
    args, bm_options, command = _parse_args(argv)
    final_cmd = _compose_buildminder_command(
        command=command,
        fuchsia_dir=args.fuchsia_dir,
        log_dir=args.log_dir,
        buildminder_options=bm_options,
        verbose=args.verbose,
    )
    if args.verbose:
        print(
            f"[{Path(__file__).name}] full command: {' '.join(final_cmd)}",
            file=sys.stderr,
        )
    child_env = buildminder_env(
        base_env=os.environ,
        fuchsia_dir=args.fuchsia_dir,
        python_dir=Path(sys.executable).parent,
    )
    os.execvpe(final_cmd[0], final_cmd, child_env)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
