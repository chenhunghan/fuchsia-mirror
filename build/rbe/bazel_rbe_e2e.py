#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""End-to-end authentication and remote execution verification for Bazel RBE."""

import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import time
from collections.abc import Mapping, Sequence

from build.auth import auth_env
from build.rbe import rbe_profile
from build.scripts import python_subprocess

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parent.parent.parent
_BAZEL_TEST_SUBDIR = pathlib.Path("out/_bazel_rbe_test")
_MINIMAL_WORKSPACE_RELPATH = pathlib.Path(
    "build/bazel/scripts/minimal_workspace.py"
)
_BAZEL_CRED_HELPER = pathlib.Path(
    "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
)

_CRED_HELPER_WARN_SEC = 2.0
_CRED_HELPER_TIMEOUT_SEC = 10.0
_MAX_ATTEMPTS = 3

_GREEN = "\033[92m"
_YELLOW_BOLD = "\033[1;33m"
_RED_BOLD = "\033[1;91m"
_RESET = "\033[0m"


class BazelRbeE2eError(Exception):
    """Raised when end-to-end Bazel RBE verification fails."""


def _format_green(text: str) -> str:
    if not sys.stdout.isatty():
        return text
    return f"{_GREEN}{text}{_RESET}"


def _print_warning(message: str) -> None:
    prefix = (
        f"{_YELLOW_BOLD}WARNING:{_RESET}" if sys.stderr.isatty() else "WARNING:"
    )
    print(f"{prefix} {message}", file=sys.stderr)


def _print_error(message: str) -> None:
    prefix = f"{_RED_BOLD}ERROR:{_RESET}" if sys.stderr.isatty() else "ERROR:"
    print(f"{prefix} {message}", file=sys.stderr)


def _check_credential_helper_latency(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> None:
    """Checks that the SrcFS Bazel credential helper is not unreasonably slow."""
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, env=env
        )
    except (RuntimeError, ValueError):
        return
    if resolved_auth != auth_env.LOAS.value:
        return

    start_time = time.perf_counter()
    try:
        subprocess.run(
            (str(_BAZEL_CRED_HELPER),),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=_CRED_HELPER_TIMEOUT_SEC,
        )
        duration = time.perf_counter() - start_time
    except (FileNotFoundError, PermissionError, OSError):
        return
    except subprocess.TimeoutExpired:
        duration = _CRED_HELPER_TIMEOUT_SEC

    if duration >= _CRED_HELPER_WARN_SEC:
        _print_warning(
            "Access to the Bazel credential helper on SrcFS is extremely slow "
            f"({duration:.1f} seconds startup latency)."
        )
        _print_warning(
            "This is likely causing Bazel's Capabilities check or credential "
            "retrieval to timeout."
        )
        _print_warning(
            "We recommend checking SrcFS/Citc mount health or warming the "
            "cache by running:"
        )
        _print_warning(f"  ls -l {_BAZEL_CRED_HELPER}")


def _setup_bazel_workspace(
    fuchsia_dir: pathlib.Path,
    bazel_test_dir: pathlib.Path,
    env: Mapping[str, str],
) -> bool:
    shutil.rmtree(bazel_test_dir, ignore_errors=True)
    bazel_bin = env.get("PREBUILT_BAZEL", "")
    bazel_bin_args: tuple[str, ...] = (
        (f"--bazel-bin={bazel_bin}",) if bazel_bin else ()
    )
    completed = python_subprocess.run(
        (
            str(fuchsia_dir / _MINIMAL_WORKSPACE_RELPATH),
            f"--fuchsia-dir={fuchsia_dir}",
            *bazel_bin_args,
            f"--topdir={bazel_test_dir}",
        ),
        source_dir=fuchsia_dir,
        base_env=env,
        check=False,
    )
    return completed.returncode == 0


def _run_single_bazel_attempt(
    fuchsia_dir: pathlib.Path,
    active_env: dict[str, str],
    profile: rbe_profile.RbeProfile,
    *,
    no_resultstore: bool,
    gcloud_args: Sequence[str],
) -> bool:
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, env=active_env
        )
        out_dir = fuchsia_dir / "out"
        auth_env.ensure_credentials(
            resolved_auth,
            out_dir=out_dir,
            quota_project=profile.quota_project,
            gcloud_args=gcloud_args,
            env=active_env,
        )
        creds_path = auth_env.resolve_credentials_file(
            resolved_auth,
            auth_env.NONE,
            out_dir=out_dir.resolve(),
            rbe_quota_project=profile.quota_project,
            env=active_env,
        )
    except (RuntimeError, ValueError) as err:
        print(str(err), file=sys.stderr)
        return False

    if creds_path is not None:
        active_env[auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS] = str(
            creds_path
        )
    else:
        active_env.pop(auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS, None)

    bazel_test_dir = fuchsia_dir / _BAZEL_TEST_SUBDIR
    if not _setup_bazel_workspace(fuchsia_dir, bazel_test_dir, active_env):
        return False

    workspace_dir = bazel_test_dir / "workspace"
    bazel_launcher = str(bazel_test_dir / "bazel")

    # Shut down any running Bazel server so it refreshes credentials cleanly.
    shutdown_proc = subprocess.run(
        (bazel_launcher, "shutdown"),
        cwd=workspace_dir,
        env=active_env,
        check=False,
    )
    if shutdown_proc.returncode != 0:
        return False

    extra_args: tuple[str, ...] = (
        ("--bes_backend=",) if no_resultstore else ("--config=resultstore",)
    )
    auth_config_args: tuple[str, ...] = (
        ("--config=gcertauth",) if resolved_auth == auth_env.LOAS.value else ()
    )
    test_cmd: tuple[str, ...] = (
        bazel_launcher,
        "--nohome_rc",
        "test",
        "--nouse_action_cache",
        "--nocache_test_results",
        "--config=remote",
        *extra_args,
        *auth_config_args,
        "--build_event_upload_max_retries=1",
        "--bes_timeout=10s",
        "--bes_upload_mode=wait_for_upload_complete",
        ":all",
    )
    test_proc = subprocess.run(
        test_cmd,
        cwd=workspace_dir,
        env=active_env,
        check=False,
    )
    return test_proc.returncode == 0


def run_bazel_rbe_e2e(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
    *,
    no_resultstore: bool = False,
    gcloud_args: Sequence[str] = (),
) -> dict[str, str]:
    """Runs a minimal end-to-end remote-enabled Bazel test build.

    Args:
        fuchsia_dir: Root directory of the Fuchsia source checkout.
        env: Environment mapping for child processes.
        no_resultstore: Disable BES/ResultStore uploads during the test build.
        gcloud_args: Extra flags forwarded to `gcloud` during authentication.

    Returns:
        Updated environment dictionary (including isolated credentials if any).

    Raises:
        BazelRbeE2eError: If the remote Bazel test fails across all attempts.
    """
    print(
        "Testing a small remote-enabled bazel build end-to-end.\n"
        "If the first build attempt fails, follow the authentication prompts.\n"
        "It will retry twice before reporting failure."
    )
    profile = rbe_profile.load(fuchsia_dir, env)
    active_env: dict[str, str] = dict(env)

    _check_credential_helper_latency(fuchsia_dir, active_env)

    for attempt in range(1, _MAX_ATTEMPTS + 1):
        print(f"\n=== Attempt {attempt}/{_MAX_ATTEMPTS} =====================")
        if _run_single_bazel_attempt(
            fuchsia_dir,
            active_env,
            profile,
            no_resultstore=no_resultstore,
            gcloud_args=gcloud_args,
        ):
            print(_format_green("bazel+RBE authentication successful"))
            return active_env

    print("\n======================================")
    print(
        "If the previous build attempt failed due to authentication, "
        "follow the prompts to authenticate."
    )
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, env=active_env
        )
        auth_env.ensure_credentials(
            resolved_auth,
            out_dir=fuchsia_dir / "out",
            quota_project=profile.quota_project,
            gcloud_args=gcloud_args,
            env=active_env,
        )
    except (RuntimeError, ValueError) as err:
        raise BazelRbeE2eError("Failed to authenticate for bazel.") from err

    raise BazelRbeE2eError(
        "Failed to use Bazel RBE for unknown reason! "
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
        "--no-rs",
        "--no-resultstore",
        dest="no_resultstore",
        action="store_true",
        help="Test Bazel RBE without uploading build events to ResultStore.",
    )
    return parser


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    if "--" in argv:
        sep = argv.index("--")
        cli_args, gcloud_args = argv[:sep], argv[sep + 1 :]
    else:
        cli_args, gcloud_args = argv, ()

    parsed = _arg_parser().parse_args(cli_args)
    try:
        run_bazel_rbe_e2e(
            parsed.fuchsia_dir,
            env,
            no_resultstore=parsed.no_resultstore,
            gcloud_args=gcloud_args,
        )
    except BazelRbeE2eError as err:
        _print_error(str(err))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
