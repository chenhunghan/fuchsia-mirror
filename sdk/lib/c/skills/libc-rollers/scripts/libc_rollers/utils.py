# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tool binary resolution, HTTP/JSON streaming, atomic file writes, and time formatting."""

import base64
import datetime
import json
import os
import pathlib
import shutil
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Mapping, Sequence
from typing import IO, Any

from libc_rollers import constants

_REMOTE_TEXT_CACHE: dict[str, str] = {}
_TOOL_OVERRIDES: dict[str, pathlib.Path | str | None] = {
    "bb": None,
    "luci-auth": None,
    "luci_project": None,
}


def set_tool_overrides(
    *,
    bb_bin: pathlib.Path | str | None = None,
    luci_auth_bin: pathlib.Path | str | None = None,
    luci_project: str | None = None,
) -> None:
    """Configure runtime CLI binary and LUCI project overrides."""
    _TOOL_OVERRIDES["bb"] = bb_bin
    _TOOL_OVERRIDES["luci-auth"] = luci_auth_bin
    _TOOL_OVERRIDES["luci_project"] = luci_project


def get_luci_project_override() -> str | None:
    """Return any explicitly configured LUCI project override."""
    val = _TOOL_OVERRIDES.get("luci_project")
    return str(val) if val is not None else None


def clear_caches_and_overrides() -> None:
    """Reset in-process Gitiles text cache and CLI tool overrides."""
    _REMOTE_TEXT_CACHE.clear()
    _TOOL_OVERRIDES["bb"] = None
    _TOOL_OVERRIDES["luci-auth"] = None
    _TOOL_OVERRIDES["luci_project"] = None


def resolve_tool_bin(
    fuchsia_root: pathlib.Path | None,
    tool_name: str,
) -> pathlib.Path | None:
    """Resolve an external CLI binary from overrides, prebuilts, or PATH."""
    override = _TOOL_OVERRIDES.get(tool_name)
    if not override and tool_name in constants.TOOL_ENV_VARS:
        override = os.environ.get(constants.TOOL_ENV_VARS[tool_name])
    if override:
        resolved = pathlib.Path(override).expanduser().resolve()
        if resolved.is_file() and os.access(resolved, os.X_OK):
            return resolved
        raise FileNotFoundError(
            f"Configured {tool_name} binary is not executable: {resolved}"
        )

    search_dirs: list[str] = []
    prebuilt_dir = constants.TOOL_PREBUILT_DIRS.get(tool_name)
    if fuchsia_root is not None and prebuilt_dir is not None:
        search_dirs.append(str(fuchsia_root.joinpath(*prebuilt_dir)))
    home = pathlib.Path.home()
    search_dirs.extend([str(home / ".local" / "bin"), str(home / "bin")])
    if env_path := os.environ.get("PATH", ""):
        search_dirs.append(env_path)

    found = shutil.which(tool_name, path=os.pathsep.join(search_dirs))
    return pathlib.Path(found) if found is not None else None


def require_tool_bin(
    fuchsia_root: pathlib.Path | None, tool_name: str
) -> pathlib.Path:
    """Resolve a required external CLI binary or raise FileNotFoundError."""
    resolved = resolve_tool_bin(fuchsia_root, tool_name)
    if resolved is not None:
        return resolved
    env_var = constants.TOOL_ENV_VARS.get(tool_name, tool_name.upper())
    raise FileNotFoundError(
        f"Executable {tool_name!r} not found in checkout prebuilts, "
        f"~/.local/bin, ~/bin, or PATH "
        f"(pass --{tool_name}-bin or set {env_var})"
    )


def urlopen_bytes(url: str, timeout: int = 15) -> bytes:
    """Fetch a URL with bounded retries on transient HTTP 429/5xx errors."""
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=timeout) as resp:
                data: bytes = resp.read()
                return data
        except urllib.error.HTTPError as exc:
            if exc.code not in (429, 500, 502, 503, 504) or attempt == 2:
                raise
        except urllib.error.URLError:
            if attempt == 2:
                raise
        time.sleep(0.5 * (attempt + 1))
    raise RuntimeError(f"Unreachable retry exhaustion for {url}")


def strip_json_xsrf_prefix(raw: str) -> str:
    """Strip Gitiles/Gerrit/LUCI `)]}'` XSRF prefix line when present."""
    if raw.startswith(")]}"):
        parts = raw.split("\n", 1)
        return parts[1] if len(parts) > 1 else ""
    return raw


def fetch_remote_gob_text(url: str) -> str | None:
    """Fetch and base64-decode a ?format=TEXT file from Gitiles once per process."""
    if url in _REMOTE_TEXT_CACHE:
        return _REMOTE_TEXT_CACHE[url]
    try:
        raw_b64 = urlopen_bytes(url, timeout=15)
        decoded = base64.b64decode(raw_b64).decode("utf-8")
        _REMOTE_TEXT_CACHE[url] = decoded
        return decoded
    except (OSError, urllib.error.URLError, ValueError):
        return None


def decode_json_stream(raw: str) -> list[dict[str, Any]]:
    """Decode whitespace-delimited JSON objects emitted by `bb ls -json`."""
    items: list[dict[str, Any]] = []
    decoder = json.JSONDecoder()
    stripped = raw.strip()
    pos = 0
    total_len = len(stripped)
    while pos < total_len:
        while pos < total_len and stripped[pos].isspace():
            pos += 1
        if pos >= total_len:
            break
        try:
            obj, idx = decoder.raw_decode(stripped, pos)
        except json.JSONDecodeError:
            break
        if isinstance(obj, dict):
            items.append(obj)
        pos = idx
    return items


def decode_json_stream_reader(
    stream: IO[str], chunk_size: int = 65536
) -> list[dict[str, Any]]:
    """Incrementally decode whitespace-delimited JSON objects from a stream."""
    items: list[dict[str, Any]] = []
    decoder = json.JSONDecoder()
    buf = ""
    eof = False
    while True:
        buf = buf.lstrip()
        if not buf and eof:
            break
        try:
            obj, idx = decoder.raw_decode(buf)
            if isinstance(obj, dict):
                items.append(obj)
            buf = buf[idx:]
            continue
        except json.JSONDecodeError:
            if eof:
                break
        chunk = stream.read(chunk_size)
        if not chunk:
            eof = True
        else:
            buf += chunk
    return items


def atomic_write_json(path: pathlib.Path, data: Mapping[str, Any]) -> None:
    """Atomically write formatted JSON to `path` via a sibling temporary file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path: pathlib.Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as tmp_file:
            tmp_path = pathlib.Path(tmp_file.name)
            json.dump(data, tmp_file, indent=2)
            tmp_file.write("\n")
        os.replace(tmp_path, path)
    except BaseException:
        if tmp_path is not None:
            tmp_path.unlink(missing_ok=True)
        raise


def parse_iso_utc(ts_str: str | None) -> datetime.datetime | None:
    """Parse an ISO-8601 UTC timestamp string from Buildbucket or LUCI."""
    if not ts_str:
        return None
    try:
        if "." in ts_str:
            main_part, frac = ts_str.split(".", 1)
            frac_digits = frac.rstrip("Z")[:6]
            clean = f"{main_part}.{frac_digits}+00:00"
        else:
            clean = ts_str.rstrip("Z") + "+00:00"
        return datetime.datetime.fromisoformat(clean)
    except ValueError:
        return None


def iso_to_epoch(ts: str | None) -> float | None:
    """Convert an ISO-8601 timestamp string to UTC epoch seconds."""
    parsed = parse_iso_utc(ts)
    return parsed.timestamp() if parsed is not None else None


def gerrit_date_to_epoch(ts: str | None) -> float | None:
    """Convert a Gerrit UTC timestamp 'YYYY-MM-DD HH:MM:SS.000000000' to epoch."""
    if not ts:
        return None
    trimmed = ts.strip().split(".", 1)[0]
    try:
        dt = datetime.datetime.strptime(trimmed, "%Y-%m-%d %H:%M:%S").replace(
            tzinfo=datetime.timezone.utc
        )
        return dt.timestamp()
    except ValueError:
        return iso_to_epoch(ts)


def format_approx_local_time(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp as an approximate local time ('~1:40pm', 'Jul 14')."""
    if epoch_sec is None or epoch_sec <= 0:
        return ""
    now = now_sec if now_sec is not None else time.time()
    diff_sec = max(0.0, now - epoch_sec)
    local_dt = datetime.datetime.fromtimestamp(epoch_sec).astimezone()
    if diff_sec > 3 * 86400:
        return f"{local_dt.strftime('%b')} {local_dt.day}"
    rounded_ts = round(epoch_sec / 300.0) * 300.0
    rounded_dt = datetime.datetime.fromtimestamp(rounded_ts).astimezone()
    hour = rounded_dt.hour % 12 or 12
    ampm = "pm" if rounded_dt.hour >= 12 else "am"
    if rounded_dt.minute == 0:
        return f"~{hour}{ampm}"
    return f"~{hour}:{rounded_dt.minute:02d}{ampm}"


def format_relative_ago(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp in relative terms ('just now', '~35m ago')."""
    if epoch_sec is None or epoch_sec <= 0:
        return ""
    now = now_sec if now_sec is not None else time.time()
    diff_sec = max(0.0, now - epoch_sec)
    if diff_sec < 90:
        return "just now"
    mins = round(diff_sec / 60.0)
    if mins < 60:
        r_mins = round(mins / 5) * 5 if mins >= 10 else mins
        return f"~{r_mins}m ago"
    hours = diff_sec / 3600.0
    if hours < 36:
        if hours < 4:
            r_hours = round(hours * 2) / 2
            h_str = (
                f"{int(r_hours)}"
                if r_hours == int(r_hours)
                else f"{r_hours:.1f}"
            )
            return f"~{h_str}h ago"
        return f"~{round(hours)}h ago"
    days = diff_sec / 86400.0
    if days < 30:
        return f"~{round(days)}d ago"
    months = round((days / 30.44) * 2) / 2
    m_str = f"{int(months)}" if months == int(months) else f"{months:.1f}"
    return f"~{m_str}mo ago"


def format_time_pair(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp as '<relative> (<approx local>)'."""
    if epoch_sec is None or epoch_sec <= 0:
        return ""
    rel = format_relative_ago(epoch_sec, now_sec)
    loc = format_approx_local_time(epoch_sec, now_sec)
    return f"{rel} ({loc})" if loc else rel


def percentile(sorted_vals: Sequence[float], pct: float) -> float:
    """Compute a linear-interpolated percentile (0..100) from sorted values."""
    if not sorted_vals:
        return 0.0
    if len(sorted_vals) == 1:
        return round(sorted_vals[0], 2)
    idx = (pct / 100.0) * (len(sorted_vals) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(sorted_vals) - 1)
    frac = idx - lo
    return round(sorted_vals[lo] * (1.0 - frac) + sorted_vals[hi] * frac, 2)
