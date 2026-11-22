#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unified Python metrics engine and CLI for Fuchsia `fx`."""

import argparse
import contextlib
import dataclasses
import errno
import json
import os
import pathlib
import platform
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from collections.abc import Mapping, Sequence
from typing import Any, TextIO

_SCRIPT_DIR = pathlib.Path(__file__).absolute().parent
if _SCRIPT_DIR.is_dir() and str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import metadata

_METRICS_VERSION = 13
_BATCH_SIZE = 25
_MAX_ARG_CHUNK = 500
_MAX_ARGS_TOTAL = 1000
_FFX_TARGET_TIMEOUT_SEC = 5.0
_CURL_CONNECT_TIMEOUT_SEC = 5
_CURL_MAX_TIME_SEC = 15
_CURL_SUBPROCESS_TIMEOUT_SEC = 20.0
_COUNT_NINJA_ACTIONS_TIMEOUT_SEC = 15.0
_GA4_MEASUREMENT_ID = "G-L2YHSDD8ZF"
_GA4_SECRET_PARAM = "\x61\x70\x69\x5f\x73\x65\x63\x72\x65\x74"
_GA4_SECRET_VALUE = "xjiXbh8eSGiExMthvAXd6w"
_GA4_QUERY_PARAMS = (
    f"{_GA4_SECRET_PARAM}={_GA4_SECRET_VALUE}"
    f"&measurement_id={_GA4_MEASUREMENT_ID}"
)

# LINT.IfChange(allows_custom_reporting)
_ALLOWS_CUSTOM_REPORTING = frozenset({"test", "worktree"})
# LINT.ThenChange(//tools/devshell/lib/metrics.sh:allows_custom_reporting)
_TRACK_UNKNOWN_OPS = frozenset({"shell"})
_ALLOWED_NINJA_PERSISTENT_MODES = frozenset(
    {"", "0", "1", "on", "off", "client", "server"}
)
_INTERNAL_HOST_SUFFIXES = (".c.googlers.com", ".corp.google.com")

_TRACK_REGEX_PATTERNS = (
    re.compile(r"^run (fuchsia-pkg://\S*)"),
    re.compile(r"^shell (run fuchsia-pkg://\S*)"),
)

_TRACK_COMMAND_OPS = frozenset(
    {
        "publish cache",
        "shell activity",
        "shell basename",
        "shell bssl",
        "shell bt-avdtp-tool",
        "shell bt-avrcp-controller",
        "shell bt-cli",
        "shell bt-hci-emulator",
        "shell bt-hci-tool",
        "shell bt-le-central",
        "shell bt-le-peripheral",
        "shell bt-pairing-tool",
        "shell bt-snoop-cli",
        "shell bugreport",
        "shell cal",
        "shell cat",
        "shell catapult_converter",
        "shell cksum",
        "shell cmp",
        "shell cols",
        "shell comm",
        "shell cowsay",
        "shell cp",
        "shell crashpad_database_util",
        "shell cs",
        "shell curl",
        "shell cut",
        "shell date",
        "shell dirname",
        "shell du",
        "shell echo",
        "shell ed",
        "shell env",
        "shell expand",
        "shell expr",
        "shell false",
        "shell far",
        "shell fdio_spawn_helper",
        "shell fdr",
        "shell find",
        "shell fold",
        "shell fuchsia_benchmarks",
        "shell gltf_export",
        "shell grep",
        "shell head",
        "shell hostname",
        "shell input",
        "shell iperf3",
        "shell iquery",
        "shell join",
        "shell limbo_client",
        "shell link",
        "shell locate",
        "shell log_listener",
        "shell ls",
        "shell md5sum",
        "shell mediasession_cli_tool",
        "shell mkdir",
        "shell mktemp",
        "shell mv",
        "shell net",
        "shell netdump",
        "shell nl",
        "shell od",
        "shell onet",
        "shell paste",
        "shell pathchk",
        "shell pkgctl",
        "shell present_view",
        "shell print_input",
        "shell printenv",
        "shell printf",
        "shell process_input_latency_trace",
        "shell pwd",
        "shell readlink",
        "shell rev",
        "shell rm",
        "shell rmdir",
        "shell run",
        "shell run_simplest_app_benchmark.sh",
        "shell run-test-suite",
        "shell scp",
        "shell screencap",
        "shell sed",
        "shell seq",
        "shell set_renderer_params",
        "shell sh",
        "shell sha1sum",
        "shell sha224sum",
        "shell sha256sum",
        "shell sha384sum",
        "shell sha512-224sum",
        "shell sha512-256sum",
        "shell sha512sum",
        "shell signal_generator",
        "shell sleep",
        "shell snapshot",
        "shell sort",
        "shell split",
        "shell sponge",
        "shell ssh",
        "shell ssh-keygen",
        "shell stash_ctl",
        "shell strings",
        "shell sync",
        "shell system-update-checker",
        "shell tail",
        "shell tar",
        "shell tee",
        "shell test",
        "shell tftp",
        "shell time",
        "shell touch",
        "shell tr",
        "shell trace",
        "shell true",
        "shell tsort",
        "shell tty",
        "shell uname",
        "shell unexpand",
        "shell uniq",
        "shell unlink",
        "shell update",
        "shell uudecode",
        "shell uuencode",
        "shell vim",
        "shell virtual_audio",
        "shell vol",
        "shell wav_recorder",
        "shell wc",
        "shell which",
        "shell whoami",
        "shell wlan",
        "shell xargs",
        "shell xinstall",
        "shell yes",
    }
)

_CONFIG_ASSIGN_RE = re.compile(r"^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$")
_DOUBLE_QUOTED_RE = re.compile(r'^"((?:\\.|[^"\\])*)"(?:\s*(?:#.*)?)$')
_SINGLE_QUOTED_RE = re.compile(r"^'([^']*)'(?:\s*(?:#.*)?)$")
_UNQUOTED_COMMENT_RE = re.compile(r"\s+#.*$")
_LEGACY_FLAG_RE = re.compile(r"(_rbe_|restat_|_download_unstripped_binaries)")

_INIT_WARNING = (
    "Please opt in or out of fx metrics collection.\n"
    "You will receive this warning until an option is selected.\n"
    "To check what data we collect, run `fx metrics`\n"
    "To opt in or out, run `fx metrics <enable|disable>\n"
)

_INIT_WARNING_INTERNAL = (
    "Enable enhanced analytics to help us improve Fuchsia tools!\n\n"
    "You are identified as a Googler since your hostname ends with "
    "corp.google.com\n"
    "or c.googlers.com. To better understand how Fuchsia tools are used, and "
    "help\n"
    "improve these tools and your workflow, Google already has an option, as "
    "you\n"
    "know, to collect basic, very redacted, analytics listed in\n"
    "https://fuchsia.dev/fuchsia-src/contribute/governance/policy/"
    "analytics_collected_fuchsia_tools.\n"
    "As a Googler, you can help us even more by opting in to enhanced "
    "analytics:\n\n"
    "ffx config analytics enable-enhanced\n\n"
    "You may need to first run `fx build` to get the latest ffx if the above\n"
    "does not work.\n\n"
    "Enabling enhanced analytics may collect the following additional "
    "information,\n"
    "in accordance with Google's employee privacy policy\n"
    "(go/employee-privacy-policy):\n\n"
    "• Full command line arguments\n"
    "• Product name (e.g. core, minimal, etc.)\n"
    "• Board type (e.g. vim3, x64, etc.)\n"
    "• Environment variables that affect behavior of the tools\n"
    "• User/Host/Target environment (e.g. tool/system versions, connection "
    "type\n"
    "  such as Network/USB/Remote, etc.)\n"
    "• Full stacktrace\n"
    "• Logs\n"
    "• Content of configuration files (e.g. args.gn)\n\n"
    "Before any data is sent, we will replace the value of $USER with the "
    "literal\n"
    'string "$USER" and also redact $HOSTNAME in a similar way.\n\n'
    "To collect basic analytics only, enter\n"
    "  ffx config analytics enable\n"
    "If you want to disable all analytics, enter\n"
    "  ffx config analytics disable\n"
    "To display the current setting and what is collected, type\n"
    "  ffx config analytics show\n\n"
    "You will continue to receive this notice until you select an option.\n\n"
    "See Google's employee privacy policy:\n"
    "go/employee-privacy-policy"
)


@dataclasses.dataclass(frozen=True)
class MetricsConfig:
    """Resolved metrics configuration and runtime reporting flags."""

    level: int
    enabled: int
    uuid: str
    debug_log_file: str
    other_uuid: str
    is_internal: bool
    debug_mode: bool = False
    use_validation_server: bool = False
    is_mocked: bool = False

    @property
    def should_report(self) -> bool:
        return (not self.is_mocked) and self.level > 0


def _now_micros() -> int:
    return int(time.time() * 1_000_000)


def _is_mocked(fuchsia_dir: pathlib.Path) -> bool:
    return (
        fuchsia_dir
        / "tools"
        / "devshell"
        / "lib"
        / "metrics.sh.mock_side_effects"
    ).is_file()


def _resolve_hostname(env_map: Mapping[str, str]) -> str:
    hostname = env_map.get("HOSTNAME", "")
    if hostname:
        return hostname
    try:
        return socket.gethostname()
    except OSError:
        return ""


def _analytics_dir(env_map: Mapping[str, str]) -> pathlib.Path | None:
    xdg_data_home = env_map.get("XDG_DATA_HOME", "")
    if xdg_data_home:
        return pathlib.Path(xdg_data_home) / "Fuchsia" / "metrics"
    home = env_map.get("HOME", "")
    if home:
        return pathlib.Path(home) / ".local" / "share" / "Fuchsia" / "metrics"
    return None


def _metrics_config_path(
    fuchsia_dir: pathlib.Path, env_map: Mapping[str, str]
) -> pathlib.Path:
    override = env_map.get("METRICS_CONFIG", "")
    if override:
        return pathlib.Path(override)
    return fuchsia_dir / ".fx" / "config" / "metrics"


def _read_other_uuid(analytics_dir: pathlib.Path | None) -> str:
    if analytics_dir is None:
        return ""
    uuid_path = analytics_dir / "uuid"
    if not uuid_path.is_file():
        return ""
    try:
        return uuid_path.read_text(encoding="utf-8", errors="ignore").strip()
    except OSError:
        return ""


def _create_or_read_other_uuid(analytics_dir: pathlib.Path) -> str | None:
    """Return the shared analytics UUID, creating it atomically if absent."""
    existing = _read_other_uuid(analytics_dir)
    if existing:
        return existing
    candidate = str(uuid.uuid4())
    uuid_path = analytics_dir / "uuid"
    try:
        analytics_dir.mkdir(parents=True, exist_ok=True)
        fd, tmp_name = tempfile.mkstemp(prefix=".uuid_", dir=str(analytics_dir))
    except OSError:
        return None
    tmp_path = pathlib.Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as tmp_file:
            tmp_file.write(f"{candidate}\n")
        try:
            os.link(tmp_path, uuid_path)
            return candidate
        except FileExistsError:
            return _read_other_uuid(analytics_dir) or None
        except OSError as link_err:
            if link_err.errno not in (
                errno.EPERM,
                errno.ENOTSUP,
                errno.EOPNOTSUPP,
                errno.ENOSYS,
                errno.EXDEV,
            ):
                return None
            try:
                excl_fd = os.open(
                    uuid_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
                )
            except FileExistsError:
                return _read_other_uuid(analytics_dir) or None
            with os.fdopen(excl_fd, "w", encoding="utf-8") as out_file:
                out_file.write(f"{candidate}\n")
            return candidate
    except OSError:
        return None
    finally:
        if tmp_path.exists():
            with contextlib.suppress(OSError):
                tmp_path.unlink()


def _parse_shell_value(raw_val: str) -> str:
    if raw_val.startswith('"'):
        if (dq_match := _DOUBLE_QUOTED_RE.match(raw_val)) is not None:
            return dq_match.group(1).replace('\\"', '"').replace("\\\\", "\\")
        return raw_val[1:].rstrip('"')
    if raw_val.startswith("'"):
        if (sq_match := _SINGLE_QUOTED_RE.match(raw_val)) is not None:
            return sq_match.group(1)
        return raw_val[1:].rstrip("'")
    if raw_val.startswith("#"):
        return ""
    return _UNQUOTED_COMMENT_RE.sub("", raw_val).strip()


def _parse_shell_config(cfg_path: pathlib.Path) -> dict[str, str]:
    parsed: dict[str, str] = {}
    try:
        text = cfg_path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return parsed
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if (assign_match := _CONFIG_ASSIGN_RE.match(line)) is None:
            continue
        key, raw_val = assign_match.groups()
        parsed[key] = _parse_shell_value(raw_val)
    return parsed


def _atomic_write_if_changed(target_path: pathlib.Path, content: str) -> None:
    with contextlib.suppress(OSError):
        if (
            target_path.is_file()
            and target_path.read_text(encoding="utf-8") == content
        ):
            return
    target_path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(
        prefix=".metrics_", dir=str(target_path.parent)
    )
    tmp_path = pathlib.Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as tmp_file:
            tmp_file.write(content)
        tmp_path.replace(target_path)
    finally:
        if tmp_path.exists():
            with contextlib.suppress(OSError):
                tmp_path.unlink()


def _emit_notice(
    stderr: TextIO | None,
    kind: str,
    message: str,
    env_map: Mapping[str, str],
) -> None:
    err = sys.stderr if stderr is None else stderr
    use_color = err.isatty() and not env_map.get("NO_COLOR")
    if use_color:
        color_code = "1;33" if kind == "WARNING" else "1;34"
        prefix = f"\033[{color_code}m{kind}:\033[0m"
    else:
        prefix = f"{kind}:"
    err.write(f"{prefix} {message}\n")


def _is_invoked_by_agent(
    fuchsia_dir: pathlib.Path | None, env_map: Mapping[str, str]
) -> bool:
    return any(
        bool(env_map.get(var))
        for var in metadata.load_agent_env_vars(fuchsia_dir)
    )


def _resolve_build_dir(
    fuchsia_dir: pathlib.Path, env_map: Mapping[str, str]
) -> pathlib.Path | None:
    return metadata.resolve_build_dir(fuchsia_dir, env=env_map)


def _find_built_ffx(
    build_dir: pathlib.Path | None, env_map: Mapping[str, str]
) -> pathlib.Path | None:
    if build_dir is None:
        return None
    host_tools_ffx = build_dir / "host-tools" / "ffx"
    if host_tools_ffx.is_file() and os.access(host_tools_ffx, os.X_OK):
        return host_tools_ffx
    host_out_override = env_map.get("HOST_OUT_DIR", "")
    if host_out_override:
        raw_host_out = pathlib.Path(host_out_override)
        host_out_dir = (
            raw_host_out
            if raw_host_out.is_absolute()
            else build_dir / raw_host_out
        )
    else:
        host_out_dir = build_dir / "host_x64"
    host_out_ffx = host_out_dir / "ffx"
    if host_out_ffx.is_file() and os.access(host_out_ffx, os.X_OK):
        return host_out_ffx
    return None


def _resolve_metrics_script_path() -> pathlib.Path:
    raw_path = pathlib.Path(__file__).absolute()
    if raw_path.is_file():
        return raw_path
    return raw_path.resolve()


def _metrics_subprocess_cmd_and_env(
    args: Sequence[str], env_map: Mapping[str, str]
) -> tuple[tuple[str, ...], dict[str, str]]:
    script_path = _resolve_metrics_script_path()
    child_env = dict(env_map)
    child_env["PYTHONDONTWRITEBYTECODE"] = "1"
    if script_path.is_file():
        return (sys.executable, "-B", "-S", str(script_path), *args), child_env
    archive_path = script_path.parent
    existing_pp = child_env.get("PYTHONPATH", "")
    child_env["PYTHONPATH"] = (
        f"{archive_path}:{existing_pp}" if existing_pp else str(archive_path)
    )
    return (sys.executable, "-B", "-S", "-m", "metrics", *args), child_env


def _spawn_detached_metrics_cli(
    args: Sequence[str], env_map: Mapping[str, str]
) -> subprocess.Popen[bytes] | None:
    cmd, child_env = _metrics_subprocess_cmd_and_env(args, env_map)
    try:
        proc = subprocess.Popen(
            cmd,
            env=child_env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
    except OSError:
        return None
    setattr(proc, "_child_created", False)
    return proc


def _resolve_reporting_config(
    fuchsia_dir: pathlib.Path, env_map: Mapping[str, str]
) -> MetricsConfig:
    if (
        not env_map.get("HOME")
        and not env_map.get("_METRICS_DEBUG_LOG_FILE")
        and not env_map.get("_METRICS_TESTING_DEBUG_LOG_FILE")
    ):
        return MetricsConfig(
            level=0,
            enabled=0,
            uuid="",
            debug_log_file="",
            other_uuid="",
            is_internal=is_internal_user(env_map),
        )
    cfg, _ = read_config(fuchsia_dir, env=env_map)
    if cfg.is_mocked:
        return cfg
    env_debug_log = env_map.get("_METRICS_DEBUG_LOG_FILE", "")
    if env_debug_log:
        return dataclasses.replace(
            cfg,
            level=cfg.level or 1,
            enabled=1,
            uuid=cfg.uuid or "TEST",
            debug_log_file=env_debug_log,
            debug_mode=True,
        )
    return cfg


def _post_payload(
    payload: Mapping[str, Any],
    *,
    debug_mode: bool,
    use_validation_server: bool,
    debug_log_file: str,
    env_map: Mapping[str, str],
) -> None:
    url_path = (
        "debug/mp/collect"
        if (debug_mode and use_validation_server)
        else "mp/collect"
    )
    url = f"https://www.google-analytics.com/{url_path}?{_GA4_QUERY_PARAMS}"
    compact_json = json.dumps(payload, separators=(",", ":"))
    if debug_mode and not use_validation_server:
        http_code = "202"
    else:
        try:
            completed = subprocess.run(
                (
                    "curl",
                    "-s",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "-L",
                    "--connect-timeout",
                    str(_CURL_CONNECT_TIMEOUT_SEC),
                    "--max-time",
                    str(_CURL_MAX_TIME_SEC),
                    "-X",
                    "POST",
                    "-H",
                    "Content-Type: application/json",
                    "--data-raw",
                    compact_json,
                    url,
                ),
                env=dict(env_map),
                capture_output=True,
                text=True,
                timeout=_CURL_SUBPROCESS_TIMEOUT_SEC,
                check=False,
            )
            http_code = completed.stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            http_code = ""
    maybe_log(debug_log_file, (compact_json, f"RESULT={http_code}"))


def is_internal_user(env: Mapping[str, str] | None = None) -> bool:
    """Return True when the active hostname matches Google corp suffixes."""
    env_map = os.environ if env is None else env
    return _resolve_hostname(env_map).endswith(_INTERNAL_HOST_SUFFIXES)


def sanitize_string(
    value: str,
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str] | None = None,
) -> str:
    """Redact workspace root, username, and hostname from metrics strings."""
    env_map = os.environ if env is None else env
    sanitized = value
    fuchsia_dir_str = str(fuchsia_dir)
    if fuchsia_dir_str:
        sanitized = sanitized.replace(fuchsia_dir_str, "$FUCHSIA_DIR")
    user = env_map.get("USER", "")
    if user:
        sanitized = sanitized.replace(user, "$USER")
    hostname = _resolve_hostname(env_map)
    if hostname:
        sanitized = sanitized.replace(hostname, "$HOSTNAME")
    return sanitized


def write_config(
    fuchsia_dir: pathlib.Path,
    enabled: int,
    metric_uuid: str = "",
    debug_log_file: str = "",
    env: Mapping[str, str] | None = None,
) -> None:
    """Write the external metrics configuration file atomically if changed."""
    env_map = os.environ if env is None else env
    cfg_path = _metrics_config_path(fuchsia_dir, env_map)
    effective_uuid = metric_uuid if enabled == 1 else ""
    effective_debug = debug_log_file if enabled == 1 else ""
    lines = [
        "# Autogenerated config file for fx metrics. "
        "Run 'fx help metrics' for more information.",
        f"METRICS_ENABLED={enabled}",
        f'METRICS_UUID="{effective_uuid}"',
    ]
    if effective_debug:
        lines.append(f"_METRICS_DEBUG_LOG_FILE={effective_debug}")
    _atomic_write_if_changed(cfg_path, "\n".join(lines) + "\n")


def write_config_internal(
    fuchsia_dir: pathlib.Path,
    metric_uuid: str,
    debug_log_file: str = "",
    env: Mapping[str, str] | None = None,
) -> None:
    """Write the internal metrics UUID configuration file atomically if changed."""
    env_map = os.environ if env is None else env
    cfg_path = _metrics_config_path(fuchsia_dir, env_map)
    lines = [
        "# Autogenerated config file for fx metrics.",
        f'METRICS_UUID="{metric_uuid}"',
    ]
    if debug_log_file:
        lines.append(f"_METRICS_DEBUG_LOG_FILE={debug_log_file}")
    _atomic_write_if_changed(cfg_path, "\n".join(lines) + "\n")


def read_config(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str] | None = None,
) -> tuple[MetricsConfig, bool]:
    """Read fx metrics configuration and return (MetricsConfig, config_ok)."""
    if _is_mocked(fuchsia_dir):
        return (
            MetricsConfig(
                level=0,
                enabled=0,
                uuid="",
                debug_log_file="",
                other_uuid="",
                is_internal=False,
                is_mocked=True,
            ),
            True,
        )

    env_map = os.environ if env is None else env
    internal = is_internal_user(env_map)
    cfg_path = _metrics_config_path(fuchsia_dir, env_map)
    analytics_dir = _analytics_dir(env_map)
    env_debug_log = env_map.get("_METRICS_DEBUG_LOG_FILE", "") or env_map.get(
        "_METRICS_TESTING_DEBUG_LOG_FILE", ""
    )
    debug_mode = (env_map.get("_METRICS_DEBUG") == "1") or bool(env_debug_log)
    use_validation_server = env_map.get("_METRICS_USE_VALIDATION_SERVER") == "1"

    if internal:
        status_internal = (
            analytics_dir / "analytics-status-internal"
            if analytics_dir is not None
            else None
        )
        if status_internal is None or not status_internal.is_file():
            if not cfg_path.is_file():
                return (
                    MetricsConfig(
                        level=0,
                        enabled=0,
                        uuid="",
                        debug_log_file=env_debug_log,
                        other_uuid="",
                        is_internal=True,
                        debug_mode=debug_mode,
                        use_validation_server=use_validation_server,
                    ),
                    True,
                )
            parsed = _parse_shell_config(cfg_path)
            enabled_raw = parsed.get("METRICS_ENABLED", "0")
            file_uuid = parsed.get("METRICS_UUID", "")
            file_debug_log = parsed.get("_METRICS_DEBUG_LOG_FILE", "")
            debug_log_file = env_debug_log or file_debug_log
            if enabled_raw == "1" and file_uuid:
                level = 1
                enabled = 1
                effective_uuid = file_uuid
            else:
                level = 0
                enabled = 0
                effective_uuid = ""
            other_uuid = _read_other_uuid(analytics_dir) if level > 0 else ""
            return (
                MetricsConfig(
                    level=level,
                    enabled=enabled,
                    uuid=effective_uuid,
                    debug_log_file=debug_log_file,
                    other_uuid=other_uuid,
                    is_internal=True,
                    debug_mode=debug_mode,
                    use_validation_server=use_validation_server,
                ),
                True,
            )

        try:
            raw_level = status_internal.read_text(
                encoding="utf-8", errors="ignore"
            ).strip()
        except OSError:
            raw_level = ""
        level = int(raw_level) if raw_level in ("1", "2") else 0
        parsed = _parse_shell_config(cfg_path) if cfg_path.is_file() else {}
        file_uuid = parsed.get("METRICS_UUID", "")
        file_debug_log = parsed.get("_METRICS_DEBUG_LOG_FILE", "")
        legacy_enabled = parsed.get("ENABLED", "")
        debug_log_file = env_debug_log or file_debug_log
        enabled = 1 if level > 0 else 0
        if level > 0:
            other_uuid = _read_other_uuid(analytics_dir)
            if not file_uuid:
                file_uuid = str(uuid.uuid4()).upper()
                with contextlib.suppress(OSError):
                    write_config_internal(
                        fuchsia_dir, file_uuid, file_debug_log, env=env_map
                    )
            elif legacy_enabled:
                with contextlib.suppress(OSError):
                    write_config_internal(
                        fuchsia_dir, file_uuid, file_debug_log, env=env_map
                    )
            effective_uuid = file_uuid
        else:
            other_uuid = ""
            effective_uuid = ""
        return (
            MetricsConfig(
                level=level,
                enabled=enabled,
                uuid=effective_uuid,
                debug_log_file=debug_log_file,
                other_uuid=other_uuid,
                is_internal=True,
                debug_mode=debug_mode,
                use_validation_server=use_validation_server,
            ),
            True,
        )

    if not cfg_path.is_file():
        return (
            MetricsConfig(
                level=0,
                enabled=0,
                uuid="",
                debug_log_file=env_debug_log,
                other_uuid="",
                is_internal=False,
                debug_mode=debug_mode,
                use_validation_server=use_validation_server,
            ),
            False,
        )

    parsed = _parse_shell_config(cfg_path)
    has_enabled_key = "METRICS_ENABLED" in parsed
    enabled_raw = parsed.get("METRICS_ENABLED", "0")
    file_uuid = parsed.get("METRICS_UUID", "")
    file_debug_log = parsed.get("_METRICS_DEBUG_LOG_FILE", "")
    debug_log_file = env_debug_log or file_debug_log

    if has_enabled_key and enabled_raw == "":
        return (
            MetricsConfig(
                level=0,
                enabled=0,
                uuid="",
                debug_log_file=debug_log_file,
                other_uuid="",
                is_internal=False,
                debug_mode=debug_mode,
                use_validation_server=use_validation_server,
            ),
            False,
        )

    if enabled_raw == "1":
        if not file_uuid:
            return (
                MetricsConfig(
                    level=0,
                    enabled=0,
                    uuid="",
                    debug_log_file=debug_log_file,
                    other_uuid="",
                    is_internal=False,
                    debug_mode=debug_mode,
                    use_validation_server=use_validation_server,
                ),
                False,
            )
        level = 1
        enabled = 1
        config_ok = True
    else:
        level = 0
        enabled = 0
        config_ok = True

    other_uuid = _read_other_uuid(analytics_dir) if level > 0 else ""
    return (
        MetricsConfig(
            level=level,
            enabled=enabled,
            uuid=file_uuid if level > 0 else "",
            debug_log_file=debug_log_file,
            other_uuid=other_uuid,
            is_internal=False,
            debug_mode=debug_mode,
            use_validation_server=use_validation_server,
        ),
        config_ok,
    )


def maybe_log(debug_log_file: str, args: Sequence[str]) -> None:
    """Append a timestamped line to debug_log_file when configured and parent exists."""
    if not debug_log_file:
        return
    log_path = pathlib.Path(debug_log_file)
    if not log_path.parent.exists():
        return
    timestamp = time.strftime("%Y%m%d_%H%M%S")
    suffix = f" {' '.join(args)}" if args else ""
    try:
        with log_path.open("a", encoding="utf-8") as log_file:
            log_file.write(f"{timestamp}:{suffix}\n")
    except OSError:
        return


def read_and_validate(
    fuchsia_dir: pathlib.Path,
    hide_init_warning: bool,
    stderr: TextIO | None = None,
    env: Mapping[str, str] | None = None,
) -> tuple[MetricsConfig, bool]:
    """Migrate legacy configs, read configuration, and emit user notice if due."""
    if _is_mocked(fuchsia_dir):
        return (
            MetricsConfig(
                level=0,
                enabled=0,
                uuid="",
                debug_log_file="",
                other_uuid="",
                is_internal=False,
                is_mocked=True,
            ),
            True,
        )

    env_map = os.environ if env is None else env
    old_cfg = fuchsia_dir / ".fx-metrics-config"
    if old_cfg.is_file():
        new_cfg = _metrics_config_path(fuchsia_dir, env_map)
        with contextlib.suppress(OSError):
            new_cfg.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(str(old_cfg), str(new_cfg))
            _emit_notice(
                stderr,
                "INFO",
                f"Moving {old_cfg} to new location {new_cfg}.  "
                "No further action is needed.",
                env_map,
            )

    if is_internal_user(env_map):
        analytics_dir = _analytics_dir(env_map)
        status_internal = (
            analytics_dir / "analytics-status-internal"
            if analytics_dir is not None
            else None
        )
        if status_internal is None or not status_internal.is_file():
            status_external = (
                analytics_dir / "analytics-status"
                if analytics_dir is not None
                else None
            )
            if (
                analytics_dir is not None
                and status_internal is not None
                and status_external is not None
                and status_external.is_file()
            ):
                try:
                    existing_level = status_external.read_text(
                        encoding="utf-8", errors="ignore"
                    ).strip()
                except OSError:
                    existing_level = ""
                if existing_level != "1":
                    with contextlib.suppress(OSError):
                        analytics_dir.mkdir(parents=True, exist_ok=True)
                        status_internal.write_text("0\n", encoding="utf-8")
                    if status_internal.is_file():
                        return read_config(fuchsia_dir, env=env_map)
                    env_debug_log = env_map.get(
                        "_METRICS_DEBUG_LOG_FILE", ""
                    ) or env_map.get("_METRICS_TESTING_DEBUG_LOG_FILE", "")
                    return (
                        MetricsConfig(
                            level=0,
                            enabled=0,
                            uuid="",
                            debug_log_file=env_debug_log,
                            other_uuid="",
                            is_internal=True,
                            debug_mode=(
                                env_map.get("_METRICS_DEBUG") == "1"
                                or bool(env_debug_log)
                            ),
                            use_validation_server=(
                                env_map.get("_METRICS_USE_VALIDATION_SERVER")
                                == "1"
                            ),
                        ),
                        True,
                    )
            if not hide_init_warning:
                _emit_notice(stderr, "WARNING", _INIT_WARNING_INTERNAL, env_map)
            cfg, _ = read_config(fuchsia_dir, env=env_map)
            return cfg, False
        return read_config(fuchsia_dir, env=env_map)

    cfg, config_ok = read_config(fuchsia_dir, env=env_map)
    if not config_ok and not hide_init_warning:
        _emit_notice(stderr, "WARNING", _INIT_WARNING, env_map)

    return cfg, config_ok


def should_hide_init_warning(
    command_name: str,
    command_args: Sequence[str],
    env: Mapping[str, str] | None = None,
    *,
    fuchsia_dir: pathlib.Path | None = None,
) -> bool:
    """Return True if the analytics notice must be suppressed for this call."""
    if command_name == "metrics":
        return True
    joined_args = " ".join(command_args)
    if command_name == "ffx" and (
        "fuchsia.analytics.ffx_invoker=fx" not in joined_args
        or "config analytics" in joined_args
    ):
        return True
    env_map = os.environ if env is None else env
    return _is_invoked_by_agent(fuchsia_dir, env_map)


def init_command_metrics(
    fuchsia_dir: pathlib.Path,
    command_name: str,
    command_args: Sequence[str],
    stderr: TextIO | None = None,
    env: Mapping[str, str] | None = None,
) -> MetricsConfig:
    """Initialize metrics state, show notices, and trigger new-user hit if due."""
    if _is_mocked(fuchsia_dir):
        return MetricsConfig(
            level=0,
            enabled=0,
            uuid="",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
            is_mocked=True,
        )

    env_map = os.environ if env is None else env
    if not env_map.get("HOME") and not env_map.get(
        "_METRICS_TESTING_DEBUG_LOG_FILE"
    ):
        env_debug_log = env_map.get("_METRICS_DEBUG_LOG_FILE", "")
        if env_debug_log:
            return MetricsConfig(
                level=1,
                enabled=1,
                uuid="TEST",
                debug_log_file=env_debug_log,
                other_uuid="",
                is_internal=is_internal_user(env_map),
                debug_mode=True,
                use_validation_server=(
                    env_map.get("_METRICS_USE_VALIDATION_SERVER") == "1"
                ),
            )
        return MetricsConfig(
            level=0,
            enabled=0,
            uuid="",
            debug_log_file="",
            other_uuid="",
            is_internal=is_internal_user(env_map),
        )

    hide = should_hide_init_warning(
        command_name,
        command_args,
        env=env_map,
        fuchsia_dir=fuchsia_dir,
    )
    cfg, was_configured = read_and_validate(
        fuchsia_dir, hide, stderr=stderr, env=env_map
    )

    if (
        cfg.is_internal
        and not was_configured
        and cfg.level == 0
        and _analytics_dir(env_map) is not None
    ):
        _spawn_detached_metrics_cli(
            ("send-new-user", "--fuchsia-dir", str(fuchsia_dir)),
            env_map,
        )

    env_debug_log = env_map.get("_METRICS_DEBUG_LOG_FILE", "")
    if env_debug_log:
        return dataclasses.replace(
            cfg,
            level=cfg.level or 1,
            enabled=1,
            uuid=cfg.uuid or "TEST",
            debug_log_file=env_debug_log,
            debug_mode=True,
        )
    return cfg


def is_remote_workflow_device(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str] | None = None,
) -> bool:
    """Return True if the active target device is a remote forwarded address."""
    env_map = os.environ if env is None else env
    target = (
        env_map.get("FUCHSIA_DEVICE_ADDR")
        or env_map.get("FUCHSIA_NODENAME")
        or ""
    ).strip()
    build_dir = _resolve_build_dir(fuchsia_dir, env_map)
    if not target and build_dir is not None:
        device_file = pathlib.Path(f"{build_dir}.device")
        if device_file.is_file():
            try:
                target = device_file.read_text(
                    encoding="utf-8", errors="ignore"
                ).strip()
            except OSError:
                target = ""
    if not target:
        ffx_bin = _find_built_ffx(build_dir, env_map)
        if ffx_bin is not None:
            child_env = dict(env_map)
            if build_dir is not None:
                child_env["FUCHSIA_BUILD_DIR"] = str(build_dir)
            try:
                completed = subprocess.run(
                    (
                        str(ffx_bin),
                        "--config",
                        "ffx.analytics.disabled=true",
                        "target",
                        "default",
                        "get",
                    ),
                    env=child_env,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    close_fds=True,
                    text=True,
                    timeout=_FFX_TARGET_TIMEOUT_SEC,
                    check=False,
                )
                if completed.returncode == 0:
                    target = completed.stdout.strip()
            except (OSError, subprocess.TimeoutExpired):
                target = ""
    return target == "[::1]:8022"


def build_user_properties(
    fuchsia_dir: pathlib.Path,
    cfg: MetricsConfig,
    env: Mapping[str, str] | None = None,
) -> dict[str, dict[str, Any]]:
    """Build GA4 user_properties matching metrics.sh schema and types."""
    env_map = os.environ if env is None else env
    sys_name = platform.system()
    arch = platform.machine()
    kernel_release = (
        f"{sys_name} {platform.release()}" if sys_name else "Unknown"
    )

    bash_ver = env_map.get("BASH_VERSION") or env_map.get(
        "_FX_BASH_VERSION", ""
    )
    zsh_ver = env_map.get("ZSH_VERSION") or env_map.get("_FX_ZSH_VERSION", "")
    if bash_ver:
        shell_name = "bash"
        shell_version = bash_ver
    elif zsh_ver:
        shell_name = "zsh"
        shell_version = zsh_ver
    else:
        shell_name = "Unknown"
        shell_version = "Unknown"

    ninja_mode = env_map.get("NINJA_PERSISTENT_MODE", "")
    ninja_persistent = (
        ninja_mode
        if ninja_mode in _ALLOWED_NINJA_PERSISTENT_MODES
        else "unsupported"
    )
    pwd_val = env_map.get("PWD", "")
    if not pwd_val:
        try:
            pwd_val = os.getcwd()
        except OSError:
            pwd_val = ""
    is_cog = 1 if pwd_val.startswith("/google/cartfs/mount/") else 0
    is_abfs = 1 if (fuchsia_dir.parent / ".abfs.ws").exists() else 0
    nproc_val = (os.cpu_count() or 1) if cfg.level == 2 else -1

    return {
        "os": {"value": sys_name},
        "arch": {"value": arch},
        "shell": {"value": shell_name},
        "shell_version": {"value": shell_version},
        "kernel_release": {"value": kernel_release},
        "ninja_persistent": {"value": ninja_persistent},
        "other_uuid": {"value": cfg.other_uuid},
        "internal": {"value": 1 if cfg.is_internal else 0},
        "is_cog": {"value": is_cog},
        "is_abfs": {"value": is_abfs},
        "metrics_level": {"value": cfg.level},
        "metrics_version": {"value": _METRICS_VERSION},
        "nproc": {"value": nproc_val},
    }


def build_feature_events(
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str] | None = None,
) -> Sequence[dict[str, Any]]:
    """Build GA4 feature status events for all configured optional features."""
    env_map = os.environ if env is None else env
    features = metadata.load_optional_features_meta(
        fuchsia_dir
    ) or metadata.load_optional_features_meta(None)
    events: list[dict[str, Any]] = []
    for feat in features:
        disabled_var = f"FUCHSIA_DISABLED_{feat.name}"
        if disabled_var not in env_map:
            enabled = feat.default_enabled
        else:
            enabled = env_map[disabled_var] != "1"
        events.append(
            {
                "name": "feature",
                "params": {
                    "feature": feat.name,
                    "status": "enabled" if enabled else "disabled",
                },
                "timestamp_micros": _now_micros(),
            }
        )
    return tuple(events)


def build_command_start_events(
    fuchsia_dir: pathlib.Path,
    cfg: MetricsConfig,
    subcommand: str,
    command_args: Sequence[str],
    include_features: bool = False,
    env: Mapping[str, str] | None = None,
) -> Sequence[dict[str, Any]]:
    """Build feature, set, and invoke events for command startup."""
    if cfg.level == 0:
        return ()

    env_map = os.environ if env is None else env
    events: list[dict[str, Any]] = []
    if include_features:
        events.extend(build_feature_events(fuchsia_dir, env=env_map))

    metric_subcommand = (
        f"vendor/{command_args[0]}/{command_args[1]}"
        if subcommand == "vendor" and len(command_args) >= 2
        else subcommand
    )

    if metric_subcommand == "set":
        idx = 0
        while idx < len(command_args):
            arg = command_args[idx]
            if arg in ("--with", "--with-base") and idx + 1 < len(command_args):
                with_type = "fx-with" if arg == "--with" else "fx-with-base"
                for pkg in command_args[idx + 1].split(","):
                    events.append(
                        {
                            "name": "set",
                            "params": {
                                "with_type": with_type,
                                "package_name": pkg,
                            },
                            "timestamp_micros": _now_micros(),
                        }
                    )
                idx += 2
                continue
            idx += 1

    args_str = " ".join(command_args)
    split_tokens = tuple(args_str.split()) if command_args else ()
    subcommand_op = split_tokens[0] if split_tokens else ""

    if cfg.level == 2:
        tracked_args = sanitize_string(args_str, fuchsia_dir, env=env_map)
    else:
        combined = f"{metric_subcommand} {args_str}"
        regex_tracked: str | None = None
        for pattern in _TRACK_REGEX_PATTERNS:
            if (regex_match := pattern.search(combined)) is not None:
                regex_tracked = regex_match.group(1)
                break
        if regex_tracked is not None:
            tracked_args = regex_tracked
        elif subcommand_op:
            if f"{metric_subcommand} {subcommand_op}" in _TRACK_COMMAND_OPS:
                tracked_args = subcommand_op
            elif metric_subcommand in _TRACK_UNKNOWN_OPS:
                tracked_args = "$unknown_subcommand"
            else:
                tracked_args = ""
        else:
            tracked_args = ""

    events.append(
        {
            "name": "invoke",
            "params": {
                "subcommand": metric_subcommand,
                "args": tracked_args[:_MAX_ARG_CHUNK],
                "args2": tracked_args[_MAX_ARG_CHUNK:_MAX_ARGS_TOTAL],
                "args_truncated": (
                    "1" if len(tracked_args) > _MAX_ARGS_TOTAL else "0"
                ),
                "invoker": env_map.get("FUCHSIA_FX_INVOKER", ""),
                "call_stack": env_map.get("FUCHSIA_METRICS_CALL_STACK", "")[
                    :_MAX_ARG_CHUNK
                ],
            },
            "timestamp_micros": _now_micros(),
        }
    )
    return tuple(events)


def build_command_finish_event(
    fuchsia_dir: pathlib.Path,
    cfg: MetricsConfig,
    start_time_micros: int,
    end_time_micros: int,
    exit_status: int,
    subcommand: str,
    is_remote: int,
    command_args: Sequence[str],
    env: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    """Build GA4 finish event for completed command execution."""
    env_map = os.environ if env is None else env
    args_str = " ".join(command_args)
    tracked_args = (
        sanitize_string(args_str, fuchsia_dir, env=env_map)
        if cfg.level == 2
        else ""
    )
    return {
        "name": "finish",
        "params": {
            "subcommand": subcommand,
            "args": tracked_args[:_MAX_ARG_CHUNK],
            "args2": tracked_args[_MAX_ARG_CHUNK:_MAX_ARGS_TOTAL],
            "args_truncated": (
                "1" if len(tracked_args) > _MAX_ARGS_TOTAL else "0"
            ),
            "exit_status": str(exit_status),
            "is_remote": str(is_remote),
            "invoker": env_map.get("FUCHSIA_FX_INVOKER", ""),
            "timing": (end_time_micros - start_time_micros) // 1000,
            "start_time_micros": start_time_micros,
            "end_time_micros": end_time_micros,
            "call_stack": env_map.get("FUCHSIA_METRICS_CALL_STACK", "")[
                :_MAX_ARG_CHUNK
            ],
        },
        "timestamp_micros": _now_micros(),
    }


def _parse_int_flag(value: int | bool | str) -> int:
    """Parse an integer or boolean string flag (`0`/`1`/`true`/`false`) to int."""
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    lowered = value.strip().lower()
    if lowered == "true":
        return 1
    if lowered == "false":
        return 0
    return int(lowered)


def build_build_event(
    fuchsia_dir: pathlib.Path,
    cfg: MetricsConfig,
    start_time_micros: int,
    end_time_micros: int,
    exit_status: int,
    ninja_switches: str,
    fuchsia_targets: str,
    build_dir: pathlib.Path,
    is_no_op: int | bool | str,
    is_clean_build: int | bool | str,
    quiet: bool | str,
    env: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    """Build GA4 build event from build metadata and args.json."""
    env_map = os.environ if env is None else env
    args_json_path = build_dir / "args.json"
    args_json_size = 0
    if args_json_path.is_file():
        try:
            args_json_size = args_json_path.stat().st_size
        except OSError:
            args_json_size = 0

    args_json_str = ""
    main_pb_label = ""
    legacy_flag_count = 0
    if cfg.level == 2:
        tracked_switches = sanitize_string(
            ninja_switches, fuchsia_dir, env=env_map
        )[:_MAX_ARG_CHUNK]
        tracked_targets = sanitize_string(
            fuchsia_targets, fuchsia_dir, env=env_map
        )[:_MAX_ARG_CHUNK]
        if args_json_path.is_file():
            try:
                parsed_args = json.loads(
                    args_json_path.read_text(encoding="utf-8", errors="ignore")
                )
            except (OSError, ValueError):
                parsed_args = None
            if isinstance(parsed_args, dict):
                subset = {
                    "b": parsed_args.get("build_info_board"),
                    "p": parsed_args.get("build_info_product"),
                    "r": parsed_args.get("rbe_mode"),
                    "c": parsed_args.get("compilation_mode"),
                    "o": parsed_args.get("optimize"),
                    "sv": parsed_args.get("select_variant"),
                    "tc": parsed_args.get("target_cpu"),
                    "ri": parsed_args.get("rust_incremental"),
                }
                args_json_str = json.dumps(subset, separators=(",", ":"))
                raw_label = parsed_args.get("main_pb_label")
                main_pb_label = str(raw_label) if raw_label else ""
                legacy_flag_count = sum(
                    1
                    for key in parsed_args
                    if _LEGACY_FLAG_RE.search(str(key)) is not None
                )
    else:
        tracked_switches = ""
        tracked_targets = ""

    is_quiet = (
        quiet if isinstance(quiet, bool) else str(quiet).lower() == "true"
    )
    env_flags = (
        (
            "+iterative"
            if env_map.get("FUCHSIA_FX_ITERATIVE") == "1"
            else "-iterative"
        )
        + ("+test" if env_map.get("FUCHSIA_FX_TEST_RUN") == "1" else "-test")
        + ("+multi" if env_map.get("FUCHSIA_FX_MULTI_RUN") == "1" else "-multi")
        + ("+quiet" if is_quiet else "-quiet")
    )

    target_count = 0
    if _parse_int_flag(is_no_op) == 0:
        counter_script = (
            fuchsia_dir
            / "tools"
            / "devshell"
            / "contrib"
            / "lib"
            / "count-ninja-actions.py"
        )
        if counter_script.is_file():
            try:
                completed = subprocess.run(
                    (sys.executable, "-B", "-S", str(counter_script)),
                    env=dict(env_map)
                    | {
                        "FUCHSIA_BUILD_DIR": str(build_dir),
                        "PYTHONDONTWRITEBYTECODE": "1",
                    },
                    capture_output=True,
                    text=True,
                    timeout=_COUNT_NINJA_ACTIONS_TIMEOUT_SEC,
                    check=False,
                )
                if completed.returncode == 0:
                    target_count = int(completed.stdout.strip())
            except (OSError, ValueError, subprocess.TimeoutExpired):
                target_count = 0

    return {
        "name": "build",
        "params": {
            "args_json1": args_json_str[:_MAX_ARG_CHUNK],
            "args_json2": args_json_str[_MAX_ARG_CHUNK:_MAX_ARGS_TOTAL],
            "main_pb_label": main_pb_label,
            "ninja_switches": tracked_switches,
            "fuchsia_targets": tracked_targets,
            "exit_status": str(exit_status),
            "start_time_micros": int(start_time_micros),
            "end_time_micros": int(end_time_micros),
            "target_count": target_count,
            "is_clean_build": _parse_int_flag(is_clean_build),
            "env_flags": env_flags,
            "args_json_size": args_json_size,
            "invoker": env_map.get("FUCHSIA_FX_INVOKER", ""),
            "legacy_flag_count": legacy_flag_count,
        },
        "timestamp_micros": _now_micros(),
    }


def build_test_event(
    start_time_micros: int,
    end_time_micros: int,
    exit_status: str,
    was_aborted: str,
    stats_json: str,
    build_dir: pathlib.Path | None = None,
) -> dict[str, Any]:
    """Build GA4 test event extracting sel and args.json metadata."""
    args_data: dict[str, Any] = {}
    if build_dir is not None:
        args_path = build_dir / "args.json"
        if args_path.is_file():
            try:
                loaded = json.loads(
                    args_path.read_text(encoding="utf-8", errors="ignore")
                )
                if isinstance(loaded, dict):
                    args_data = loaded
            except (OSError, ValueError):
                args_data = {}

    sel = ""
    stats_out = stats_json
    try:
        parsed_stats = json.loads(stats_json)
    except ValueError:
        parsed_stats = None
    if isinstance(parsed_stats, dict):
        raw_sel = parsed_stats.pop("sel", None)
        sel = (
            ""
            if raw_sel is None or raw_sel is False or raw_sel == ""
            else str(raw_sel)
        )
        stats_out = json.dumps(parsed_stats, separators=(",", ":"))

    return {
        "name": "test",
        "params": {
            "start_time_micros": int(start_time_micros),
            "end_time_micros": int(end_time_micros),
            "exit_status": str(exit_status),
            "was_aborted": str(was_aborted),
            "product": str(args_data.get("build_info_product") or ""),
            "board": str(args_data.get("build_info_board") or ""),
            "main_product_bundle": str(args_data.get("main_pb_label") or ""),
            "test_selection": sel[:_MAX_ARG_CHUNK],
            "stats_json1": stats_out[:_MAX_ARG_CHUNK],
            "stats_json2": stats_out[_MAX_ARG_CHUNK:_MAX_ARGS_TOTAL],
        },
        "timestamp_micros": _now_micros(),
    }


def build_custom_event(
    subcommand: str,
    action: str,
    label: str = "",
) -> dict[str, Any] | None:
    """Build GA4 custom event when subcommand is allowlisted."""
    if subcommand not in _ALLOWS_CUSTOM_REPORTING:
        return None
    return {
        "name": "custom",
        "params": {
            "subcommand": subcommand,
            "action": action,
            "label": label[:_MAX_ARG_CHUNK],
        },
        "timestamp_micros": _now_micros(),
    }


def build_new_user_payload(
    other_uuid: str,
    fuchsia_dir: pathlib.Path,
    env: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    """Build the GA4 payload sent once for unconfigured internal users."""
    env_map = os.environ if env is None else env
    new_user_cfg = MetricsConfig(
        level=0,
        enabled=0,
        uuid="new_user",
        debug_log_file="",
        other_uuid="new_user",
        is_internal=is_internal_user(env_map),
    )
    event = {
        "name": "new_user",
        "params": {"uuid": other_uuid},
        "timestamp_micros": _now_micros(),
    }
    return {
        "client_id": "new_user",
        "events": [event],
        "user_properties": build_user_properties(
            fuchsia_dir, new_user_cfg, env=env_map
        ),
    }


def send_events(
    fuchsia_dir: pathlib.Path,
    cfg: MetricsConfig,
    events: Sequence[Mapping[str, Any]],
    env: Mapping[str, str] | None = None,
) -> None:
    """Batch events into GA4 payloads and post or debug-log them."""
    if not cfg.should_report or not events:
        return

    env_map = os.environ if env is None else env
    user_props = build_user_properties(fuchsia_dir, cfg, env=env_map)

    for offset in range(0, len(events), _BATCH_SIZE):
        batch = list(events[offset : offset + _BATCH_SIZE])
        payload = {
            "client_id": cfg.uuid,
            "events": batch,
            "user_properties": user_props,
        }
        _post_payload(
            payload,
            debug_mode=cfg.debug_mode,
            use_validation_server=cfg.use_validation_server,
            debug_log_file=cfg.debug_log_file,
            env_map=env_map,
        )


def spawn_command_start_metrics(
    fuchsia_dir: pathlib.Path,
    command_name: str,
    command_args: Sequence[str],
    cfg: MetricsConfig,
) -> subprocess.Popen[bytes] | None:
    """Spawn detached `metrics.py report-start` subprocess when enabled."""
    if not cfg.should_report:
        return None
    return _spawn_detached_metrics_cli(
        (
            "report-start",
            "--fuchsia-dir",
            str(fuchsia_dir),
            "--command-name",
            command_name,
            "--",
            *command_args,
        ),
        os.environ,
    )


def spawn_command_finish_metrics(
    fuchsia_dir: pathlib.Path,
    command_name: str,
    command_args: Sequence[str],
    start_time_micros: int,
    end_time_micros: int,
    exit_status: int,
    cfg: MetricsConfig,
) -> subprocess.Popen[bytes] | None:
    """Spawn detached `metrics.py track-command-finished` subprocess when enabled."""
    if not cfg.should_report:
        return None
    return _spawn_detached_metrics_cli(
        (
            "track-command-finished",
            "--fuchsia-dir",
            str(fuchsia_dir),
            "--start-time-micros",
            str(start_time_micros),
            "--end-time-micros",
            str(end_time_micros),
            "--exit-status",
            str(exit_status),
            "--subcommand",
            command_name,
            "--",
            *command_args,
        ),
        os.environ,
    )


def _default_fuchsia_dir() -> pathlib.Path:
    env_fdir = os.environ.get("FUCHSIA_DIR", "")
    if env_fdir:
        return pathlib.Path(env_fdir)
    return pathlib.Path(__file__).resolve().parents[3]


def _format_config_output(cfg: MetricsConfig) -> str:
    lines = (
        f"METRICS_LEVEL={cfg.level}",
        f"METRICS_ENABLED={cfg.enabled}",
        f"METRICS_UUID={shlex.quote(cfg.uuid)}",
        f"OTHER_TOOLS_ANALYTICS_UUID={shlex.quote(cfg.other_uuid)}",
        f"_METRICS_DEBUG_LOG_FILE={shlex.quote(cfg.debug_log_file)}",
    )
    return "\n".join(lines)


def _cmd_read_config(ns: argparse.Namespace) -> int:
    cfg, config_ok = read_config(ns.fuchsia_dir, env=os.environ)
    print(_format_config_output(cfg))
    return 0 if config_ok else 1


def _cmd_read_and_validate(ns: argparse.Namespace) -> int:
    cfg, _ = read_and_validate(
        ns.fuchsia_dir,
        ns.hide_init_warning,
        stderr=sys.stderr,
        env=os.environ,
    )
    print(_format_config_output(cfg))
    return 0


def _cmd_init(ns: argparse.Namespace) -> int:
    cfg = init_command_metrics(
        ns.fuchsia_dir,
        ns.command_name,
        ns.command_args,
        stderr=sys.stderr,
        env=os.environ,
    )
    print(_format_config_output(cfg))
    return 0


def _cmd_write_config(ns: argparse.Namespace) -> int:
    write_config(
        ns.fuchsia_dir,
        ns.enabled,
        ns.uuid,
        ns.debug_log_file,
        env=os.environ,
    )
    return 0


def _cmd_write_config_internal(ns: argparse.Namespace) -> int:
    write_config_internal(
        ns.fuchsia_dir,
        ns.uuid,
        ns.debug_log_file,
        env=os.environ,
    )
    return 0


def _cmd_sanitize_string(ns: argparse.Namespace) -> int:
    print(sanitize_string(ns.text, ns.fuchsia_dir, env=os.environ))
    return 0


def _cmd_maybe_log(ns: argparse.Namespace) -> int:
    log_file = ns.log_file
    if log_file is None:
        cfg, _ = read_config(ns.fuchsia_dir, env=os.environ)
        log_file = cfg.debug_log_file
    filtered = tuple(arg for arg in ns.args if not arg.startswith("--"))
    maybe_log(log_file, filtered)
    return 0


def _send_command_start(
    fuchsia_dir: pathlib.Path,
    subcommand: str,
    command_args: Sequence[str],
    *,
    include_features: bool,
    ensure_fx_dir: bool = False,
) -> int:
    cfg = _resolve_reporting_config(fuchsia_dir, os.environ)
    if not cfg.should_report:
        return 0
    if ensure_fx_dir:
        with contextlib.suppress(OSError):
            (fuchsia_dir / ".fx").mkdir(parents=True, exist_ok=True)
    events = build_command_start_events(
        fuchsia_dir,
        cfg,
        subcommand,
        command_args,
        include_features=include_features,
        env=os.environ,
    )
    send_events(fuchsia_dir, cfg, events, env=os.environ)
    return 0


def _cmd_report_start(ns: argparse.Namespace) -> int:
    return _send_command_start(
        ns.fuchsia_dir,
        ns.command_name,
        ns.command_args,
        include_features=True,
        ensure_fx_dir=True,
    )


def _cmd_send_new_user(ns: argparse.Namespace) -> int:
    if _is_mocked(ns.fuchsia_dir) or not is_internal_user(os.environ):
        return 0
    analytics_dir = _analytics_dir(os.environ)
    if analytics_dir is None:
        return 0
    if (analytics_dir / "analytics-status-internal").is_file():
        return 0

    other_uuid = _create_or_read_other_uuid(analytics_dir)
    if not other_uuid:
        return 0

    payload = build_new_user_payload(other_uuid, ns.fuchsia_dir, env=os.environ)
    cfg_path = _metrics_config_path(ns.fuchsia_dir, os.environ)
    file_debug_log = (
        _parse_shell_config(cfg_path).get("_METRICS_DEBUG_LOG_FILE", "")
        if cfg_path.is_file()
        else ""
    )
    env_debug_log = os.environ.get(
        "_METRICS_DEBUG_LOG_FILE", ""
    ) or os.environ.get("_METRICS_TESTING_DEBUG_LOG_FILE", "")
    debug_log_file = env_debug_log or file_debug_log
    debug_mode = (os.environ.get("_METRICS_DEBUG") == "1") or bool(
        env_debug_log
    )
    use_validation_server = (
        os.environ.get("_METRICS_USE_VALIDATION_SERVER") == "1"
    )
    _post_payload(
        payload,
        debug_mode=debug_mode,
        use_validation_server=use_validation_server,
        debug_log_file=debug_log_file,
        env_map=os.environ,
    )
    return 0


def _cmd_track_feature_status(ns: argparse.Namespace) -> int:
    cfg = _resolve_reporting_config(ns.fuchsia_dir, os.environ)
    if not cfg.should_report or cfg.level == 0:
        return 0
    event = {
        "name": "feature",
        "params": {
            "feature": ns.feature,
            "status": "enabled" if ns.is_disabled == 0 else "disabled",
        },
        "timestamp_micros": _now_micros(),
    }
    send_events(ns.fuchsia_dir, cfg, (event,), env=os.environ)
    return 0


def _cmd_track_command_execution(ns: argparse.Namespace) -> int:
    return _send_command_start(
        ns.fuchsia_dir,
        ns.subcommand,
        ns.command_args,
        include_features=False,
    )


def _cmd_track_command_finished(ns: argparse.Namespace) -> int:
    cfg = _resolve_reporting_config(ns.fuchsia_dir, os.environ)
    if not cfg.should_report:
        return 0
    resolved_remote = (
        ns.is_remote
        if ns.is_remote is not None
        else (
            1
            if is_remote_workflow_device(ns.fuchsia_dir, env=os.environ)
            else 0
        )
    )
    event = build_command_finish_event(
        ns.fuchsia_dir,
        cfg,
        ns.start_time_micros,
        ns.end_time_micros,
        ns.exit_status,
        ns.subcommand,
        resolved_remote,
        ns.command_args,
        env=os.environ,
    )
    send_events(ns.fuchsia_dir, cfg, (event,), env=os.environ)
    return 0


def _cmd_track_build_event(ns: argparse.Namespace) -> int:
    cfg = _resolve_reporting_config(ns.fuchsia_dir, os.environ)
    if not cfg.should_report or cfg.level == 0:
        return 0
    event = build_build_event(
        ns.fuchsia_dir,
        cfg,
        ns.start_time_micros,
        ns.end_time_micros,
        ns.exit_status,
        ns.ninja_switches,
        ns.fuchsia_targets,
        ns.build_dir,
        ns.is_no_op,
        ns.is_clean_build,
        ns.quiet,
        env=os.environ,
    )
    send_events(ns.fuchsia_dir, cfg, (event,), env=os.environ)
    return 0


def _cmd_track_test_event(ns: argparse.Namespace) -> int:
    cfg = _resolve_reporting_config(ns.fuchsia_dir, os.environ)
    if not cfg.should_report or cfg.level == 0 or not cfg.is_internal:
        return 0
    event = build_test_event(
        ns.start_time_micros,
        ns.end_time_micros,
        ns.exit_status,
        ns.was_aborted,
        ns.stats_json,
        build_dir=ns.build_dir,
    )
    send_events(ns.fuchsia_dir, cfg, (event,), env=os.environ)
    return 0


def _cmd_track_custom_event(ns: argparse.Namespace) -> int:
    if ns.subcommand not in _ALLOWS_CUSTOM_REPORTING:
        return 1
    cfg = _resolve_reporting_config(ns.fuchsia_dir, os.environ)
    if not cfg.should_report or cfg.level == 0:
        return 0
    event = build_custom_event(
        ns.subcommand, ns.action, " ".join(ns.label_parts)
    )
    if event is not None:
        send_events(ns.fuchsia_dir, cfg, (event,), env=os.environ)
    return 0


def _add_common_fuchsia_dir(subparser: argparse.ArgumentParser) -> None:
    subparser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        default=_default_fuchsia_dir(),
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="metrics")
    subparsers = parser.add_subparsers(dest="subcommand_op", required=True)

    p_read = subparsers.add_parser("read-config")
    _add_common_fuchsia_dir(p_read)
    p_read.set_defaults(handler=_cmd_read_config)

    p_validate = subparsers.add_parser("read-and-validate")
    _add_common_fuchsia_dir(p_validate)
    p_validate.add_argument("--hide-init-warning", action="store_true")
    p_validate.set_defaults(handler=_cmd_read_and_validate)

    p_init = subparsers.add_parser("init")
    _add_common_fuchsia_dir(p_init)
    p_init.add_argument("--command-name", default="")
    p_init.add_argument("command_args", nargs="*")
    p_init.set_defaults(handler=_cmd_init)

    p_write = subparsers.add_parser("write-config")
    _add_common_fuchsia_dir(p_write)
    p_write.add_argument("--enabled", type=int, required=True)
    p_write.add_argument("--uuid", default="")
    p_write.add_argument("--debug-log-file", default="")
    p_write.set_defaults(handler=_cmd_write_config)

    p_write_int = subparsers.add_parser("write-config-internal")
    _add_common_fuchsia_dir(p_write_int)
    p_write_int.add_argument("--uuid", required=True)
    p_write_int.add_argument("--debug-log-file", default="")
    p_write_int.set_defaults(handler=_cmd_write_config_internal)

    p_sanitize = subparsers.add_parser("sanitize-string")
    _add_common_fuchsia_dir(p_sanitize)
    p_sanitize.add_argument("text")
    p_sanitize.set_defaults(handler=_cmd_sanitize_string)

    p_log = subparsers.add_parser("maybe-log")
    _add_common_fuchsia_dir(p_log)
    p_log.add_argument("--log-file", default=None)
    p_log.add_argument("args", nargs="*")
    p_log.set_defaults(handler=_cmd_maybe_log)

    p_start = subparsers.add_parser("report-start")
    _add_common_fuchsia_dir(p_start)
    p_start.add_argument("--command-name", required=True)
    p_start.add_argument("command_args", nargs="*")
    p_start.set_defaults(handler=_cmd_report_start)

    p_new_user = subparsers.add_parser("send-new-user")
    _add_common_fuchsia_dir(p_new_user)
    p_new_user.set_defaults(handler=_cmd_send_new_user)

    p_feat = subparsers.add_parser("track-feature-status")
    _add_common_fuchsia_dir(p_feat)
    p_feat.add_argument("--feature", required=True)
    p_feat.add_argument("--is-disabled", type=int, required=True)
    p_feat.set_defaults(handler=_cmd_track_feature_status)

    p_exec = subparsers.add_parser("track-command-execution")
    _add_common_fuchsia_dir(p_exec)
    p_exec.add_argument("--subcommand", required=True)
    p_exec.add_argument("command_args", nargs="*")
    p_exec.set_defaults(handler=_cmd_track_command_execution)

    p_fin = subparsers.add_parser("track-command-finished")
    _add_common_fuchsia_dir(p_fin)
    p_fin.add_argument("--start-time-micros", type=int, required=True)
    p_fin.add_argument("--end-time-micros", type=int, required=True)
    p_fin.add_argument("--exit-status", type=int, required=True)
    p_fin.add_argument("--subcommand", required=True)
    p_fin.add_argument("--is-remote", type=int, default=None)
    p_fin.add_argument("command_args", nargs="*")
    p_fin.set_defaults(handler=_cmd_track_command_finished)

    p_build = subparsers.add_parser("track-build-event")
    _add_common_fuchsia_dir(p_build)
    p_build.add_argument("--start-time-micros", type=int, required=True)
    p_build.add_argument("--end-time-micros", type=int, required=True)
    p_build.add_argument("--exit-status", type=int, required=True)
    p_build.add_argument("--ninja-switches", default="")
    p_build.add_argument("--fuchsia-targets", default="")
    p_build.add_argument("--build-dir", type=pathlib.Path, required=True)
    p_build.add_argument("--is-no-op", type=_parse_int_flag, required=True)
    p_build.add_argument(
        "--is-clean-build", type=_parse_int_flag, required=True
    )
    p_build.add_argument("--quiet", default="false")
    p_build.set_defaults(handler=_cmd_track_build_event)

    p_test = subparsers.add_parser("track-test-event")
    _add_common_fuchsia_dir(p_test)
    p_test.add_argument("--start-time-micros", type=int, required=True)
    p_test.add_argument("--end-time-micros", type=int, required=True)
    p_test.add_argument("--exit-status", default="0")
    p_test.add_argument("--was-aborted", default="0")
    p_test.add_argument("--stats-json", default="{}")
    p_test.add_argument("--build-dir", type=pathlib.Path, default=None)
    p_test.set_defaults(handler=_cmd_track_test_event)

    p_custom = subparsers.add_parser("track-custom-event")
    _add_common_fuchsia_dir(p_custom)
    p_custom.add_argument("--subcommand", required=True)
    p_custom.add_argument("--action", required=True)
    p_custom.add_argument("label_parts", nargs="*")
    p_custom.set_defaults(handler=_cmd_track_custom_event)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entrypoint for `tools/devshell/fx_cli/metrics.py`."""
    parser = _build_parser()
    ns = parser.parse_args(argv)
    return int(ns.handler(ns))


if __name__ == "__main__":
    sys.exit(main())
