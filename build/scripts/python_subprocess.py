# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for executing in-tree Python scripts in hermetic subprocesses."""

import os
import pathlib
import subprocess
import sys
from collections.abc import Mapping, Sequence
from typing import Any

# Under fuchsia-vendored-python, sys.executable points to the platform prebuilt Python.
PYTHON_BIN = pathlib.Path(sys.executable)


def _prepend_env_path(
    env: dict[str, str],
    varname: str,
    path_entry: str | pathlib.Path,
) -> None:
    """Prepends a path to an environment variable value such as PATH or PYTHONPATH.

    Ensures that path_entry is at the front of the colon-separated list of paths,
    moving any existing occurrence to the front to avoid duplicates.

    Args:
        env: Environment dictionary to mutate in-place.
        varname: Name of the environment variable (e.g. "PYTHONPATH", "PATH").
        path_entry: Path entry to prepend.
    """
    entry_str = str(path_entry)
    existing = env.get(varname)
    if not existing:
        env[varname] = entry_str
        return

    paths = [p for p in existing.split(os.pathsep) if p and p != entry_str]
    env[varname] = os.pathsep.join([entry_str, *paths])


def _curate_python_env(
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Curates an environment for executing in-tree Python scripts.

    Ensures that PYTHONPATH includes the source_dir (usually the FUCHSIA_DIR root)
    so that first-party absolute imports (such as build.auth and tools.integration.fint)
    resolve hermetically regardless of the ambient environment. Inherits
    PYTHONPYCACHEPREFIX as set by the outer build orchestrator (fx or fint).

    Args:
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.

    Returns:
        A curated environment dictionary.

    Raises:
        ValueError: If source_dir is empty or represents the current directory dot.
    """
    if not source_dir or str(source_dir) in ("", "."):
        raise ValueError("source_dir must be a valid non-empty directory path")

    sub_env = dict(base_env if base_env is not None else os.environ)
    _prepend_env_path(sub_env, "PYTHONPATH", source_dir)
    return sub_env


def cmd_and_env(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    env: Mapping[str, str],
    popen_kwargs: dict[str, Any] | None = None,
) -> tuple[list[str], dict[str, str]]:
    """Prepares command argv and environment for hermetic Python child execution.

    Guarantees PYTHON_BIN and site-package isolation ("-S"), and prepends
    source_dir to PYTHONPATH. Callers must explicitly provide the base env
    mapping to ensure deterministic environment propagation.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR).
        env: Base environment mapping to curate. Required without default to
            prevent implicit reliance on ambient process environment state.
        popen_kwargs: Optional dictionary of keyword arguments for subprocess.

    Returns:
        A tuple of (full_cmd_argv, curated_env_dict).
    """
    bin_str = str(PYTHON_BIN)
    if not cmd:
        full_cmd = [bin_str, "-S"]
    elif cmd[0] == "env":
        full_cmd = list(cmd)
    elif cmd[0] != bin_str:
        full_cmd = [bin_str, "-S", *cmd]
    elif len(cmd) < 2 or cmd[1] != "-S":
        full_cmd = [bin_str, "-S", *cmd[1:]]
    else:
        full_cmd = list(cmd)

    curated_env = _curate_python_env(
        source_dir=source_dir,
        base_env=env,
    )
    if popen_kwargs is not None:
        ext_env = popen_kwargs.pop("env", None)
        if ext_env is not None:
            curated_env.update(ext_env)
        # Guarantee text mode and prevent TypeError if callers pass text kwargs
        popen_kwargs.pop("text", None)
        popen_kwargs.pop("universal_newlines", None)

    return full_cmd, curated_env


_prepare_cmd_and_env = cmd_and_env


def cmd(
    source_dir: pathlib.Path,
    script: pathlib.Path,
    *args: str,
    base_env: Mapping[str, str] | None = None,
) -> list[str]:
    """Constructs a self-contained command with hermetic PYTHONPATH and PYTHON_BIN.

    Returns ['env', f'PYTHONPATH={pythonpath}', PYTHON_BIN, '-S', str(script), *args],
    leveraging cmd_and_env() so the returned argv can be executed directly
    by any subprocess runner or command-chain compositor without requiring
    caller-managed environment setup.

    Args:
        source_dir: Root directory of the source checkout (FUCHSIA_DIR), used to
            curate PYTHONPATH for in-tree first-party imports.
        script: Path to the in-tree Python script to execute.
        *args: Command-line argument string tokens passed to the script.
        base_env: Optional base environment mapping. Defaults to an empty dict
            to guarantee hermetic isolation from ambient process environment paths.

    Returns:
        A list of command tokens prefixed with 'env' and curated 'PYTHONPATH'.
    """
    clean_base = {} if base_env is None else base_env
    full_cmd, env = cmd_and_env(
        [str(script), *args], source_dir, env=clean_base
    )
    pythonpath = env["PYTHONPATH"]
    return ["env", f"PYTHONPATH={pythonpath}", *full_cmd]


def run(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
    **popen_kwargs: Any,
) -> subprocess.CompletedProcess[str]:
    """Runs an in-tree Python script hermetically using PYTHON_BIN.

    Automatically:
    - Prepends PYTHON_BIN and ensures the site-package isolation flag ("-S")
      is present immediately after the interpreter.
    - Injects curated env ensuring PYTHONPATH includes source_dir.
    - Sets text=True.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.
        **popen_kwargs: Additional keyword arguments forwarded to subprocess.run.

    Returns:
        A subprocess.CompletedProcess instance containing execution results.

    Raises:
        ValueError: If source_dir is empty.
        subprocess.CalledProcessError: If check=True is passed and the command fails.
    """
    base = os.environ if base_env is None else base_env
    full_cmd, env = cmd_and_env(
        cmd=cmd,
        source_dir=source_dir,
        env=base,
        popen_kwargs=popen_kwargs,
    )
    return subprocess.run(
        full_cmd,
        text=True,
        env=env,
        **popen_kwargs,
    )


def check_output(
    cmd: Sequence[str],
    source_dir: pathlib.Path,
    base_env: Mapping[str, str] | None = None,
    **popen_kwargs: Any,
) -> str:
    """Invokes an in-tree Python script and returns its stripped stdout string.

    Ensures that PYTHONPATH includes source_dir to resolve in-tree first-party absolute imports.

    Args:
        cmd: Sequence of command string tokens to execute.
        source_dir: Root of the source tree (FUCHSIA_DIR). Must not be empty.
        base_env: Optional base environment mapping; defaults to os.environ.
        **popen_kwargs: Additional keyword arguments forwarded to run/subprocess.
            Note: stdout is set to subprocess.PIPE and check is set to True.

    Returns:
        The stripped stdout string of the completed process.

    Raises:
        ValueError: If source_dir is empty.
        subprocess.CalledProcessError: If the process exits with a non-zero exit code.
    """
    completed = run(
        cmd=cmd,
        source_dir=source_dir,
        base_env=base_env,
        stdout=subprocess.PIPE,
        check=True,
        **popen_kwargs,
    )
    return completed.stdout.strip()
