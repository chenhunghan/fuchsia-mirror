# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines the canonical runtime environment contract for build authentication."""

import argparse
import enum
import functools
import os
import shutil
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

from build.auth import gcloud, gcloud_creds, loas, select_auth_method

# Canonical environment variable names for build-level authentication.
ENV_RBE_AUTH_TYPE = "FX_BUILD_RBE_AUTH_TYPE"
ENV_RS_AUTH_TYPE = "FX_BUILD_RS_AUTH_TYPE"
ENV_GOOGLE_APPLICATION_CREDENTIALS = "GOOGLE_APPLICATION_CREDENTIALS"
ENV_USER = "USER"

_BUILD_AUTH_CONFIG = Path(".fx/config/build-auth")
_LEGACY_BUILD_AUTH_CONFIG = Path(".fx/auth-config")


class AuthType(str, enum.Enum):
    """Supported authentication methods across Fuchsia build tools."""

    LOAS = "loas"
    OAUTH = "oauth"
    MACHINE = "machine"
    NONE = "none"
    AUTO = "auto"

    def __str__(self) -> str:
        return self.value


# Top-level aliases for convenient access.
LOAS = AuthType.LOAS
OAUTH = AuthType.OAUTH
MACHINE = AuthType.MACHINE
NONE = AuthType.NONE
AUTO = AuthType.AUTO

_EXPLICIT_AUTH_CHOICES = tuple(t.value for t in AuthType if t != AuthType.AUTO)


def make_auth_env(
    rbe_auth: str | AuthType = AuthType.NONE,
    rs_auth: str | AuthType = AuthType.NONE,
    user: str | None = None,
    credentials_file: Path | str | None = None,
) -> dict[str, str]:
    """Constructs a clean dictionary of build authentication environment variables."""
    env: dict[str, str] = {
        ENV_RBE_AUTH_TYPE: str(rbe_auth),
        ENV_RS_AUTH_TYPE: str(rs_auth),
    }
    if user is not None:
        env[ENV_USER] = user
    if credentials_file is not None:
        env[ENV_GOOGLE_APPLICATION_CREDENTIALS] = str(credentials_file)
    return env


def rbe_auth_type(env: Mapping[str, str]) -> str:
    """Returns the RBE auth type, defaulting to 'none'."""
    return env.get(ENV_RBE_AUTH_TYPE, AuthType.NONE.value)


def rs_auth_type(env: Mapping[str, str]) -> str:
    """Returns the ResultStore auth type, defaulting to 'none'."""
    return env.get(ENV_RS_AUTH_TYPE, AuthType.NONE.value)


def is_loas(env: Mapping[str, str]) -> bool:
    """True if either RBE or ResultStore is configured to use LOAS."""
    return AuthType.LOAS.value in (rbe_auth_type(env), rs_auth_type(env))


def requires_gcert(env: Mapping[str, str]) -> bool:
    """Semantic alias for is_loas; True if corporate gcert is required."""
    return is_loas(env)


def is_machine(env: Mapping[str, str]) -> bool:
    """True if either RBE or ResultStore is configured to use machine credentials."""
    return AuthType.MACHINE.value in (rbe_auth_type(env), rs_auth_type(env))


def is_user_auth(auth_type: str | AuthType) -> bool:
    """Returns True if auth_type represents interactive user credentials."""
    return auth_type in (AuthType.LOAS, AuthType.OAUTH)


def _uses_adc(auth_type: str | AuthType) -> bool:
    """Returns True if auth_type uses gcloud Application Default Credentials."""
    return auth_type == AuthType.OAUTH


def credentials_file(env: Mapping[str, str]) -> Path | None:
    """Returns the path to GOOGLE_APPLICATION_CREDENTIALS if set."""
    path = env.get(ENV_GOOGLE_APPLICATION_CREDENTIALS)
    return Path(path) if path else None


def _select_quota_project(
    rbe_auth: str | AuthType,
    rs_auth: str | AuthType,
    rbe_quota_project: str | None = None,
    rs_quota_project: str | None = None,
) -> str | None:
    """Selects the active quota project for credential isolation.

    RBE takes precedence over ResultStore if both use ADC-based auth.
    """
    if _uses_adc(rbe_auth):
        return rbe_quota_project
    if _uses_adc(rs_auth):
        return rs_quota_project
    return None


def resolve_credentials_file(
    rbe_auth: str | AuthType,
    rs_auth: str | AuthType,
    *,
    out_dir: Path,
    rbe_quota_project: str | None = None,
    rs_quota_project: str | None = None,
    env: Mapping[str, str] | None = None,
) -> Path | None:
    """Isolates or resolves the GOOGLE_APPLICATION_CREDENTIALS path if needed."""
    active_env = os.environ if env is None else env
    explicit_creds = credentials_file(active_env)
    if _uses_adc(rbe_auth) or _uses_adc(rs_auth):
        quota_project = _select_quota_project(
            rbe_auth, rs_auth, rbe_quota_project, rs_quota_project
        )
        if quota_project:
            isolated = gcloud_creds.isolate_credentials_safe(
                out_dir=out_dir,
                quota_project=quota_project,
                source_credentials_path=explicit_creds,
            )
            if isolated is not None:
                return isolated
        if explicit_creds is not None:
            return explicit_creds
        try:
            adc_path = gcloud.resolve_global_adc_path(env=active_env)
            if adc_path.is_file():
                return adc_path
        except (RuntimeError, OSError):
            pass
        return None
    if AuthType.MACHINE in (rbe_auth, rs_auth):
        return explicit_creds
    return None


def ensure_credentials(
    auth_type: str | AuthType,
    *,
    force: bool = False,
    out_dir: Path | None = None,
    quota_project: str | None = None,
    gcloud_args: Sequence[str] = (),
    env: Mapping[str, str] | None = None,
) -> Path | None:
    """Ensures active credentials for auth_type, returning isolated ADC path if any."""
    if auth_type == AuthType.LOAS:
        try:
            loas.ensure_loas(force=force, env=env)
        except loas.LoasError as err:
            raise RuntimeError(str(err)) from err
        return None
    if auth_type == AuthType.OAUTH:
        try:
            return gcloud_creds.ensure_oauth(
                force=force,
                out_dir=out_dir,
                quota_project=quota_project,
                gcloud_args=gcloud_args,
            )
        except gcloud_creds.GcloudCredsError as err:
            raise RuntimeError(str(err)) from err
    if auth_type in (AuthType.MACHINE, AuthType.NONE):
        return None
    raise ValueError(f"Unhandled authentication type: {auth_type!r}")


def reclient_credshelper_path(fuchsia_dir: Path, host_tag: str) -> Path:
    """Returns the canonical path to the prebuilt reclient credshelper binary."""
    return (
        fuchsia_dir
        / "prebuilt"
        / "third_party"
        / "reclient"
        / host_tag
        / "credshelper"
    )


def _parse_build_auth_config(text: str) -> tuple[str | None, str | None]:
    """Parses explicit `auth_type` and legacy `loas_cert_type` from config text."""
    explicit_auth: str | None = None
    legacy_auth: str | None = None
    for line in text.splitlines():
        key, _, value = line.partition("#")[0].partition("=")
        key = key.strip()
        value = value.strip().strip("'\"")
        if key == "auth_type" and value:
            explicit_auth = value
            if explicit_auth != AuthType.AUTO.value:
                return explicit_auth, legacy_auth
        elif key == "loas_cert_type":
            if value == "unrestricted":
                legacy_auth = AuthType.LOAS.value
            elif value == "restricted":
                legacy_auth = AuthType.OAUTH.value
        if explicit_auth is not None and legacy_auth is not None:
            return explicit_auth, legacy_auth
    return explicit_auth, legacy_auth


def cached_auth_type(fuchsia_dir: Path) -> str | None:
    """Returns the auth type cached in .fx/config/build-auth."""
    # TODO(https://fxbug.dev/573624782): Use //build/scripts:properties_lib
    # once it has a Bazel target.
    try:
        text = (fuchsia_dir / _BUILD_AUTH_CONFIG).read_text(errors="replace")
    except OSError:
        return None
    explicit_auth, legacy_auth = _parse_build_auth_config(text)
    if explicit_auth not in (None, AuthType.AUTO.value):
        return explicit_auth
    return legacy_auth if legacy_auth is not None else explicit_auth


def _probe_system_auth_type(env: Mapping[str, str]) -> AuthType:
    try:
        detected = select_auth_method.select_auth_method(env=env)
        return AuthType(detected.value)
    except ValueError as err:
        raise RuntimeError(
            f"Unable to infer build authentication type: {err}"
        ) from err


def resolve_and_cache_auth_type(
    fuchsia_dir: Path,
    *,
    force: bool = False,
    env: Mapping[str, str] | None = None,
) -> str:
    """Resolves the host build auth type and caches it in .fx/config/build-auth.

    Also migrates legacy `.fx/auth-config` files and `loas_cert_type=` entries
    to `.fx/config/build-auth` with `auth_type=<value>`.
    """
    active_env = os.environ if env is None else env
    config_path = fuchsia_dir / _BUILD_AUTH_CONFIG
    legacy_path = fuchsia_dir / _LEGACY_BUILD_AUTH_CONFIG
    if legacy_path.is_file():
        config_path.parent.mkdir(parents=True, exist_ok=True)
        print(
            f"INFO: Moving {legacy_path} to new location {config_path}.  "
            "No further action is necessary.",
            file=sys.stderr,
        )
        legacy_path.replace(config_path)

    if not force:
        try:
            text = config_path.read_text(errors="replace")
        except OSError:
            text = None
        if text is not None:
            explicit_auth, legacy_auth = _parse_build_auth_config(text)
            if (
                explicit_auth is not None
                and explicit_auth != AuthType.AUTO.value
            ):
                return explicit_auth
            if legacy_auth is not None:
                config_path.write_text(
                    f"auth_type={legacy_auth}\n", encoding="utf-8"
                )
                return legacy_auth

    if force:
        loas_destroy_bin = shutil.which(
            "loas_destroy", path=active_env.get("PATH")
        )
        if loas_destroy_bin is not None:
            subprocess.run((loas_destroy_bin,), env=active_env, check=False)

    detected = _probe_system_auth_type(active_env)
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(f"auth_type={detected.value}\n", encoding="utf-8")
    return detected.value


@functools.cache
def detect_system_auth_type() -> AuthType:
    """Probes the host system to determine the appropriate authentication type.

    Cached in-process to avoid redundant gcertstatus and gcloud subprocess invocations.
    """
    return _probe_system_auth_type(os.environ)


def resolve_build_auth_type(
    auth_type: str | AuthType = AuthType.AUTO,
    use_machine_credentials: bool = False,
    env: Mapping[str, str] | None = None,
    fuchsia_dir: Path | None = None,
    service_env_var: str = ENV_RBE_AUTH_TYPE,
) -> str:
    """Resolves effective build authentication type (loas, oauth, machine, none).

    Precedence:
      1. use_machine_credentials -> machine
      2. auth_type != 'auto' -> explicit auth_type
      3. env.get(service_env_var) -> inherited parent wrapper environment
      4. cached_auth_type() -> auth type cached by `fx rbe`
      5. detect_system_auth_type() -> host system detection (cached when env is None)
    """
    if use_machine_credentials:
        return AuthType.MACHINE.value

    active_env = os.environ if env is None else env
    resolved = str(auth_type)

    if resolved == AuthType.AUTO.value:
        env_auth = active_env.get(service_env_var)
        if env_auth and env_auth != AuthType.AUTO.value:
            resolved = env_auth

    if resolved == AuthType.AUTO.value and fuchsia_dir:
        resolved = cached_auth_type(fuchsia_dir) or resolved

    if resolved == AuthType.AUTO.value:
        if env is None:
            detected = detect_system_auth_type()
        else:
            detected = _probe_system_auth_type(env)
        resolved = str(detected)

    if resolved not in _EXPLICIT_AUTH_CHOICES:
        raise ValueError(f"Unhandled authentication type: {resolved!r}")

    return resolved


def resolve_rbe_auth_env(
    env: Mapping[str, str],
    fuchsia_dir: Path | None = None,
    default: str | AuthType = AuthType.OAUTH,
) -> dict[str, str]:
    """Resolves RBE authentication from env/cache/system into a canonical auth_env dict."""
    try:
        resolved = resolve_build_auth_type(env=env, fuchsia_dir=fuchsia_dir)
    except (RuntimeError, ValueError):
        resolved = default.value if isinstance(default, AuthType) else default
    return make_auth_env(rbe_auth=resolved)


def _cmd_ensure(
    parsed: argparse.Namespace,
    extra_args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    try:
        ensure_credentials(
            parsed.auth_type,
            force=parsed.force,
            out_dir=parsed.out_dir,
            quota_project=parsed.quota_project,
            gcloud_args=extra_args,
            env=env,
        )
    except (RuntimeError, ValueError) as err:
        print(str(err), file=sys.stderr)
        return 1
    return 0


def _cmd_resolve_creds(
    parsed: argparse.Namespace,
    _extra_args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    resolved_creds = resolve_credentials_file(
        parsed.auth_type,
        AuthType.NONE,
        out_dir=parsed.out_dir.resolve(),
        rbe_quota_project=parsed.quota_project,
        env=env,
    )
    if resolved_creds is not None:
        print(resolved_creds)
    return 0


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    ensure_parser = subparsers.add_parser(
        "ensure",
        help="Ensure active credentials for the specified build auth type.",
    )
    ensure_parser.add_argument(
        "--auth-type",
        required=True,
        choices=_EXPLICIT_AUTH_CHOICES,
        help="Resolved authentication type (loas, oauth, machine, none).",
    )
    ensure_parser.add_argument(
        "--force",
        action="store_true",
        help="Force interactive credential renewal.",
    )
    ensure_parser.add_argument(
        "--out-dir",
        type=Path,
        help="Output directory for isolated ADC credentials.",
    )
    ensure_parser.add_argument(
        "--quota-project",
        help="Quota project ID for isolated user credentials.",
    )
    ensure_parser.set_defaults(func=_cmd_ensure)

    resolve_parser = subparsers.add_parser(
        "resolve-creds",
        help="Isolate or resolve GOOGLE_APPLICATION_CREDENTIALS for an auth type.",
    )
    resolve_parser.add_argument(
        "--auth-type",
        required=True,
        choices=_EXPLICIT_AUTH_CHOICES,
        help="Resolved authentication type (loas, oauth, machine, none).",
    )
    resolve_parser.add_argument(
        "--out-dir",
        type=Path,
        required=True,
        help="Output directory for isolated ADC credentials.",
    )
    resolve_parser.add_argument(
        "--quota-project",
        help="Quota project ID for isolated user credentials.",
    )
    resolve_parser.set_defaults(func=_cmd_resolve_creds)
    return parser


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    if "--" in argv:
        sep = argv.index("--")
        cli_args, extra_args = argv[:sep], argv[sep + 1 :]
    else:
        cli_args, extra_args = argv, ()

    parser = _main_arg_parser()
    parsed = parser.parse_args(cli_args)
    return parsed.func(parsed, extra_args, env)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
