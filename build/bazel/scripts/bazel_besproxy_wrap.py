#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A wrapper script that intercepts Bazel invocations with besproxy when Buildminder is active."""

import argparse
import dataclasses
import os
import sys
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path

_DEFAULT_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent.parent

_BESPROXY_LOG_SUBDIR = Path("besproxy")

from build.auth import auth_env
from build.bazel.scripts import bazel_invocation, build_utils
from build.resultstore import rsclient


def should_wrap_with_besproxy(env: Mapping[str, str]) -> bool:
    """Returns True if the environment indicates besproxy should be enabled."""
    return env.get(rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY) not in (
        "0",
        "",
        None,
    )


def _generate_besproxy_prefix(
    fuchsia_dir: Path,
    env: Mapping[str, str],
    log_dir: Path | None = None,
) -> Iterable[str]:
    """Yields command-line arguments for the besproxy wrapper prefix.

    This unconditionally yields the wrapper prefix:
    besproxy-wrap.sh --besproxy <bin> [--log-dir ...] [--besproxy_options ...] -- rs-bazel.sh
    """
    bin_dir = rsclient.bin_dir(fuchsia_dir, build_utils.get_host_tag())
    besproxy_wrap = bin_dir / rsclient.BESPROXY_WRAP_SH
    besproxy_bin = bin_dir / rsclient.BESPROXY_BIN
    rs_bazel = bin_dir / rsclient.RS_BAZEL_SH

    yield str(besproxy_wrap)
    yield "--besproxy"
    yield str(besproxy_bin)

    if log_dir:
        yield "--log-dir"
        yield str(log_dir / _BESPROXY_LOG_SUBDIR)

    besproxy_opts: list[str] = []

    # 1. CAS Backend: Route besproxy's CAS upstream to RBE, optionally through a proxy socket.
    besproxy_opts.append(
        "--cas_backend=remotebuildexecution.googleapis.com:443"
    )
    rbe_socket = env.get(bazel_invocation.ENV_BAZEL_RBE_SOCKET)
    if rbe_socket:
        if not rbe_socket.startswith("unix://"):
            rbe_socket = f"unix://{rbe_socket}"
        besproxy_opts.append(f"--cas_proxy={rbe_socket}")
        besproxy_opts.append(
            "--cas_tls_server_name=remotebuildexecution.googleapis.com"
        )

    # 2. Upstream Authentication: Mirror rsproxy / Bazel authentication.
    resolved_auth = auth_env.resolve_build_auth_type(
        fuchsia_dir=fuchsia_dir,
        env=env,
        service_env_var=auth_env.ENV_RS_AUTH_TYPE,
    )
    if resolved_auth == auth_env.LOAS:
        tag = build_utils.get_host_tag()
        credshelper = auth_env.reclient_credshelper_path(fuchsia_dir, tag)
        if credshelper.is_file():
            besproxy_opts.append(f"--credentials_helper={credshelper}")
            besproxy_opts.append(
                "--credentials_helper_args=--auth_source=automaticAuth --gcert_refresh_timeout=10"
            )
    elif resolved_auth == auth_env.OAUTH:
        besproxy_opts.append("--use_application_default_credentials")
    elif resolved_auth == auth_env.MACHINE:
        besproxy_opts.append("--use_gce_credentials")
    elif resolved_auth == auth_env.NONE:
        besproxy_opts.append("--service_no_auth")

    # 3. Upstream BES: If ResultStore is disabled, operate besproxy in sink mode.
    # Otherwise, forward to buildeventservice (optionally via proxy socket in infra).
    rs_enabled = env.get(bazel_invocation.ENV_RESULTSTORE_BAZEL) not in (
        "0",
        "",
        None,
    )
    if not rs_enabled:
        besproxy_opts.append("--bes_sink")
    else:
        besproxy_opts.append(
            "--bes_backend=buildeventservice.googleapis.com:443"
        )
        bes_socket = env.get(bazel_invocation.ENV_BAZEL_BES_SOCKET) or env.get(
            bazel_invocation.ENV_BAZEL_RESULTSTORE_SOCKET
        )
        if bes_socket:
            if not bes_socket.startswith("unix://"):
                bes_socket = f"unix://{bes_socket}"
            besproxy_opts.append(f"--bes_proxy={bes_socket}")
        besproxy_opts.append(
            "--bes_tls_server_name=buildeventservice.googleapis.com"
        )

    if besproxy_opts:
        yield "--besproxy_options"
        yield from besproxy_opts

    # Pass rs-bazel.sh to execute Bazel.
    # rs-bazel.sh intercepts the Bazel invocation and injects --bes_proxy
    # to route the Build Event Stream through besproxy to Buildminder.
    # Native remote execution and caching remain unhindered as besproxy
    # operates solely as a BES proxy with on-demand CAS log retrieval.
    yield "--"
    yield str(rs_bazel)


@dataclasses.dataclass(frozen=True)
class WrappedCommand:
    """Encapsulates a composed command and its execution environment."""

    command: list[str]
    env: dict[str, str] = dataclasses.field(default_factory=dict)


def compose_besproxy_command(
    command: Sequence[str],
    fuchsia_dir: Path,
    env: Mapping[str, str],
    log_dir: Path | None = None,
) -> WrappedCommand:
    """Wraps a Bazel command with besproxy-wrap.sh and rs-bazel.sh if Buildminder is active.

    Args:
        command: The original Bazel command to execute.
        fuchsia_dir: Path to the root Fuchsia checkout.
        env: Environment mapping to inspect for sockets and flags.
        log_dir: Directory where per-invocation logs are stored.

    Returns:
        A WrappedCommand containing the command list and child environment.
    """
    child_env = dict(env)
    # Remove PYTHONPATH from the child environment so Bazel actions are not
    # contaminated by the in-tree Python path used to invoke this wrapper script.
    child_env.pop("PYTHONPATH", None)

    if not should_wrap_with_besproxy(env):
        return WrappedCommand(command=list(command), env=child_env)

    prefix = list(
        _generate_besproxy_prefix(fuchsia_dir, env=env, log_dir=log_dir)
    )
    return WrappedCommand(command=prefix + list(command), env=child_env)


def _parse_args(argv: Sequence[str]) -> tuple[argparse.Namespace, list[str]]:
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
        help="Directory where per-invocation logs should be saved.",
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
    if not wrapped_command:
        parser.error("A command must be specified after '--'.")

    return parsed_args, wrapped_command


def main(argv: Sequence[str]) -> int:
    args, command = _parse_args(argv)
    wrapped = compose_besproxy_command(
        command=command,
        fuchsia_dir=args.fuchsia_dir,
        env=os.environ,
        log_dir=args.log_dir,
    )
    os.execvpe(wrapped.command[0], wrapped.command, wrapped.env)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
