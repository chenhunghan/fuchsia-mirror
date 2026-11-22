#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""End-to-end authentication and profile validation for reclient RBE."""

import argparse
import dataclasses
import json
import os
import pathlib
import subprocess
import sys
import tempfile
from collections.abc import Mapping, Sequence

from build.auth import auth_env
from build.rbe import casfs, rbe_profile
from build.scripts import python_subprocess

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parent.parent.parent
_REPROXY_WRAP_RELPATH = pathlib.Path("build/rbe/fuchsia-reproxy-wrap.sh")
_REMOTE_ACTION_RELPATH = pathlib.Path("build/rbe/remote_action.py")
_SELF_RELPATH = pathlib.Path("build/rbe/reclient_e2e.py")

_WORKER_POOLS: tuple[str, ...] = ("default", "large")
_MAX_ATTEMPTS = 3

_UNSET_ENV_VARS: frozenset[str] = frozenset(
    {
        "FX_BUILD_RBE_STATS",
        casfs.ENV_CASFS_VIRTUALIZE,
    }
)

_GREEN = "\033[92m"
_RED_BOLD = "\033[1;91m"
_RESET = "\033[0m"


@dataclasses.dataclass(frozen=True)
class PoolProbe:
    """Observed remote execution working directory for a worker pool."""

    pool: str
    remote_working_dir: str


class ReclientE2eError(Exception):
    """Raised when end-to-end reclient RBE verification fails."""


class RemoteExecRootMismatchError(ReclientE2eError):
    """Raised when a worker's working directory mismatches RbeProfile."""


def _format_green(text: str) -> str:
    if not sys.stdout.isatty():
        return text
    return f"{_GREEN}{text}{_RESET}"


def _print_error(message: str) -> None:
    prefix = f"{_RED_BOLD}ERROR:{_RESET}" if sys.stderr.isatty() else "ERROR:"
    print(f"{prefix} {message}", file=sys.stderr)


def _build_reproxy_env(
    base_env: Mapping[str, str],
    profile: rbe_profile.RbeProfile,
) -> dict[str, str]:
    merged = {
        k: v for k, v in base_env.items() if k not in _UNSET_ENV_VARS
    } | profile.to_env_dict()
    merged["FX_REMOTE_BUILD_METRICS"] = "0"
    return merged


def _refresh_auth(
    fuchsia_dir: pathlib.Path,
    active_env: dict[str, str],
    quota_project: str,
    gcloud_args: Sequence[str],
) -> None:
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, env=active_env
        )
        out_dir = fuchsia_dir / "out"
        auth_env.ensure_credentials(
            resolved_auth,
            out_dir=out_dir,
            quota_project=quota_project,
            gcloud_args=gcloud_args,
            env=active_env,
        )
        creds_path = auth_env.resolve_credentials_file(
            resolved_auth,
            auth_env.NONE,
            out_dir=out_dir.resolve(),
            rbe_quota_project=quota_project,
            env=active_env,
        )
    except (RuntimeError, ValueError) as err:
        raise ReclientE2eError("Failed to authenticate for reclient.") from err

    if creds_path is not None:
        active_env[auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS] = str(
            creds_path
        )
    else:
        active_env.pop(auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS, None)


def _run_pool_probes(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
    probe_output: pathlib.Path | None = None,
) -> int:
    remote_action_script = fuchsia_dir / _REMOTE_ACTION_RELPATH
    observed_roots: dict[str, str] = {}
    for pool in _WORKER_POOLS:
        print(f"greetings from {pool} remote RBE worker")
        completed = python_subprocess.run(
            (
                str(remote_action_script),
                "--exec_strategy=remote",
                "--remote_accept_cache=false",
                "--remote_update_cache=false",
                f"--remote-worker-pool={pool}",
                "--",
                "/bin/pwd",
            ),
            source_dir=fuchsia_dir,
            base_env=env,
            cwd=fuchsia_dir,
            stdout=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            return completed.returncode
        lines = [
            line.rstrip("\r")
            for line in completed.stdout.splitlines()
            if line.strip()
        ]
        if not lines:
            return 1
        observed_pwd = lines[-1]
        print(f"  remote working dir ({pool}): {observed_pwd}")
        observed_roots[pool] = observed_pwd

    if probe_output is not None:
        probe_output.write_text(json.dumps(observed_roots), encoding="utf-8")
    return 0


def _read_pool_probes(probe_output: pathlib.Path) -> tuple[PoolProbe, ...]:
    try:
        raw_data = json.loads(probe_output.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as err:
        raise ReclientE2eError(
            f"Failed to read remote worker probe output: {err}"
        ) from err
    if not isinstance(raw_data, dict):
        raise ReclientE2eError(
            "Invalid remote worker probe output: expected a JSON object."
        )
    probes: list[PoolProbe] = []
    for pool in _WORKER_POOLS:
        observed = raw_data.get(pool)
        if not isinstance(observed, str) or not observed:
            raise ReclientE2eError(
                f"Missing remote working directory probe for pool '{pool}'."
            )
        probes.append(PoolProbe(pool=pool, remote_working_dir=observed))
    return tuple(probes)


def _validate_pool_probes(
    profile: rbe_profile.RbeProfile,
    probes: Sequence[PoolProbe],
) -> None:
    for probe in probes:
        if not profile.matches_remote_exec_root(probe.remote_working_dir):
            raise RemoteExecRootMismatchError(
                f"Pool '{probe.pool}': "
                f"{profile.format_remote_exec_root_mismatch(probe.remote_working_dir)}"
            )


def run_reclient_e2e(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
    *,
    xtrace: bool = False,
    gcloud_args: Sequence[str] = (),
) -> tuple[PoolProbe, ...]:
    """Probes `/bin/pwd` across reclient worker pools and validates RbeProfile.

    Args:
        fuchsia_dir: Root directory of the Fuchsia source checkout.
        env: Environment mapping for child processes.
        xtrace: Whether to invoke fuchsia-reproxy-wrap.sh under `/bin/bash -x`.
        gcloud_args: Extra flags forwarded to `gcloud` during re-authentication.

    Returns:
        Observed `PoolProbe` entries for each probed worker pool.

    Raises:
        RemoteExecRootMismatchError: If remote execution succeeded but a
            worker's working directory mismatched `profile.remote_exec_root`.
        ReclientE2eError: If remote execution or re-authentication failed.
    """
    print(
        "Testing a small remote-enabled reclient remote action end-to-end.\n"
        "If the first attempt fails, follow the authentication prompts.\n"
        "It will retry twice before reporting failure."
    )
    profile = rbe_profile.load(fuchsia_dir, env)
    active_env: dict[str, str] = dict(env)

    reproxy_wrap = str(fuchsia_dir / _REPROXY_WRAP_RELPATH)
    wrap_prefix: tuple[str, ...] = (
        ("/bin/bash", "-x", reproxy_wrap) if xtrace else (reproxy_wrap,)
    )
    with tempfile.TemporaryDirectory() as probe_tmp_dir:
        probe_output = pathlib.Path(probe_tmp_dir) / "pool_probes.json"
        probe_cmd: tuple[str, ...] = (
            *wrap_prefix,
            "--",
            *python_subprocess.cmd(
                fuchsia_dir,
                fuchsia_dir / _SELF_RELPATH,
                "--run-pool-probes",
                f"--fuchsia-dir={fuchsia_dir}",
                f"--probe-output={probe_output}",
                base_env=env,
            ),
        )

        # Coerce the working directory to match the exec-root (fuchsia_dir), so
        # the remote environment does not try to run in a nonexistent subdir
        # (b/376105913).
        for attempt in range(1, _MAX_ATTEMPTS + 1):
            print(
                f"\n=== Attempt {attempt}/{_MAX_ATTEMPTS} ====================="
            )
            probe_output.unlink(missing_ok=True)
            reproxy_env = _build_reproxy_env(active_env, profile)
            completed = subprocess.run(
                probe_cmd,
                cwd=fuchsia_dir,
                env=reproxy_env,
                check=False,
            )
            if completed.returncode == 0:
                probes = _read_pool_probes(probe_output)
                _validate_pool_probes(profile, probes)
                print(
                    _format_green(
                        "re-client+RBE authentication and profile validation "
                        "successful"
                    )
                )
                return probes
            if attempt < _MAX_ATTEMPTS:
                _refresh_auth(
                    fuchsia_dir,
                    active_env,
                    quota_project=profile.quota_project,
                    gcloud_args=gcloud_args,
                )

    raise ReclientE2eError(
        f"Failed to use reclient RBE after {_MAX_ATTEMPTS} attempts! "
        "Please contact fuchsia-build-team@ !"
    )


def _arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        default=_FUCHSIA_DIR,
        help="Path to the Fuchsia source checkout root.",
    )
    parser.add_argument(
        "--xtrace",
        action="store_true",
        help="Trace fuchsia-reproxy-wrap.sh execution with /bin/bash -x.",
    )
    parser.add_argument(
        "--run-pool-probes",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--probe-output",
        type=pathlib.Path,
        default=None,
        help=argparse.SUPPRESS,
    )
    return parser


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    if "--" in argv:
        sep = argv.index("--")
        cli_args, gcloud_args = argv[:sep], argv[sep + 1 :]
    else:
        cli_args, gcloud_args = argv, ()

    parsed = _arg_parser().parse_args(cli_args)
    if parsed.run_pool_probes:
        return _run_pool_probes(
            parsed.fuchsia_dir,
            env,
            probe_output=parsed.probe_output,
        )

    try:
        run_reclient_e2e(
            parsed.fuchsia_dir,
            env,
            xtrace=parsed.xtrace,
            gcloud_args=gcloud_args,
        )
    except ReclientE2eError as err:
        _print_error(str(err))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
