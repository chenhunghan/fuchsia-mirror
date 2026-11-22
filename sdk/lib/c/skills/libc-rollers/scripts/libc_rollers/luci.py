# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""LUCI OAuth token acquisition, Scheduler pRPC, Buildbucket streaming, and TreeStatus."""

import json
import os
import pathlib
import re
import subprocess
import urllib.error
import urllib.request
from collections.abc import Mapping, MutableMapping, Sequence
from typing import Any

from libc_rollers import constants, utils

_STEP_CL_PATCHSET_RE = re.compile(
    r"change%22%3A\+%22(\d+)%22%2C\+%22patchset%22%3A\+%22(\d+)%22"
)
_STEP_GERRIT_CL_RE = re.compile(
    r"https://fuchsia-review\.googlesource\.com/(?:c/fuchsia/\+/)?(\d+)"
)
_STEP_COMMIT_COUNT_RE = re.compile(
    r'Step "get gerrit change numbers for [^|]+\|(\d+) commits?"'
)
_STEP_ROLL_REV_RE = re.compile(r'Step "roll ([0-9a-f]{12,40})"')
_CHILD_BUILD_URL_RE = re.compile(
    r"(?:cr-buildbucket\.appspot\.com/build|ci\.chromium\.org/b)/([0-9]{15,25})"
)


def _stream_bb_json(
    cmd: Sequence[str], *, check: bool
) -> tuple[int, list[dict[str, Any]]]:
    """Stream `bb ls -json` output incrementally from stdout pipe."""
    with subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        text=True,
    ) as proc:
        assert proc.stdout is not None
        records = utils.decode_json_stream_reader(proc.stdout)
        rc = proc.wait()
        if check and rc != 0:
            raise subprocess.CalledProcessError(rc, cmd)
        return rc, records


def get_luci_token(fuchsia_root: pathlib.Path) -> str:
    """Obtain a LUCI OAuth token via luci-auth or LUCI_AUTH_CREDENTIAL_HELPER."""
    luci_auth = utils.resolve_tool_bin(fuchsia_root, "luci-auth")
    luci_auth_err: subprocess.CalledProcessError | None = None
    if luci_auth is not None:
        cmd = (str(luci_auth), "token")
        res = subprocess.run(
            cmd,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
        if res.returncode == 0 and (token := res.stdout.strip()):
            return token
        luci_auth_err = subprocess.CalledProcessError(
            res.returncode, cmd, output=res.stdout, stderr=res.stderr
        )

    cred_helper = os.environ.get("LUCI_AUTH_CREDENTIAL_HELPER", "").strip()
    if cred_helper:
        helper_path = pathlib.Path(cred_helper).expanduser()
        if helper_path.is_file() and os.access(helper_path, os.X_OK):
            res = subprocess.run(
                (str(helper_path),),
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                check=True,
            )
            raw_out = res.stdout.strip()
            if raw_out.startswith("{"):
                parsed = json.loads(raw_out)
                tok = str(
                    parsed.get("token") or parsed.get("access_token") or ""
                ).strip()
                if tok:
                    return tok
                raise ValueError(
                    f"LUCI_AUTH_CREDENTIAL_HELPER ({helper_path}) JSON output "
                    "missing non-empty token"
                )
            if raw_out:
                return raw_out.splitlines()[0].strip()
            raise ValueError(
                f"LUCI_AUTH_CREDENTIAL_HELPER ({helper_path}) returned empty output"
            )

    if luci_auth_err is not None:
        raise luci_auth_err
    raise FileNotFoundError(
        "Neither executable luci-auth nor LUCI_AUTH_CREDENTIAL_HELPER is configured"
    )


def scheduler_rpc(
    token: str, method: str, payload: Mapping[str, Any]
) -> dict[str, Any]:
    """Call a LUCI Scheduler pRPC method."""
    req = urllib.request.Request(
        f"{constants.SCHEDULER_PRPC_BASE}/{method}",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        raw = resp.read().decode("utf-8")
    parsed: dict[str, Any] = json.loads(utils.strip_json_xsrf_prefix(raw))
    return parsed


def fetch_recent_builds(
    fuchsia_root: pathlib.Path, builder: str, count: int = 3
) -> list[dict[str, Any]]:
    """Run `bb ls` for a single builder and stream parsed JSON records."""
    bb_bin = utils.require_tool_bin(fuchsia_root, "bb")
    _, builds = _stream_bb_json(
        (
            str(bb_bin),
            "ls",
            "-n",
            str(count),
            "-json",
            "-fields",
            "id,status,create_time,end_time,summary_markdown,tags",
            builder,
        ),
        check=True,
    )
    return builds


def fetch_last_failed_build(
    fuchsia_root: pathlib.Path,
    builder: str,
    recent_builds: Sequence[Mapping[str, Any]],
) -> dict[str, Any] | None:
    """Return the most recent failed build, querying `bb ls -status failure` if needed."""
    for build in recent_builds:
        if build.get("status") in ("FAILURE", "INFRA_FAILURE"):
            return dict(build)
    bb_bin = utils.require_tool_bin(fuchsia_root, "bb")
    rc, failed = _stream_bb_json(
        (
            str(bb_bin),
            "ls",
            "-n",
            "1",
            "-status",
            "failure",
            "-json",
            "-fields",
            "id,status,create_time,end_time,summary_markdown,tags",
            builder,
        ),
        check=False,
    )
    if rc != 0 or not failed:
        return None
    return failed[0]


def update_scheduler_job_from_builds(
    roller_info: MutableMapping[str, Any],
    builds: Sequence[Mapping[str, Any]],
) -> None:
    """Extract `scheduler_job_id` tag or Retrigger URL from recent builds."""
    for build in builds:
        for tag in build.get("tags", ()):
            if tag.get("key") == "scheduler_job_id" and "/" in tag.get(
                "value", ""
            ):
                proj, job_id = tag["value"].split("/", 1)
                roller_info["luci_project"] = proj
                roller_info["scheduler_job"] = job_id
                return
        summary = build.get("summaryMarkdown", "")
        if match := re.search(
            r"https://luci-scheduler\.appspot\.com/jobs/([^/\s\)]+)/([^/\s\)]+)",
            summary,
        ):
            roller_info["luci_project"] = match.group(1)
            roller_info["scheduler_job"] = match.group(2)
            return


def inspect_roll_build_steps(
    fuchsia_root: pathlib.Path, build_id: str
) -> dict[str, Any]:
    """Stream `bb get -steps` to extract CL/patchset, commit count, and roll revision."""
    info: dict[str, Any] = {
        "cl_num": None,
        "patchset": None,
        "commit_count": None,
        "roll_revision": None,
        "restored_previous_attempt": False,
    }
    bb_bin = utils.resolve_tool_bin(fuchsia_root, "bb")
    if bb_bin is None:
        return info

    last_cl_patchset: tuple[str, str] | None = None
    last_cl_only: str | None = None
    has_restore_step = False
    try:
        with subprocess.Popen(
            (str(bb_bin), "get", str(build_id), "-steps"),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        ) as proc:
            assert proc.stdout is not None
            for line in proc.stdout:
                for m_cp in _STEP_CL_PATCHSET_RE.finditer(line):
                    last_cl_patchset = (m_cp.group(1), m_cp.group(2))
                for m_cl in _STEP_GERRIT_CL_RE.finditer(line):
                    last_cl_only = m_cl.group(1)
                if m_commits := _STEP_COMMIT_COUNT_RE.search(line):
                    info["commit_count"] = int(m_commits.group(1))
                if m_rev := _STEP_ROLL_REV_RE.search(line):
                    info["roll_revision"] = m_rev.group(1)
                if 'Step "restore CL from previous roll attempt"' in line:
                    has_restore_step = True
            if proc.wait() != 0:
                return info
    except OSError:
        return info

    if last_cl_patchset is not None:
        info["cl_num"], info["patchset"] = last_cl_patchset
    elif last_cl_only is not None:
        info["cl_num"] = last_cl_only

    ps_str = str(info["patchset"] or "")
    ps_int = int(ps_str) if ps_str.isdigit() else 1
    info["restored_previous_attempt"] = has_restore_step or ps_int > 1
    return info


def fetch_cl_builds(
    fuchsia_root: pathlib.Path,
    change_num: str,
    patchset: str | None = None,
) -> tuple[str, list[dict[str, Any]]]:
    """Return `(cl_url, builds)` for a Gerrit CL from Buildbucket."""
    ps_part = patchset if patchset else "1"
    cl_url = f"{constants.GERRIT_BASE_URL}/c/fuchsia/+/{change_num}/{ps_part}"

    bb_bin = utils.require_tool_bin(fuchsia_root, "bb")
    _, builds = _stream_bb_json(
        (str(bb_bin), "ls", "-cl", cl_url, "-json"),
        check=True,
    )
    return cl_url, builds


def resolve_downstream_leaf_builds(
    fuchsia_root: pathlib.Path,
    build_id: str,
    cache: MutableMapping[str, list[dict[str, Any]]] | None = None,
    _depth: int = 0,
) -> list[dict[str, Any]]:
    """Follow linked Buildbucket child IDs in failing steps up to depth 3."""
    bid_key = str(build_id)
    if _depth > 3 or not re.fullmatch(r"[0-9]{15,25}", bid_key):
        return []
    if cache is not None and bid_key in cache:
        return cache[bid_key]
    if cache is not None:
        cache[bid_key] = []

    bb_bin = utils.resolve_tool_bin(fuchsia_root, "bb")
    if bb_bin is None:
        return []
    try:
        with subprocess.Popen(
            (str(bb_bin), "get", "-json", "-steps", bid_key),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        ) as proc:
            assert proc.stdout is not None
            try:
                build_data = json.load(proc.stdout)
            except json.JSONDecodeError:
                build_data = None
            if proc.wait() != 0 or not isinstance(build_data, dict):
                return []
    except OSError:
        return []

    child_ids: list[str] = []
    for step in build_data.get("steps") or ():
        if step.get("status") not in ("FAILURE", "INFRA_FAILURE"):
            continue
        step_md = step.get("summaryMarkdown") or ""
        for m_id in _CHILD_BUILD_URL_RE.finditer(step_md):
            cid = m_id.group(1)
            if cid != bid_key and cid not in child_ids:
                child_ids.append(cid)

    if not child_ids:
        leaf_builds = [build_data] if _depth > 0 else []
    elif len(child_ids) > 4:
        leaf_builds = []
    else:
        collected: list[dict[str, Any]] = []
        for cid in child_ids:
            sub = resolve_downstream_leaf_builds(
                fuchsia_root, cid, cache=cache, _depth=_depth + 1
            )
            if not sub:
                return []
            collected.extend(sub)
        leaf_builds = collected

    if cache is not None:
        cache[bid_key] = leaf_builds
    return leaf_builds


def fetch_tree_status(
    fuchsia_root: pathlib.Path, tree_name: str = "fuchsia-stem"
) -> dict[str, Any]:
    """Query LUCI TreeStatus for current state and recent closure/reopen history."""
    info: dict[str, Any] = {
        "tree": tree_name,
        "general_state": "OPEN",
        "message": "",
        "create_time": "",
        "create_user": "",
        "last_reopened_at": None,
        "last_reopened_message": None,
        "last_closed_at": None,
        "last_closed_message": None,
    }
    try:
        token = get_luci_token(fuchsia_root)
        req = urllib.request.Request(
            "https://treestatus.api.luci.app/prpc/luci.tree_status.v1.TreeStatus/ListStatus",
            data=json.dumps(
                {"parent": f"trees/{tree_name}/status", "pageSize": 25}
            ).encode("utf-8"),
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            raw = resp.read().decode("utf-8")
        statuses = json.loads(utils.strip_json_xsrf_prefix(raw)).get(
            "status", ()
        )
        if statuses:
            latest = statuses[0]
            info["general_state"] = latest.get("generalState", "OPEN")
            info["message"] = latest.get("message", "")
            info["create_time"] = latest.get("createTime", "")
            info["create_user"] = latest.get("createUser", "")
            for idx in range(len(statuses) - 1):
                curr = statuses[idx]
                older = statuses[idx + 1]
                if (
                    curr.get("generalState") == "OPEN"
                    and older.get("generalState") != "OPEN"
                    and info["last_reopened_at"] is None
                ):
                    info["last_reopened_at"] = curr.get("createTime")
                    info["last_reopened_message"] = curr.get("message", "")
                    info["last_closed_at"] = older.get("createTime")
                    info["last_closed_message"] = older.get("message", "")
                    break
    except (
        OSError,
        subprocess.SubprocessError,
        urllib.error.URLError,
        ValueError,
    ):
        pass
    return info
