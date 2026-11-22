#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Python CLI implementation backing `fx rbe` subcommands."""

import argparse
import os
import pathlib
import re
import shutil
import subprocess
import sys
from collections.abc import Mapping, Sequence

from build.auth import auth_env
from build.rbe import (
    bazel_rbe_e2e,
    fuchsia,
    rbe_diagnose,
    rbe_profile,
    rbe_settings,
    reclient_e2e,
)

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parent.parent.parent
_CHECK_RUNT_RELPATH = pathlib.Path(
    "vendor/google/scripts/devshell/check-runt-network"
)
_REPROXY_LOGS_RELPATH = pathlib.Path("out/.reproxy_logs")
_FX_BUILD_DIR_FILE = pathlib.Path(".fx-build-dir")

_DISABLED_RS_VALUES: frozenset[str] = frozenset({"none", "false", "0", "no"})

_BOLD = "\033[1m"
_GREEN = "\033[92m"
_BOLD_GREEN = "\033[1;92m"
_RED_BOLD = "\033[1;91m"
_RESET = "\033[0m"


def _style_stdout(text: str, ansi_code: str) -> str:
    if not sys.stdout.isatty():
        return text
    return f"{ansi_code}{text}{_RESET}"


def _print_error(message: str) -> None:
    prefix = f"{_RED_BOLD}ERROR:{_RESET}" if sys.stderr.isatty() else "ERROR:"
    print(f"{prefix} {message}", file=sys.stderr)


def _resolve_build_dir(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> pathlib.Path | None:
    """Resolves the active build directory if configured and present on disk."""
    env_build_dir = env.get("FUCHSIA_BUILD_DIR", "").strip()
    if env_build_dir:
        candidate = pathlib.Path(env_build_dir)
        if not candidate.is_absolute():
            candidate = fuchsia_dir / candidate
        return candidate if candidate.is_dir() else None

    fx_build_dir_file = fuchsia_dir / _FX_BUILD_DIR_FILE
    try:
        raw_rel = fx_build_dir_file.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    if not raw_rel:
        return None
    candidate = pathlib.Path(raw_rel)
    if not candidate.is_absolute():
        candidate = fuchsia_dir / candidate
    return candidate if candidate.is_dir() else None


def _build_needs_auth(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> bool:
    """Returns True if the configured build uses authenticated remote services."""
    build_dir = _resolve_build_dir(fuchsia_dir, env)
    if build_dir is None:
        return False

    rs_mode = (
        env.get("FX_BUILD_RESULTSTORE_OVERRIDE")
        or env.get("CONFIG_RESULTSTORE_ENABLED")
        or "none"
    ).lower()
    if rs_mode not in _DISABLED_RS_VALUES:
        return True

    if not rbe_settings.exists(build_dir):
        return False
    try:
        return rbe_settings.load(build_dir).needs_auth
    except (ValueError, KeyError):
        return False


def _parse_force_only_args(args: Sequence[str]) -> bool | None:
    """Parses `-f`/`--force` for check_loas, returning None on unknown flags."""
    force = False
    for opt in args:
        if opt in ("-f", "--force"):
            force = True
        else:
            _print_error(f"get_auth_type: Unknown option {opt}")
            return None
    return force


def _auth_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fx rbe auth",
        allow_abbrev=False,
        add_help=False,
        exit_on_error=False,
    )
    parser.add_argument("-f", "--force", action="store_true")
    parser.add_argument("-p", "--profile", dest="override_profile", default="")
    parser.add_argument(
        "--no-rs",
        "--no-resultstore",
        dest="no_resultstore",
        action="store_true",
    )
    return parser


def _cmd_auth(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    env: Mapping[str, str],
    *,
    xtrace: bool,
) -> int:
    try:
        parsed, unknown_args = _auth_arg_parser().parse_known_args(args)
    except argparse.ArgumentError as err:
        _print_error(str(err))
        return 1

    gcloud_args: tuple[str, ...] = tuple(unknown_args)
    active_env: dict[str, str] = dict(env)
    if parsed.override_profile:
        active_env = active_env | {
            "FX_BUILD_RBE_PROFILE": parsed.override_profile,
            "FX_BUILD_RS_PROFILE": parsed.override_profile,
        }

    try:
        profile = rbe_profile.load(fuchsia_dir, active_env)
    except ValueError as err:
        _print_error(str(err))
        return 1

    print(
        _style_stdout(
            f"Testing RBE authentication for profile: {profile.profile_name}",
            _BOLD,
        )
    )

    if profile.auth_type == "none":
        print(
            _style_stdout(
                f"The active RBE profile ('{profile.profile_name}') does not "
                "require authentication (auth_type: none).",
                _GREEN,
            )
        )
        print(
            _style_stdout(
                "No authentication setup is required. "
                "You can now 'fx build' with RBE.",
                _BOLD_GREEN,
            )
        )
        return 0

    if parsed.force:
        try:
            force_auth_type = auth_env.resolve_and_cache_auth_type(
                fuchsia_dir, env=active_env
            )
            auth_env.ensure_credentials(
                force_auth_type,
                force=True,
                out_dir=fuchsia_dir / "out",
                quota_project=profile.quota_project,
                gcloud_args=gcloud_args,
                env=active_env,
            )
        except (RuntimeError, ValueError):
            print("Failed to authenticate.")
            return 1

    # Run check-runt-network command if available (http://b/399096925).
    check_runt_script = fuchsia_dir / _CHECK_RUNT_RELPATH
    if os.access(check_runt_script, os.X_OK):
        runt_proc = subprocess.run(
            (str(check_runt_script),),
            env=active_env,
            check=False,
        )
        if runt_proc.returncode != 0:
            return runt_proc.returncode

    try:
        updated_env = bazel_rbe_e2e.run_bazel_rbe_e2e(
            fuchsia_dir,
            active_env,
            no_resultstore=parsed.no_resultstore,
            gcloud_args=gcloud_args,
        )
        print()
        reclient_e2e.run_reclient_e2e(
            fuchsia_dir,
            updated_env,
            xtrace=xtrace,
            gcloud_args=gcloud_args,
        )
    except (
        bazel_rbe_e2e.BazelRbeE2eError,
        reclient_e2e.ReclientE2eError,
    ) as err:
        _print_error(str(err))
        return 1

    print()
    print(_style_stdout("You can now 'fx build' with RBE.", _BOLD_GREEN))
    return 0


def _cmd_check_loas_type(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    force = _parse_force_only_args(args)
    if force is None:
        return 1
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, force=force, env=env
        )
    except (RuntimeError, ValueError) as err:
        _print_error(
            f"Error trying to select build authentication method: {err}"
        )
        return 1
    print(resolved_auth)
    return 0


def _cmd_check_loas(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    print("Checking build authentication method:")
    force = _parse_force_only_args(args)
    if force is None:
        return 1
    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, force=force, env=env
        )
    except (RuntimeError, ValueError) as err:
        _print_error(
            f"Error trying to select build authentication method: {err}"
        )
        return 1
    print(_style_stdout(resolved_auth, _GREEN))
    return 0


def _cmd_cleanlogs(fuchsia_dir: pathlib.Path) -> int:
    rbe_logs_dir_root = fuchsia_dir / _REPROXY_LOGS_RELPATH
    print(f"Removing {rbe_logs_dir_root}")
    shutil.rmtree(rbe_logs_dir_root, ignore_errors=True)
    return 0


def _cmd_ensure_loas(env: Mapping[str, str]) -> int:
    try:
        auth_env.ensure_credentials(auth_env.LOAS, env=env)
    except (RuntimeError, ValueError) as err:
        print(str(err), file=sys.stderr)
        return 1
    return 0


def _cmd_ensure_oauth(
    args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    try:
        auth_env.ensure_credentials(auth_env.OAUTH, gcloud_args=args, env=env)
    except (RuntimeError, ValueError) as err:
        print(str(err), file=sys.stderr)
        return 1
    return 0


def _natural_sort_key(
    entry: pathlib.Path,
) -> tuple[tuple[int, int, str], ...]:
    return tuple(
        (0, int(part), part) if part.isdigit() else (1, 0, part)
        for part in re.split(r"(\d+)", entry.name)
    )


def _latest_matching_subdir(
    parent_dir: pathlib.Path, prefix: str
) -> pathlib.Path | None:
    if not parent_dir.is_dir():
        return None
    candidates = sorted(
        (
            entry
            for entry in parent_dir.iterdir()
            if entry.name.startswith(prefix) and entry.is_dir()
        ),
        key=_natural_sort_key,
    )
    return candidates[-1] if candidates else None


def _cmd_last_logdir(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> int:
    build_dir = _resolve_build_dir(fuchsia_dir, env)
    if build_dir is None:
        return 0

    build_subdir = build_dir.name
    out_dir = build_dir.parent.name

    new_logs_dir = fuchsia_dir / out_dir / ".build_logs" / build_subdir
    recent_new = _latest_matching_subdir(new_logs_dir, "build.")
    if recent_new is not None:
        print(recent_new)
        return 0

    old_logs_dir = fuchsia_dir / out_dir / ".reproxy_logs" / build_subdir
    recent_old = _latest_matching_subdir(old_logs_dir, "reproxy.")
    if recent_old is not None:
        print(recent_old)
    return 0


def _cmd_preflight(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> int:
    if not _build_needs_auth(fuchsia_dir, env):
        return 0

    try:
        profile = rbe_profile.load(fuchsia_dir, env)
    except ValueError as err:
        _print_error(str(err))
        return 1

    if profile.auth_type == "none":
        return 0

    try:
        resolved_auth = auth_env.resolve_and_cache_auth_type(
            fuchsia_dir, env=env
        )
        auth_env.ensure_credentials(
            resolved_auth,
            out_dir=fuchsia_dir / "out",
            quota_project=profile.quota_project,
            env=env,
        )
    except (RuntimeError, ValueError) as err:
        _print_error(str(err))
        return 1
    return 0


def _cmd_profile(
    fuchsia_dir: pathlib.Path,
    args: Sequence[str],
    env: Mapping[str, str],
) -> int:
    profile_env = dict(env) | {"FUCHSIA_DIR": str(fuchsia_dir)}
    return rbe_profile.main(args, profile_env)


def _cmd_version(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str],
) -> int:
    reclient_env = env.get("PREBUILT_RECLIENT_DIR", "").strip()
    reclient_dir = (
        pathlib.Path(reclient_env)
        if reclient_env
        else fuchsia_dir / fuchsia.RECLIENT_BINDIR
    )
    reproxy_bin = reclient_dir / "reproxy"
    if not reproxy_bin.exists():
        _print_error(f"reproxy binary not found: {reproxy_bin}")
        return 1
    print("reclient version:")
    completed = subprocess.run(
        (str(reproxy_bin), "--version"),
        env=env,
        check=False,
    )
    return completed.returncode


def _top_level_parser(
    default_fuchsia_dir: pathlib.Path,
) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fx rbe",
        description=__doc__,
        allow_abbrev=False,
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        default=default_fuchsia_dir,
        help="Path to the Fuchsia source checkout root.",
    )
    parser.add_argument(
        "--xtrace",
        action="store_true",
        help="Propagate shell tracing to wrapper scripts.",
    )
    parser.add_argument(
        "action",
        help="Subcommand to execute.",
    )
    parser.add_argument(
        "action_args",
        nargs=argparse.REMAINDER,
        help="Arguments forwarded to the subcommand.",
    )
    return parser


def main(argv: Sequence[str], env: Mapping[str, str]) -> int:
    env_fuchsia_dir = env.get("FUCHSIA_DIR", "").strip()
    default_fuchsia_dir = (
        pathlib.Path(env_fuchsia_dir) if env_fuchsia_dir else _FUCHSIA_DIR
    )
    parsed = _top_level_parser(default_fuchsia_dir).parse_args(argv)
    fuchsia_dir: pathlib.Path = parsed.fuchsia_dir
    action: str = parsed.action
    action_args: tuple[str, ...] = tuple(parsed.action_args)

    if action == "auth":
        return _cmd_auth(fuchsia_dir, action_args, env, xtrace=parsed.xtrace)
    if action == "check_loas":
        return _cmd_check_loas(fuchsia_dir, action_args, env)
    if action == "_check_loas_type":
        return _cmd_check_loas_type(fuchsia_dir, action_args, env)
    if action == "cleanlogs":
        return _cmd_cleanlogs(fuchsia_dir)
    if action == "ensure_loas":
        return _cmd_ensure_loas(env)
    if action == "ensure_oauth":
        return _cmd_ensure_oauth(action_args, env)
    if action == "last_logdir":
        return _cmd_last_logdir(fuchsia_dir, env)
    if action == "diagnose":
        return rbe_diagnose.main(action_args, env)
    if action == "preflight":
        return _cmd_preflight(fuchsia_dir, env)
    if action == "profile":
        return _cmd_profile(fuchsia_dir, action_args, env)
    if action == "version":
        return _cmd_version(fuchsia_dir, env)

    _print_error(f"Unknown subcommand: {action}")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
