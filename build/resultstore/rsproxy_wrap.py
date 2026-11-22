#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia-specific wrapper for rsclient's rsproxy tool."""

import argparse
import dataclasses
import os
import sys
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path

_SCRIPT_PATH = Path(__file__).resolve()
_SCRIPT_NAME = _SCRIPT_PATH.name
_SCRIPT_DIR = _SCRIPT_PATH.parent
_DEFAULT_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent

from build.auth import auth_env
from build.resultstore import rs_profile, rsclient

LOGS_SUBDIR: str = "rsproxy_logs"
INFO_LOG_NAME: str = "rsproxy.INFO"
WARNING_LOG_NAME: str = "rsproxy.WARNING"

# Environment variables that directly influence rsclient's rsproxy tool.
# These names are defined by the upstream rsclient project and directly
# configure rsproxy daemon behavior and endpoints.
_ENV_LOG_DIR = "RS_log_dir"
_ENV_RS_SERVICE = "RS_rs_service"
_ENV_CAS_SERVICE = "RS_cas_service"
_ENV_USE_APPLICATION_DEFAULT_CREDENTIALS = (
    "RS_use_application_default_credentials"
)
_ENV_USE_GCE_CREDENTIALS = "RS_use_gce_credentials"
_ENV_EXPERIMENTAL_CREDENTIALS_HELPER = "RS_experimental_credentials_helper"

_GCERTAUTH_CFG = _SCRIPT_DIR / "fuchsia-resultstore-gcertauth.cfg"
_DEFAULT_CFG = _SCRIPT_DIR / "fuchsia-resultstore.cfg"


def rsproxy_artifacts(log_dir: Path) -> dict[str, Path]:
    """Returns standard artifact paths produced in an rsproxy log directory."""
    return {
        "log_dir": log_dir,
    }


def _get_host_tag() -> str:
    """Returns the host platform-arch tag (e.g. linux-x64, mac-arm64)."""
    platform = (
        "linux"
        if sys.platform.startswith("linux")
        else "mac"
        if sys.platform == "darwin"
        else os.uname().sysname.lower()
    )
    arch = os.uname().machine
    if arch == "x86_64":
        arch = "x64"
    elif arch.startswith(("armv8", "aarch64")):
        arch = "arm64"
    return f"{platform}-{arch}"


def _is_resultstore_enabled(env: Mapping[str, str]) -> bool:
    """Determines whether ResultStore (rsproxy) is enabled for Ninja builds."""
    return env.get(rs_profile.ENV_RESULTSTORE_NINJA, "0") == "1" or bool(
        env.get(_ENV_RS_SERVICE)
    )


def _extract_subbuild_base(command: Sequence[str]) -> str:
    """Extracts subbuild directory base name from -C ninja flags, or 'top'."""
    subbuild_dir: str | None = None
    prev_opt = False
    for arg in command:
        if prev_opt:
            subbuild_dir = arg
            prev_opt = False
        elif arg == "-C":
            prev_opt = True
        elif arg.startswith("-C"):
            subbuild_dir = arg[2:]

    if subbuild_dir:
        return Path(subbuild_dir).name
    return "top"


@dataclasses.dataclass(frozen=True)
class _WrappedCommand:
    """Encapsulates a composed command and its execution environment."""

    command: list[str]
    env: dict[str, str] = dataclasses.field(default_factory=dict)


def _resolve_auth_type(
    auth_type: str,
    use_machine_credentials: bool,
    env: Mapping[str, str],
    fuchsia_dir: Path,
) -> str:
    """Resolves the effective auth type for rsproxy."""
    return auth_env.resolve_build_auth_type(
        auth_type=auth_type,
        use_machine_credentials=use_machine_credentials,
        env=env,
        fuchsia_dir=fuchsia_dir,
        service_env_var=auth_env.ENV_RS_AUTH_TYPE,
    )


def generate_rsproxy_prefix(
    fuchsia_dir: Path,
    env: Mapping[str, str],
    log_dir: Path | None = None,
    auth_type: str = auth_env.AUTO,
    use_machine_credentials: bool = False,
    override_proxy_options: Sequence[str] = (),
    pre_build_uploads: Sequence[Path | str] = (),
    post_build_uploads: Sequence[Path | str] = (),
    host_tag: str | None = None,
    verbose: bool = False,
) -> Iterable[str]:
    """Yields command tokens for the rsproxy wrapper prefix up to and including '--'."""
    tag = host_tag or _get_host_tag()
    bin_dir = rsclient.bin_dir(fuchsia_dir, tag)
    proxy_wrap = bin_dir / rsclient.RSPROXY_WRAP_SH
    rsproxy_bin = bin_dir / rsclient.RSPROXY_BIN

    resolved_auth = _resolve_auth_type(
        auth_type=auth_type,
        use_machine_credentials=use_machine_credentials,
        env=env,
        fuchsia_dir=fuchsia_dir,
    )

    yield str(proxy_wrap)
    yield "--rsproxy"
    yield str(rsproxy_bin)

    if log_dir is not None:
        yield "--log-dir"
        yield str(log_dir)

    rsproxy_options = ["--project_id=fuchsia"]

    if resolved_auth == auth_env.LOAS:
        credshelper = auth_env.reclient_credshelper_path(fuchsia_dir, tag)
        rsproxy_options.extend(
            [
                f"--cfg={_GCERTAUTH_CFG}",
                f"--credentials_helper={credshelper}",
            ]
        )
    elif resolved_auth == auth_env.OAUTH:
        rsproxy_options.append(f"--cfg={_DEFAULT_CFG}")

    # When service endpoints point to unix sockets, TLS assumes "localhost", which certs reject.
    rs_service = env.get(_ENV_RS_SERVICE, "")
    if rs_service.startswith("unix://"):
        rsproxy_options.append(
            "--rs_tls_server_name=resultstore.googleapis.com"
        )
    cas_service = env.get(_ENV_CAS_SERVICE, "")
    if cas_service.startswith("unix://"):
        rsproxy_options.append(
            "--cas_tls_server_name=remotebuildexecution.googleapis.com"
        )

    for upload in pre_build_uploads:
        rsproxy_options.extend(["--pre_build_uploads", str(upload)])
    for upload in post_build_uploads:
        rsproxy_options.extend(["--post_build_uploads", str(upload)])

    yield "--rsproxy_options"
    yield from rsproxy_options
    yield from override_proxy_options
    yield "--"


def _rsproxy_env(
    base_env: Mapping[str, str],
    log_dir: Path | None = None,
    auth_type: str = auth_env.AUTO,
    use_machine_credentials: bool = False,
    python_dir: Path | None = None,
) -> dict[str, str]:
    """Curates environment variables needed for rsproxy execution."""
    env = dict(base_env)

    if use_machine_credentials or auth_type == auth_env.MACHINE:
        env[_ENV_USE_APPLICATION_DEFAULT_CREDENTIALS] = "false"
        env[_ENV_USE_GCE_CREDENTIALS] = "true"
        env[_ENV_EXPERIMENTAL_CREDENTIALS_HELPER] = ""

    if log_dir is not None:
        env[_ENV_LOG_DIR] = str(log_dir)

    # Workaround: avoid DNS lookup of "localhost" for GCE metadata host.
    gce_meta_host = env.get("GCE_METADATA_HOST")
    if gce_meta_host:
        env["GCE_METADATA_HOST"] = gce_meta_host.replace(
            "localhost", "127.0.0.1"
        )

    # Ensure python3 bindir is in PATH for mkfifo and sleep in rsproxy-wrap.sh.
    py_bindir = str(python_dir or Path(sys.executable).parent)
    current_path = env.get("PATH", "")
    env["PATH"] = f"{py_bindir}:{current_path}" if current_path else py_bindir

    return env


def _compose_rsproxy_command(
    command: Sequence[str],
    fuchsia_dir: Path,
    env: Mapping[str, str],
    log_dir: Path | None = None,
    auth_type: str = auth_env.AUTO,
    use_machine_credentials: bool = False,
    override_proxy_options: Sequence[str] = (),
    pre_build_uploads: Sequence[Path | str] = (),
    post_build_uploads: Sequence[Path | str] = (),
    host_tag: str | None = None,
) -> _WrappedCommand:
    """Composes the rsproxy wrapper command line and environment."""
    child_env = dict(env)

    # If ResultStore is disabled, run the original command directly without rsproxy.
    if not _is_resultstore_enabled(env):
        return _WrappedCommand(command=list(command), env=child_env)

    resolved_auth = _resolve_auth_type(
        auth_type=auth_type,
        use_machine_credentials=use_machine_credentials,
        env=env,
        fuchsia_dir=fuchsia_dir,
    )

    # Setup log dir and subbuild nesting if needed.
    subbuild_base = _extract_subbuild_base(command)
    effective_log_dir = log_dir
    if effective_log_dir is None and _ENV_LOG_DIR in env:
        effective_log_dir = Path(env[_ENV_LOG_DIR]) / subbuild_base

    if effective_log_dir is not None:
        effective_log_dir.mkdir(parents=True, exist_ok=True)

    prefix = list(
        generate_rsproxy_prefix(
            fuchsia_dir=fuchsia_dir,
            log_dir=effective_log_dir,
            auth_type=resolved_auth,
            use_machine_credentials=use_machine_credentials,
            override_proxy_options=override_proxy_options,
            pre_build_uploads=pre_build_uploads,
            post_build_uploads=post_build_uploads,
            env=env,
            host_tag=host_tag,
        )
    )

    curated_env = _rsproxy_env(
        base_env=child_env,
        log_dir=effective_log_dir,
        auth_type=resolved_auth,
        use_machine_credentials=use_machine_credentials,
    )

    return _WrappedCommand(command=prefix + list(command), env=curated_env)


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
        help="Directory where rsproxy logs should be saved.",
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
        "-v",
        "--verbose",
        action="store_true",
        help="Print verbose execution details.",
    )
    parser.add_argument(
        "--pre_build_uploads",
        action="append",
        default=[],
        help="Files to upload prior to build execution.",
    )
    parser.add_argument(
        "--post_build_uploads",
        action="append",
        default=[],
        help="Files to upload after build completion.",
    )

    args_list = list(argv)
    if "--" in args_list:
        sep_idx = args_list.index("--")
        opt_args = args_list[:sep_idx]
        wrapped_command = args_list[sep_idx + 1 :]
    else:
        opt_args = args_list
        wrapped_command = []

    parsed_args, extra_proxy_opts = parser.parse_known_args(opt_args)
    return parsed_args, extra_proxy_opts, wrapped_command


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    args, extra_proxy_opts, wrapped_command = _parse_args(list(argv))

    if not wrapped_command:
        sys.stderr.write(
            f"{_SCRIPT_NAME}: error: A command must be specified after '--'.\n"
        )
        return 2

    wrapped = _compose_rsproxy_command(
        command=wrapped_command,
        fuchsia_dir=args.fuchsia_dir,
        env=env,
        log_dir=args.log_dir,
        auth_type=args.auth_type,
        use_machine_credentials=args.use_machine_credentials,
        override_proxy_options=extra_proxy_opts,
        pre_build_uploads=args.pre_build_uploads,
        post_build_uploads=args.post_build_uploads,
    )

    if args.verbose:
        sys.stderr.write(f"[{_SCRIPT_NAME}] Executing: {wrapped.command}\n")

    os.execvpe(wrapped.command[0], wrapped.command, wrapped.env)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
