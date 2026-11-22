#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Audits Planter package cutover CLs for failed guards and unverified cutovers.

Planter posts a patchset-level comment "Package cutover summary for patchset
N" with one block per cutover package, written by
//build/beads/verifications/verify_package_cutover.py. Each block has a
"- Guard: <status>" line, and either "- Differences: package contents ..."
or, when nothing was compared, "- Package contents: not compared (...)".

For each CL, this reports the latest summarized patchset as:

  failed             a guard line says FAILED (the Bazel package changed
                     after phase 1 and now differs in ways phase 1 did not
                     report),
  unverified         no package of the cutover was verified in the final phase
                     (not compared, or its guard was not run, not
                     recompared or skipped),
  partly_unverified  some packages were not verified in the final phase,
  ok                 otherwise,

plus whether any earlier summary was failed or unverified, and the kinds of
the patchsets uploaded after the last summary.

It also reports the uploader's cutover review from the latest gate record
Planter posted (a "Planter gate record." comment with a planter-gate-record
JSON block):

  approved           every cutover item has a decision (version 2: a
                     `cutover_decisions` entry with the item's ID and
                     fingerprint, and a real/benign verdict at stage 1;
                     version 1: an acknowledgment of the patchset),
  pending            some items have no such decision (a WIP-held CL: the
                     decisions reach Gerrit on the release upload),
  none               the record has no cutover items,
  unknown            no gate record, or it left cutover items out.

Examples:

  guard_audit.py --status merged --after 2026-11-09
  guard_audit.py --status open --json --only-flagged
"""

import argparse
import concurrent.futures
import json
import re
import subprocess
import sys
import time
import urllib.parse
from collections.abc import Mapping, Sequence
from typing import Any

_HOST = "https://fuchsia-review.googlesource.com"
_GUARD_RE = re.compile(r"^- Guard: (.*)$", re.M)
_SUMMARY_RE = re.compile(r"Package cutover summary for patchset (\d+)")
_BLOCK_RE = re.compile(r"^\*\*Package cutover:\*\*", re.M)
# Both summary formats: "- Package contents: not compared (...)" and the
# older "- Differences: package contents not compared (...)".
_NOT_COMPARED_RE = re.compile(
    r"^- (?:Package contents: |Differences: package contents )not compared",
    re.M,
)
_UNVERIFIED_GUARD_RE = re.compile(
    r"^- Guard: .*\b(?:(?:unverified|not run|not recompared)\b|skipped \(rebased)",
    re.M,
)
_STATUSES = ("failed", "unverified", "partly_unverified", "ok")
_GATE_RECORD_FOOTER = "Planter gate record."
_GATE_RECORD_FENCE = "```planter-gate-record"
_STAGE1_VERDICTS = frozenset(("real", "benign"))


def _get(path: str, attempts: int = 5) -> Any:
    """GETs a Gerrit REST path with gob-curl, retrying with backoff."""
    detail = ""
    for i in range(attempts):
        proc = subprocess.run(
            ("gob-curl", "-s", _HOST + path), text=True, capture_output=True
        )
        if proc.stdout.startswith(")]}'"):
            return json.loads(proc.stdout.split("\n", 1)[1])
        detail = proc.stdout or proc.stderr
        if i + 1 < attempts:
            time.sleep(2**i)
    raise RuntimeError(f"Gerrit request failed: {path}: {detail[:200]!r}")


def _query(q: str) -> Sequence[Mapping[str, Any]]:
    changes: list[Mapping[str, Any]] = []
    start = 0
    while True:
        batch = _get(
            "/changes/?q=%s&o=ALL_REVISIONS&n=200&S=%d"
            % (urllib.parse.quote(q), start)
        )
        changes += batch
        if not batch or not batch[-1].get("_more_changes"):
            return tuple(changes)
        start += len(batch)


def _classify(message: str) -> dict[str, Any] | None:
    """Reads one summary comment. None if it is not a cutover summary."""
    guards = tuple(g.strip() for g in _GUARD_RE.findall(message))
    if not guards or "cutover summary" not in message:
        return None
    parts = _BLOCK_RE.split(message)
    blocks = tuple(parts[1:]) if len(parts) > 1 else (message,)
    not_compared = sum(
        bool(_NOT_COMPARED_RE.search(b) or _UNVERIFIED_GUARD_RE.search(b))
        for b in blocks
    )
    if any("FAILED" in g for g in guards):
        status = "failed"
    elif not_compared >= len(blocks):
        status = "unverified"
    elif not_compared:
        status = "partly_unverified"
    else:
        status = "ok"
    return {
        "status": status,
        "guards": guards,
        "packages": len(blocks),
        "not_compared": not_compared,
    }


def _gate_record(message: str) -> Mapping[str, Any] | None:
    """The gate record in a "Planter gate record." comment, or None."""
    if _GATE_RECORD_FOOTER not in message:
        return None
    start = message.find(_GATE_RECORD_FENCE)
    if start < 0:
        return None
    rest = message[start + len(_GATE_RECORD_FENCE) :]
    end = rest.find("\n```")
    if end < 0:
        return None
    try:
        rec = json.loads(rest[:end])
    except ValueError:
        return None
    return rec if isinstance(rec, dict) else None


def _cutover_review(rec: Mapping[str, Any] | None) -> dict[str, Any]:
    """The uploader's cutover review state in a gate record."""
    if rec is None:
        return {"review": "unknown", "decided": 0, "items": 0}
    items = [
        (f"{d.get('category', '')}:{d.get('key', '')}", d)
        for d in rec.get("cutover_diffs") or ()
    ] + [
        (f"{u.get('category', '')}:{u.get('package', '')}", u)
        for u in rec.get("cutover_unverified") or ()
    ]
    omitted = int(rec.get("omitted_cutover_diffs") or 0)
    if int(rec.get("version") or 1) >= 2:
        decisions = {
            (d.get("id", ""), d.get("fingerprint", "")): d
            for d in rec.get("cutover_decisions") or ()
        }

        def decided(item_id: str, item: Mapping[str, Any]) -> bool:
            d = decisions.get((item_id, item.get("fingerprint", "")))
            if d is None:
                return False
            stage = max(int(item.get("stage") or 0), int(d.get("stage") or 0))
            return stage < 1 or d.get("verdict") in _STAGE1_VERDICTS

        n = sum(decided(i, item) for i, item in items)
    else:
        ack = rec.get("acknowledgment") or {}
        acked = bool(ack) and ack.get("patchset") == rec.get("patchset")
        n = len(items) if acked else 0
    if omitted:
        review = "unknown"
    elif not items:
        review = "none"
    elif n == len(items):
        review = "approved"
    else:
        review = "pending"
    return {"review": review, "decided": n, "items": len(items) + omitted}


def _audit_change(
    ch: Mapping[str, Any], comments: Mapping[str, Any]
) -> Mapping[str, Any] | None:
    """Summarizes one CL from its change info and its comments."""
    summaries = []
    records = []
    for c in comments.get("/PATCHSET_LEVEL", ()):
        message = c.get("message", "")
        rec = _gate_record(message)
        if rec is not None:
            records.append(
                (int(rec.get("patchset") or 0), c.get("updated", ""), rec)
            )
            continue
        summary = _classify(message)
        if summary is None:
            continue
        m = _SUMMARY_RE.search(message)
        summaries.append(
            summary
            | {
                "ps": int(m.group(1))
                if m is not None
                else c.get("patch_set", 0),
                "updated": c.get("updated", "")[:19],
            }
        )
    if not summaries:
        return None
    summaries.sort(key=lambda s: (s["ps"], s["updated"]))
    last = summaries[-1]
    records.sort(key=lambda r: (r[0], r[1]))
    review = _cutover_review(records[-1][2] if records else None)
    revs = {v["_number"]: v for v in ch.get("revisions", {}).values()}
    num = ch["_number"]
    return {
        "change": num,
        "url": f"{_HOST}/c/fuchsia/+/{num}",
        "subject": ch.get("subject", ""),
        "status": ch.get("status", ""),
        "owner": ch.get("owner", {}).get("email")
        or ch.get("owner", {}).get("_account_id"),
        "submitted": ch.get("submitted", "")[:19],
        "latest_ps": max(revs) if revs else 0,
        "last_summary_ps": last["ps"],
        "last_status": last["status"],
        "last_guards": last["guards"],
        "ever_failed": any(s["status"] == "failed" for s in summaries),
        "ever_unverified": any(
            s["status"] in ("unverified", "partly_unverified")
            for s in summaries
        ),
        "cutover_review": review["review"],
        "cutover_decided": f"{review['decided']}/{review['items']}",
        "gate_record_ps": records[-1][0] if records else 0,
        "later_patchset_kinds": tuple(
            revs[p].get("kind") for p in sorted(revs) if p > last["ps"]
        ),
        "history": tuple(f"ps{s['ps']}:{s['status']}" for s in summaries),
    }


def _audit(ch: Mapping[str, Any]) -> Mapping[str, Any] | None:
    return _audit_change(ch, _get(f"/changes/{ch['_number']}/comments"))


def _build_query(status: str, after: str, extra: str) -> str:
    return f"project:fuchsia status:{status} after:{after} {extra}".strip()


def _format_row(r: Mapping[str, Any]) -> str:
    return "%-17s %s %d ps%d(summary ps%d) later=%s review=%s(%s) %s | %s" % (
        r["last_status"].upper(),
        r["submitted"] or "-",
        r["change"],
        r["latest_ps"],
        r["last_summary_ps"],
        ",".join(k or "?" for k in r["later_patchset_kinds"]) or "-",
        r["cutover_review"],
        r["cutover_decided"],
        " ".join(r["history"]),
        r["subject"][:70],
    )


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--status", choices=("open", "merged"), default="merged")
    ap.add_argument(
        "--after", default="2026-11-09", help="only CLs updated after this date"
    )
    ap.add_argument("--query", default="message:bazel_migration")
    ap.add_argument("--json", action="store_true")
    ap.add_argument(
        "--only-flagged",
        action="store_true",
        help="only CLs whose last summary is not ok",
    )
    a = ap.parse_args(argv)
    q = _build_query(a.status, a.after, a.query)
    changes = _query(q)
    with concurrent.futures.ThreadPoolExecutor(8) as ex:
        rows = sorted(
            (
                r
                for r in ex.map(_audit, changes)
                if r is not None
                and not (a.only_flagged and r["last_status"] == "ok")
            ),
            key=lambda r: (r["submitted"], r["change"]),
        )
    counts = {s: sum(r["last_status"] == s for r in rows) for s in _STATUSES}
    if a.json:
        json.dump(
            {
                "query": q,
                "scanned": len(changes),
                "counts": counts,
                "rows": rows,
            },
            sys.stdout,
            indent=1,
        )
        print()
        return 0
    print(
        f"query: {q}  scanned={len(changes)}  with_summary={len(rows)}  "
        + "  ".join(f"{k}={v}" for k, v in counts.items()),
        file=sys.stderr,
    )
    for r in rows:
        print(_format_row(r))
    return 0


if __name__ == "__main__":
    sys.exit(main())
