# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host interface for querying and refreshing corporate LOAS (gcert) credentials."""

import enum
import getpass
import os
import pathlib
import shutil
import subprocess
import sys
from collections.abc import Iterator, Mapping, Sequence
from typing import Any

from build.rbe import textpb

_CREDENTIAL_PRINTER = pathlib.Path(
    "/google/bin/releases/prodsec/tools/credential_printer"
)

_BASENAME = pathlib.Path(__file__).name


class LoasError(Exception):
    """Raised when LOAS tools are unavailable or the host OS is unsupported."""


@enum.unique
class RestrictionType(str, enum.Enum):
    """LOAS certificate restriction classification."""

    UNRESTRICTED = "unrestricted"
    RESTRICTED = "restricted"

    def __str__(self) -> str:
        return self.value


def _msg(text: str) -> None:
    print(f"[{_BASENAME}] {text}", file=sys.stderr)


def _is_executable(path: pathlib.Path) -> bool:
    return path.exists() and os.access(path, os.X_OK)


def _require_linux(platform: str) -> None:
    # Fuchsia host builds only support Linux.
    if not platform.startswith("linux"):
        raise LoasError(f"Unhandled OS: {platform}")


def _loas2_credentials_file(
    user: str,
    platform: str = sys.platform,
) -> pathlib.Path:
    _require_linux(platform)
    return pathlib.Path(
        f"/run/credentials-cache/loas-{user}/cookies/{user}.loas2credentials"
    )


def _has_valid_loas_cert(
    gcertstatus_bin: str,
    env: Mapping[str, str],
    remaining: str | None = None,
) -> bool:
    args = (
        (gcertstatus_bin, "-check_ssh=false", f"--check_remaining={remaining}")
        if remaining is not None
        else (gcertstatus_bin, "-check_ssh=false")
    )
    status = subprocess.run(
        args,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
        check=False,
    )
    return status.returncode == 0


def _run_gcert(gcert_bin: str, env: Mapping[str, str]) -> bool:
    return subprocess.run((gcert_bin,), env=env, check=False).returncode == 0


def _can_run_credential_printer(
    cred_printer: pathlib.Path,
    env: Mapping[str, str],
) -> bool:
    try:
        probe = subprocess.run(
            (str(cred_printer),),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
            check=False,
        )
    except OSError:
        return False
    return probe.returncode == 0


def _read_loas2_credentials(
    cred_printer: pathlib.Path,
    creds_file: pathlib.Path,
    env: Mapping[str, str],
) -> str | None:
    try:
        return subprocess.check_output(
            (str(cred_printer), str(creds_file)),
            text=True,
            stderr=subprocess.DEVNULL,
            env=env,
        )
    except (subprocess.CalledProcessError, OSError):
        return None


def ensure_loas(
    *,
    force: bool = False,
    remaining: str = "2h",
    env: Mapping[str, str] | None = None,
) -> None:
    """Ensures valid LOAS credentials, running interactive gcert if needed.

    Args:
        force: Whether to force re-authentication regardless of current status.
        remaining: Minimum remaining certificate validity required, formatted as
            a gcertstatus duration string (e.g., "2h" or "30m").
        env: Environment variables map to use, or None for os.environ.

    Raises:
        LoasError: If gcert is missing or authentication fails.
    """
    active_env = os.environ if env is None else env
    path_env = active_env.get("PATH")
    if (
        not force
        and (gcertstatus_bin := shutil.which("gcertstatus", path=path_env))
        is not None
        and _has_valid_loas_cert(
            gcertstatus_bin, active_env, remaining=remaining
        )
    ):
        return
    gcert_bin = shutil.which("gcert", path=path_env)
    if gcert_bin is None:
        raise LoasError("gcert not found.")
    if not _run_gcert(gcert_bin, active_env):
        raise LoasError("Failed to authenticate with gcert.")


def _is_other_token(value: Any, expected: str) -> bool:
    """Returns True if value is an unquoted textpb token matching expected."""
    return (
        isinstance(value, textpb.Token)
        and value.type == textpb.TokenType.OTHER_VALUE
        and value.text == expected
    )


def _has_token_value(
    msg: textpb.TextProtoMapping, key: str, expected: str
) -> bool:
    """Returns True if msg[key] contains an unquoted token matching expected."""
    values = msg.get(key)
    if not isinstance(values, Sequence) or isinstance(values, (str, bytes)):
        return False
    return any(_is_other_token(v, expected) for v in values)


def _has_restricted_role(msg: textpb.TextProtoMapping) -> bool:
    """Returns True if any message in msg sets restricted_role: true."""
    return any(
        _has_token_value(sub_msg, "restricted_role", "true")
        for sub_msg in textpb.walk_messages(msg)
    )


def _has_dest_perm_type(
    msg: textpb.TextProtoMapping, expected_type: str
) -> bool:
    """Returns True if msg has a destination_permissions entry of expected_type."""
    perms = msg.get("destination_permissions")
    if not isinstance(perms, Sequence) or isinstance(perms, (str, bytes)):
        return False
    return any(
        isinstance(perm, Mapping)
        and _has_token_value(perm, "type", expected_type)
        for perm in perms
    )


def _is_unrestricted_acct(acct: textpb.TextProtoMapping) -> bool:
    """Returns True if an [AcctRestrictions] block permits unrestricted access."""
    if len(acct) == 0 or _has_dest_perm_type(acct, "CORP"):
        return True
    dest_restrictions = acct.get("destination_restriction")
    if not isinstance(dest_restrictions, Sequence) or isinstance(
        dest_restrictions, (str, bytes)
    ):
        return False
    return any(
        isinstance(restr, Mapping) and _has_dest_perm_type(restr, "PROD")
        for restr in dest_restrictions
    )


def _iter_acct_restrictions(
    msg: textpb.TextProtoMapping,
) -> Iterator[textpb.TextProtoMapping]:
    """Yields all [AcctRestrictions] message blocks in msg."""
    for sub_msg in textpb.walk_messages(msg):
        if (acct_values := sub_msg.get("[AcctRestrictions]")) is None:
            continue
        if not isinstance(acct_values, Sequence) or isinstance(
            acct_values, (str, bytes)
        ):
            raise textpb.ParseError(
                "Expected sequence of blocks for [AcctRestrictions]."
            )
        for item in acct_values:
            if not isinstance(item, Mapping):
                raise textpb.ParseError(
                    "Expected message block for [AcctRestrictions]."
                )
            yield item


def _classify_loas2(output: str) -> RestrictionType:
    """Classifies LOAS2 restrictions from gcertstatus or credential_printer text."""
    try:
        msg = textpb.parse(output.splitlines())
        if _has_restricted_role(msg):
            return RestrictionType.RESTRICTED
        acct_blocks = tuple(_iter_acct_restrictions(msg))
    except textpb.ParseError:
        return RestrictionType.RESTRICTED
    if acct_blocks and all(_is_unrestricted_acct(acct) for acct in acct_blocks):
        return RestrictionType.UNRESTRICTED
    return RestrictionType.RESTRICTED


def _run_gcertstatus_loas2(
    gcertstatus_bin: str,
    env: Mapping[str, str],
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        (gcertstatus_bin, "-format=loas2", "-check_ssh=false"),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=env,
        check=False,
    )


def check_loas_restrictions(
    env: Mapping[str, str] | None = None,
    cred_printer: pathlib.Path = _CREDENTIAL_PRINTER,
    verbose: bool = False,
    platform: str = sys.platform,
) -> RestrictionType:
    """Determines whether the host environment can get unrestricted LOAS credentials."""
    _require_linux(platform)
    active_env = os.environ if env is None else env
    path_env = active_env.get("PATH")

    gcert_bin = shutil.which("gcert", path=path_env)
    if gcert_bin is None:
        raise LoasError("gcert not found.")
    gcertstatus_bin = shutil.which("gcertstatus", path=path_env)
    if gcertstatus_bin is None:
        raise LoasError("gcertstatus not found.")

    # Check status and query LOAS2 metadata in a single call; renew if expired.
    status = _run_gcertstatus_loas2(gcertstatus_bin, active_env)
    if status.returncode != 0:
        if not _run_gcert(gcert_bin, active_env):
            if verbose:
                _msg("Failed to renew LOAS2 certificate.")
            return RestrictionType.RESTRICTED
        status = _run_gcertstatus_loas2(gcertstatus_bin, active_env)

    if status.returncode == 0 and status.stdout.strip():
        if verbose:
            _msg("Inspected LOAS2 restrictions via gcertstatus -format=loas2.")
        return _classify_loas2(status.stdout)

    if not _is_executable(cred_printer):
        if verbose:
            _msg(
                f"Unable to access {cred_printer}.  See go/binfs-glinux-workstations."
            )
        return RestrictionType.RESTRICTED

    if not _can_run_credential_printer(cred_printer, active_env):
        return RestrictionType.RESTRICTED

    user = active_env.get("USER")
    if not user:
        try:
            user = getpass.getuser()
        except (KeyError, OSError):
            return RestrictionType.RESTRICTED

    creds_file = _loas2_credentials_file(user, platform=platform)
    if verbose:
        _msg(f"Examining credentials file: {creds_file}")

    output = _read_loas2_credentials(cred_printer, creds_file, active_env)
    if output is None:
        return RestrictionType.RESTRICTED

    return _classify_loas2(output)
