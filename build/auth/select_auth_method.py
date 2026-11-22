#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Determines the appropriate build authentication method.

It chooses from three outcomes:
  1. "loas"    : corporate gcert/LOAS credentials and accessible SrcFS helper.
  2. "oauth"   : local gcloud Application Default Credentials (ADC).
  3. "machine" : bot/VM using GCE VM Metadata.
"""

import argparse
import enum
import os
import pathlib
import shutil
import sys
from collections.abc import Mapping, Sequence
from typing import TextIO

from build.auth import gcloud, loas


class AuthType(str, enum.Enum):
    """The supported authentication types."""

    LOAS = "loas"
    OAUTH = "oauth"
    MACHINE = "machine"

    def __str__(self) -> str:
        return self.value

    @property
    def is_user_based(self) -> bool:
        """Returns True if the authentication type is user-based (LOAS or OAuth)."""
        return self in (AuthType.LOAS, AuthType.OAUTH)


_BAZEL_CRED_HELPER = pathlib.Path(
    "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
)

_SCRIPT_NAME = pathlib.Path(__file__).name


def _msg(text: str, file: TextIO | None = None) -> None:
    """Print a self-identifying message to a stream."""
    if file is None:
        file = sys.stderr
    print(f"[{_SCRIPT_NAME}] {text}", file=file)


def _is_executable(path: pathlib.Path) -> bool:
    """Checks if a path exists and is executable."""
    return path.exists() and os.access(path, os.X_OK)


def _is_infra_env(env: Mapping[str, str]) -> bool:
    """Checks if the environment is a bot/infrastructure environment."""
    return "BUILDBUCKET_ID" in env or "SWARMING_TASK_ID" in env


def _has_gcloud_or_adc(env: Mapping[str, str]) -> bool:
    """Checks if local gcloud ADC credentials or gcloud binary are present."""
    env_adc = env.get("GOOGLE_APPLICATION_CREDENTIALS", "")
    has_env_adc = bool(env_adc and pathlib.Path(env_adc).is_file())
    home = env.get("HOME", "")
    adc_file = pathlib.Path(home) / gcloud.ADC_SUBPATH if home else None
    has_gcloud = shutil.which("gcloud", path=env.get("PATH")) is not None
    return bool(has_env_adc or (adc_file and adc_file.exists()) or has_gcloud)


def _check_loas_type(
    env: Mapping[str, str], verbose: bool = False
) -> loas.RestrictionType | None:
    """Returns the host LOAS restriction type, or None if LOAS tools are unavailable."""
    try:
        return loas.check_loas_restrictions(env=env, verbose=verbose)
    except loas.LoasError:
        return None


def select_auth_method(
    env: Mapping[str, str],
    cred_helper: pathlib.Path = _BAZEL_CRED_HELPER,
    verbose: bool = False,
) -> AuthType:
    """Select the correct authentication method based on the environment.

    Args:
        env: Environment variables to inspect.
        cred_helper: Path to the Bazel credential helper.
        verbose: Print detailed diagnostic reasoning to stderr.

    Returns:
        The selected AuthType.

    Raises:
        ValueError: If no valid authentication method is detected.
    """

    def vmsg(text: str) -> None:
        if verbose:
            _msg(text)

    # 1. Infrastructure environment
    if _is_infra_env(env):
        vmsg(
            f"Detected infrastructure/bot environment (BUILDBUCKET_ID="
            f"{env.get('BUILDBUCKET_ID', '')}, SWARMING_TASK_ID="
            f"{env.get('SWARMING_TASK_ID', '')}). Selecting machine credentials."
        )
        return AuthType.MACHINE

    has_local_creds = _has_gcloud_or_adc(env)

    # 2. Determine LOAS restriction type
    loas_type = _check_loas_type(env, verbose=verbose)
    vmsg(f"LOAS certificate type check returned: {loas_type}")

    # Case 1: Ideal corporate flow (unrestricted LOAS + accessible credhelper)
    if loas_type == loas.RestrictionType.UNRESTRICTED and _is_executable(
        cred_helper
    ):
        vmsg(
            "Unrestricted LOAS certificate and accessible Bazel credential helper detected. Selecting LOAS."
        )
        return AuthType.LOAS

    # Case 2: Fallback to local gcloud OAuth/ADC if available
    if has_local_creds:
        if loas_type == loas.RestrictionType.UNRESTRICTED:
            _msg(
                f"Warning: Bazel credential helper on SrcFS is not accessible: {cred_helper}"
            )
            _msg("Gracefully falling back to OAuth (local gcloud/ADC) mode.")
        else:
            vmsg(
                "Local gcloud ADC or gcloud installation detected. Selecting OAuth."
            )
        return AuthType.OAUTH

    # Case 3: Completely unauthenticated
    raise ValueError(
        "We could not find a valid corporate LOAS credential, and local "
        "gcloud Application Default Credentials (ADC) are not configured.\n"
        "To configure RBE and authenticate successfully, please run: 'fx rbe auth'"
    )


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print diagnostic reasoning to stderr.",
    )
    args = parser.parse_args(argv)

    try:
        auth_method = select_auth_method(
            env=env,
            verbose=args.verbose,
        )
        print(auth_method)
        return 0
    except ValueError as e:
        _msg(str(e))
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
