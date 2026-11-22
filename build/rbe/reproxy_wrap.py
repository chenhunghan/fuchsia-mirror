#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia-specific wrapper for reclient's reproxy daemon."""

import argparse
import datetime
import os
import shutil
import subprocess
import sys
import uuid
from collections.abc import Mapping, Sequence
from pathlib import Path

_SCRIPT_PATH = Path(__file__)
_SCRIPT_NAME = _SCRIPT_PATH.name
_SCRIPT_DIR = _SCRIPT_PATH.parent
_DEFAULT_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent
_SCRIPT_RELPATH = _SCRIPT_PATH.relative_to(_DEFAULT_FUCHSIA_DIR)

# Canonical relative path to the reproxy wrapper from repository root.
REPROXY_WRAP: Path = _SCRIPT_RELPATH

# Configuration file written by `fx build-metrics enable`
_BUILD_METRICS_CONFIG: Path = Path(".fx/config/build-metrics")

from build.auth import auth_env
from build.scripts import python_subprocess, signal_utils

LOGS_SUBDIR: str = "reproxy_logs"
TMPDIRS_SUBDIR: str = ".reproxy_tmpdirs"
METRICS_TXT_NAME: str = "rbe_metrics.txt"
RRPL_NAME: str = "reproxy.rrpl"

# Environment variables defined by upstream reclient (reproxy and rewrapper).
# These control daemon lifecycle, IPC socket paths, and authentication.
_ENV_RECLIENT_SERVER_ADDRESS = "RBE_server_address"
_ENV_RECLIENT_LOG_DIR = "RBE_log_dir"
_ENV_RECLIENT_PROXY_LOG_DIR = "RBE_proxy_log_dir"
_ENV_RECLIENT_OUTPUT_DIR = "RBE_output_dir"
_ENV_RECLIENT_CACHE_DIR = "RBE_cache_dir"
_ENV_RECLIENT_INVOCATION_ID = "RBE_invocation_id"
_ENV_RECLIENT_TLS_SERVER_NAME = "RBE_tls_server_name"
_ENV_RECLIENT_USE_APPLICATION_DEFAULT_CREDENTIALS = (
    "RBE_use_application_default_credentials"
)
_ENV_RECLIENT_USE_GCE_CREDENTIALS = "RBE_use_gce_credentials"
_ENV_RECLIENT_CREDENTIALS_HELPER = "RBE_credentials_helper"
_ENV_RECLIENT_XATTR_DIGEST = "RBE_xattr_digest"

_DEFAULT_CFG = _SCRIPT_DIR / "fuchsia-reproxy.cfg"
_GCERTAUTH_CFG = _SCRIPT_DIR / "fuchsia-reproxy-gcertauth.cfg"
_CASFS_SCRIPT = _SCRIPT_DIR / "casfs.py"


def is_active(env: Mapping[str, str]) -> bool:
    """Returns True if the reproxy daemon environment is active."""
    return bool(
        env.get(_ENV_RECLIENT_PROXY_LOG_DIR) and env.get(_ENV_RECLIENT_LOG_DIR)
    )


def reproxy_log_dir(env: Mapping[str, str]) -> Path | None:
    """Returns the reproxy daemon log directory if set in env."""
    val = env.get(_ENV_RECLIENT_PROXY_LOG_DIR)
    return Path(val) if val else None


def rewrapper_log_dir(env: Mapping[str, str]) -> Path | None:
    """Returns the rewrapper log directory if set in env."""
    val = env.get(_ENV_RECLIENT_LOG_DIR)
    return Path(val) if val else None


def relaunch_cmd_and_env(
    script: Path,
    argv: Sequence[str],
    fuchsia_dir: Path,
    env: Mapping[str, str],
    verbose: bool = True,
) -> tuple[list[str], dict[str, str]]:
    """Constructs the command and environment to relaunch a script with reproxy.

    Uses python_subprocess.cmd_and_env to execute reproxy_wrap.py hermetically,
    explicitly propagating the build's configured RBE auth type and credentials.
    """
    reproxy_args: list[str] = []
    if verbose:
        reproxy_args.append("-v")

    rbe_auth = env.get(auth_env.ENV_RBE_AUTH_TYPE)
    if rbe_auth:
        reproxy_args.extend(["--auth-type", rbe_auth])
        if rbe_auth == auth_env.MACHINE.value:
            reproxy_args.append("--use-machine-credentials")

    reproxy_args.append("--")
    reproxy_args.extend([sys.executable, "-S", str(script.resolve()), *argv])

    return python_subprocess.cmd_and_env(
        cmd=[str(fuchsia_dir / REPROXY_WRAP), *reproxy_args],
        source_dir=fuchsia_dir,
        env=env,
    )


def _get_host_tag() -> str:
    """Returns the host platform-arch tag (e.g. linux-x64, mac-arm64)."""
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


def _reclient_bindir(fuchsia_dir: Path, host_tag: str | None = None) -> Path:
    tag = host_tag or _get_host_tag()
    return fuchsia_dir / "prebuilt" / "third_party" / "reclient" / tag


def reproxy_artifacts(log_dir: Path) -> dict[str, Path]:
    """Returns standard artifact paths produced in a reproxy log directory."""
    return {
        "metrics": log_dir / METRICS_TXT_NAME,
        "rrpl": log_dir / RRPL_NAME,
    }


def _reproxy_cache_dir(env: Mapping[str, str]) -> Path:
    """Determines the persistent deps cache directory."""
    home = env.get("HOME")
    if home:
        return Path(home) / ".cache" / "reproxy" / "deps"
    return Path("/tmp/.cache/reproxy/deps")


def _resolve_auth_type(
    auth_type: str,
    use_machine_credentials: bool,
    env: Mapping[str, str],
    fuchsia_dir: Path,
) -> str:
    """Resolves the effective auth type for reproxy."""
    return auth_env.resolve_build_auth_type(
        auth_type=auth_type,
        use_machine_credentials=use_machine_credentials,
        env=env,
        fuchsia_dir=fuchsia_dir,
        service_env_var=auth_env.ENV_RBE_AUTH_TYPE,
    )


def _generate_bootstrap_env(
    server_address: str,
    log_dir: Path,
    tmp_dir: Path,
    cache_dir: Path,
    use_machine_credentials: bool,
    is_unix_socket: bool,
    casfs_virtualize: bool,
    base_env: Mapping[str, str],
) -> dict[str, str]:
    """Assembles the environment variables passed to the reproxy bootstrap command."""
    env = dict(base_env)
    env["TMPDIR"] = str(tmp_dir)
    env[_ENV_RECLIENT_PROXY_LOG_DIR] = str(log_dir)
    env[_ENV_RECLIENT_LOG_DIR] = str(log_dir)
    env[_ENV_RECLIENT_SERVER_ADDRESS] = server_address
    env[_ENV_RECLIENT_OUTPUT_DIR] = str(log_dir)
    env[_ENV_RECLIENT_CACHE_DIR] = str(cache_dir)

    if use_machine_credentials:
        env[_ENV_RECLIENT_USE_APPLICATION_DEFAULT_CREDENTIALS] = "false"
        env[_ENV_RECLIENT_USE_GCE_CREDENTIALS] = "true"
        env[_ENV_RECLIENT_CREDENTIALS_HELPER] = ""

    if is_unix_socket:
        env[
            _ENV_RECLIENT_TLS_SERVER_NAME
        ] = "remotebuildexecution.googleapis.com"

    if casfs_virtualize:
        env[_ENV_RECLIENT_XATTR_DIGEST] = "casfs.rbe_ari_hash"

    return env


def _generate_rewrapper_env(
    server_address: str,
    log_dir: Path,
    build_uuid: str,
    casfs_virtualize: bool,
    base_env: Mapping[str, str],
) -> dict[str, str]:
    """Assembles the environment variables passed to the wrapped command (for rewrapper)."""
    env = dict(base_env)
    rewrapper_log_dir = log_dir / "rewrapper-logs"
    env[_ENV_RECLIENT_SERVER_ADDRESS] = server_address
    env[_ENV_RECLIENT_LOG_DIR] = str(rewrapper_log_dir)
    env[_ENV_RECLIENT_PROXY_LOG_DIR] = str(log_dir)
    env[_ENV_RECLIENT_INVOCATION_ID] = f"user:{build_uuid}"
    env["FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE"] = (
        "1" if casfs_virtualize else "0"
    )
    return env


def _bootstrap_start(
    bootstrap_bin: Path,
    reproxy_bin: Path,
    cfg_path: Path,
    bootstrap_env: Mapping[str, str],
    extra_options: Sequence[str] = (),
    log_dir: Path | None = None,
    verbose: bool = False,
) -> int:
    """Invokes reproxy bootstrap to start the reproxy daemon."""
    cmd = [
        str(bootstrap_bin),
        f"--re_proxy={reproxy_bin}",
        f"--cfg={cfg_path}",
        *extra_options,
    ]
    stdout_file = log_dir / "bootstrap.stdout" if log_dir else None
    if stdout_file:
        with open(stdout_file, "w") as out_f:
            res = subprocess.run(
                cmd,
                env=dict(bootstrap_env),
                stdout=out_f,
                stderr=subprocess.STDOUT,
                check=False,
            )
            return res.returncode
    res = subprocess.run(
        cmd,
        env=dict(bootstrap_env),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
        check=False,
    )
    return res.returncode


def _bootstrap_shutdown(
    bootstrap_bin: Path,
    cfg_path: Path,
    bootstrap_env: Mapping[str, str],
    extra_options: Sequence[str] = (),
    log_dir: Path | None = None,
    verbose: bool = False,
) -> int:
    """Invokes reproxy bootstrap to cleanly stop the reproxy daemon."""
    cmd = [
        str(bootstrap_bin),
        "--shutdown",
        "--fast_log_collection",
        f"--cfg={cfg_path}",
        *extra_options,
    ]
    stdout_file = log_dir / "shutdown.stdout" if log_dir else None
    if stdout_file:
        with open(stdout_file, "w") as out_f:
            res = subprocess.run(
                cmd,
                env=dict(bootstrap_env),
                stdout=out_f,
                stderr=subprocess.STDOUT,
                check=False,
            )
            return res.returncode
    res = subprocess.run(
        cmd,
        env=dict(bootstrap_env),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
        check=False,
    )
    return res.returncode


def _link_deterministic_rrpl(log_dir: Path) -> None:
    """Creates a deterministic symlink reproxy.rrpl -> *.rrpl in log_dir."""
    rrpls = list(log_dir.glob("*.rrpl"))
    if rrpls:
        target = rrpls[0]
        symlink_path = log_dir / RRPL_NAME
        try:
            if symlink_path.is_symlink() or symlink_path.exists():
                symlink_path.unlink()
            symlink_path.symlink_to(target.name)
        except OSError:
            pass


def _maybe_upload_reproxy_logs(
    log_dir: Path,
    bindir: Path,
    build_uuid: str,
    env: Mapping[str, str],
    fuchsia_dir: Path | None = None,
) -> None:
    """Uploads reproxy logs to BigQuery after build completion if developer metrics collection is enabled.

    Metrics collection is configured via `fx build-metrics enable` in
    `.fx/config/build-metrics`, and can be disabled per-run with
    `FX_REMOTE_BUILD_METRICS=0`.
    """
    if env.get("FX_REMOTE_BUILD_METRICS") == "0":
        return

    upload_script = _SCRIPT_DIR / "upload_reproxy_logs.sh"
    if not upload_script.is_file():
        return

    root = fuchsia_dir or _DEFAULT_FUCHSIA_DIR
    metrics_config = root / _BUILD_METRICS_CONFIG
    try:
        config_lines = metrics_config.read_text().splitlines()
    except OSError:
        return
    if "BUILD_METRICS_ENABLED=1" not in config_lines:
        return

    try:
        cloud_project = "fuchsia-engprod-metrics-prod"
        dataset = "metrics"
        subprocess.run(
            [
                str(upload_script),
                f"--reclient-bindir={bindir}",
                f"--uuid={build_uuid}",
                f"--bq-logs-table={cloud_project}:{dataset}.rbe_client_command_logs_developer_raw",
                f"--bq-metrics-table={cloud_project}:{dataset}.rbe_client_metrics_developer_raw",
                str(log_dir),
            ],
            check=False,
        )
    except OSError as err:
        sys.stderr.write(
            f"[{_SCRIPT_NAME}] Warning: Failed to invoke metrics upload script: {err}\n"
        )


def _parse_args(
    argv: Sequence[str],
) -> tuple[argparse.Namespace, list[str], list[str]]:
    parser = argparse.ArgumentParser(
        description=__doc__,
        usage="%(prog)s [options] -- command [args...]",
    )
    parser.add_argument(
        "--cfg",
        action="append",
        type=Path,
        default=[],
        help="Reclient configs for reproxy [repeatable].",
    )
    parser.add_argument(
        "--bindir",
        type=Path,
        default=None,
        help="Location of reclient binaries.",
    )
    parser.add_argument(
        "--logdir",
        type=Path,
        default=None,
        help="Reproxy log directory.",
    )
    parser.add_argument(
        "--tmpdir",
        type=Path,
        default=None,
        help="Reproxy temp directory.",
    )
    parser.add_argument(
        "--auth-type",
        type=str,
        default=auth_env.AUTO,
        help="Authentication method (loas, oauth, machine, none, auto).",
    )
    parser.add_argument(
        "--use-machine-credentials",
        action="store_true",
        help="Use GCE machine credentials unconditionally.",
    )
    parser.add_argument(
        "--signal-policy",
        type=str,
        default="relay",
        choices=["relay", "relay-group", "passive"],
        help="Signal propagation policy.",
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

    parsed_args, extra_bootstrap_opts = parser.parse_known_args(opt_args)
    return parsed_args, extra_bootstrap_opts, wrapped_command


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    args, extra_bootstrap_opts, wrapped_command = _parse_args(list(argv))

    if not wrapped_command:
        sys.stderr.write(
            f"{_SCRIPT_NAME}: error: A command must be specified after '--'.\n"
        )
        return 2

    fuchsia_dir = _DEFAULT_FUCHSIA_DIR
    bindir = args.bindir or _reclient_bindir(fuchsia_dir)
    bootstrap_bin = bindir / "bootstrap"
    reproxy_bin = bindir / "reproxy"

    auth_type = _resolve_auth_type(
        auth_type=args.auth_type,
        use_machine_credentials=args.use_machine_credentials,
        env=env,
        fuchsia_dir=fuchsia_dir,
    )

    now_str = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    log_dir = (
        args.logdir
        or fuchsia_dir / "out/_unknown/.reproxy_logs" / f"reproxy.{now_str}"
    )
    log_dir.mkdir(parents=True, exist_ok=True)
    (log_dir / "rewrapper-logs").mkdir(parents=True, exist_ok=True)

    tmp_dir = (
        args.tmpdir
        or fuchsia_dir / "out/_unknown/.reproxy_tmpdirs" / log_dir.name
    )
    tmp_dir.mkdir(parents=True, exist_ok=True)

    cache_dir = _reproxy_cache_dir(env)
    cache_dir.mkdir(parents=True, exist_ok=True)

    # Unix domain socket paths are limited to ~104-108 characters on Unix systems.
    # If the default timestamped path exceeds the limit, fall back to a shorter path.
    socket_path = f"/tmp/reproxy.{os.getpid()}.{now_str}.sock"
    if len(socket_path.encode()) > 104:
        socket_path = f"/tmp/r.{os.getpid()}.sock"
    server_address = f"unix://{socket_path}"

    cfg_files = list(args.cfg) or [_DEFAULT_CFG]
    if auth_type == auth_env.LOAS and _GCERTAUTH_CFG not in cfg_files:
        cfg_files.append(_GCERTAUTH_CFG)

    if len(cfg_files) > 1:
        joined_cfg = log_dir / "joined_reproxy.cfg"
        with open(joined_cfg, "w") as out_f:
            for c in cfg_files:
                out_f.write(c.read_text())
                out_f.write("\n")
        bootstrap_cfg = joined_cfg
    else:
        bootstrap_cfg = cfg_files[0]

    is_unix_socket = env.get("RBE_service", "").startswith("unix://") or any(
        opt.startswith("--service=unix://") for opt in extra_bootstrap_opts
    )

    casfs_virtualize = env.get("FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE") == "1"
    use_machine_credentials = (
        args.use_machine_credentials or auth_type == auth_env.MACHINE.value
    )

    boot_env = _generate_bootstrap_env(
        server_address=server_address,
        log_dir=log_dir,
        tmp_dir=tmp_dir,
        cache_dir=cache_dir,
        use_machine_credentials=use_machine_credentials,
        is_unix_socket=is_unix_socket,
        casfs_virtualize=casfs_virtualize,
        base_env=env,
    )

    boot_status = _bootstrap_start(
        bootstrap_bin=bootstrap_bin,
        reproxy_bin=reproxy_bin,
        cfg_path=bootstrap_cfg,
        bootstrap_env=boot_env,
        extra_options=extra_bootstrap_opts,
        log_dir=log_dir,
        verbose=args.verbose,
    )
    if boot_status != 0:
        stdout_file = log_dir / "bootstrap.stdout"
        if stdout_file.exists():
            sys.stderr.write(stdout_file.read_text())
        return boot_status

    build_uuid = env.get("FX_BUILD_UUID") or str(uuid.uuid4())
    rewrapper_env = _generate_rewrapper_env(
        server_address=server_address,
        log_dir=log_dir,
        build_uuid=build_uuid,
        casfs_virtualize=casfs_virtualize,
        base_env=env,
    )

    separate_pgrp = args.signal_policy in ("relay", "relay-group")
    exit_code = 1
    try:
        exit_code = signal_utils.run_command(
            wrapped_command,
            env=rewrapper_env,
            separate_pgrp=separate_pgrp,
            verbose=args.verbose,
        )
    except signal_utils.BuildInterruptedError as err:
        exit_code = err.return_code
    finally:
        # Forward extra_options (e.g. --async_reproxy_termination) to shutdown as well.
        _bootstrap_shutdown(
            bootstrap_bin=bootstrap_bin,
            cfg_path=bootstrap_cfg,
            bootstrap_env=boot_env,
            extra_options=extra_bootstrap_opts,
            log_dir=log_dir,
            verbose=args.verbose,
        )
        _link_deterministic_rrpl(log_dir)
        shutil.rmtree(tmp_dir, ignore_errors=True)

        if env.get("FX_BUILD_RBE_STATS") == "1":
            try:
                from build.rbe import build_summary

                build_summary.print_build_summary(log_dir)
            except Exception as err:
                if args.verbose:
                    sys.stderr.write(
                        f"[{_SCRIPT_NAME}] Warning: Failed to generate build summary: {err}\n"
                    )

        _maybe_upload_reproxy_logs(
            log_dir=log_dir,
            bindir=bindir,
            build_uuid=build_uuid,
            env=env,
            fuchsia_dir=fuchsia_dir,
        )

    return exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
